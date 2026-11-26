//! Mapping of core errors to WebSocket `ERROR` frames.

use zerodeck_core::CoreError;
use zerodeck_plugin_host::HostError;
use zerodeck_protocol::{WsErrorDetail, WsServerMessage};

pub(crate) fn core_error_to_ws(id: u64, err: CoreError) -> WsServerMessage {
    match err {
        CoreError::Host(HostError::WorkerStopped | HostError::StartupTimeout) => {
            WsServerMessage::Discard { id }
        }
        CoreError::Host(HostError::PluginCallFailed {
            name,
            message,
            traceback,
        }) => ws_error(id, name, message, traceback),
        CoreError::Host(HostError::PluginNotFound(name)) => ws_error(
            id,
            "PluginNotFoundError",
            format!("plugin `{name}` is not loaded"),
            None,
        ),
        CoreError::Host(HostError::PassivePlugin(name)) => ws_error(
            id,
            "PassivePluginError",
            format!("plugin `{name}` has no backend worker"),
            None,
        ),
        CoreError::Host(HostError::PluginFailed(name)) => ws_error(
            id,
            "PluginFailedError",
            format!("plugin `{name}` failed repeatedly"),
            None,
        ),
        CoreError::PluginDisabled(name) => ws_error(
            id,
            "PluginDisabledError",
            format!("plugin `{name}` is disabled"),
            None,
        ),
        CoreError::PluginBlocked { .. } => {
            ws_error(id, "PluginBlockedError", err.to_string(), None)
        }
        CoreError::DuplicatePluginName { .. } => {
            ws_error(id, "DuplicatePluginNameError", err.to_string(), None)
        }
        CoreError::Host(HostError::InvalidMethodName(method)) => ws_error(
            id,
            "PermissionError",
            format!("invalid method `{method}`"),
            None,
        ),
        e if e.is_path_escape() => ws_error(id, "PathEscapeError", e.to_string(), None),
        CoreError::MissingSha256 => ws_error(id, "MissingSha256Error", err.to_string(), None),
        CoreError::Sha256Mismatch { .. } => {
            ws_error(id, "Sha256MismatchError", err.to_string(), None)
        }
        CoreError::ConfirmationRequired => {
            ws_error(id, "ConfirmationRequiredError", err.to_string(), None)
        }
        CoreError::Zip(_)
        | CoreError::UnsafeArchiveEntry { .. }
        | CoreError::ArchiveSizeExceeded { .. }
        | CoreError::ArchiveTooManyEntries { .. }
        | CoreError::MissingPluginManifest
        | CoreError::ManifestMismatch { .. }
        | CoreError::ManifestParse { .. } => ws_error(id, "ArchiveError", err.to_string(), None),
        CoreError::HttpPolicy(_) => ws_error(id, "HttpPolicyError", err.to_string(), None),
        CoreError::InvalidSetting(_) | CoreError::InvalidInstallSource(_) => {
            ws_error(id, "ValueError", err.to_string(), None)
        }
        other => ws_error(id, "BackendError", other.to_string(), None),
    }
}

pub(crate) fn ws_error(
    id: u64,
    name: impl Into<String>,
    error: impl Into<String>,
    traceback: Option<String>,
) -> WsServerMessage {
    WsServerMessage::Error {
        id,
        error: WsErrorDetail {
            name: name.into(),
            error: error.into(),
            traceback,
        },
    }
}
