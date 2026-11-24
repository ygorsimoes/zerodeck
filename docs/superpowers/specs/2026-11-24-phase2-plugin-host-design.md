# ZeroDeck Phase 2 — Plugin Host, Supervisor & Python Parity Design

**Date:** 2026-11-24  
**Status:** Draft — Pending Approval  
**Scope Size:** Project (Phase 2 + General Hardening & Docs)  
**Prerequisite:** [2026-11-24-zerodeck-foundation-design.md](2026-11-24-zerodeck-foundation-design.md)

---

## 1. Intent & Why

Phase 1 established ZeroDeck's workspace, wire/IPC schemas (`zerodeck-protocol`), `SafePath` jail, `atomic_write`, archive validation (`zerodeck-core`), HTTP/WS security perimeter (`zerodeck-server`), Python worker skeleton (`zerodeck-py-host`), and Docker-first CI.

However, `zerodeckd` today only inspects `plugin.json`/`package.json` at startup and serves stub responses. It cannot launch, isolate, communicate with, or restart plugin backends, and `zerodeck-py-host` lacks key runtime modules (`settings`, `helpers`) and legacy dispatch subtleties that real Decky plugins rely on.

Phase 2 turns `zerodeck-plugin-host` and `zerodeck-py-host` into a real, supervised, privilege-isolated plugin runtime integrated into `zerodeckd` and `/ws`, while resolving technical debt and documentation gaps uncovered after Phase 1.

---

## 2. Goals, Non-Goals, and Anti-Goals

### Goals
1. **True privilege isolation (`zerodeck-platform` + `zerodeck-plugin-host`):**
   - Resolve the unprivileged host user identity (`DECKY_USER`, owner of `DECKY_USER_HOME`, or `deck` fallback) to `uid`, `gid`, and supplemental groups on Unix.
   - Pre-create and `chown` per-plugin `settings/<dir>`, `data/<dir>`, and `logs/<dir>` directories when running as `root` so non-root workers can write logs and settings without `EACCES`.
   - Clear inherited parent environment (`env_clear()`), inject a controlled base environment (`PATH`, `LANG`/`LC_*`) + `build_plugin_env()`, set `cwd` to `DECKY_PLUGIN_DIR`, and drop `setgroups` → `setgid` → `setuid` in `pre_exec` unless `"root"` is declared in `plugin.json`.
2. **Private `socketpair()` IPC & `PluginSupervisor` (`zerodeck-plugin-host`):**
   - Create an anonymous Unix `socketpair()` per active plugin (`has_entrypoint == true`) and pass the child FD via `--fd <n>` (keeping optional `--socket-path` inside a `0700` chowned per-plugin runtime directory).
   - Implement `PluginRuntime` in a concrete supervised worker/actor (`ManagedPlugin` + `PluginSupervisor`): async JSON-line I/O (4 MiB max line, matching `zerodeck-py-host`), UUID request-response correlation (`CALL` → `RESPONSE`), `PluginIpcError` propagation, `EVENT` forwarding, and structured capture of worker `stdout`/`stderr` into `tracing`.
   - Enforce lifecycle & resilience: cancel in-flight calls on worker death (`HostError::WorkerStopped` → `WsServerMessage::Discard`), exponential restart backoff (`1s`..`30s`, reset after stability window), crash-loop circuit breaker (trip to `Failed` after repeated rapid crashes), and bounded graceful shutdown (`Control { uninstall }` + `_unload`/`_uninstall` grace timeout → `SIGTERM` → `SIGKILL`).
