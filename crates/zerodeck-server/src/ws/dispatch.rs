//! Route table for WebSocket `CALL` messages.

use std::sync::Arc;

use serde::Serialize;
use serde_json::{Map, Value, json};
use zerodeck_core::{CoreError, PluginManager, PluginSummary};
use zerodeck_protocol::{WsCallPayload, WsServerMessage};

use crate::state::AppState;
use crate::state::PluginBackend;
use crate::ws::args::parse_filepicker_query;
use crate::ws::args::parse_get_settings_key;
use crate::ws::args::parse_install_request;
use crate::ws::args::parse_plugin_name_arg;
use crate::ws::args::parse_plugin_order_args;
use crate::ws::args::parse_set_setting_args;
use crate::ws::args::parse_uninstall_args;
use crate::ws::errors::core_error_to_ws;
use crate::ws::errors::ws_error;

/// Returns `true` only for routes whose execution is side-effect free.
///
/// `FULL_SYNC` may run one of these routes again when the daemon lost the recorded outcome.
/// Every other route, including arbitrary plugin methods, is conservatively treated as potentially
/// mutating and is never re-executed without a recorded outcome.
pub(crate) fn is_replay_safe(route: &str) -> bool {
    matches!(
        route,
        "utilities/ping" | "utilities/filepicker" | "loader/get_plugins" | "loader/get_settings"
    )
}

pub(crate) async fn dispatch_call(state: AppState, call: WsCallPayload) -> WsServerMessage {
    match call.route.as_str() {
        "utilities/ping" => WsServerMessage::Reply {
            id: call.id,
            result: json!("pong"),
        },
        "utilities/filepicker" => dispatch_filepicker(&state, &call).await,
        "loader/get_plugins" => WsServerMessage::Reply {
            id: call.id,
            result: serialize_plugins_list(&state),
        },
        "loader/call_plugin_method" => dispatch_plugin_rpc(&state, &call, false).await,
        "loader/call_legacy_plugin_method" => dispatch_plugin_rpc(&state, &call, true).await,
        "loader/reload_plugin" => dispatch_reload_plugin(&state, &call).await,
        "loader/install_plugin" | "utilities/install_plugin" => {
            dispatch_install_plugin(&state, &call).await
        }
        "loader/uninstall_plugin" => dispatch_uninstall_plugin(&state, &call).await,
        "loader/enable_plugin" => dispatch_enable_plugin(&state, &call).await,
        "loader/disable_plugin" => dispatch_disable_plugin(&state, &call).await,
        "loader/set_plugin_order" => dispatch_set_plugin_order(&state, &call).await,
        "loader/get_settings" => dispatch_get_settings(&state, &call),
        "loader/set_setting" => dispatch_set_setting(&state, &call).await,
        other => ws_error(
            call.id,
            "RouteNotFoundError",
            format!("Route {other} does not exist."),
            None,
        ),
    }
}

pub(crate) fn serialize_plugin_summaries(plugins: &[PluginSummary]) -> Value {
    Value::Array(
        plugins
            .iter()
            .map(|p| {
                json!({
                    "name": p.name,
                    "version": p.version,
                    "load_type": p.load_type,
                    "disabled": p.disabled,
                    "passive": p.passive,
                    "state": format!("{:?}", p.state),
                    "failure_reason": p.failure_reason,
                })
            })
            .collect(),
    )
}

fn serialize_plugins_list(state: &AppState) -> Value {
    match state.backend() {
        PluginBackend::Static(plugins) => Value::Array(
            plugins
                .iter()
                .map(|p| {
                    json!({
                        "name": p.manifest.name,
                        "version": p.package.as_ref().and_then(|pkg| pkg.version.as_deref()),
                        "load_type": p.load_type,
                        "disabled": false,
                    })
                })
                .collect(),
        ),
        PluginBackend::Live(manager) => serialize_plugin_summaries(&manager.list_plugins()),
    }
}

fn require_manager(state: &AppState, id: u64) -> Result<&Arc<PluginManager>, WsServerMessage> {
    match state.backend() {
        PluginBackend::Live(manager) => Ok(manager),
        PluginBackend::Static(_) => Err(ws_error(
            id,
            "BackendError",
            "plugin manager is not initialized",
            None,
        )),
    }
}

