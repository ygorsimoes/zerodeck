use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::ProtocolError;

/// Discriminator values for `/ws` messages matching Decky's `WSRouter` protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i16)]
pub enum WsMessageType {
    Error = -1,
    Call = 0,
    Reply = 1,
    Discard = 2,
    ReceivedResponse = 3,
    FullSync = 4,
    Event = 5,
}

impl WsMessageType {
    /// Returns the integer wire discriminator for this message type.
    #[must_use]
    pub const fn as_i16(self) -> i16 {
        self as i16
    }

    /// Parses an integer wire discriminator into a [`WsMessageType`].
    pub fn from_i64(code: i64) -> Result<Self, ProtocolError> {
        match code {
            -1 => Ok(Self::Error),
            0 => Ok(Self::Call),
            1 => Ok(Self::Reply),
            2 => Ok(Self::Discard),
            3 => Ok(Self::ReceivedResponse),
            4 => Ok(Self::FullSync),
            5 => Ok(Self::Event),
            other => Err(ProtocolError::UnknownMessageType(other)),
        }
    }
}

impl Serialize for WsMessageType {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_i16(self.as_i16())
    }
}

impl<'de> Deserialize<'de> for WsMessageType {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let code = i64::deserialize(deserializer)?;
        Self::from_i64(code).map_err(serde::de::Error::custom)
    }
}

/// Payload for an RPC `CALL` issued by the frontend client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WsCallPayload {
    pub id: u64,
    pub route: String,
    #[serde(default)]
    pub args: Vec<Value>,
}

/// Error payload returned in a `MessageType::Error` (`-1`) frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WsErrorDetail {
    pub name: String,
    pub error: String,
    #[serde(default)]
    pub traceback: Option<String>,
}

/// Messages sent from the frontend WebSocket client to `zerodeck-server`.
#[derive(Debug, Clone, PartialEq)]
pub enum WsClientMessage {
    Call(WsCallPayload),
    ReceivedResponse { id: u64 },
    FullSync { messages: Vec<WsCallPayload> },
}

impl WsClientMessage {
    /// Parses a raw JSON text frame sent by the client.
    pub fn from_json_str(raw: &str) -> Result<Self, ProtocolError> {
        let value: Value = serde_json::from_str(raw)?;
        let Some(code) = value.get("type").and_then(Value::as_i64) else {
            return Err(ProtocolError::MissingMessageType);
        };
        let msg_type = WsMessageType::from_i64(code)?;
        match msg_type {
            WsMessageType::Call => Ok(Self::Call(serde_json::from_value(value)?)),
            WsMessageType::ReceivedResponse => {
                let id = value
                    .get("id")
                    .and_then(Value::as_u64)
                    .ok_or(ProtocolError::MissingField("id"))?;
                Ok(Self::ReceivedResponse { id })
            }
            WsMessageType::FullSync => {
                let messages = value
                    .get("messages")
                    .cloned()
                    .ok_or(ProtocolError::MissingField("messages"))?;
                Ok(Self::FullSync {
                    messages: serde_json::from_value(messages)?,
                })
            }
            WsMessageType::Error
            | WsMessageType::Reply
            | WsMessageType::Discard
            | WsMessageType::Event => Err(ProtocolError::UnexpectedMessageDirection(code)),
        }
    }
}

/// Messages sent from `zerodeck-server` to the frontend WebSocket client.
#[derive(Debug, Clone, PartialEq)]
pub enum WsServerMessage {
    Error { id: u64, error: WsErrorDetail },
    Reply { id: u64, result: Value },
    Discard { id: u64 },
    Event { event: String, args: Vec<Value> },
}

impl Serialize for WsServerMessage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        #[serde(untagged)]
        enum Wire<'a> {
            Error {
                r#type: WsMessageType,
                id: u64,
                error: &'a WsErrorDetail,
            },
            Reply {
                r#type: WsMessageType,
                id: u64,
                result: &'a Value,
            },
            Discard {
                r#type: WsMessageType,
                id: u64,
            },
            Event {
                r#type: WsMessageType,
                event: &'a str,
                args: &'a [Value],
            },
        }

        let wire = match self {
            Self::Error { id, error } => Wire::Error {
                r#type: WsMessageType::Error,
                id: *id,
                error,
            },
            Self::Reply { id, result } => Wire::Reply {
                r#type: WsMessageType::Reply,
                id: *id,
                result,
            },
            Self::Discard { id } => Wire::Discard {
                r#type: WsMessageType::Discard,
                id: *id,
            },
            Self::Event { event, args } => Wire::Event {
                r#type: WsMessageType::Event,
                event,
                args,
            },
        };
        wire.serialize(serializer)
    }
}
