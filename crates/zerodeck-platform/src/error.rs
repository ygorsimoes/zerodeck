use std::path::PathBuf;

use thiserror::Error;

/// Errors returned by OS path, filesystem, and privilege operations.
#[derive(Debug, Error)]
pub enum PlatformError {
    /// Target path attempts to escape its allowed jail root.
    #[error("path `{candidate}` escapes allowed root `{root}`")]
    PathEscape { candidate: PathBuf, root: PathBuf },

    /// No canonical root could be resolved for a path-jail set.
    #[error("no valid root paths configured")]
    EmptyRootSet,

    /// Underlying filesystem I/O failure.
    #[error("filesystem error at `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}
