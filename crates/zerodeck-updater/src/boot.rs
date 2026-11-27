//! Per-boot recovery marker consulted by the managed launcher.
//!
//! systemd orders `zerodeck-recovery.service` before `zerodeck.service`, but ordering alone does not
//! stop the daemon when recovery failed (for example because the lock was still held). The launcher
//! therefore refuses to exec a generation unless disk reconciliation completed for the current
//! kernel boot.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::schema::{Schema, SchemaFile};
use crate::{GenerationStore, UpdateError};

const BOOT_MARKER_SCHEMA: u32 = 1;
const BOOT_MARKER_FILE: &str = "state/boot-recovery.json";
const BOOT_ID_PATH: &str = "/proc/sys/kernel/random/boot_id";

#[derive(Debug, Serialize, Deserialize)]
struct BootMarker {
    #[serde(default)]
    schema: u32,
    boot_id: String,
}

impl Schema for BootMarker {
    const SCHEMA: u32 = BOOT_MARKER_SCHEMA;

    fn schema(&self) -> u32 {
        self.schema
    }
}

/// Records and verifies the disk-recovery completion marker for the current boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootRecoveryMarker;

impl BootRecoveryMarker {
    /// Records a successful disk reconciliation for the current boot.
    ///
    /// On hosts without a kernel boot ID (non-Linux development), this is a no-op: the managed
    /// daemon is a Linux/systemd deployment, so there is nothing to bind to.
    pub fn record(store: &GenerationStore) -> Result<(), UpdateError> {
        let Some(boot_id) = boot_id_from(Path::new(BOOT_ID_PATH))? else {
            return Ok(());
        };
        Self::record_boot_id(store, &boot_id)
    }

    /// Records `boot_id` as reconciled. Exposed for tests and tooling.
    #[doc(hidden)]
    pub fn record_boot_id(store: &GenerationStore, boot_id: &str) -> Result<(), UpdateError> {
        marker_store(store).write(&BootMarker {
            schema: BOOT_MARKER_SCHEMA,
            boot_id: boot_id.to_owned(),
        })
    }

    /// Requires that recovery completed during this boot before the daemon starts.
    ///
    /// Fails closed on Linux when the kernel boot ID cannot be read.
    pub fn verify(store: &GenerationStore) -> Result<(), UpdateError> {
        Self::verify_with_boot_id_path(store, Path::new(BOOT_ID_PATH))
    }

    /// [`Self::verify`] against an explicit boot-ID file. Exposed for tests.
    #[doc(hidden)]
    pub fn verify_with_boot_id_path(
        store: &GenerationStore,
        boot_id_path: &Path,
    ) -> Result<(), UpdateError> {
        let Some(boot_id) = boot_id_from(boot_id_path)? else {
            return Ok(());
        };
        match marker_store(store).read_optional()? {
            Some(marker) if marker.boot_id == boot_id => Ok(()),
            _ => Err(UpdateError::BootRecoveryRequired),
        }
    }
}

fn marker_store(store: &GenerationStore) -> SchemaFile<BootMarker> {
    SchemaFile::new(PathBuf::from(store.root()).join(BOOT_MARKER_FILE))
}

/// Reads the kernel boot ID. A missing file is "no boot ID" off Linux and a hard error on Linux,
/// so a host that cannot prove which boot recovered never starts the daemon on a stale marker.
fn boot_id_from(path: &Path) -> Result<Option<String>, UpdateError> {
    match fs::read_to_string(path) {
        Ok(value) if !value.trim().is_empty() => Ok(Some(value.trim().to_owned())),
        Ok(_) | Err(_) if cfg!(not(target_os = "linux")) => Ok(None),
        Ok(_) => Err(UpdateError::BootIdUnavailable(path.to_path_buf())),
        Err(source) => {
            let _ = source;
            Err(UpdateError::BootIdUnavailable(path.to_path_buf()))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use std::path::Path;

    use super::BootRecoveryMarker;
    use crate::{GenerationStore, UpdateError};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "zerodeck-boot-{label}-{}-{}",
                std::process::id(),
                NEXT_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("create temp test dir");
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_marker_for_another_boot_does_not_satisfy_recovery() {
        if !Path::new(super::BOOT_ID_PATH).exists() {
            eprintln!("skipping: no kernel boot id on this host");
            return;
        }
        let dir = TestDir::new("stale");
        let store = GenerationStore::new(&dir.0).expect("open store");

        let error = BootRecoveryMarker::verify(&store).expect_err("missing marker must fail");
        assert!(matches!(error, UpdateError::BootRecoveryRequired));

        BootRecoveryMarker::record_boot_id(&store, "a-different-boot").expect("write stale marker");
        let error = BootRecoveryMarker::verify(&store).expect_err("stale marker must fail");
        assert!(matches!(error, UpdateError::BootRecoveryRequired));

        BootRecoveryMarker::record(&store).expect("record this boot");
        BootRecoveryMarker::verify(&store).expect("current boot marker passes");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn boot_marker_fails_closed_when_boot_id_unreadable() {
        let dir = TestDir::new("no-boot-id");
        let store = GenerationStore::new(&dir.0).expect("open store");
        let missing = dir.0.join("no-such-boot-id");

        let error = BootRecoveryMarker::verify_with_boot_id_path(&store, &missing)
            .expect_err("an unreadable boot id must not satisfy recovery");
        assert!(matches!(error, UpdateError::BootIdUnavailable(_)));
    }
}
