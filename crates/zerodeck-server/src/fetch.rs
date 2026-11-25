use std::collections::{HashMap, HashSet};

use axum::body::{Body, to_bytes};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use reqwest::header::{HeaderMap as ReqwestHeaderMap, HeaderName, HeaderValue};
use url::Url;
use zerodeck_core::{CoreError, DEFAULT_MAX_REDIRECTS, send_checked_request};

use crate::AppState;

const MAX_FETCH_REQUEST_BODY_BYTES: usize = 16 * 1024 * 1024;
const DECKY_HEADER_PREFIX: &str = "x-decky-header-";
const EXCLUDED_HEADERS_KEY: &str = "x-decky-fetch-excluded-headers";

const DEFAULT_STRIPPED_REQUEST_HEADERS: &[&str] = &[
    "host",
    "x-decky-auth",
    "x-decky-fetch-excluded-headers",
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
    "content-length",
    "accept-encoding",
    "cookie",
];

const HOP_BY_HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
];

/// Streaming CORS proxy handler for `ANY /fetch?fetch_url=<url>`.
pub async fn handle_fetch(
    State(state): State<AppState>,
    method: Method,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let Some(raw_url) = query
        .get("fetch_url")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    else {
        return (StatusCode::BAD_REQUEST, "missing fetch_url query parameter").into_response();
    };

    if !is_valid_http_url(raw_url) {
        return (StatusCode::BAD_REQUEST, "invalid fetch_url").into_response();
    }

    let Ok(req_method) = reqwest::Method::from_bytes(method.as_str().as_bytes()) else {
        return (StatusCode::BAD_REQUEST, "unsupported HTTP method").into_response();
    };

    let out_headers = build_upstream_headers(&headers);
    let Ok(body_bytes) = to_bytes(body, MAX_FETCH_REQUEST_BODY_BYTES).await else {
        return (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response();
    };
    let req_body = if body_bytes.is_empty() {
        None
    } else {
        Some(reqwest::Body::from(body_bytes.to_vec()))
    };

    match send_checked_request(
        state.http_client(),
        req_method,
        raw_url,
        out_headers,
        req_body,
        DEFAULT_MAX_REDIRECTS,
    )
    .await
    {
        Ok(upstream) => stream_upstream_response(upstream),
        Err(CoreError::HttpPolicy(msg)) => (StatusCode::FORBIDDEN, msg).into_response(),
        Err(CoreError::HttpRequest(msg)) => {
            let lower = msg.to_ascii_lowercase();
            let status = if lower.contains("timed out") || lower.contains("timeout") {
                StatusCode::GATEWAY_TIMEOUT
            } else {
                StatusCode::BAD_GATEWAY
            };
            (status, msg).into_response()
        }
        Err(err) => (StatusCode::BAD_GATEWAY, err.to_string()).into_response(),
    }
}

fn is_valid_http_url(raw: &str) -> bool {
    let Ok(parsed) = Url::parse(raw) else {
        return false;
    };
    matches!(parsed.scheme(), "http" | "https")
        && parsed.host().is_some()
        && parsed.port_or_known_default().is_some_and(|p| p != 0)
}

fn connection_token_set(headers: &HeaderMap) -> HashSet<String> {
    let mut set = HashSet::new();
    for val in headers.get_all(axum::http::header::CONNECTION) {
        if let Ok(s) = val.to_str() {
            for token in s.split(',').map(|t| t.trim().to_ascii_lowercase()) {
                if !token.is_empty() {
                    set.insert(token);
                }
            }
        }
    }
    set
}

fn build_upstream_headers(incoming: &HeaderMap) -> ReqwestHeaderMap {
    let mut excluded: HashSet<String> = DEFAULT_STRIPPED_REQUEST_HEADERS
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    excluded.extend(connection_token_set(incoming));

    for val in incoming.get_all(EXCLUDED_HEADERS_KEY) {
        if let Ok(s) = val.to_str() {
            for item in s.split(',').map(|t| t.trim().to_ascii_lowercase()) {
                if !item.is_empty() {
                    excluded.insert(item);
                }
            }
        }
    }

    let mut out = ReqwestHeaderMap::new();
    let mut translated: Vec<(HeaderName, HeaderValue)> = Vec::new();

    for (name, val) in incoming {
        let lower = name.as_str().to_ascii_lowercase();
        if let Some(target) = lower.strip_prefix(DECKY_HEADER_PREFIX) {
            if !target.is_empty()
                && !is_blocked_translated_header(target)
                && let (Ok(hname), Ok(hval)) = (
                    HeaderName::from_bytes(target.as_bytes()),
                    HeaderValue::from_bytes(val.as_bytes()),
                )
            {
                translated.push((hname, hval));
            }
            continue;
        }

        if excluded.contains(&lower) {
            continue;
        }
        if let (Ok(hname), Ok(hval)) = (
            HeaderName::from_bytes(name.as_str().as_bytes()),
            HeaderValue::from_bytes(val.as_bytes()),
        ) {
            out.append(hname, hval);
        }
    }

    for (hname, _) in &translated {
        out.remove(hname);
    }
    for (hname, hval) in translated {
        out.append(hname, hval);
    }

    out
}

fn is_blocked_translated_header(lower_name: &str) -> bool {
    matches!(
        lower_name,
        "host" | "connection" | "content-length" | "transfer-encoding" | "accept-encoding"
    )
}

fn stream_upstream_response(upstream: reqwest::Response) -> Response {
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let conn_tokens = connection_token_set_reqwest(upstream.headers());

    let mut builder = Response::builder().status(status);
    if let Some(out_headers) = builder.headers_mut() {
        for (name, val) in upstream.headers() {
            let lower = name.as_str();
            if HOP_BY_HOP_HEADERS.contains(&lower)
                || conn_tokens.contains(lower)
                || lower.starts_with("access-control-")
            {
                continue;
            }
            if let (Ok(hname), Ok(hval)) = (
                axum::http::HeaderName::from_bytes(name.as_str().as_bytes()),
                axum::http::HeaderValue::from_bytes(val.as_bytes()),
            ) {
                out_headers.append(hname, hval);
            }
        }
    }

    builder
        .body(Body::from_stream(upstream.bytes_stream()))
        .unwrap_or_else(|err| (StatusCode::BAD_GATEWAY, err.to_string()).into_response())
}

fn connection_token_set_reqwest(headers: &ReqwestHeaderMap) -> HashSet<String> {
    let mut set = HashSet::new();
    for val in headers.get_all(reqwest::header::CONNECTION) {
        if let Ok(s) = val.to_str() {
            for token in s.split(',').map(|t| t.trim().to_ascii_lowercase()) {
                if !token.is_empty() {
                    set.insert(token);
                }
            }
        }
    }
    set
}
