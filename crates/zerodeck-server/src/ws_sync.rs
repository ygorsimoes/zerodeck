use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::watch;
use zerodeck_protocol::{WsCallPayload, WsServerMessage};

/// Namespace for cached WS calls. The frontend restarts call ids at 1 on every page load, so each
/// client session gets its own namespace; clients that send no session share [`Legacy`].
///
/// [`Legacy`]: SyncScope::Legacy
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SyncScope {
    /// Shared namespace for clients that connect without a `session` query parameter.
    Legacy,
    /// Namespace private to one client session.
    Session(String),
}

/// Composite deduplication key `(scope, call.id, call.route, canonical_args_json)`.
pub type WsSyncKey = (SyncScope, u64, String, String);

const MAX_SESSION_LEN: usize = 64;

/// Parses the optional `session` query parameter: absent selects [`SyncScope::Legacy`]; present
/// must be 1-64 characters of `[A-Za-z0-9-]`.
///
/// # Errors
/// Returns `Err(())` when the value is empty, too long, or contains other characters.
#[allow(clippy::result_unit_err)]
pub fn parse_session_param(raw: Option<&str>) -> Result<SyncScope, ()> {
    let Some(raw) = raw else {
        return Ok(SyncScope::Legacy);
    };
    if raw.is_empty()
        || raw.len() > MAX_SESSION_LEN
        || !raw.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(());
    }
    Ok(SyncScope::Session(raw.to_owned()))
}

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

    /// Executes `call` once for its `(scope, id, route, canonical_args)` key, joining an active
    /// in-flight run or returning a cached unacknowledged frame when present.
    pub async fn execute<F, Fut>(
        self: Arc<Self>,
        scope: SyncScope,
        call: WsCallPayload,
        run: F,
    ) -> WsServerMessage
    where
        F: FnOnce(WsCallPayload) -> Fut,
        Fut: Future<Output = WsServerMessage>,
    {
        let call_id = call.id;
        let key = canonical_key(scope, &call);

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

    /// Evicts completed cached responses matching `scope` and `id` when the client sends
    /// `RECEIVED_RESPONSE`.
    pub fn evict_id(&self, scope: &SyncScope, id: u64) {
        let mut state = lock_unpoisoned(&self.inner);
        state.entries.retain(|k, entry| {
            !(&k.0 == scope && k.1 == id && matches!(entry, CacheEntry::Completed { .. }))
        });
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

fn canonical_key(scope: SyncScope, call: &WsCallPayload) -> WsSyncKey {
    let args_json = serde_json::to_string(&call.args).unwrap_or_default();
    (scope, call.id, call.route.clone(), args_json)
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use serde_json::json;
    use tokio::sync::Notify;
    use zerodeck_protocol::{WsCallPayload, WsServerMessage};

    use super::{SyncScope, WsSyncCache, parse_session_param};

    fn call_payload(id: u64) -> WsCallPayload {
        WsCallPayload {
            id,
            route: "utilities/ping".to_owned(),
            args: vec![json!(id)],
        }
    }

    fn reply_frame(id: u64, tag: &str) -> WsServerMessage {
        WsServerMessage::Reply {
            id,
            result: json!(tag),
        }
    }

    #[tokio::test]
    async fn capacity_eviction_drops_oldest_completed_and_preserves_in_flight() {
        let cache = Arc::new(WsSyncCache::new(2, Duration::from_secs(60)));
        let leader_started = Arc::new(Notify::new());
        let leader_release = Arc::new(Notify::new());
        let leader_runs = Arc::new(AtomicUsize::new(0));

        let leader_handle = {
            let cache = Arc::clone(&cache);
            let started = Arc::clone(&leader_started);
            let release = Arc::clone(&leader_release);
            let runs = Arc::clone(&leader_runs);
            tokio::spawn(async move {
                cache
                    .execute(SyncScope::Legacy, call_payload(100), move |c| async move {
                        runs.fetch_add(1, Ordering::SeqCst);
                        started.notify_one();
                        release.notified().await;
                        reply_frame(c.id, "in-flight-ok")
                    })
                    .await
            })
        };

        leader_started.notified().await;

        let waiter_runs = Arc::new(AtomicUsize::new(0));
        let waiter_handle = {
            let cache = Arc::clone(&cache);
            let runs = Arc::clone(&waiter_runs);
            tokio::spawn(async move {
                cache
                    .execute(SyncScope::Legacy, call_payload(100), move |c| async move {
                        runs.fetch_add(1, Ordering::SeqCst);
                        reply_frame(c.id, "unexpected-waiter-run")
                    })
                    .await
            })
        };
        tokio::task::yield_now().await;

        // Insert capacity + 1 (3) completed entries while call 100 remains InFlight.
        let completed_runs = Arc::new(AtomicUsize::new(0));
        for id in 1..=3 {
            let runs = Arc::clone(&completed_runs);
            let frame = Arc::clone(&cache)
                .execute(SyncScope::Legacy, call_payload(id), move |c| async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    reply_frame(c.id, "first")
                })
                .await;
            assert_eq!(frame, reply_frame(id, "first"));
        }
        assert_eq!(completed_runs.load(Ordering::SeqCst), 3);

        // Entries 2 and 3 remain cached; entry 1 (oldest seq) was evicted.
        for id in [2, 3] {
            let runs = Arc::clone(&completed_runs);
            let frame = Arc::clone(&cache)
                .execute(SyncScope::Legacy, call_payload(id), move |c| async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    reply_frame(c.id, "re-executed")
                })
                .await;
            assert_eq!(frame, reply_frame(id, "first"));
        }
        assert_eq!(completed_runs.load(Ordering::SeqCst), 3);

        let re_run = Arc::clone(&cache)
            .execute(SyncScope::Legacy, call_payload(1), {
                let runs = Arc::clone(&completed_runs);
                move |c| async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    reply_frame(c.id, "re-executed")
                }
            })
            .await;
        assert_eq!(re_run, reply_frame(1, "re-executed"));
        assert_eq!(completed_runs.load(Ordering::SeqCst), 4);

        // InFlight entry 100 was never evicted; releasing it wakes both leader and waiter.
        leader_release.notify_one();
        let leader_frame = leader_handle.await.expect("leader join");
        let waiter_frame = waiter_handle.await.expect("waiter join");
        assert_eq!(leader_frame, reply_frame(100, "in-flight-ok"));
        assert_eq!(waiter_frame, reply_frame(100, "in-flight-ok"));
        assert_eq!(leader_runs.load(Ordering::SeqCst), 1);
        assert_eq!(waiter_runs.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn zero_capacity_drops_completed_entries_while_notifying_in_flight_waiters() {
        let cache = Arc::new(WsSyncCache::new(0, Duration::from_secs(60)));
        let leader_started = Arc::new(Notify::new());
        let leader_release = Arc::new(Notify::new());
        let runs = Arc::new(AtomicUsize::new(0));

        let leader_handle = {
            let cache = Arc::clone(&cache);
            let started = Arc::clone(&leader_started);
            let release = Arc::clone(&leader_release);
            let runs = Arc::clone(&runs);
            tokio::spawn(async move {
                cache
                    .execute(SyncScope::Legacy, call_payload(7), move |c| async move {
                        runs.fetch_add(1, Ordering::SeqCst);
                        started.notify_one();
                        release.notified().await;
                        reply_frame(c.id, "run-1")
                    })
                    .await
            })
        };

        leader_started.notified().await;

        let waiter_handle = {
            let cache = Arc::clone(&cache);
            let runs = Arc::clone(&runs);
            tokio::spawn(async move {
                cache
                    .execute(SyncScope::Legacy, call_payload(7), move |c| async move {
                        runs.fetch_add(1, Ordering::SeqCst);
                        reply_frame(c.id, "unexpected")
                    })
                    .await
            })
        };
        tokio::task::yield_now().await;

        leader_release.notify_one();
        assert_eq!(
            leader_handle.await.expect("leader"),
            reply_frame(7, "run-1")
        );
        assert_eq!(
            waiter_handle.await.expect("waiter"),
            reply_frame(7, "run-1")
        );
        assert_eq!(runs.load(Ordering::SeqCst), 1);

        // Completed entry must not be retained when capacity == 0.
        let second = Arc::clone(&cache)
            .execute(SyncScope::Legacy, call_payload(7), {
                let runs = Arc::clone(&runs);
                move |c| async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    reply_frame(c.id, "run-2")
                }
            })
            .await;
        assert_eq!(second, reply_frame(7, "run-2"));
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn expired_entries_are_pruned_and_re_executed_after_ttl() {
        let cache = Arc::new(WsSyncCache::new(8, Duration::from_millis(80)));
        let runs = Arc::new(AtomicUsize::new(0));

        let first = Arc::clone(&cache)
            .execute(SyncScope::Legacy, call_payload(42), {
                let runs = Arc::clone(&runs);
                move |c| async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    reply_frame(c.id, "fresh-1")
                }
            })
            .await;
        assert_eq!(first, reply_frame(42, "fresh-1"));
        assert_eq!(runs.load(Ordering::SeqCst), 1);

        // Before TTL expires, the cached frame is returned.
        let cached = Arc::clone(&cache)
            .execute(SyncScope::Legacy, call_payload(42), {
                let runs = Arc::clone(&runs);
                move |c| async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    reply_frame(c.id, "should-not-run")
                }
            })
            .await;
        assert_eq!(cached, reply_frame(42, "fresh-1"));
        assert_eq!(runs.load(Ordering::SeqCst), 1);

        tokio::time::sleep(Duration::from_millis(120)).await;

        let after_ttl = Arc::clone(&cache)
            .execute(SyncScope::Legacy, call_payload(42), {
                let runs = Arc::clone(&runs);
                move |c| async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    reply_frame(c.id, "fresh-2")
                }
            })
            .await;
        assert_eq!(after_ttl, reply_frame(42, "fresh-2"));
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }

    async fn run_counting(
        cache: &Arc<WsSyncCache>,
        scope: SyncScope,
        id: u64,
        runs: &Arc<AtomicUsize>,
    ) -> WsServerMessage {
        let runs = Arc::clone(runs);
        Arc::clone(cache)
            .execute(scope, call_payload(id), move |c| async move {
                runs.fetch_add(1, Ordering::SeqCst);
                reply_frame(c.id, "run")
            })
            .await
    }

    fn session(name: &str) -> SyncScope {
        SyncScope::Session(name.to_owned())
    }

    #[tokio::test]
    async fn sessions_with_same_call_key_execute_independently() {
        let cache = Arc::new(WsSyncCache::new(8, Duration::from_secs(60)));
        let runs = Arc::new(AtomicUsize::new(0));
        run_counting(&cache, session("a"), 1, &runs).await;
        run_counting(&cache, session("b"), 1, &runs).await;
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn evict_in_one_session_keeps_other_session_entry() {
        let cache = Arc::new(WsSyncCache::new(8, Duration::from_secs(60)));
        let runs = Arc::new(AtomicUsize::new(0));
        run_counting(&cache, session("a"), 1, &runs).await;
        run_counting(&cache, session("b"), 1, &runs).await;
        assert_eq!(runs.load(Ordering::SeqCst), 2);

        cache.evict_id(&session("a"), 1);

        run_counting(&cache, session("b"), 1, &runs).await;
        assert_eq!(runs.load(Ordering::SeqCst), 2, "B stays cached");
        run_counting(&cache, session("a"), 1, &runs).await;
        assert_eq!(runs.load(Ordering::SeqCst), 3, "A re-executes after evict");
    }

    #[tokio::test]
    async fn legacy_scope_shares_entries_across_connections() {
        let cache = Arc::new(WsSyncCache::new(8, Duration::from_secs(60)));
        let runs = Arc::new(AtomicUsize::new(0));
        run_counting(&cache, SyncScope::Legacy, 1, &runs).await;
        run_counting(&cache, SyncScope::Legacy, 1, &runs).await;
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn parse_session_param_accepts_uuid_and_rejects_bad_values() {
        let uuid = "3f2b8c1e-9d4a-4e7b-a1c2-0123456789ab";
        assert_eq!(
            parse_session_param(Some(uuid)),
            Ok(SyncScope::Session(uuid.to_owned()))
        );
        assert_eq!(parse_session_param(None), Ok(SyncScope::Legacy));
        assert_eq!(parse_session_param(Some("")), Err(()));
        assert_eq!(parse_session_param(Some("../x")), Err(()));
        assert_eq!(parse_session_param(Some("a b")), Err(()));
        assert_eq!(parse_session_param(Some(&"a".repeat(65))), Err(()));
        assert_eq!(
            parse_session_param(Some(&"a".repeat(64))),
            Ok(SyncScope::Session("a".repeat(64)))
        );
    }
}
