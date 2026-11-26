//! Single root-owned apply request slot shared by `upgrade`, `rollback`, and apply.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zerodeck_platform::atomic_write;

use crate::{AuthorizedRelease, GenerationId, StateStore, UpdateError};

/// Current on-disk [`ApplyRequest`] schema version.
pub const APPLY_REQUEST_SCHEMA: u32 = 1;

const APPLY_REQUEST_FILE: &str = "apply.json";
const MAX_APPLY_REQUEST_BYTES: u64 = 512 * 1024;

/// Which operation the privileged apply unit must perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyKind {
    /// Activate a freshly staged release selected by the trusted catalog.
    Upgrade,
    /// Restore the retained, previously confirmed offline rollback target.
    Rollback,
}

/// One privileged activation request written by `zerodeckctl` and consumed by `apply`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyRequest {
    /// Schema version of this document.
    #[serde(default)]
    pub schema: u32,
    /// Unique operation identifier, reused as the pending transaction id.
    pub id: Uuid,
    /// Upgrade or offline rollback.
    pub kind: ApplyKind,
    /// Generation to activate.
    pub generation: GenerationId,
    /// The active reference the operator observed when enqueueing the request.
    #[serde(default)]
    pub expected_active: Option<GenerationId>,
    /// Signed release metadata for an upgrade; absent for an explicit rollback.
    #[serde(default)]
    pub release: Option<AuthorizedRelease>,
    /// Whether the operator explicitly authorized starting a stopped service.
    #[serde(default)]
    pub start_service: bool,
}

impl ApplyRequest {
    /// Creates an upgrade request for an already staged release.
    #[must_use]
    pub fn upgrade(
        release: AuthorizedRelease,
        expected_active: Option<GenerationId>,
        start_service: bool,
    ) -> Self {
        Self {
            schema: APPLY_REQUEST_SCHEMA,
            id: Uuid::new_v4(),
            kind: ApplyKind::Upgrade,
            generation: release.digest.clone(),
            expected_active,
            release: Some(release),
            start_service,
        }
    }

    /// Creates an explicit offline rollback request.
    #[must_use]
    pub fn rollback(target: GenerationId, expected_active: Option<GenerationId>) -> Self {
        Self {
            schema: APPLY_REQUEST_SCHEMA,
            id: Uuid::new_v4(),
            kind: ApplyKind::Rollback,
            generation: target,
            expected_active,
            release: None,
            start_service: false,
        }
    }

    /// Validates the document shape independently of current disk state.
    pub fn validate(&self) -> Result<(), UpdateError> {
        if self.schema != APPLY_REQUEST_SCHEMA {
            return Err(UpdateError::InvalidApplyRequest(format!(
                "unsupported schema {}",
                self.schema
            )));
        }
        match self.kind {
            ApplyKind::Upgrade => {
                let release = self.release.as_ref().ok_or_else(|| {
                    UpdateError::InvalidApplyRequest(
                        "an upgrade request requires signed release metadata".to_owned(),
                    )
                })?;
                if release.digest != self.generation {
                    return Err(UpdateError::InvalidApplyRequest(
                        "the request generation differs from the authorized release digest"
                            .to_owned(),
                    ));
                }
            }
            ApplyKind::Rollback => {
                if self.release.is_some() {
                    return Err(UpdateError::InvalidApplyRequest(
                        "a rollback request must not carry catalog metadata".to_owned(),
                    ));
                }
                if self.start_service {
                    return Err(UpdateError::InvalidApplyRequest(
                        "a rollback request must not request a service start".to_owned(),
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Atomic single-slot storage for [`ApplyRequest`] under the update state directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyRequestStore {
    path: PathBuf,
}

impl ApplyRequestStore {
    /// Creates a request store backed by `path`.
    #[must_use]
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }

    /// Creates a request store next to the persisted update state.
    #[must_use]
    pub fn for_state(state: &StateStore) -> Self {
        let directory = state
            .path()
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        Self::new(directory.join(APPLY_REQUEST_FILE))
    }

    /// Returns the backing request file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads the request, failing when the slot is empty.
    pub fn read(&self) -> Result<ApplyRequest, UpdateError> {
        self.read_optional()?
            .ok_or(UpdateError::MissingApplyRequest)
    }

    /// Reads the request when present.
    pub fn read_optional(&self) -> Result<Option<ApplyRequest>, UpdateError> {
        let metadata = match fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(UpdateError::Io {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(UpdateError::InvalidApplyRequest(
                "the apply request is not a regular file".to_owned(),
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let euid = rustix::process::geteuid().as_raw();
            if metadata.uid() != euid {
                return Err(UpdateError::InvalidApplyRequest(
                    "the apply request is not owned by the applying user".to_owned(),
                ));
            }
            if metadata.mode() & 0o022 != 0 {
                return Err(UpdateError::InvalidApplyRequest(
                    "the apply request is writable by another user".to_owned(),
                ));
            }
        }
        if metadata.len() > MAX_APPLY_REQUEST_BYTES {
            return Err(UpdateError::InvalidApplyRequest(format!(
                "the apply request exceeds {MAX_APPLY_REQUEST_BYTES} bytes"
            )));
        }
        let bytes = fs::read(&self.path).map_err(|source| UpdateError::Io {
            path: self.path.clone(),
            source,
        })?;
        let request: ApplyRequest = serde_json::from_slice(&bytes)
            .map_err(|error| UpdateError::InvalidApplyRequest(error.to_string()))?;
        request.validate()?;
        Ok(Some(request))
    }

    /// Atomically replaces the single request slot.
    pub fn write(&self, request: &ApplyRequest) -> Result<(), UpdateError> {
        request.validate()?;
        let mut bytes = serde_json::to_vec_pretty(request)?;
        bytes.push(b'\n');
        atomic_write(&self.path, &bytes)?;
        set_private_mode(&self.path)?;
        Ok(())
    }

    /// Atomically writes the request, refusing to overwrite an unconsumed one.
    pub fn enqueue(&self, request: &ApplyRequest) -> Result<(), UpdateError> {
        if self.read_optional()?.is_some() {
            return Err(UpdateError::RequestPending);
        }
        self.write(request)
    }

    /// Removes the request slot, treating an absent slot as success.
    pub fn remove(&self) -> Result<(), UpdateError> {
        match fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(source) => {
                return Err(UpdateError::Io {
                    path: self.path.clone(),
                    source,
                });
            }
        }
        sync_parent(&self.path)
    }
}

fn set_private_mode(path: &Path) -> Result<(), UpdateError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|source| {
            UpdateError::Io {
                path: path.to_path_buf(),
                source,
            }
        })?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn sync_parent(path: &Path) -> Result<(), UpdateError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| UpdateError::Io {
            path: parent.to_path_buf(),
            source,
        })
}
