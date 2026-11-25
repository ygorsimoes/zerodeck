# PR 3 — Organization and cleanup Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the codebase easier to navigate and harder to misuse without changing behavior (C1–C10), plus the Python worker and frontend fixes that ride along (C8, C9).

**Architecture:** Pure moves first (shared helper, lint attributes), then structural splits (core
tests, server router), then API consolidation (builders), then the small behavioral fixes in
Python/frontend/platform, each with its own test. Refactor tasks are proven by "same tests, same
count, all green".

**Tech Stack:** Rust 1.100, Python ≥3.10 (unittest, ruff, pyright strict), TypeScript (node:test, biome).

**Spec:** `docs/superpowers/specs/2026-11-25-general-improvements-design.md` (PR 3 section, C1–C10)

**Branch:** `refactor/cleanup` from `main` after PR 2 is merged.

## Global Constraints

- Same as PR 1/PR 2 constraints; `hosts/python` keeps `dependencies = []`; frontend keeps `react`, `react/jsx-runtime`, `react-dom` external; `pnpm` only.
- Refactor tasks change no behavior: the test count per crate before and after is recorded in the commit body and must match (moves) or grow (new tests).
- Adding the direct `zerodeck-server → zerodeck-platform` edge requires updating the graph in `AGENTS.md` §2.
- Finish with `just check`.

## Review Focus

1. Moving core tests must not silently drop a test (e.g. one guarded by `#[cfg(unix)]`) — Task 3 compares `cargo test -- --list` output.
2. `forbid(unsafe_code)` must not be added to a crate that a later Phase 4 CEF task needs `unsafe` in — Task 2 leaves `zerodeck-cef` at `deny` instead of `forbid` (see Task 2).
3. A Python plugin whose synchronous `_main` raises must still log the exception — test in Task 7.
4. Frontend loaded from a non-HTTP URL (tests, `file:`) must fall back to `127.0.0.1:1337` — test in Task 8.
5. The builder API must still allow the static (no manager) backend used by server unit tests — Task 5 keeps it reachable under `#[doc(hidden)]`.

---

### Task 1: One `lock_unpoisoned` (C3)

**Files:**
- Create: `crates/zerodeck-platform/src/sync.rs`; export from `crates/zerodeck-platform/src/lib.rs`
- Modify: `crates/zerodeck-core/src/manager.rs:861`, `crates/zerodeck-plugin-host/src/supervisor.rs:1059`, `crates/zerodeck-server/src/ws_sync.rs:228` (delete local copies, import)
- Modify: `crates/zerodeck-server/Cargo.toml` (`zerodeck-platform` in `[dependencies]` if only a dev-dependency today), `AGENTS.md` §2 graph

**Interfaces:**
- Produces: `pub fn lock_unpoisoned<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T>` in `zerodeck_platform`.

- [ ] **Step 1: Write the test** `lock_unpoisoned_recovers_after_panic` in `sync.rs` (poison a mutex in a thread that panics while holding it; `lock_unpoisoned` still returns the guard with the value).
- [ ] **Step 2:** Move the function, replace the three copies, update `AGENTS.md` graph (`zerodeck-server ── zerodeck-core, zerodeck-cef, zerodeck-plugin-host, zerodeck-platform, zerodeck-protocol`).
- [ ] **Step 3:** `just check-rust` → PASS.
- [ ] **Step 4: Commit** — `refactor: share lock_unpoisoned from zerodeck-platform`

### Task 2: `unsafe` policy (C5)

**Files:**
- Modify: crate roots `crates/zerodeck-protocol/src/lib.rs`, `crates/zerodeck-platform/src/lib.rs`, `crates/zerodeck-core/src/lib.rs`, `crates/zerodeck-server/src/lib.rs`, `crates/zerodeckd/src/main.rs`, `crates/zerodeck-cef/src/lib.rs`
- Modify: `AGENTS.md` §3

