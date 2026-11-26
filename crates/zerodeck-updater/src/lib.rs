//! Durable update state and immutable loader generations for `zerodeckctl`.

#![forbid(unsafe_code)]

mod catalog;
mod generation;
mod stage;
mod state;

use std::io;
use std::path::PathBuf;

use semver::Version;
use thiserror::Error;
use zerodeck_platform::PlatformError;

pub use catalog::{AuthorizedRelease, Catalog, Channel, InstalledRelease, SelectionPolicy};
pub use generation::{GenerationId, GenerationStore};
pub use stage::{StageFault, StageLimits, stage_release, stage_release_with};
pub use state::{
    CATALOG_STATE_SCHEMA, CatalogState, CatalogStateStore, PendingTransaction, StateStore,
    TransactionPhase, UPDATE_STATE_SCHEMA, UpdateState,
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

    /// Trusted TUF metadata loading or verification failed.
    #[error("trusted catalog error: {0}")]
    Catalog(Box<tough::error::Error>),

    /// A signed loader target does not match the loader distribution contract.
    #[error("invalid loader target `{target}`: {reason}")]
    InvalidLoaderTarget { target: String, reason: String },

    /// A catalog base URL is not a valid directory URL.
    #[error("invalid catalog base URL `{url}`: {reason}")]
    InvalidBaseUrl { url: String, reason: String },

    /// A version label is reused with a different archive digest.
    #[error(
        "version `{version}` was already installed with digest `{installed}` but the catalog authorizes `{candidate}`"
    )]
    VersionConflict {
        version: Version,
        installed: GenerationId,
        candidate: GenerationId,
    },

    /// The in-memory catalog is older than the persisted trusted metadata.
    #[error("trusted catalog is stale; reload it before selecting")]
    StaleCatalog,

    /// The catalog was opened for offline authorization only and cannot select targets.
    #[error("trusted catalog metadata is not loaded")]
    CatalogNotLoaded,

    /// No successful trusted metadata verification exists.
    #[error("no trusted catalog verification is recorded")]
    CatalogNotVerified,

    /// A release is not the one authorized by the locally trusted catalog.
    #[error("release `{version}` ({digest}) is not authorized by the locally trusted catalog")]
    ReleaseNotAuthorized {
        version: Version,
        digest: GenerationId,
    },

    /// A newer catalog selection superseded the staged release.
    #[error("release `{version}` ({digest}) was superseded by newer trusted metadata")]
    ReleaseSuperseded {
        version: Version,
        digest: GenerationId,
    },

    /// The locally verified release authorization has expired.
    #[error("release authorization expired at {expires}")]
    AuthorizationExpired { expires: u64 },

    /// The system clock predates the persisted last successful verification time.
    #[error("system clock is {now} but the last successful verification was {floor}")]
    ClockRollback { now: u64, floor: u64 },

    /// The system clock predates the Unix epoch.
    #[error("system clock predates the Unix epoch")]
    ClockBeforeEpoch,

    /// A release has no download URL.
    #[error("release has no download URL")]
    MissingDownloadUrl,

    /// Streaming the archive failed at the HTTP transport.
    #[error("HTTP request to `{url}` failed: {source}")]
    Download {
        url: String,
        #[source]
        source: Box<reqwest::Error>,
    },

    /// The archive server returned a non-success status.
    #[error("HTTP request to `{url}` returned status {status}")]
    DownloadStatus { url: String, status: u16 },

    /// The downloaded archive length differs from the signed target length.
    #[error("archive size mismatch for `{url}`: expected {expected}, got {actual}")]
    SizeMismatch {
        url: String,
        expected: u64,
        actual: u64,
    },

    /// The downloaded archive digest differs from the signed target digest.
    #[error("archive digest mismatch: expected `{expected}`, got `{actual}`")]
    DigestMismatch {
        expected: GenerationId,
        actual: String,
    },

    /// The archive exceeds the configured download limit.
    #[error("archive exceeds the {limit}-byte download limit")]
    DownloadTooLarge { limit: u64 },

    /// An archive entry is a link, device, escape, or otherwise unsupported path.
    #[error("archive entry `{path}` is unsafe: {reason}")]
    UnsafeArchiveEntry { path: String, reason: String },

    /// The archive repeats a path.
    #[error("archive contains duplicate entry `{path}`")]
    DuplicateArchiveEntry { path: String },

    /// The archive contains more entries than allowed.
    #[error("archive exceeds the {limit}-entry limit")]
    TooManyArchiveEntries { limit: usize },

    /// The archive expands beyond the allowed byte budget.
    #[error("archive expands beyond the {limit}-byte limit")]
    ArchiveExpandedTooLarge { limit: u64 },

    /// The archive stream could not be parsed as a tar.gz archive.
    #[error("archive `{path}` is invalid: {reason}")]
    InvalidArchive { path: PathBuf, reason: String },

    /// The embedded release manifest is missing or malformed.
    #[error("release manifest is invalid: {0}")]
    InvalidReleaseManifest(String),

    /// The embedded release manifest disagrees with the signed target.
    #[error("release manifest `{field}` is `{found}` but the signed target says `{expected}`")]
    ReleaseIdentityMismatch {
        field: &'static str,
        expected: String,
        found: String,
    },

    /// A generation directory does not contain the expected release identity.
    #[error("cached generation `{0}` is incomplete or corrupt")]
    CorruptGeneration(GenerationId),

    /// A `zerodeck-core` operation failed.
    #[error(transparent)]
    Core(Box<zerodeck_core::CoreError>),
}

impl From<tough::error::Error> for UpdateError {
    fn from(error: tough::error::Error) -> Self {
        Self::Catalog(Box::new(error))
    }
}

impl From<zerodeck_core::CoreError> for UpdateError {
    fn from(error: zerodeck_core::CoreError) -> Self {
        Self::Core(Box::new(error))
    }
}
