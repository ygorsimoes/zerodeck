use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

use rustix::fs::{FlockOperation, flock};

/// RAII advisory lock that serializes `ZeroDeck` maintenance operations.
///
/// The guard owns the open file description, so the lock is released when the guard is dropped.
/// The descriptor is close-on-exec, so a spawned worker never inherits the lock.
#[derive(Debug)]
#[must_use = "the lock is released as soon as the guard is dropped"]
pub struct MaintenanceLock {
    _file: File,
}

impl MaintenanceLock {
    /// Acquires a non-blocking shared maintenance lock at `path`.
    ///
    /// Multiple shared guards may coexist. An exclusive guard conflicts until every shared guard
    /// has been dropped. The parent directory must already exist and be owned by the caller.
    pub fn try_shared(path: &Path) -> io::Result<Self> {
        Self::try_lock(path, FlockOperation::NonBlockingLockShared)
    }

    /// Acquires a non-blocking exclusive maintenance lock at `path`.
    ///
    /// Conflicts with any other shared or exclusive guard, returning [`io::ErrorKind::WouldBlock`].
    /// The parent directory must already exist and be owned by the caller.
    pub fn try_exclusive(path: &Path) -> io::Result<Self> {
        Self::try_lock(path, FlockOperation::NonBlockingLockExclusive)
    }

    fn try_lock(path: &Path, operation: FlockOperation) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        flock(&file, operation).map_err(io::Error::from)?;
        Ok(Self { _file: file })
    }
}
