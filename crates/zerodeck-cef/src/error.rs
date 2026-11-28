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

    /// Invalid supervisor or debugger configuration.
    #[error("invalid CEF configuration: {0}")]
    InvalidConfig(String),

    /// A confirmation does not belong to the current daemon instance, context and attempt.
    #[error("stale CEF confirmation")]
    StaleConfirmation,

    /// The CDP command did not answer before its deadline.
    #[error("CDP command timed out")]
    CdpTimeout,

    /// The supervisor only allows the specific CDP methods it issues itself.
    #[error("CDP method not allowed: {0}")]
    CdpMethodNotAllowed(String),

    /// The evaluated bootstrap raised an exception or returned a CDP error.
    #[error("CDP evaluation failed: {0}")]
    CdpEvaluationFailed(String),

    /// The CDP endpoint closed or returned malformed data.
    #[error("invalid CDP response: {0}")]
    CdpResponse(String),
}
