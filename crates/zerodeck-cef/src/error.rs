use thiserror::Error;

/// Errors encountered while communicating with Steam's Chromium Embedded Framework debugger.
#[derive(Debug, Error)]
pub enum CefError {
    /// Failed to query CEF `/json` endpoint.
    #[error("CEF HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    /// Failed to connect or exchange CDP WebSocket messages.
    #[error("CEF WebSocket error: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),

    /// Expected `GamepadUI` tab is not available yet.
    #[error("GamepadUI CEF tab not found")]
    GamepadUiNotFound,

    /// Protocol failure while parsing CDP payload.
    #[error(transparent)]
    Protocol(#[from] zerodeck_protocol::ProtocolError),
}
