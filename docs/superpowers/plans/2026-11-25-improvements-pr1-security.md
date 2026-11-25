# PR 1 — Security and robustness Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the security and robustness defects S1–S7 (plus the privileged test job I1) from the general-improvements design, each pinned by a test.

**Architecture:** Fixes stay inside the crate that owns the behavior. One new pure module per
cross-cutting check: `zerodeck-platform::ownership` (root-only tree check) and
`zerodeck-core::preflight` (decides whether a plugin may spawn and why not). Plugins that fail
preflight are recorded by `PluginManager` with a reason that `loader/get_plugins` exposes.

**Tech Stack:** Rust 1.100 (tokio, axum, rustix, clap), TypeScript (node:test), Docker compose, just, GitHub Actions.

**Spec:** `docs/superpowers/specs/2026-11-25-general-improvements-design.md` (PR 1 section, S1–S7, I1)

**Branch:** `fix/security-hardening` from `main`. First commit adds the spec and the four plan files:
`docs: add general improvements design and implementation plans`.

## Global Constraints

- MIT clean-room; no GPL/copyleft code; new deps must pass `deny.toml`.
- Crate graph from `AGENTS.md` stays acyclic; `zerodeck-protocol` has no internal deps.
- Never weaken: `SafePath` jails, `atomic_write`, Zip-Slip/`0755`/fail-closed SHA-256, uid/gid drop, socketpair IPC, CORS `https://steamloopback.host` + `X-Decky-Auth`/`?auth=`.
- Decky wire format unchanged; additions are optional fields/params only.
- Conventional Commits, one commit per task; no manual version bumps.
- Every `unsafe` block keeps a `// SAFETY:` comment.
- This PR touches `compose.yaml`, `justfile`, `Dockerfile`, `.github/**`: finish with `just ci` (which includes `just check`).

## Review Focus

1. `--host localhost` resolving to both `127.0.0.1` and `::1` must still start (all loopback) — test in Task 3.
2. A `session` query value with odd characters (`../`, spaces, 200 chars) must be rejected with `400`, not keyed — test in Task 2.
3. A `"root"` plugin reloaded by the debug watcher must go through the same preflight as startup — test in Task 4.
4. Running the daemon as a normal user (macOS / dev) must never block a plugin for ownership — test in Task 4.
5. `loader/set_setting` with key `developer` and an object containing `allow_unsafe_root_plugins` must be rejected like the dotted key — test in Task 4.

---

### Task 1: Workers exit with the daemon after privilege drop (S1) + privileged test job (I1)

**Files:**
- Modify: `crates/zerodeck-plugin-host/src/supervisor.rs` (`build_isolated_command`, pre_exec block ~lines 760–805)
- Modify: `crates/zerodeckd/tests/daemon.rs` (new tests; reuse `write_e2e_plugin`, `Daemon`, `TempHome`)
- Modify: `compose.yaml` (service `privileged-test`, volumes `privileged-target`, `privileged-cargo-home`)
- Modify: `justfile` (`test-privileged`, `_test-privileged`; add `test-privileged` to `ci`)
- Modify: `.github/workflows/ci.yml` (job `privileged`, not required)
- Modify: `AGENTS.md` §6/§7 (new recipe, root-only tests naming rule)