3. **Real-world `zerodeck-py-host` parity (`hosts/python`):**
   - Provide clean-room `settings` (`SettingsManager` with `name`, `settings_directory`, `default_settings`, `read()`, `commit()`, `getSetting()`, `setSetting()`, `.settings`) and `helpers` (`get_ssl_context()`, `get_homebrew_path()`, `get_user()`) registered in `sys.modules`.
   - Fix `api_version == 0` method & lifecycle dispatch so `@classmethod`, `@staticmethod`, and plain functions on `class Plugin` all execute cleanly.
   - Prepend `py_modules/` (`sys.path.insert(0, ...)` when present), offload blocking synchronous plugin methods via `asyncio.to_thread` so they do not stall `emit()` or concurrent RPCs, and harden log filename uniqueness in `decky_api.py`.
   - Keep `hosts/python/pyproject.toml` free of third-party runtime dependencies (`dependencies = []`).
4. **Live `PluginManager` & Async `/ws` Integration (`zerodeck-core`, `zerodeck-server`, `zerodeckd`):**
   - Replace `zerodeckd`'s discarded `SettingsManager` + static `Vec<DiscoveredPlugin>` snapshot with a shared `PluginManager` coordinating `SettingsManager`, discovered plugins, disabled/order state, and `PluginSupervisor`.
   - Make `/ws` call handling asynchronous (`tokio::spawn` per call writing into `outbox`) so slow plugin calls never block WebSocket keepalive `pong` handling.
   - Broadcast `WsServerMessage::Event` (`loader/plugin_event`) to connected `/ws` clients and route `loader/call_plugin_method`, `loader/call_legacy_plugin_method`, `loader/get_plugins`, and `POST /methods/{get_plugins,call_plugin_method}`.
5. **General Project Improvements & Documentation:**
   - Update `docs/architecture/wire-protocol.md` with the Python runtime modules (`settings`, `helpers`), `cwd` guarantee, `loader/call_plugin_method` / `loader/call_legacy_plugin_method` / `loader/plugin_event` exact wire payloads, and worker shutdown/discard semantics.
   - Update `README.md` status table and Phase 1 roadmap checklist to reflect shipped vs. in-progress capabilities accurately.

### Non-Goals (Deferred to Phase 3 & Phase 4)
- Remote Store ZIP downloads, `remote_binary` downloader, `/fetch` streaming CORS proxy, `utilities/filepicker`, and plugin HTTP static/asset/data routes (Phase 3).
- `notify`-based debug hot-reload file watching (Phase 3).
- Optional compatibility Python `venv` management for unvendored third-party packages like `aiohttp`/`certifi` (evaluated later only if target real plugins lack `py_modules/`).
- CEF CDP injection, `instrumentation/resolve_scope`, Steam QAM React UI, and Steam Deck install/update script (Phase 4).

### Anti-Goals
- **Never** copy GPL code from `decky-loader` (`settings` and `helpers` must be written clean-room from public plugin contracts).
- **Never** perform heap allocations, mutex locks, or non-async-signal-safe calls inside `CommandExt::pre_exec`.
- **Never** put plugin IPC sockets in `/tmp` or weaken `0700`/`0755` permissions.
- **Never** let a single plugin crash, blocking sync method, or unserializable return value crash the daemon or block other plugins or `/ws` keepalive.

---

## 3. Approaches Considered

| Approach | Summary | Trade-offs | Decision |
|---|---|---|---|
| **A. Core Plugin Host + Supervisor + Python Parity + Daemon/WS Integration (Stdlib-only Python)** | Build privilege isolation, `socketpair` IPC, `PluginSupervisor`, `settings`/`helpers`/v0 fixes in `zerodeck-py-host`, wire live `PluginManager` into `zerodeckd` and async `/ws` (`call_plugin_method`, `call_legacy_plugin_method`, `loader/plugin_event`), plus docs/Phase-1 fixes. | Keeps network/downloaders (`reqwest`) and `venv` packaging out of Phase 2; plugins using external libs without `py_modules/` wait until an optional compat `venv` is needed. Enables full e2e testing of real plugins inside `zerodeckd` right away. | **Selected** |
| **B. Option A + Compat `venv` + `remote_binary` Downloader** | Also implement managed compatibility `venv` and HTTP `remote_binary` downloading inside Phase 2. | Pulls Phase 3 HTTP download/install complexity and Python packaging edge cases into Phase 2 before the installer and store routes exist. | Rejected |
| **C. Isolated `zerodeck-plugin-host` + `zerodeck-py-host` Only** | Keep `zerodeckd` and `zerodeck-server` unchanged until Phase 3. | Smaller diff, but leaves `zerodeckd` unable to run or test any real plugin end-to-end (`just dev`) until Phase 3. | Rejected |

