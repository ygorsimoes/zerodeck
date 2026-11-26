//! Durable update state and immutable loader generations for `zerodeckctl`.

#![forbid(unsafe_code)]

mod generation;
mod state;

use std::io;
use std::path::PathBuf;

use thiserror::Error;
use zerodeck_platform::PlatformError;

pub use generation::{GenerationId, GenerationStore};
pub use state::{
    PendingTransaction, StateStore, TransactionPhase, UPDATE_STATE_SCHEMA, UpdateState,
};

/// Errors returned by durable update state and generation operations.
#[derive(Debug, Error)]
pub enum UpdateError {
    /// A generation identifier is not a lowercase SHA-256 hex string.
    #[error("invalid generation id `{0}`")]
    InvalidGenerationId(String),

    /// The active reference points at a generation that is not installed.
    #[error("generation `{0}` is not installed")]
    MissingGeneration(GenerationId),

    /// The persisted update state uses an unsupported schema.
    #[error("unsupported update state schema {0}")]
    UnsupportedSchema(u32),

    /// A path escapes the owned update root.
    #[error("path `{path}` escapes the owned update root `{root}`")]
    PathEscape { path: PathBuf, root: PathBuf },

    /// An atomic filesystem operation failed.
    #[error(transparent)]
    Platform(#[from] PlatformError),

    /// Persisted update state is not valid JSON.
    #[error(transparent)]
    Json(#[from] serde_json::Error),

    /// A filesystem operation inside the owned update root failed.
    #[error("filesystem error at `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}
