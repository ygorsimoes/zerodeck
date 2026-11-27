use std::collections::BTreeSet;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use semver::Version;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;
use zerodeck_platform::LayoutError;

use crate::{ReleaseIdentity, UpdateError, UpdateState};

const GENERATIONS_DIR: &str = "generations";
const CURRENT_FILE: &str = "current";
const MAINTENANCE_LOCK_FILE: &str = "maintenance.lock";

/// A validated lowercase SHA-256 generation digest.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GenerationId(String);

impl GenerationId {
    /// Validates a lowercase 64-character SHA-256 hex digest.
    pub fn new(value: impl Into<String>) -> Result<Self, UpdateError> {
        let value = value.into();
        let valid = value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if !valid {
            return Err(UpdateError::InvalidGenerationId(value));
        }
        Ok(Self(value))
    }

    /// Returns the digest as a lowercase hex string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for GenerationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl AsRef<str> for GenerationId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl FromStr for GenerationId {
    type Err = UpdateError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for GenerationId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for GenerationId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Release identity recorded in an immutable generation's `release.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationRelease {
    build_id: String,
    version: Version,
}

impl GenerationRelease {
    /// Release version packaged in the generation.
    #[must_use]
    pub fn version(&self) -> &Version {
        &self.version
    }

    /// Embedded build identity packaged in the generation.
    #[must_use]
    pub fn build_id(&self) -> &str {
        &self.build_id
    }
}

/// A validated immutable generation and the paths owned by it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationLayout {
    root: PathBuf,
    binary: PathBuf,
    release: GenerationRelease,
}

impl GenerationLayout {
    /// Canonical generation directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The packaged `bin/zerodeckd` executable.
    #[must_use]
    pub fn binary(&self) -> &Path {
        &self.binary
    }

    /// The release identity recorded in `release.json`.
    #[must_use]
    pub fn release(&self) -> &GenerationRelease {
        &self.release
    }

    /// Expected readiness identity for this generation.
    #[must_use]
    pub fn identity(&self) -> ReleaseIdentity {
        ReleaseIdentity::new(self.release.version.clone(), self.release.build_id.clone())
    }
}

/// Immutable loader generations under one owned root and their active reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationStore {
    root: PathBuf,
    generations: PathBuf,
    current: PathBuf,
}

