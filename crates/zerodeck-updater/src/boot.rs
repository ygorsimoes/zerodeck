//! Per-boot recovery marker consulted by the managed launcher.
//!
//! systemd orders `zerodeck-recovery.service` before `zerodeck.service`, but ordering alone does not
//! stop the daemon when recovery failed (for example because the lock was still held). The launcher
//! therefore refuses to exec a generation unless disk reconciliation completed for the current
//! kernel boot.

use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use zerodeck_platform::atomic_write;

use crate::{GenerationStore, UpdateError};

const BOOT_MARKER_SCHEMA: u32 = 1;
const BOOT_MARKER_FILE: &str = "state/boot-recovery.json";
const BOOT_ID_PATH: &str = "/proc/sys/kernel/random/boot_id";

#[derive(Debug, Serialize, Deserialize)]
struct BootMarker {
    schema: u32,
    boot_id: String,
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
        let Some(boot_id) = current_boot_id() else {
            return Ok(());
        };
        Self::record_boot_id(store, &boot_id)
    }

    /// Records `boot_id` as reconciled. Exposed for tests and tooling.
    #[doc(hidden)]
    pub fn record_boot_id(store: &GenerationStore, boot_id: &str) -> Result<(), UpdateError> {
        let marker = BootMarker {
            schema: BOOT_MARKER_SCHEMA,
            boot_id: boot_id.to_owned(),
        };
        let mut bytes = serde_json::to_vec_pretty(&marker)?;
        bytes.push(b'\n');
        let path = marker_path(store);
        atomic_write(&path, &bytes)?;
        Ok(())
    }

    /// Requires that recovery completed during this boot before the daemon starts.
    pub fn verify(store: &GenerationStore) -> Result<(), UpdateError> {
        let Some(boot_id) = current_boot_id() else {
            return Ok(());
        };
        let path = marker_path(store);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                return Err(UpdateError::BootRecoveryRequired);
            }
            Err(source) => {
                return Err(UpdateError::Io { path, source });
            }
        };
        let marker: BootMarker = serde_json::from_slice(&bytes)?;
        if marker.schema != BOOT_MARKER_SCHEMA || marker.boot_id != boot_id {
            return Err(UpdateError::BootRecoveryRequired);
        }
        Ok(())
    }
}

fn marker_path(store: &GenerationStore) -> PathBuf {
    store.root().join(BOOT_MARKER_FILE)
}

fn current_boot_id() -> Option<String> {
    let value = fs::read_to_string(BOOT_ID_PATH).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

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
        if super::current_boot_id().is_none() {
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
}
