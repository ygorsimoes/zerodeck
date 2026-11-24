use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, Query, State, WebSocketUpgrade};
use axum::http::{HeaderValue, Method, Request, StatusCode, Uri, header};
use axum::middleware;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};
use tokio::time::MissedTickBehavior;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing::{Span, debug, debug_span, warn};
use uuid::Uuid;
use zerodeck_cef::CefConfig;
use zerodeck_core::{CoreError, DiscoveredPlugin, PluginManager};
use zerodeck_plugin_host::{HostError, PluginEvent, PythonHostConfig};
use zerodeck_protocol::{WsCallPayload, WsClientMessage, WsErrorDetail, WsServerMessage};

use crate::ServerError;
use crate::auth::{csrf_middleware, host_guard};

/// Runtime configuration for the `ZeroDeck` HTTP/WebSocket server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub cef: CefConfig,
    pub python_host: PythonHostConfig,
    /// Interval between server keepalive pings; a client that misses one pong is disconnected.
    pub ws_ping_interval: Duration,
    /// Maximum size of a single inbound WebSocket message (and frame), in bytes.
    pub ws_max_message_bytes: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_owned(),
            port: 1337,
            cef: CefConfig::default(),
            python_host: PythonHostConfig::default(),
            ws_ping_interval: Duration::from_secs(30),
            ws_max_message_bytes: 16 * 1024 * 1024,
        }
    }
}

/// Shared application state passed across Axum handlers and middleware.
#[derive(Debug, Clone)]
pub struct AppState {
    inner: Arc<AppStateInner>,
}

#[derive(Debug)]
enum PluginBackend {
    Static(Vec<DiscoveredPlugin>),
    Live(Arc<PluginManager>),
}

#[derive(Debug)]
struct AppStateInner {
    csrf_token: String,
    config: ServerConfig,
    backend: PluginBackend,
}

impl AppState {
    /// Creates a new [`AppState`] with a fresh ephemeral CSRF token and no plugins.
    #[must_use]
    pub fn new(config: ServerConfig) -> Self {
        Self::with_plugins(config, Vec::new())
    }

    /// Creates a new [`AppState`] preloaded with static discovered plugin metadata.
    #[must_use]
    pub fn with_plugins(config: ServerConfig, plugins: Vec<DiscoveredPlugin>) -> Self {
        Self {
            inner: Arc::new(AppStateInner {
                csrf_token: Uuid::new_v4().to_string(),
                config,
                backend: PluginBackend::Static(plugins),
            }),
        }
    }

    /// Creates a new [`AppState`] backed by a live [`PluginManager`].
    #[must_use]
    pub fn with_manager(config: ServerConfig, manager: Arc<PluginManager>) -> Self {
        Self {
            inner: Arc::new(AppStateInner {
                csrf_token: Uuid::new_v4().to_string(),
                config,
                backend: PluginBackend::Live(manager),
            }),
        }
    }

    /// Returns the daemon's active CSRF token.
    #[must_use]
    pub fn csrf_token(&self) -> &str {
        &self.inner.csrf_token
    }

    /// Returns the active server configuration.
    #[must_use]
    pub fn config(&self) -> &ServerConfig {
        &self.inner.config
    }

    fn subscribe_plugin_events(&self) -> Option<broadcast::Receiver<PluginEvent>> {
        match &self.inner.backend {
            PluginBackend::Static(_) => None,
            PluginBackend::Live(manager) => Some(manager.subscribe_events()),
        }
    }
}

/// Constructs the Axum [`Router`] with strict CORS, CSRF enforcement, and core endpoints.
pub fn build_router(state: AppState) -> Router {
    let steam_origin = HeaderValue::from_static("https://steamloopback.host");
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::exact(steam_origin))
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::PATCH,
            Method::OPTIONS,
        ])
        .allow_headers([
            header::CONTENT_TYPE,
            header::AUTHORIZATION,
            header::HeaderName::from_static("x-decky-auth"),
        ])
        .allow_credentials(true);

    Router::new()
        .route("/healthz", get(handle_healthz))
        .route("/auth/token", get(handle_auth_token))
        .route("/ws", get(handle_ws_upgrade))
        .route("/methods/{method_name}", post(handle_legacy_method))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            csrf_middleware,
        ))
        .layer(cors)
        .layer(middleware::from_fn(host_guard))
        .layer(TraceLayer::new_for_http().make_span_with(make_request_span))
        .with_state(state)
}