**Interfaces:**
- Produces: `#![forbid(unsafe_code)]` in protocol, platform, core, server, zerodeckd. `zerodeck-cef` gets `#![deny(unsafe_code)]` (Phase 4 may need a reviewed `#[allow]`; `forbid` cannot be overridden). `zerodeck-plugin-host` keeps the workspace `warn` and its single `// SAFETY:` block.

- [ ] **Step 1:** Add the attributes; `just check-rust` → PASS (proves no hidden `unsafe`).
- [ ] **Step 2:** `AGENTS.md` §3: "Crates without `unsafe` declare `#![forbid(unsafe_code)]`; `zerodeck-cef` uses `deny`; only `zerodeck-plugin-host` may contain `unsafe`."
- [ ] **Step 3: Commit** — `refactor: forbid unsafe code in crates that do not need it`

### Task 3: Core tests next to the code (C1)

**Files:**
- Modify: `crates/zerodeck-core/src/lib.rs` (remove the `tests` module, ~2.7k lines)
- Create: `crates/zerodeck-core/tests/common/mod.rs` (shared fixtures: temp homebrew, zip builders, mock HTTP server, paths)
- Create: `crates/zerodeck-core/tests/{installer.rs, manager.rs, filepicker.rs, watcher.rs, http_policy.rs, settings.rs, archive.rs, discovery.rs}`
- Modify: `#[cfg(test)] mod tests` in the module files for tests that need private items

**Interfaces:**
- Rule: a test that compiles against the crate's `pub` API goes to `tests/<area>.rs`; one that needs a private item goes into that module's `#[cfg(test)] mod tests`. Tests keep their names.

- [ ] **Step 1:** Record `cargo test -p zerodeck-core --locked -- --list 2>/dev/null | grep -c ': test$'` (inside `just shell`).
- [ ] **Step 2:** Move tests area by area; after each area run `cargo test -p zerodeck-core` → PASS.
- [ ] **Step 3:** Recount; the number equals Step 1 (plus any tests added earlier in this PR). `just check-rust` → PASS.
- [ ] **Step 4: Commit** — `test(core): move tests next to the modules and public API they cover` (counts in body)

### Task 4: Split the server router (C2)

**Files:**
- Create: `crates/zerodeck-server/src/state.rs` (`ServerConfig`, `AppState`, `PluginBackend`)
- Create: `crates/zerodeck-server/src/routes/{mod.rs, static_files.rs, legacy.rs}` (`build_router`, `serve`, `redacted_uri`, file serving, `/methods/*`)
- Create: `crates/zerodeck-server/src/ws/{mod.rs, socket.rs, dispatch.rs, args.rs}` (upgrade + loop + limiter, route table, argument parsers)
- Delete: `crates/zerodeck-server/src/router.rs`
- Modify: `crates/zerodeck-server/src/lib.rs` (`pub use` paths unchanged for `AppState`, `ServerConfig`, `build_router`, `serve`, `WsSyncCache`)

**Interfaces:**
- Produces: `pub(crate) fn reply<T: serde::Serialize>(id: u64, result: Result<T, CoreError>) -> WsServerMessage` in `ws/dispatch.rs` (serialize error → `BackendError`, `CoreError` → existing `core_error_to_ws`). Every `dispatch_*` that ends in "match result → Reply / error" uses it.

- [ ] **Step 1:** Record the server test count (same command as Task 3, `-p zerodeck-server`).
- [ ] **Step 2:** Move code into the new files without edits; `just check-rust` → PASS.
- [ ] **Step 3:** Introduce `reply` and collapse the `dispatch_*` match blocks; `just check-rust` → PASS; test count unchanged; no file above ~500 lines.
- [ ] **Step 4: Commit** — two commits: `refactor(server): split router into state, routes and ws modules` and `refactor(server): collapse WebSocket dispatch boilerplate`

### Task 5: Fallible builders instead of constructor sprawl (C4)

**Files:**
- Modify: `crates/zerodeck-server/src/state.rs`, `crates/zerodeck-core/src/manager.rs`, `crates/zerodeckd/src/main.rs`, all tests constructing them

