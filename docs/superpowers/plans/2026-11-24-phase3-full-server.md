# ZeroDeck Phase 3 — Full Server, FilePicker & Plugin Installer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Complete `zerodeck-server` (static/plugin routes, `/fetch` streaming CORS proxy, deduplicated `FULL_SYNC`, FilePicker and plugin-lifecycle WS routes) and `zerodeck-core` (catalog-agnostic ZIP/`remote_binary` installer, dynamic `PluginManager`, and hybrid `inotify`/polling debug hot-reload).

**Architecture:** `zerodeck-core` owns domain logic — shared outbound SSRF policy (`http_policy`), hardened archive permissions (`archive`), all-or-nothing ZIP + `remote_binary` staging (`installer`), path-jailed `FilePicker` (`filepicker`), async-serialized dynamic `PluginManager` + dotted-key `SettingsManager`, and `inotify`/polling `DebugWatcher` (`watcher`). `zerodeck-server` remains a thin HTTP/WS layer adding `WsSyncCache` (`FULL_SYNC`/`RECEIVED_RESPONSE`), `/fetch` streaming, MIME-typed `SafePath` file serving, and WS route dispatch, wired together by `zerodeckd`.

**Tech Stack:** Rust Edition 2024 (`1.100.0`, `tokio 1.54`, `axum 0.8`, `reqwest 0.13` w/ `rustls` + `stream`, `tokio-util 0.7` w/ `io`, `url 2.5`, `percent-encoding 2.3`, Linux `inotify 0.11` [`ISC`]), Docker-first `just check` / `just ci`.

