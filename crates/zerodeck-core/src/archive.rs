use std::fs::{self, File};
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tracing::{info, warn};
use uuid::Uuid;
use zip::ZipArchive;

use crate::CoreError;

/// Default maximum total uncompressed bytes permitted when extracting a plugin archive (512 MiB).
pub const DEFAULT_MAX_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;

/// Default maximum number of entries accepted in a plugin archive.
pub const DEFAULT_MAX_ARCHIVE_ENTRIES: usize = 10_000;

/// Resource limits enforced while extracting a plugin archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtractLimits {
    /// Maximum total uncompressed bytes across all entries.
    pub max_total_bytes: u64,
    /// Maximum number of entries (files and directories).
    pub max_entries: usize,
}

impl Default for ExtractLimits {
    fn default() -> Self {
        Self {
            max_total_bytes: DEFAULT_MAX_ARCHIVE_BYTES,
            max_entries: DEFAULT_MAX_ARCHIVE_ENTRIES,
        }
    }
}

/// Provenance and verification context for a plugin ZIP or `remote_binary` download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallSource {
    Remote {
        sha256: Option<String>,
    },
    LocalFile {
        sha256: Option<String>,
        user_confirmed: bool,
    },
}

/// Enforces `ZeroDeck`'s fail-closed SHA-256 policy and returns the computed lowercase hex digest.
pub fn verify_sha256_policy(
    bytes: &[u8],
    source: &InstallSource,
    allow_unverified: bool,
) -> Result<String, CoreError> {
    let actual = hex::encode(Sha256::digest(bytes));

    let expected_opt = match source {
        InstallSource::Remote { sha256 } => {
            let trimmed = sha256.as_deref().map(str::trim).filter(|s| !s.is_empty());
            if trimmed.is_none() && !allow_unverified {
                return Err(CoreError::MissingSha256);
            }
            trimmed
        }
        InstallSource::LocalFile {
            sha256,
            user_confirmed,
        } => {
            let trimmed = sha256.as_deref().map(str::trim).filter(|s| !s.is_empty());
            if trimmed.is_none() && !user_confirmed && !allow_unverified {
                return Err(CoreError::ConfirmationRequired);
            }
            trimmed
        }
    };

    if let Some(expected) = expected_opt {
        if !actual.eq_ignore_ascii_case(expected) {
            return Err(CoreError::Sha256Mismatch {
                expected: expected.to_owned(),
                actual,
            });
        }
    } else {
        info!(sha256 = %actual, "verified unhashed sideload archive by explicit confirmation/override");
    }

    Ok(actual)
}

/// Extracts a ZIP archive into `dest_dir` with the default [`ExtractLimits`].
///
/// See [`extract_zip_safely_with_limits`] for the safety and replacement guarantees.
pub fn extract_zip_safely(bytes: &[u8], dest_dir: &Path) -> Result<Vec<PathBuf>, CoreError> {
    extract_zip_safely_with_limits(bytes, dest_dir, &ExtractLimits::default())
}

/// Extracts a ZIP archive into `dest_dir` with a bounded total uncompressed byte limit.
pub fn extract_zip_safely_with_limit(
    bytes: &[u8],
    dest_dir: &Path,
    max_total_bytes: u64,
) -> Result<Vec<PathBuf>, CoreError> {
    let limits = ExtractLimits {
        max_total_bytes,
        ..ExtractLimits::default()
    };
    extract_zip_safely_with_limits(bytes, dest_dir, &limits)
}

/// Extracts a ZIP archive so that it **replaces** `dest_dir` all-or-nothing.
///
/// Every entry is validated first (Zip-Slip, symlinks, entry count, declared size). The archive is
/// then written into a hidden staging directory next to `dest_dir`, and only once every entry has
/// been written is the staging directory swapped into place. Any failure leaves an existing
/// `dest_dir` exactly as it was and removes the staging directory. Returns the extracted file paths
/// under the final, canonicalized `dest_dir`.
pub fn extract_zip_safely_with_limits(
    bytes: &[u8],
    dest_dir: &Path,
    limits: &ExtractLimits,
) -> Result<Vec<PathBuf>, CoreError> {
    let mut archive = ZipArchive::new(Cursor::new(bytes))?;
    if archive.len() > limits.max_entries {
        return Err(CoreError::ArchiveTooManyEntries {
            count: archive.len(),
            max_entries: limits.max_entries,
        });
    }
    let entries = validate_entries(&mut archive, limits.max_total_bytes)?;

    let (parent, dest_name) = split_destination(dest_dir)?;
    fs::create_dir_all(&parent).map_err(|source| CoreError::Io {
        path: parent.clone(),
        source,
    })?;
    let parent = fs::canonicalize(&parent).map_err(|source| CoreError::Io {
        path: parent.clone(),
        source,
    })?;
    let final_dest = parent.join(&dest_name);

    let staging = parent.join(format!(".{dest_name}.staging-{}", Uuid::new_v4()));
    fs::create_dir(&staging).map_err(|source| CoreError::Io {
        path: staging.clone(),
        source,
    })?;

    let written = write_entries(&mut archive, entries, &staging, limits.max_total_bytes).and_then(
        |relative| {
            swap_into_place(&staging, &final_dest, &parent, &dest_name)?;
            Ok(relative)
        },
    );
    match written {
        Ok(relative) => Ok(relative
            .into_iter()
            .map(|rel| final_dest.join(rel))
            .collect()),
        Err(err) => {
            let _ = fs::remove_dir_all(&staging);
            Err(err)
        }
    }
}

