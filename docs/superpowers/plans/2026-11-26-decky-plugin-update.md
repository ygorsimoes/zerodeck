# Decky-Compatible Plugin Update and Upgrade Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Update and recover a single Decky plugin without replacing unrelated plugins, compromising its managed data, or restarting ZeroDeck.

**Architecture:** The Decky adapter resolves identity and candidate bytes; the existing install pipeline stages verified files; `PluginManager` owns durable per-plugin activation and recovery. The backend and frontend provide separate evidence for the generation they have loaded.

**Tech Stack:** Rust, Tokio, reqwest/rustls, SHA-256, existing ZIP and `SafePath` helpers, Python stdlib, WebSocket IPC, TypeScript, Steam CEF.

**Spec:** [ZeroDeck and Decky Plugin Update Design](../specs/2026-11-26-update-and-upgrade-design.md), plugin and shared sections.

## Global Constraints

- Rust `1.100.0`, Edition `2024`; Python `>=3.10`, stdlib only; frontend uses `pnpm@12.16.1` and Steam's React globals.
- MIT clean-room implementation; keep the Cargo graph acyclic and pass `cargo deny` on the actual lockfile.
- Keep `SafePath`, atomic writes, archive containment, fail-closed remote SHA-256 for managed updates, `0755` executables and the existing IPC, privilege, CORS and auth boundaries.
- No installs from SteamOS's read-only `/usr`; test a persistent root-controlled install root and systemd configuration across an actual SteamOS update before Stable.
- The first supported native distribution target is SteamOS `linux-x86_64` with systemd; Docker images are upgraded externally by digest.
- `just check` after each change; use `just ci` for changes to `Dockerfile`, `compose.yaml`, `justfile`, `.devcontainer/`, or `.github/**`; keep required CI job IDs stable.

## Review Focus

1. A catalog version is opaque, or a name/digest is reused: avoid replacing the wrong plugin (Task P1).
2. Cancelled network work, a truncated binary or exhausted disk must not stop the live worker (Task P2).
3. A child starts but fails migration, sends a stale acknowledgement or restarts after its code changes ownership (Task P3).
4. Loss of power between the directory moves, or a late retry after another operation, must not lose the recoverable version (Tasks P4–P5).
5. A detached writer changes data or a browser reload acknowledges another generation: protect the data and keep frontend verification honest (Tasks P4 and P6).

## File map

| Path | Responsibility |
|---|---|
| `crates/zerodeck-core/src/{catalog,installer,archive}.rs` | Store adapter, verified source and staging pipeline. |
| `crates/zerodeck-core/src/{plugin_update,manager,preflight,watcher}.rs` | Records, checkpoints, activation/recovery, serialization of lifecycle changes. |
| `crates/zerodeck-plugin-host/src/{supervisor,env}.rs`, `crates/zerodeck-protocol/src/ipc.rs`, `hosts/python/zerodeck_py_host/runner.py` | Per-spawn validation and generation-tagged host readiness. |
| `crates/zerodeck-server/src/{admin.rs,ws/dispatch.rs,ws_sync.rs,routes/static_files.rs}` | Backward-compatible API, root-only administration, durable operation status and generation-aware assets. |
| `crates/zerodeckd/src/main.rs`, `crates/zerodeck-updater/src/main.rs` | Recover before plugin discovery; administrative plugin CLI over authenticated local control. |
| `frontend/src/{index,wsrouter,plugin_update}.ts` | Update state, frontend trial lifecycle and recovery after reconnect. |
| `crates/zerodeck-core/src/tests/`, `crates/zerodeckd/tests/daemon.rs`, `hosts/python/tests/`, `frontend/tests/`, `docs/architecture/` | Fixtures, integration and compatibility evidence. |

P1–P3 can be tested without the loader's update transport. P4 consumes
`MaintenanceLock::try_shared(path: &Path) -> io::Result<MaintenanceLock>` and
`MaintenanceLock::try_exclusive(path: &Path) -> io::Result<MaintenanceLock>` from native
plan L1; P5 uses the independent CLI from L4. P6 needs the Game Mode bootstrap and plugin
mount/unmount entry points from Phase 4; headless operations and their tests ship before it.

---

### Task 1: Catalog contract and identity

**Files:** `docs/architecture/wire-protocol.md`, `crates/zerodeck-core/src/catalog.rs`,
`crates/zerodeck-core/src/{lib.rs,tests/catalog.rs,tests/mod.rs}`.

