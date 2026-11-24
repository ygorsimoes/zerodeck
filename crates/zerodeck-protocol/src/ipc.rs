use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use uuid::Uuid;

use crate::ProtocolError;

/// Discriminator values for newline-delimited JSON plugin worker IPC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum PluginIpcType {
    Call = 0,
    Response = 1,
    Event = 2,
}

impl PluginIpcType {
    /// Parses an integer IPC discriminator.
    pub fn from_i64(code: i64) -> Result<Self, ProtocolError> {
        match code {
            0 => Ok(Self::Call),
            1 => Ok(Self::Response),
            2 => Ok(Self::Event),
            other => Err(ProtocolError::UnknownMessageType(other)),
        }
    }
}

impl Serialize for PluginIpcType {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u8(*self as u8)
    }
}

impl<'de> Deserialize<'de> for PluginIpcType {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let code = i64::deserialize(deserializer)?;
        Self::from_i64(code).map_err(serde::de::Error::custom)
    }
}

/// Messages sent from the Rust daemon to a plugin worker process.
#[derive(Debug, Clone, PartialEq)]
pub enum PluginIpcToWorker {
    Call {
        id: String,
        method: String,
        args: Value,
        legacy: bool,
    },
    Control {
        uninstall: bool,
    },
}

impl PluginIpcToWorker {
    /// Builds a new RPC call with a fresh UUID request identifier.
    #[must_use]
    pub fn new_call(method: impl Into<String>, args: Value, legacy: bool) -> Self {
        Self::Call {
            id: Uuid::new_v4().to_string(),
            method: method.into(),
            args,
            legacy,
        }
    }
}

impl Serialize for PluginIpcToWorker {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        #[serde(untagged)]
        enum Wire<'a> {
            Call {
                r#type: PluginIpcType,
                id: &'a str,
                method: &'a str,
                args: &'a Value,
                legacy: bool,
            },
            Control {
                uninstall: bool,
            },
        }

        let wire = match self {
            Self::Call {
                id,
                method,
                args,
                legacy,
            } => Wire::Call {
                r#type: PluginIpcType::Call,
                id,
                method,
                args,
                legacy: *legacy,
            },
            Self::Control { uninstall } => Wire::Control {
                uninstall: *uninstall,
            },
        };
        wire.serialize(serializer)
    }
}

/// Messages received from a plugin worker process over newline-delimited JSON.
#[derive(Debug, Clone, PartialEq)]
pub enum PluginIpcFromWorker {
    Response {
        id: String,
        success: bool,
        res: Value,
    },
    Event {
        event: String,
        args: Vec<Value>,
    },
}

impl PluginIpcFromWorker {
    /// Parses a single JSON line emitted by the plugin worker.
    pub fn from_json_str(raw: &str) -> Result<Self, ProtocolError> {
        let value: Value = serde_json::from_str(raw)?;
        let Some(code) = value.get("type").and_then(Value::as_i64) else {
            return Err(ProtocolError::MissingMessageType);
        };
        match PluginIpcType::from_i64(code)? {
            PluginIpcType::Response => {
                let id = value
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or(ProtocolError::MissingField("id"))?
                    .to_owned();
                let success = value
                    .get("success")
                    .and_then(Value::as_bool)
                    .ok_or(ProtocolError::MissingField("success"))?;
                let res = value.get("res").cloned().unwrap_or(Value::Null);
                Ok(Self::Response { id, success, res })
            }
            PluginIpcType::Event => {
                let event = value
                    .get("event")
                    .and_then(Value::as_str)
                    .ok_or(ProtocolError::MissingField("event"))?
                    .to_owned();
                let args = value
                    .get("args")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()?
                    .unwrap_or_default();
                Ok(Self::Event { event, args })
            }
            PluginIpcType::Call => Err(ProtocolError::UnexpectedMessageDirection(code)),
        }
    }
}