**Spec:** [2026-11-24-phase3-full-server-design.md](file:///Users/ygorsimoes/Documents/projects/ygorsimoes/zerodeck/docs/superpowers/specs/2026-11-24-phase3-full-server-design.md)

## Global Constraints
- **Clean-room MIT only:** Zero GPL code copied from `decky-loader`; implement strictly against public contracts documented in `docs/architecture/wire-protocol.md`.
- **License allowlist unchanged (`deny.toml` / `AGENTS.md` §1):** Do **not** add `notify` (`CC0-1.0`) or relax `deny.toml`; use `inotify` (`ISC`) on Linux + pure-Rust `mtime` polling.
- **Acyclic crate graph (`AGENTS.md` §2):** `zerodeck-protocol` (zero internal deps) → `zerodeck-platform` → `zerodeck-plugin-host` → `zerodeck-core` → `zerodeck-server` → `zerodeckd` (do not add `zerodeck-platform` to `zerodeck-server`'s `[dependencies]`).
- **Rust quality:** `rustfmt.toml`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, minimize `unsafe`, never hold `std::sync::Mutex` across `.await`.
- **Verification:** `just check` and `just ci` (triggered by `Cargo.toml`/`Cargo.lock` changes) must pass before finishing.

## Review Focus
- **HTTP redirect SSRF bypass (`302`/`307` → `127.0.0.1` / `169.254.169.254`):** Every redirect hop (up to 5) in `/fetch` and remote installer downloads must re-run `http_policy` and fail closed (`403` / `CoreError::HttpPolicy`) — pinned by tests in Task 1 and Task 5.
- **DOS-style ZIPs (`unix_mode == None`) extracted by `root` daemon under `UMask=0077`:** Extracted dirs and files must explicitly receive `0o755` / `0o644` (never `0o700` / `0o600` unreadable by `deck`) — pinned by test in Task 1.
- **Partial upgrade failure during `remote_binary` fetch/hash check:** Staging must download and verify ZIP + all `remote_binary` entries before stopping the running worker or swapping directories so an existing plugin stays running untouched on failure — pinned by test in Task 3.
- **Missing `/run/media` root or symlink escaping `$HOME` in FilePicker:** `FilePicker` must use `SafePathSet::from_existing` (not `SafePathSet::new`) and silently skip directory entries whose symlink target escapes the allowed roots — pinned by test in Task 2.
- **`FULL_SYNC` double-execution vs. `id = 1` collision across page reloads:** `WsSyncCache` keyed by `(id, route, canonical_args)` must return cached replies on `FULL_SYNC` without re-running side effects, evict on `RECEIVED_RESPONSE`, and never collide when a distinct route/args arrives — pinned by test in Task 5.

---

### Task 1: Workspace Dependencies, Archive `UMask` Hardening & Outbound SSRF Policy (`zerodeck-core`)

**Files:**
- Modify: `Cargo.toml:25-53`
- Modify: `crates/zerodeck-core/Cargo.toml:13-25`
- Modify: `crates/zerodeck-core/src/error.rs:1-81`
- Modify: `crates/zerodeck-core/src/archive.rs:215-287`
- Create: `crates/zerodeck-core/src/http_policy.rs`
- Modify: `crates/zerodeck-core/src/lib.rs:1-20` (and tests)

**Interfaces:**
- Consumes: `reqwest`, `url`, `futures-util`, `zip`, `sha2`, `hex`, `tokio`.
- Produces:
  - `CoreError` variants: `HttpPolicy(String)`, `HttpRequest(String)`, `DownloadSizeExceeded { max_bytes: usize }`, `InvalidInstallSource(String)`, `ManifestMismatch { field: &'static str, expected: String, actual: String }`, `MissingPluginManifest`, `InvalidSetting(String)`, `Watcher(String)`
  - Normalized Unix `0o755` dir / `0o644` (`0o755` exec) file permissions in `extract_zip_safely_with_limits`
  - `http_policy::OutboundHttpPolicy` (`production()`, `with_allow_loopback_for_tests(bool)`, `validate_ip(IpAddr)`, `validate_url(&self, raw: &str) -> Result<Url, CoreError>`)
  - `http_policy::ValidatedResponse` + `http_policy::send_checked_request(client: &reqwest::Client, policy: &OutboundHttpPolicy, method: reqwest::Method, url: &str, headers: reqwest::header::HeaderMap, body: Option<reqwest::Body>, max_redirects: usize) -> Result<reqwest::Response, CoreError>` (validates DNS resolution + follows up to `max_redirects = 5` hops checking each `Location`)

- [ ] **Step 1: Write failing tests in `crates/zerodeck-core/src/lib.rs`**
  - `extract_zip_normalizes_permissions_without_unix_mode`: build a raw ZIP entry with `unix_mode` stripped (`SimpleFileOptions::default().unix_permissions(0)` / DOS creator metadata) containing a nested `sub/file.txt`, extract, and assert on Unix that `sub` is `0o755` (`& 0o777 == 0o755`) and `sub/file.txt` is `0o644`.
  - `http_policy_blocks_loopback_metadata_and_allows_private_lan`: assert `OutboundHttpPolicy::production()` rejects `http://127.0.0.1/x`, `http://[::1]:8080/json`, `http://localhost:1337/auth/token`, `http://sub.localhost/a`, `http://0.0.0.0/x`, `http://169.254.169.254/latest`, `file:///etc/passwd`, and accepts `https://plugins.deckbrew.xyz/a.zip`, `http://192.168.1.50:8096`, `http://10.0.0.5:8123`, `http://172.16.4.2`, `http://[fd00::1]/api`.
  - `http_policy_blocks_redirect_into_loopback`: bind an ephemeral mock HTTP server on `127.0.0.1:0` returning `302 Found` with `Location: http://127.0.0.1:1337/auth/token`; test with a custom policy permitting only the mock listener port or checking `validate_redirect_target` / `send_checked_request` rejects the redirect hop to `127.0.0.1:1337` with `CoreError::HttpPolicy`.

- [ ] **Step 2: Run tests to verify failure**
  Run: `cargo test -p zerodeck-core`
  Expected: FAIL (`http_policy` and new `CoreError` variants not defined).

- [ ] **Step 3: Add dependencies and implement `error.rs`, `archive.rs` permissions fix, and `http_policy.rs`**
  - In root `Cargo.toml`, add `futures-util` features cleanly, `inotify = "0.11.5"`, `percent-encoding = "2.3.2"`, `tokio-util = { version = "0.7.20", features = ["io"] }`, `url = "2.5.9"`.
  - In `crates/zerodeck-core/Cargo.toml`, add `futures-util`, `reqwest`, `url` and `[target.'cfg(target_os = "linux")'.dependencies] inotify.workspace = true`.
  - In `archive.rs` `write_entries`, explicitly `chmod 0o755` directories and `0o755` (executable) or `0o644` (regular/no `unix_mode`) files on Unix.
  - In `http_policy.rs`, use `url::Url::parse`, `tokio::net::lookup_host` before issuing each hop, `reqwest::redirect::Policy::none()` on the `Client` + manual bounded loop (max 5 hops) validating every `Location` URI and resolving/pinning allowed IPs (`resolve_to_addrs`) before connecting.

- [ ] **Step 4: Run tests, clippy, and license check**
  Run: `cargo test -p zerodeck-core --locked && cargo clippy -p zerodeck-core --all-targets --locked -- -D warnings && just deny`
  Expected: PASS with 0 warnings and clean `cargo-deny`.

- [ ] **Step 5: Commit**
  ```bash
  git add Cargo.toml Cargo.lock crates/zerodeck-core
  git commit -m "feat(core): harden archive permissions and add SSRF-guarded outbound HTTP policy"
  ```

---

### Task 2: `zerodeck-core` Dotted-Key `SettingsManager` & Decky-Compatible `FilePicker`

**Files:**
- Modify: `crates/zerodeck-core/src/settings.rs:1-67`
- Create: `crates/zerodeck-core/src/filepicker.rs`
- Modify: `crates/zerodeck-core/src/lib.rs` (exports + tests)

**Interfaces:**
- Consumes: `zerodeck-platform` (`PlatformPaths`, `SafePathSet`, `PlatformError`), `CoreError`.
- Produces:
  - `SettingsManager`: `values(&self) -> &Map<String, Value>`, `get_path(&self, dotted_key: &str) -> Option<Value>`, `set_path(&mut self, dotted_key: &str, value: Value) -> Result<(), CoreError>`, `remove(&mut self, key: &str) -> Result<Option<Value>, CoreError>`
  - `FilePickerQuery { pub path: Option<String>, pub include_files: bool, pub include_folders: bool, pub filter: Option<String>, pub extensions: Vec<String>, pub show_hidden: bool, pub max_entries: usize }` (with `Default`: `include_files: true`, `include_folders: true`, `max_entries: 5_000`)
  - `FilePickerEntry { pub name: String, pub path: String, pub realpath: String, pub isdir: bool, pub size: u64, pub modified: f64 }`
  - `FilePickerListing { pub path: String, pub realpath: String, pub files: Vec<FilePickerEntry>, pub truncated: bool }`
  - `FilePicker::list(paths: &PlatformPaths, query: FilePickerQuery) -> Result<FilePickerListing, CoreError>`

- [ ] **Step 1: Write failing tests in `crates/zerodeck-core/src/lib.rs`**
  - `settings_dotted_path_reads_writes_and_rejects_non_object_intermediate`: sets `"developer.allow_unverified_installs"` to `true`, reads via `get_path` and reopen, verifies `set_path("developer.sub", json!(1))` works, and verifies `set_path("scalar", json!("x"))` followed by `set_path("scalar.child", json!(1))` returns `Err(CoreError::InvalidSetting(_))` without modifying on-disk `loader.json`.
  - `filepicker_lists_allowed_roots_filters_extensions_and_omits_escaping_symlinks`: creates temp `home` + secondary `media` root (omitting `/run/media` so `from_existing` is exercised), adds `.hidden`, `a.zip`, `b.txt`, `sub/` dir, and an escaping symlink `leak -> /etc/passwd` (Unix); tests `show_hidden: false`, `extensions: ["zip"]`, `filter`, `max_entries: 1` (`truncated == true`), verifies `isdir`, `size == 0` on dirs, `modified > 0.0`, `leak` is omitted, and querying an outside path returns `Err(CoreError::Platform(PlatformError::PathEscape { .. }))`.

- [ ] **Step 2: Run tests to verify failure**
  Run: `cargo test -p zerodeck-core`
  Expected: FAIL (`filepicker` and `SettingsManager::{get_path, set_path}` missing).

- [ ] **Step 3: Implement `settings.rs` dotted-path helpers and `filepicker.rs`**
  - Reject empty segments (`""`, `"a..b"`, `".a"`) in `get_path`/`set_path`.
  - In `filepicker.rs`, build `SafePathSet::from_existing(paths.filepicker_roots())`, cap `max_entries` to `20_000`, match `filter` case-insensitively (bounded glob `*`/`?` or substring up to 256 chars, zero ReDoS), and sort dirs before files case-insensitively.

- [ ] **Step 4: Run tests and clippy**
  Run: `cargo test -p zerodeck-core --locked && cargo clippy -p zerodeck-core --all-targets --locked -- -D warnings`
  Expected: PASS with 0 warnings.

- [ ] **Step 5: Commit**
  ```bash
  git add crates/zerodeck-core
  git commit -m "feat(core): add dotted settings paths and path-jailed FilePicker"
  ```

---

### Task 3: `zerodeck-core` Atomic Plugin Installer & `remote_binary` Staging (`installer.rs`)

**Files:**
- Create: `crates/zerodeck-core/src/installer.rs`
- Modify: `crates/zerodeck-core/src/lib.rs` (exports + tests)

**Interfaces:**
- Consumes: `archive::{extract_zip_safely_with_limits, verify_sha256_policy, InstallSource}`, `http_policy::{OutboundHttpPolicy, send_checked_request}`, `discovery`, `SafePath`, `SafePathSet`, `PlatformPaths`, `PluginManifest`, `PackageManifest`.
- Produces:
  - `InstallPluginSource` (`RemoteZip { url: String, sha256: Option<String> }`, `LocalZip { path: PathBuf, sha256: Option<String>, user_confirmed: bool }`)
  - `InstallPluginRequest { pub source: InstallPluginSource, pub directory_hint: Option<String>, pub expected_name: Option<String>, pub expected_version: Option<String> }`
  - `InstallOutcome { pub name: String, pub directory: String, pub version: Option<String>, pub sha256: String, pub replaced_existing: bool }`
  - `InstallerLimits { pub max_zip_download_bytes: usize, pub max_remote_binary_bytes: usize, pub max_redirects: usize }`
  - `parse_install_source(url_or_path: &str, sha256: Option<String>, install_type: Option<&Value>, user_confirmed: bool) -> Result<InstallPluginSource, CoreError>` (unwraps `file://` URIs via `Url::to_file_path` and bare paths into `LocalZip`, `http`/`https` into `RemoteZip`, supporting `"remote"`/`"local"` or Decky `InstallType` ints `0`/`1`/`2`)
  - `prepare_staged_plugin(paths: &PlatformPaths, client: &reqwest::Client, policy: &OutboundHttpPolicy, limits: &InstallerLimits, req: &InstallPluginRequest, allow_unverified: bool) -> Result<StagedPluginInstall, CoreError>`
  - `commit_staged_plugin(staged: StagedPluginInstall, target_dir: &Path) -> Result<InstallOutcome, CoreError>` (cleans up staging on Drop if dropped uncommitted)

- [ ] **Step 1: Write failing tests in `crates/zerodeck-core/src/lib.rs`**
  - `installer_unwraps_top_level_wrapper_and_verifies_expected_version`: local Axum/TCP mock server serves a ZIP with `demo-v1.2.3/plugin.json` + `package.json` (`"version": "1.2.3"`), tests successful install with matching SHA-256 and failure (`ManifestMismatch`) when `expected_version` is `"9.9.9"`.
  - `installer_stages_remote_binary_and_preserves_existing_on_failure`: installs v1 plugin with `main.py` intact, then runs upgrade v2 declaring `remote_binary` with a bad SHA-256; asserts install returns `Err(CoreError::Sha256Mismatch { .. })` and v1's `main.py` on disk is untouched with zero leftover staging dirs; then retries with matching hash and asserts `<plugin_dir>/bin/<name>` exists with `0o755` permissions.
  - `installer_enforces_sha256_policy_and_file_uri_sideload`: tests `RemoteZip` without hash fails with `MissingSha256` unless `allow_unverified == true`; `parse_install_source("file://<home>/p.zip", ...)` resolves `LocalZip`, `ConfirmationRequired` unless `user_confirmed == true`, and `file:///etc/passwd` fails `PathEscape`.

- [ ] **Step 2: Run tests to verify failure**
  Run: `cargo test -p zerodeck-core`
  Expected: FAIL (`installer` module missing).

- [ ] **Step 3: Implement `installer.rs`**
  - Ensure `plugins_dir` exists (`0o755` mode on Unix); stream downloads up to `limits.max_zip_download_bytes` / `max_remote_binary_bytes`; support single top-level wrapper folder unwrapping (ignoring `__MACOSX`); validate `RemoteBinarySpec.name` with `SafePath` at `<stage>/bin` (single filename, no separators); enforce `0o755` on binaries and dirs, `0o644` on regular files.

- [ ] **Step 4: Run tests and clippy**
  Run: `cargo test -p zerodeck-core --locked && cargo clippy -p zerodeck-core --all-targets --locked -- -D warnings`
  Expected: PASS with 0 warnings.

- [ ] **Step 5: Commit**
  ```bash
  git add crates/zerodeck-core
  git commit -m "feat(core): add staged ZIP and remote_binary plugin installer"
  ```

---

### Task 4: `zerodeck-core` Reactive `PluginManager` & Hybrid `DebugWatcher` (`manager.rs` + `watcher.rs`)

**Files:**
- Modify: `crates/zerodeck-core/src/manager.rs:1-249`
- Create: `crates/zerodeck-core/src/watcher.rs`
- Modify: `crates/zerodeck-core/src/lib.rs` (exports + lifecycle/watcher tests)

**Interfaces:**
- Consumes: `installer::{InstallPluginRequest, InstallOutcome, InstallerLimits, prepare_staged_plugin, commit_staged_plugin}`, `http_policy::OutboundHttpPolicy`, `filepicker::{FilePicker, FilePickerQuery, FilePickerListing}`, `PluginSupervisor`, `SafePath`, `PlatformPaths`.
- Produces:
  - `PluginFileScope` (`Dist`, `Assets`, `Data`, `FrontendBundle`)
  - `LoaderEvent` (`PluginsChanged(Vec<PluginSummary>)`, `PluginReloaded { plugin: String, directory: String }`, `FrontendReloaded { plugin: String, directory: String }`)
  - `PluginManager` new methods:
    - `with_http_config(..., client: reqwest::Client, policy: OutboundHttpPolicy, limits: InstallerLimits)` (keeping `open` / `from_parts` signatures compatible with defaults!)
    - `paths(&self) -> &PlatformPaths`
    - `allow_unverified_installs(&self) -> bool`
    - `subscribe_loader_events(&self) -> broadcast::Receiver<LoaderEvent>`
    - `install_plugin(&self, req: InstallPluginRequest) -> Result<InstallOutcome, CoreError>`
    - `uninstall_plugin(&self, plugin_name: &str, purge_data: bool) -> Result<(), CoreError>`
    - `enable_plugin(&self, plugin_name: &str) -> Result<(), CoreError>`
    - `disable_plugin(&self, plugin_name: &str) -> Result<(), CoreError>`
    - `set_plugin_order(&self, order: Vec<String>) -> Result<(), CoreError>`
    - `get_setting(&self, dotted_key: Option<&str>) -> Value`
    - `set_setting(&self, dotted_key: &str, value: Value) -> Result<(), CoreError>`
    - `list_files(&self, query: FilePickerQuery) -> Result<FilePickerListing, CoreError>`
    - `resolve_plugin_file(&self, plugin_id: &str, scope: PluginFileScope, rel_path: &str) -> Result<PathBuf, CoreError>`
    - `resolve_static_file(root: &Path, rel_path: &str) -> Result<PathBuf, CoreError>`
  - `DebugWatcherHandle` (`start_debug_watcher(manager: Arc<PluginManager>) -> Result<DebugWatcherHandle, CoreError>`, `stop(self)` + `Drop` shutdown)

- [ ] **Step 1: Write failing tests in `crates/zerodeck-core/src/lib.rs`**
  - `plugin_manager_install_disable_enable_order_and_uninstall_preserve_data_by_default`: installs a Python fixture plugin (`_uninstall` writes marker inside `settings/<dir>` or temp path), verifies RPC works, `disable_plugin` stops worker (`PluginState::Stopped` + `PluginDisabled`), `enable_plugin` respawns worker and RPC succeeds again, `set_plugin_order` updates `list_plugins()`, `uninstall_plugin(name, false)` runs `_uninstall` and deletes `plugins/<dir>` while preserving `settings/<dir>` and `data/<dir>`; second test with `purge_data = true` removes `settings/<dir>` and `data/<dir>` too.
  - `plugin_manager_set_setting_rejects_managed_keys_and_resolve_plugin_file_checks_jail`: verifies `set_setting("disabledPlugins", ...)` / `"pluginOrder"` / `"disabled_plugins"` fails with `CoreError::InvalidSetting`, `"developer.allow_unverified_installs"` persists cleanly, and `resolve_plugin_file` resolves both `manifest.name` and `directory` while rejecting `../secret`.
  - `debug_watcher_reloads_debug_plugin_on_py_edit_and_emits_frontend_event_on_dist_edit`: starts `start_debug_watcher`, edits `main.py` of a `"debug"` plugin (`v1` -> `v2` return value), waits for `LoaderEvent::PluginReloaded`, calls RPC and verifies `"v2"`; edits `dist/index.js` and asserts `LoaderEvent::FrontendReloaded`; edits a non-debug plugin and asserts no reload event fires.

- [ ] **Step 2: Run tests to verify failure**
  Run: `cargo test -p zerodeck-core`
  Expected: FAIL.

- [ ] **Step 3: Implement `manager.rs` extensions and `watcher.rs`**
  - Serialize structural mutations under `lifecycle_lock: tokio::sync::Mutex<()>`; copy `settings` / `plugins` snapshots out of sync `Mutex` guards before any `.await`.
  - Stage install completely (`prepare_staged_plugin`) before acquiring `lifecycle_lock` to stop an existing worker and swap (`commit_staged_plugin`).
  - In `watcher.rs`, combine `#[cfg(target_os = "linux")] inotify` with a lightweight `mtime` snapshot sweep (500ms non-Linux, 2s fallback on Linux) over `debug && !disabled` plugins, ignoring `.`-prefixed dirs, `__pycache__`, and `*.pyc`, with 300ms debounce.

- [ ] **Step 4: Run tests and clippy**
  Run: `cargo test -p zerodeck-core --locked && cargo clippy -p zerodeck-core --all-targets --locked -- -D warnings`
  Expected: PASS with 0 warnings.

- [ ] **Step 5: Commit**
  ```bash
  git add crates/zerodeck-core
  git commit -m "feat(core): add dynamic PluginManager lifecycle, dotted settings, and debug hot-reload watcher"
  ```

---

### Task 5: `zerodeck-server` Full HTTP Routes, `/fetch` Proxy, `WsSyncCache` & Lifecycle WS Routes

**Files:**
- Modify: `crates/zerodeck-server/Cargo.toml:13-31`
- Modify: `crates/zerodeck-server/src/auth.rs:11-57`
- Create: `crates/zerodeck-server/src/fetch.rs`
- Create: `crates/zerodeck-server/src/ws_sync.rs`
- Modify: `crates/zerodeck-server/src/router.rs:1-663`
- Modify: `crates/zerodeck-server/src/lib.rs` (tests)

**Interfaces:**
- Consumes: `zerodeck-core` (`PluginManager`, `LoaderEvent`, `PluginFileScope`, `FilePickerQuery`, `InstallPluginRequest`, `parse_install_source`, `http_policy`), `tokio_util::io::ReaderStream`, `percent_encoding`, `reqwest`, `url`.
- Produces:
  - Extended `ServerConfig { ..., pub frontend_dir: Option<PathBuf>, pub max_zip_download_bytes: usize, pub max_remote_binary_bytes: usize, pub fetch_connect_timeout: Duration, pub fetch_response_timeout: Duration, pub ws_sync_cache_capacity: usize, pub ws_sync_cache_ttl: Duration }`
  - `AppState::with_manager_and_http(config, manager, client, policy)` (preserving `new`, `with_plugins`, `with_manager`)
  - HTTP routes: `/frontend/{*path}`, `/static/{*path}`, `/locales/{*path}`, `/plugins/load_main/{name}`, `/plugins/{name}/frontend_bundle`, `/plugins/{name}/dist/{*path}`, `/plugins/{name}/assets/{*path}`, `/plugins/{name}/data/{*path}`, `ANY /fetch`
  - `WsSyncCache` deduplicating `(id, route, canonical_args)` across `Call` + `FullSync` and evicting on `ReceivedResponse`
  - WS routes: `utilities/filepicker`, `loader/install_plugin`, `utilities/install_plugin`, `loader/uninstall_plugin`, `loader/enable_plugin`, `loader/disable_plugin`, `loader/set_plugin_order`, `loader/get_settings`, `loader/set_setting`, plus top-level `LoaderEvent` forwarding (`loader/plugins_changed`, `zerodeck/plugin_reloaded`, `zerodeck/frontend_reloaded`)

- [ ] **Step 1: Write failing tests in `crates/zerodeck-server/src/lib.rs`**
  - Update existing `query_auth_paths_require_a_valid_token` test where `/fetch?url=x&auth={token}` previously expected `404` -> now expects `400 Bad Request` (missing `fetch_url`), and `/fetch?auth={token}` returns `400`.
  - `http_static_and_plugin_routes_serve_mime_types_and_block_traversal`: verifies `/frontend/index.js`, `/static/app.css`, `/locales/en.json`, `/plugins/Demo/dist/index.js`, `/plugins/Demo/assets/icon.svg`, `/plugins/Demo/data/cache.json`, `/plugins/load_main/Demo`, `/plugins/Demo/frontend_bundle` (`200`, right `Content-Type`), `403` on `..` / `%2e%2e` / `%00`, `404` on missing files / symlink escape.
  - `fetch_proxy_translates_headers_streams_body_and_blocks_ssrf_redirects`: tests `X-Decky-Header-*`, `X-Decky-Fetch-Excluded-Headers`, stripped upstream `Access-Control-*`, streaming body, and `403` on loopback/metadata + `302` redirect to `127.0.0.1:1337`.
  - `ws_full_sync_deduplicates_completed_call_and_evicts_on_received_response`: Python plugin increments a disk counter file on each invocation; client sends `CALL`, drops before receiving reply (or verifies cached reply), reconnects and sends `FULL_SYNC` with the same `CALL` -> receives `REPLY` while counter file stays `"1"`; sends `RECEIVED_RESPONSE`, then calls again -> executes fresh.
  - `ws_routes_cover_filepicker_install_lifecycle_and_settings`: verifies `utilities/filepicker` (`files`, `isdir`, `modified`, `size`), `loader/install_plugin` & `utilities/install_plugin`, `loader/plugins_changed` event, `loader/disable_plugin`, `loader/enable_plugin`, `loader/set_plugin_order`, `loader/get_settings`, `loader/set_setting` (and rejection on `disabledPlugins`), `loader/uninstall_plugin`.

- [ ] **Step 2: Run tests to verify failure**
  Run: `cargo test -p zerodeck-server`
  Expected: FAIL.

- [ ] **Step 3: Implement `auth.rs`, `fetch.rs`, `ws_sync.rs`, and `router.rs`**
  - In `auth.rs`, allow `GET`/`HEAD` `/plugins/{name}/frontend_bundle` and reject `\`, `%5c`, `%00`.
  - In `router.rs` file handlers, percent-decode path segments, validate via `PluginManager::resolve_plugin_file` / `resolve_static_file`, stream with `ReaderStream`, and set `Content-Type` from extension (`text/javascript; charset=utf-8` for `.js`/`.mjs`).
  - In `fetch.rs`, validate `fetch_url` via `http_policy`, translate headers, strip `cookie` (unless `X-Decky-Header-Cookie`), stream body both ways, strip hop-by-hop and `Access-Control-*` response headers.
  - In `ws_sync.rs`, use `tokio::sync::watch` for `InFlight` callers so reconnecting `FULL_SYNC` awaits the active run without re-spawning.

- [ ] **Step 4: Run tests and clippy**
  Run: `cargo test -p zerodeck-server --locked && cargo clippy -p zerodeck-server --all-targets --locked -- -D warnings`
  Expected: PASS with 0 warnings.

- [ ] **Step 5: Commit**
  ```bash
  git add crates/zerodeck-server
  git commit -m "feat(server): add static/plugin routes, /fetch CORS proxy, WsSyncCache, and lifecycle WS routes"
  ```

---

### Task 6: `zerodeckd` Integration, Daemon E2E Tests, Documentation & Full Gate Verification

**Files:**
- Modify: `crates/zerodeckd/src/main.rs:45-221`
- Modify: `crates/zerodeckd/tests/daemon.rs:173-291`
- Modify: `docs/architecture/wire-protocol.md:1-96`
- Modify: `README.md:43-59,229-239`

**Interfaces:**
- Consumes: `ServerConfig::frontend_dir`, `zerodeck_core::start_debug_watcher`.
- Produces:
  - `resolve_frontend_path() -> Option<PathBuf>` in `zerodeckd/src/main.rs`
  - Daemon startup launching `start_debug_watcher` and stopping it cleanly on `shutdown_signal()`
  - Complete Phase 3 documentation in `docs/architecture/wire-protocol.md` and updated `README.md`

- [ ] **Step 1: Write failing E2E test in `crates/zerodeckd/tests/daemon.rs`**
  - `daemon_serves_frontend_sideloads_plugin_assets_and_uninstalls`: creates temp `frontend/` (`index.js`), sets `ZERODECK_FRONTEND_DIR`, spawns `zerodeckd`, fetches `GET /frontend/index.js` (`200`, `text/javascript`), creates a temp ZIP plugin inside `home` with `assets/hello.txt` + `main.py`, installs it via `/ws` `loader/install_plugin` (`file://...`, `user_confirmed: true`), fetches `GET /plugins/{name}/assets/hello.txt` (`200`), calls plugin method via `/ws`, uninstalls via `loader/uninstall_plugin`, verifies clean `SIGTERM` shutdown.

- [ ] **Step 2: Run E2E test to verify failure**
  Run: `cargo test -p zerodeckd --test daemon`
  Expected: FAIL (`404` on `/frontend/index.js` and missing `loader/install_plugin`).

- [ ] **Step 3: Wire `zerodeckd/src/main.rs` and update `docs/architecture/wire-protocol.md` and `README.md`**
  - Keep `--check-config` and `--healthcheck` free of watcher / HTTP side effects.
  - Document every Phase 3 HTTP route, `/fetch` header/SSRF contract, `FULL_SYNC` + `RECEIVED_RESPONSE` cache, `LoaderEvent` frames, and all WS routes in `docs/architecture/wire-protocol.md`; check off Phase 3 in `README.md`.

- [ ] **Step 4: Run full verification suite (`just ci`)**
  Run: `just ci`
  Expected: `just check` (Rust `fmt`, `clippy`, `cargo test`, `cargo deny`, Python `ruff` + `pyright` + `unittest`, Frontend `biome` + `tsc` + `vite build` + `wsrouter.test.ts`), `just py-matrix`, `just lint-infra`, and `just smoke` ALL PASS.

- [ ] **Step 5: Commit**
  ```bash
  git add crates/zerodeckd docs/architecture/wire-protocol.md README.md docs/superpowers/specs/2026-11-24-phase3-full-server-design.md docs/superpowers/plans/2026-11-24-phase3-full-server.md
  git commit -m "feat(daemon): wire frontend assets, debug watcher, E2E coverage, and Phase 3 docs"
  ```

---

## Self-Review Results
1. **Spec coverage:** Every section of [2026-11-24-phase3-full-server-design.md](file:///Users/ygorsimoes/Documents/projects/ygorsimoes/zerodeck/docs/superpowers/specs/2026-11-24-phase3-full-server-design.md) — `UMask=0077` archive permission normalization, `http_policy` SSRF + redirect defense, dotted `SettingsManager` paths, Decky-compatible `FilePicker`, staged `installer` + `remote_binary` + `file://` sideload, reactive `PluginManager` + `LoaderEvent`, hybrid `inotify`/polling `DebugWatcher`, `WsSyncCache`, static/plugin HTTP routes, `/fetch` proxy, WS routes, `zerodeckd` `frontend_dir`/watcher wiring, and docs — maps 1:1 to Tasks 1–6.
2. **Step scan:** Every step specifies exact files, signatures, test names, assertions, and verification commands without placeholder TBDs or raw function-body transcripts.
3. **Type consistency:** `OutboundHttpPolicy`, `send_checked_request`, `FilePickerQuery`/`FilePickerListing`/`FilePickerEntry`, `InstallPluginRequest`/`InstallPluginSource`/`InstallOutcome`, `PluginFileScope`, `LoaderEvent`, `DebugWatcherHandle`, `WsSyncCache`, and `ServerConfig` names match across Tasks 1–6.
4. **Review Focus:** All 5 high-risk failure modes in `Review Focus` have explicit failing tests first in Tasks 1, 2, 3, and 5.
