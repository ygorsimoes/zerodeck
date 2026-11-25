use std::net::IpAddr;

use axum::extract::State;
use axum::http::{Method, Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use tracing::warn;

use crate::AppState;

const ALLOWED_PUBLIC_PREFIXES: &[&str] = &[
    "/frontend/",
    "/static/",
    "/locales/",
    "/steam_resource/",
    "/plugins/load_main/",
];

const ALLOWED_EXACT_PATHS: &[&str] = &["/auth/token", "/healthz"];

/// Paths whose clients cannot set custom headers (WebSocket upgrades, `fetch` proxying from the
/// Steam UI), so they may authenticate with `?auth=<token>` instead of `X-Decky-Auth`.
const QUERY_AUTH_PATHS: &[&str] = &["/ws", "/fetch"];

/// Returns `true` for a `..` path segment, including percent-encoded forms such as `%2e%2e`.
fn is_parent_segment(segment: &str) -> bool {
    segment.to_ascii_lowercase().replace("%2e", ".") == ".."
}

/// Returns `true` when the raw URI path contains traversal (`..` / `%2e%2e`), backslashes
/// (`\` / `%5c`), or encoded null bytes (`%00`).
fn has_unsafe_path_tokens(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.contains('\\')
        || lower.contains("%5c")
        || lower.contains("%00")
        || path.split('/').any(is_parent_segment)
}

/// Returns `true` when the request path does not require the `X-Decky-Auth` header.
fn is_csrf_exempt(method: &Method, path: &str) -> bool {
    if has_unsafe_path_tokens(path) {
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
    if segments.len() == 3
        && segments[0] == "plugins"
        && !segments[1].is_empty()
        && segments[2] == "frontend_bundle"
    {
        return true;
    }

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
    if has_unsafe_path_tokens(request.uri().path()) {
        return (StatusCode::FORBIDDEN, "Forbidden").into_response();
    }

    if request.method() == Method::OPTIONS || is_csrf_exempt(request.method(), request.uri().path())
    {
        return next.run(request).await;
    }

    let header_ok = request
        .headers()
        .get("X-Decky-Auth")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|provided| tokens_match(provided, state.csrf_token()));
    let query_ok = QUERY_AUTH_PATHS.contains(&request.uri().path())
        && query_auth_token(request.uri().query())
            .is_some_and(|provided| tokens_match(provided, state.csrf_token()));

    if header_ok || query_ok {
        next.run(request).await
    } else {
        (StatusCode::FORBIDDEN, "Forbidden").into_response()
    }
}

/// Returns `true` when an HTTP `Host` value names a loopback address (`localhost`, `127.0.0.0/8`,
/// `[::1]`), with or without a port.
fn is_loopback_host(value: &str) -> bool {
    let hostname = if let Some(rest) = value.strip_prefix('[') {
        // Bracketed IPv6 literal: `[::1]` or `[::1]:1337`.
        match rest.split_once(']') {
            Some((inner, tail)) if tail.is_empty() || tail.starts_with(':') => inner,
            _ => return false,
        }
    } else {
        value.rsplit_once(':').map_or(value, |(host, _port)| host)
    };

    hostname.eq_ignore_ascii_case("localhost")
        || hostname.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Rejects requests whose `Host` is not a loopback name, blocking DNS-rebinding attacks in which a
/// remote page re-points its own hostname at `127.0.0.1` to read `/auth/token` same-origin.
///
/// Requests without any host information (HTTP/1.0 clients, in-process calls) pass through, since
/// browsers always send `Host` and a local non-browser client can already reach the loopback port.
pub async fn host_guard(request: Request<axum::body::Body>, next: Next) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .map(|value| value.to_str().unwrap_or_default().to_owned())
        .or_else(|| request.uri().authority().map(ToString::to_string));

    match host {
        Some(host) if !is_loopback_host(&host) => {
            warn!(host = %host, "rejected request with non-loopback Host header");
            (StatusCode::FORBIDDEN, "Forbidden").into_response()
        }
        _ => next.run(request).await,
    }
}

/// Extracts the raw `auth` value from a query string.
fn query_auth_token(query: Option<&str>) -> Option<&str> {
    query?
        .split('&')
        .find_map(|pair| pair.strip_prefix("auth="))
}

/// Compares tokens in time independent of where they first differ, so response timing does not
/// leak how much of a guessed token was correct. Only the length comparison returns early.
pub(crate) fn tokens_match(provided: &str, expected: &str) -> bool {
    let (provided, expected) = (provided.as_bytes(), expected.as_bytes());
    if provided.len() != expected.len() {
        return false;
    }
    provided
        .iter()
        .zip(expected)
        .fold(0u8, |diff, (a, b)| diff | (a ^ b))
        == 0
}
