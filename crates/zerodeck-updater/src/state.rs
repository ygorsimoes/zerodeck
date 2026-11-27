use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use semver::Version;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zerodeck_platform::atomic_write;

use crate::{AuthorizedRelease, Channel, GenerationId, UpdateError};

/// Current on-disk [`UpdateState`] schema version.
pub const UPDATE_STATE_SCHEMA: u32 = 1;

/// Current on-disk [`CatalogState`] schema version.
pub const CATALOG_STATE_SCHEMA: u32 = 1;

/// Durable state shared by the update, apply, and recovery helpers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateState {
    /// Schema version of this document.
    #[serde(default)]
    pub schema: u32,
    /// The healthy generation that is currently installed.
    #[serde(default)]
    pub last_confirmed: Option<GenerationId>,
    /// The most recent distinct healthy generation, retained for offline recovery.
    #[serde(default)]
    pub rollback_target: Option<GenerationId>,
    /// Digests rejected by a failed trial and never selected again without review.
    #[serde(default)]
    pub quarantined: Vec<GenerationId>,
    /// The single in-flight activation transaction, when one exists.
    #[serde(default)]
    pub pending: Option<PendingTransaction>,
    /// Redacted terminal result of the most recent activation or recovery, for `status`.
    #[serde(default)]
    pub last_result: Option<String>,
    /// Redacted failure reason of the most recent activation or recovery, for `status`.
    #[serde(default)]
    pub last_error: Option<String>,
}

impl UpdateState {
    /// Creates an empty state document at the current schema version.
    #[must_use]
    pub fn new() -> Self {
        Self {
            schema: UPDATE_STATE_SCHEMA,
            last_confirmed: None,
            rollback_target: None,
            quarantined: Vec::new(),
            pending: None,
            last_result: None,
            last_error: None,
        }
    }

    /// Commits the pending candidate as the healthy generation.
    ///
    /// The previous `last_confirmed` becomes `rollback_target` when it differs from the candidate.
    /// Without a pending transaction this does nothing.
    pub fn confirm(&mut self) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        let previous = self.last_confirmed.replace(pending.candidate.clone());
        if let Some(previous) = previous
            && previous != pending.candidate
        {
            self.rollback_target = Some(previous);
        }
    }

    /// Abandons a failed trial without changing the confirmed or rollback generations.
    pub fn revert(&mut self) {
        self.pending = None;
    }

    /// Records a rejected digest, keeping the quarantine list free of duplicates.
    pub fn quarantine(&mut self, generation: GenerationId) {
        if !self.quarantined.contains(&generation) {
            self.quarantined.push(generation);
        }
    }
}

impl Default for UpdateState {
    fn default() -> Self {
        Self::new()
    }
}

/// One durable activation attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingTransaction {
    /// Unique operation identifier for idempotent retries.
    pub id: Uuid,
    /// The healthy active generation before the trial.
    #[serde(default)]
    pub previous: Option<GenerationId>,
    /// The generation being tried.
    pub candidate: GenerationId,
    /// Last completed activation step.
    pub phase: TransactionPhase,
    /// Whether the daemon was running before the apply stopped it.
    #[serde(default)]
    pub was_running: bool,
    /// Whether the candidate was durably attempted for startup.
    ///
    /// Pre-start failures remain recoverable without quarantining the candidate.
    #[serde(default)]
    pub start_attempted: bool,
}

/// Durable progress markers for a pending activation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionPhase {
    /// The transaction intent is durable, before the service is stopped.
    Prepared,
    /// The daemon was stopped for activation.
    Stopped,
    /// The active reference was switched to the candidate.
    Switched,
    /// The candidate is starting and awaiting readiness confirmation.
    Verifying,
    /// Readiness succeeded and the state commit is pending.
    Confirmed,
    /// A failed trial is being reverted to the previous generation.
    Reverting,
}

/// JSON persistence for [`UpdateState`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateStore {
    path: PathBuf,
}

impl StateStore {
    /// Creates a state store backed by `path`.
    #[must_use]
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }

    /// Returns the backing state file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads and validates the persisted state, returning an empty document when absent.
    pub fn read(&self) -> Result<UpdateState, UpdateError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                return Ok(UpdateState::new());
            }
            Err(source) => {
                return Err(UpdateError::Io {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        let state: UpdateState = serde_json::from_slice(&bytes)?;
        if state.schema != UPDATE_STATE_SCHEMA {
            return Err(UpdateError::UnsupportedSchema(state.schema));
        }
        Ok(state)
    }

    /// Atomically persists `state` after rejecting an unknown schema.
    pub fn write(&self, state: &UpdateState) -> Result<(), UpdateError> {
        if state.schema != UPDATE_STATE_SCHEMA {
            return Err(UpdateError::UnsupportedSchema(state.schema));
        }
        let mut bytes = serde_json::to_vec_pretty(state)?;
        bytes.push(b'\n');
        atomic_write(&self.path, &bytes)?;
        Ok(())
    }
}

/// Persisted outcome of the freshest successful trusted catalog verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogState {
    /// Schema version of this document.
    #[serde(default)]
    pub schema: u32,
    /// Unix seconds of the last successful trusted metadata verification.
    #[serde(default)]
    pub last_verified_at: Option<u64>,
    /// Unix seconds at which the trusted metadata expires.
    #[serde(default)]
    pub expires_at: Option<u64>,
    /// The release selected from the freshest trusted metadata, if any.
    #[serde(default)]
    pub authorization: Option<AuthorizedRelease>,
    /// The selection inputs that produced `authorization`.
    #[serde(default)]
    pub selection: Option<SelectionRecord>,
}