**Interfaces:** `DeckyChannel::{Stable, Testing}`; `CatalogPluginId { channel: DeckyChannel, id: u64 }`;
`DeckyCandidate { plugin: CatalogPluginId, name: String, catalog_version: String, sha256: String, artifact_url: Url }`;
`async fn load_catalog(client: &CheckedHttpClient, channel: DeckyChannel) -> Result<DeckyCatalog, CoreError>`;
`DeckyCatalog::select(&self, id: CatalogPluginId, selection: CatalogSelection) -> Result<DeckyCandidate, CoreError>`,
where selection is an exact version or a documented upstream recommendation.

- [ ] Document `/plugins`, verify the artifact request through a documented public
  contract or a black-box Decky `3.3.1` run, and record stable/testing identity,
  publication order, version labels and digest binding. Record the `3.3.0` update API
  separately only if its scope and privacy behavior can be verified. Use the documented
  catalog and explicit versions if a newer recommendation API is not confirmed.
- [ ] Add `catalog_keeps_legacy_version_labels_and_channels` and
  `catalog_refuses_ambiguous_updates_and_unverifiable_entries`. Fixtures assert exact
  handling of `0.11.1.6`, `1.3` and `1.0.3-bd019d3`; same IDs across different channels,
  reused labels, missing digest and same-name unrelated sources don't select silently.
- [ ] Run `docker compose run --rm dev cargo test --locked -p zerodeck-core catalog_`;
  expect RED before the adapter exists.
- [ ] Implement the documented reader, validate lengths and hashes, tolerate unknown
  response fields and retain a bounded cache with an explicit stale state. Don't use a
  stale or ambiguous catalog as fresh permission to change origin, channel or digest.
  Don't invent URLs or copy Decky implementation code.
- [ ] Rerun the focused tests; commit `feat(plugins): read verified decky catalog candidates`.

### Task 2: Verified ZIP and dependency staging with bounded resources

**Files:** `crates/zerodeck-core/src/{installer,archive}.rs`,
`crates/zerodeck-core/src/tests/{installer,archive}.rs`.

**Interfaces:** keep `prepare_staged_plugin(...) -> Result<StagedPluginInstall, CoreError>`;
add `InstallPluginSource::Decky(DeckyCandidate)` for managed input. Add a path/reader
entry point to the existing ZIP extractor:
`extract_zip_reader_safely<R: Read + Seek>(reader: R, dest_dir: &Path, limits: &ExtractLimits) -> Result<Vec<PathBuf>, CoreError>`.
`StagedPluginInstall::sha256()` and `manifest()` continue to identify the candidate
before any lifecycle mutation.

- [ ] Add `installer_streams_and_preserves_active_on_partial_binary_or_enospc`: interrupt a
  ZIP and a secondary binary separately; assert no swap, prior RPC still replies and the
  bounded staged/cache directory remains consistent. Add `installer_managed_rejects_missing_hash`
  and retain tests for wrapped/unwrapped ZIPs, traversal and legacy missing version.
- [ ] Run `docker compose run --rm dev cargo test --locked -p zerodeck-core --lib installer_`;
  assert the new cases fail before implementing streamed staging.
- [ ] Stream ZIP and `remote_binary` assets to private files, calculate the digest as
  bytes arrive and perform ZIP validation from the verified file. Preserve existing
  redirect/path/hash policy; require all hashes for the managed path even if developers
  opted out for manual sideloads. Only apply store/manifest version matching rules
  recorded by P1. Do blocking extraction off the async runtime and cap parallel staging
  plus cache size; hold no lifecycle lock while downloading.
- [ ] Rerun installer and archive tests; commit
  `perf(plugins): stream verified plugin artifacts into staging`.

### Task 3: Backend readiness and validation on every spawn

**Files:** `crates/zerodeck-protocol/src/ipc.rs`,
`crates/zerodeck-plugin-host/src/{supervisor,env,lib}.rs`,
`crates/zerodeck-core/src/{preflight,manager}.rs`,
`hosts/python/zerodeck_py_host/runner.py`, `hosts/python/tests/test_runner.py`.

**Interfaces:** additive internal host IPC readiness report `{generation, status, error}`
with status `ready` or `startup_failed`; do not change plugin-call/event contracts.
`ManagedPlugin::wait_ready(&self, expected_generation: &str, timeout: Duration) -> Result<(), HostError>`;
`PluginSpawnSpec` carries an opaque generation and the root-owned policy needed at
every spawn. P4 uses `wait_ready` only for the candidate it is activating.