async fn dispatch_plugin_rpc(
    state: &AppState,
    call: &WsCallPayload,
    legacy: bool,
) -> WsServerMessage {
    let Some(plugin_name) = call.args.first().and_then(Value::as_str) else {
        return ws_error(call.id, "ValueError", "missing plugin_name argument", None);
    };
    let Some(method_name) = call.args.get(1).and_then(Value::as_str) else {
        return ws_error(call.id, "ValueError", "missing method_name argument", None);
    };
    if plugin_name.is_empty() || method_name.is_empty() || method_name.starts_with('_') {
        return ws_error(
            call.id,
            "PermissionError",
            format!("cannot call private or empty plugin method `{method_name}`"),
            None,
        );
    }

    let payload_args = if legacy {
        match call.args.get(2) {
            None | Some(Value::Null) => Value::Object(Map::new()),
            Some(Value::Object(obj)) => Value::Object(obj.clone()),
            Some(_) => {
                return ws_error(
                    call.id,
                    "ValueError",
                    "legacy plugin call kwargs must be a JSON object",
                    None,
                );
            }
        }
    } else {
        Value::Array(call.args.iter().skip(2).cloned().collect())
    };

    let PluginBackend::Live(manager) = state.backend() else {
        return ws_error(
            call.id,
            "PluginNotFoundError",
            format!("plugin `{plugin_name}` is not loaded"),
            None,
        );
    };

    // Plugin results are already JSON; hand them over without re-serializing a copy.
    match manager
        .call_plugin_method(plugin_name, method_name, payload_args, legacy)
        .await
    {
        Ok(result) => WsServerMessage::Reply {
            id: call.id,
            result,
        },
        Err(err) => core_error_to_ws(call.id, err),
    }
}

async fn dispatch_reload_plugin(state: &AppState, call: &WsCallPayload) -> WsServerMessage {
    let plugin_name = match parse_plugin_name_arg(&call.args) {
        Ok(name) => name,
        Err(msg) => return ws_error(call.id, "ValueError", msg, None),
    };
    let PluginBackend::Live(manager) = state.backend() else {
        return ws_error(
            call.id,
            "PluginNotFoundError",
            format!("plugin `{plugin_name}` is not loaded"),
            None,
        );
    };
    reply_done(call.id, manager.reload_plugin(&plugin_name).await)
}

async fn dispatch_filepicker(state: &AppState, call: &WsCallPayload) -> WsServerMessage {
    let manager = match require_manager(state, call.id) {
        Ok(m) => m,
        Err(err_msg) => return err_msg,
    };
    let query = match parse_filepicker_query(&call.args) {
        Ok(q) => q,
        Err(msg) => return ws_error(call.id, "ValueError", msg, None),
    };
    reply(call.id, manager.list_files(query).await)
}

async fn dispatch_install_plugin(state: &AppState, call: &WsCallPayload) -> WsServerMessage {
    let manager = match require_manager(state, call.id) {
        Ok(m) => m,
        Err(err_msg) => return err_msg,
    };
    let req = match parse_install_request(&call.args) {
        Ok(r) => r,
        Err(msg) => return ws_error(call.id, "ValueError", msg, None),
    };
    reply(call.id, manager.install_plugin(req).await)
}

async fn dispatch_uninstall_plugin(state: &AppState, call: &WsCallPayload) -> WsServerMessage {
    let manager = match require_manager(state, call.id) {
        Ok(m) => m,
        Err(err_msg) => return err_msg,
    };
    let (plugin_name, purge_data) = match parse_uninstall_args(&call.args) {
        Ok(v) => v,
        Err(msg) => return ws_error(call.id, "ValueError", msg, None),
    };
    reply_done(
        call.id,
        manager.uninstall_plugin(&plugin_name, purge_data).await,
    )
}

async fn dispatch_enable_plugin(state: &AppState, call: &WsCallPayload) -> WsServerMessage {
    let manager = match require_manager(state, call.id) {
        Ok(m) => m,
        Err(err_msg) => return err_msg,
    };
    let plugin_name = match parse_plugin_name_arg(&call.args) {
        Ok(n) => n,
        Err(msg) => return ws_error(call.id, "ValueError", msg, None),
    };
    reply_done(call.id, manager.enable_plugin(&plugin_name).await)
}