---

## 4. Architecture & Crate-by-Crate Design

The acyclic workspace dependency graph in `Cargo.toml` stays unchanged:

```text
zerodeckd
 ├── zerodeck-server ── zerodeck-core, zerodeck-cef, zerodeck-plugin-host, zerodeck-protocol
 ├── zerodeck-cef ───── zerodeck-protocol
 ├── zerodeck-core ──── zerodeck-platform, zerodeck-plugin-host, zerodeck-protocol
 └── zerodeck-plugin-host ── zerodeck-platform, zerodeck-protocol
```

### 4.1 `zerodeck-platform` (Privilege & Directory Ownership)
- **`HostUserIdentity` (`#[cfg(unix)]`):** *(Spec call — unprivileged bypass & fail-closed root resolution)*
  - **When `current_process_identity().euid != 0`** (macOS dev, unprivileged Docker `dev`/`runtime` containers): bypass `/etc/passwd` lookup entirely and construct `HostUserIdentity` directly from `current_process_identity()` (`uid`, `gid`), `DECKY_USER` / `USER` env, and `PlatformPaths::user_home()`. Both `chown` and `setuid`/`setgid`/`setgroups` are skipped when `euid != 0`.
  - **When `current_process_identity().euid == 0`** (e.g., `deploy/systemd/zerodeck.service` running as `User=root`, where `HOME` may be `/root` and `DECKY_USER` unset): resolve a non-root user strictly in order:
    1. `DECKY_USER` env (if non-empty) → `/etc/passwd` lookup by username (must have `uid != 0`).
    2. `DECKY_USER_HOME` env (if non-empty) → `stat` directory owner; **skip `uid == 0`** (e.g. `/root`), otherwise look up UID in `/etc/passwd`.
    3. `"deck"` username in `/etc/passwd` (must have `uid != 0`), or owner of `/home/deck` (`uid != 0`).
    4. If no non-root target user can be resolved while `euid == 0`, **fail startup closed** with `PlatformError` rather than falling back to `uid = 0` or `nobody`.
  - Parse `/etc/passwd` + `/etc/group` in pure Rust (zero `unsafe`, works identically in minimal containers without `libc` NSS FFI), using `rustix` for `chown`/`setgroups`/`setgid`/`setuid` syscalls.
- **`ensure_owned_dir(path, target_uid_gid)` (`#[cfg(unix)]`):**
  - Creates directory (`create_dir_all`) and, only when `current_process_identity().euid == 0`, sets ownership (`chown`) of the target directory (and `0o700` for private runtime/socket directories) to `(uid, gid)`. When running unprivileged (`euid != 0`), skips `chown` cleanly.