/// Checks every entry before anything touches the filesystem; returns `(index, relative path)`.
fn validate_entries(
    archive: &mut ZipArchive<Cursor<&[u8]>>,
    max_total_bytes: u64,
) -> Result<Vec<(usize, PathBuf)>, CoreError> {
    let mut entries = Vec::with_capacity(archive.len());
    let mut declared_total: u64 = 0;
    for idx in 0..archive.len() {
        let entry = archive.by_index(idx)?;
        let raw_name = || String::from_utf8_lossy(entry.name_raw()).into_owned();
        if entry.is_symlink() {
            return Err(CoreError::UnsafeArchiveEntry { entry: raw_name() });
        }
        let Some(rel_path) = entry.enclosed_name() else {
            return Err(CoreError::UnsafeArchiveEntry { entry: raw_name() });
        };
        declared_total = declared_total.saturating_add(entry.size());
        if declared_total > max_total_bytes {
            return Err(CoreError::ArchiveSizeExceeded {
                entry: raw_name(),
                max_bytes: max_total_bytes,
            });
        }
        entries.push((idx, rel_path));
    }
    Ok(entries)
}

/// Splits `dest_dir` into its parent directory and final component name.
fn split_destination(dest_dir: &Path) -> Result<(PathBuf, String), CoreError> {
    let invalid = || CoreError::Io {
        path: dest_dir.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "extraction destination must end in a normal directory name",
        ),
    };
    let name = dest_dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(invalid)?
        .to_owned();
    let parent = dest_dir
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    Ok((parent, name))
}

/// Writes the validated entries under `root`, returning the relative paths of extracted files.
fn write_entries(
    archive: &mut ZipArchive<Cursor<&[u8]>>,
    entries: Vec<(usize, PathBuf)>,
    root: &Path,
    max_total_bytes: u64,
) -> Result<Vec<PathBuf>, CoreError> {
    let mut extracted = Vec::with_capacity(entries.len());
    let mut total_written: u64 = 0;

    for (idx, rel_path) in entries {
        let entry = archive.by_index(idx)?;
        let entry_name = String::from_utf8_lossy(entry.name_raw()).into_owned();
        let unix_mode = entry.unix_mode();
        let out_path = root.join(&rel_path);

        if entry.is_dir() {
            fs::create_dir_all(&out_path).map_err(|source| CoreError::Io {
                path: out_path.clone(),
                source,
            })?;
            ensure_inside_canonical_root(&out_path, root, &entry_name)?;
            continue;
        }

        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).map_err(|source| CoreError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
            ensure_inside_canonical_root(parent, root, &entry_name)?;
        }
        if let Ok(meta) = fs::symlink_metadata(&out_path)
            && meta.file_type().is_symlink()
        {
            return Err(CoreError::UnsafeArchiveEntry { entry: entry_name });
        }

        let remaining = max_total_bytes.saturating_sub(total_written);
        let mut limited = entry.take(remaining.saturating_add(1));
        let mut outfile = File::create(&out_path).map_err(|source| CoreError::Io {
            path: out_path.clone(),
            source,
        })?;
        let copied = std::io::copy(&mut limited, &mut outfile).map_err(|source| CoreError::Io {
            path: out_path.clone(),
            source,
        })?;
        total_written = total_written.saturating_add(copied);
        if total_written > max_total_bytes {
            return Err(CoreError::ArchiveSizeExceeded {
                entry: entry_name,
                max_bytes: max_total_bytes,
            });
        }

        #[cfg(unix)]
        if let Some(mode) = unix_mode {
            use std::os::unix::fs::PermissionsExt;
            let perms = if mode & 0o111 != 0 { 0o755 } else { 0o644 };
            fs::set_permissions(&out_path, fs::Permissions::from_mode(perms)).map_err(
                |source| CoreError::Io {
                    path: out_path.clone(),
                    source,
                },
            )?;
        }

        extracted.push(rel_path);
    }

    Ok(extracted)
}

/// Moves `staging` to `final_dest`, parking any previous `final_dest` aside and restoring it if
/// the swap fails. The previous contents are deleted only after the new tree is in place.
fn swap_into_place(
    staging: &Path,
    final_dest: &Path,
    parent: &Path,
    dest_name: &str,
) -> Result<(), CoreError> {
    let io_err = |path: &Path| {
        let path = path.to_path_buf();
        move |source| CoreError::Io { path, source }
    };

    if fs::symlink_metadata(final_dest).is_err() {
        return fs::rename(staging, final_dest).map_err(io_err(final_dest));
    }

    let backup = parent.join(format!(".{dest_name}.previous-{}", Uuid::new_v4()));
    fs::rename(final_dest, &backup).map_err(io_err(final_dest))?;
    if let Err(source) = fs::rename(staging, final_dest) {
        let _ = fs::rename(&backup, final_dest);
        return Err(CoreError::Io {
            path: final_dest.to_path_buf(),
            source,
        });
    }

    let removed = match fs::symlink_metadata(&backup) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(&backup),
        Ok(_) => fs::remove_file(&backup),
        Err(err) => Err(err),
    };
    if let Err(err) = removed {
        warn!(path = %backup.display(), error = %err, "failed to remove previous install");
    }
    Ok(())
}

fn ensure_inside_canonical_root(
    target: &Path,
    canonical_root: &Path,
    entry_name: &str,
) -> Result<(), CoreError> {
    let canonical = fs::canonicalize(target).map_err(|source| CoreError::Io {
        path: target.to_path_buf(),
        source,
    })?;
    if !canonical.starts_with(canonical_root) {
        return Err(CoreError::UnsafeArchiveEntry {
            entry: entry_name.to_owned(),
        });
    }
    Ok(())
}