async fn dispatch_disable_plugin(state: &AppState, call: &WsCallPayload) -> WsServerMessage {
    let manager = match require_manager(state, call.id) {
        Ok(m) => m,
        Err(err_msg) => return err_msg,
    };
    let plugin_name = match parse_plugin_name_arg(&call.args) {
        Ok(n) => n,
        Err(msg) => return ws_error(call.id, "ValueError", msg, None),
    };
    reply_done(call.id, manager.disable_plugin(&plugin_name).await)
}

async fn dispatch_set_plugin_order(state: &AppState, call: &WsCallPayload) -> WsServerMessage {
    let manager = match require_manager(state, call.id) {
        Ok(m) => m,
        Err(err_msg) => return err_msg,
    };
    let order = match parse_plugin_order_args(&call.args) {
        Ok(o) => o,
        Err(msg) => return ws_error(call.id, "ValueError", msg, None),
    };
    reply_done(call.id, manager.set_plugin_order(order).await)
}

fn dispatch_get_settings(state: &AppState, call: &WsCallPayload) -> WsServerMessage {
    let manager = match require_manager(state, call.id) {
        Ok(m) => m,
        Err(err_msg) => return err_msg,
    };
    let key = match parse_get_settings_key(&call.args) {
        Ok(k) => k,
        Err(msg) => return ws_error(call.id, "ValueError", msg, None),
    };
    WsServerMessage::Reply {
        id: call.id,
        result: manager.get_setting(key.as_deref()),
    }
}

async fn dispatch_set_setting(state: &AppState, call: &WsCallPayload) -> WsServerMessage {
    let manager = match require_manager(state, call.id) {
        Ok(m) => m,
        Err(err_msg) => return err_msg,
    };
    let (key, value) = match parse_set_setting_args(&call.args) {
        Ok(pair) => pair,
        Err(msg) => return ws_error(call.id, "ValueError", msg, None),
    };
    reply_done(call.id, manager.set_setting(&key, value).await)
}

/// Turns a manager result into a `REPLY` carrying the serialized value, or an `ERROR` frame.
pub(crate) fn reply<T: Serialize>(id: u64, result: Result<T, CoreError>) -> WsServerMessage {
    match result.map(serde_json::to_value) {
        Ok(Ok(result)) => WsServerMessage::Reply { id, result },
        Ok(Err(err)) => ws_error(id, "BackendError", err.to_string(), None),
        Err(err) => core_error_to_ws(id, err),
    }
}

/// [`reply`] for mutations that return nothing: a successful call answers `true`.
pub(crate) fn reply_done(id: u64, result: Result<(), CoreError>) -> WsServerMessage {
    reply(id, result.map(|()| true))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use zerodeck_core::CoreError;
    use zerodeck_plugin_host::HostError;
    use zerodeck_protocol::WsServerMessage;

    use super::{is_replay_safe, reply, reply_done};

    #[test]
    fn replay_safety_is_an_explicit_read_only_allowlist() {
        for route in [
            "utilities/ping",
            "utilities/filepicker",
            "loader/get_plugins",
            "loader/get_settings",
        ] {
            assert!(is_replay_safe(route), "{route} must be replay-safe");
        }
        for route in [
            "loader/call_plugin_method",
            "loader/call_legacy_plugin_method",
            "loader/install_plugin",
            "utilities/install_plugin",
            "loader/uninstall_plugin",
            "loader/enable_plugin",
            "loader/disable_plugin",
            "loader/set_plugin_order",
            "loader/set_setting",
            "loader/reload_plugin",
            "utilities/unknown",
        ] {
            assert!(!is_replay_safe(route), "{route} must not be replay-safe");
        }
    }

    #[test]
    fn reply_serializes_the_ok_value() {
        assert_eq!(
            reply(3, Ok::<_, CoreError>(vec!["a", "b"])),
            WsServerMessage::Reply {
                id: 3,
                result: json!(["a", "b"])
            }
        );
    }

    #[test]
    fn reply_done_answers_true() {
        assert_eq!(
            reply_done(4, Ok(())),
            WsServerMessage::Reply {
                id: 4,
                result: json!(true)
            }
        );
    }

    #[test]
    fn reply_maps_core_errors_to_error_frames() {
        let frame = reply_done(
            5,
            Err(CoreError::Host(HostError::PluginNotFound(
                "Gone".to_owned(),
            ))),
        );
        match frame {
            WsServerMessage::Error { id, error } => {
                assert_eq!(id, 5);
                assert_eq!(error.name, "PluginNotFoundError");
            }
            other => panic!("expected an error frame, got {other:?}"),
        }
    }
}
