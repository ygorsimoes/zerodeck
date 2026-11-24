# ZeroDeck — Architecture & Repository Foundation Design

**Date:** 2026-11-24  
**Status:** Ready for Approval  
**License:** MIT  
**Target:** Clean-room, high-performance, security-hardened Rust replacement for `decky-loader` with full Decky plugin compatibility.

---

## 1. Intent & Why

`decky-loader` established the Steam Deck / SteamOS plugin ecosystem, but its architecture carries structural limitations inherent to a single-process Python/PyInstaller daemon:

- **Runtime footprint & performance:** Python GIL contention, PyInstaller startup overhead, and `aiohttp` memory footprint running continuously in the background on handheld hardware.
- **Security weaknesses:** Permissive filesystem operations (`chmod 777` in binary downloads), missing path-traversal canonicalization on plugin asset routes, unguarded ZIP extraction (Zip-Slip risk), and coarse privilege boundaries.
- **Reliability gaps:** Non-atomic settings writes (`open(..., "w+")` risking JSON corruption), swallowed exceptions (`except: pass`) across IPC and CEF loops, brittle `/tmp/plugin_socket_{time}` socket paths, and lack of structured supervision/backoff per plugin.

**ZeroDeck** solves these at the root by rebuilding the core daemon in **modern Rust (Edition 2024)** while preserving **100% protocol and runtime compatibility** with existing Decky Loader plugins (`plugin.json`, `package.json`, `ESMODULE_V1`, `LEGACY_EVAL_IIFE`, `@decky/api`, `@decky/ui`, `main.py` + `decky`/`decky_plugin`).

---

## 2. Goals, Non-Goals, and Anti-Goals

