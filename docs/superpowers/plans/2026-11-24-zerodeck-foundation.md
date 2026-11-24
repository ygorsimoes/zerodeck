# ZeroDeck Phase 1 — Repository & Core Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build ZeroDeck's clean-room monorepo foundation — repository governance, Rust 2024 workspace crates (`zerodeck-protocol`, `zerodeck-platform`, `zerodeck-core`, `zerodeck-plugin-host`, `zerodeck-cef`, `zerodeck-server`, `zerodeckd`), Python plugin host shim (`hosts/python`), and `pnpm` + TypeScript 7.0.2 CEF frontend skeleton (`frontend/`) — with passing tests, lints, and CI.

**Architecture:** Option A + Monorepo: a lean, zero-GIL native Rust daemon (`zerodeckd`) owns HTTP/WS serving, CEF CDP injection, path-jailed asset serving, atomic settings, ZIP/SHA-256 validation, and supervision, while spawning isolated `system python3` subprocesses (`zerodeck-py-host`) over inherited Unix `socketpair()` FDs (or per-UID `0700` socket dirs) only for plugins with `main.py`. The CEF frontend (`@zerodeck/frontend`) externalizes React to Steam's `SP_REACT`/`SP_JSX`/`SP_REACTDOM` globals and communicates via the `WSRouter` `FULL_SYNC` protocol.

**Tech Stack:** Rust Edition 2024 (`tokio 1.54`, `axum 0.8`, `tower-http 0.7`, `reqwest 0.13` w/ `rustls`, `tokio-tungstenite 0.30`, `serde 1.0`, `thiserror 2.0`, `anyhow 1.0`, `tracing 0.1`, `clap 4.6`, `notify 9.0`, `sha2 0.11`, `zip 9.8`, `uuid 1.29`, `rustix 1.1`), `just`, `cargo-deny`, Python `>=3.10` stdlib (`ruff`, `pyright`), `pnpm 12.x` (exclusive, no `npm`), `TypeScript ^7.0.2`, `Vite ^8.3.8`, `@biomejs/biome ^2.6.9`, `React ^19.3.1`.