- [ ] Add `worker_is_not_ready_before_migration_and_waits_for_matching_generation`,
  `worker_long_running_main_does_not_block_readiness` and
  `privileged_restart_revalidates_tree`. Assert a migration exception, startup timeout,
  stale report and early `_main` failure cannot confirm an upgrade; a live long-running
  `_main` can. Assert a root restart stops when ownership no longer passes validation.
- [ ] Run `just check-python`, the Rust plugin-host suite via
  `docker compose run --rm dev cargo test --locked -p zerodeck-plugin-host`, and
  `just test-privileged`; expect the new cases to fail before implementation.
- [ ] Signal readiness only after plugin import and migration and when the IPC handler can
  serve requests. Propagate early `_main` failures without requiring it to finish;
  use the existing default 15-second startup deadline and a 2-second observation
  window. Give the supervisor the same ownership validation on each launch/relaunch
  and never trust a CEF client as evidence of backend readiness.
- [ ] Rerun the three suites; commit `feat(plugins): confirm worker readiness before activation`.

### Task 4: Durable activation, checkpoints and safe recovery

**Files:** `crates/zerodeck-core/src/{plugin_update,installer,manager,discovery,watcher}.rs`,
`crates/zerodeck-core/src/tests/plugin_update.rs`, `crates/zerodeckd/src/main.rs`.

**Interfaces:** `PluginOperationId` is a UUID. `PluginUpdateRequest { id: PluginOperationId, source: InstallPluginSource, expected_active_digest: Option<String> }`;
`async fn PluginManager::prepare_update(&self, req: PluginUpdateRequest) -> Result<PluginUpdateStatus, CoreError>`;
`async fn PluginManager::activate_update(&self, id: PluginOperationId) -> Result<PluginUpdateStatus, CoreError>`;
`async fn PluginManager::rollback_update(&self, req: PluginRollbackRequest) -> Result<PluginUpdateStatus, CoreError>`.
`PluginRollbackRequest` carries its own ID, plugin directory, expected active digest
and `restore_data` (default `false`). `recover_pending_plugin_updates(paths: &PlatformPaths) -> Result<RecoveryReport, CoreError>` runs before discovery.
Operation state distinguishes prepared, applying, backend-confirmed, awaiting frontend,
confirmed, reverted and recovery-required. In-flight and complete records bind ID to
immutable request inputs and reject a mismatched replay.
Managed installs use the archive digest as the expected active identity. Explicit adoption
also captures a guarded fingerprint of the legacy tree so activation cannot replace a
checkout changed after review. That fingerprint is not treated as an upstream archive hash.

- [ ] Add `activation_reverts_worker_after_failed_candidate_migration`: v1 RPC returns
  `"v1"` before the swap and once again after v2 fails; assert prior code, settings,
  ordering and an unrelated plugin remain available. Add power-loss cases at each
  journal/rename boundary, including the fallback between two renames, and
  `recovery_retains_both_data_copies_when_a_writer_is_unaccounted_for`.
- [ ] Run `docker compose run --rm dev cargo test --locked -p zerodeck-core --lib plugin_update`;
  expect the current post-swap failure to lose the known-good generation or the new API to be absent.
- [ ] Reuse the existing staged ZIP and keep the prior tree through trial. Write intent
  and checkpoint before swap, sync directory transitions and restore the last known
  generation before discovery after a crash. Use shared maintenance and per-plugin guards
  in that order, and serialize reload, enable, disable, delete and hot-reload with apply.
  Bind the operation to the reviewed digest and root-privilege authorization. If data
  did not change, restore code without rewriting it. Restore modified trial data only
  when exclusive ownership of writes is established; otherwise disable that plugin and
  retain both copies. No old data restore for an ordinary post-confirmation downgrade.
- [ ] Rerun the focused suite and `just check`; commit
  `feat(plugins): persist upgrades and recover previous generations`.

### Task 5: Headless commands and legacy compatibility

**Files:** `crates/zerodeck-server/src/{admin.rs,lib.rs,ws/args.rs,ws/dispatch.rs,ws/socket.rs}`,
`crates/zerodeck-server/src/ws_sync.rs`, `crates/zerodeckd/tests/daemon.rs`,
`crates/zerodeckd/src/main.rs`, `crates/zerodeck-updater/src/main.rs`,
`docs/architecture/wire-protocol.md`.

