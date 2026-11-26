//! Static loader assets and plugin-scoped files behind the path jail.

use std::error::Error as StdError;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use percent_encoding::percent_decode_str;
use tokio_util::io::ReaderStream;
use zerodeck_core::{CoreError, PluginFileScope, resolve_static_file};
use zerodeck_plugin_host::HostError;

use crate::state::AppState;
use crate::state::PluginBackend;

pub(crate) async fn handle_frontend_file(
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

pub(crate) async fn handle_static_file(
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

pub(crate) async fn handle_locales_file(
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

pub(crate) async fn handle_plugin_frontend_bundle(
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

pub(crate) async fn handle_plugin_dist_file(
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

pub(crate) async fn handle_plugin_assets_file(
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

pub(crate) async fn handle_plugin_data_file(
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
    let PluginBackend::Live(manager) = state.backend() else {
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
