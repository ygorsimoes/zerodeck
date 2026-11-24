use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use axum::Router;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, Query, State, WebSocketUpgrade};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing::{debug, warn};
use uuid::Uuid;
use zerodeck_cef::CefConfig;
use zerodeck_core::DiscoveredPlugin;
use zerodeck_plugin_host::PythonHostConfig;
use zerodeck_protocol::{WsClientMessage, WsErrorDetail, WsServerMessage};

use crate::ServerError;
use crate::auth::csrf_middleware;

/// Runtime configuration for the `ZeroDeck` HTTP/WebSocket server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub cef: CefConfig,
    pub python_host: PythonHostConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_owned(),
            port: 1337,
            cef: CefConfig::default(),
            python_host: PythonHostConfig::default(),
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
        .layer(TraceLayer::new_for_http())
        .with_state(state)
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
    if query.get("auth").map(String::as_str) != Some(state.csrf_token()) {
        return (StatusCode::FORBIDDEN, "Forbidden").into_response();
    }
    ws.on_upgrade(move |socket| handle_ws_socket(state, socket))
}

async fn handle_ws_socket(state: AppState, mut socket: WebSocket) {
    debug!("WebSocket client connected");
    while let Some(Ok(msg)) = socket.recv().await {
        let Message::Text(text) = msg else {
            continue;
        };
        match WsClientMessage::from_json_str(&text) {
            Ok(WsClientMessage::Call(call)) => {
                let reply = match call.route.as_str() {
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
                };
                if let Ok(encoded) = serde_json::to_string(&reply) {
                    let _ = socket.send(Message::Text(encoded.into())).await;
                }
            }
            Ok(WsClientMessage::ReceivedResponse { id }) => {
                debug!(call_id = id, "client acknowledged response");
            }
            Ok(WsClientMessage::FullSync { messages }) => {
                debug!(count = messages.len(), "processed FULL_SYNC replay");
            }
            Err(err) => {
                warn!(error = %err, "rejected malformed WebSocket frame");
            }
        }
    }
}
