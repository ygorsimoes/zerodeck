# ZeroDeck Phase 2 — Plugin Host, Supervisor & Python Parity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build ZeroDeck's privilege-isolated, `socketpair`-based plugin supervisor and full `zerodeck-py-host` compatibility layer, wire them into `zerodeckd` and async `/ws`, and update project documentation.

**Architecture:** `zerodeck-platform` resolves non-root host identities from `/etc/passwd` + `/etc/group` (or current UID/GID when unprivileged) and manages `chown`/`0700` runtime directories. `zerodeck-plugin-host` spawns each active `main.py` worker over an anonymous `socketpair()` (`--fd`), clears env, drops privileges in `pre_exec`, captures `stdout`/`stderr`, correlates UUID RPCs, and supervises crashes/shutdowns. `zerodeck-core` (`PluginManager`) coordinates `SettingsManager` (`loader.json`), `discover_plugins`, and `PluginSupervisor`, while `zerodeck-server` dispatches `/ws` calls concurrently and broadcasts `loader/plugin_event` frames.

**Tech Stack:** Rust Edition 2024 (`1.100.0`, `tokio 1.54`, `axum 0.8`, `rustix 1.1` with `thread`/`process`/`fs`/`io`, `serde_json`, `thiserror`, `tracing`, `uuid`), Python `>=3.10` stdlib-only (`asyncio`, `json`, `ssl`, `ruff`, `pyright`), Docker-first `just check`.

