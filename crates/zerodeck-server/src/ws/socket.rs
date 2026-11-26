//! Per-connection WebSocket loop, call admission limits, and event fan-out.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket};
use serde_json::json;
use tokio::sync::{Semaphore, broadcast, mpsc};
use tokio::time::MissedTickBehavior;
use tracing::{debug, warn};
use zerodeck_core::LoaderEvent;
use zerodeck_plugin_host::PluginEvent;
use zerodeck_protocol::{WsCallPayload, WsClientMessage, WsServerMessage};

use crate::state::AppState;
use crate::ws::dispatch::dispatch_call;
use crate::ws::dispatch::serialize_plugin_summaries;
use crate::ws::errors::ws_error;
use crate::ws_sync::SyncScope;

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
pub(crate) async fn handle_ws_socket(state: AppState, mut socket: WebSocket, scope: SyncScope) {
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
