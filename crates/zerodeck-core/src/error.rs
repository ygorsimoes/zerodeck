use std::path::PathBuf;

use thiserror::Error;
use zerodeck_platform::PlatformError;
use zerodeck_plugin_host::HostError;

/// Errors produced by `ZeroDeck` core domain services.
#[derive(Debug, Error)]
pub enum CoreError {
    /// Underlying OS/platform operation failed.
    #[error(transparent)]
    Platform(#[from] PlatformError),

    /// Plugin worker host error.
    #[error(transparent)]
    Host(#[from] HostError),

    /// Target plugin is disabled in `loader.json`.
    #[error("plugin `{0}` is disabled")]
    PluginDisabled(String),

    /// A plugin's manifest `name` is already owned by another plugin directory.
    #[error("plugin name `{name}` is already used by directory `{directory}`")]
    DuplicatePluginName { name: String, directory: String },

    /// A spawn preflight check refused to start the plugin's worker.
    #[error("plugin `{name}` cannot start: {reason}")]
    PluginBlocked { name: String, reason: String },

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

    /// Extracted archive exceeded the configured maximum uncompressed byte budget.
    #[error("archive `{entry}` exceeded maximum uncompressed limit of {max_bytes} bytes")]
    ArchiveSizeExceeded { entry: String, max_bytes: u64 },

    /// Archive declares more entries than the configured maximum.
    #[error("archive has {count} entries, exceeding the maximum of {max_entries}")]
    ArchiveTooManyEntries { count: usize, max_entries: usize },

    /// Remote install requires a SHA-256 digest unless `allow_unverified_installs` is enabled.
    #[error("missing required SHA-256 digest for remote installation")]
    MissingSha256,

    /// Local unverified sideload requires explicit user confirmation.
    #[error("local archive without SHA-256 requires explicit user confirmation")]
    ConfirmationRequired,

    /// Computed SHA-256 digest does not match expected value.
    #[error("SHA-256 mismatch: expected {expected}, got {actual}")]
    Sha256Mismatch { expected: String, actual: String },

    /// Outbound HTTP request was blocked by the SSRF policy.
    #[error("outbound HTTP request blocked by policy: {0}")]
    HttpPolicy(String),

    /// Outbound HTTP request failed at transport or protocol level.
    #[error("outbound HTTP request failed: {0}")]
    HttpRequest(String),

    /// Outbound HTTP request hit the client's connect or response timeout.
    #[error("outbound HTTP request timed out: {0}")]
    HttpTimeout(String),

    /// Remote download exceeded the configured maximum byte budget.
    #[error("download exceeded maximum allowed size of {max_bytes} bytes")]
    DownloadSizeExceeded { max_bytes: usize },

    /// Plugin install source URI or path could not be parsed.
    #[error("invalid install source: {0}")]
    InvalidInstallSource(String),

    /// Installed plugin manifest metadata did not match caller expectations.
    #[error("plugin manifest {field} mismatch: expected `{expected}`, got `{actual}`")]
    ManifestMismatch {
        field: &'static str,
        expected: String,
        actual: String,
    },

    /// Extracted plugin archive does not contain a top-level `plugin.json`.
    #[error("archive does not contain a top-level plugin.json manifest")]
    MissingPluginManifest,

    /// Dotted setting path or protected lifecycle setting write is invalid.
    #[error("invalid setting: {0}")]
    InvalidSetting(String),

    /// Debug hot-reload watcher failed.
    #[error("debug watcher error: {0}")]
    Watcher(String),
}

impl CoreError {
    /// Returns `true` if this error represents a path-jail escape attempt.
    #[must_use]
    pub fn is_path_escape(&self) -> bool {
        matches!(self, Self::Platform(PlatformError::PathEscape { .. }))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use zerodeck_platform::PlatformError;

    use super::CoreError;

    #[test]
    fn is_path_escape_matches_only_platform_path_escape() {
        let escape = CoreError::Platform(PlatformError::PathEscape {
            candidate: PathBuf::from("/etc/passwd"),
            root: PathBuf::from("/srv/plugins"),
        });
        assert!(escape.is_path_escape());

        let empty_roots = CoreError::Platform(PlatformError::EmptyRootSet);
        assert!(!empty_roots.is_path_escape());

        let other = CoreError::InvalidSetting("bad key".to_owned());
        assert!(!other.is_path_escape());
    }
}
