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
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)?;
        if let Some(permissions) = existing_permissions {
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
