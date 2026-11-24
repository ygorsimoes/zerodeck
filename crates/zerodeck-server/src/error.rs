use thiserror::Error;

/// Errors returned by the `ZeroDeck` HTTP/WebSocket server.
#[derive(Debug, Error)]
pub enum ServerError {
    /// Failed to bind TCP listener.
    #[error("failed to bind server socket: {0}")]
    Bind(#[from] std::io::Error),

    /// Core subsystem error.
    #[error(transparent)]
    Core(#[from] zerodeck_core::CoreError),

    /// CEF subsystem error.
    #[error(transparent)]
    Cef(#[from] zerodeck_cef::CefError),

    /// Plugin host error.
    #[error(transparent)]
    Host(#[from] zerodeck_plugin_host::HostError),
}
