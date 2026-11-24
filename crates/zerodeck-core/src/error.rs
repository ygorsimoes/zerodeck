use std::path::PathBuf;

use thiserror::Error;
use zerodeck_platform::PlatformError;

/// Errors produced by `ZeroDeck` core domain services.
#[derive(Debug, Error)]
pub enum CoreError {
    /// Underlying OS/platform operation failed.
    #[error(transparent)]
    Platform(#[from] PlatformError),

    /// Filesystem I/O error.
    #[error("I/O error at `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Failed to parse settings JSON without modifying on-disk state.
    #[error("failed to parse settings file `{path}`: {source}")]
    SettingsParse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    /// Settings root JSON value is not a JSON object.
    #[error("settings file `{path}` must contain a JSON object")]
    SettingsNotObject { path: PathBuf },

    /// Failed to serialize settings JSON.
    #[error("failed to serialize settings: {0}")]
    SettingsSerialize(#[from] serde_json::Error),

    /// Plugin manifest (`plugin.json` or `package.json`) could not be parsed.
    #[error("invalid manifest at `{path}`: {source}")]
    ManifestParse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    /// ZIP archive is malformed or unreadable.
    #[error("invalid ZIP archive: {0}")]
    Zip(#[from] zip::result::ZipError),

    /// ZIP archive contains a Zip-Slip traversal or disallowed symlink entry.
    #[error("unsafe archive entry rejected: `{entry}`")]
    UnsafeArchiveEntry { entry: String },

    /// Remote install requires a SHA-256 digest unless `allow_unverified_installs` is enabled.
    #[error("missing required SHA-256 digest for remote installation")]
    MissingSha256,

    /// Local unverified sideload requires explicit user confirmation.
    #[error("local archive without SHA-256 requires explicit user confirmation")]
    ConfirmationRequired,

    /// Computed SHA-256 digest does not match expected value.
    #[error("SHA-256 mismatch: expected {expected}, got {actual}")]
    Sha256Mismatch { expected: String, actual: String },
}
