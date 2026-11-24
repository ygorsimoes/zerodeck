use axum::extract::State;
use axum::http::{Method, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::AppState;

const ALLOWED_PUBLIC_PREFIXES: &[&str] = &[
    "/frontend/",
    "/static/",
    "/locales/",
    "/steam_resource/",
    "/plugins/load_main/",
];

const ALLOWED_EXACT_PATHS: &[&str] = &["/auth/token", "/healthz", "/ws", "/fetch"];

/// Returns `true` when the request path does not require the `X-Decky-Auth` header.
fn is_csrf_exempt(method: &Method, path: &str) -> bool {
    if path.split('/').any(|seg| seg == "..") {
        return false;
    }

    if ALLOWED_EXACT_PATHS.contains(&path) {
        return true;
    }

    if !matches!(*method, Method::GET | Method::HEAD) {
        return false;
    }

    if ALLOWED_PUBLIC_PREFIXES
        .iter()
        .any(|prefix| path.starts_with(prefix))
    {
        return true;
    }

    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    segments.len() >= 4
        && segments[0] == "plugins"
        && !segments[1].is_empty()
        && matches!(segments[2], "assets" | "data" | "dist")
}

/// Enforces `X-Decky-Auth` CSRF token verification on protected HTTP endpoints.
pub async fn csrf_middleware(
    State(state): State<AppState>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    if request.method() == Method::OPTIONS || is_csrf_exempt(request.method(), request.uri().path())
    {
        return next.run(request).await;
    }

    let provided = request
        .headers()
        .get("X-Decky-Auth")
        .and_then(|value| value.to_str().ok());

    if provided == Some(state.csrf_token()) {
        next.run(request).await
    } else {
        (StatusCode::FORBIDDEN, "Forbidden").into_response()
    }
}