impl GenerationStore {
    /// Opens the generation store rooted at `root`, creating its generations directory.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, UpdateError> {
        Self::open(root.as_ref(), true)
    }

    /// Opens an existing generation store without creating or repairing directories.
    ///
    /// `zerodeckctl status` uses this so a read-only summary never mutates the host layout.
    pub fn open_existing(root: impl AsRef<Path>) -> Result<Self, UpdateError> {
        Self::open(root.as_ref(), false)
    }

    fn open(root: &Path, create: bool) -> Result<Self, UpdateError> {
        let requested = root;
        let root = fs::canonicalize(requested).map_err(|source| UpdateError::Io {
            path: requested.to_path_buf(),
            source,
        })?;
        // A root daemon reads release files from here. Never accept a root reachable through a
        // user-writable or symlinked ancestor; unprivileged development runs are exempt.
        #[cfg(unix)]
        zerodeck_platform::ensure_root_owned_ancestors(&root)
            .map_err(|violation| UpdateError::UnsafeRoot(violation.to_string()))?;

        let generations_path = root.join(GENERATIONS_DIR);
        if create {
            fs::create_dir_all(&generations_path).map_err(|source| UpdateError::Io {
                path: generations_path.clone(),
                source,
            })?;
        }
        // `status` opens an existing root without creating anything. A missing generations
        // directory is an empty store, not a reason to fail the read-only summary.
        let generations = if !create && !generations_path.exists() {
            generations_path
        } else {
            fs::canonicalize(&generations_path).map_err(|source| UpdateError::Io {
                path: generations_path.clone(),
                source,
            })?
        };
        if !generations.starts_with(&root) {
            return Err(UpdateError::PathEscape {
                path: generations,
                root,
            });
        }

        Ok(Self {
            current: root.join(CURRENT_FILE),
            root,
            generations,
        })
    }

    /// Returns the canonical update root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the maintenance lock shared with `zerodeckctl update` and recovery.
    #[must_use]
    pub fn lock_path(&self) -> PathBuf {
        self.root.join(MAINTENANCE_LOCK_FILE)
    }

    /// Returns the directory reserved for `generation`.
    #[must_use]
    pub fn generation_dir(&self, generation: &GenerationId) -> PathBuf {
        self.generations.join(generation.as_str())
    }

    /// Reads and validates the active generation reference without requiring the generation.
    ///
    /// Recovery uses this to observe an interrupted switch even when the target directory is
    /// missing or corrupt.
    pub fn current_reference(&self) -> Result<Option<GenerationId>, UpdateError> {
        let reference = match fs::read_to_string(&self.current) {
            Ok(reference) => reference,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(UpdateError::Io {
                    path: self.current.clone(),
                    source,
                });
            }
        };
        GenerationId::new(reference.trim()).map(Some)
    }

    /// Reads the active generation reference, or `None` before the first switch.
    pub fn current(&self) -> Result<Option<GenerationId>, UpdateError> {
        let Some(generation) = self.current_reference()? else {
            return Ok(None);
        };
        self.ensure_generation(&generation)?;
        Ok(Some(generation))
    }

    /// Removes the active reference so the daemon unit cannot start.
    ///
    /// This is the fail-closed terminal state when activation failed and no good previous
    /// generation exists. It is idempotent and leaves every generation directory untouched.
    pub fn clear_current(&self) -> Result<(), UpdateError> {
        match fs::remove_file(&self.current) {
            Ok(()) => {}
            Err(source) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(UpdateError::Io {
                    path: self.current.clone(),
                    source,
                });
            }
        }
        sync_directory(&self.root).map_err(|source| UpdateError::Io {
            path: self.root.clone(),
            source,
        })
    }

    /// Verifies the complete immutable layout of `generation`.
    ///
    /// The generation directory, binary, Python host and frontend asset directories must be real
    /// (not symlinks), stay inside the generation root, and the manifest must be schema 1.
    pub fn verify_generation(
        &self,
        generation: &GenerationId,
    ) -> Result<GenerationLayout, UpdateError> {
        let requested = self.generation_dir(generation);
        let metadata = match fs::symlink_metadata(&requested) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                return Err(UpdateError::MissingGeneration(generation.clone()));
            }
            Err(source) => {
                return Err(UpdateError::Io {
                    path: requested.clone(),
                    source,
                });
            }
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(UpdateError::CorruptGeneration(generation.clone()));
        }
        let root = fs::canonicalize(&requested).map_err(|source| UpdateError::Io {
            path: requested.clone(),
            source,
        })?;
        if !root.starts_with(&self.generations) {
            return Err(UpdateError::PathEscape {
                path: root,
                root: self.generations.clone(),
            });
        }

        let platform = zerodeck_platform::GenerationLayout::open(&root)
            .map_err(|error| layout_error(generation, error))?;
        let manifest = platform.manifest();
        let version = Version::parse(&manifest.version)
            .map_err(|error| UpdateError::InvalidReleaseManifest(error.to_string()))?;

        Ok(GenerationLayout {
            root,
            binary: platform.daemon().to_path_buf(),
            release: GenerationRelease {
                build_id: manifest.build_id.clone(),
                version,
            },
        })
    }

    /// Removes a generation directory without ever leaving a half-deleted tree under its name.
    ///
    /// The directory is renamed to `.trash-<uuid>` first (atomic), so a crash during the recursive
    /// delete leaves only trash that [`Self::sweep`] removes later.
    pub(crate) fn discard(&self, path: &Path) -> Result<(), UpdateError> {
        let trash = self.generations.join(format!(".trash-{}", Uuid::new_v4()));
        fs::rename(path, &trash).map_err(|source| UpdateError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        sync_directory(&self.generations).map_err(|source| UpdateError::Io {
            path: self.generations.clone(),
            source,
        })?;
        fs::remove_dir_all(&trash).map_err(|source| UpdateError::Io {
            path: trash,
            source,
        })
    }

    /// Removes leftovers of interrupted operations: `.trash-*` from a crashed garbage collection
    /// and `.staging-*` from an interrupted extraction.
    ///
    /// Callers hold the exclusive maintenance lock, which every staging operation also holds.
    pub fn sweep(&self) -> Result<(), UpdateError> {
        let entries = fs::read_dir(&self.generations).map_err(|source| UpdateError::Io {
            path: self.generations.clone(),
            source,
        })?;
        for entry in entries {
            let entry = entry.map_err(|source| UpdateError::Io {
                path: self.generations.clone(),
                source,
            })?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !(name.starts_with(".trash-") || name.starts_with(".staging-")) {
                continue;
            }
            let path = entry.path();
            let is_dir = fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir());
            let removed = if is_dir {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            };
            removed.map_err(|source| UpdateError::Io { path, source })?;
        }
        Ok(())
    }

    /// Atomically switches the active reference to `generation`.
    pub fn switch_to(&self, generation: &GenerationId) -> Result<(), UpdateError> {
        self.switch_to_with(generation, sync_directory)
    }

    /// Switches the active reference using `sync_directory` as the durability barrier.
    ///
    /// This exists so fault tests can observe the window where the rename is already visible but
    /// its directory sync fails. Production callers use [`Self::switch_to`].
    #[doc(hidden)]
    pub fn switch_to_with<F>(
        &self,
        generation: &GenerationId,
        sync_directory: F,
    ) -> Result<(), UpdateError>
    where
        F: FnOnce(&Path) -> io::Result<()>,
    {
        self.ensure_generation(generation)?;

        let temp = self
            .root
            .join(format!(".{CURRENT_FILE}.tmp.{}", Uuid::new_v4()));
        let write_result = (|| -> io::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            file.write_all(generation.as_str().as_bytes())?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temp, &self.current)?;
            Ok(())
        })();

        if let Err(source) = write_result {
            let _ = fs::remove_file(&temp);
            return Err(UpdateError::Io {
                path: self.current.clone(),
                source,
            });
        }

        sync_directory(&self.root).map_err(|source| UpdateError::Io {
            path: self.root.clone(),
            source,
        })
    }

    /// Deletes generations that are not confirmed, a rollback target, pending, active, or staged.
    ///
    /// Persisted state can lag the active reference after an interrupted switch, so `current` is
    /// read from disk instead of inferred from `last_confirmed`. `staged` carries an authorized
    /// candidate that has not been activated yet. Callers must hold the exclusive maintenance
    /// guard so a concurrent stage cannot race this collection.
    pub fn gc(&self, state: &UpdateState, staged: &[GenerationId]) -> Result<(), UpdateError> {
        let mut keep = BTreeSet::new();
        if let Some(generation) = &state.last_confirmed {
            keep.insert(generation.clone());
        }
        if let Some(generation) = &state.rollback_target {
            keep.insert(generation.clone());
        }
        if let Some(pending) = &state.pending {
            if let Some(previous) = &pending.previous {
                keep.insert(previous.clone());
            }
            keep.insert(pending.candidate.clone());
        }
        if let Some(current) = self.current_reference()? {
            keep.insert(current);
        }
        keep.extend(staged.iter().cloned());
        // A quarantined digest stays on disk until an operator releases it: `unquarantine`
        // must never find the generation already deleted.
        keep.extend(state.quarantined.iter().cloned());

        let entries = fs::read_dir(&self.generations).map_err(|source| UpdateError::Io {
            path: self.generations.clone(),
            source,
        })?;
        for entry in entries {
            let entry = entry.map_err(|source| UpdateError::Io {
                path: self.generations.clone(),
                source,
            })?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(generation) = GenerationId::new(name) else {
                continue;
            };
            if keep.contains(&generation) {
                continue;
            }

            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(|source| UpdateError::Io {
                path: path.clone(),
                source,
            })?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                continue;
            }
            self.discard(&path)?;
        }
        self.sweep()?;

        sync_directory(&self.generations).map_err(|source| UpdateError::Io {
            path: self.generations.clone(),
            source,
        })
    }

    fn ensure_generation(&self, generation: &GenerationId) -> Result<(), UpdateError> {
        let path = self.generation_dir(generation);
        let canonical = match fs::canonicalize(&path) {
            Ok(canonical) => canonical,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                return Err(UpdateError::MissingGeneration(generation.clone()));
            }
            Err(source) => return Err(UpdateError::Io { path, source }),
        };

        if !canonical.starts_with(&self.generations) {
            return Err(UpdateError::PathEscape {
                path: canonical,
                root: self.root.clone(),
            });
        }
        let metadata = fs::metadata(&canonical).map_err(|source| UpdateError::Io {
            path: canonical.clone(),
            source,
        })?;
        if !metadata.is_dir() {
            return Err(UpdateError::MissingGeneration(generation.clone()));
        }
        Ok(())
    }
}

