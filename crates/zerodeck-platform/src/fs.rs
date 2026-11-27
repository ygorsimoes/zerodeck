use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;

use uuid::Uuid;

use crate::PlatformError;

/// Atomically writes `contents` to `path` via a sibling temp file, `sync_all`, and `rename`.
///
/// An existing target keeps its permission bits, and on Unix the parent directory is synced after
/// the rename so the new directory entry itself survives a power loss.
pub fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), PlatformError> {
    atomic_write_inner(path, contents, None)
}

/// Like [`atomic_write`], but the temp file gets `mode` before any byte is written, so the final
/// file never exists with wider permissions than requested (even briefly).
#[cfg(unix)]
pub fn atomic_write_mode(path: &Path, contents: &[u8], mode: u32) -> Result<(), PlatformError> {
    atomic_write_inner(path, contents, Some(mode))
}

fn atomic_write_inner(
    path: &Path,
    contents: &[u8],
    mode: Option<u32>,
) -> Result<(), PlatformError> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    fs::create_dir_all(parent).map_err(|source| PlatformError::Io {
        path: parent.to_path_buf(),
        source,
    })?;

    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("zerodeck");
    let tmp_path = parent.join(format!(".{file_name}.tmp.{}", Uuid::new_v4()));
    let existing_permissions = fs::metadata(path).ok().map(|meta| meta.permissions());

    let write_res = (|| -> std::io::Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(mode);
        }
        let mut file = options.open(&tmp_path)?;
        if let (None, Some(permissions)) = (mode, existing_permissions) {
            file.set_permissions(permissions)?;
        }
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp_path, path)?;
        Ok(())
    })();

    if let Err(source) = write_res {
        let _ = fs::remove_file(&tmp_path);
        return Err(PlatformError::Io {
            path: path.to_path_buf(),
            source,
        });
    }

    sync_dir(parent)
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> Result<(), PlatformError> {
    File::open(dir)
        .and_then(|handle| handle.sync_all())
        .map_err(|source| PlatformError::Io {
            path: dir.to_path_buf(),
            source,
        })
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
fn sync_dir(_dir: &Path) -> Result<(), PlatformError> {
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use uuid::Uuid;

    use super::atomic_write_mode;

    #[test]
    fn atomic_write_mode_sets_mode_before_rename() {
        let dir = std::env::temp_dir().join(format!("zerodeck-fs-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("create dir");
        let target = dir.join("secret.json");

        atomic_write_mode(&target, b"{}", 0o600).expect("write");

        let mode = fs::metadata(&target).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .expect("list")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name() != "secret.json")
            .collect();
        assert!(leftovers.is_empty(), "temp files remain: {leftovers:?}");
        fs::remove_dir_all(&dir).expect("cleanup");
    }
}
