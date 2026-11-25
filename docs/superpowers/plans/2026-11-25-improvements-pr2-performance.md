# PR 2 — Stop blocking the runtime Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move blocking filesystem/CPU work off tokio worker threads, bound per-connection WebSocket work, and trim avoidable costs (P1–P6).

**Architecture:** Blocking sections become `tokio::task::spawn_blocking` closures over owned data.
Settings persistence is split into a pure "prepare" step (under the in-memory lock) and a blocking
"write" step serialized by an async writer lock, so readers never wait on `fsync`. The WS socket
loop gains a per-connection call limiter that never blocks the loop.

**Tech Stack:** Rust 1.100, tokio, axum, reqwest.

**Spec:** `docs/superpowers/specs/2026-11-25-general-improvements-design.md` (PR 2 section, P1–P6)

**Branch:** `perf/runtime-offload` from `main` after PR 1 is merged.

## Global Constraints

- Same as PR 1: MIT clean-room, `deny.toml`, acyclic crate graph, no weakened invariants (notably `atomic_write` stays temp file + `sync_all()` + `rename()` + dir sync), Conventional Commits, no manual version bumps.
- Write ordering of `loader.json` must be preserved: a later successful write never gets overwritten by an earlier one.
- `panic = "unwind"` stays (a panicking task must not abort the daemon).
- Finish with `just check` (this PR touches no infra files).

## Review Focus

1. Install aborted midway (download error after staging starts, or the WS client disconnects) must still remove the staging dir — covered by existing installer rollback tests; Task 1 must keep them green unchanged.
2. Two concurrent `set_setting` calls on the same key: disk and memory must agree afterwards — test in Task 2.
3. A client that floods calls while one slow call runs must still get pongs answered (no keepalive disconnect) — test in Task 3.
4. `FULL_SYNC` larger than the queue must reply errors only for the excess, not drop the whole batch — test in Task 3.
5. A plugin flagged `"debug"` installed while the watcher is idle must start being swept — test in Task 5.

---

### Task 1: Offload installer extraction, hashing, and commit (P1a)

**Files:**
- Modify: `crates/zerodeck-core/src/installer.rs` (`prepare_staged_plugin`, `fetch_and_verify_archive`, `stage_remote_binaries`)
- Modify: `crates/zerodeck-core/src/manager.rs` (`install_plugin`: `commit_staged_plugin` + `discover_single_plugin`)
- Test: `crates/zerodeck-core/src/lib.rs` tests module

**Interfaces:**
- Produces: private `async fn run_blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, CoreError> + Send + 'static) -> Result<T, CoreError>` in `installer.rs` (maps `JoinError` to `CoreError::Io` with `ErrorKind::Other`, message `"blocking task failed"`); made `pub(crate)` and reused by Task 2 (settings writes, file picker).
- Unchanged public signatures: `prepare_staged_plugin`, `commit_staged_plugin`, `PluginManager::install_plugin`.

- [ ] **Step 1: Write the failing test** `install_does_not_starve_current_thread_runtime` (`#[tokio::test(flavor = "current_thread")]`): build a sideload ZIP with one 128 MiB zero-filled `assets/big.bin` plus a minimal valid plugin; spawn a ticker task that records the max gap between 10 ms `tokio::time::sleep` ticks; run `manager.install_plugin(local zip, user_confirmed)`; assert max gap < 250 ms.
- [ ] **Step 2: Run to verify failure** — `just check-rust`; Expected: FAIL (gap ≈ extraction time).
- [ ] **Step 3: Implement.** In `prepare_staged_plugin`, wrap everything from `ensure_dir_0755(&plugins_dir)` through `read_and_validate_manifests` in one `run_blocking` closure that owns `zip_bytes`, `paths.clone()`, `req` fields it needs, and returns `(staging_root, plugin_stage_dir, wrapper_dir, manifest, package)`; the `StagingDirGuard` lives inside the closure and is disarmed there. SHA-256 of downloaded/local bytes runs in `run_blocking`. `remote_binary` file writes + chmod run in `run_blocking` per binary. In `install_plugin`, `commit_staged_plugin(staged, &target_path)` and the following `discover_single_plugin` run in one `run_blocking`.
- [ ] **Step 4: Run to verify pass** — `just check-rust`; Expected: PASS, all existing installer tests green.
- [ ] **Step 5: Commit** — `perf(core): run plugin extraction, hashing and commit on the blocking pool`