### 4.2 `zerodeck-plugin-host` (`socketpair`, `pre_exec`, Supervisor)
- **Spawning (`UnixStream::pair()` + `pre_exec`):**
  - Create an anonymous connected `std::os::unix::net::UnixStream::pair()`.
  - Keep the parent end (set non-blocking, wrap in `tokio::net::UnixStream` split into `OwnedReadHalf` / `OwnedWriteHalf`).
  - Pass the child raw FD via `--fd <raw_fd>` to `python3 -m zerodeck_py_host`.
  - In `CommandExt::pre_exec` (marked `// SAFETY:`):
    - Clear `FD_CLOEXEC` on `child_fd` (`fcntl_setfd` without `FD_CLOEXEC`) using raw syscall / `rustix::io::fcntl_setfd` (pre-computed `RawFd`, no allocations).
    - If `PrivilegeTarget::HostUser` and daemon `euid == 0`, call pre-resolved `setgroups`, `setgid`, `setuid` in strict order (drop groups and GID before UID!).
    - Set process group (`setpgid(0, 0)`) or `PR_SET_PDEATHSIG(SIGTERM)` on Linux (`#[cfg(target_os = "linux")]`) so orphaned plugin children cannot outlive a crashed daemon.
  - Clear inherited env with `cmd.env_clear()`, inject minimal safe base vars (`PATH`, `LANG`, `LC_ALL`, `TERM`, `TZ`, `SSL_CERT_FILE`, `SSL_CERT_DIR` if present in daemon env) plus `build_plugin_env(spec, paths)` (note: `build_plugin_env` sets `HOME`, `USER`, `DECKY_*`; `PYTHONPATH` includes `host_module_path` when configured), and set `cmd.current_dir(&plugin_dir)`.
  - Also retain `--socket-path` builder support with a `0700` chowned directory under `<privileged>/services/.ipc/<plugin>/` (or `$XDG_RUNTIME_DIR`) for non-FD fallback scenarios.
- **IPC Framing & Error Semantics:**
  - Read newline-delimited JSON from the parent socket half with a bounded line length (`4 MiB`, returning `HostError::FrameTooLarge` if exceeded without OOM).
  - Correlate `PluginIpcFromWorker::Response { id, success, res, error }` against `pending: Mutex<HashMap<String, oneshot::Sender<Result<Value, HostError>>>>`.
  - Add `HostError::PluginCallFailed { name: String, message: String, traceback: Option<String> }` and `HostError::WorkerStopped` (used when the worker exits/crashes while calls are pending, mapped by `zerodeck-server` to `WsServerMessage::Discard`).
  - Forward `PluginIpcFromWorker::Event { event, args }` through an `mpsc` / `broadcast` channel tagged with the plugin's `manifest.name` (`PluginEvent { plugin: String, event: String, args: Vec<Value> }`).
  - Pipe child `stdout` and `stderr` via `Stdio::piped()` into background reader tasks logging each line via `tracing` (`target: "zerodeck_plugin_host::worker"`, fields `plugin = %name`, `stream = "stdout"|"stderr"`, bounded line length).
- **Supervision, Backoff & Shutdown:** *(Spec call — call readiness & non-running state mapping)*
  - Plugin states: `Stopped`, `Starting`, `Running`, `BackingOff`, `Failed` (crash-loop tripped), `Stopping`.
  - **Call behavior by state:**
    - `Starting`: wait up to a bounded startup readiness window (e.g. `15s`) for the worker to reach `Running` (so startup calls made while `_migration`/`_main` execute succeed seamlessly), failing with `HostError::WorkerStopped` if it exits or times out.
    - `BackingOff` or `Stopped` (worker died mid-call or is waiting to respawn): fail fast with `HostError::WorkerStopped` → mapped on `/ws` to `WsServerMessage::Discard { id }`.
    - `Failed` (crash-loop tripped), `Disabled`, `Passive` (no `main.py`), or unknown plugin: fail fast with a typed `HostError` → mapped on `/ws` to `WsServerMessage::Error { id, error: WsErrorDetail { name, error, traceback: None } }`.
  - On unexpected exit (when not shutting down):
    - Fail all in-flight `pending` calls with `HostError::WorkerStopped`.
    - Record exit timestamp; if `>= 5` exits occur within a `60s` sliding window, transition to `Failed` and emit an error log (no infinite restart storm).
    - Otherwise transition to `BackingOff` and respawn after exponential delay (`1s`, `2s`, `4s`, `8s`, `16s`, capped at `30s`), resetting the failure counter once the worker stays up past a `30s` stability window.
  - On `shutdown(uninstall)`:
    - Stop auto-restart; write `PluginIpcToWorker::Control { uninstall }` (if `uninstall == true`); send `SIGTERM` to child (or close stdin/IPC write half / signal process group); wait up to `5s` (`shutdown_timeout`) for clean exit; escalate to `SIGKILL` if still alive; fail any remaining `pending` calls with `HostError::WorkerStopped`.
