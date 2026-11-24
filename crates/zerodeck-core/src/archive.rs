use std::fs::{self, File};
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tracing::info;
use zip::ZipArchive;

use crate::CoreError;

/// Default maximum total uncompressed bytes permitted when extracting a plugin archive (512 MiB).
pub const DEFAULT_MAX_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;

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

/// Extracts a ZIP archive into `dest_dir` with default size cap (`512 MiB`), rejecting Zip-Slip and symlinks.
pub fn extract_zip_safely(bytes: &[u8], dest_dir: &Path) -> Result<Vec<PathBuf>, CoreError> {
    extract_zip_safely_with_limit(bytes, dest_dir, DEFAULT_MAX_ARCHIVE_BYTES)
}

/// Extracts a ZIP archive into `dest_dir` with a bounded total uncompressed byte limit.
pub fn extract_zip_safely_with_limit(
    bytes: &[u8],
    dest_dir: &Path,
    max_total_bytes: u64,
) -> Result<Vec<PathBuf>, CoreError> {
    let mut archive = ZipArchive::new(Cursor::new(bytes))?;
    let mut entries = Vec::with_capacity(archive.len());
    let mut declared_total: u64 = 0;

    // Pre-validate all entries before writing anything to `dest_dir`.
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

    fs::create_dir_all(dest_dir).map_err(|source| CoreError::Io {
        path: dest_dir.to_path_buf(),
        source,
    })?;
    let canonical_dest = fs::canonicalize(dest_dir).map_err(|source| CoreError::Io {
        path: dest_dir.to_path_buf(),
        source,
    })?;

    let mut extracted = Vec::with_capacity(entries.len());
    let mut total_written: u64 = 0;

    for (idx, rel_path) in entries {
        let entry = archive.by_index(idx)?;
        let entry_name = String::from_utf8_lossy(entry.name_raw()).into_owned();
        let unix_mode = entry.unix_mode();
        let out_path = canonical_dest.join(&rel_path);

        if entry.is_dir() {
            fs::create_dir_all(&out_path).map_err(|source| CoreError::Io {
                path: out_path.clone(),
                source,
            })?;
            ensure_inside_canonical_root(&out_path, &canonical_dest, &entry_name)?;
        } else {
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent).map_err(|source| CoreError::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
                ensure_inside_canonical_root(parent, &canonical_dest, &entry_name)?;
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
            let copied =
                std::io::copy(&mut limited, &mut outfile).map_err(|source| CoreError::Io {
                    path: out_path.clone(),
                    source,
                })?;
            total_written = total_written.saturating_add(copied);
            if total_written > max_total_bytes {
                let _ = fs::remove_file(&out_path);
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

            extracted.push(out_path);
        }
    }

    Ok(extracted)
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
