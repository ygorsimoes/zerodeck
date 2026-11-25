# ZeroDeck Phase 3 — Full Server, FilePicker & Plugin Installer Design

**Date:** 2026-11-24  
**Status:** Draft — Pending Approval  
**Scope Size:** Project (Phase 3 — Full Server & Installer Lifecycle)  
**Prerequisites:**  
- [2026-11-24-zerodeck-foundation-design.md](2026-11-24-zerodeck-foundation-design.md)  
- [2026-11-24-phase2-plugin-host-design.md](2026-11-24-phase2-plugin-host-design.md)

---

## 1. Intent & Why

Phase 1 and Phase 2 delivered ZeroDeck's clean-room protocol contracts (`zerodeck-protocol`), `SafePath` / `SafePathSet` jails and privilege resolution (`zerodeck-platform`), Zip-Slip/SHA-256 archive primitives and `PluginManager` (`zerodeck-core`), `socketpair()` worker supervision (`zerodeck-plugin-host`), stdlib-only Python runtime parity (`zerodeck-py-host`), and `/ws` RPC dispatch (`zerodeck-server`).

However, `zerodeckd` currently operates as a static-startup runtime:
- `zerodeck-server` only serves `/healthz`, `/auth/token`, `/ws`, and `/methods/{ping,get_plugins,call_plugin_method}`; static loader assets (`/frontend/*`, `/static/*`, `/locales/*`), plugin HTTP routes (`/plugins/{name}/...`), and the `/fetch` CORS proxy return `404` despite `auth.rs` already defining their CSRF perimeter.
- `FULL_SYNC` on `/ws` replays calls blindly on reconnect with no unacknowledged reply cache or in-flight deduplication.
- `SafePathSet` and `PlatformPaths::filepicker_roots()` have no `utilities/filepicker` implementation yet.
- `PluginManager` only discovers plugins at daemon startup; there is no remote/local plugin installer, `remote_binary` downloader, dynamic `enable`/`disable`/`uninstall`/`pluginOrder` mutation, or `"debug"` file-watcher hot-reload.
- `zerodeckd` does not resolve the packaged `frontend/dist` directory yet (even though `Dockerfile` already installs it at `/opt/zerodeck/frontend`).

Phase 3 completes the full `zerodeck-server` HTTP/WebSocket surface and `zerodeck-core` installer/lifecycle engine so `zerodeckd` exposes every backend contract required by existing Decky plugins and the upcoming Steam CEF integration in Phase 4.

---

## 2. Goals, Non-Goals, and Anti-Goals

### Goals
1. **Static & Plugin HTTP Asset Routes (`zerodeck-server` + `zerodeck-core` + `zerodeckd`):**
   - Resolve `frontend_dir` in `zerodeckd` (`ZERODECK_FRONTEND_DIR` → `/opt/zerodeck/frontend` → `<exe>/../frontend` → `frontend/dist` → `<CARGO_MANIFEST_DIR>/../../frontend/dist`) and serve `/frontend/*`, `/static/*`, and `/locales/*` through `SafePath`.
   - Serve `/plugins/{name}/dist/*`, `/plugins/{name}/assets/*`, `/plugins/{name}/data/*`, `/plugins/load_main/{name}`, and `/plugins/{name}/frontend_bundle` (`GET`/`HEAD`), resolving `{name}` cleanly against `manifest.name` or `directory` and validating relative paths through per-plugin `SafePath` jails.
