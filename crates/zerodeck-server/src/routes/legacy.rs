//! Legacy `/methods/*` HTTP RPC endpoints.

use axum::body::Bytes;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use zerodeck_core::CoreError;
use zerodeck_plugin_host::HostError;

use crate::state::AppState;
use crate::state::PluginBackend;

#[derive(Debug, Deserialize)]
struct LegacyHttpPluginCall {
    plugin_name: String,
    method_name: String,
    #[serde(default)]
    args: Option<Value>,
    #[serde(default)]
    legacy: Option<bool>,
}

pub(crate) async fn handle_legacy_method(
    State(state): State<AppState>,
    AxumPath(method_name): AxumPath<String>,
    body: Bytes,
) -> Response {
    match method_name.as_str() {
        "ping" => (
            StatusCode::OK,
            Json(json!({ "success": true, "result": "pong" })),
        )
            .into_response(),
        "get_plugins" => {
            let names: Vec<String> = match state.backend() {
                PluginBackend::Static(plugins) => {
                    plugins.iter().map(|p| p.manifest.name.clone()).collect()
                }
                PluginBackend::Live(manager) => {
                    manager.list_plugins().into_iter().map(|p| p.name).collect()
                }
            };
            (
                StatusCode::OK,
                Json(json!({ "success": true, "result": names })),
            )
                .into_response()
        }
        "call_plugin_method" => handle_http_call_plugin_method(&state, &body).await,
        _ => (
            StatusCode::NOT_FOUND,
            Json(json!({ "success": false, "result": format!("unknown method: {method_name}") })),
        )
            .into_response(),
    }
}

async fn handle_http_call_plugin_method(state: &AppState, body: &[u8]) -> Response {
    let Ok(req) = serde_json::from_slice::<LegacyHttpPluginCall>(body) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "result": "invalid call_plugin_method JSON body" })),
        )
            .into_response();
    };

    if req.plugin_name.is_empty() || req.method_name.is_empty() || req.method_name.starts_with('_')
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "result": "invalid or private plugin method name" })),
        )
            .into_response();
    }

    let PluginBackend::Live(manager) = state.backend() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "success": false, "result": format!("plugin `{}` not found", req.plugin_name) })),
        )
            .into_response();
    };

    let Some(api_version) = manager.plugin_api_version(&req.plugin_name) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "success": false, "result": format!("plugin `{}` not found", req.plugin_name) })),
        )
            .into_response();
    };

    let legacy = req.legacy.unwrap_or(api_version == 0);
    let args = req
        .args
        .unwrap_or_else(|| if legacy { json!({}) } else { json!([]) });

    match manager
        .call_plugin_method(&req.plugin_name, &req.method_name, args, legacy)
        .await
    {
        Ok(result) => (
            StatusCode::OK,
            Json(json!({ "success": true, "result": result })),
        )
            .into_response(),
        Err(CoreError::Host(HostError::PluginCallFailed {
            name,
            message,
            traceback,
        })) => (
            StatusCode::OK,
            Json(json!({
                "success": false,
                "result": {
                    "name": name,
                    "error": message,
                    "traceback": traceback,
                }
            })),
        )
            .into_response(),
        Err(CoreError::Host(HostError::PluginNotFound(name))) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "success": false, "result": format!("plugin `{name}` not found") })),
        )
            .into_response(),
        Err(CoreError::Host(HostError::InvalidMethodName(m))) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "result": format!("invalid method `{m}`") })),
        )
            .into_response(),
        Err(
            err @ (CoreError::PluginDisabled(_)
            | CoreError::PluginBlocked { .. }
            | CoreError::Host(
                HostError::PassivePlugin(_)
                | HostError::PluginFailed(_)
                | HostError::WorkerStopped
                | HostError::StartupTimeout,
            )),
        ) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "success": false, "result": err.to_string() })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "success": false, "result": err.to_string() })),
        )
            .into_response(),
    }
}
