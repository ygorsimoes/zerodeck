use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use uuid::Uuid;

use crate::PlatformError;

/// Atomically writes `contents` to `path` via a sibling temp file, `sync_all`, and `rename`.
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

    let write_res = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)?;
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

    Ok(())
}