2. **SSRF-Guarded Streaming `/fetch` CORS Proxy (`zerodeck-server` + `zerodeck-core`):**
   - Implement `/fetch?auth=<token>&fetch_url=<url>` (accepting `?auth=` or `X-Decky-Auth` via existing `csrf_middleware`), forwarding method, request body, and `X-Decky-Header-*` headers while honoring `X-Decky-Fetch-Excluded-Headers` and streaming response bytes back via `reqwest` without buffering large payloads in RAM.
   - Enforce an outbound URL/SSRF guard shared with the installer: `http`/`https` only; block loopback (`127.0.0.0/8`, `::1`, `localhost`), unspecified (`0.0.0.0/8`, `::`), and link-local/metadata (`169.254.0.0/16`, `fe80::/10`) targets (including across HTTP redirects), while permitting private LAN addresses (`10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `fc00::/7`) needed by home-network Decky plugins.
3. **Deduplicated `FULL_SYNC` & `RECEIVED_RESPONSE` Tracking (`zerodeck-server`):**
   - Add a bounded `WsSyncCache` keyed by `(call.id, route, canonical_args)` so reconnecting clients replaying `FULL_SYNC` either attach to an in-flight call or receive the cached `Reply`/`Error`/`Discard` without re-executing mutations, and entries clear promptly upon `RECEIVED_RESPONSE (3)` or TTL expiry.
4. **Decky-Compatible, Path-Jailed FilePicker (`zerodeck-core` + `zerodeck-server`):**
   - Implement `FilePicker` on top of `SafePathSet::from_existing(paths.filepicker_roots())` (`$HOME`, `/run/media`, `DECKY_HOME`), returning Decky-compatible `{ path, realpath, files: [{ name, path, realpath, isdir, size, modified }], truncated }` while enforcing extension/hidden/name filters and dropping any entry whose canonical symlink target escapes the allowed root set.
5. **Catalog-Agnostic Plugin Installer, `remote_binary` & Dynamic `PluginManager` (`zerodeck-core` + `zerodeck-server`):**
   - Implement all-or-nothing installation/upgrade from remote ZIP URLs (`http`/`https`) or local ZIP files (`file://` URIs and filesystem paths validated through `SafePathSet`), unwrapping optional single top-level archive directories, normalizing permissions (`0o755` dirs, `0o644` files, `0o755` executables even when `unix_mode` is absent and `UMask=0077`), enforcing `verify_sha256_policy` (fail-closed unless `developer.allow_unverified_installs` is enabled or local sideload has `user_confirmed`), and downloading `package.json` `remote_binary` assets into `<plugin_dir>/bin/<name>` (`0o755`, mandatory SHA-256) inside staging before swapping into place.
   - Add dynamic `PluginManager` mutations serialized by an async lifecycle lock: `install_plugin`, `uninstall_plugin` (runs `_unload` + `_uninstall`, deletes `plugins/<dir>`, preserves `settings/` and `data/` by default unless `purge_data = true`), `enable_plugin`, `disable_plugin`, `set_plugin_order`, and dotted-key `loader.json` settings get/set.
   - Expose matching WS routes (`utilities/filepicker`, `loader/install_plugin`, `utilities/install_plugin` alias, `loader/uninstall_plugin`, `loader/enable_plugin`, `loader/disable_plugin`, `loader/set_plugin_order`, `loader/get_settings`, `loader/set_setting`) and emit top-level loader change events.
6. **Debug Hot-Reload without `CC0-1.0` Dependencies (`zerodeck-core`):**
   - Watch active `"debug"`-flagged plugins in `plugins_dir` using `inotify` (`ISC`-licensed on Linux) + lightweight `mtime` polling (`~500 ms`, also covering non-Linux hosts and container bind-mounts), debouncing bursts to reload backend workers on `main.py`/`py_modules/`/manifest changes and emit `zerodeck/frontend_reloaded` on `dist/` changes.
7. **Clean-Room Documentation (`docs/architecture/wire-protocol.md` & `README.md`):**
   - Document all new HTTP and WebSocket contracts cleanly in `docs/architecture/wire-protocol.md` and update `README.md` status/roadmap once Phase 3 lands.

### Non-Goals (Deferred to Phase 4 or On-Demand)
- Steam CEF CDP discovery/injection, `instrumentation/resolve_scope`, QAM React UI, and SteamOS one-line install/update script (Phase 4).
- `updater/*` daemon self-update endpoints (Phase 4, alongside Steam Deck install/update tooling).
- Optional Python compatibility `venv` for third-party libraries like `aiohttp`/`certifi` when plugins omit `py_modules/` (still deferred until testing real plugins shows an unmet dependency).
- Embedded Store catalog client/caching for `plugins.deckbrew.xyz`: the frontend fetches Store listings (via `/fetch`) and passes the resolved ZIP URL + SHA-256 to `zerodeck-core`.

### Anti-Goals
- **Never** copy GPL code from `decky-loader` (`fetch`, `filepicker`, `installer`, or `loader` routes must be built clean-room from `docs/architecture/wire-protocol.md`).
- **Never** add dependencies outside `deny.toml` permissive OSI licenses (e.g. do not relax `deny.toml` for `notify`'s `CC0-1.0` license) or break the acyclic crate graph in `AGENTS.md`.
- **Never** serve a filesystem path without `SafePath`/`SafePathSet` canonicalization, allow `%2e%2e`/null-byte escapes, or write `0777` files/directories.
- **Never** leave a working plugin half-replaced if a ZIP download, SHA-256 check, or `remote_binary` fetch fails.
- **Never** hold synchronous `std::sync::Mutex` locks across `.await` points in `PluginManager` or `WsSyncCache`.

---

## 3. Approaches Considered

| Approach | Summary | Trade-offs | Decision |
|---|---|---|---|
| **A. Domain Centralized in `zerodeck-core` + Thin Transport in `zerodeck-server`** | Add `reqwest` (and `#[cfg(target_os = "linux")] inotify` + polling watcher) to `zerodeck-core` (`installer`, `filepicker`, `watcher`, `http_policy`, reactive `PluginManager` with async `lifecycle_lock`); keep `zerodeck-server` focused on HTTP/WS framing, static responses, `/fetch` streaming, and `WsSyncCache`. | Cleanly isolates filesystem/network domain logic for unit & CLI testing, avoids races between installer/watcher/RPC by coordinating through `PluginManager`, keeps `deny.toml` intact, and preserves the exact `AGENTS.md` crate dependency graph (`zerodeck-server` stays decoupled from `zerodeck-platform`). Requires splitting `PluginManager` async structural lock from fast sync state reads. | **Selected** |
| **B. Independent Services Orchestrated in `zerodeck-server`** | Keep `PluginInstaller`, `FilePicker`, and `DebugWatcher` separate from `PluginManager`, coordinating state transitions inside `zerodeck-server` handlers. | Smaller initial diff to `manager.rs`, but scatters disk/`loader.json`/supervisor coordination across WS handlers and risks races between concurrent `uninstall`, `reload`, and watcher events. | Rejected |
| **C. Keep `reqwest` Only in `zerodeck-server`, Pass Bytes to `zerodeck-core`** | Download remote ZIPs in `zerodeck-server` and pass byte buffers down to `zerodeck-core`. | `package.json` `remote_binary` specs are only discovered *after* extracting the ZIP inside `zerodeck-core`, forcing awkward callbacks across crates and buffering whole remote files in RAM instead of streaming. | Rejected |

---

## 4. Constraints & Security Invariants

1. **Acyclic Crate Graph (`AGENTS.md` §2):**
   - `zerodeck-protocol` retains **zero** internal `zerodeck-*` dependencies.
   - `zerodeck-platform` depends only on `zerodeck-protocol` and OS primitives.
   - `zerodeck-core` depends on `zerodeck-platform`, `zerodeck-plugin-host`, `zerodeck-protocol` (plus `reqwest` and Linux `inotify`).
   - `zerodeck-server` depends on `zerodeck-core`, `zerodeck-cef`, `zerodeck-plugin-host`, `zerodeck-protocol` (plus `reqwest`, `tokio-util` / `futures-util`). Keeping `SafePath`-backed file resolution and `http_policy` exposed through `zerodeck-core` preserves the exact dependency edges documented in `AGENTS.md` without adding a direct `zerodeck-platform` runtime dependency to `zerodeck-server`.
2. **License Compliance (`deny.toml` & `AGENTS.md` §1):**
   - `deny.toml` remains unchanged (`MIT`, `Apache-2.0`, `BSD-2-Clause`, `BSD-3-Clause`, `ISC`, `Zlib`, `Unicode-3.0`, `CDLA-Permissive-2.0`).
   - Any newly added workspace dependencies (`inotify` [`ISC`] on Linux, `tokio-util` [`MIT`] if used for `ReaderStream`) must pass `just deny`.
3. **`UMask=0077` Safety under `root` Daemon:**
   - When `zerodeckd` runs as `root` (`deploy/systemd/zerodeck.service`), newly created plugin directories and files must remain readable/executable by non-root `host_user` workers (`0o755` directories, `0o644` regular files, `0o755` executables), while `settings/<dir>`, `data/<dir>`, and `logs/<dir>` remain chowned to `(uid, gid)` for standard plugins via `ensure_owned_dir`.

---

## 5. Architecture & Crate-by-Crate Design

### 5.1 `zerodeck-core` — Archive Hardening, SSRF Policy, Installer, FilePicker, Manager & Watcher

#### A. Archive Permission Normalization & Top-Level Unwrapping (`archive.rs`)
- Update `write_entries` in `archive.rs` so that on Unix:
  - Every created directory explicitly gets `0o755` (rather than inheriting process `umask`, which breaks non-root workers under `UMask=0077`).
  - Every extracted regular file explicitly gets `0o755` when `unix_mode.is_some_and(|m| m & 0o111 != 0)` and `0o644` otherwise (even when `unix_mode` is `None`, such as DOS-created ZIPs).
- Keep `extract_zip_safely_with_limits` pure and unchanged in contract (so existing unit tests and `fuzz/fuzz_targets/zip_extract.rs` continue testing raw all-or-nothing extraction), and build wrapper-dir unwrapping + `remote_binary` staging on top of it in `installer.rs`.

#### B. Shared Outbound HTTP & SSRF Policy (`http_policy.rs`)
- Provide a shared outbound HTTP guard used by both `installer.rs` and `zerodeck-server`'s `/fetch` proxy:
  - **Scheme validation:** Allow `http` and `https` only (`Url::parse`, rejecting `file:`, `ftp:`, `data:`, empty hosts).
  - **IP / Host classification (`is_blocked_ip` / `validate_target_host`):**
    - Reject literal/resolved loopback (`127.0.0.0/8`, `::1`, `localhost` and `*.localhost`), unspecified (`0.0.0.0/8`, `::`), and link-local/metadata (`169.254.0.0/16`, `fe80::/10`) — protecting `:1337` and CEF `:8080`.
    - Explicitly allow private RFC1918 / ULA LAN ranges (`10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `fc00::/7`) so local-network Decky plugins keep working.
  - **DNS-rebinding & redirect defense:**
    - Resolve hostname via `tokio::net::lookup_host` before issuing a request, verify every returned `IpAddr` passes `is_blocked_ip == false`, and pin the validated addresses on the `reqwest::Client` (handling HTTP `301`/`302`/`303`/`307`/`308` redirects manually in a bounded loop of max `5` hops, re-validating every `Location` target) so neither initial DNS resolution nor redirects can hit `127.0.0.1` or `169.254.169.254`. *(Spec call — unit tests can supply an injectable `allow_loopback_for_tests` policy for local `TcpListener` mocks, never enabled in production `ServerConfig` or `PluginManager`.)*

#### C. Plugin Installer & `remote_binary` (`installer.rs`)
- **`InstallPluginRequest`:**
  - `source: InstallPluginSource`:
    - `RemoteZip { url: String, sha256: Option<String> }`
    - `LocalZip { path: PathBuf, sha256: Option<String>, user_confirmed: bool }`
  - `directory_hint: Option<String>` (optional directory override; sanitized).
  - `expected_name: Option<String>` (if specified, verified against parsed `plugin.json` `name`).
  - `expected_version: Option<String>` *(Spec call)*: if provided and `package.json` declares a `version`, verifies equality (failing closed on mismatch); if `package.json` omits `version`, does not fail.
- **End-to-end atomic staging flow:**
  1. **Fetch archive bytes (bounded):**
     - `RemoteZip`: validate URL via `http_policy`, stream response with `reqwest` up to `max_download_bytes` (default `128 MiB`, failing closed if exceeded or `Content-Length > max_download_bytes`).
     - `LocalZip`: resolve `path` through `SafePathSet::from_existing(paths.filepicker_roots())`, enforce `metadata.len() <= max_download_bytes`, and read bytes.
  2. **Verify archive SHA-256:**
     - Read `developer.allow_unverified_installs` (`bool`, default `false`) from `SettingsManager` (`loader.json`).
     - Call `verify_sha256_policy(&bytes, &install_source, allow_unverified)` (`CoreError::MissingSha256`, `CoreError::ConfirmationRequired`, or `CoreError::Sha256Mismatch`).
  3. **Extract into temporary staging directory:**
     - Create hidden temp staging dir inside `paths.plugins_dir()` (`..<uuid>.extract-staging`) with `0o755`.
     - Run `extract_zip_safely_with_limits` into a `raw/` child inside the temp dir.
     - Locate `plugin.json`: either at `raw/plugin.json` or inside a single top-level directory `raw/<wrapper>/plugin.json` (ignoring macOS `__MACOSX` metadata folder if present). If neither holds, fail cleanly and delete staging.
     - Parse `PluginManifest` (`plugin.json`) and optional `PackageManifest` (`package.json`); verify `expected_name` and `expected_version` (when `package.json` has `version`) if provided.
  4. **Download & verify `remote_binary` inside staging:**
     - For each `RemoteBinarySpec { name, url, sha256hash }` in `package.json`:
       - Validate `name` is a single relative filename (no `/`, `\`, `..`, or empty string) using `SafePath` anchored at `<plugin_stage>/bin`.
       - Validate `url` with `http_policy`, stream binary up to `max_binary_bytes` (default `256 MiB`).
       - Enforce `verify_sha256_policy(&bin_bytes, &InstallSource::Remote { sha256: spec.sha256hash.clone() }, allow_unverified)` (fail closed if hash missing/mismatched!).
       - Write `<plugin_stage>/bin/<name>` atomically and `chmod 0o755`.
  5. **Resolve target directory & swap cleanly:**
     - If an existing discovered plugin has the same `manifest.name`, treat as an **upgrade**: reuse its existing `directory` (stopping its running worker with `uninstall = false` *only after* staging + `remote_binary` succeed).
     - Otherwise derive target `directory` from `directory_hint` (if valid) → single ZIP wrapper folder name → sanitized `manifest.name` (ASCII alphanumeric/`-`/`_`/`.`, no leading `.`, no traversal, disambiguated if an unrelated folder already exists).
     - Ensure `paths.plugins_dir()` exists (`0o755`), atomically move staging into `<plugins_dir>/<directory>` (with backup rollback if upgrading an existing install), refresh in-memory plugin list, and start the worker if enabled and `!passive`.
  - Return `InstallOutcome { name, directory, version, sha256, replaced_existing }`.

#### D. Decky-Compatible, Path-Jailed FilePicker (`filepicker.rs`)
- **`FilePickerQuery`:** *(aligned with Decky `@decky/api` contract)*
  - `path: Option<String>` (empty/omitted defaults to `paths.user_home()`).
  - `include_files: bool` (default `true`).
  - `include_folders: bool` (default `true`).
  - `filter: Option<String>` (optional case-insensitive filename filter; bounded pattern matching without ReDoS risk).
  - `extensions: Vec<String>` (case-insensitive extension match applied to files, ignored for directories; empty = all).
  - `show_hidden: bool` (default `false`; hides `.`-prefixed entries).
  - `max_entries: usize` (default `5_000`, capped at `20_000`).
- **Response (`FilePickerListing` & `FilePickerEntry`):**
  - `FilePickerListing`: `{ path: String, realpath: String, files: Vec<FilePickerEntry>, truncated: bool }`.
  - `FilePickerEntry`: `{ name: String, path: String, realpath: String, isdir: bool, size: u64, modified: f64 }` (where `isdir` has `size = 0`, and `modified` is Unix epoch seconds as `f64`, matching Decky's wire schema).
- **Behavior:**
  - Construct `SafePathSet::from_existing(paths.filepicker_roots())`. *(Spec call — uses `from_existing` so missing optional roots like `/run/media` on macOS or containers never fail initialization.)*
  - Validate requested `path` with `safe_set.resolve(target)` — rejecting non-existent paths or any traversal/symlink outside roots (`PlatformError::PathEscape`).
  - Verify target is a directory; iterate `fs::read_dir(&canonical_dir)`:
    - Skip `.`-prefixed names when `!show_hidden`.
    - For each entry, resolve `safe_set.resolve(entry.path())`; if an entry is a broken symlink or points outside `SafePathSet`, **skip it** rather than leaking external targets or failing the whole listing.
    - Apply `include_files`/`include_folders`, `extensions`, and `filter` cleanly.
    - Sort directories before files, then lexicographically case-insensitive (`to_lowercase()`), and truncate to `max_entries` setting `truncated: true`.

#### E. Reactive `PluginManager`, Dotted Settings & Loader Events (`manager.rs` + `settings.rs`)
- **Concurrency model:**
  - Add `lifecycle_lock: tokio::sync::Mutex<()>` to serialize `start_all`, `install_plugin`, `uninstall_plugin`, `enable_plugin`, `disable_plugin`, `reload_plugin`, and `set_plugin_order`.
  - Keep `plugins: Mutex<Vec<DiscoveredPlugin>>` and `settings: Mutex<SettingsManager>` synchronous and short-lived (never held across `.await`).
  - Add `paths() -> &PlatformPaths` and `allow_unverified_installs() -> bool` helpers.
- **Loader-level event channel (`LoaderEvent`):** *(Spec call — separates loader-level events from worker `decky.emit`)*
  - Keep `PluginSupervisor`'s `PluginEvent` strictly for worker `decky.emit(...)` events (wrapped into `loader/plugin_event` by `zerodeck-server`).
  - Add `loader_events_tx: broadcast::Sender<LoaderEvent>` on `PluginManager` (`subscribe_loader_events()`) for top-level server events:
    - `LoaderEvent::PluginsChanged(Vec<PluginSummary>)`
    - `LoaderEvent::PluginReloaded { plugin: String, directory: String }`
    - `LoaderEvent::FrontendReloaded { plugin: String, directory: String }`
- **New `PluginManager` methods:**
  - `install_plugin(&self, req: InstallPluginRequest) -> Result<InstallOutcome, CoreError>` (emits `PluginsChanged`).
  - `uninstall_plugin(&self, plugin_name: &str, purge_data: bool) -> Result<(), CoreError>`:
    - Calls `supervisor.shutdown_plugin(&manifest_name, true)` if active (running `_unload` + `_uninstall`), removes `<plugins_dir>/<dir>`, removes `<settings_dir>/<dir>` and `<data_dir>/<dir>` only when `purge_data == true`, updates in-memory `plugins`, strips `manifest_name` from `pluginOrder` / `disabledPlugins` in `loader.json`, and emits `PluginsChanged`.
  - `enable_plugin(&self, plugin_name: &str) -> Result<(), CoreError>` / `disable_plugin(&self, plugin_name: &str) -> Result<(), CoreError>`:
    - Persists `disabledPlugins` atomically in `loader.json` (migrating/removing legacy `disabled_plugins` key on write); stops active worker on disable (`uninstall = false`) or spawns worker on enable if `!passive`; emits `PluginsChanged`.
  - `set_plugin_order(&self, order: Vec<String>) -> Result<(), CoreError>`:
    - Persists `pluginOrder` atomically in `loader.json` and emits `PluginsChanged`.
  - **Dotted-key `loader.json` settings (`SettingsManager` + `PluginManager`):** *(Spec call)*
    - `get_setting_path(&self, dotted_key: &str) -> Option<Value>` traverses dotted segments (e.g. `"developer.allow_unverified_installs"` or any `"a.b.c"` path), returning `None` (`null` on WS) when absent; `PluginManager::allow_unverified_installs()` treats absent/non-bool as `false` (fail-closed).
    - `set_setting_path(&mut self, dotted_key: &str, value: Value) -> Result<(), CoreError>` creates missing intermediate JSON objects and rejects overwriting a non-object intermediate value (e.g., if `"developer"` is a scalar) without touching disk.
    - `PluginManager::set_setting` rejects direct writes to managed lifecycle keys (`"pluginOrder"`, `"disabledPlugins"`, `"disabled_plugins"`) with a clear error directing callers to `set_plugin_order` / `enable_plugin` / `disable_plugin` so worker state and `loader.json` cannot diverge.
  - **Safe HTTP file resolution (`resolve_plugin_file(&self, plugin_id: &str, scope: PluginFileScope, rel_path: &str) -> Result<PathBuf, CoreError>`):**
    - `PluginFileScope`: `Dist`, `Assets`, `Data`, `FrontendBundle`.
    - Resolves `plugin_id` by matching `manifest.name` first, falling back to `directory`.
    - Anchors `SafePath::new(root)` to:
      - `Dist`: `<plugins_dir>/<dir>/dist`
      - `Assets`: `<plugins_dir>/<dir>/assets`
      - `Data`: `<data_dir>/<dir>`
      - `FrontendBundle`: `<plugins_dir>/<dir>/dist/index.js` (`SafePath::new(<plugins_dir>/<dir>/dist).resolve("index.js")`)
    - Returns canonical `PathBuf` verified inside that plugin's jail.
  - **Loader static root resolution (`resolve_static_file`):**
    - Anchors `SafePath::new(frontend_dir)` for `/frontend/*`, `<frontend_dir>/static` for `/static/*`, and `<frontend_dir>/locales` for `/locales/*`.

#### F. Debug Hot-Reload Watcher (`watcher.rs`)
- Zero `CC0-1.0` dependencies: use `inotify` (`ISC`, `#[cfg(target_os = "linux")]`) combined with a lightweight periodic `mtime` sweep (`~500 ms` on non-Linux hosts, and ~`2s` fallback sweep on Linux to handle Docker Desktop bind-mounts over VirtioFS cleanly) scanning only subdirectories of active plugins where `manifest.is_debug() && !disabled`.
- Debounce bursts (`300 ms`), ignoring `.`-prefixed staging/backup dirs, `__pycache__/`, and `*.pyc`.
- On backend changes (`main.py`, `py_modules/`, `plugin.json`, `package.json`):
  - Re-read manifests if changed (refreshing `DiscoveredPlugin` metadata and emitting `LoaderEvent::PluginsChanged` if metadata changed), call `manager.reload_plugin(&name).await`, and emit `LoaderEvent::PluginReloaded { plugin, directory }`.
- On frontend bundle changes (`dist/`):
  - Emit `LoaderEvent::FrontendReloaded { plugin, directory }`.
- Dropping/stopping `DebugWatcherHandle` cleanly stops the background watcher task on daemon shutdown.

---

### 5.2 `zerodeck-server` — Full HTTP Surface, `/fetch` Proxy, `FULL_SYNC` Cache & WS Routes

#### A. `ServerConfig`, `AppState` & `WsSyncCache`
- Extend `ServerConfig` with:
  - `frontend_dir: Option<PathBuf>`
  - `max_zip_download_bytes: usize` (default `128 * 1024 * 1024`)
  - `max_remote_binary_bytes: usize` (default `256 * 1024 * 1024`)
  - `fetch_connect_timeout: Duration` (default `10s`) / `fetch_response_timeout: Duration` (default `60s`)
  - `ws_sync_cache_capacity: usize` (default `256`) / `ws_sync_cache_ttl: Duration` (default `300s`)
- Extend `AppStateInner` with:
  - `frontend_dir: Option<PathBuf>`
  - `http_policy: OutboundHttpPolicy` (non-loopback production SSRF guard)
  - `ws_sync_cache: Arc<WsSyncCache>`
- **`WsSyncCache` (`crates/zerodeck-server/src/ws_sync.rs`):** *(Spec call — clean-room replay deduplication)*
  - Key: `(call.id, call.route.clone(), canonical_args_json)` (prevents collisions between a fresh page load starting at `id = 1` and a reconnecting `WSRouter` replaying an in-flight call).
  - States per entry: `InFlight(watch/broadcast waiter)` or `Completed { frame: WsServerMessage, stored_at: Instant }`.
  - On inbound `CALL (0)`:
    - If identical key is already `InFlight` or `Completed` within TTL, returns/awaits the cached result instead of spawning duplicate work.
    - Otherwise marks `InFlight`, runs `dispatch_call`, stores `Completed { frame }` (evicting oldest entries when over capacity), and delivers `frame`.
  - On `FULL_SYNC (4)`:
    - Replays each message through the exact same lookup/join path: completed unacked calls reply immediately without re-execution; in-flight calls join the running task and forward its single result; unknown calls execute once.
  - On `RECEIVED_RESPONSE (3)`:
    - Evicts entries matching `id` from `WsSyncCache`.

#### B. HTTP Routes (`router.rs` & `auth.rs`)
- Update `auth.rs`:
  - Keep `ALLOWED_PUBLIC_PREFIXES`: `"/frontend/"`, `"/static/"`, `"/locales/"`, `"/steam_resource/"`, `"/plugins/load_main/"`.
  - Update `is_csrf_exempt` so `GET`/`HEAD` on `/plugins/{name}/frontend_bundle` is also allowed alongside `/plugins/{name}/{assets,data,dist}/*`, while continuing to reject `..` / `%2e%2e`, `\`, `%5c`, and `%00`.
- Add HTTP routes in `build_router`:
  - `GET /frontend/{*path}` → `SafePath` inside `frontend_dir`.
  - `GET /static/{*path}` → `SafePath` inside `<frontend_dir>/static` (returns `404` if `frontend_dir` unset or dir absent).
  - `GET /locales/{*path}` → `SafePath` inside `<frontend_dir>/locales`.
  - `GET /plugins/load_main/{name}` and `GET /plugins/{name}/frontend_bundle` → `resolve_plugin_file(name, FrontendBundle, "index.js")`, served as `text/javascript; charset=utf-8` with `Cache-Control: no-store`.
  - `GET /plugins/{name}/dist/{*path}` → `resolve_plugin_file(name, Dist, rel_path)`.
  - `GET /plugins/{name}/assets/{*path}` → `resolve_plugin_file(name, Assets, rel_path)`.
  - `GET /plugins/{name}/data/{*path}` → `resolve_plugin_file(name, Data, rel_path)`.
  - All file handlers percent-decode `{name}` and `{*path}`, reject empty/null/backslash/parent segments, stream file bytes (`tokio::fs::File` + `tokio_util::io::ReaderStream`), and map extensions to accurate MIME `Content-Type` headers (`.js`/`.mjs` → `text/javascript; charset=utf-8`, `.css`, `.json`, `.html`, `.svg`, `.png`, `.jpg`/`.jpeg`, `.webp`, `.woff2`, `.wasm`, fallback `application/octet-stream`).
- **`/fetch` CORS Streaming Proxy (`crates/zerodeck-server/src/fetch.rs`):**
  - Route: `any("/fetch", handle_fetch)` (protected by `csrf_middleware` requiring `?auth=<token>` or `X-Decky-Auth`).
  - Extracts `fetch_url` query parameter (`400 Bad Request` if missing/empty).
  - Validates `fetch_url` via `http_policy` (`400 Bad Request` on invalid scheme/URL, `403 Forbidden` when blocked by SSRF guard — loopback/link-local/metadata — including across redirects).
  - Header mapping:
    - Strips `host`, `x-decky-auth`, `connection`, `content-length`, `transfer-encoding`, `accept-encoding`, and any header names listed in `X-Decky-Fetch-Excluded-Headers` (comma-separated, case-insensitive).
    - Strips `cookie` unless an explicit `X-Decky-Header-Cookie` is supplied.
    - Unwraps every `X-Decky-Header-<Target>` header into `<Target>` on the upstream request.
  - Streams incoming request body to upstream `reqwest` request and streams upstream status + filtered headers (stripping upstream hop-by-hop headers and upstream `Access-Control-*` headers so `CorsLayer` remains authoritative) + `Body::from_stream(...)` back to the client.
  - Errors contacting upstream return `502 Bad Gateway` (or `504 Gateway Timeout` on timeout).

#### C. WebSocket Routes & Event Framing (`router.rs`)
- In `handle_ws_socket`, select over both `PluginManager::subscribe_events()` (worker `PluginEvent` wrapped as `loader/plugin_event`) and `PluginManager::subscribe_loader_events()` (`LoaderEvent` emitted as top-level `WsServerMessage::Event`):
  - `LoaderEvent::PluginsChanged(list)` → `{ "type": 5, "event": "loader/plugins_changed", "args": [list] }`
  - `LoaderEvent::PluginReloaded { plugin, directory }` → `{ "type": 5, "event": "zerodeck/plugin_reloaded", "args": [{ "plugin": plugin, "directory": directory }] }`
  - `LoaderEvent::FrontendReloaded { plugin, directory }` → `{ "type": 5, "event": "zerodeck/frontend_reloaded", "args": [{ "plugin": plugin, "directory": directory }] }`
- Add the following WS routes in `dispatch_call`:

| WS Route | `args` Wire Shape | Behavior / Result |
|---|---|---|
| `utilities/filepicker` | Positional `[path?, include_files?, filter?, extensions?, show_hidden?, max_entries?, include_folders?]` or 1-element options object `{ path?, include_files?, include_folders?, filter?, extensions?, show_hidden?, max_entries? }` | Calls `FilePicker::list` via `SafePathSet`; replies with `{ path, realpath, files: [{ name, path, realpath, isdir, size, modified }], truncated }`. |
| `loader/install_plugin` & `utilities/install_plugin` | Positional `[url_or_path, name?, version?, sha256?, install_type?, user_confirmed?]` or 1-element spec object `{ url_or_path (aliases: url, artifact, path), name?, version?, sha256 (alias: hash)?, install_type?, user_confirmed?, directory? }` | Unwraps `file://` URIs and local paths to `LocalZip` (via `SafePathSet`) or `http(s)://` to `RemoteZip` (`install_type` accepts `"remote"`/`"local"` or Decky `InstallType` ints `0`/`1`/`2`); runs `PluginManager::install_plugin`; replies with `InstallOutcome`. |
| `loader/uninstall_plugin` | `[plugin_name, purge_data?]` | Calls `PluginManager::uninstall_plugin` (default `purge_data = false`); replies `true`. |
| `loader/enable_plugin` | `[plugin_name]` | Calls `PluginManager::enable_plugin`; replies `true`. |
| `loader/disable_plugin` | `[plugin_name]` | Calls `PluginManager::disable_plugin`; replies `true`. |
| `loader/set_plugin_order` | `[plugin_order: string[]]` | Calls `PluginManager::set_plugin_order`; replies `true`. |
| `loader/get_settings` | `[]` or `[dotted_key?]` | Returns full `loader.json` object or dotted-key value (`null` when unset). |
| `loader/set_setting` | `[dotted_key, value]` | Atomically updates dotted key in `loader.json` (rejecting direct writes to `pluginOrder`, `disabledPlugins`, `disabled_plugins` with `ValueError`); replies `true`. |

- Map typed `CoreError` / `PlatformError` failures cleanly onto `WsErrorDetail` (`PathEscapeError`, `MissingSha256Error`, `Sha256MismatchError`, `ConfirmationRequiredError`, `ArchiveError`, `HttpPolicyError`, `PluginNotFoundError`, `ValueError`).

---

### 5.3 `zerodeckd` & Documentation Wiring

- **`crates/zerodeckd/src/main.rs`:**
  - Add `resolve_frontend_path()` searching `ZERODECK_FRONTEND_DIR`, `/opt/zerodeck/frontend`, `<exe_dir>/../frontend`, `frontend/dist`, and `<CARGO_MANIFEST_DIR>/../../frontend/dist`.
  - Pass `frontend_dir` into `ServerConfig`.
  - Start `DebugWatcherHandle` after `manager.start_all()` (only in daemon run mode, never in `--check-config` or `--healthcheck`) and shut it down cleanly alongside `manager.shutdown_all()` on `shutdown_signal()`.
- **Documentation:**
  - Update [docs/architecture/wire-protocol.md](../../architecture/wire-protocol.md) with all Phase 3 HTTP routes (`/frontend/*`, `/static/*`, `/locales/*`, `/plugins/*/dist/*`, `/plugins/*/assets/*`, `/plugins/*/data/*`, `/plugins/load_main/*`, `/plugins/*/frontend_bundle`, `/fetch`), `FULL_SYNC` + `RECEIVED_RESPONSE` cache semantics, top-level loader/reload events, and all new WebSocket routes.
  - Update [README.md](../../../README.md) capability table and Phase 3 roadmap checkbox once Phase 3 lands.

---

## 6. Verification Strategy (Docker-First & Non-Root CI Safe)

- **`zerodeck-core` unit & integration tests (offline local `TcpListener` / Axum mock HTTP servers, no external internet):**
  - `archive.rs` permission normalization test verifying extracted dirs are `0o755` and files `0o644`/`0o755` even with `unix_mode == None` under restrictive umask.
  - `http_policy.rs` unit tests blocking `127.0.0.1`, `::1`, `localhost`, `0.0.0.0`, `169.254.169.254`, `file://`, and HTTP 302 redirects into loopback, while allowing RFC1918 LAN IPs (`192.168.1.50`, `10.0.0.5`).
  - `installer.rs` tests against local mock HTTP server (`allow_loopback_for_tests` enabled only in test policy) + `file://`/local ZIP sideloads: remote SHA-256 fail-closed, `developer.allow_unverified_installs` opt-in, local sideload `ConfirmationRequired` vs confirmed, `expected_version` validation, top-level wrapper folder unwrapping, `remote_binary` into `bin/<name>` (`0o755`), rollback leaving previous working plugin untouched when `remote_binary` fails SHA-256.
  - `filepicker.rs` tests: `$HOME` + secondary root browsing, hidden/extension/name filters, `isdir`/`modified`/`size: 0` Decky schema parity, deterministic sort, `truncated`, and symlink-escape entry omission / `PathEscape` rejection.
  - `manager.rs` lifecycle & settings tests: `install` → call RPC → `disable` (worker stopped) → `enable` (worker restarted) → `set_plugin_order` → `uninstall` (`_uninstall` marker verified, `settings`/`data` preserved by default, removed when `purge_data = true`); dotted-key `set_setting` + rejection of direct `pluginOrder`/`disabledPlugins` writes.
  - `watcher.rs` test: modifying a `"debug"` plugin's `main.py` triggers debounced reload + `PluginReloaded` event; modifying `dist/` triggers `FrontendReloaded`; modifying a non-debug plugin does not.
- **`zerodeck-server` HTTP/WS integration tests:**
  - `/frontend/*`, `/static/*`, `/locales/*`, `/plugins/{name}/dist/*`, `/plugins/{name}/assets/*`, `/plugins/{name}/data/*`, `/plugins/load_main/{name}`, `/plugins/{name}/frontend_bundle`: `200 OK` with expected MIME type, `403 Forbidden` on `..` / `%2e%2e`, `404` on missing files, symlink-escape rejection.
  - `/fetch` tests: auth required (`403` without token), `400` on missing `fetch_url`, `X-Decky-Header-*` translation, `X-Decky-Fetch-Excluded-Headers`, streaming body passthrough, upstream `Access-Control-*` stripped, SSRF blocking `127.0.0.1` / `169.254.169.254` / redirect-to-loopback (`403`).
  - `FULL_SYNC` deduplication test: call invoked before WS drops completes server-side, reconnect sends `FULL_SYNC` and receives cached reply *without* re-executing a side-effect counter in Python, and `RECEIVED_RESPONSE` evicts the entry.
  - New WS routes (`utilities/filepicker`, `loader/install_plugin`, `utilities/install_plugin`, `loader/enable_plugin`, `loader/disable_plugin`, `loader/uninstall_plugin`, `loader/set_plugin_order`, `loader/get_settings`, `loader/set_setting`) and `loader/plugins_changed` events verified over real WS connection.
- **`zerodeckd` E2E test (`crates/zerodeckd/tests/daemon.rs`):**
  - Start real `zerodeckd` binary with `ZERODECK_FRONTEND_DIR`, fetch `/frontend/index.js`, sideload/install a ZIP plugin via `/ws`, invoke a plugin RPC, fetch `/plugins/{name}/assets/*`, disable/uninstall cleanly.
- **Standard gate:** `just check` (and `just ci` if `Cargo.toml`/`Cargo.lock`/deps change).

---

## 7. Left to the Builder

- Exact crate minor/patch versions for `inotify` (`#[cfg(target_os = "linux")]`) and `tokio-util` (if used for `ReaderStream`), provided they satisfy `deny.toml` and `cargo clippy -- -D warnings`.
- Exact internal struct names inside `http_policy.rs`, `installer.rs`, `filepicker.rs`, `watcher.rs`, `fetch.rs`, and `ws_sync.rs`, provided public crate boundaries and wire schemas hold.
- Exact `WsSyncCache` eviction/cleanup timing constants (bounded capacity ~256, TTL ~300s) and watcher debounce/polling constants (~300ms debounce, ~500ms non-Linux / ~2s Linux fallback mtime sweep).
- Exact HTTP response header pass-through allowlist/denylist in `/fetch` (stripping hop-by-hop headers and upstream CORS headers while preserving `Content-Type`, `Content-Length`, `ETag`, etc.).