**Interfaces:**
- Produces: test naming rule — root-only tests are named `privileged_*`, marked `#[ignore = "requires root; run with just test-privileged"]`, and start with `assert_eq!(rustix::process::geteuid().as_raw(), 0)`. Host user for the drop comes from env `ZERODECK_TEST_HOST_USER` (compose sets `dev`).
- Produces: `just test-privileged` (used by Task 4's privileged test).

- [ ] **Step 1: Write the failing tests** in `crates/zerodeckd/tests/daemon.rs`, both `#[cfg(target_os = "linux")]`:
  - `worker_exits_when_daemon_is_killed`: start daemon with the e2e plugin (no `"root"` flag), wait for the worker pid file, `kill -KILL <daemon pid>`, then poll `kill -0 <worker pid>` every 100 ms; assert the worker is gone within 5 s.
  - `privileged_worker_exits_when_root_daemon_is_killed`: same flow with `DECKY_USER=$ZERODECK_TEST_HOST_USER` in the daemon env; additionally assert the worker's `/proc/<pid>/status` `Uid:` line is not `0` before killing the daemon.
- [ ] **Step 2: Add the privileged harness.** compose service:
  ```yaml
  privileged-test:
    <<: *dev
    profiles: ["privileged"]
    user: root
    cap_drop: [ALL]
    cap_add: [SETUID, SETGID, CHOWN, KILL, DAC_OVERRIDE, FOWNER]
    volumes:  # separate target/cargo-home: root-owned build files must not land in the dev volumes
      - .:/workspace
      - ${ZERODECK_PRIV_TARGET:-privileged-target}:/workspace/target
      - ${ZERODECK_PRIV_CARGO_HOME:-privileged-cargo-home}:/home/dev/.cargo
    environment: [CI, CARGO_INCREMENTAL, ZERODECK_TEST_HOST_USER=dev, RUST_BACKTRACE=1]
  ```
  `security_opt: no-new-privileges:true` is inherited. justfile: public `test-privileged` runs `docker compose run --rm [-T] privileged-test just _test-privileged` (native when `native == "true"`); `_test-privileged: cargo test --workspace --locked -- --ignored privileged_`.
- [ ] **Step 3: Run to verify failure**
  Run: `just test-privileged`
  Expected: `privileged_worker_exits_when_root_daemon_is_killed` FAILS (worker still alive after 5 s). `ZERODECK_NATIVE=1 cargo test -p zerodeckd worker_exits_when_daemon_is_killed` inside `just shell` PASSES (no drop, death signal kept) — it guards against regressions.
- [ ] **Step 4: Fix the pre_exec order.** Before building the closure capture `let daemon_pid = rustix::process::getpid();`. Inside the closure, after the `set_thread_groups`/`set_thread_res_gid`/`set_thread_res_uid` block, call `set_parent_process_death_signal(Some(Signal::TERM))`, then `if rustix::process::getppid() != Some(daemon_pid) { return Err(std::io::Error::from_raw_os_error(rustix::io::Errno::SRCH.raw_os_error())); }`. Update the `// SAFETY:` comment to say the death signal is set after the credential change because Linux clears it on uid/gid changes. `deploy/systemd/zerodeck.service` keeps `KillMode=process` (spec: builder's call); add a comment line there explaining why.
- [ ] **Step 5: Run to verify pass**
  Run: `just test-privileged`
  Expected: PASS (all `privileged_*` tests).
- [ ] **Step 6: CI job.** In `ci.yml` add job `privileged` (`runs-on: ubuntu-latest`, `timeout-minutes: 30`, checkout with `persist-credentials: false`, `./.github/actions/dev-image`, `run: just test-privileged`). Not added to branch protection. Update `AGENTS.md`: §6 mentions the `privileged-test` service is the only root container; §7 lists `just test-privileged` as part of `just ci`.
- [ ] **Step 7: Commit**
  ```bash
  git add crates/zerodeck-plugin-host/src/supervisor.rs crates/zerodeckd/tests/daemon.rs compose.yaml justfile .github/workflows/ci.yml AGENTS.md deploy/systemd/zerodeck.service
  git commit -m "fix(plugin-host): keep parent-death signal after dropping worker privileges"
  ```

### Task 2: Session-scoped `FULL_SYNC` cache (S2)

**Files:**
- Modify: `crates/zerodeck-server/src/ws_sync.rs`
- Modify: `crates/zerodeck-server/src/router.rs` (`handle_ws_upgrade`, `handle_ws_socket`, `handle_ws_text`, `spawn_ws_call`)
- Modify: `crates/zerodeck-server/src/lib.rs` (WS integration test)
- Modify: `frontend/src/wsrouter.ts`, `frontend/tests/wsrouter.test.ts`
- Modify: `docs/architecture/wire-protocol.md` (FULL_SYNC section)

**Interfaces:**
- Produces (`ws_sync.rs`):
  ```rust
  #[derive(Debug, Clone, PartialEq, Eq, Hash)]
  pub enum SyncScope { Legacy, Session(String) }
  pub type WsSyncKey = (SyncScope, u64, String, String);
  impl WsSyncCache {
      pub async fn execute<F, Fut>(self: Arc<Self>, scope: SyncScope, call: WsCallPayload, run: F) -> WsServerMessage;
      pub fn evict_id(&self, scope: &SyncScope, id: u64);
  }
  pub fn parse_session_param(raw: Option<&str>) -> Result<SyncScope, ()>; // None -> Legacy
  ```
  Valid session: 1–64 chars of `[A-Za-z0-9-]`; anything else → `Err(())`, which `handle_ws_upgrade` turns into `400 Bad Request` (after the auth check).
- Produces (`wsrouter.ts`): `WSRouterOptions.sessionId?: string` (default `crypto.randomUUID()`), `readonly sessionId: string`; socket URL `${baseUrl}/ws?auth=<enc>&session=<enc>`.

- [ ] **Step 1: Write failing Rust unit tests** in `ws_sync.rs` tests:
  - `sessions_with_same_call_key_execute_independently`: `Session("a")` and `Session("b")` executing `call_payload(1)` both run (`runs == 2`).
  - `evict_in_one_session_keeps_other_session_entry`: complete id 1 in A and B; `evict_id(&Session("a"), 1)`; re-execute in B returns cached frame without running; in A runs again.
  - `legacy_scope_shares_entries_across_connections`: two executes with `SyncScope::Legacy` run once.
  - `parse_session_param_accepts_uuid_and_rejects_bad_values`: `Some("3f2b…uuid")` → `Session`, `None` → `Legacy`, `Some("")`, `Some("../x")`, `Some("a b")`, `Some(&"a".repeat(65))` → `Err(())`.
- [ ] **Step 2: Write failing integration test** in `lib.rs` WS tests: `ws_new_session_reexecutes_unacknowledged_call_from_previous_session` — reuse the side-effect-counter plugin from `ws_full_sync_deduplicates_completed_call_and_evicts_on_received_response`; socket 1 with `session=s1` calls id 1 and closes without ack; socket 2 with `session=s2` calls id 1 with the same args; assert the counter reached 2. Also `ws_rejects_malformed_session_param` → upgrade request with `session=../x` gets `400`.
- [ ] **Step 3: Write failing frontend test** `WSRouter sends one stable session id across reconnects`: capture URLs passed to `createSocket` over two `connect()` calls; both contain the same `session=` value equal to `router.sessionId`; a second `WSRouter` has a different `sessionId`.
- [ ] **Step 4: Run to verify failure**
  Run: `just check-rust` then `just check-frontend`
  Expected: new tests FAIL (compile errors on `SyncScope`, URL lacks `session=`).
- [ ] **Step 5: Implement.** Thread the scope from `handle_ws_upgrade` (query `session`) into `handle_ws_socket(state, socket, scope)` → `handle_ws_text` → `spawn_ws_call` → `cache.execute(scope.clone(), …)`; `ReceivedResponse` uses `evict_id(&scope, id)`. Existing `canonical_key` gains the scope as first element. Frontend appends the session param in `connect`.
- [ ] **Step 6: Run to verify pass**
  Run: `just check-rust && just check-frontend`
  Expected: PASS, including the Phase 3 test `ws_full_sync_deduplicates_completed_call_and_evicts_on_received_response` unchanged (it sends no session → Legacy).
- [ ] **Step 7: Docs.** `wire-protocol.md`: keys are `(session, call.id, route, canonical_args_json)`; `session` is optional; without it the connection shares the legacy namespace; format rules; `400` on invalid.
- [ ] **Step 8: Commit** — `fix(server): scope FULL_SYNC dedup cache to the client session`

### Task 3: Refuse non-loopback binds without an explicit opt-in (S3)

**Files:**
- Modify: `crates/zerodeckd/src/main.rs` (`Cli`, new `check_bind_policy`, call sites)
- Modify: `crates/zerodeckd/tests/daemon.rs`
- Modify: `Dockerfile` (runtime `ENV ZERODECK_ALLOW_NON_LOOPBACK=1`), `compose.yaml` (`daemon` env)
- Modify: `README.md` (configuration table, container instructions), `docs/architecture/wire-protocol.md` (bind policy)

**Interfaces:**
- Produces: `Cli.allow_non_loopback: bool` — `#[arg(long, env = "ZERODECK_ALLOW_NON_LOOPBACK")]` (clap flag; env accepts `1/true/yes`, falsey `0/false/no/off`).
- Produces: `fn check_bind_policy(addrs: &[SocketAddr], allow_non_loopback: bool) -> anyhow::Result<()>` — Ok when every address is loopback, or when allowed (then `warn!` once naming the addresses). Error text must contain `--allow-non-loopback` and `ZERODECK_ALLOW_NON_LOOPBACK`.

- [ ] **Step 1: Write failing unit tests** in `main.rs` `#[cfg(test)] mod tests`:
  - `bind_policy_accepts_loopback_addresses`: `[127.0.0.1:1337]`, `[[::1]:1337]`, `[127.0.0.1:1337, [::1]:1337]` → Ok with `false`.
  - `bind_policy_rejects_unspecified_and_lan_without_opt_in`: `0.0.0.0`, `[::]`, `192.168.1.10`, and mixed `[127.0.0.1, 192.168.1.10]` → Err containing both opt-in names.
  - `bind_policy_allows_any_address_with_opt_in`: the same inputs with `true` → Ok.
- [ ] **Step 2: Write failing daemon test** `refuses_non_loopback_bind_without_opt_in`: run `zerodeckd --host 0.0.0.0 --port <free>`; exits non-zero within 5 s; stderr contains `--allow-non-loopback`. And `serves_on_localhost_hostname` (existing) still passes.
- [ ] **Step 3: Run to verify failure** — `just check-rust`; Expected: FAIL (function missing).
- [ ] **Step 4: Implement.** Call `check_bind_policy(&addrs, cli.allow_non_loopback)` right after `resolve(...)` in `run_daemon` and in the `--check-config` path (resolve there too, so `--check-config` reports the problem). `--healthcheck` is exempt. Dockerfile runtime stage and compose `daemon` service set `ZERODECK_ALLOW_NON_LOOPBACK=1` (the `zerodeckd` compose service inherits it from the image).
- [ ] **Step 5: Run to verify pass** — `just check-rust`; Expected: PASS.
- [ ] **Step 6: Docs.** README config table row: `--allow-non-loopback` / `ZERODECK_ALLOW_NON_LOOPBACK` / off / "Allow binding to a non-loopback address (exposes the auth token to that network)". Every `docker run` example uses `-p 127.0.0.1:1337:1337` with one sentence why. `wire-protocol.md`: bind policy paragraph next to the `Host` allowlist.
- [ ] **Step 7: Commit** — `fix(zerodeckd): refuse non-loopback binds unless explicitly allowed`

### Task 4: Spawn preflight — root plugins in non-root-owned trees, missing Python host (S5, S4)

**Files:**
- Create: `crates/zerodeck-platform/src/ownership.rs` (+ export in `lib.rs`)
- Create: `crates/zerodeck-core/src/preflight.rs` (+ `mod` in `lib.rs`)
- Modify: `crates/zerodeck-core/src/manager.rs` (all spawn paths, `PluginSummary`, `set_setting`)
- Modify: `crates/zerodeck-core/src/error.rs` (new variant)
- Modify: `crates/zerodeck-server/src/router.rs` (`serialize_plugin_summaries`, `core_error_to_ws`)
- Modify: `crates/zerodeckd/src/main.rs` (candidate lists, Python host probe, ancestor warning)
- Modify: `crates/zerodeckd/tests/daemon.rs` (privileged test)
- Modify: `docs/architecture/wire-protocol.md`, `README.md`, `SECURITY.md`

**Interfaces:**
- Produces (`zerodeck-platform::ownership`, `#[cfg(unix)]`):
  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub struct EntryMeta { pub uid: u32, pub mode: u32, pub is_symlink: bool }
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub enum ViolationKind { NotRootOwned { uid: u32 }, WritableByOthers { mode: u32 }, Symlink }
  #[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
  #[error("`{path}` is not safe to run as root: {kind:?}")]
  pub struct OwnershipViolation { pub path: PathBuf, pub kind: ViolationKind }
  pub fn check_entry(meta: EntryMeta) -> Option<ViolationKind>;          // uid != 0, mode & 0o022 != 0, symlink
  pub fn check_root_only_tree(root: &Path) -> Result<(), OwnershipViolation>; // lstat walk, no follow
  pub fn non_root_writable_ancestors(path: &Path) -> Vec<PathBuf>;       // owner != 0 or mode & 0o022
  ```
  A symlink anywhere in the tree is a violation (the installer never creates symlinks; the spec's "not followed" is met by rejecting them).
- Produces (`zerodeck-core::preflight`, crate-private):
  ```rust
  pub(crate) struct PreflightInput<'a> {
      pub requires_root: bool, pub daemon_is_root: bool,
      pub allow_unsafe_root_plugins: bool, pub python_host_missing: Option<&'a str>,
  }
  pub(crate) fn preflight(input: &PreflightInput<'_>, tree_check: impl FnOnce() -> Result<(), OwnershipViolation>) -> Result<(), String>;
  ```
  Order: missing Python host → `Err(reason)`; `requires_root && daemon_is_root && !allow_unsafe_root_plugins` → run `tree_check`, which the manager builds as `check_entry` on the `lstat` of `plugins_dir` itself (not recursive) followed by `check_root_only_tree(plugin_dir)` → `Err(violation.to_string())`; with the opt-in → `Ok` plus a `warn!`.
- Produces (manager):
  - `PluginSummary.failure_reason: Option<String>`; `state` is `PluginState::Failed` when a reason is recorded.
  - `CoreError::PluginBlocked { name: String, reason: String }` (`#[error("plugin `{name}` cannot start: {reason}")]`); WS error name `PluginBlockedError`; legacy HTTP maps it to `503`.
  - `pub fn set_python_host_missing(&self, reason: Option<String>)`.
  - Private `async fn spawn_checked(&self, plugin: &DiscoveredPlugin) -> Result<(), CoreError>` used by `start_all` (logs and continues per plugin), `reload_plugin` (both spawn and `supervisor.reload_plugin` branches), `install_plugin`, `enable_plugin`. Success clears a recorded reason.
  - Setting key constant `ALLOW_UNSAFE_ROOT_PLUGINS_PATH = "developer.allow_unsafe_root_plugins"`.
- Produces (`main.rs`): `fn python_host_candidates(exe_dir: Option<&Path>) -> Vec<PathBuf>` and `fn frontend_candidates(exe_dir: Option<&Path>) -> Vec<PathBuf>`; cwd-relative and `CARGO_MANIFEST_DIR` entries only under `#[cfg(debug_assertions)]`. `async fn probe_python_host(cfg: &PythonHostConfig) -> Option<String>` returns a reason when `python3 -c "import zerodeck_py_host"` (with the resolved `PYTHONPATH`) fails.

- [ ] **Step 1: Write failing platform tests** (`ownership.rs`): `root_owned_0755_dir_and_0644_file_pass` (`check_entry` → None), `non_root_owner_is_violation` (uid 1000), `group_or_world_writable_is_violation` (`0o775`, `0o757`), `symlink_is_violation`, and `tree_owned_by_current_user_is_reported_when_not_root` (temp dir; skip assertion body if euid == 0) checking `check_root_only_tree` returns `NotRootOwned` for the temp root. `non_root_writable_ancestors_lists_user_owned_temp_dir`.
- [ ] **Step 2: Write failing core tests** (`preflight.rs` tests): `blocks_root_plugin_with_violation_when_daemon_is_root`, `skips_tree_check_when_daemon_is_not_root` (closure panics if called), `skips_tree_check_for_non_root_plugins`, `opt_in_bypasses_tree_check`, `missing_python_host_blocks_every_backend_plugin`. Manager tests (in `crates/zerodeck-core/src/lib.rs` tests module, next to the existing manager tests): `set_setting_rejects_unsafe_root_plugins_opt_in` — both `set_setting("developer.allow_unsafe_root_plugins", true)` and `set_setting("developer", json!({"allow_unsafe_root_plugins": true}))` → `CoreError::InvalidSetting`; `set_setting("developer.allow_unverified_installs", true)` still Ok. `python_host_missing_marks_plugins_failed_with_reason` — `set_python_host_missing(Some(..))`, `start_all`, `list_plugins()` shows `Failed` + reason, no worker spawned. `debug_reload_runs_preflight` — with Python host missing, `reload_plugin` returns `PluginBlocked`.
- [ ] **Step 3: Write failing `main.rs` tests**: `release_candidates_exclude_cwd_and_manifest_dir` gated `#[cfg(not(debug_assertions))]` and `debug_candidates_include_repo_paths` gated `#[cfg(debug_assertions)]`, asserting on `python_host_candidates(Some(Path::new("/usr/local/bin")))` and `frontend_candidates(...)`.
- [ ] **Step 4: Write failing privileged daemon test** `privileged_root_plugin_in_user_owned_dir_is_blocked`: as root, write a `"root"`-flagged plugin, `chown -R $ZERODECK_TEST_HOST_USER` it, start daemon, `loader/get_plugins` entry has `state` `"Failed"` and `failure_reason` containing the plugin path; stop; write `{"developer":{"allow_unsafe_root_plugins":true}}` to `settings/loader.json`; restart; entry reaches `"Running"`.
- [ ] **Step 5: Run to verify failure** — `just check-rust` and `just test-privileged`; Expected: FAIL.
- [ ] **Step 6: Implement** per Interfaces. `serialize_plugin_summaries` adds `"failure_reason"` (null when none). At startup, when euid == 0, `main.rs` logs one `warn!` listing `non_root_writable_ancestors(paths.privileged_root())`. If `probe_python_host` returns a reason, `error!` once and call `manager.set_python_host_missing(Some(reason))` before `start_all`.
- [ ] **Step 7: Run to verify pass** — `just check-rust && just test-privileged`; Expected: PASS.
- [ ] **Step 8: Docs.** `wire-protocol.md`: `failure_reason` field on plugin summaries, `PluginBlockedError`, the opt-in key and that `set_setting` rejects it. `README.md`: note on root plugins and the developer opt-in. `SECURITY.md`: threat-model paragraph — root plugins must live in root-owned trees; `~/homebrew` ancestors are `deck`-owned on stock SteamOS (startup warning), relocation tracked as an open decision.
- [ ] **Step 9: Commit** — two commits: `fix(core): refuse to start root plugins from trees writable by non-root users` (platform + core + server + daemon privileged test + docs) and `fix(zerodeckd): drop dev-only search paths from release builds and fail plugins fast without a Python host`.

### Task 5: Duplicate plugin names (S6)

**Files:**
- Modify: `crates/zerodeck-core/src/discovery.rs`
- Test: `crates/zerodeck-core/src/lib.rs` tests module (next to existing discovery tests)

**Interfaces:**
- Produces: `discover_plugins` returns at most one plugin per `manifest.name`, keeping the first in sorted directory order; skipped ones log `warn!(plugin = %name, kept = %first_dir, skipped = %dir, "duplicate plugin name")`.

- [ ] **Step 1: Write the failing test** `discover_plugins_skips_duplicate_names_keeping_first_directory`: dirs `a-copy` and `b-orig` both declare `"name": "Same"` → result has one plugin with `directory == "a-copy"`.
- [ ] **Step 2: Run to verify failure** — `just check-rust`; Expected: FAIL (2 plugins).
- [ ] **Step 3: Implement** the dedupe after the sort with a `HashSet<String>` of seen names.
- [ ] **Step 4: Run to verify pass** — `just check-rust`; Expected: PASS.
- [ ] **Step 5: Commit** — `fix(core): skip plugins whose manifest name duplicates another directory`

### Task 6: Static file headers, MIME types, conditional GET (S7)

**Files:**
- Modify: `Cargo.toml` (`httpdate = "1.0.3"` in `[workspace.dependencies]`; already in `Cargo.lock` via hyper), `crates/zerodeck-server/Cargo.toml`
- Modify: `crates/zerodeck-server/src/router.rs` (`stream_file_response`, `build_file_headers`, `mime_for_path`, static/plugin handlers gain a `HeaderMap` extractor)
- Test: `crates/zerodeck-server/src/lib.rs` tests (extend `http_static_and_plugin_routes_serve_mime_types_and_block_traversal` fixtures)
- Modify: `docs/architecture/wire-protocol.md`

**Interfaces:**
- Produces: `async fn stream_file_response(path: &Path, method: Method, no_store: bool, if_modified_since: Option<SystemTime>) -> Response`. Every file response carries `X-Content-Type-Options: nosniff` and `Last-Modified` (second precision); when `If-Modified-Since` ≥ mtime (truncated to seconds) the response is `304` with no body. `no_store` responses (frontend bundle) still send `Cache-Control: no-store` and never `304`.
- MIME additions: `gif` image/gif, `ico` image/x-icon, `avif` image/avif, `ttf` font/ttf, `otf` font/otf, `woff` font/woff, `map` application/json, `txt` text/plain; charset=utf-8, `mp3` audio/mpeg, `ogg` audio/ogg, `wav` audio/wav, `mp4` video/mp4, `webm` video/webm.

- [ ] **Step 1: Write failing tests**: `mime_for_path_covers_common_asset_types` (unit, one assert per extension above), `static_responses_send_nosniff_and_last_modified` (GET `/plugins/<p>/assets/a.png` → both headers present), `static_if_modified_since_returns_304` (send the returned `Last-Modified` back → `304`, empty body), `frontend_bundle_never_returns_304`.
- [ ] **Step 2: Run to verify failure** — `just check-rust`; Expected: FAIL.
- [ ] **Step 3: Implement** per Interfaces using `httpdate::fmt_http_date` / `parse_http_date`.
- [ ] **Step 4: Run to verify pass** — `just check-rust`; Expected: PASS.
- [ ] **Step 5: Docs.** `wire-protocol.md`: headers on static routes; one paragraph on why `/plugins/{name}/data/*` stays public (Phase 3 decision, Decky parity; files are served read-only through `SafePath`).
- [ ] **Step 6: Commit** — `fix(server): send nosniff and Last-Modified on static files and widen MIME table`

### Task 7: Final verification and PR

- [ ] **Step 1:** Run `just ci`. Expected: all recipes pass (check, py-matrix, lint-infra, smoke, test-privileged).
- [ ] **Step 2:** Open the PR titled `fix: security and robustness hardening` with one bullet per commit and the spec link.
