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
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing::{Span, debug, debug_span, warn};
use uuid::Uuid;
use zerodeck_cef::CefConfig;
use zerodeck_core::DiscoveredPlugin;
use zerodeck_plugin_host::PythonHostConfig;
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
struct AppStateInner {
    csrf_token: String,
    config: ServerConfig,
    plugins: Vec<DiscoveredPlugin>,
}

impl AppState {
    /// Creates a new [`AppState`] with a fresh ephemeral CSRF token.
    #[must_use]
    pub fn new(config: ServerConfig) -> Self {
        Self::with_plugins(config, Vec::new())
    }

    /// Creates a new [`AppState`] preloaded with discovered plugin metadata.
    #[must_use]
    pub fn with_plugins(config: ServerConfig, plugins: Vec<DiscoveredPlugin>) -> Self {
        Self {
            inner: Arc::new(AppStateInner {
                csrf_token: Uuid::new_v4().to_string(),
                config,
                plugins,
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

    /// Returns the discovered plugins snapshot.
    #[must_use]
    pub fn plugins(&self) -> &[DiscoveredPlugin] {
        &self.inner.plugins
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

async fn handle_legacy_method(
    State(state): State<AppState>,
    Path(method_name): Path<String>,
) -> Response {
    match method_name.as_str() {
        "ping" => (
            StatusCode::OK,
            Json(json!({ "success": true, "result": "pong" })),
        )
            .into_response(),
        "get_plugins" => {
            let names: Vec<&str> = state
                .plugins()
                .iter()
                .map(|p| p.manifest.name.as_str())
                .collect();
            (
                StatusCode::OK,
                Json(json!({ "success": true, "result": names })),
            )
                .into_response()
        }
        _ => (
            StatusCode::NOT_FOUND,
            Json(json!({ "success": false, "result": format!("unknown method: {method_name}") })),
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

/// Drives one WebSocket client: inbound frames, outbound replies, and keepalive pings.
///
/// Replies flow through a channel so that call handlers can later run as independent tasks (for
/// slow plugin calls) without contending for the socket.
async fn handle_ws_socket(state: AppState, mut socket: WebSocket) {
    debug!("WebSocket client connected");
    let (outbox, mut outbound) = mpsc::channel::<WsServerMessage>(64);

    let mut keepalive = tokio::time::interval(state.config().ws_ping_interval);
    keepalive.set_missed_tick_behavior(MissedTickBehavior::Delay);
    keepalive.tick().await; // The first tick completes immediately.
    let mut awaiting_pong = false;

    loop {
        tokio::select! {
            inbound = socket.recv() => match inbound {
                Some(Ok(Message::Text(text))) => handle_ws_text(&state, &text, &outbox).await,
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

async fn handle_ws_text(state: &AppState, text: &str, outbox: &mpsc::Sender<WsServerMessage>) {
    match WsClientMessage::from_json_str(text) {
        Ok(WsClientMessage::Call(call)) => {
            let _ = outbox.send(dispatch_call(state, &call)).await;
        }
        Ok(WsClientMessage::ReceivedResponse { id }) => {
            debug!(call_id = id, "client acknowledged response");
        }
        Ok(WsClientMessage::FullSync { messages }) => {
            // The client replays calls that never got a reply before it reconnected; the previous
            // connection's state is gone, so each one is dispatched again.
            debug!(count = messages.len(), "replaying FULL_SYNC calls");
            for call in &messages {
                let _ = outbox.send(dispatch_call(state, call)).await;
            }
        }
        Err(err) => {
            warn!(error = %err, "rejected malformed WebSocket frame");
        }
    }
}

fn dispatch_call(state: &AppState, call: &WsCallPayload) -> WsServerMessage {
    match call.route.as_str() {
        "utilities/ping" => WsServerMessage::Reply {
            id: call.id,
            result: json!("pong"),
        },
        "loader/get_plugins" => {
            let list: Vec<Value> = state
                .plugins()
                .iter()
                .map(|p| {
                    json!({
                        "name": p.manifest.name,
                        "version": p.package.as_ref().and_then(|pkg| pkg.version.as_deref()),
                        "load_type": p.load_type,
                        "disabled": false,
                    })
                })
                .collect();
            WsServerMessage::Reply {
                id: call.id,
                result: Value::Array(list),
            }
        }
        other => WsServerMessage::Error {
            id: call.id,
            error: WsErrorDetail {
                name: "RouteNotFoundError".to_owned(),
                error: format!("Route {other} does not exist."),
                traceback: None,
            },
        },
    }
}