- **Traits & Testability:**
  - Keep `PluginRuntime` object-safe (`call_method`, `shutdown`, plus status inspection on `PluginSupervisor`) so `zerodeck-server` unit tests can still swap in mock runtimes without spawning OS processes.

### 4.3 `hosts/python` (`zerodeck-py-host` Parity & Bug Fixes)
- **Clean-room `settings` module (`zerodeck_py_host/settings.py`, aliased as `sys.modules["settings"]`):**
  - Class `SettingsManager(name: str, settings_directory: str | Path | None = None, default_settings: dict[str, Any] | None = None)`:
    - Defaults `settings_directory` to `os.getenv("DECKY_PLUGIN_SETTINGS_DIR", "")`.
    - File path is `Path(settings_directory) / f"{name}.json"`.
    - Automatically calls `read()` in `__init__` (if file exists, loads JSON object; if absent, initializes from `default_settings` copy; if corrupt JSON, logs error and preserves the on-disk file rather than silently overwriting it).
    - Methods: `read() -> dict[str, Any]`, `commit() -> None` (atomic temp file in same directory + `flush()` + `os.fsync()` + `os.replace()`), `getSetting(key: str, default: Any = None) -> Any`, `setSetting(key: str, value: Any) -> None` (updates dict and calls `commit()`), plus `.settings` attribute (`dict[str, Any]`) and `.path` attribute.
- **Clean-room `helpers` module (`zerodeck_py_host/helpers.py`, aliased as `sys.modules["helpers"]`):**
  - `get_ssl_context() -> ssl.SSLContext`: builds `ssl.create_default_context()`, consulting `SSL_CERT_FILE` / `SSL_CERT_DIR` and common system CA bundle paths (`/etc/ssl/certs/ca-certificates.crt`, `/etc/pki/tls/certs/ca-bundle.crt`, macOS/OpenSSL defaults) if present.
  - `get_homebrew_path() -> str`: returns `os.getenv("DECKY_HOME", "")`.
  - `get_user() -> str`: returns `os.getenv("DECKY_USER", os.getenv("USER", ""))`.
- **Fixes in `runner.py` and `decky_api.py`:**
  - **`api_version == 0` dispatch:** Inspect class attribute with `inspect.getattr_static(self.plugin, method_name)` (for RPCs and lifecycle hooks `_migration`, `_main`, `_unload`, `_uninstall`):
    - If `isinstance(raw, staticmethod)` or `isinstance(raw, classmethod)`: call `getattr(self.plugin, method_name)(...)` (bound cleanly for classmethod, raw function for staticmethod) **without** injecting an extra `self.plugin` positional arg.
    - Otherwise (plain function on class): call `method(self.plugin, ...)` as legacy v0 expects.
  - **Blocking sync methods:** When `not inspect.isawaitable(result)` — wait: calling `method(...)` synchronously already ran the sync body before returning `result`! To keep blocking sync methods from stalling the loop, detect `not inspect.iscoroutinefunction(target_fn)` (Wait: a wrapper returning a coroutine or `async def` returns an awaitable, whereas a regular sync `def` blocks inside `method(...)`!). So inspect `callable_obj` *before* calling it: if `inspect.iscoroutinefunction(callable_obj)`, invoke directly on the event loop; otherwise run via `await asyncio.to_thread(callable_obj, ...)` and if the returned value still happens to be awaitable (e.g. a sync factory returning a coroutine), await it on the loop!
  - **`py_modules/` precedence:** If `Path(plugin_dir, "py_modules").is_dir()`, `sys.path.insert(0, py_modules_dir)` (if not already present) so plugin-vendored modules take precedence over system packages.
  - **Log file uniqueness (`decky_api.py`):** Keep the timestamp format `%Y-%m-%d %H.%M.%S` and disambiguate with a counter suffix if the file already exists (rapid restarts within 1 second).
  - **Lifecycle error resilience:** If `_migration` or `_main` raises an exception, log traceback via `decky.logger.exception(...)` (and fail startup cleanly rather than hanging).