### Goals
1. **Superior core in Rust:** Low-memory, zero-GIL, async-native daemon (`zerodeckd`) with strict error handling, structured telemetry (`tracing`), and clean modular crate boundaries.
2. **100% Decky plugin compatibility:** Existing Decky plugins (passive, `LEGACY_EVAL_IIFE`, `ESMODULE_V1`, `api_version` 0 and 1+, `py_modules/`, `remote_binary`, `"root"` flag) run unmodified.
3. **Security-by-default foundation:**
   - Path-jail canonicalization (`SafePath`) on every plugin file/asset/data/dist HTTP endpoint and `filepicker` request (multi-root allowlist defaulting to `$HOME`, `/run/media`, and `DECKY_HOME`).
   - Zip-Slip validation on all archive extractions + fail-closed SHA-256 verification for remote Store installs and `remote_binary` downloads (configurable opt-in `developer.allow_unverified_installs` for dev overrides; local `file://` sideloads require explicit install confirmation and audit logging of computed SHA-256).
   - Atomic file writes (temp file + `fsync` + `rename`) for settings and metadata.
   - Isolated plugin IPC using inherited Unix `socketpair()` file descriptors (or per-plugin `0700` runtime directories chowned to the plugin's target `uid:gid`), eliminating predictable `/tmp` paths and `EACCES` permission bugs when dropping privileges.
   - Explicit privilege dropping (`uid`/`gid`) per plugin from the `root`-supervised `zerodeckd` service unless `"root"` is declared in `plugin.json`, structured to support Linux hardening (Landlock/seccomp/cgroups) in later milestones.
4. **Modern monorepo & developer experience:** Strict lints, formatting, security/license auditing, reproducible builds, and fast CI for Rust, Python host, and TypeScript frontend.

### Non-Goals (for the Foundation Milestone)
- Reimplementing every Store/Settings React view in full visual polish during Phase 1 (Phase 1 establishes the workspace, build pipelines, shared contracts, core platform/protocol crates, and runnable skeleton).
- Supporting Windows or macOS Steam client injection in production (Linux/SteamOS `x86_64` is primary; macOS/non-Linux hosts must compile and pass unit/integration tests via `zerodeck-platform` abstractions).
- Embedding a Python interpreter (`PyO3`) or writing a custom Python VM (`RustPython`).

### Anti-Goals
- **Never** copy GPL-licensed code from `decky-loader` — ZeroDeck is a clean-room implementation targeting public protocols and plugin contracts.
- **Never** silently swallow errors in IPC, filesystem, or CEF loops.
- **Never** allow plugins to bypass path canonicalization or execute without explicit privilege classification.

---

## 3. Compatibility & Platform Constraints

Based on a full audit of `decky-loader`, ZeroDeck targets the **`decky-loader` v3.x (`@decky/api` v1+ & `@decky/ui` v5) baseline** while maintaining backward compatibility for legacy `api_version = 0` (`LEGACY_EVAL_IIFE`) plugins. Public wire/IPC schemas will be documented clean-room under `docs/architecture/` alongside implementation.

1. **Filesystem Layout & Privilege Paths (`PRIVILEGED_PATH` / `UNPRIVILEGED_PATH` / `DECKY_HOME`):**
   - `UNPRIVILEGED_PATH` (default `$DECKY_USER_HOME/homebrew`, e.g., `/home/deck/homebrew`): `settings/`, `data/`, `logs/`.
   - `PRIVILEGED_PATH` (defaults to `UNPRIVILEGED_PATH` unless overridden via env): `plugins/`, daemon runtime state, and `services/`.
   - Inside `plugins/<plugin_dir>/`: `plugin.json`, optional `package.json`, optional `main.py`, optional `py_modules/`, `dist/index.js`, `dist/assets/`.
   - `zerodeckd` runs as `root` (via `systemd`) to support `"root"`-flagged plugins and `systemctl` integration, and explicitly drops `uid`/`gid` to the host user (`deck`) for all standard plugins.
2. **HTTP & WebSocket Server (`127.0.0.1:1337` by default, configurable via `SERVER_HOST`/`SERVER_PORT`):**
   - `GET /auth/token` (ephemeral CSRF token; `X-Decky-Auth` header or `?auth=` query parameter)
   - CORS strictly restricted to `https://steamloopback.host` (`allow_credentials = true`)
   - `GET /ws?auth=<token>` implementing `WSRouter` (`ERROR=-1`, `CALL=0`, `REPLY=1`, `DISCARD=2`, `RECEIVED_RESPONSE=3`, `FULL_SYNC=4`, `EVENT=5`)
   - Static & plugin routes: `/frontend/*`, `/static/*`, `/locales/*`, `/plugins/{name}/frontend_bundle`, `/plugins/{name}/dist/*`, `/plugins/{name}/assets/*`, `/plugins/{name}/data/*`
   - Streaming CORS proxy: `/fetch?auth=<token>&fetch_url=<url>` supporting `X-Decky-Header-*` and `X-Decky-Fetch-Excluded-Headers`
   - Legacy RPC compatibility: `POST /methods/{method_name}` and `loader/call_legacy_plugin_method`
   - WS routes: `loader/*`, `utilities/*`, `updater/*`, `instrumentation/resolve_scope`
3. **CEF Injection (`http://127.0.0.1:8080/json` + CDP WebSocket):**
   - Locate GamepadUI tab (`SharedJSContext`, `Steam Shared Context presented by Valve™`, `Steam`, `SP` at `https://steamloopback.host/routes/` or `/index.html`).
   - Inject loader bootstrap via `Runtime.evaluate`, monitor `Page.domContentEventFired` & `Inspector.detached`, fall back to `fallback.js` after crash loops.
   - Implement `instrumentation/resolve_scope` (`Runtime.getProperties` -> `[[Scopes]]` -> `Runtime.callFunctionOn`) for `@decky/ui` module resolution.
4. **Plugin Backend IPC & Environment:**
   - Environment variables: `HOME`, `USER`, `DECKY_VERSION`, `DECKY_USER`, `DECKY_USER_HOME`, `DECKY_HOME`, `DECKY_PLUGIN_SETTINGS_DIR`, `DECKY_PLUGIN_RUNTIME_DIR`, `DECKY_PLUGIN_LOG_DIR`, `DECKY_PLUGIN_DIR`, `DECKY_PLUGIN_NAME`, `DECKY_PLUGIN_VERSION`, `DECKY_PLUGIN_AUTHOR`.
   - Newline-delimited JSON over Unix Domain Socket (`CALL=0`, `RESPONSE=1`, `EVENT=2`, plus shutdown/uninstall signal).
   - Python imports: `decky` and `decky_plugin` alias, plus `py_modules/` added to `sys.path`.

---

## 4. Approaches Considered

### Backend Plugin Execution
| Approach | Summary | Trade-offs | Decision |
|---|---|---|---|
| **A. Rust Core + External Python Host (`zerodeck-py-host`)** | Rust daemon owns all HTTP/WS, CEF, storage, supervision, and plugin lifecycle; spawns an isolated `python3` worker only for plugins with `main.py` over a private Unix socket. | Requires host `python3` (present on SteamOS) or bundled runtime; clean static Rust builds, zero GIL in daemon, per-plugin crash & privilege isolation, natural path for future native Rust/WASM plugins. | **Selected** |
| **B. Embedded CPython (`PyO3`)** | Embed `libpython` directly in the Rust binary. | Couples binary to host `libpython` ABI, reintroduces GIL & C-extension crash blast radius into core daemon, breaks clean static cross-compilation. | Rejected |
| **C. `RustPython` VM** | Pure-Rust Python interpreter. | Incompatible with existing plugins using native C extensions (`evdev`, `psutil`, `ctypes`, `py_modules` wheels) or full `asyncio` stdlib parity. | Rejected |

### Repository Topology
| Approach | Summary | Trade-offs | Decision |
|---|---|---|---|
| **Monorepo (Cargo Workspace + `hosts/python` + `frontend`)** | Single repository with shared protocol definitions, unified CI, synchronized releases, and root task runner (`justfile`). | Slightly richer root tooling setup up front, prevents schema drift between Rust daemon, Python host, and CEF frontend. | **Selected** |
| **Split Repositories** | Separate repos for daemon, Python shim, and frontend. | Protocol drift risk during active development, extra release coordination overhead. | Rejected |

---

## 5. Monorepo Architecture & Modern Tech Stack

### Directory Layout
```text
zerodeck/
├── Cargo.toml                  # Rust 2024 workspace, shared deps & strict clippy/rustc lints
├── Cargo.lock
├── rust-toolchain.toml         # Pinned stable Rust toolchain + clippy/rustfmt components
├── rustfmt.toml                # Formatting rules
├── deny.toml                   # cargo-deny (licenses, advisories, banned/duplicate crates)
├── justfile                    # Unified dev/test/lint/build commands
├── biome.json                  # Modern Rust-based JS/TS formatter & linter
├── pnpm-workspace.yaml         # Frontend workspace
├── .editorconfig
├── .gitignore
├── .dockerignore
├── Dockerfile                  # Multi-stage BuildKit (frontend pnpm -> cargo-chef Rust -> minimal runtime)
├── compose.yaml                # Modern Compose spec (init, read_only, tmpfs, cap_drop, healthcheck)
├── .github/
│   └── workflows/
│       └── ci.yml              # Rust + Python + Frontend + Docker CI gates
├── docs/
│   ├── architecture/           # ADRs and protocol specs
│   └── superpowers/specs/      # Design specifications
├── crates/
│   ├── zerodeck-protocol/      # Pure types: WS messages, Plugin IPC, plugin.json/package.json schemas
│   ├── zerodeck-platform/      # OS paths, atomic FS, path-jail, uid/gid/privilege drop, service control
│   ├── zerodeck-core/          # Settings, plugin registry, ZIP installer (Zip-Slip/SHA-256), filepicker, hot-reload
│   ├── zerodeck-plugin-host/   # PluginRuntime trait + out-of-process Python host supervisor & Unix socket transport
│   ├── zerodeck-cef/           # CDP client, GamepadUI discovery, reinjector, crash-loop guard, V8 scope resolver
│   ├── zerodeck-server/        # Axum HTTP + WS server, CSRF, CORS, /ws FULL_SYNC, /fetch streaming proxy
│   └── zerodeckd/              # Main daemon binary (clap CLI, tracing init, graceful shutdown wiring)
├── hosts/
│   └── python/                 # zerodeck-py-host (uv/pyproject.toml, ruff, pyright, decky/decky_plugin shim)
├── frontend/                   # TypeScript + React 19 + Vite/Rolldown CEF bundle (@zerodeck/frontend)
└── deploy/
    └── systemd/                # Hardened zerodeck.service templates
```

### Crate Dependency Graph (Acyclic)
```text
zerodeckd
 ├── zerodeck-server
 │    ├── zerodeck-core
 │    ├── zerodeck-cef
 │    ├── zerodeck-plugin-host
 │    └── zerodeck-protocol
 ├── zerodeck-cef ─────────── zerodeck-protocol
 ├── zerodeck-core ────────── zerodeck-platform, zerodeck-plugin-host, zerodeck-protocol
 └── zerodeck-plugin-host ─── zerodeck-platform, zerodeck-protocol
```
- `zerodeck-protocol` has **zero** internal dependencies (only `serde`, `serde_json`, `uuid`).
- `zerodeck-platform` depends only on `zerodeck-protocol` + OS crates (`rustix` / `nix`, `tokio`).

### Modern Technology Choices (Builder Calls)
- **Rust (`edition = "2024"`, `rust-version = "1.88"` / pinned stable via `rust-toolchain.toml`):**
  - Async runtime: `tokio` (full features selectively enabled)
  - HTTP / WebSocket server: `axum` 0.8 + `tower` + `tower-http` (CORS, TraceLayer, compression)
  - HTTP client & streaming proxy: `reqwest` with `rustls` (zero OpenSSL system dependency)
  - CEF CDP WebSocket client: `tokio-tungstenite`
  - Serialization & errors: `serde`, `serde_json`, `thiserror` (library crates), `anyhow` (binary crate `zerodeckd`)
  - Observability: `tracing`, `tracing-subscriber` (JSON/compact structured logs, env-filter)
  - CLI & FS watching: `clap` (derive), `notify` (cross-platform hot reload)
  - Security & archives: `sha2`, `hex`, `zip`, `uuid`
  - Unix/Linux syscalls: `rustix` / `nix` for safe `uid`/`gid` privilege dropping and permissions
- **Python Plugin Host (`hosts/python`):**
  - Uses the **system `python3`** (`>=3.10`) on SteamOS/Linux so `zerodeckd` remains a lean native binary.
  - Managed via `pyproject.toml` with zero third-party runtime dependencies in the shim itself (pure stdlib `asyncio` + `json` + `importlib` for instant startup), plus an optional managed compatibility `venv` for legacy plugins that expect loader-bundled libs (e.g., `aiohttp`, `certifi`), in addition to each plugin's `py_modules/`.
  - Quality gates: `ruff` (lint + format) and `pyright` (strict type checking), runnable via `uv`.
- **Frontend (`frontend/`):**
  - **`pnpm` exclusive** (`packageManager: "pnpm@12.16.1"`, workspace via `pnpm-workspace.yaml`; `npm` is not used) + **TypeScript `^7.0.2`** (latest stable native TS compiler; strict mode) + `React 19` (`^19.3.1` types/dev) + `Vite ^8.3.8` (Rolldown-powered ESM bundle output targeting CEF + `fallback.js`) + `@biomejs/biome ^2.6.9` for linting/formatting.
  - **React externalization (Builder Call):** Externalizes `react`, `react/jsx-runtime`, and `react-dom` to Steam CEF's globals (`SP_REACT`, `SP_JSX`, `SP_REACTDOM`) via Vite/Rollup config, consuming `@decky/ui` and `@decky/api` contracts so plugins and ZeroDeck share Steam's live React tree without duplicate-React hook mismatches.
- **License & Dependency Governance (`Cargo.toml` / `deny.toml`):**
  - Published under **MIT**.
  - `cargo-deny` allows permissive OSI licenses (`MIT`, `Apache-2.0`, `BSD-2-Clause`, `BSD-3-Clause`, `ISC`, `Zlib`, `Unicode-3.0`) and denies copyleft (`GPL-*`, `AGPL-*`) dependencies.
- **Containerization (`Dockerfile`, `.dockerignore`, `compose.yaml`):**
  - Multi-stage BuildKit `Dockerfile`: `frontend-builder` (`node:26-bookworm-slim` + `corepack`/pinned `pnpm` + `--mount=type=cache`), `rust-chef` / `rust-builder` (`rust:slim-bookworm` + `cargo-chef` dependency layer caching), `linux-test` target (runs Rust + Python + frontend tests inside Linux), and minimal `runtime` (`debian:bookworm-slim` with `python3`, `ca-certificates`, `zerodeckd`, `zerodeck_py_host`, frontend static bundle, non-root `zerodeck` user by default, `STOPSIGNAL SIGTERM`, `HEALTHCHECK` on `/healthz`, OCI labels).
  - Modern `compose.yaml` (no deprecated `version:` key) with `init: true` (clean zombie reaping), `read_only: true`, `tmpfs` for `/tmp` and `/run/zerodeck`, `cap_drop: ["ALL"]`, `security_opt: ["no-new-privileges:true"]`, and bind-mounted dev `./.dev-homebrew` volume.

---

## 6. Security & Reliability Design Highlights

1. **Path-Jail Canonicalization (`zerodeck-platform`):**
   - Every `/plugins/{name}/dist/*`, `/plugins/{name}/assets/*`, `/plugins/{name}/data/*`, `/frontend/*`, `/locales/*`, and `filepicker` operation resolves symlinks/components through a `SafePath` helper verifying `canonical_target.starts_with(&canonical_root)` against one or more configured roots (for `filepicker`: `$HOME`, `/run/media`, and `DECKY_HOME`) before any read.
2. **Atomic Persistence (`zerodeck-platform` + `zerodeck-core`):**
   - `SettingsManager` writes to `<name>.json.tmp.<pid>`, calls `sync_all()`, and atomically `rename()`s over `<name>.json`, preventing half-written JSON on power loss.
3. **Zip-Slip & Hash Enforcement (`zerodeck-core`):**
   - Every archive entry is validated with `enclosed_name()` (rejecting `..`, absolute paths, and unexpected symlinks).
   - Remote Store installs and `package.json` `remote_binary` entries (`sha256hash: Option<String>` in `zerodeck-protocol`) enforce mandatory SHA-256 verification (fail-closed if missing/mismatched unless `developer.allow_unverified_installs` is explicitly enabled).
   - Local `file://` ZIP sideloads without a hash require explicit confirmation in the install flow and log the computed SHA-256; extracted/downloaded binaries receive `0755` (never `0777`).
4. **Isolated Plugin IPC (`zerodeck-plugin-host`):**
   - On Unix, `zerodeckd` creates an anonymous `UnixStream::pair()` (`socketpair`) and passes the child FD across `exec` after dropping `uid`/`gid`, eliminating filesystem socket races and `EACCES` errors; when a filesystem socket path is needed, `zerodeckd` creates a per-plugin `0700` directory chowned to the plugin's target `uid:gid` and passes `--socket-path` to `zerodeck-py-host`.
   - Supervisor tracks process health, enforces bounded shutdown (`SIGTERM` -> grace timeout -> `SIGKILL`), cancels in-flight RPCs cleanly with `PluginStopped` -> `MessageType::DISCARD`, and isolates plugin crashes.
5. **Extensible Runtime Trait:**
   - `PluginRuntime` trait in `zerodeck-plugin-host` abstracts `PythonHostProcess` today and lets ZeroDeck support native Rust binaries or WASM plugins in the future without touching `zerodeck-core` or `zerodeck-server`.

---

## 7. Phasing & Foundation Scope (What Gets Built First)

### Phase 1 — Repository & Core Foundation (Current Target)
1. **Repository & Tooling Configuration:** Root `Cargo.toml` workspace with strict `clippy`/`rust` lints, `rust-toolchain.toml`, `rustfmt.toml`, `deny.toml`, `biome.json`, `pnpm-workspace.yaml`, `justfile`, `.editorconfig`, `.gitignore`, and GitHub Actions CI.
2. **Core Crate Skeletons & Foundational Implementations:**
   - `zerodeck-protocol`: Complete typed definitions for `WsRouter` wire messages (`MessageType`, `CallMessage`, `ReplyMessage`, `ErrorMessage`, `DiscardMessage`, `ReceivedResponseMessage`, `FullSyncMessage`, `EventMessage`), `PluginIpcMessage` (`Call`, `Response`, `Event`, `Control`), `PluginManifest` (`plugin.json`), and `PackageManifest` (`package.json`) with unit tests verifying wire JSON parity with `decky-loader`.
   - `zerodeck-platform`: Path resolution (`PRIVILEGED_PATH` / `UNPRIVILEGED_PATH` / `DECKY_HOME`), `SafePath` path-jail utility, `atomic_write` helper, and OS user/privilege types with thorough tests (including path-traversal and symlink-escape tests).
   - `zerodeck-core`: Atomic `SettingsManager` + plugin discovery (`plugin.json`/`package.json` parsing & `PluginLoadType` detection) + initial tests.
   - Stubs/foundations with clean traits and compiling module boundaries for `zerodeck-plugin-host`, `zerodeck-cef`, `zerodeck-server`, and `zerodeckd` (runnable `--help` / `--version` + config load + healthcheck/graceful shutdown).
3. **`hosts/python` Foundation:** `pyproject.toml`, `zerodeck_py_host` package skeleton implementing the `decky`/`decky_plugin` module surface and Unix socket IPC protocol, plus unit tests.
4. **`frontend` Foundation:** `package.json`, `tsconfig.json`, `vite.config.ts`, minimal `@zerodeck/frontend` entry + `WSRouter` TypeScript client skeleton passing `pnpm exec biome check`, `pnpm exec tsc --noEmit`, and `pnpm build`.
5. **Dockerization & Linux Parity:** `.dockerignore`, multi-stage `Dockerfile` (`frontend-builder`, `rust-builder` with `cargo-chef`, `linux-test`, `runtime`), hardened `compose.yaml`, `justfile` Docker targets, and CI image build validation.

### Subsequent Phases (Out of Scope for Phase 1 Plan, Informed by This Design)
- **Phase 2:** Full `zerodeck-plugin-host` process supervisor & `zerodeck-py-host` parity (tested against real Decky Python plugins).
- **Phase 3:** Full `zerodeck-server` (Axum HTTP/WS, `FULL_SYNC`, `/fetch` streaming proxy, plugin static/data routes, FilePicker) & `zerodeck-core` ZIP/Store installer.
- **Phase 4:** Full `zerodeck-cef` CDP injector, V8 `[[Scopes]]` resolver, crash-loop recovery, and full `@zerodeck/frontend` Steam QAM UI.

---

## 8. Settled Architect Calls & What's Left to the Builder

### Settled Calls (for user review)
1. **SHA-256 Policy for Sideloads vs. Remote Installs:** Remote Store ZIPs and `remote_binary` entries fail closed if `sha256` is missing or mismatched (overrideable only via explicit `developer.allow_unverified_installs` opt-in). Local `file://` ZIP sideloads remain usable with explicit user confirmation and SHA-256 audit logging, plus mandatory Zip-Slip checks.
2. **Privilege & Socket IPC Model:** `zerodeckd` runs as `root` (matching SteamOS `decky-loader` requirements for `"root"` plugins) and drops `uid`/`gid` for standard plugins. IPC uses inherited Unix `socketpair()` FDs (or per-plugin `0700` directories chowned to target `uid:gid`) to prevent `EACCES`.
3. **Frontend React Externalization:** `@zerodeck/frontend` externalizes `react`, `react/jsx-runtime`, and `react-dom` to Steam's `SP_REACT`/`SP_JSX`/`SP_REACTDOM` globals.
4. **Protocol Baseline & Clean-Room Docs:** Targets `decky-loader` v3.x (`@decky/api` v1+ & `@decky/ui` v5) + legacy `api_version = 0` compatibility, with clean-room wire/IPC specs documented under `docs/architecture/`.
5. **`filepicker` Roots:** `SafePath` multi-root allowlist defaulting to `$HOME`, `/run/media`, and `DECKY_HOME`.

### Left to the Builder
- Exact crate patch/minor version pins when generating `Cargo.toml` / `package.json` (using current stable crates compatible with Rust 2024).
- Exact internal module split inside each crate (e.g., `error.rs`, `config.rs`) as long as public boundaries and acyclic dependencies hold.
- Choosing whether `zerodeckd` default binary config uses `systemd` unit names `zerodeck.service` while supporting env overrides for `~/homebrew`.