**Interfaces:**
- Produces (server):
  ```rust
  impl AppState { pub fn builder(config: ServerConfig) -> AppStateBuilder; }
  impl AppStateBuilder {
      pub fn manager(self, manager: Arc<PluginManager>) -> Self;
      pub fn http_client(self, client: CheckedHttpClient) -> Self;
      #[doc(hidden)] pub fn static_plugins(self, plugins: Vec<DiscoveredPlugin>) -> Self;
      pub fn build(self) -> Result<AppState, ServerError>;   // builds the default client when none given
  }
  ```
  `ServerError` gains `HttpClient(#[from] CoreError)` if needed. `AppState::new`, `with_plugins`, `with_manager`, `with_manager_and_http`, `with_http_for_tests` are removed; `build_default_http_client`'s `expect` is gone.
- Produces (core): `PluginManager::builder(paths: PlatformPaths, settings: SettingsManager) -> PluginManagerBuilder` with `.plugins(Vec<DiscoveredPlugin>)`, `.supervisor_config(SupervisorConfig)`, `.loader_version(impl Into<String>)`, `.http_client(CheckedHttpClient)`, `.installer_limits(InstallerLimits)`, `.build() -> Result<PluginManager, CoreError>`. `PluginManager::open` stays as a convenience that uses the builder; `from_parts` and `with_http_config` are removed.

- [ ] **Step 1:** Write `app_state_builder_defaults_to_static_empty_backend` and `plugin_manager_builder_uses_production_http_policy_by_default` (assert `policy()` blocks `127.0.0.1`).
- [ ] **Step 2:** Implement, migrate call sites; `just check-rust` → PASS.
- [ ] **Step 3: Commit** — `refactor: replace AppState and PluginManager constructors with fallible builders`

### Task 6: Deduplicate `tokio-tungstenite` (C6)

**Files:**
- Modify: `Cargo.toml`, `Cargo.lock`

- [ ] **Step 1:** Set `tokio-tungstenite = "0.29.0"` (the version axum 0.8.9 resolves). `cargo build --workspace --all-targets --locked` won't pass with the old lock: run `cargo update -p tokio-tungstenite@0.30.0 --precise 0.29.0` inside `just shell`.
- [ ] **Step 2:** `just check` → PASS and `cargo tree -d` no longer lists `tokio-tungstenite`. If a used 0.30 API is missing, revert and note the reason in the PR body instead.
- [ ] **Step 3: Commit** — `build(deps): align tokio-tungstenite with axum to drop the duplicate`
  (Narrowing tokio features is skipped: every crate needs most of `full`, so the gain is not "cheap" per the spec.)

### Task 7: Python lint rules and worker fixes (C7, C8)

**Files:**
- Modify: `hosts/python/pyproject.toml` (`select = ["E", "F", "I", "UP", "B", "SIM", "RUF", "ASYNC", "C4", "PIE"]`)
- Modify: `hosts/python/zerodeck_py_host/runner.py` (`start_lifecycle`, `stop_lifecycle`), `hosts/python/zerodeck_py_host/settings.py` (`commit`), any file with new lint findings
- Test: `hosts/python/tests/test_runner.py`

**Interfaces:**
- Produces: `PluginRunner.main_thread: threading.Thread | None` — a synchronous `_main` runs in `threading.Thread(target=..., name=f"{plugin_name}-main", daemon=True)`; exceptions are logged with the same `"plugin _main task failed"` message. Async `_main` keeps using `main_task`. `stop_lifecycle` never joins the daemon thread.
- Produces: `SettingsManager.commit` opens the settings directory with `os.open(dir, os.O_RDONLY)` after `os.replace` and `os.fsync`s it (skipped on platforms without `O_DIRECTORY` support errors → ignore `OSError`).