**Spec:** [2026-11-24-phase2-plugin-host-design.md](file:///Users/ygorsimoes/Documents/projects/ygorsimoes/zerodeck/docs/superpowers/specs/2026-11-24-phase2-plugin-host-design.md)

## Global Constraints
- **Clean-room MIT only:** Zero GPL code copied from `decky-loader`; implement `settings`, `helpers`, and protocol handlers strictly from public contracts; all Rust deps must satisfy `deny.toml`.
- **Acyclic crate graph:** `zerodeck-protocol` (no internal deps) → `zerodeck-platform` → `zerodeck-plugin-host` → `zerodeck-core` → `zerodeck-server` → `zerodeckd`.
- **Rust quality:** `rustfmt.toml`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, minimize `unsafe` and document every `unsafe` block with `// SAFETY:`.
- **No allocations/locks in `pre_exec`:** Only async-signal-safe syscalls inside `CommandExt::pre_exec`.
- **Python worker:** `dependencies = []` in `hosts/python/pyproject.toml`, compatible with Python `3.10..3.13`, `ruff` + `pyright` strict clean.
- **Verification:** `just check` (Docker dev container) must pass before opening PR.

## Review Focus
- **`api_version == 0` `@classmethod` / `@staticmethod` / plain function dispatch in `runner.py`:** Legacy v0 plugins define methods on `class Plugin` in all three forms; calling `@classmethod` or `@staticmethod` with an extra `self.plugin` arg causes `TypeError` — pinned by tests in Task 2.
- **Blocking synchronous plugin methods stalling `emit()` and concurrent IPC:** A sync `def` doing `time.sleep` or blocking I/O must not block concurrent RPCs or `decky.emit()` — pinned by tests in Task 2.
- **Slow plugin RPC starving `/ws` keepalive pong:** A slow plugin call on `/ws` must run in a spawned task so `socket.recv()` keeps reading `Pong` frames during keepalive rounds — pinned by test in Task 5.
- **Worker crash mid-call and crash-loop circuit breaker:** Crashing while an RPC is pending must immediately return `WorkerStopped` / `WsServerMessage::Discard` (not hang until timeout), and repeated rapid crashes must trip to `Failed` — pinned by tests in Task 3 and Task 5.
- **`HostUserIdentity` on `root` (`HOME=/root`) vs. unprivileged macOS/Docker:** When `euid == 0`, `stat(/root)` yielding `uid == 0` must be skipped and fail closed if no non-root user exists; when `euid != 0`, `/etc/passwd` is bypassed cleanly — pinned by tests in Task 1.

---

### Task 1: `zerodeck-platform` Host Identity & Owned Directory Primitives

**Files:**
- Modify: `Cargo.toml` (add `"thread"` and `"io"` features to `rustix` workspace dep)
- Modify: `crates/zerodeck-platform/src/error.rs`
- Modify: `crates/zerodeck-platform/src/privilege.rs`
- Modify: `crates/zerodeck-platform/src/lib.rs`

**Interfaces:**
- Consumes: `rustix`, `PlatformPaths`, `PlatformError`, `PrivilegeTarget`, `current_process_identity()`.
- Produces:
  - `PlatformError::HostUserNotFound(String)`
  - `HostUserIdentity { pub username: String, pub uid: u32, pub gid: u32, pub supplemental_gids: Vec<u32>, pub home_dir: PathBuf }` (under `#[cfg(unix)]`)
  - `HostUserIdentity::resolve(paths: &PlatformPaths) -> Result<Self, PlatformError>` and `HostUserIdentity::resolve_from(paths: &PlatformPaths, euid: u32, passwd: &str, group: &str, env_user: Option<&str>, home_owner_uid: Option<u32>) -> Result<Self, PlatformError>`
  - `ensure_owned_dir(path: &Path, owner: Option<(u32, u32)>, mode: Option<u32>) -> Result<(), PlatformError>`

- [ ] **Step 1: Write failing tests in `crates/zerodeck-platform/src/lib.rs`**
  - `host_user_identity_bypasses_passwd_when_unprivileged`: `euid = 1000`, empty `/etc/passwd`, resolves current UID/GID + `paths.user_home()`.
  - `host_user_identity_skips_uid_zero_home_and_falls_back_to_deck_when_root`: `euid = 0`, `home_owner_uid = Some(0)` (`/root`), `passwd` has `deck:x:1000:1000::/home/deck:/bin/bash` and `group` has `wheel:x:998:deck`, resolves `uid = 1000, gid = 1000, supplemental_gids = [998]`.
  - `host_user_identity_fails_closed_when_root_cannot_find_non_root_user`: `euid = 0`, no valid non-root user in `passwd`, returns `Err(PlatformError::HostUserNotFound(_))`.
  - `ensure_owned_dir_creates_directory_and_applies_mode`: creates nested directory with `0o700` mode on Unix (`owner = None` when unprivileged).

- [ ] **Step 2: Run test to verify it fails**
  Run: `just check-rust` (or targeted `cargo test -p zerodeck-platform`)
  Expected: FAIL (missing `HostUserIdentity`, `ensure_owned_dir`).

- [ ] **Step 3: Implement `HostUserIdentity`, `/etc/passwd` + `/etc/group` parser, and `ensure_owned_dir`**
  - Update `Cargo.toml` `rustix` features to `["process", "fs", "net", "io", "thread"]`.
  - Implement pure-Rust `/etc/passwd` + `/etc/group` parsing and `resolve_from` / `resolve` in `privilege.rs` (zero `unsafe`).
  - Implement `ensure_owned_dir` using `fs::create_dir_all`, `Permissions::from_mode`, and `rustix::fs::chown` when `owner.is_some()` and `current_process_identity().euid == 0`.

- [ ] **Step 4: Run tests and clippy to verify pass**
  Run: `cargo test -p zerodeck-platform && cargo clippy -p zerodeck-platform --all-targets --locked -- -D warnings`
  Expected: PASS with 0 warnings.

- [ ] **Step 5: Commit**
  ```bash
  git add Cargo.toml crates/zerodeck-platform
  git commit -m "feat(platform): resolve host user identity and owned runtime directories"
  ```

---

### Task 2: `hosts/python` (`zerodeck-py-host`) Real-Plugin Parity & Fixes

**Files:**
- Create: `hosts/python/zerodeck_py_host/settings.py`
- Create: `hosts/python/zerodeck_py_host/helpers.py`
- Modify: `hosts/python/zerodeck_py_host/decky_api.py`
- Modify: `hosts/python/zerodeck_py_host/runner.py`
- Modify: `hosts/python/zerodeck_py_host/__init__.py`
- Modify: `hosts/python/tests/test_runner.py`

**Interfaces:**
- Produces:
  - `sys.modules["settings"]`: `SettingsManager(name, settings_directory=None, default_settings=None)` with `.settings`, `.path`, `.read()`, `.commit()`, `.getSetting(key, default=None)`, `.setSetting(key, value)` (atomic temp + `fsync` + `os.replace`, preserves corrupt JSON).
  - `sys.modules["helpers"]`: `get_ssl_context() -> ssl.SSLContext`, `get_homebrew_path() -> str`, `get_user() -> str`.
  - Updated `PluginRunner`: `sys.path.insert(0, py_modules)` when present, `api_version == 0` dispatch supporting `@classmethod`, `@staticmethod`, and plain functions on `class Plugin`, and `asyncio.to_thread` execution for synchronous plugin methods.

- [ ] **Step 1: Write failing unit tests in `hosts/python/tests/test_runner.py`**
  - `test_settings_module_roundtrips_atomically_and_preserves_corrupt_json`: imports `from settings import SettingsManager`, checks `default_settings`, `setSetting`/`getSetting`, atomic persistence across reload, and ensures corrupt JSON logs an error without overwriting the file.
  - `test_helpers_module_exposes_ssl_context_homebrew_path_and_user`: imports `from helpers import get_ssl_context, get_homebrew_path, get_user`, verifies `ssl.SSLContext` and env values.
  - `test_api_version_0_supports_classmethod_staticmethod_and_plain_methods`: tests all three method forms on `class Plugin` with `legacy: True` and lifecycle `_main`/`_unload`.
  - `test_blocking_sync_method_does_not_block_concurrent_calls_or_emit`: runs a slow sync `time.sleep(0.15)` method concurrently with a fast async method + `decky.emit` and asserts the fast call finishes before the slow sync call.
  - `test_py_modules_precedes_stdlib_or_existing_sys_path`: verifies `py_modules/` is inserted at index 0.

- [ ] **Step 2: Run Python tests to verify failure**
  Run: `just check-python`
  Expected: FAIL (`ModuleNotFoundError: No module named 'settings'`, `TypeError` on classmethod/staticmethod).

- [ ] **Step 3: Implement `settings.py`, `helpers.py`, and fixes in `runner.py` / `decky_api.py`**
  - Keep Python `>=3.10` compatible, strict `pyright`, and `ruff` clean with `dependencies = []`.
  - In `decky_api.py`, disambiguate duplicate log filenames within the same second (`timestamp-1.log`).
  - In `runner.py`, use `inspect.getattr_static` for `api_version == 0` methods/lifecycle hooks, and `asyncio.to_thread` when `not inspect.iscoroutinefunction(...)` (still awaiting if a sync wrapper returns an awaitable).

- [ ] **Step 4: Run Python lint, strict typecheck, and unit tests**
  Run: `just check-python`
  Expected: `ruff`, `pyright`, and all `unittest` tests PASS.

- [ ] **Step 5: Commit**
  ```bash
  git add hosts/python
  git commit -m "feat(py-host): add settings/helpers modules, v0 classmethod/staticmethod dispatch, and threaded sync calls"
  ```

---

### Task 3: `zerodeck-plugin-host` (`socketpair`, `pre_exec`, Supervisor, Backoff & Shutdown)

**Files:**
- Modify: `crates/zerodeck-plugin-host/Cargo.toml`
- Modify: `crates/zerodeck-plugin-host/src/error.rs`
- Modify: `crates/zerodeck-plugin-host/src/runtime.rs`
- Create: `crates/zerodeck-plugin-host/src/supervisor.rs`
- Modify: `crates/zerodeck-plugin-host/src/lib.rs`

**Interfaces:**
- Consumes: `zerodeck-platform` (`HostUserIdentity`, `ensure_owned_dir`, `PlatformPaths`, `PrivilegeTarget`, `current_process_identity`), `zerodeck-protocol` (`PluginIpcToWorker`, `PluginIpcFromWorker`, `PluginIpcError`).
- Produces:
  - `HostError::{PluginCallFailed { name, message, traceback }, WorkerStopped, FrameTooLarge, StartupTimeout, InvalidMethodName(String), Platform(PlatformError)}`
  - `PluginState` (`Stopped`, `Starting`, `Running`, `BackingOff`, `Failed`, `Stopping`)
  - `PluginEvent { pub plugin: String, pub event: String, pub args: Vec<Value> }`
  - `SupervisorConfig { pub python_host: PythonHostConfig, pub startup_timeout: Duration, pub shutdown_timeout: Duration, pub crash_window: Duration, pub max_crashes_in_window: usize, pub stability_window: Duration, pub max_backoff: Duration }`
  - `ManagedPlugin` implementing `PluginRuntime`
  - `PluginSupervisor` (`new`, `spawn_plugin`, `call_method`, `reload_plugin`, `plugin_state`, `subscribe_events`, `shutdown_all`)

- [ ] **Step 1: Write failing integration tests in `crates/zerodeck-plugin-host/src/lib.rs`**
  - `supervisor_spawns_python_worker_over_socketpair_and_calls_v0_and_v1_methods`: launches real `python3 -m zerodeck_py_host` with `PYTHONPATH` pointing at `hosts/python`, tests `api_version = 0` (`@classmethod`, `@staticmethod`, plain method), `api_version = 1` (`settings`, `helpers`, `py_modules`, `decky.emit` → `PluginEvent`), and `PluginCallFailed` traceback propagation.
  - `supervisor_cancels_in_flight_calls_with_worker_stopped_and_restarts_with_backoff`: plugin calls `os._exit(1)` during an in-flight RPC; caller receives `HostError::WorkerStopped` promptly, and subsequent calls succeed after auto-restart.
  - `supervisor_trips_crash_loop_to_failed_after_repeated_crashes`: configures low `max_crashes_in_window = 3`, spawns crashing `main.py`, verifies state transitions to `PluginState::Failed` and stops restarting.
  - `supervisor_shutdown_runs_uninstall_lifecycle_and_kills_hung_worker`: verifies `Control { uninstall: true }` runs `_unload` + `_uninstall`, and verifies a worker ignoring `SIGTERM` is killed after `shutdown_timeout`.

- [ ] **Step 2: Run tests to verify failure**
  Run: `cargo test -p zerodeck-plugin-host`
  Expected: FAIL (`PluginSupervisor` / `ManagedPlugin` not implemented).

- [ ] **Step 3: Implement `supervisor.rs`, `error.rs`, and `runtime.rs` updates**
  - Use `std::os::unix::net::UnixStream::pair()` + `BorrowedFd` + `rustix::io::fcntl_setfd(..., FdFlags::empty())` in `pre_exec` (alongside `setpgid(0, 0)`, `PR_SET_PDEATHSIG` on Linux, and `setgroups`/`setresgid`/`setresuid` when `euid == 0` and `PrivilegeTarget::HostUser`). Document `// SAFETY:` on `pre_exec` with zero allocations inside the closure.
  - Clear env (`env_clear()`), preserve safe base vars (`PATH`, `LANG`, `LC_*`, `TERM`, `TZ`, `SSL_CERT_FILE`, `SSL_CERT_DIR`), inject `build_plugin_env`, set `current_dir(&plugin_dir)`.
  - Pipe `stdout`/`stderr` to `tracing` reader tasks; read IPC lines up to `4 MiB`; correlate UUIDs; emit `PluginEvent` over `broadcast`.
  - Gate `setuid`/`chown` OS tests on `current_process_identity().euid == 0` while exercising `HostUserIdentity` + `env_clear()` + `cwd` + `socketpair` unconditionally.

- [ ] **Step 4: Run tests and clippy**
  Run: `cargo test -p zerodeck-plugin-host && cargo clippy -p zerodeck-plugin-host --all-targets --locked -- -D warnings`
  Expected: PASS with 0 warnings.

- [ ] **Step 5: Commit**
  ```bash
  git add crates/zerodeck-plugin-host
  git commit -m "feat(plugin-host): implement socketpair IPC, privilege drop, and supervised Python worker lifecycle"
  ```

---

### Task 4: `zerodeck-core` `PluginManager` (`loader.json` Order/Disabled + Supervisor Coordination)

**Files:**
- Modify: `crates/zerodeck-core/Cargo.toml` (add `tokio` workspace dep)
- Create: `crates/zerodeck-core/src/manager.rs`
- Modify: `crates/zerodeck-core/src/error.rs`
- Modify: `crates/zerodeck-core/src/lib.rs`

**Interfaces:**
- Consumes: `zerodeck-platform` (`PlatformPaths`, `HostUserIdentity`), `zerodeck-plugin-host` (`PluginSupervisor`, `SupervisorConfig`, `PluginEvent`, `PluginState`, `HostError`), `SettingsManager`, `discover_plugins`.
- Produces:
  - `PluginSummary { pub name: String, pub directory: String, pub version: Option<String>, pub load_type: PluginLoadType, pub disabled: bool, pub passive: bool, pub state: PluginState }`
  - `PluginManager` (`new`, `start_all`, `call_plugin_method`, `reload_plugin`, `list_plugins`, `subscribe_events`, `shutdown_all`)

- [ ] **Step 1: Write failing tests in `crates/zerodeck-core/src/lib.rs`**
  - `plugin_manager_respects_plugin_order_and_disabled_plugins`: seeds `loader.json` with `"pluginOrder": ["Beta", "Alpha"]` and `"disabledPlugins": ["Gamma"]` (plus a `"disabled_plugins"` fallback test), verifies `list_plugins()` ordering (`Beta`, `Alpha`, then remaining sorted by directory) and confirms `Gamma` is not spawned (`PluginState::Stopped`, `disabled == true`), while passive plugins are listed without spawning a worker.
  - `plugin_manager_calls_and_reloads_active_plugin`: starts a fixture Python plugin via `PluginManager`, invokes a method, reloads it, and shuts down cleanly.

- [ ] **Step 2: Run tests to verify failure**
  Run: `cargo test -p zerodeck-core`
  Expected: FAIL (`PluginManager` not implemented).

- [ ] **Step 3: Implement `manager.rs` and wire exports in `crates/zerodeck-core/src/lib.rs`**
  - Keep `CoreError` mapping cleanly to `HostError` and `PlatformError`.
  - Respect `pluginOrder` (`Vec<String>` of `manifest.name`) and `disabledPlugins` (with `disabled_plugins` read fallback).
  - Forward `PluginSupervisor` event broadcast stream.

- [ ] **Step 4: Run tests and clippy**
  Run: `cargo test -p zerodeck-core && cargo clippy -p zerodeck-core --all-targets --locked -- -D warnings`
  Expected: PASS with 0 warnings.

- [ ] **Step 5: Commit**
  ```bash
  git add crates/zerodeck-core
  git commit -m "feat(core): add PluginManager integrating loader.json ordering, disabled state, and supervisor"
  ```

---

### Task 5: `zerodeck-server` Async `/ws` Dispatch, Event Broadcast & `zerodeckd` Integration

**Files:**
- Modify: `crates/zerodeck-server/Cargo.toml` (ensure `zerodeck-platform` if needed, `tokio-tungstenite` tests)
- Modify: `crates/zerodeck-server/src/error.rs`
- Modify: `crates/zerodeck-server/src/router.rs`
- Modify: `crates/zerodeck-server/src/lib.rs`
- Modify: `crates/zerodeckd/src/main.rs`
- Modify: `crates/zerodeckd/tests/daemon.rs`

**Interfaces:**
- Consumes: `PluginManager`, `PluginSummary`, `PluginEvent`, `PluginState`, `HostError`.
- Produces:
  - `AppState::with_manager(config: ServerConfig, manager: Arc<PluginManager>)` (retaining `AppState::new` and `AppState::with_plugins` for unit test ergonomics, backed by a lightweight `PluginManager` or controller trait).
  - Async `/ws` dispatch via `tokio::spawn` + `broadcast` forwarding of `loader/plugin_event`.
  - Routes: `loader/call_plugin_method`, `loader/call_legacy_plugin_method`, `loader/get_plugins`, `loader/reload_plugin`, `utilities/ping`, `POST /methods/{ping,get_plugins,call_plugin_method}`.
  - Live `zerodeckd` startup (`start_all()`) and graceful `shutdown_all()` on `SIGTERM`/Ctrl+C (while keeping `--check-config` side-effect-free without spawning workers).

- [ ] **Step 1: Write failing tests in `crates/zerodeck-server/src/lib.rs` and `crates/zerodeckd/tests/daemon.rs`**
  - `zerodeck-server`:
    - `ws_slow_plugin_call_does_not_starve_keepalive_or_concurrent_calls`: spawns a sleeping Python fixture plugin (or slow runtime), verifies `ping` / keepalive pongs continue uninterrupted while the slow call completes.
    - `ws_dispatches_v0_legacy_v1_positional_private_guard_and_plugin_events`: tests `loader/call_plugin_method`, `loader/call_legacy_plugin_method`, `_private` rejection, `PluginError` traceback mapping, `WorkerStopped` → `WsServerMessage::Discard`, and `loader/plugin_event` broadcast (`args: [{ plugin, event, args }]`).
    - `http_methods_call_plugin_method_respects_contract`: tests `200` success, `200` `{ success: false, result: { name, error, traceback } }` on Python exception, `400` on private method/bad JSON, `404` on unknown plugin, `503` on disabled/passive/stopped.
  - `zerodeckd/tests/daemon.rs`:
    - `daemon_runs_python_plugin_over_ws_and_shuts_down_worker_on_sigterm`: creates temp homebrew dir with a real `main.py` plugin, starts `zerodeckd` with `PYTHONPATH=<repo>/hosts/python`, fetches `/auth/token`, connects WS, calls `loader/call_plugin_method`, receives `REPLY` and `loader/plugin_event`, sends `SIGTERM`, and asserts clean exit and child process termination.

- [ ] **Step 2: Run tests to verify failure**
  Run: `cargo test -p zerodeck-server -p zerodeckd`
  Expected: FAIL.

- [ ] **Step 3: Implement `zerodeck-server` async routing/broadcast and `zerodeckd` lifecycle wiring**
  - Forward `PluginManager::subscribe_events()` into a server `broadcast::Sender<WsServerMessage>` (background task translating `PluginEvent` → `WsServerMessage::Event { event: "loader/plugin_event", args: vec![json!({ "plugin": ..., "event": ..., "args": ... })] }`).
  - Spawn each WS `Call` and `FullSync` call on `tokio::spawn` sending to `outbox`.
  - In `zerodeckd/src/main.rs`, resolve `PYTHONPATH` fallback automatically if unset (e.g., `/opt/zerodeck/python` or `<repo>/hosts/python` when running from source), call `manager.start_all().await`, run `serve(...)`, then `manager.shutdown_all().await`.

- [ ] **Step 4: Run tests and clippy**
  Run: `cargo test --workspace --locked && cargo clippy --workspace --all-targets --locked -- -D warnings`
  Expected: PASS with 0 warnings.

- [ ] **Step 5: Commit**
  ```bash
  git add crates/zerodeck-server crates/zerodeckd
  git commit -m "feat(server,daemon): wire live PluginManager, async WS RPC dispatch, and event broadcasting"
  ```

---

### Task 6: Documentation Updates & Full Verification

**Files:**
- Modify: `docs/architecture/wire-protocol.md`
- Modify: `README.md`

**Interfaces:**
- Documents exact Phase 2 contracts (`loader/call_plugin_method`, `loader/call_legacy_plugin_method`, `loader/plugin_event`, `POST /methods/call_plugin_method`, Python `settings` & `helpers` modules, `cwd`, `stdout`/`stderr`, `DISCARD` semantics) and updates `README.md` capability table + Phase 1/2 roadmap checkboxes.

- [ ] **Step 1: Update `docs/architecture/wire-protocol.md` and `README.md`**
  - Verify every documented route, field name, and status matches the implemented code.

- [ ] **Step 2: Run full project checks (`just check`) and smoke test (`just smoke`)**
  Run: `just check && just smoke`
  Expected: All Rust (`fmt`, `clippy`, `cargo test`), `cargo-deny`, Python (`ruff`, `pyright`, `unittest` across `just py-matrix`), Frontend (`biome`, `tsc`, `vite build`, `wsrouter.test.ts`), and hardened Docker runtime smoke test PASS.

- [ ] **Step 3: Commit docs**
  ```bash
  git add docs/architecture/wire-protocol.md README.md docs/superpowers/specs/2026-11-24-phase2-plugin-host-design.md docs/superpowers/plans/2026-11-24-phase2-plugin-host.md
  git commit -m "docs(phase2): document plugin host, Python compat modules, and WS RPC contracts"
  ```

---

## Self-Review Results
1. **Spec coverage:** Every requirement in [2026-11-24-phase2-plugin-host-design.md](file:///Users/ygorsimoes/Documents/projects/ygorsimoes/zerodeck/docs/superpowers/specs/2026-11-24-phase2-plugin-host-design.md) — `HostUserIdentity`, `ensure_owned_dir`, `socketpair`/`pre_exec`, `PluginSupervisor` backoff/crash-loop/shutdown, `settings`/`helpers`/v0/`to_thread`/`py_modules` fixes, `PluginManager` (`loader.json`), async `/ws` + `loader/plugin_event` + HTTP `/methods/call_plugin_method`, `zerodeckd` wiring, and docs — maps 1:1 to Tasks 1–6.
2. **Type consistency:** `HostUserIdentity`, `ensure_owned_dir`, `PluginSupervisor`, `ManagedPlugin`, `PluginState`, `PluginEvent`, `HostError::{PluginCallFailed, WorkerStopped}`, `PluginManager`, `PluginSummary`, and `SettingsManager` match across all tasks.
3. **Review Focus:** All 5 high-risk failure modes have explicit failing tests first in Tasks 1, 2, 3, and 5.