**Interfaces:** keep `loader/install_plugin` and `utilities/install_plugin` and their
legacy arguments/results; funnel them into the same installer. Add
`zerodeck/plugins/prepare`, `zerodeck/plugins/activate`, `zerodeck/plugins/status` and
`zerodeck/plugins/rollback`, each with one JSON object argument. `prepare` accepts
`{id, channel, plugin_id, version?, expected_active_digest?}`; `activate` and `status`
take `{id}`; `rollback` accepts `{id, directory, expected_active_digest, restore_data?}`.
Map `zerodeckctl plugins update|upgrade|status|rollback` to those operations. Add
`sudo zerodeckctl plugins adopt <directory> <channel> <plugin-id> <version>`,
`sudo zerodeckctl plugins detach <directory>` and
`sudo zerodeckctl plugins approve-root <operation-id> <sha256>` over a Unix socket
inside the service's root-owned `0700` runtime directory with root peer UID.
Administrative approval binds an operation ID and digest; a session token or legacy
`user_confirmed` is not root-privilege approval.

- [ ] Add `plugin_operation_replay_survives_daemon_restart`, `legacy_sideload_cannot_overwrite_catalog_plugin`
  and `explicit_adoption_uses_exact_verified_candidate`. Test an ID retry after the
  daemon restarts, mutation payload mismatch, legacy result shape, date/channel/version
  conflicts, disabled-state preservation and unavailable unverified store entries.
  Add `privileged_plugin_approval_requires_root_peer_and_exact_digest`: the app token,
  an unprivileged socket client and an approval for a different digest cannot authorize
  the candidate.
- [ ] Run `docker compose run --rm dev cargo test --locked -p zerodeckd --test daemon`;
  expect failure on the three new cases.
- [ ] Wire new operations to P4 under the existing auth model. Preserve legacy error
  and result shapes for supported operations; reject a managed overwrite without
  explicit detachment, and never replay a pending new mutation under a new ID after
  reconnection. Deduplicate legacy managed activation by the resolved digest and the
  durable state instead of depending on the volatile `FULL_SYNC` cache. Keep privileged
  socket typed operations scoped to plugin identities and the already prepared digest.
- [ ] Rerun the focused suite, `just test-privileged` and `just check`; document
  commands and operator recovery.
  Commit `feat(plugins): expose durable plugin updates through the local API`.

### Task 6: Confirm frontends in Game Mode and publish compatibility evidence

**Files:** `crates/zerodeck-server/src/routes/static_files.rs`,
`crates/zerodeck-server/src/ws/dispatch.rs`, `crates/zerodeck-core/src/plugin_update.rs`,
`frontend/src/{index,plugin_update,wsrouter}.ts`, `frontend/tests/plugin_update.test.ts`,
`docs/architecture/plugin-compatibility.md`, `README.md`.

**Interfaces:** expose an additional generation-scoped namespace
`/plugins/{name}/generation/{digest}/dist/{*path}` for managed modules and their
relative chunks. Keep legacy routes intact. `zerodeck/plugins/frontend_result`
receives `{id, generation, status, detail?}` from the authenticated Steam UI after
unmount/mount; status is `ready`, `failed`, or `context_lost`. The server accepts only
the matching pending trial. Make the acknowledgement able to reach the waiter without
taking the lifecycle lock held by activation. Compatibility results distinguish verified package,
verified backend, unchecked frontend, passed Game Mode and failed Game Mode.

- [ ] Add `wrong_frontend_generation_cannot_confirm_update`,
  `cef_context_loss_defers_confirmation`, `pending_frontend_failure_reverts_candidate`
  and `deferred_frontend_failure_disables_only_affected_plugin`.
  Test stale/chunked JS, one active panel, unmount cleanup, browser restart and a
  disabled/headless plugin that has never rendered.
- [ ] Run `just check-frontend` and
  `docker compose run --rm dev cargo test --locked -p zerodeck-server`; expect red
  before wiring the managed trial and versioned files.
- [ ] Reconcile pending operation on reconnect, request controlled reload when the
  legacy frontend cannot unmount, and publish the frontend state only against a
  matching generation/operation. If CEF is absent or restarts, leave the frontend
  unchecked and retry on the next eligible startup. For a definite failure during
  the original activation, use P4's rollback rules. A later failure during deferred
  checking disables that plugin and retains the prior code/data for recovery. Never
  restore an old checkpoint over already active backend writes automatically.
- [ ] Rerun `just check` and `just test-privileged`; perform the spec's Steam Deck
  stable matrix with original fixtures plus independently obtained upstream plugins.
  Record tested loader, plugin, Steam and SteamOS versions. Measure prep resources and
  stop-to-ready/old-RPC-return time. Block a Game Mode support claim without device
  evidence. Commit `test(plugins): gate game mode updates on generation-aware trials`.
