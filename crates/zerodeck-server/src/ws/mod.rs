//! WebSocket endpoint: upgrade, connection loop, call dispatch, and argument parsing.

pub(crate) mod args;
pub(crate) mod dispatch;
pub(crate) mod errors;
pub(crate) mod socket;

use std::collections::HashMap;

use axum::extract::{Query, State, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::state::AppState;
use crate::ws::socket::handle_ws_socket;
use crate::ws_sync::parse_session_param;

pub(crate) async fn handle_ws_upgrade(
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
