use std::collections::HashMap;
use std::error::Error as StdError;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path as AxumPath, Query, State, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode, Uri, header};
use axum::middleware;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{any, get, post};
use percent_encoding::percent_decode_str;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, broadcast, mpsc};
use tokio::time::MissedTickBehavior;
use tokio_util::io::ReaderStream;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing::{Span, debug, debug_span, warn};
use uuid::Uuid;
use zerodeck_cef::CefConfig;
use zerodeck_core::{
    CheckedHttpClient, CoreError, DEFAULT_FILEPICKER_MAX_ENTRIES, DEFAULT_MAX_REMOTE_BINARY_BYTES,
    DEFAULT_MAX_ZIP_DOWNLOAD_BYTES, DiscoveredPlugin, FilePickerQuery, InstallPluginRequest,
    LoaderEvent, OutboundHttpPolicy, PluginFileScope, PluginManager, PluginSummary,
    parse_install_source, resolve_static_file,
};
use zerodeck_plugin_host::{HostError, PluginEvent, PythonHostConfig};
use zerodeck_protocol::{WsCallPayload, WsClientMessage, WsErrorDetail, WsServerMessage};

use crate::ServerError;
use crate::auth::{csrf_middleware, host_guard};
use crate::ws_sync::{SyncScope, WsSyncCache, parse_session_param};

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
    /// Optional path to the packaged `@zerodeck/frontend` dist directory.
    pub frontend_dir: Option<PathBuf>,
    /// Maximum compressed plugin ZIP archive size accepted by the installer.
    pub max_zip_download_bytes: usize,
    /// Maximum size of a single `remote_binary` download.
    pub max_remote_binary_bytes: usize,
    /// Connect timeout for outbound `/fetch` proxy requests.
    pub fetch_connect_timeout: Duration,
    /// Total response timeout for outbound `/fetch` proxy requests.
    pub fetch_response_timeout: Duration,
    /// Maximum number of completed unacknowledged replies kept in [`WsSyncCache`].
    pub ws_sync_cache_capacity: usize,
    /// Time-to-live for completed [`WsSyncCache`] entries.
    pub ws_sync_cache_ttl: Duration,
    /// Maximum number of calls one WebSocket connection runs at the same time.
    pub ws_max_in_flight_calls: usize,
    /// Maximum number of calls one connection may have waiting for a free slot; calls beyond
    /// in-flight + queued are rejected with `TooManyPendingCalls`.
    pub ws_max_queued_calls: usize,
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
            frontend_dir: None,
            max_zip_download_bytes: DEFAULT_MAX_ZIP_DOWNLOAD_BYTES,
            max_remote_binary_bytes: DEFAULT_MAX_REMOTE_BINARY_BYTES,
            fetch_connect_timeout: Duration::from_secs(10),
            fetch_response_timeout: Duration::from_secs(60),
            ws_sync_cache_capacity: 256,
            ws_sync_cache_ttl: Duration::from_secs(300),
            ws_max_in_flight_calls: 64,
            ws_max_queued_calls: 1024,
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
    http_client: CheckedHttpClient,
    ws_sync_cache: Arc<WsSyncCache>,
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
        let http_client = build_default_http_client(&config);
        Self::from_parts(config, PluginBackend::Static(plugins), http_client)
    }

    /// Creates a new [`AppState`] backed by a live [`PluginManager`].
    #[must_use]
    pub fn with_manager(config: ServerConfig, manager: Arc<PluginManager>) -> Self {
        let http_client = build_default_http_client(&config);
        Self::with_manager_and_http(config, manager, http_client)
    }

    /// Creates a new [`AppState`] backed by a live [`PluginManager`] and explicit [`CheckedHttpClient`].
    #[must_use]
    pub fn with_manager_and_http(
        config: ServerConfig,
        manager: Arc<PluginManager>,
        http_client: CheckedHttpClient,
    ) -> Self {
        Self::from_parts(config, PluginBackend::Live(manager), http_client)
    }

    /// Test helper constructing an [`AppState`] with a custom [`CheckedHttpClient`] and no manager.
    #[doc(hidden)]
    #[must_use]
    pub fn with_http_for_tests(config: ServerConfig, http_client: CheckedHttpClient) -> Self {
        Self::from_parts(config, PluginBackend::Static(Vec::new()), http_client)
    }

    fn from_parts(
        config: ServerConfig,
        backend: PluginBackend,
        http_client: CheckedHttpClient,
    ) -> Self {
        let ws_sync_cache = Arc::new(WsSyncCache::new(
            config.ws_sync_cache_capacity,
            config.ws_sync_cache_ttl,
        ));
        Self {
            inner: Arc::new(AppStateInner {
                csrf_token: Uuid::new_v4().to_string(),
                config,
                backend,
                http_client,
                ws_sync_cache,
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

    pub(crate) fn http_client(&self) -> &CheckedHttpClient {
        &self.inner.http_client
    }

    pub(crate) fn ws_sync_cache(&self) -> &Arc<WsSyncCache> {
        &self.inner.ws_sync_cache
    }

    fn subscribe_plugin_events(&self) -> Option<broadcast::Receiver<PluginEvent>> {
        match &self.inner.backend {
            PluginBackend::Static(_) => None,
            PluginBackend::Live(manager) => Some(manager.subscribe_events()),
        }
    }

    fn subscribe_loader_events(&self) -> Option<broadcast::Receiver<LoaderEvent>> {
        match &self.inner.backend {
            PluginBackend::Static(_) => None,
            PluginBackend::Live(manager) => Some(manager.subscribe_loader_events()),
        }
    }
}

fn build_default_http_client(config: &ServerConfig) -> CheckedHttpClient {
    OutboundHttpPolicy::production()
        .with_timeouts(
            Some(config.fetch_connect_timeout),
            Some(config.fetch_response_timeout),
        )
        .build_checked_client()
        .expect("build production HTTP client")
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
        .route("/fetch", any(crate::fetch::handle_fetch))
        .route("/methods/{method_name}", post(handle_legacy_method))
        .route("/frontend/{*path}", get(handle_frontend_file))
        .route("/static/{*path}", get(handle_static_file))
        .route("/locales/{*path}", get(handle_locales_file))
        .route(
            "/plugins/load_main/{name}",
            get(handle_plugin_frontend_bundle),
        )
        .route(
            "/plugins/{name}/frontend_bundle",
            get(handle_plugin_frontend_bundle),
        )
        .route("/plugins/{name}/dist/{*path}", get(handle_plugin_dist_file))
        .route(
            "/plugins/{name}/assets/{*path}",
            get(handle_plugin_assets_file),
        )
        .route("/plugins/{name}/data/{*path}", get(handle_plugin_data_file))
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

async fn handle_frontend_file(
    State(state): State<AppState>,
    method: Method,
    headers: HeaderMap,
    AxumPath(raw_path): AxumPath<String>,
) -> Response {
    let Some(root) = state.config().frontend_dir.as_deref() else {
        return (StatusCode::NOT_FOUND, "Not Found").into_response();
    };
    serve_static_under(root, &raw_path, method, &headers, false).await
}

async fn handle_static_file(
    State(state): State<AppState>,
    method: Method,
    headers: HeaderMap,
    AxumPath(raw_path): AxumPath<String>,
) -> Response {
    let Some(root) = state.config().frontend_dir.as_deref() else {
        return (StatusCode::NOT_FOUND, "Not Found").into_response();
    };
    serve_static_under(&root.join("static"), &raw_path, method, &headers, false).await
}

async fn handle_locales_file(
    State(state): State<AppState>,
    method: Method,
    headers: HeaderMap,
    AxumPath(raw_path): AxumPath<String>,
) -> Response {
    let Some(root) = state.config().frontend_dir.as_deref() else {
        return (StatusCode::NOT_FOUND, "Not Found").into_response();
    };
    serve_static_under(&root.join("locales"), &raw_path, method, &headers, false).await
}

async fn handle_plugin_frontend_bundle(
    State(state): State<AppState>,
    method: Method,
    headers: HeaderMap,
    AxumPath(raw_name): AxumPath<String>,
) -> Response {
    serve_plugin_scope_file(
        &state,
        &raw_name,
        PluginFileScope::FrontendBundle,
        "index.js",
        method,
        &headers,
        true,
    )
    .await
}

async fn handle_plugin_dist_file(
    State(state): State<AppState>,
    method: Method,
    headers: HeaderMap,
    AxumPath((raw_name, raw_path)): AxumPath<(String, String)>,
) -> Response {
    serve_plugin_scope_file(
        &state,
        &raw_name,
        PluginFileScope::Dist,
        &raw_path,
        method,
        &headers,
        false,
    )
    .await
}

async fn handle_plugin_assets_file(
    State(state): State<AppState>,
    method: Method,
    headers: HeaderMap,
    AxumPath((raw_name, raw_path)): AxumPath<(String, String)>,
) -> Response {
    serve_plugin_scope_file(
        &state,
        &raw_name,
        PluginFileScope::Assets,
        &raw_path,
        method,
        &headers,
        false,
    )
    .await
}

async fn handle_plugin_data_file(
    State(state): State<AppState>,
    method: Method,
    headers: HeaderMap,
    AxumPath((raw_name, raw_path)): AxumPath<(String, String)>,
) -> Response {
    serve_plugin_scope_file(
        &state,
        &raw_name,
        PluginFileScope::Data,
        &raw_path,
        method,
        &headers,
        false,
    )
    .await
}

async fn serve_static_under(
    root: &Path,
    raw_rel: &str,
    method: Method,
    headers: &HeaderMap,
    no_store: bool,
) -> Response {
    let rel_path = match decode_safe_rel_path(raw_rel) {
        Ok(p) => p,
        Err(err) => return err.into_response(),
    };
    match resolve_static_file(root, &rel_path) {
        Ok(resolved) => {
            stream_file_response(&resolved, method, no_store, if_modified_since(headers)).await
        }
        Err(err) => core_file_error_response(&err),
    }
}

async fn serve_plugin_scope_file(
    state: &AppState,
    raw_name: &str,
    scope: PluginFileScope,
    raw_rel: &str,
    method: Method,
    headers: &HeaderMap,
    no_store: bool,
) -> Response {
    let plugin_name = match decode_safe_segment(raw_name) {
        Ok(n) => n,
        Err(err) => return err.into_response(),
    };
    let rel_path = match decode_safe_rel_path(raw_rel) {
        Ok(p) => p,
        Err(err) => return err.into_response(),
    };
    let PluginBackend::Live(manager) = &state.inner.backend else {
        return (StatusCode::NOT_FOUND, "Not Found").into_response();
    };
    match manager.resolve_plugin_file(&plugin_name, scope, &rel_path) {
        Ok(resolved) => {
            stream_file_response(&resolved, method, no_store, if_modified_since(headers)).await
        }
        Err(err) => core_file_error_response(&err),
    }
}

fn decode_safe_segment(raw: &str) -> Result<String, (StatusCode, &'static str)> {
    let decoded = percent_decode_str(raw)
        .decode_utf8()
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid UTF-8 path"))?
        .into_owned();
    if decoded.contains('\0') || decoded.contains('\\') || decoded.contains('/') || decoded == ".."
    {
        return Err((StatusCode::FORBIDDEN, "Forbidden"));
    }
    if decoded.is_empty() || decoded == "." {
        return Err((StatusCode::NOT_FOUND, "Not Found"));
    }
    Ok(decoded)
}

fn decode_safe_rel_path(raw: &str) -> Result<String, (StatusCode, &'static str)> {
    let decoded = percent_decode_str(raw)
        .decode_utf8()
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid UTF-8 path"))?
        .into_owned();
    if decoded.contains('\0') || decoded.contains('\\') {
        return Err((StatusCode::FORBIDDEN, "Forbidden"));
    }
    if decoded.is_empty() {
        return Err((StatusCode::NOT_FOUND, "Not Found"));
    }
    for seg in decoded.split('/') {
        if seg == ".." {
            return Err((StatusCode::FORBIDDEN, "Forbidden"));
        }
        if seg.is_empty() {
            return Err((StatusCode::NOT_FOUND, "Not Found"));
        }
    }
    Ok(decoded)
}

fn core_file_error_response(err: &CoreError) -> Response {
    match err {
        CoreError::Host(HostError::PluginNotFound(_)) => {
            (StatusCode::NOT_FOUND, "Not Found").into_response()
        }
        CoreError::Io { source, .. } => io_error_status(source).into_response(),
        CoreError::Platform(plat) => {
            if let Some(io_err) =
                StdError::source(plat).and_then(|s| s.downcast_ref::<std::io::Error>())
            {
                io_error_status(io_err).into_response()
            } else if err.is_path_escape() {
                (StatusCode::FORBIDDEN, "Forbidden").into_response()
            } else {
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
            }
        }
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response(),
    }
}

fn io_error_status(err: &std::io::Error) -> (StatusCode, &'static str) {
    match err.kind() {
        std::io::ErrorKind::NotFound => (StatusCode::NOT_FOUND, "Not Found"),
        std::io::ErrorKind::PermissionDenied => (StatusCode::FORBIDDEN, "Forbidden"),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error"),
    }
}

fn if_modified_since(headers: &HeaderMap) -> Option<SystemTime> {
    headers
        .get(header::IF_MODIFIED_SINCE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| httpdate::parse_http_date(v).ok())
}

/// Whole seconds since the Unix epoch (HTTP dates have second precision).
fn epoch_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

async fn stream_file_response(
    path: &Path,
    method: Method,
    no_store: bool,
    if_modified_since: Option<SystemTime>,
) -> Response {
    let content_type = mime_for_path(path);
    let build_err = |_| (StatusCode::INTERNAL_SERVER_ERROR, "Error").into_response();
    let file = if method == Method::HEAD {
        None
    } else {
        match tokio::fs::File::open(path).await {
            Ok(f) => Some(f),
            Err(err) => return io_error_status(&err).into_response(),
        }
    };
    let meta = match &file {
        Some(f) => f.metadata().await,
        None => tokio::fs::metadata(path).await,
    };
    let Ok(meta) = meta else {
        return if file.is_some() {
            (StatusCode::INTERNAL_SERVER_ERROR, "Error").into_response()
        } else {
            (StatusCode::NOT_FOUND, "Not Found").into_response()
        };
    };
    let modified = meta.modified().ok();

    // RFC 7232 section 3.3: an `If-Modified-Since` later than the current time is ignored.
    if !no_store
        && let (Some(since), Some(mtime)) = (if_modified_since, modified)
        && since <= SystemTime::now()
        && epoch_secs(mtime) <= epoch_secs(since)
    {
        let builder = Response::builder().status(StatusCode::NOT_MODIFIED);
        return with_file_validators(builder, modified)
            .body(Body::empty())
            .unwrap_or_else(build_err);
    }

    let builder = build_file_headers(
        Response::builder(),
        content_type,
        Some(meta.len()),
        no_store,
        modified,
    );
    let body = match file {
        Some(f) => Body::from_stream(ReaderStream::new(f)),
        None => Body::empty(),
    };
    builder.body(body).unwrap_or_else(build_err)
}

fn with_file_validators(
    mut builder: axum::http::response::Builder,
    modified: Option<SystemTime>,
) -> axum::http::response::Builder {
    builder = builder.header(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    if let Some(mtime) = modified {
        let secs = UNIX_EPOCH + Duration::from_secs(epoch_secs(mtime));
        builder = builder.header(header::LAST_MODIFIED, httpdate::fmt_http_date(secs));
    }
    builder
}

fn build_file_headers(
    mut builder: axum::http::response::Builder,
    content_type: &'static str,
    content_length: Option<u64>,
    no_store: bool,
    modified: Option<SystemTime>,
) -> axum::http::response::Builder {
    builder = builder
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type);
    if let Some(len) = content_length {
        builder = builder.header(header::CONTENT_LENGTH, len);
    }
    if no_store {
        builder = builder.header(header::CACHE_CONTROL, "no-store");
    }
    with_file_validators(builder, modified)
}

pub(crate) fn mime_for_path(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("html") => "text/html; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("woff2") => "font/woff2",
        Some("wasm") => "application/wasm",
        Some("gif") => "image/gif",
        Some("ico") => "image/x-icon",
        Some("avif") => "image/avif",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        Some("woff") => "font/woff",
        Some("txt") => "text/plain; charset=utf-8",
        Some("mp3") => "audio/mpeg",
        Some("ogg") => "audio/ogg",
        Some("wav") => "audio/wav",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        _ => "application/octet-stream",
    }
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
    let Ok(scope) = parse_session_param(query.get("session").map(String::as_str)) else {
        return (StatusCode::BAD_REQUEST, "Invalid session").into_response();
    };
    let max_bytes = state.config().ws_max_message_bytes;
    ws.max_message_size(max_bytes)
        .max_frame_size(max_bytes)
        .on_upgrade(move |socket| handle_ws_socket(state, socket, scope))
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

async fn recv_loader_event(rx: &mut Option<broadcast::Receiver<LoaderEvent>>) -> WsServerMessage {
    let Some(receiver) = rx.as_mut() else {
        return std::future::pending().await;
    };
    loop {
        match receiver.recv().await {
            Ok(LoaderEvent::PluginsChanged(summaries)) => {
                return WsServerMessage::Event {
                    event: "loader/plugins_changed".to_owned(),
                    args: vec![serialize_plugin_summaries(&summaries)],
                };
            }
            Ok(LoaderEvent::PluginReloaded { plugin, directory }) => {
                return WsServerMessage::Event {
                    event: "zerodeck/plugin_reloaded".to_owned(),
                    args: vec![json!({ "plugin": plugin, "directory": directory })],
                };
            }
            Ok(LoaderEvent::FrontendReloaded { plugin, directory }) => {
                return WsServerMessage::Event {
                    event: "zerodeck/frontend_reloaded".to_owned(),
                    args: vec![json!({ "plugin": plugin, "directory": directory })],
                };
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                warn!(skipped, "WebSocket client lagged on loader event broadcast");
            }
            Err(broadcast::error::RecvError::Closed) => {
                *rx = None;
                return std::future::pending().await;
            }
        }
    }
}

/// Drives one WebSocket client: inbound frames, spawned RPC replies, plugin/loader events, and
/// keepalive pings.
async fn handle_ws_socket(state: AppState, mut socket: WebSocket, scope: SyncScope) {
    debug!("WebSocket client connected");
    let (outbox, mut outbound) = mpsc::channel::<WsServerMessage>(64);
    let mut plugin_events = state.subscribe_plugin_events();
    let mut loader_events = state.subscribe_loader_events();

    let mut keepalive = tokio::time::interval(state.config().ws_ping_interval);
    keepalive.set_missed_tick_behavior(MissedTickBehavior::Delay);
    keepalive.tick().await;
    let mut awaiting_pong = false;
    let limiter = CallLimiter::new(
        state.config().ws_max_in_flight_calls,
        state.config().ws_max_queued_calls,
    );

    loop {
        tokio::select! {
            inbound = socket.recv() => match inbound {
                Some(Ok(Message::Text(text))) => {
                    let rejections = handle_ws_text(&state, &scope, &limiter, &text, &outbox);
                    if send_frames(&mut socket, rejections).await.is_err() {
                        break;
                    }
                }
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
            loader_msg = recv_loader_event(&mut loader_events) => {
                let Ok(encoded) = serde_json::to_string(&loader_msg) else { continue };
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

/// Per-connection bound on WebSocket calls: at most `in_flight` run at once and at most
/// `in_flight + queued` are pending (running or waiting for a slot). It never blocks the socket
/// loop, so keepalive pongs keep flowing while the queue is full.
#[derive(Debug, Clone)]
struct CallLimiter {
    permits: Arc<Semaphore>,
    pending: Arc<AtomicUsize>,
    capacity: usize,
}

impl CallLimiter {
    fn new(in_flight: usize, queued: usize) -> Self {
        let in_flight = in_flight.max(1);
        Self {
            permits: Arc::new(Semaphore::new(in_flight)),
            pending: Arc::new(AtomicUsize::new(0)),
            capacity: in_flight.saturating_add(queued),
        }
    }

    /// Reserves a pending slot, or `None` when the connection is at capacity.
    fn try_admit(&self) -> Option<PendingGuard> {
        self.pending
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |pending| {
                (pending < self.capacity).then_some(pending + 1)
            })
            .ok()
            .map(|_| PendingGuard {
                pending: Arc::clone(&self.pending),
            })
    }
}

/// Releases one pending slot of a [`CallLimiter`] when the call finishes (or is dropped).
#[derive(Debug)]
struct PendingGuard {
    pending: Arc<AtomicUsize>,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.pending.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Handles one inbound text frame; returns the frames to send right away (calls rejected
/// because the connection is at capacity).
fn handle_ws_text(
    state: &AppState,
    scope: &SyncScope,
    limiter: &CallLimiter,
    text: &str,
    outbox: &mpsc::Sender<WsServerMessage>,
) -> Vec<WsServerMessage> {
    let mut rejections = Vec::new();
    let mut admit = |call: WsCallPayload| match limiter.try_admit() {
        Some(guard) => spawn_ws_call(state, scope, limiter, guard, call, outbox),
        None => rejections.push(ws_error(
            call.id,
            "TooManyPendingCalls",
            "too many pending calls on this connection",
            None,
        )),
    };
    match WsClientMessage::from_json_str(text) {
        Ok(WsClientMessage::Call(call)) => admit(call),
        Ok(WsClientMessage::ReceivedResponse { id }) => {
            debug!(call_id = id, "client acknowledged response");
            state.ws_sync_cache().evict_id(scope, id);
        }
        Ok(WsClientMessage::FullSync { messages }) => {
            debug!(count = messages.len(), "replaying FULL_SYNC calls");
            for call in messages {
                admit(call);
            }
        }
        Err(err) => {
            warn!(error = %err, "rejected malformed WebSocket frame");
        }
    }
    rejections
}

/// Sends frames on the socket in order, stopping at the first write error.
async fn send_frames(socket: &mut WebSocket, frames: Vec<WsServerMessage>) -> Result<(), ()> {
    for frame in frames {
        let Ok(encoded) = serde_json::to_string(&frame) else {
            continue;
        };
        socket
            .send(Message::Text(encoded.into()))
            .await
            .map_err(|_| ())?;
    }
    Ok(())
}

fn spawn_ws_call(
    state: &AppState,
    scope: &SyncScope,
    limiter: &CallLimiter,
    pending: PendingGuard,
    call: WsCallPayload,
    outbox: &mpsc::Sender<WsServerMessage>,
) {
    let state = state.clone();
    let scope = scope.clone();
    let permits = Arc::clone(&limiter.permits);
    let outbox = outbox.clone();
    let cache = Arc::clone(state.ws_sync_cache());
    tokio::spawn(async move {
        let _pending = pending;
        // The semaphore is never closed, so acquiring only waits for a free slot.
        let Ok(_permit) = permits.acquire_owned().await else {
            return;
        };
        let reply = cache
            .execute(scope, call, |c| dispatch_call(state.clone(), c))
            .await;
        let _ = outbox.send(reply).await;
    });
}

async fn dispatch_call(state: AppState, call: WsCallPayload) -> WsServerMessage {
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

fn serialize_plugin_summaries(plugins: &[PluginSummary]) -> Value {
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
    match &state.inner.backend {
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
    match &state.inner.backend {
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
    let plugin_name = match parse_plugin_name_arg(&call.args) {
        Ok(name) => name,
        Err(msg) => return ws_error(call.id, "ValueError", msg, None),
    };
    let PluginBackend::Live(manager) = &state.inner.backend else {
        return ws_error(
            call.id,
            "PluginNotFoundError",
            format!("plugin `{plugin_name}` is not loaded"),
            None,
        );
    };
    match manager.reload_plugin(&plugin_name).await {
        Ok(()) => WsServerMessage::Reply {
            id: call.id,
            result: Value::Bool(true),
        },
        Err(err) => core_error_to_ws(call.id, err),
    }
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
    match manager.list_files(query).await {
        Ok(listing) => match serde_json::to_value(listing) {
            Ok(result) => WsServerMessage::Reply {
                id: call.id,
                result,
            },
            Err(err) => ws_error(call.id, "BackendError", err.to_string(), None),
        },
        Err(err) => core_error_to_ws(call.id, err),
    }
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
    match manager.install_plugin(req).await {
        Ok(outcome) => match serde_json::to_value(outcome) {
            Ok(result) => WsServerMessage::Reply {
                id: call.id,
                result,
            },
            Err(err) => ws_error(call.id, "BackendError", err.to_string(), None),
        },
        Err(err) => core_error_to_ws(call.id, err),
    }
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
    match manager.uninstall_plugin(&plugin_name, purge_data).await {
        Ok(()) => WsServerMessage::Reply {
            id: call.id,
            result: Value::Bool(true),
        },
        Err(err) => core_error_to_ws(call.id, err),
    }
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
    match manager.enable_plugin(&plugin_name).await {
        Ok(()) => WsServerMessage::Reply {
            id: call.id,
            result: Value::Bool(true),
        },
        Err(err) => core_error_to_ws(call.id, err),
    }
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
    match manager.disable_plugin(&plugin_name).await {
        Ok(()) => WsServerMessage::Reply {
            id: call.id,
            result: Value::Bool(true),
        },
        Err(err) => core_error_to_ws(call.id, err),
    }
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
    match manager.set_plugin_order(order).await {
        Ok(()) => WsServerMessage::Reply {
            id: call.id,
            result: Value::Bool(true),
        },
        Err(err) => core_error_to_ws(call.id, err),
    }
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
    match manager.set_setting(&key, value).await {
        Ok(()) => WsServerMessage::Reply {
            id: call.id,
            result: Value::Bool(true),
        },
        Err(err) => core_error_to_ws(call.id, err),
    }
}

fn opt_string_field(val: Option<&Value>, label: &str) -> Result<Option<String>, String> {
    match val {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("`{label}` must be a string")),
    }
}

fn opt_bool_field(val: Option<&Value>, default: bool, label: &str) -> Result<bool, String> {
    match val {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(format!("`{label}` must be a boolean")),
    }
}

fn opt_usize_field(val: Option<&Value>, default: usize, label: &str) -> Result<usize, String> {
    match val {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Number(n)) => n
            .as_u64()
            .and_then(|u| usize::try_from(u).ok())
            .ok_or_else(|| format!("`{label}` must be a non-negative integer")),
        Some(_) => Err(format!("`{label}` must be a non-negative integer")),
    }
}

fn opt_string_vec_field(val: Option<&Value>, label: &str) -> Result<Vec<String>, String> {
    match val {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| format!("`{label}` elements must be strings"))
            })
            .collect(),
        Some(_) => Err(format!("`{label}` must be an array of strings")),
    }
}

fn parse_filepicker_query(args: &[Value]) -> Result<FilePickerQuery, String> {
    if args.len() == 1
        && let Some(obj) = args[0].as_object()
    {
        return Ok(FilePickerQuery {
            path: opt_string_field(obj.get("path"), "path")?,
            include_files: opt_bool_field(obj.get("include_files"), true, "include_files")?,
            include_folders: opt_bool_field(obj.get("include_folders"), true, "include_folders")?,
            filter: opt_string_field(obj.get("filter"), "filter")?,
            extensions: opt_string_vec_field(obj.get("extensions"), "extensions")?,
            show_hidden: opt_bool_field(obj.get("show_hidden"), false, "show_hidden")?,
            max_entries: opt_usize_field(
                obj.get("max_entries"),
                DEFAULT_FILEPICKER_MAX_ENTRIES,
                "max_entries",
            )?,
        });
    }

    Ok(FilePickerQuery {
        path: opt_string_field(args.first(), "path")?,
        include_files: opt_bool_field(args.get(1), true, "include_files")?,
        filter: opt_string_field(args.get(2), "filter")?,
        extensions: opt_string_vec_field(args.get(3), "extensions")?,
        show_hidden: opt_bool_field(args.get(4), false, "show_hidden")?,
        max_entries: opt_usize_field(args.get(5), DEFAULT_FILEPICKER_MAX_ENTRIES, "max_entries")?,
        include_folders: opt_bool_field(args.get(6), true, "include_folders")?,
    })
}

fn parse_install_request(args: &[Value]) -> Result<InstallPluginRequest, String> {
    let (
        url_or_path,
        expected_name,
        expected_version,
        sha256,
        install_type,
        user_confirmed,
        dir_hint,
    ) = if args.len() == 1
        && let Some(obj) = args[0].as_object()
    {
        let url_val = obj
            .get("url_or_path")
            .or_else(|| obj.get("url"))
            .or_else(|| obj.get("artifact"))
            .or_else(|| obj.get("path"));
        let url_or_path = opt_string_field(url_val, "url_or_path")?
            .ok_or_else(|| "missing `url_or_path` argument".to_owned())?;
        let expected_name = opt_string_field(obj.get("name"), "name")?;
        let expected_version = opt_string_field(obj.get("version"), "version")?;
        let sha256 = opt_string_field(obj.get("sha256").or_else(|| obj.get("hash")), "sha256")?;
        let install_type = obj.get("install_type");
        let user_confirmed = opt_bool_field(obj.get("user_confirmed"), false, "user_confirmed")?;
        let dir_hint = opt_string_field(
            obj.get("directory").or_else(|| obj.get("directory_hint")),
            "directory",
        )?;
        (
            url_or_path,
            expected_name,
            expected_version,
            sha256,
            install_type,
            user_confirmed,
            dir_hint,
        )
    } else {
        let url_or_path = opt_string_field(args.first(), "url_or_path")?
            .ok_or_else(|| "missing `url_or_path` argument".to_owned())?;
        let expected_name = opt_string_field(args.get(1), "name")?;
        let expected_version = opt_string_field(args.get(2), "version")?;
        let sha256 = opt_string_field(args.get(3), "sha256")?;
        let install_type = args.get(4);
        let user_confirmed = opt_bool_field(args.get(5), false, "user_confirmed")?;
        let dir_hint = opt_string_field(args.get(6), "directory")?;
        (
            url_or_path,
            expected_name,
            expected_version,
            sha256,
            install_type,
            user_confirmed,
            dir_hint,
        )
    };

    let source = parse_install_source(&url_or_path, sha256, install_type, user_confirmed)
        .map_err(|err| err.to_string())?;
    Ok(InstallPluginRequest {
        source,
        directory_hint: dir_hint,
        expected_name,
        expected_version,
    })
}

fn parse_plugin_name_arg(args: &[Value]) -> Result<String, String> {
    if args.len() == 1
        && let Some(obj) = args[0].as_object()
    {
        let raw = obj.get("plugin_name").or_else(|| obj.get("name"));
        return opt_string_field(raw, "plugin_name")?
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "missing plugin_name argument".to_owned());
    }
    opt_string_field(args.first(), "plugin_name")?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "missing plugin_name argument".to_owned())
}

fn parse_uninstall_args(args: &[Value]) -> Result<(String, bool), String> {
    if args.len() == 1
        && let Some(obj) = args[0].as_object()
    {
        let name = opt_string_field(
            obj.get("plugin_name").or_else(|| obj.get("name")),
            "plugin_name",
        )?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "missing plugin_name argument".to_owned())?;
        let purge = opt_bool_field(obj.get("purge_data"), false, "purge_data")?;
        return Ok((name, purge));
    }
    let name = opt_string_field(args.first(), "plugin_name")?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "missing plugin_name argument".to_owned())?;
    let purge = opt_bool_field(args.get(1), false, "purge_data")?;
    Ok((name, purge))
}

fn parse_plugin_order_args(args: &[Value]) -> Result<Vec<String>, String> {
    if args.len() == 1
        && let Some(obj) = args[0].as_object()
    {
        let val = obj
            .get("plugin_order")
            .or_else(|| obj.get("pluginOrder"))
            .or_else(|| obj.get("order"));
        let Some(v) = val else {
            return Err("missing plugin_order argument".to_owned());
        };
        return opt_string_vec_field(Some(v), "plugin_order");
    }
    let Some(first) = args.first() else {
        return Err("missing plugin_order argument".to_owned());
    };
    if first.is_null() {
        return Err("`plugin_order` must be an array of strings".to_owned());
    }
    opt_string_vec_field(Some(first), "plugin_order")
}

fn parse_get_settings_key(args: &[Value]) -> Result<Option<String>, String> {
    if args.len() == 1
        && let Some(obj) = args[0].as_object()
    {
        return opt_string_field(obj.get("key"), "key");
    }
    opt_string_field(args.first(), "key")
}

fn parse_set_setting_args(args: &[Value]) -> Result<(String, Value), String> {
    if args.len() == 1
        && let Some(obj) = args[0].as_object()
    {
        let key = opt_string_field(obj.get("key"), "key")?
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "missing setting key".to_owned())?;
        let val = obj
            .get("value")
            .cloned()
            .ok_or_else(|| "missing setting value".to_owned())?;
        return Ok((key, val));
    }
    let key = opt_string_field(args.first(), "key")?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "missing setting key".to_owned())?;
    let val = args
        .get(1)
        .cloned()
        .ok_or_else(|| "missing setting value".to_owned())?;
    Ok((key, val))
}

pub(crate) fn core_error_to_ws(id: u64, err: CoreError) -> WsServerMessage {
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
        CoreError::PluginBlocked { .. } => {
            ws_error(id, "PluginBlockedError", err.to_string(), None)
        }
        CoreError::DuplicatePluginName { .. } => {
            ws_error(id, "DuplicatePluginNameError", err.to_string(), None)
        }
        CoreError::Host(HostError::InvalidMethodName(method)) => ws_error(
            id,
            "PermissionError",
            format!("invalid method `{method}`"),
            None,
        ),
        e if e.is_path_escape() => ws_error(id, "PathEscapeError", e.to_string(), None),
        CoreError::MissingSha256 => ws_error(id, "MissingSha256Error", err.to_string(), None),
        CoreError::Sha256Mismatch { .. } => {
            ws_error(id, "Sha256MismatchError", err.to_string(), None)
        }
        CoreError::ConfirmationRequired => {
            ws_error(id, "ConfirmationRequiredError", err.to_string(), None)
        }
        CoreError::Zip(_)
        | CoreError::UnsafeArchiveEntry { .. }
        | CoreError::ArchiveSizeExceeded { .. }
        | CoreError::ArchiveTooManyEntries { .. }
        | CoreError::MissingPluginManifest
        | CoreError::ManifestMismatch { .. }
        | CoreError::ManifestParse { .. } => ws_error(id, "ArchiveError", err.to_string(), None),
        CoreError::HttpPolicy(_) => ws_error(id, "HttpPolicyError", err.to_string(), None),
        CoreError::InvalidSetting(_) | CoreError::InvalidInstallSource(_) => {
            ws_error(id, "ValueError", err.to_string(), None)
        }
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