### Task 2: Non-blocking settings persistence and file picker (P1b, P6)

**Files:**
- Modify: `crates/zerodeck-core/src/settings.rs`
- Modify: `crates/zerodeck-core/src/manager.rs` (all settings mutators, `list_files`)
- Modify: `crates/zerodeck-server/src/router.rs` (`dispatch_set_setting`, `dispatch_filepicker` become `async`)
- Test: `crates/zerodeck-core/src/lib.rs` tests module, `settings.rs` unit tests

**Interfaces:**
- Produces (`settings.rs`):
  ```rust
  #[derive(Debug)]
  pub struct PendingSettings { values: Map<String, Value>, bytes: Vec<u8>, path: PathBuf }
  impl PendingSettings { pub fn write(&self) -> Result<(), CoreError>; }      // atomic_write, blocking
  impl SettingsManager {
      pub fn prepare_update<F>(&self, mutate: F) -> Result<Option<PendingSettings>, CoreError>
          where F: FnOnce(&mut Map<String, Value>) -> Result<bool, CoreError>; // None = no change
      pub fn apply(&mut self, pending: PendingSettings);
  }
  ```
  `set`, `set_path`, `remove`, `update_map` keep their signatures and become `prepare_update` + `write` + `apply`. Serialization uses `serde_json::to_vec_pretty(&values)` directly (no second clone — P6).
- Produces (manager): field `settings_writer: tokio::sync::Mutex<()>`; private `async fn persist_settings<F>(&self, mutate: F) -> Result<(), CoreError>` = lock writer → `prepare_update` under the std lock → `run_blocking(pending.write)` → `apply` under the std lock. Public changes: `pub async fn set_setting(&self, key: &str, value: Value) -> Result<(), CoreError>`; `pub async fn list_files(&self, query: FilePickerQuery) -> Result<FilePickerListing, CoreError>` (runs `FilePicker::list` in `run_blocking` with cloned `PlatformPaths`). `set_plugin_order`, enable/disable, uninstall cleanup use `persist_settings`.

- [ ] **Step 1: Write failing tests**: `prepare_update_returns_none_when_unchanged_and_apply_updates_values` (settings unit), `concurrent_setting_writes_leave_disk_and_memory_consistent` (spawn 32 `set_setting("k", json!(i))` concurrently on a multi-thread runtime; then reopen `loader.json` from disk and assert its `k` equals `manager.get_setting(Some("k"))`), `reads_are_not_blocked_by_settings_writes` (while 20 concurrent `set_setting` calls run, 100 `list_plugins()` calls on another task all complete within 1 s).
- [ ] **Step 2: Run to verify failure** — `just check-rust`; Expected: FAIL (compile: `prepare_update`, `async set_setting`).
- [ ] **Step 3: Implement** per Interfaces; update router call sites (`.await`) and existing tests.
- [ ] **Step 4: Run to verify pass** — `just check-rust`; Expected: PASS.
- [ ] **Step 5: Commit** — `perf(core): persist settings and list files off the async workers`

### Task 3: Per-connection WebSocket call limits (P2)

**Files:**
- Modify: `crates/zerodeck-server/src/router.rs` (`ServerConfig`, `handle_ws_socket`, `handle_ws_text`, `spawn_ws_call`)
- Test: `crates/zerodeck-server/src/lib.rs` WS tests
- Modify: `docs/architecture/wire-protocol.md`

**Interfaces:**
- Produces: `ServerConfig.ws_max_in_flight_calls: usize` (default `64`), `ServerConfig.ws_max_queued_calls: usize` (default `1024`).
- Produces: `struct CallLimiter { permits: Arc<Semaphore>, pending: Arc<AtomicUsize>, capacity: usize }` (capacity = in-flight + queued) with `fn try_admit(&self) -> Option<PendingGuard>`; `PendingGuard` decrements `pending` on drop. `handle_ws_text(...) -> Vec<WsServerMessage>` returns immediate rejections, which the loop sends itself; admitted calls are spawned and acquire a permit inside the task.
- Rejection frame: `ws_error(id, "TooManyPendingCalls", "too many pending calls on this connection", None)`. `FULL_SYNC` messages are admitted one by one; only those beyond capacity are rejected.

