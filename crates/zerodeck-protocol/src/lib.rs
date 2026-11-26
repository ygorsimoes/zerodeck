//! Clean-room wire, IPC, and manifest contracts for `ZeroDeck`.

#![forbid(unsafe_code)]

mod error;
mod ipc;
mod manifest;
mod ws;

pub use error::ProtocolError;
pub use ipc::{PluginIpcError, PluginIpcFromWorker, PluginIpcToWorker, PluginIpcType};
pub use manifest::{PackageManifest, PluginLoadType, PluginManifest, RemoteBinarySpec};
pub use ws::{WsCallPayload, WsClientMessage, WsErrorDetail, WsMessageType, WsServerMessage};

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::{
        PackageManifest, PluginIpcError, PluginIpcFromWorker, PluginIpcToWorker, PluginLoadType,
        PluginManifest, ProtocolError, WsCallPayload, WsClientMessage, WsErrorDetail,
        WsMessageType, WsServerMessage,
    };

    #[test]
    fn ws_error_code_is_negative_one_and_roundtrips() {
        let raw = r#"{"type": 0, "id": 1, "route": "loader/get_plugins", "args": []}"#;
        let parsed = WsClientMessage::from_json_str(raw).expect("valid CALL");
        assert_eq!(
            parsed,
            WsClientMessage::Call(WsCallPayload {
                id: 1,
                route: "loader/get_plugins".to_owned(),
                args: vec![],
            })
        );

        let err_msg = WsServerMessage::Error {
            id: 42,
            error: WsErrorDetail {
                name: "RuntimeError".to_owned(),
                error: "boom".to_owned(),
                traceback: Some("Traceback...".to_owned()),
            },
        };
        let v: Value = serde_json::to_value(&err_msg).expect("serialize Error");
        assert_eq!(v["type"], json!(WsMessageType::Error.as_i16()));
        assert_eq!(v["type"], json!(-1));
        assert_eq!(v["id"], json!(42));
        assert_eq!(v["error"]["name"], json!("RuntimeError"));
    }

    #[test]
    fn ws_unknown_message_type_returns_protocol_error() {
        let raw = r#"{"type": 99, "id": 1}"#;
        let err = WsClientMessage::from_json_str(raw).expect_err("must reject type 99");
        assert_eq!(err, ProtocolError::UnknownMessageType(99));
    }

    #[test]
    fn ipc_call_and_response_roundtrip() {
        let call = PluginIpcToWorker::Call {
            id: "req-1".to_owned(),
            method: "get_status".to_owned(),
            args: json!(["arg1"]),
            legacy: false,
        };
        let encoded = serde_json::to_value(&call).expect("serialize IPC call");
        assert_eq!(encoded["type"], json!(0));
        assert_eq!(encoded["method"], json!("get_status"));

        let resp_raw = r#"{"type": 1, "id": "req-1", "success": true, "res": {"ok": true}}"#;
        let resp = PluginIpcFromWorker::from_json_str(resp_raw).expect("parse IPC response");
        assert_eq!(
            resp,
            PluginIpcFromWorker::Response {
                id: "req-1".to_owned(),
                success: true,
                res: json!({"ok": true}),
                error: None,
            }
        );
    }

    #[test]
    fn ipc_failed_response_carries_error_name_and_traceback() {
        let raw = r#"{"type": 1, "id": "req-2", "success": false, "res": "bad input",
            "error": {"name": "ValueError", "traceback": "Traceback (most recent call last):\n"}}"#;
        let resp = PluginIpcFromWorker::from_json_str(raw).expect("parse failed response");
        assert_eq!(
            resp,
            PluginIpcFromWorker::Response {
                id: "req-2".to_owned(),
                success: false,
                res: json!("bad input"),
                error: Some(PluginIpcError {
                    name: "ValueError".to_owned(),
                    traceback: Some("Traceback (most recent call last):\n".to_owned()),
                }),
            }
        );

        // A malformed `error` from an older or buggy worker must not reject the whole response.
        let malformed = r#"{"type": 1, "id": "req-3", "success": false, "res": "x", "error": 5}"#;
        let resp = PluginIpcFromWorker::from_json_str(malformed).expect("parse");
        assert!(matches!(
            resp,
            PluginIpcFromWorker::Response { error: None, .. }
        ));
    }

    #[test]
    fn manifest_defaults_api_version_zero_and_detects_load_type() {
        let plugin: PluginManifest =
            serde_json::from_str(r#"{"name": "Demo", "author": "Alice"}"#).expect("plugin.json");
        assert_eq!(plugin.api_version, 0);
        assert_eq!(plugin.flags, Vec::<String>::new());

        let pkg: PackageManifest =
            serde_json::from_str(r#"{"version": "1.2.3", "type": "module"}"#)
                .expect("package.json");
        assert_eq!(pkg.load_type(), PluginLoadType::EsModuleV1);

        let legacy_pkg: PackageManifest = serde_json::from_str("{}").expect("empty package.json");
        assert_eq!(legacy_pkg.load_type(), PluginLoadType::LegacyEvalIife);
    }

    #[test]
    fn ws_full_sync_and_missing_fields_are_validated() {
        let sync_raw = r#"{"type": 4, "messages": [{"type": 0, "id": 7, "route": "utilities/ping", "args": []}]}"#;
        let parsed = WsClientMessage::from_json_str(sync_raw).expect("valid FULL_SYNC");
        assert_eq!(
            parsed,
            WsClientMessage::FullSync {
                messages: vec![WsCallPayload {
                    id: 7,
                    route: "utilities/ping".to_owned(),
                    args: vec![],
                }]
            }
        );

        let missing_id = r#"{"type": 3}"#;
        let err = WsClientMessage::from_json_str(missing_id).expect_err("missing id");
        assert_eq!(err, ProtocolError::MissingField("id"));
    }

    #[test]
    fn plugin_load_type_serializes_as_integer_zero_and_one() {
        assert_eq!(
            serde_json::to_value(PluginLoadType::LegacyEvalIife).expect("serialize legacy"),
            json!(0)
        );
        assert_eq!(
            serde_json::to_value(PluginLoadType::EsModuleV1).expect("serialize esmodule"),
            json!(1)
        );
        assert_eq!(
            serde_json::from_value::<PluginLoadType>(json!(0)).expect("deserialize 0"),
            PluginLoadType::LegacyEvalIife
        );
        assert_eq!(
            serde_json::from_value::<PluginLoadType>(json!(1)).expect("deserialize 1"),
            PluginLoadType::EsModuleV1
        );
    }
}
