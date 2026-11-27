//! HTTP router assembly: CORS, auth middleware, tracing, and the plain routes.

pub(crate) mod legacy;
pub(crate) mod static_files;

use std::future::Future;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderValue, Method, Request, Uri, header};
use axum::middleware;
use axum::response::Json;
use axum::routing::{any, get, post};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing::{Span, debug_span};

use crate::ServerError;
use crate::auth::{csrf_middleware, host_guard};
use crate::routes::legacy::handle_legacy_method;
use crate::routes::static_files::handle_frontend_file;
use crate::routes::static_files::handle_locales_file;
use crate::routes::static_files::handle_plugin_assets_file;
use crate::routes::static_files::handle_plugin_data_file;
use crate::routes::static_files::handle_plugin_dist_file;
use crate::routes::static_files::handle_plugin_frontend_bundle;
use crate::routes::static_files::handle_static_file;
use crate::state::AppState;
use crate::ws::handle_ws_upgrade;

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
        .route("/readyz", get(handle_readyz))
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

async fn handle_readyz(State(state): State<AppState>) -> Json<Value> {
    let config = state.config();
    Json(json!({
        "version": &config.version,
        "build_id": &config.build_id,
        "python_host_ready": config.python_host_ready,
        "invocation_id": &config.invocation_id,
    }))
}

async fn handle_auth_token(State(state): State<AppState>) -> String {
    state.csrf_token().to_owned()
}