**Spec:** [2026-11-24-zerodeck-foundation-design.md](file:///Users/ygorsimoes/Documents/projects/ygorsimoes/zerodeck/docs/superpowers/specs/2026-11-24-zerodeck-foundation-design.md)

## Global Constraints
- **License:** MIT (`LICENSE` at root, `license = "MIT"` in `Cargo.toml` and `package.json`; `deny.toml` allows `MIT`, `Apache-2.0`, `BSD-2-Clause`, `BSD-3-Clause`, `ISC`, `Zlib`, `Unicode-3.0` and denies `GPL-*`/`AGPL-*`).
- **Clean-room rule:** Zero GPL code copied from `decky-loader`; implement strictly from public wire/API contracts documented in `docs/architecture/wire-protocol.md`.
- **Rust quality floor:** `edition = "2024"`, `rust-version = "1.88"`, strict `[workspace.lints.rust]` (`unsafe_code = "warn"`) and `[workspace.lints.clippy]` (`all = "warn"`, `pedantic = "warn"` with targeted allows if needed), zero warnings under `cargo clippy --workspace --all-targets -- -D warnings`, formatted via `rustfmt`.
- **Acyclic crate graph:** `zerodeck-protocol` has zero internal deps; `zerodeck-platform` depends only on `zerodeck-protocol`; `zerodeck-core` depends on `platform`, `plugin-host`, `protocol`; `zerodeck-cef` depends on `protocol`; `zerodeck-server` depends on `core`, `cef`, `plugin-host`, `protocol`; `zerodeckd` wires all crates.
- **Package managers & JS toolchain:** Exclusive `pnpm` (`packageManager: "pnpm@12.16.1"`, no `npm`); `typescript: "^7.0.2"`; `vite: "^8.3.8"`; `@biomejs/biome: "^2.6.9"`; React externalized to `SP_REACT`, `SP_JSX`, `SP_REACTDOM`.
- **Python host:** Uses system `python3`, pure stdlib in the shim (`hosts/python`), supporting `api_version` 0 and 1+, `decky` + `decky_plugin` module aliases, and `py_modules/` path injection.
- **Error handling:** Never silently swallow errors (`except: pass` / `.ok()` without logging); surface typed errors via `thiserror`.

## Review Focus
- **Path-traversal & symlink escapes in `SafePath`:** Reject literal `..`, unnormalized paths, and symlinks resolving outside canonical roots -> pinned by tests in Task 3.
- **Malformed or negative `MessageType` (`ERROR = -1`) frames on `WSRouter`:** Unknown types or missing required fields must return typed `ProtocolError`, never panic -> pinned by tests in Task 2 & Task 5.
- **Corrupt JSON in `SettingsManager`:** Loading malformed JSON must return an error without overwriting or zeroing the existing settings file -> pinned by tests in Task 4.
- **Zip-Slip entries (`../../...`) and missing SHA-256 on remote installs:** Archive extraction must reject escaping paths and remote policy must fail closed when SHA-256 is absent unless `allow_unverified_installs` is enabled -> pinned by tests in Task 4.
- **`api_version = 0` vs `api_version = 1` dispatch and `decky_plugin` alias in `zerodeck_py_host`:** Class-based legacy plugins and instance-based v1 plugins must both execute methods and emit events via `decky` or `decky_plugin` -> pinned by tests in Task 6.

---

### Task 1: Monorepo Root Config, Tooling, CI & Clean-Room Protocol Spec

**Files:**
- Create: `LICENSE`
- Create: `.editorconfig`
- Create: `.gitignore`
- Create: `rustfmt.toml`
- Create: `deny.toml`
- Create: `Cargo.toml`
- Create: `justfile`
- Create: `pnpm-workspace.yaml`
- Create: `biome.json`
- Create: `.github/workflows/ci.yml`
- Create: `deploy/systemd/zerodeck.service`
- Create: `docs/architecture/wire-protocol.md`

**Interfaces:**
- Produces: Cargo workspace root (`[workspace.package]`, `[workspace.dependencies]`, `[workspace.lints]`), `justfile` recipes (`fmt`, `lint`, `test`, `check`, `build-frontend`), and clean-room protocol spec `docs/architecture/wire-protocol.md`.

- [ ] **Step 1: Create root governance and formatting config files**
  Create `LICENSE` (MIT, 2026 ZeroDeck Contributors), `.editorconfig` (LF, UTF-8, 4-space `.rs`/`.py`, 2-space `.ts`/`.tsx`/`.json`/`.yml`), `.gitignore` (`target/`, `node_modules/`, `dist/`, `.venv/`, `__pycache__/`, `.DS_Store`), `rustfmt.toml` (`edition = "2024"`, `max_width = 100`, `use_field_init_shorthand = true`), `deny.toml` (`[licenses] allow = ["MIT", "Apache-2.0", "BSD-2-Clause", "BSD-3-Clause", "ISC", "Zlib", "Unicode-3.0"]`, `[advisories]`, `[bans]`), `pnpm-workspace.yaml` (`packages: ["frontend"]`), and `biome.json` (Biome 2.x config with single quotes, 100 lineWidth, recommended lints).

- [ ] **Step 2: Create root `Cargo.toml` workspace**
  Configure `[workspace]` (`resolver = "3"`, `members = ["crates/*"]`), `[workspace.package]` (`version = "0.1.0"`, `edition = "2024"`, `rust-version = "1.88"`, `license = "MIT"`, `repository = "https://github.com/ygorsimoes/zerodeck"`), `[workspace.lints.rust]` (`unsafe_code = "warn"`, `missing_debug_implementations = "warn"`), `[workspace.lints.clippy]` (`all = { level = "warn", priority = -1 }`, `pedantic = { level = "warn", priority = -1 }`, `module_name_repetitions = "allow"`), and `[workspace.dependencies]` pinning the verified crate versions (`tokio = "1.54.1"`, `axum = "0.8.9"`, `tower = "0.5.3"`, `tower-http = "0.7.1"`, `reqwest = { version = "0.13.5", default-features = false, features = ["rustls", "stream", "json"] }`, `tokio-tungstenite = "0.30.0"`, `serde = { version = "1.0.229", features = ["derive"] }`, `serde_json = "1.0.154"`, `thiserror = "2.0.21"`, `anyhow = "1.0.105"`, `tracing = "0.1.44"`, `tracing-subscriber = { version = "0.3.23", features = ["env-filter", "fmt", "json"] }`, `clap = { version = "4.6.9", features = ["derive", "env"] }`, `notify = "9.0.0"`, `sha2 = "0.11.1"`, `hex = "0.4.3"`, `zip = { version = "9.8.2", default-features = false, features = ["deflate"] }`, `uuid = { version = "1.29.3", features = ["v4"] }`, `rustix = { version = "1.1.6", features = ["process", "fs", "net"] }`).

- [ ] **Step 3: Create `justfile`, CI workflow, systemd unit, and clean-room protocol spec**
  - `justfile`: recipes `fmt`, `fmt-check`, `lint`, `test`, `check` (runs all Rust + Python + `pnpm` frontend checks), `py-test` (`python3 -m unittest discover -s hosts/python/tests`), `fe-check` (`pnpm --dir frontend exec biome check . && pnpm --dir frontend exec tsc --noEmit && pnpm --dir frontend build`).
  - `.github/workflows/ci.yml`: jobs for `rust` (`cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`), `python` (`python3 -m unittest`, `ruff`, `pyright`), `frontend` (`pnpm/action-setup` + `pnpm install --frozen-lockfile`, `biome check`, `tsc --noEmit`, `pnpm build`), and `deny` (`EmbarkStudios/cargo-deny-action`).
  - `deploy/systemd/zerodeck.service`: hardened systemd template launching `zerodeckd` with `UNPRIVILEGED_PATH`, `PRIVILEGED_PATH`, `RUST_LOG=info`.
  - `docs/architecture/wire-protocol.md`: clean-room specification of HTTP routes (`127.0.0.1:1337`), `WSRouter` JSON frames (`-1..5`, `FULL_SYNC`), Plugin IPC JSON-line protocol (`CALL=0`, `RESPONSE=1`, `EVENT=2`, `CONTROL`), `plugin.json`/`package.json` schemas, and `DECKY_*` env contract.

- [ ] **Step 4: Verify root tooling**
  Run: `just --list && python3 -c "import json; json.load(open('biome.json')); print('biome.json ok')"`
  Expected: Lists `justfile` recipes and prints `biome.json ok` with exit code 0.

- [ ] **Step 5: Commit**
  ```bash
  git add LICENSE .editorconfig .gitignore rustfmt.toml deny.toml Cargo.toml justfile pnpm-workspace.yaml biome.json .github/workflows/ci.yml deploy/systemd/zerodeck.service docs/architecture/wire-protocol.md
  git commit -m "chore: initialize ZeroDeck monorepo config, CI, and clean-room protocol spec"
  ```

---

### Task 2: `zerodeck-protocol` Crate (Wire, IPC & Manifest Contracts)

**Files:**
- Create: `crates/zerodeck-protocol/Cargo.toml`
- Create: `crates/zerodeck-protocol/src/lib.rs`
- Create: `crates/zerodeck-protocol/src/ws.rs`
- Create: `crates/zerodeck-protocol/src/ipc.rs`
- Create: `crates/zerodeck-protocol/src/manifest.rs`
- Create: `crates/zerodeck-protocol/src/error.rs`

**Interfaces:**
- Consumes: `serde`, `serde_json`, `thiserror`, `uuid` (workspace deps).
- Produces:
  - `ProtocolError`
  - `WsMessageType` (`Error = -1`, `Call = 0`, `Reply = 1`, `Discard = 2`, `ReceivedResponse = 3`, `FullSync = 4`, `Event = 5`)
  - `WsClientMessage` (`Call { id: u64, route: String, args: Vec<Value> }`, `ReceivedResponse { id: u64 }`, `FullSync { messages: Vec<WsCallPayload> }`)
  - `WsServerMessage` (`Reply { id: u64, result: Value }`, `Error { id: u64, error: WsErrorDetail }`, `Discard { id: u64 }`, `Event { event: String, args: Vec<Value> }`)
  - `PluginIpcToWorker` (`Call { id: String, method: String, args: Value, legacy: bool }`, `Control { uninstall: bool }`)
  - `PluginIpcFromWorker` (`Response { id: String, success: bool, res: Value }`, `Event { event: String, args: Vec<Value> }`)
  - `PluginManifest` (`name: String`, `author: String`, `flags: Vec<String>`, `api_version: u32`)
  - `PackageManifest` (`version: Option<String>`, `package_type: Option<String>`, `remote_binary: Vec<RemoteBinarySpec>`)
  - `RemoteBinarySpec` (`name: String`, `url: String`, `sha256hash: Option<String>`)
  - `PluginLoadType` (`LegacyEvalIife = 0`, `EsModuleV1 = 1`)

- [ ] **Step 1: Write failing protocol serialization/deserialization tests**
  In `crates/zerodeck-protocol/src/lib.rs` (or module tests), write unit tests:
  - `ws_error_code_is_negative_one_and_roundtrips`: deserialize `{"type": 0, "id": 1, "route": "loader/get_plugins", "args": []}` -> `WsClientMessage::Call`, and serialize `WsServerMessage::Error` -> `type == -1`.
  - `ws_unknown_message_type_returns_protocol_error`: parsing `{"type": 99, "id": 1}` returns `Err(ProtocolError::UnknownMessageType(99))`.
  - `ipc_call_and_response_roundtrip`: verify `PluginIpcToWorker::Call` serializes `"type": 0` and `PluginIpcFromWorker::Response` deserializes `"type": 1`.
  - `manifest_defaults_api_version_zero_and_detects_load_type`: `PluginManifest` without `api_version` defaults to `0`; `PackageManifest` with `"type": "module"` resolves `PluginLoadType::EsModuleV1`.

- [ ] **Step 2: Run test to verify it fails**
  Run: `cargo test -p zerodeck-protocol`
  Expected: FAIL (crate/types not implemented yet).

- [ ] **Step 3: Implement `zerodeck-protocol` modules**
  Create `Cargo.toml` (`[lints] workspace = true`) and implement `error.rs`, `ws.rs`, `ipc.rs`, `manifest.rs`, and `lib.rs` with custom integer tag `Serialize`/`Deserialize` matching exact `decky-loader` numeric `type` codes (`-1..5` for WS, `0..2` for Plugin IPC). Keep all public types deriving `Debug, Clone, PartialEq`.

- [ ] **Step 4: Run tests and clippy to verify they pass**
  Run: `cargo test -p zerodeck-protocol && cargo clippy -p zerodeck-protocol --all-targets -- -D warnings`
  Expected: PASS with 0 warnings.

- [ ] **Step 5: Commit**
  ```bash
  git add crates/zerodeck-protocol
  git commit -m "feat(protocol): add typed WS, plugin IPC, and manifest contracts"
  ```

---

### Task 3: `zerodeck-platform` Crate (Paths, `SafePath` Jail, Atomic Writes & Privileges)

**Files:**
- Create: `crates/zerodeck-platform/Cargo.toml`
- Create: `crates/zerodeck-platform/src/lib.rs`
- Create: `crates/zerodeck-platform/src/error.rs`
- Create: `crates/zerodeck-platform/src/paths.rs`
- Create: `crates/zerodeck-platform/src/safe_path.rs`
- Create: `crates/zerodeck-platform/src/fs.rs`
- Create: `crates/zerodeck-platform/src/privilege.rs`

**Interfaces:**
- Consumes: `zerodeck-protocol`, `thiserror`, `rustix`, `uuid`.
- Produces:
  - `PlatformError`
  - `PlatformPaths` (`from_env()`, `plugins_dir()`, `settings_dir()`, `data_dir()`, `logs_dir()`, `filepicker_roots()`)
  - `SafePath` (`new(root: impl AsRef<Path>) -> Result<Self, PlatformError>`, `resolve(&self, rel: impl AsRef<Path>) -> Result<PathBuf, PlatformError>`)
  - `SafePathSet` (`new(roots: Vec<PathBuf>)`, `resolve(&self, candidate: impl AsRef<Path>) -> Result<PathBuf, PlatformError>`)
  - `atomic_write(path: &Path, contents: &[u8]) -> Result<(), PlatformError>`
  - `PrivilegeTarget` (`HostUser`, `Root`)

- [ ] **Step 1: Write failing tests for `SafePath`, `SafePathSet`, `atomic_write`, and `PlatformPaths`**
  Write tests covering:
  - `safe_path_rejects_parent_traversal_and_absolute_paths`: `safe.resolve("../secret")` and `safe.resolve("/etc/passwd")` return `Err(PlatformError::PathEscape { .. })`.
  - `safe_path_rejects_symlink_pointing_outside_root` (under `#[cfg(unix)]`): symlink inside `root` pointing to external temp file fails canonical verification.
  - `safe_path_set_allows_multiple_roots_and_blocks_outside`: allows `$HOME` and `/run/media`-style temp root, rejects outside paths.
  - `atomic_write_replaces_existing_file_without_leaving_temp_files`: writes initial bytes, overwrites atomically, asserts new contents and no leftover `.tmp.` files in parent directory.
  - `platform_paths_respects_unprivileged_and_privileged_overrides`: verifies env override behavior cleanly using a custom `EnvLookup` trait/closure (avoiding global `set_var` races in parallel tests!).

- [ ] **Step 2: Run test to verify it fails**
  Run: `cargo test -p zerodeck-platform`
  Expected: FAIL.

- [ ] **Step 3: Implement `zerodeck-platform`**
  Implement `paths.rs` (injectable env lookup for clean unit testing + `PlatformPaths::from_env()`), `safe_path.rs` (lexical check against `RootDir`/`Prefix`/`ParentDir` + `fs::canonicalize` check `starts_with`), `fs.rs` (`atomic_write` creating temp file in same directory, writing, `sync_all()`, `rename()`), `privilege.rs` (`PrivilegeTarget`, Unix UID/GID lookup helpers via `rustix` gated cleanly with `#[cfg(unix)]`).

- [ ] **Step 4: Run tests and clippy to verify they pass**
  Run: `cargo test -p zerodeck-platform && cargo clippy -p zerodeck-platform --all-targets -- -D warnings`
  Expected: PASS with 0 warnings.

- [ ] **Step 5: Commit**
  ```bash
  git add crates/zerodeck-platform
  git commit -m "feat(platform): add SafePath jail, atomic_write, env paths, and privilege types"
  ```

---

### Task 4: `zerodeck-core` Crate (Atomic `SettingsManager`, Plugin Discovery & Archive/Hash Policy)

**Files:**
- Create: `crates/zerodeck-core/Cargo.toml`
- Create: `crates/zerodeck-core/src/lib.rs`
- Create: `crates/zerodeck-core/src/error.rs`
- Create: `crates/zerodeck-core/src/settings.rs`
- Create: `crates/zerodeck-core/src/discovery.rs`
- Create: `crates/zerodeck-core/src/archive.rs`

**Interfaces:**
- Consumes: `zerodeck-protocol`, `zerodeck-platform`, `serde`, `serde_json`, `thiserror`, `sha2`, `hex`, `zip`, `tracing`.
- Produces:
  - `CoreError`
  - `SettingsManager` (`open(path: PathBuf) -> Result<Self, CoreError>`, `get<T: DeserializeOwned>(&self, key: &str) -> Option<T>`, `set<T: Serialize>(&mut self, key: &str, value: T) -> Result<(), CoreError>`)
  - `DiscoveredPlugin` (`directory: String`, `manifest: PluginManifest`, `package: Option<PackageManifest>`, `load_type: PluginLoadType`, `passive: bool`, `has_entrypoint: bool`)
  - `discover_plugins(plugins_dir: &Path) -> Result<Vec<DiscoveredPlugin>, CoreError>`
  - `InstallSource` (`Remote { sha256: Option<String> }`, `LocalFile { sha256: Option<String>, user_confirmed: bool }`)
  - `verify_sha256_policy(bytes: &[u8], source: &InstallSource, allow_unverified: bool) -> Result<String, CoreError>`
  - `extract_zip_safely(bytes: &[u8], dest_dir: &Path) -> Result<Vec<PathBuf>, CoreError>`

- [ ] **Step 1: Write failing tests for settings, discovery, Zip-Slip, and SHA-256 policy**
  Write tests:
  - `settings_corrupt_json_returns_error_and_preserves_disk_file`: write `"{bad json"` to `loader.json`, call `SettingsManager::open`, assert `Err(CoreError::SettingsParse { .. })` AND disk content remains `"{bad json"` (never truncated!).
  - `settings_set_persists_atomically_across_reopen`: set `"pluginOrder"` and reopen from disk.
  - `discover_plugins_detects_esmodule_legacy_and_passive`: create temp `plugins/` with 3 plugins (ESModule w/ `main.py`, Legacy w/o `package.json`, Passive w/o `main.py`), assert exact `load_type`, `passive`, and `manifest`.
  - `extract_zip_safely_rejects_zip_slip_traversal`: build an in-memory ZIP with entry `"../../evil.txt"`, assert `Err(CoreError::UnsafeArchiveEntry { .. })` and destination stays empty.
  - `verify_sha256_policy_enforces_remote_and_local_rules`: remote without hash fails when `allow_unverified == false`, passes with matching hash, local without hash fails unless `user_confirmed == true`, returns computed hex digest.

- [ ] **Step 2: Run test to verify it fails**
  Run: `cargo test -p zerodeck-core`
  Expected: FAIL.

- [ ] **Step 3: Implement `zerodeck-core`**
  Implement `settings.rs` (using `zerodeck_platform::atomic_write`), `discovery.rs`, `archive.rs` (`zip::ZipArchive` validating `file.enclosed_name()` + rejecting symlinks, `sha2::Sha256` digest check + `0o755` permissions on executables on Unix), and `error.rs`.

- [ ] **Step 4: Run tests and clippy to verify they pass**
  Run: `cargo test -p zerodeck-core && cargo clippy -p zerodeck-core --all-targets -- -D warnings`
  Expected: PASS with 0 warnings.

- [ ] **Step 5: Commit**
  ```bash
  git add crates/zerodeck-core
  git commit -m "feat(core): add atomic SettingsManager, plugin discovery, and Zip-Slip/SHA-256 validator"
  ```

---

### Task 5: `zerodeck-plugin-host`, `zerodeck-cef`, `zerodeck-server` & `zerodeckd` Foundation

**Files:**
- Create: `crates/zerodeck-plugin-host/Cargo.toml` & `src/{lib.rs,error.rs,env.rs,runtime.rs}`
- Create: `crates/zerodeck-cef/Cargo.toml` & `src/{lib.rs,error.rs,tab.rs}`
- Create: `crates/zerodeck-server/Cargo.toml` & `src/{lib.rs,error.rs,auth.rs,router.rs}`
- Create: `crates/zerodeckd/Cargo.toml` & `src/main.rs`

**Interfaces:**
- `zerodeck-plugin-host`:
  - Consumes: `zerodeck-protocol`, `zerodeck-platform`, `tokio`, `thiserror`.
  - Produces: `PluginRuntime` trait (async `call_method`, `shutdown`), `PluginSpawnSpec`, `build_plugin_env(spec: &PluginSpawnSpec, paths: &PlatformPaths) -> BTreeMap<String, String>`.
- `zerodeck-cef`:
  - Consumes: `zerodeck-protocol`, `serde`, `thiserror`.
  - Produces: `CdpTabDescriptor`, `is_gamepadui_tab(tab: &CdpTabDescriptor) -> bool`, `CefConfig`.
- `zerodeck-server`:
  - Consumes: `zerodeck-protocol`, `zerodeck-core`, `zerodeck-cef`, `zerodeck-plugin-host`, `axum`, `tower`, `tower-http`, `tokio`, `uuid`, `thiserror`, `tracing`.
  - Produces: `ServerConfig`, `AppState`, `build_router(state: AppState) -> axum::Router`.
- `zerodeckd`:
  - Consumes: all workspace crates + `clap`, `anyhow`, `tracing`, `tracing-subscriber`, `tokio`.
  - Produces: `zerodeckd` CLI binary (`--version`, `--check-config`, `--healthcheck`, `--host`, `--port`).

- [ ] **Step 1: Write failing tests across `plugin-host`, `cef`, and `server`**
  - `zerodeck-plugin-host`: `build_plugin_env_sets_all_decky_vars_and_respects_root_flag` (checks `DECKY_HOME`, `DECKY_PLUGIN_SETTINGS_DIR`, `DECKY_PLUGIN_RUNTIME_DIR`, `DECKY_PLUGIN_LOG_DIR`, `DECKY_PLUGIN_DIR`, `USER`).
  - `zerodeck-cef`: `gamepadui_tab_matches_shared_ctx_and_steamloopback_url` (accepts `SharedJSContext` + `https://steamloopback.host/index.html`, rejects `about:blank` or non-Steam URLs).
  - `zerodeck-server`: using `tower::ServiceExt::oneshot`, verify:
    - `GET /auth/token` returns `200` with token matching `state.csrf_token`.
    - `GET /healthz` returns `200` (`{"status":"ok"}`).
    - `POST /methods/ping` without `X-Decky-Auth` returns `403 Forbidden`, and with valid `X-Decky-Auth` does not return `403`.
    - Invalid/unknown WS message frame parsed by protocol helper returns `ProtocolError`.

- [ ] **Step 2: Run tests to verify they fail**
  Run: `cargo test -p zerodeck-plugin-host -p zerodeck-cef -p zerodeck-server`
  Expected: FAIL.

- [ ] **Step 3: Implement `plugin-host`, `cef`, `server`, and `zerodeckd` crates**
  Implement clean trait/struct foundations, `axum` router with `tower_http::cors::CorsLayer` allowing `https://steamloopback.host` (`allow_credentials(true)`), CSRF middleware matching the clean-room spec, and `zerodeckd` binary (`--check-config` initializes `PlatformPaths` + `tracing` and exits 0; `--healthcheck` probes `http://127.0.0.1:<port>/healthz` via `reqwest` and exits 0/1; default mode binds server and waits for Ctrl+C/SIGTERM).

- [ ] **Step 4: Run full Rust workspace check**
  Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && cargo run -p zerodeckd -- --check-config`
  Expected: All workspace crates compile with 0 warnings, all tests pass, `--check-config` exits 0.

- [ ] **Step 5: Commit**
  ```bash
  git add crates/zerodeck-plugin-host crates/zerodeck-cef crates/zerodeck-server crates/zerodeckd
  git commit -m "feat(daemon): add plugin-host, cef, server, and zerodeckd foundations"
  ```

---

### Task 6: `hosts/python` (`zerodeck-py-host`) Foundation

**Files:**
- Create: `hosts/python/pyproject.toml`
- Create: `hosts/python/zerodeck_py_host/__init__.py`
- Create: `hosts/python/zerodeck_py_host/decky_api.py`
- Create: `hosts/python/zerodeck_py_host/ipc.py`
- Create: `hosts/python/zerodeck_py_host/runner.py`
- Create: `hosts/python/tests/test_runner.py`

**Interfaces:**
- Produces:
  - `zerodeck_py_host.decky_api`: exposes `HOME`, `USER`, `DECKY_VERSION`, `DECKY_USER`, `DECKY_USER_HOME`, `DECKY_HOME`, `DECKY_PLUGIN_SETTINGS_DIR`, `DECKY_PLUGIN_RUNTIME_DIR`, `DECKY_PLUGIN_LOG_DIR`, `DECKY_PLUGIN_DIR`, `DECKY_PLUGIN_NAME`, `DECKY_PLUGIN_VERSION`, `DECKY_PLUGIN_AUTHOR`, `logger`, `emit()`, `migrate_any()`, `migrate_settings()`, `migrate_runtime()`, `migrate_logs()`.
  - `zerodeck_py_host.runner.PluginRunner`: loads plugin `main.py`, supports `api_version = 0` (class-level `Plugin`) and `api_version >= 1` (instantiated `Plugin()`), registers `sys.modules["decky"]` and `sys.modules["decky_plugin"]`, appends `py_modules/`, dispatches JSON-line `CALL` (`0`) -> `RESPONSE` (`1`) and `emit()` -> `EVENT` (`2`).
  - Note: Keep Python syntax compatible with `>=3.9` in implementation files (using `from __future__ import annotations`, no 3.10+ `match`/`case`) while declaring `requires-python = ">=3.10"` for production so tests run cleanly on macOS dev `python3` (`3.9.6`) as well as SteamOS `3.10+`.

- [ ] **Step 1: Write failing Python unit tests in `hosts/python/tests/test_runner.py`**
  Tests using `unittest.IsolatedAsyncioTestCase`:
  - `test_api_version_0_legacy_kwargs_dispatch_and_decky_plugin_alias`: loads temp `main.py` importing `decky_plugin`, invokes legacy method with kwargs, asserts `{"type": 1, "id": ..., "success": True, "res": ...}`.
  - `test_api_version_1_positional_args_and_emit_event`: loads temp `main.py` importing `decky`, invokes positional method, calls `await decky.emit("tick", 42)`, asserts response + `{"type": 2, "event": "tick", "args": [42]}`.
  - `test_py_modules_is_added_to_sys_path`: verifies plugin importing helper from `<plugin_dir>/py_modules/` succeeds.

- [ ] **Step 2: Run test to verify it fails**
  Run: `PYTHONPATH=hosts/python python3 -m unittest discover -s hosts/python/tests -v`
  Expected: FAIL (`ModuleNotFoundError`).

- [ ] **Step 3: Implement `hosts/python/pyproject.toml` and `zerodeck_py_host` modules**
  Configure `[project]`, `[tool.ruff]`, `[tool.pyright]` in `pyproject.toml`, and implement `decky_api.py`, `ipc.py` (Unix socket path & `--fd` inherited FD transport), and `runner.py`.

- [ ] **Step 4: Run Python tests to verify they pass**
  Run: `PYTHONPATH=hosts/python python3 -m unittest discover -s hosts/python/tests -v`
  Expected: PASS (3 tests OK).

- [ ] **Step 5: Commit**
  ```bash
  git add hosts/python
  git commit -m "feat(py-host): add zerodeck-py-host foundation with v0/v1 plugin compatibility"
  ```

---

### Task 7: `frontend` (`@zerodeck/frontend`) Foundation with `pnpm` & TypeScript 7.0.2

**Files:**
- Create: `frontend/package.json`
- Create: `frontend/tsconfig.json`
- Create: `frontend/vite.config.ts`
- Create: `frontend/src/wsrouter.ts`
- Create: `frontend/src/index.ts`
- Create: `frontend/src/fallback.ts`
- Create: `frontend/tests/wsrouter.test.ts`

**Interfaces:**
- Produces:
  - `frontend/package.json`: `"name": "@zerodeck/frontend"`, `"private": true`, `"license": "MIT"`, `"type": "module"`, `"packageManager": "pnpm@12.16.1"`, `devDependencies`: `"typescript": "^7.0.2"`, `"vite": "^8.3.8"`, `"@biomejs/biome": "^2.6.9"`, `"@types/react": "^19.1.0"`, `"@types/react-dom": "^19.1.0"`, `"react": "^19.3.1"`, `"react-dom": "^19.3.1"`, `"@decky/api": "^1.1.3"`, `"@decky/ui": "^5.0.2"`.
  - `WSRouter` class in `frontend/src/wsrouter.ts`: implements `MessageType` (`-1..5`), `call()`, `callable()`, `addEventListener()`, `removeEventListener()`, `FULL_SYNC` reconnect tracking, `PyError`.
  - `vite.config.ts`: builds ESM bundle (`dist/index.js` + `dist/fallback.js`), externalizes `react`, `react/jsx-runtime`, `react-dom` to global `SP_REACT`, `SP_JSX`, `SP_REACTDOM` via Rollup/Rolldown `output.globals` / external plugin.

- [ ] **Step 1: Write failing test `frontend/tests/wsrouter.test.ts` & package scaffold**
  Create `frontend/package.json`, `frontend/tsconfig.json` (`strict: true`, `target: "ES2024"`, `module: "ESNext"`, `moduleResolution: "Bundler"`, `jsx: "react-jsx"`), and `frontend/tests/wsrouter.test.ts` (using `node:test` + `node:assert/strict`) testing:
  - `WSRouter.call()` sends `MessageType.CALL (0)` with incrementing `id` and resolves on `MessageType.REPLY (1)`, sending `MessageType.RECEIVED_RESPONSE (3)`.
  - `WSRouter` rejects with `PyError` on `MessageType.ERROR (-1)`.
  - `WSRouter` dispatches `MessageType.EVENT (5)` to registered listeners and `FULL_SYNC (4)` resends in-flight calls on socket open.

- [ ] **Step 2: Install dependencies with `pnpm` and run test to verify failure**
  Run: `pnpm --dir frontend install && node --test frontend/tests/wsrouter.test.ts`
  Expected: FAIL (`Cannot find module '../src/wsrouter'`).

- [ ] **Step 3: Implement `wsrouter.ts`, `index.ts`, `fallback.ts`, and `vite.config.ts`**
  Implement clean, Biome-compliant TypeScript modules and Vite 8 config outputting `frontend/dist/index.js` and `frontend/dist/fallback.js`.

- [ ] **Step 4: Run all frontend & full-monorepo checks**
  Run: `node --test frontend/tests/wsrouter.test.ts && just fe-check && just check`
  Expected: All unit tests (`wsrouter`, Rust workspace, Python `unittest`), Biome check, `tsc --noEmit` (TypeScript 7.0.2), `vite build`, `cargo fmt --check`, and `cargo clippy` PASS with 0 errors/warnings.

- [ ] **Step 5: Commit**
  ```bash
  git add frontend pnpm-lock.yaml
  git commit -m "feat(frontend): add @zerodeck/frontend foundation with pnpm, TS 7.0.2, Vite 8, and Biome"
  ```

---

### Task 8: Dockerization (`Dockerfile`, `.dockerignore`, `compose.yaml`, `justfile` & CI Docker Gate)

**Files:**
- Create: `.dockerignore`
- Create: `Dockerfile`
- Create: `compose.yaml`
- Modify: `justfile` (add `docker-build`, `docker-test`, `docker-up`, `docker-down`)
- Modify: `.github/workflows/ci.yml` (add `docker` job with `docker/setup-buildx-action` + `docker/build-push-action` `load: false` cache build)

**Interfaces:**
- Consumes: Workspace `Cargo.toml`, `crates/*`, `hosts/python`, `frontend/package.json` + `pnpm-lock.yaml`, `zerodeckd` (`/healthz` and `--check-config`).
- Produces: Hardened multi-stage `Dockerfile` (`frontend-builder`, `chef`, `planner`, `rust-builder`, `linux-test`, `runtime`), `.dockerignore`, `compose.yaml`, and verified Linux container smoke test.

- [ ] **Step 1: Write `.dockerignore` and hardened multi-stage `Dockerfile`**
  - `.dockerignore`: ignore `.git`, `target`, `**/node_modules`, `frontend/dist`, `docs`, `.github`, `.venv`, `**/__pycache__`, `.dev-homebrew`, `*.log`.
  - `Dockerfile` (`# syntax=docker/dockerfile:1`):
    1. `frontend-builder` (`node:26-bookworm-slim`): enable corepack / install `pnpm@12.16.1`, copy `pnpm-workspace.yaml`, `biome.json`, `frontend/package.json`, `frontend/pnpm-lock.yaml`, run `pnpm --dir frontend install --frozen-lockfile` with `--mount=type=cache,target=/root/.local/share/pnpm/store`, copy `frontend/`, run `pnpm --dir frontend build`.
    2. `chef` (`rust:1-slim-bookworm`): install `pkg-config` & `cargo-chef` (and `python3` in `linux-test`), prepare `planner` stage (`cargo chef prepare --recipe-path recipe.json`).
    3. `rust-builder` (`chef`): `cargo chef cook --release --recipe-path recipe.json` (BuildKit cargo cache mounts), copy `crates/`, build `cargo build --release --bin zerodeckd`.
    4. `linux-test` (`rust-builder` + `python3`): copies `hosts/python`, runs `cargo test --workspace` and `PYTHONPATH=hosts/python python3 -m unittest discover -s hosts/python/tests -v` inside Linux.
    5. `runtime` (`debian:bookworm-slim`): installs `ca-certificates`, `python3`, `curl` (or minimal Rust healthcheck flag `zerodeckd --healthcheck` so `curl` isn't even needed!), creates non-root user/group `zerodeck` (`uid=1000`), copies `zerodeckd` to `/usr/local/bin/zerodeckd`, `hosts/python/zerodeck_py_host` to `/opt/zerodeck/python`, `frontend/dist` to `/opt/zerodeck/frontend`, creates `/homebrew` & `/run/zerodeck`, sets OCI labels (`org.opencontainers.image.*`), `USER zerodeck`, `EXPOSE 1337`, `STOPSIGNAL SIGTERM`, `HEALTHCHECK` calling `/usr/local/bin/zerodeckd --healthcheck` (or HTTP probe), `ENTRYPOINT ["/usr/local/bin/zerodeckd"]`.
  *(Note: add `--healthcheck` flag or minimal HTTP probe to `zerodeckd` in Task 5 or via `zerodeckd` CLI so `runtime` stays minimal without `curl`!)*

- [ ] **Step 2: Create `compose.yaml` and update `justfile` & `.github/workflows/ci.yml`**
  - `compose.yaml` (modern Compose spec, no `version:` field):
    - `zerodeckd` service building `target: runtime`, `init: true`, `read_only: true`, `cap_drop: ["ALL"]`, `security_opt: ["no-new-privileges:true"]`, `tmpfs: ["/tmp:rw,noexec,nosuid,size=64m", "/run/zerodeck:rw,nosuid,size=16m"]`, `volumes: ["./.dev-homebrew:/homebrew"]`, `environment` (`UNPRIVILEGED_PATH=/homebrew`, `PRIVILEGED_PATH=/homebrew`, `SERVER_HOST=0.0.0.0`, `SERVER_PORT=1337`, `RUST_LOG=info`), `ports: ["127.0.0.1:1337:1337"]`.
    - Optional `test` profile service (`profiles: ["test"]`, `target: linux-test`) for one-command Linux test execution.
  - Update `justfile` (`docker-build`, `docker-test`, `docker-up`, `docker-down`) and `.github/workflows/ci.yml` (`docker` job validating `docker compose config --quiet` and `docker buildx build`).

- [ ] **Step 3: Validate Compose syntax, build image, and run container smoke checks**
  Run: `docker compose config --quiet && docker build --target linux-test -t zerodeck:test . && docker build --target runtime -t zerodeck:runtime . && docker run --rm --read-only --cap-drop=ALL --security-opt=no-new-privileges:true zerodeck:runtime --check-config`
  Expected: Compose config validates cleanly, `linux-test` passes all Rust + Python tests inside Linux, `runtime` builds cleanly, and `zerodeckd --check-config` exits 0 as non-root `zerodeck` inside a read-only container.

- [ ] **Step 4: Commit**
  ```bash
  git add .dockerignore Dockerfile compose.yaml justfile .github/workflows/ci.yml
  git commit -m "chore(docker): add hardened multi-stage Dockerfile, compose.yaml, and CI build gate"
  ```

---

## Self-Review Results
1. **Spec coverage:** Every section of Phase 1 in `docs/superpowers/specs/2026-11-24-zerodeck-foundation-design.md` (repo hygiene, MIT license, `deny.toml`, `justfile`, CI, `docs/architecture/wire-protocol.md`, all 7 crates, `hosts/python`, `frontend/` with `pnpm` + `TypeScript 7.0.2` + `SP_REACT` externalization, and Dockerization) maps 1:1 to Tasks 1–8.
2. **Step scan:** Every step specifies exact files, test names/assertions, signatures, and verification commands without placeholder TBDs or full code transcripts.
3. **Type consistency:** `WsMessageType`, `WsClientMessage`, `WsServerMessage`, `PluginManifest`, `PackageManifest`, `SafePath`, `SafePathSet`, `SettingsManager`, `PluginRuntime`, `CdpTabDescriptor`, and `WSRouter` names match across all tasks.
4. **Review Focus:** All 5 edge-case/failure modes listed in `Review Focus` have explicit tests in Tasks 2, 3, 4, 5, and 6.
5. **Proportion:** Concise task plan (~340 lines), well-proportioned to the 225-line design spec.