- [ ] **Step 1: Write failing tests**: `test_sync_main_runs_in_daemon_thread_and_stop_returns_promptly` (sync `_main` sleeps 30 s; `stop_lifecycle` returns in < 1 s; `runner.main_thread.daemon is True`), `test_sync_main_exception_is_logged`, `test_settings_commit_fsyncs_file_and_directory` (patch `os.fsync`, expect 2 calls).
- [ ] **Step 2: Run to verify failure** — `just check-python`; Expected: FAIL.
- [ ] **Step 3: Implement**, then enable the new ruff rules and fix findings (no `# noqa` unless the rule conflicts with Decky's API names, as `N802` already does).
- [ ] **Step 4: Run to verify pass** — `just check-python` and `just py-matrix`; Expected: PASS on 3.10–3.13.
- [ ] **Step 5: Commit** — two commits: `fix(py-host): run synchronous _main in a daemon thread and fsync the settings directory` and `style(py-host): enable SIM, RUF, ASYNC, C4 and PIE ruff rules`

### Task 8: Frontend daemon origin (C9)

**Files:**
- Create: `frontend/src/origin.ts`
- Modify: `frontend/src/index.ts`
- Create: `frontend/tests/origin.test.ts`
- Modify: `frontend/package.json` (`"test": "node --test tests/"`), `justfile` `_fe-test` (`node --test frontend/tests/`)

**Interfaces:**
- Produces:
  ```ts
  export const FALLBACK_ORIGIN = 'http://127.0.0.1:1337';
  export function daemonOrigin(moduleUrl: string | undefined): string; // http(s) URL → its origin, else FALLBACK_ORIGIN
  export function webSocketBase(origin: string): string;              // http→ws, https→wss
  ```
  `index.ts` fetches `${origin}/auth/token` and builds `WSRouter({ baseUrl: webSocketBase(origin), … })` with `origin = daemonOrigin(import.meta.url)`.

- [ ] **Step 1: Write failing tests**: `daemonOrigin('http://127.0.0.1:4242/frontend/index.js') === 'http://127.0.0.1:4242'`, `daemonOrigin('file:///x/index.js') === FALLBACK_ORIGIN`, `daemonOrigin(undefined) === FALLBACK_ORIGIN`, `daemonOrigin('not a url') === FALLBACK_ORIGIN`, `webSocketBase('https://h:1') === 'wss://h:1'`, `webSocketBase('http://h:1') === 'ws://h:1'`.
- [ ] **Step 2: Run to verify failure** — `just check-frontend`; Expected: FAIL (module missing).
- [ ] **Step 3: Implement** and wire `index.ts`.
- [ ] **Step 4: Run to verify pass** — `just check-frontend`; Expected: PASS.
- [ ] **Step 5: Commit** — `fix(frontend): derive the daemon origin from the served bundle URL`

### Task 9: `SafePathSet` reports every root (C10)

**Files:**
- Modify: `crates/zerodeck-platform/src/error.rs`, `crates/zerodeck-platform/src/safe_path.rs`, `crates/zerodeck-core/src/error.rs` (`is_path_escape`), server error mapping if it matches on `PathEscape`

**Interfaces:**
- Produces: `PlatformError::PathOutsideRoots { candidate: PathBuf, roots: Vec<PathBuf> }` (`#[error("path `{candidate}` is outside the allowed roots {roots:?}")]`), returned by `SafePathSet::resolve`; `CoreError::is_path_escape()` matches both `PathEscape` and `PathOutsideRoots` (→ `403` unchanged).

- [ ] **Step 1: Write failing tests**: `safe_path_set_error_lists_every_root`, `is_path_escape_matches_path_outside_roots`.
- [ ] **Step 2: Run to verify failure** — `just check-rust`; Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run to verify pass** — `just check-rust`; Expected: PASS.
- [ ] **Step 5: Commit** — `refactor(platform): report every allowed root when a path escapes a SafePathSet`

### Task 10: Final verification and PR

- [ ] **Step 1:** `just check` passes (this PR touches `justfile` in Task 8 → run `just ci`).
- [ ] **Step 2:** Open PR `refactor: organize modules, tests and constructors` listing test counts before/after.
