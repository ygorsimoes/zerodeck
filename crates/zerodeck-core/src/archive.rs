use std::fs::{self, File};
use std::io::Cursor;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tracing::info;
use zip::ZipArchive;

use crate::CoreError;

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
            if sha256.as_deref().is_none_or(str::is_empty) && !allow_unverified {
                return Err(CoreError::MissingSha256);
            }
            sha256.as_deref().filter(|s| !s.is_empty())
        }
        InstallSource::LocalFile {
            sha256,
            user_confirmed,
        } => {
            let trimmed = sha256.as_deref().filter(|s| !s.is_empty());
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

/// Extracts a ZIP archive into `dest_dir`, rejecting Zip-Slip traversal and symlink entries.
pub fn extract_zip_safely(bytes: &[u8], dest_dir: &Path) -> Result<Vec<PathBuf>, CoreError> {
    let mut archive = ZipArchive::new(Cursor::new(bytes))?;
    let mut entries = Vec::with_capacity(archive.len());

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
        entries.push((idx, rel_path));
    }

    fs::create_dir_all(dest_dir).map_err(|source| CoreError::Io {
        path: dest_dir.to_path_buf(),
        source,
    })?;

    let mut extracted = Vec::with_capacity(entries.len());
    for (idx, rel_path) in entries {
        let mut entry = archive.by_index(idx)?;
        let out_path = dest_dir.join(&rel_path);

        if entry.is_dir() {
            fs::create_dir_all(&out_path).map_err(|source| CoreError::Io {
                path: out_path.clone(),
                source,
            })?;
        } else {
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent).map_err(|source| CoreError::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
            let mut outfile = File::create(&out_path).map_err(|source| CoreError::Io {
                path: out_path.clone(),
                source,
            })?;
            std::io::copy(&mut entry, &mut outfile).map_err(|source| CoreError::Io {
                path: out_path.clone(),
                source,
            })?;

            #[cfg(unix)]
            if let Some(mode) = entry.unix_mode() {
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