fn layout_error(generation: &GenerationId, error: LayoutError) -> UpdateError {
    match error {
        LayoutError::Manifest(error) => UpdateError::InvalidReleaseManifest(error.to_string()),
        LayoutError::Io { path, source } => UpdateError::Io { path, source },
        LayoutError::Missing(_) | LayoutError::BadEntry(_) | LayoutError::BadMode(_) => {
            UpdateError::CorruptGeneration(generation.clone())
        }
    }
}

fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use uuid::Uuid;

    use super::{GenerationId, GenerationStore};
    use crate::{PendingTransaction, TransactionPhase, UpdateError, UpdateState};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "zerodeck-generation-{label}-{}-{}",
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

    #[test]
    fn generation_id_requires_lowercase_sha256_hex() {
        assert!(GenerationId::new("a".repeat(64)).is_ok());
        for invalid in ["A".repeat(64), "a".repeat(63), "g".repeat(64)] {
            let err = GenerationId::new(invalid).expect_err("must reject invalid digest");
            assert!(matches!(err, UpdateError::InvalidGenerationId(_)));
        }
    }

    #[test]
    fn clear_current_is_idempotent_and_keeps_generation_directories() {
        let dir = TestDir::new("clear-current");
        let store = GenerationStore::new(&dir.0).expect("open generation store");
        let generation = generation_id('a');
        fs::create_dir(store.generation_dir(&generation)).expect("create generation");

        store.clear_current().expect("clear an absent reference");
        store.switch_to(&generation).expect("activate generation");
        store.clear_current().expect("clear the active reference");
        assert_eq!(store.current_reference().expect("read reference"), None);
        store.clear_current().expect("clearing again is idempotent");
        assert!(store.generation_dir(&generation).is_dir());
    }

    #[test]
    fn verify_generation_requires_the_complete_immutable_layout() {
        let dir = TestDir::new("verify-layout");
        let store = GenerationStore::new(&dir.0).expect("open generation store");
        let generation = generation_id('a');
        let root = store.generation_dir(&generation);
        fs::create_dir_all(root.join("bin")).expect("create bin");
        fs::create_dir(root.join("python")).expect("create python");
        fs::create_dir(root.join("frontend")).expect("create frontend");
        let binary = root.join("bin/zerodeckd");
        fs::write(&binary, b"#!/bin/sh\n").expect("write binary");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))
                .expect("make binary executable");
        }
        fs::write(
            root.join("release.json"),
            br#"{"schema":1,"build_id":"build-a","version":"1.2.3"}"#,
        )
        .expect("write manifest");

        let layout = store
            .verify_generation(&generation)
            .expect("valid layout resolves");
        assert_eq!(layout.release().build_id(), "build-a");
        assert_eq!(layout.release().version().to_string(), "1.2.3");
        assert_eq!(layout.identity().build_id, "build-a");

        fs::remove_dir(root.join("python")).expect("remove python");
        let error = store
            .verify_generation(&generation)
            .expect_err("an incomplete layout must fail");
        assert!(matches!(error, UpdateError::CorruptGeneration(id) if id == generation));
    }

    #[test]
    fn gc_keeps_confirmed_rollback_and_pending_generations() {
        let dir = TestDir::new("gc");
        let store = GenerationStore::new(&dir.0).expect("open generation store");
        let confirmed = generation_id('a');
        let rollback = generation_id('b');
        let pending_previous = generation_id('c');
        let pending_candidate = generation_id('d');
        let orphan = generation_id('e');
        for generation in [
            &confirmed,
            &rollback,
            &pending_previous,
            &pending_candidate,
            &orphan,
        ] {
            fs::create_dir(store.generation_dir(generation)).expect("create generation");
        }

        let state = UpdateState {
            last_confirmed: Some(confirmed.clone()),
            rollback_target: Some(rollback.clone()),
            pending: Some(PendingTransaction {
                id: Uuid::new_v4(),
                previous: Some(pending_previous.clone()),
                candidate: pending_candidate.clone(),
                phase: TransactionPhase::Switched,
                was_running: true,
                start_attempted: true,
                rollback_target: None,
            }),
            ..UpdateState::new()
        };

        store.gc(&state, &[]).expect("collect stale generations");

        for generation in [&confirmed, &rollback, &pending_previous, &pending_candidate] {
            assert!(
                store.generation_dir(generation).is_dir(),
                "protected generation {generation} must survive"
            );
        }
        assert!(!store.generation_dir(&orphan).exists());
    }

    #[test]
    fn gc_keeps_the_active_reference_and_staged_candidates() {
        let dir = TestDir::new("gc-current");
        let store = GenerationStore::new(&dir.0).expect("open generation store");
        let confirmed = generation_id('a');
        let current = generation_id('b');
        let staged = generation_id('c');
        let orphan = generation_id('d');
        for generation in [&confirmed, &current, &staged, &orphan] {
            fs::create_dir(store.generation_dir(generation)).expect("create generation");
        }
        store.switch_to(&current).expect("activate current");

        let state = UpdateState {
            last_confirmed: Some(confirmed.clone()),
            ..UpdateState::new()
        };
        store
            .gc(&state, std::slice::from_ref(&staged))
            .expect("collect stale generations");

        assert!(store.generation_dir(&confirmed).is_dir());
        assert!(
            store.generation_dir(&current).is_dir(),
            "the on-disk active reference must survive a stale state document"
        );
        assert!(
            store.generation_dir(&staged).is_dir(),
            "an authorized staged candidate must survive"
        );
        assert!(!store.generation_dir(&orphan).exists());
    }
}