### 4.4 `zerodeck-core`, `zerodeck-server` & `zerodeckd` Integration
- **`PluginManager` (`zerodeck-core`) & `settings/loader.json` Schema:** *(Spec call — `loader.json` keys)*
  - Owns `PlatformPaths`, `SettingsManager` (`loader.json`), plugin registry (`DiscoveredPlugin` + runtime state/order/disabled status), and `PluginSupervisor` (`zerodeck-plugin-host`).
  - `loader.json` keys (matched by `manifest.name`, consistent with existing `pluginOrder` tests in `zerodeck-core`):
    - `"pluginOrder"`: optional `Vec<String>` of plugin names; `list_plugins()` emits listed plugins in `pluginOrder` first, followed by any remaining discovered plugins in deterministic directory order.
    - `"disabledPlugins"`: optional `Vec<String>` of disabled plugin names (also accepts legacy/snake-case `"disabled_plugins"` when `"disabledPlugins"` is absent; writes `"disabledPlugins"` on update).
  - Methods: `start_all()`, `call_plugin_method(plugin, method, args, legacy)`, `reload_plugin(name)`, `list_plugins()`, `subscribe_events()`, `shutdown_all()`.
  - Skips spawning disabled plugins and passive plugins (`has_entrypoint == false`).
- **Async `/ws` & Routes (`zerodeck-server`):**
  - `AppState` holds `Arc<PluginManager>` (or `Arc<dyn PluginController>` / `PluginManager` handle) and `broadcast::Sender<WsServerMessage>` (capacity e.g. 256).
  - `handle_ws_socket` subscribes to the event broadcast channel inside its `tokio::select!` loop and forwards `WsServerMessage::Event` frames to the client.
  - `handle_ws_text` spawns each `Call` and each `FullSync` message via `tokio::spawn` sending its `WsServerMessage` (`Reply`, `Error`, or `Discard`) into `outbox`.
  - Wire routes implemented in `zerodeck-server`:
    - `loader/get_plugins`: returns array of `{ name, version, load_type, disabled }` reflecting live `PluginManager` state and `pluginOrder`.
    - `loader/call_plugin_method`: expects `args = [plugin_name, method_name, ...positional_args]`; rejects empty/private (`_`-prefixed) method names with a structured `WsErrorDetail`; dispatches `legacy: false`.
    - `loader/call_legacy_plugin_method`: expects `args = [plugin_name, method_name, kwargs_obj?]`; rejects private (`_`-prefixed) method names; dispatches `legacy: true` with `Value::Object` kwargs (default `{}`).
    - `loader/reload_plugin`: expects `args = [plugin_name]`; restarts the plugin worker cleanly (handy for testing & development).
    - `utilities/ping`: returns `"pong"`.
    - `POST /methods/{method_name}`: *(Spec call — HTTP legacy contract)*
      - Retains `"ping"` and `"get_plugins"` (backed by live `PluginManager`).
      - Supports `"call_plugin_method"` accepting JSON `{ "plugin_name": string, "method_name": string, "args"?: object | array, "legacy"?: bool }`. When `"legacy"` is omitted, infers `legacy = (plugin.manifest.api_version == 0)` and defaults omitted `args` to `{}` (legacy) or `[]` (v1).
      - HTTP response contract: `200 OK` `{ "success": true, "result": <val> }` on success; `400 Bad Request` for malformed body or private `_` method; `404 Not Found` for unknown plugin or unknown `/methods/{name}`; `503 Service Unavailable` when disabled, passive, `Failed`, or `WorkerStopped`; `200 OK` `{ "success": false, "result": { "name": ..., "error": ..., "traceback": ... } }` on Python runtime exception.
  - Worker `PluginEvent { plugin, event, args }` is published on the broadcast channel as:
    `WsServerMessage::Event { event: "loader/plugin_event".to_owned(), args: vec![json!({ "plugin": plugin, "event": event, "args": args })] }`.