- [ ] **Step 1: Write failing tests** (config `ws_max_in_flight_calls: 1`, `ws_max_queued_calls: 1`, slow plugin from `ws_slow_plugin_call_does_not_starve_keepalive_or_concurrent_calls`): `ws_rejects_calls_beyond_pending_capacity` (3 slow calls → third gets `TooManyPendingCalls` before the first finishes; first two eventually reply), `ws_full_sync_rejects_only_excess_messages` (FULL_SYNC of 3 → exactly one rejection), `ws_keepalive_survives_full_queue` (short `ws_ping_interval`, queue full for 3 intervals, client still connected).
- [ ] **Step 2: Run to verify failure** — `just check-rust`; Expected: FAIL.
- [ ] **Step 3: Implement** per Interfaces.
- [ ] **Step 4: Run to verify pass** — `just check-rust`; Expected: PASS.
- [ ] **Step 5: Docs** — `wire-protocol.md`: limits, defaults, error name.
- [ ] **Step 6: Commit** — `perf(server): bound in-flight and queued WebSocket calls per connection`

### Task 4: `/fetch` body copy and typed timeouts (P3)

**Files:**
- Modify: `crates/zerodeck-core/src/error.rs` (variant), `crates/zerodeck-core/src/http_policy.rs` (`send_checked_request` error mapping)
- Modify: `crates/zerodeck-server/src/fetch.rs`
- Test: `crates/zerodeck-server/src/lib.rs` fetch tests

**Interfaces:**
- Produces: `CoreError::HttpTimeout(String)`; `send_checked_request` maps `reqwest::Error` with `is_timeout()` to it, everything else stays `HttpRequest`. `/fetch` maps `HttpTimeout` → `504`, `HttpRequest` → `502` (no string matching). Request body passed as `reqwest::Body::from(body_bytes)`.

- [ ] **Step 1: Write the failing test** `fetch_proxy_maps_upstream_timeout_to_504`: mock upstream accepts and sleeps 2 s; server config `fetch_response_timeout: 200 ms`; expect `504`. Plus `fetch_proxy_maps_connection_refused_to_502`.
- [ ] **Step 2: Run to verify failure** — `just check-rust`; Expected: FAIL (variant missing).
- [ ] **Step 3: Implement** per Interfaces; update installer call sites that match `HttpRequest` to also handle `HttpTimeout` where they surface messages.
- [ ] **Step 4: Run to verify pass** — `just check-rust`; Expected: PASS.
- [ ] **Step 5: Commit** — `perf(server): avoid copying /fetch bodies and classify upstream timeouts by type`

### Task 5: Idle debug watcher (P5)

**Files:**
- Modify: `crates/zerodeck-core/src/watcher.rs` (`run_watcher_loop`, `WatcherState`)

**Interfaces:**
- Produces: the sweep branch of the `select!` is only polled while `!state.tracked.is_empty()` (`if` precondition on the branch); `WatcherState` gets `#[cfg(test)] sweeps: usize` incremented per sweep.

- [ ] **Step 1: Write failing tests** in `watcher.rs` `#[cfg(test)]` (`tokio::time::pause()`): `watcher_without_debug_plugins_does_not_sweep` (advance 10 s → `sweeps == 0`), `watcher_resumes_sweeping_after_debug_plugin_appears` (emit `PluginsChanged` with a debug plugin tracked → advance 5 s → `sweeps >= 2`).
- [ ] **Step 2: Run to verify failure** — `just check-rust`; Expected: FAIL.
- [ ] **Step 3: Implement** the branch precondition; reset the interval when tracking resumes.
- [ ] **Step 4: Run to verify pass** — `just check-rust`; Expected: PASS.
- [ ] **Step 5: Commit** — `perf(core): pause the debug watcher sweep when no debug plugin is tracked`

### Task 6: Release profile (P4)

**Files:**
- Modify: `Cargo.toml`

- [ ] **Step 1: Record the baseline:** `just build && docker run --rm --entrypoint stat zerodeck:runtime -c %s /usr/local/bin/zerodeckd`.
- [ ] **Step 2: Add**
  ```toml
  [profile.release]
  lto = "thin"
  codegen-units = 1
  strip = "debuginfo"
  ```
- [ ] **Step 3: Rebuild and record** the new size with the same command; `just smoke --no-build` passes.
- [ ] **Step 4: Commit** — `perf: enable thin LTO and single codegen unit for release builds` (sizes in the commit body).

### Task 7: Final verification and PR

- [ ] **Step 1:** `just check` passes.
- [ ] **Step 2:** Open PR `perf: stop blocking the async runtime` with the before/after binary size.