/// Persisted selection policy for the currently authorized release.
///
/// `enqueue_upgrade` rechecks the signed channel, the running updater version, and the installed
/// generation against this record. A Stable client therefore cannot silently activate a Testing
/// selection, a downgrade, or a release that requires a newer controller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionRecord {
    /// Channel the operator explicitly selected for this authorization.
    pub channel: Channel,
    /// Updater version that made the selection.
    pub updater_version: Version,
    /// Installed version observed at selection time.
    #[serde(default)]
    pub installed_version: Option<Version>,
    /// Installed generation digest observed at selection time.
    #[serde(default)]
    pub installed_digest: Option<GenerationId>,
}

impl CatalogState {
    /// Creates an empty catalog state document at the current schema version.
    #[must_use]
    pub fn new() -> Self {
        Self {
            schema: CATALOG_STATE_SCHEMA,
            last_verified_at: None,
            expires_at: None,
            authorization: None,
            selection: None,
        }
    }
}

impl Default for CatalogState {
    fn default() -> Self {
        Self::new()
    }
}

/// JSON persistence for [`CatalogState`] under the owned update state root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogStateStore {
    path: PathBuf,
}

impl CatalogStateStore {
    /// Creates a catalog state store backed by `path`.
    #[must_use]
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }

    /// Returns the backing state file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads and validates the persisted catalog state, returning an empty document when absent.
    pub fn read(&self) -> Result<CatalogState, UpdateError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                return Ok(CatalogState::new());
            }
            Err(source) => {
                return Err(UpdateError::Io {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        let state: CatalogState = serde_json::from_slice(&bytes)?;
        if state.schema != CATALOG_STATE_SCHEMA {
            return Err(UpdateError::UnsupportedSchema(state.schema));
        }
        Ok(state)
    }

    /// Atomically persists `state` after rejecting an unknown schema.
    pub fn write(&self, state: &CatalogState) -> Result<(), UpdateError> {
        if state.schema != CATALOG_STATE_SCHEMA {
            return Err(UpdateError::UnsupportedSchema(state.schema));
        }
        let mut bytes = serde_json::to_vec_pretty(state)?;
        bytes.push(b'\n');
        atomic_write(&self.path, &bytes)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use uuid::Uuid;

    use super::{PendingTransaction, StateStore, TransactionPhase, UpdateState};
    use crate::{GenerationId, UpdateError};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "zerodeck-state-{label}-{}-{}",
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

    fn generation_id(fill: char) -> GenerationId {
        GenerationId::new(fill.to_string().repeat(64)).expect("valid generation id")
    }

    fn pending(previous: Option<GenerationId>, candidate: GenerationId) -> PendingTransaction {
        PendingTransaction {
            id: Uuid::new_v4(),
            previous,
            candidate,
            phase: TransactionPhase::Switched,
            was_running: true,
            start_attempted: true,
        }
    }

    #[test]
    fn confirm_promotes_candidate_and_rotates_rollback_target() {
        let previous = generation_id('a');
        let candidate = generation_id('b');
        let mut state = UpdateState::new();
        state.last_confirmed = Some(previous.clone());
        state.pending = Some(pending(Some(previous.clone()), candidate.clone()));

        state.confirm();

        assert_eq!(state.last_confirmed, Some(candidate));
        assert_eq!(state.rollback_target, Some(previous));
        assert!(state.pending.is_none());
    }

    #[test]
    fn confirm_of_the_same_generation_keeps_a_distinct_rollback_target() {
        let current = generation_id('a');
        let rollback = generation_id('b');
        let mut state = UpdateState::new();
        state.last_confirmed = Some(current.clone());
        state.rollback_target = Some(rollback.clone());
        state.pending = Some(pending(Some(current.clone()), current.clone()));

        state.confirm();

        assert_eq!(state.last_confirmed, Some(current));
        assert_eq!(state.rollback_target, Some(rollback));
    }

    #[test]
    fn revert_leaves_confirmed_and_rollback_target_unchanged() {
        let confirmed = generation_id('a');
        let rollback = generation_id('b');
        let candidate = generation_id('c');
        let mut state = UpdateState::new();
        state.last_confirmed = Some(confirmed.clone());
        state.rollback_target = Some(rollback.clone());
        state.pending = Some(pending(Some(confirmed.clone()), candidate));

        state.revert();

        assert_eq!(state.last_confirmed, Some(confirmed));
        assert_eq!(state.rollback_target, Some(rollback));
        assert!(state.pending.is_none());
    }

    #[test]
    fn state_store_defaults_missing_state_and_roundtrips() {
        let dir = TestDir::new("roundtrip");
        let store = StateStore::new(dir.0.join("state.json"));
        let empty = store.read().expect("missing state defaults");
        assert_eq!(empty, UpdateState::new());

        let mut state = UpdateState::new();
        state.last_confirmed = Some(generation_id('a'));
        state.rollback_target = Some(generation_id('b'));
        state.quarantined.push(generation_id('c'));
        state.pending = Some(pending(state.last_confirmed.clone(), generation_id('d')));
        store.write(&state).expect("persist state");
        assert_eq!(store.read().expect("read persisted state"), state);
    }

    #[test]
    fn state_store_rejects_unknown_schema() {
        let dir = TestDir::new("schema");
        let path = dir.0.join("state.json");
        fs::write(&path, br#"{"schema": 2}"#).expect("seed unknown schema");
        let store = StateStore::new(&path);

        let err = store.read().expect_err("unknown schema must fail");
        assert!(matches!(err, UpdateError::UnsupportedSchema(2)));

        let mut state = UpdateState::new();
        state.schema = 2;
        let err = store
            .write(&state)
            .expect_err("unknown schema must not persist");
        assert!(matches!(err, UpdateError::UnsupportedSchema(2)));
    }
}