/// Builds the per-request tracing span without leaking the `?auth=` token into logs.
fn make_request_span<B>(request: &Request<B>) -> Span {
    debug_span!(
        "request",
        method = %request.method(),
        uri = %redacted_uri(request.uri()),
        version = ?request.version(),
    )
}

/// Renders `uri` as path and query with the value of any `auth` query parameter replaced.
pub(crate) fn redacted_uri(uri: &Uri) -> String {
    let Some(query) = uri.query() else {
        return uri.path().to_owned();
    };
    let redacted: Vec<&str> = query
        .split('&')
        .map(|pair| {
            if pair == "auth" || pair.starts_with("auth=") {
                "auth=[redacted]"
            } else {
                pair
            }
        })
        .collect();
    format!("{}?{}", uri.path(), redacted.join("&"))
}

/// Serves the `ZeroDeck` HTTP/WebSocket router until `shutdown` resolves.
pub async fn serve<F>(
    listener: TcpListener,
    state: AppState,
    shutdown: F,
) -> Result<(), ServerError>
where
    F: Future<Output = ()> + Send + 'static,
{
    axum::serve(listener, build_router(state))
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}

async fn handle_healthz() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

async fn handle_auth_token(State(state): State<AppState>) -> String {
    state.csrf_token().to_owned()
}

#[derive(Debug, Deserialize)]
struct LegacyHttpPluginCall {
    plugin_name: String,
    method_name: String,
    #[serde(default)]
    args: Option<Value>,
    #[serde(default)]
    legacy: Option<bool>,
}