- **`zerodeckd` Wiring:**
  - Keeps `--check-config` fast and side-effect-free (validates paths, `loader.json`, `discover_plugins`, does **not** spawn plugin processes).
  - In normal daemon mode: initializes `PluginManager`, starts active plugins via `PluginSupervisor` (passing `PythonHostConfig` with `PYTHONPATH` from environment or default module discovery), runs `serve(...)`, and on `shutdown_signal()` drains HTTP/WS and shuts down all plugin workers cleanly before exiting.

### 4.5 Documentation & Project Hygiene Updates
- Update [docs/architecture/wire-protocol.md](file:///Users/ygorsimoes/Documents/projects/ygorsimoes/zerodeck/docs/architecture/wire-protocol.md) to document:
  - `loader/call_plugin_method`, `loader/call_legacy_plugin_method`, and `loader/plugin_event` payload formats.
  - Python worker `settings` (`SettingsManager`), `helpers` (`get_ssl_context`, `get_homebrew_path`, `get_user`), `cwd` = `DECKY_PLUGIN_DIR`, and `stdout`/`stderr` capture.
  - `DISCARD (2)` behavior when a plugin worker exits mid-call.
- Update [README.md](file:///Users/ygorsimoes/Documents/projects/ygorsimoes/zerodeck/README.md) capability table and Phase 1 / Phase 2 roadmap checkboxes once Phase 2 lands.

---

## 5. Verification Strategy (Docker-First & Non-Root CI Safe)

- **Privilege & `chown` tests:** Unit tests for `/etc/passwd` + `/etc/group` parsing and `PrivilegeTarget` resolution run everywhere; integration tests that actually invoke `setuid`/`chown` check `current_process_identity().euid == 0` so they never fail inside unprivileged `cap_drop: ALL` dev/CI containers.
- **Rust ↔ Python IPC integration tests:** Spawn `python3 -m zerodeck_py_host` over a real `socketpair()` (`--fd`) covering:
  - `api_version = 0` (`@classmethod`, `@staticmethod`, plain method, kwargs, `decky_plugin`).
  - `api_version = 1` (`from settings import SettingsManager`, `from helpers import get_ssl_context`, `py_modules/` precedence, `decky.emit` → `PluginEvent`, blocking sync method via `asyncio.to_thread` not stalling `emit` or concurrent calls).
  - Exception traceback propagation (`WsErrorDetail`).
  - Crash recovery (`DISCARD` on in-flight call, backoff restart, crash-loop trip to `Failed`).
  - Graceful shutdown timeout & `Control { uninstall: true }` triggering `_unload` + `_uninstall`.
- **Daemon E2E tests (`crates/zerodeckd/tests/daemon.rs`):** Start `zerodeckd` with a temp homebrew directory containing a real Python fixture plugin, fetch `/auth/token`, connect to `/ws?auth=...`, invoke `loader/call_plugin_method`, receive `REPLY` and `loader/plugin_event`, and verify clean `SIGTERM` shutdown reaps the child Python worker.
- **Standard gate:** `just check` (and `just ci` if infra/Dockerfile changes).

---

## 6. Left to the Builder

- Exact internal struct/actor layout inside `zerodeck-plugin-host` and `zerodeck-core` (as long as `PluginRuntime` and the acyclic crate graph hold).
- Exact numeric tuning constants for supervisor backoff (`1s..30s`), crash-loop window (`5` crashes in `60s`), stability reset (`30s`), shutdown grace timeout (`5s`), and stdout/stderr log line cap.
- Whether `zerodeckd` resolves default `PYTHONPATH` for `zerodeck_py_host` automatically relative to the binary / repo checkout when `PYTHONPATH` is unset (helpful for `cargo run` outside Docker).
