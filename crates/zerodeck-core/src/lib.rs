//! Core domain services for `ZeroDeck`: atomic settings, plugin discovery, and archive validation.

#![forbid(unsafe_code)]

mod archive;
mod blocking;
mod discovery;
mod error;
pub mod filepicker;
pub mod http_policy;
pub mod installer;
mod manager;
mod settings;
pub mod watcher;

pub use archive::{
    DEFAULT_MAX_ARCHIVE_BYTES, DEFAULT_MAX_ARCHIVE_ENTRIES, ExtractLimits, InstallSource,
    extract_zip_safely, extract_zip_safely_with_limit, extract_zip_safely_with_limits,
    verify_sha256_policy,
};
pub use discovery::{DiscoveredPlugin, discover_plugins};
pub use error::CoreError;
pub use filepicker::{
    DEFAULT_FILEPICKER_MAX_ENTRIES, FilePicker, FilePickerEntry, FilePickerListing,
    FilePickerQuery, MAX_FILEPICKER_MAX_ENTRIES,
};
pub use http_policy::{
    CheckedHttpClient, DEFAULT_MAX_REDIRECTS, OutboundHttpPolicy, ValidatedResponse,
    send_checked_request,
};
pub use installer::{
    DEFAULT_MAX_REMOTE_BINARY_BYTES, DEFAULT_MAX_ZIP_DOWNLOAD_BYTES, InstallOutcome,
    InstallPluginRequest, InstallPluginSource, InstallerLimits, StagedPluginInstall,
    commit_staged_plugin, parse_install_source, prepare_staged_plugin, sanitize_directory_name,
};
pub use manager::{
    LoaderEvent, PluginFileScope, PluginManager, PluginManagerBuilder, PluginSummary,
    resolve_static_file,
};
pub use settings::SettingsManager;
pub use watcher::{DebugWatcherHandle, start_debug_watcher};

#[cfg(test)]
mod tests;
