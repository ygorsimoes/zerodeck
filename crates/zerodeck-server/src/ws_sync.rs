use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::watch;
use zerodeck_protocol::{WsCallPayload, WsServerMessage};

/// Composite deduplication key `(call.id, call.route, canonical_args_json)`.
pub type WsSyncKey = (u64, String, String);

/// Bounded, TTL-expiring cache deduplicating in-flight and completed unacknowledged WS RPC calls
/// across `CALL` and `FULL_SYNC` replays.
#[derive(Debug)]
pub struct WsSyncCache {
    capacity: usize,
    ttl: Duration,
    inner: Mutex<WsSyncState>,
}

#[derive(Debug)]
struct WsSyncState {
    next_seq: u64,
    entries: HashMap<WsSyncKey, CacheEntry>,
}

#[derive(Debug)]
enum CacheEntry {
    InFlight {
        tx: watch::Sender<Option<WsServerMessage>>,
    },
    Completed {
        frame: WsServerMessage,
        stored_at: Instant,
        seq: u64,
    },
}

enum BeginOutcome<'a> {
    Completed(WsServerMessage),
    Wait(watch::Receiver<Option<WsServerMessage>>),
    Leader(LeaderGuard<'a>),
}

struct LeaderGuard<'a> {
    cache: &'a WsSyncCache,
    key: WsSyncKey,
    tx: watch::Sender<Option<WsServerMessage>>,
    finished: bool,
}

impl LeaderGuard<'_> {
    fn finish(mut self, frame: WsServerMessage) {
        self.finished = true;
        self.cache.complete(&self.key, &self.tx, frame);
    }
}

impl Drop for LeaderGuard<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.cache.abort_in_flight(&self.key);
        }
    }
}

impl WsSyncCache {
    /// Creates a new [`WsSyncCache`] with the given capacity and completed-entry TTL.
    #[must_use]
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            capacity,
            ttl,
            inner: Mutex::new(WsSyncState {
                next_seq: 0,
                entries: HashMap::new(),
            }),
        }
    }

    /// Executes `call` once for its `(id, route, canonical_args)` key, joining an active
    /// in-flight run or returning a cached unacknowledged frame when present.
    pub async fn execute<F, Fut>(self: Arc<Self>, call: WsCallPayload, run: F) -> WsServerMessage
    where
        F: FnOnce(WsCallPayload) -> Fut,
        Fut: Future<Output = WsServerMessage>,
    {
        let call_id = call.id;
        let key = canonical_key(&call);

        match self.begin(key) {
            BeginOutcome::Completed(frame) => frame,
            BeginOutcome::Wait(mut rx) => {
                if let Some(frame) = rx.borrow().clone() {
                    return frame;
                }
                while rx.changed().await.is_ok() {
                    if let Some(frame) = rx.borrow().clone() {
                        return frame;
                    }
                }
                WsServerMessage::Discard { id: call_id }
            }
            BeginOutcome::Leader(guard) => {
                let frame = run(call).await;
                guard.finish(frame.clone());
                frame
            }
        }
    }

    /// Evicts completed cached responses matching `id` when the client sends `RECEIVED_RESPONSE`.
    pub fn evict_id(&self, id: u64) {
        let mut state = lock_unpoisoned(&self.inner);
        state
            .entries
            .retain(|k, entry| !(k.0 == id && matches!(entry, CacheEntry::Completed { .. })));
    }

    fn begin(&self, key: WsSyncKey) -> BeginOutcome<'_> {
        let mut state = lock_unpoisoned(&self.inner);
        self.prune_expired(&mut state, Instant::now());

        if let Some(entry) = state.entries.get(&key) {
            match entry {
                CacheEntry::Completed { frame, .. } => {
                    return BeginOutcome::Completed(frame.clone());
                }
                CacheEntry::InFlight { tx } => {
                    return BeginOutcome::Wait(tx.subscribe());
                }
            }
        }

        let (tx, _rx) = watch::channel(None);
        state
            .entries
            .insert(key.clone(), CacheEntry::InFlight { tx: tx.clone() });
        BeginOutcome::Leader(LeaderGuard {
            cache: self,
            key,
            tx,
            finished: false,
        })
    }

    fn complete(
        &self,
        key: &WsSyncKey,
        tx: &watch::Sender<Option<WsServerMessage>>,
        frame: WsServerMessage,
    ) {
        {
            let mut state = lock_unpoisoned(&self.inner);
            let now = Instant::now();
            self.prune_expired(&mut state, now);

            if self.capacity == 0 {
                if matches!(state.entries.get(key), Some(CacheEntry::InFlight { .. })) {
                    state.entries.remove(key);
                }
            } else {
                let seq = state.next_seq;
                state.next_seq = state.next_seq.saturating_add(1);
                state.entries.insert(
                    key.clone(),
                    CacheEntry::Completed {
                        frame: frame.clone(),
                        stored_at: now,
                        seq,
                    },
                );
                self.enforce_capacity(&mut state);
            }
        }

        let _ = tx.send(Some(frame));
    }

    fn abort_in_flight(&self, key: &WsSyncKey) {
        let mut state = lock_unpoisoned(&self.inner);
        if matches!(state.entries.get(key), Some(CacheEntry::InFlight { .. })) {
            state.entries.remove(key);
        }
    }

    fn prune_expired(&self, state: &mut WsSyncState, now: Instant) {
        state.entries.retain(|_, entry| match entry {
            CacheEntry::InFlight { .. } => true,
            CacheEntry::Completed { stored_at, .. } => {
                now.saturating_duration_since(*stored_at) < self.ttl
            }
        });
    }

    fn enforce_capacity(&self, state: &mut WsSyncState) {
        loop {
            let completed_count = state
                .entries
                .values()
                .filter(|e| matches!(e, CacheEntry::Completed { .. }))
                .count();
            if completed_count <= self.capacity {
                break;
            }
            let oldest_key = state
                .entries
                .iter()
                .filter_map(|(k, entry)| match entry {
                    CacheEntry::Completed { seq, .. } => Some((*seq, k)),
                    CacheEntry::InFlight { .. } => None,
                })
                .min_by_key(|(seq, _)| *seq)
                .map(|(_, k)| k.clone());
            let Some(victim) = oldest_key else {
                break;
            };
            state.entries.remove(&victim);
        }
    }
}

fn canonical_key(call: &WsCallPayload) -> WsSyncKey {
    let args_json = serde_json::to_string(&call.args).unwrap_or_default();
    (call.id, call.route.clone(), args_json)
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
