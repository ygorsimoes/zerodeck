use thiserror::Error;

/// Errors returned when parsing or validating `ZeroDeck` wire, IPC, or manifest payloads.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProtocolError {
    /// JSON serialization or deserialization failed.
    #[error("invalid JSON payload: {0}")]
    InvalidJson(String),

    /// Message lacks an integer `"type"` discriminator field.
    #[error("missing or non-integer message type discriminator")]
    MissingMessageType,

    /// Integer `"type"` discriminator is outside the supported protocol range.
    #[error("unknown message type code: {0}")]
    UnknownMessageType(i64),

    /// Client sent a server-only message type or vice versa.
    #[error("unexpected message direction for type code: {0}")]
    UnexpectedMessageDirection(i64),

    /// Required field is missing from a typed frame.
    #[error("missing required field `{0}`")]
    MissingField(&'static str),
}

impl From<serde_json::Error> for ProtocolError {
    fn from(err: serde_json::Error) -> Self {
        Self::InvalidJson(err.to_string())
    }
}