async fn handle_legacy_method(
    State(state): State<AppState>,
    Path(method_name): Path<String>,
    body: Bytes,
) -> Response {
    match method_name.as_str() {
        "ping" => (
            StatusCode::OK,
            Json(json!({ "success": true, "result": "pong" })),
        )
            .into_response(),
        "get_plugins" => {
            let names: Vec<String> = match &state.inner.backend {
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

    let PluginBackend::Live(manager) = &state.inner.backend else {
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

async fn handle_ws_upgrade(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> Response {
    let authorized = query
        .get("auth")
        .is_some_and(|provided| crate::auth::tokens_match(provided, state.csrf_token()));
    if !authorized {
        return (StatusCode::FORBIDDEN, "Forbidden").into_response();
    }
    let max_bytes = state.config().ws_max_message_bytes;
    ws.max_message_size(max_bytes)
        .max_frame_size(max_bytes)
        .on_upgrade(move |socket| handle_ws_socket(state, socket))
}

async fn recv_plugin_event(rx: &mut Option<broadcast::Receiver<PluginEvent>>) -> WsServerMessage {
    let Some(receiver) = rx.as_mut() else {
        return std::future::pending().await;
    };
    loop {
        match receiver.recv().await {
            Ok(ev) => {
                return WsServerMessage::Event {
                    event: "loader/plugin_event".to_owned(),
                    args: vec![json!({
                        "plugin": ev.plugin,
                        "event": ev.event,
                        "args": ev.args,
                    })],
                };
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                warn!(skipped, "WebSocket client lagged on plugin event broadcast");
            }
            Err(broadcast::error::RecvError::Closed) => {
                *rx = None;
                return std::future::pending().await;
            }
        }
    }
}

/// Drives one WebSocket client: inbound frames, spawned RPC replies, plugin events, and keepalive pings.
async fn handle_ws_socket(state: AppState, mut socket: WebSocket) {
    debug!("WebSocket client connected");
    let (outbox, mut outbound) = mpsc::channel::<WsServerMessage>(64);
    let mut plugin_events = state.subscribe_plugin_events();

    let mut keepalive = tokio::time::interval(state.config().ws_ping_interval);
    keepalive.set_missed_tick_behavior(MissedTickBehavior::Delay);
    keepalive.tick().await; // The first tick completes immediately.
    let mut awaiting_pong = false;

    loop {
        tokio::select! {
            inbound = socket.recv() => match inbound {
                Some(Ok(Message::Text(text))) => handle_ws_text(&state, &text, &outbox),
                Some(Ok(Message::Pong(_))) => awaiting_pong = false,
                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                Some(Ok(Message::Binary(_) | Message::Ping(_))) => {}
            },
            Some(reply) = outbound.recv() => {
                let Ok(encoded) = serde_json::to_string(&reply) else { continue };
                if socket.send(Message::Text(encoded.into())).await.is_err() {
                    break;
                }
            },
            event_msg = recv_plugin_event(&mut plugin_events) => {
                let Ok(encoded) = serde_json::to_string(&event_msg) else { continue };
                if socket.send(Message::Text(encoded.into())).await.is_err() {
                    break;
                }
            },
            _ = keepalive.tick() => {
                if awaiting_pong {
                    warn!("WebSocket client missed keepalive pong; disconnecting");
                    break;
                }
                awaiting_pong = true;
                if socket.send(Message::Ping(Bytes::new())).await.is_err() {
                    break;
                }
            },
        }
    }
    debug!("WebSocket client disconnected");
}

fn handle_ws_text(state: &AppState, text: &str, outbox: &mpsc::Sender<WsServerMessage>) {
    match WsClientMessage::from_json_str(text) {
        Ok(WsClientMessage::Call(call)) => {
            spawn_ws_call(state.clone(), call, outbox.clone());
        }
        Ok(WsClientMessage::ReceivedResponse { id }) => {
            debug!(call_id = id, "client acknowledged response");
        }
        Ok(WsClientMessage::FullSync { messages }) => {
            // The client replays calls that never got a reply before it reconnected; the previous
            // connection's state is gone, so each one is dispatched again.
            debug!(count = messages.len(), "replaying FULL_SYNC calls");
            for call in messages {
                spawn_ws_call(state.clone(), call, outbox.clone());
            }
        }
        Err(err) => {
            warn!(error = %err, "rejected malformed WebSocket frame");
        }
    }
}

fn spawn_ws_call(state: AppState, call: WsCallPayload, outbox: mpsc::Sender<WsServerMessage>) {
    tokio::spawn(async move {
        let reply = dispatch_call(state, call).await;
        let _ = outbox.send(reply).await;
    });
}

async fn dispatch_call(state: AppState, call: WsCallPayload) -> WsServerMessage {
    match call.route.as_str() {
        "utilities/ping" => WsServerMessage::Reply {
            id: call.id,
            result: json!("pong"),
        },
        "loader/get_plugins" => WsServerMessage::Reply {
            id: call.id,
            result: serialize_plugins_list(&state),
        },
        "loader/call_plugin_method" => dispatch_plugin_rpc(&state, &call, false).await,
        "loader/call_legacy_plugin_method" => dispatch_plugin_rpc(&state, &call, true).await,
        "loader/reload_plugin" => dispatch_reload_plugin(&state, &call).await,
        other => ws_error(
            call.id,
            "RouteNotFoundError",
            format!("Route {other} does not exist."),
            None,
        ),
    }
}

fn serialize_plugins_list(state: &AppState) -> Value {
    let list: Vec<Value> = match &state.inner.backend {
        PluginBackend::Static(plugins) => plugins
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
        PluginBackend::Live(manager) => manager
            .list_plugins()
            .into_iter()
            .map(|p| {
                json!({
                    "name": p.name,
                    "version": p.version,
                    "load_type": p.load_type,
                    "disabled": p.disabled,
                    "passive": p.passive,
                    "state": format!("{:?}", p.state),
                })
            })
            .collect(),
    };
    Value::Array(list)
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
            None | Some(Value::Null) => Value::Object(serde_json::Map::new()),
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

    let PluginBackend::Live(manager) = &state.inner.backend else {
        return ws_error(
            call.id,
            "PluginNotFoundError",
            format!("plugin `{plugin_name}` is not loaded"),
            None,
        );
    };

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
    let Some(plugin_name) = call.args.first().and_then(Value::as_str) else {
        return ws_error(call.id, "ValueError", "missing plugin_name argument", None);
    };
    let PluginBackend::Live(manager) = &state.inner.backend else {
        return ws_error(
            call.id,
            "PluginNotFoundError",
            format!("plugin `{plugin_name}` is not loaded"),
            None,
        );
    };

    match manager.reload_plugin(plugin_name).await {
        Ok(()) => WsServerMessage::Reply {
            id: call.id,
            result: Value::Bool(true),
        },
        Err(err) => core_error_to_ws(call.id, err),
    }
}

fn core_error_to_ws(id: u64, err: CoreError) -> WsServerMessage {
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
        CoreError::Host(HostError::InvalidMethodName(method)) => ws_error(
            id,
            "PermissionError",
            format!("invalid method `{method}`"),
            None,
        ),
        other => ws_error(id, "BackendError", other.to_string(), None),
    }
}

fn ws_error(
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
