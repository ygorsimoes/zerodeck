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
fn is_csrf_exempt(path: &str) -> bool {
    if ALLOWED_EXACT_PATHS.contains(&path) {
        return true;
    }
    if ALLOWED_PUBLIC_PREFIXES
        .iter()
        .any(|prefix| path.starts_with(prefix))
    {
        return true;
    }
    if let Some(rest) = path.strip_prefix("/plugins/") {
        return rest.contains("/assets/") || rest.contains("/data/") || rest.contains("/dist/");
    }
    false
}

/// Enforces `X-Decky-Auth` CSRF token verification on protected HTTP endpoints.
pub async fn csrf_middleware(
    State(state): State<AppState>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    if request.method() == Method::OPTIONS || is_csrf_exempt(request.uri().path()) {
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
