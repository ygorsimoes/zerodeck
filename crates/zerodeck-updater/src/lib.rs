//! Durable update state and immutable loader generations for `zerodeckctl`.

#![forbid(unsafe_code)]
#![cfg(unix)]

mod activation;
mod boot;
mod bootstrap;
mod catalog;
mod generation;
mod request;
mod schema;
pub mod service;
mod stage;
mod state;

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use semver::Version;
use thiserror::Error;
use zerodeck_platform::PlatformError;

pub use activation::{
    APPLY_UNIT, ActivationCheckpoint, ActivationFault, DAEMON_UNIT, RecoveryMode, UpgradeOutcome,
    acquire_maintenance_exclusive, acquire_maintenance_shared, apply_pending, apply_pending_with,
    enqueue_rollback, enqueue_upgrade, enqueue_upgrade_with_channel, recover, recover_before_boot,
    recover_with,
};
pub use boot::BootRecoveryMarker;
pub use bootstrap::{BootstrapOutcome, bootstrap_generation, confirm_bootstrap, prepare_bootstrap};
pub use catalog::{AuthorizedRelease, Catalog, Channel, InstalledRelease, SelectionPolicy};
pub use generation::{GenerationId, GenerationLayout, GenerationRelease, GenerationStore};
pub use request::{APPLY_REQUEST_SCHEMA, ApplyKind, ApplyRequest, ApplyRequestStore};
pub use schema::{Schema, SchemaFile};
pub use service::{
    ACTIVATION_DEADLINE, ReleaseIdentity, Systemctl, SystemctlFuture, SystemdService,
    TokioSystemctl, verify_ready,
};
pub use stage::{StageFault, StageLimits, stage_release, stage_release_with};
pub use state::{
    CATALOG_STATE_SCHEMA, CatalogState, CatalogStateStore, PendingTransaction, SelectionRecord,
    StateStore, TransactionPhase, UPDATE_STATE_SCHEMA, UpdateState,
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

    /// The update root is reachable through a user-writable or symlinked ancestor.
    #[error("unsafe update root: {0}")]
    UnsafeRoot(String),

    /// The selected release does not belong to the channel that was persisted for it.
    #[error(
        "release channel `{release}` does not match the persisted selection channel `{selected}`"
    )]
    ChannelMismatch {
        selected: &'static str,
        release: &'static str,
    },

    /// The requested channel is not the channel under which the candidate was selected.
    #[error(
        "the staged release was selected for `{selected}` but this request is for `{requested}`; \
         rerun `update --channel {requested}` to select it explicitly"
    )]
    SelectionChannelMismatch {
        selected: &'static str,
        requested: &'static str,
    },

    /// The candidate would downgrade the active generation.
    #[error("candidate {candidate} is older than the installed generation {installed}")]
    DowngradeRejected {
        installed: Version,
        candidate: Version,
    },

    /// The running updater is older than the signed or selected minimum.
    #[error("updater {current} is older than the required {required}")]
    UpdaterTooOld { required: Version, current: Version },

    /// Boot recovery has not completed for the current boot.
    #[error(
        "boot recovery has not completed for this boot; run `zerodeckctl recover-boot` before starting the daemon"
    )]
    BootRecoveryRequired,
    /// The kernel boot ID could not be read, so boot recovery cannot be attested.
    #[error("cannot read the kernel boot id at `{0}`")]
    BootIdUnavailable(std::path::PathBuf),

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

    /// Another maintenance operation holds the shared/exclusive guard.
    #[error("another ZeroDeck maintenance operation is in progress")]
    Busy,

    /// The privileged apply unit has no request to consume.
    #[error("no apply request is pending")]
    MissingApplyRequest,

    /// The single apply request slot already holds an unconsumed request.
    #[error("an apply request is already pending")]
    RequestPending,

    /// The apply request document is malformed or unsafe.
    #[error("invalid apply request: {0}")]
    InvalidApplyRequest(String),

    /// The request was prepared for a different active generation.
    #[error("stale apply request: prepared for active `{expected:?}` but `{found:?}` is active")]
    StaleRequest {
        expected: Option<GenerationId>,
        found: Option<GenerationId>,
    },

    /// A transaction is already in flight for this state.
    #[error("another activation transaction is already in flight")]
    DuplicateTransaction,

    /// The request kind does not match the persisted target it names.
    #[error("the apply request kind does not match the persisted generation role")]
    RequestKindMismatch,

    /// The requested generation was rejected by a failed trial.
    #[error("generation `{0}` is quarantined and requires explicit review")]
    Quarantined(GenerationId),

    /// The managed service is stopped and activation requires an explicit start.
    #[error("the managed service is stopped; rerun the command with `--start` to activate")]
    StartRequired,

    /// An administrative bootstrap cannot safely replace the current controller or generation.
    #[error("bootstrap refused: {0}")]
    BootstrapRefused(String),

    /// No release is authorized by the persisted trusted metadata.
    #[error("no staged release is authorized; run `zerodeckctl update` first")]
    NoStagedRelease,

    /// No previously confirmed generation is retained for offline rollback.
    #[error("no rollback target is retained")]
    NoRollbackTarget,

    /// No active generation reference exists.
    #[error("no managed generation is active")]
    NoActiveGeneration,

    /// The requested generation is already active.
    #[error("generation `{0}` is already active")]
    AlreadyActive(GenerationId),

    /// The CLI channel is not one of the signed loader channels.
    #[error("unsupported update channel `{0}`")]
    InvalidChannel(String),

    /// A `systemctl` query or transition for the activation service failed.
    #[error("systemctl operation `{operation}` for `{unit}` failed: {reason}")]
    Systemctl {
        unit: String,
        operation: &'static str,
        reason: String,
    },

    /// A `Systemctl` test seam did not implement a required transition.
    #[error("systemctl operation `{operation}` is not supported for `{unit}`")]
    SystemctlUnsupported {
        unit: String,
        operation: &'static str,
    },

    /// A test fault stopped the process at a durable activation boundary.
    #[error("simulated process loss at activation checkpoint {0:?}")]
    SimulatedCrash(crate::activation::ActivationCheckpoint),

    /// Activation failed and no healthy previous generation could be restored.
    #[error(
        "activation failed and no healthy previous generation is available; service is stopped"
    )]
    NoGoodGeneration,

    /// The apply unit did not confirm the requested generation.
    #[error("managed activation did not confirm: {0}")]
    ActivationFailed(String),

    /// A required update source environment variable is missing.
    #[error("required environment variable `{0}` is not set")]
    MissingEnvironment(&'static str),

    /// A configured update source URL is invalid.
    #[error("environment variable `{name}` is not a valid URL: {reason}")]
    InvalidEnvironmentUrl { name: &'static str, reason: String },

    /// No pre-activation invocation was recorded for the service.
    #[error("no pre-activation invocation was recorded for service `{0}`")]
    InvocationBaselineMissing(String),

    /// The systemd unit is not running.
    #[error("systemd service `{0}` is not running")]
    ServiceNotRunning(String),

    /// The systemd unit is still on its pre-activation invocation.
    #[error("systemd service `{0}` is still on its previous invocation")]
    ServiceNotRestarted(String),

    /// The readiness state lock was poisoned.
    #[error("readiness state is poisoned")]
    ReadinessStatePoisoned,

    /// A local readiness endpoint could not be built.
    #[error("invalid readiness endpoint `{endpoint}`: {reason}")]
    InvalidReadyEndpoint { endpoint: String, reason: String },

    /// A local readiness request failed at the HTTP transport.
    #[error("readiness request to `{url}` failed: {source}")]
    ReadyHttp {
        url: String,
        #[source]
        source: Box<reqwest::Error>,
    },

    /// A readiness endpoint returned a non-success status.
    #[error("readiness endpoint `{path}` returned status {status}")]
    ReadyStatus { path: &'static str, status: u16 },

    /// The readiness response was malformed or incomplete.
    #[error("invalid readiness response: {0}")]
    InvalidReadyResponse(String),

    /// `/readyz` reported a different release identity than the activated generation.
    #[error("readiness {field} mismatch: expected `{expected}`, got `{found}`")]
    ReadinessIdentityMismatch {
        field: &'static str,
        expected: String,
        found: String,
    },

    /// The Python worker host is not importable.
    #[error("the Python worker host is not importable")]
    PythonHostNotReady,

    /// The packaged frontend asset is missing or unusable.
    #[error("expected frontend asset `{path}` is unavailable: {reason}")]
    FrontendAsset { path: &'static str, reason: String },

    /// The authenticated readiness RPC failed permanently (auth, route or protocol reply).
    #[error("authenticated readiness RPC failed: {0}")]
    ReadinessRpc(String),

    /// The authenticated readiness RPC failed at the transport layer and may be retried.
    #[error("authenticated readiness RPC transport failed: {0}")]
    ReadinessRpcTransport(String),

    /// Readiness did not pass within the activation deadline.
    #[error("readiness did not pass within the {0:?} activation deadline")]
    ReadinessDeadline(Duration),

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
