# Native Update Polish Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make PR #23's native updater correct, smaller, SteamOS-persistent, fully dockerized and gated by a real VM run.

**Architecture:** Shared release-manifest/layout code moves down into `zerodeck-protocol`/`zerodeck-platform`; the updater crate is reorganized around one `UpdateRoot`, a generic schema file store, a `tough` transport and a single revert path, and gains `install`/`uninstall`/`verify-archive`. Release assets come from a Dockerfile `dist` stage. A stdlib-Python VM harness runs in a pinned `vm` container against a pinned Arch cloud image with a SteamOS-like profile and a real local TUF repository.

**Tech Stack:** Rust 1.100 (tough 0.24, reqwest, tokio, tar/flate2, rustix), systemd, Docker Buildx, QEMU/OVMF/cloud-init, Python ≥3.10 stdlib, GitHub Actions, bash (thin wrappers only).

**Spec:** [docs/superpowers/specs/2026-11-27-native-update-polish-design.md](../specs/2026-11-27-native-update-polish-design.md), amending [2026-11-26-update-and-upgrade-design.md](../specs/2026-11-26-update-and-upgrade-design.md).

## Global Constraints

- Rust `1.100.0`, Edition `2024`; Python `>=3.10`, stdlib only; `pnpm@12.16.1`.
- MIT clean room: no code from `decky-loader` or the beallio fork; ideas only.
- Crate graph stays acyclic; `zerodeck-protocol` has no internal deps; `zerodeck-platform` depends only on `zerodeck-protocol` and OS/async primitives; `zerodeck-updater` ends with `zerodeck-platform`, `zerodeck-protocol` only.
- Keep `SafePath`, `atomic_write` (temp + `sync_all` + rename), archive containment, fail-closed SHA-256, `0755` executables, privilege/IPC/CORS/auth boundaries.
- Default update root `/home/.zerodeck`; controller `/home/.zerodeck/bin/zerodeckctl`; units in `/etc/systemd/system`; keep-list `/etc/atomic-update.conf.d/zerodeck.conf`.
- Every public `just` recipe forwards to a private `_recipe`; CI steps are single `just` calls; required job ids `rust`, `deny`, `python`, `frontend`, `docker` unchanged.
- Pin tools by version ARG and, for downloads, SHA-256; actions by commit SHA; per-job `permissions`; `persist-credentials: false`.
- Arch image pin: `https://geo.mirror.pkgbuild.com/images/v20261115.625457/Arch-Linux-x86_64-cloudimg-20261115.625457.qcow2`, SHA-256 `dbd02830f6eb0fe07ef6e5edaddbae8b577ce4296d9e79118904704cce0a1f60`, 569933312 bytes.
- VM exit codes: `0` pass, `1` fail, `77` skip; CI treats `77` as failure.
- Conventional Commits; never bump versions by hand; keep release-please markers.
- Focused Rust commands run as `docker compose run --rm -T dev cargo test --locked -p <crate> --test <file> <filter>`; finish every task with `just check` (and `just ci` when the task touches `Dockerfile`, `compose.yaml`, `justfile`, `.devcontainer/` or `.github/**`).

## Review Focus

1. The Deck suspends mid-download, so the connection goes idle and resets: `update` fails cleanly with a transport error, leaves no `.staging-*` behind after the next run, and a retry succeeds (Task 5: `stage_idle_failure_leaves_no_staging_after_sweep`).
2. `/home` fills up while staging (on a Deck, games live there): no partial generation, `current` unchanged, an error naming ENOSPC (Task 6: `stage_enospc_keeps_current_and_sweeps_partial_tree`).
3. The RTC resets to an earlier date: a new activation is refused by the clock floor, but offline `rollback` still works (Task 8: `rollback_ignores_clock_floor`).
4. An operator runs `zerodeckctl upgrade` without `sudo`: a clear "requires root" error and no request file (Task 4: `cli_mutations_require_root`).
5. Port 1337 is still bound by a leftover Decky process: `install` refuses and names the port and the unit it found (Task 10: `install_refuses_port_in_use_and_names_it`).

---

### Task 1: Shared release manifest and generation layout

**Files:**
- Create: `crates/zerodeck-protocol/src/release.rs`, `crates/zerodeck-platform/src/generation.rs`
- Modify: `crates/zerodeck-protocol/src/lib.rs`, `crates/zerodeck-platform/src/lib.rs`, `crates/zerodeck-platform/src/fs.rs`, `crates/zerodeck-updater/src/{generation,stage}.rs`, `crates/zerodeckd/src/{install,main}.rs`, `crates/zerodeck-server/src/{lib,state}.rs`
- Test: unit tests in the two new files; `crates/zerodeckd/tests/daemon.rs`

**Interfaces:**
- Produces: `zerodeck_protocol::release::{RELEASE_MANIFEST: &str = "release.json", RELEASE_MANIFEST_SCHEMA: u32 = 1, MAX_RELEASE_MANIFEST_BYTES: u64 = 65536, ReleaseManifest { schema: u32, version: String, build_id: String }, ReleaseManifest::parse(bytes: &[u8]) -> Result<ReleaseManifest, ReleaseManifestError>}` (serde, `deny_unknown_fields`).
- Produces: `zerodeck_platform::{GenerationLayout, LayoutError}` with `GenerationLayout::open(root: &Path) -> Result<GenerationLayout, LayoutError>`, accessors `root() -> &Path`, `daemon() -> &Path` (`bin/zerodeckd`), `controller() -> Option<&Path>` (`bin/zerodeckctl` when present), `python_dir()`, `frontend_dir()`, `manifest() -> &ReleaseManifest`.
- Produces: `zerodeck_platform::atomic_write_mode(path: &Path, contents: &[u8], mode: u32) -> Result<(), PlatformError>` (mode applied to the temp file before rename; `atomic_write` calls it with `0o644`).
- Produces: `zerodeck_server::BUILD_ID: &str` (the existing `default_build_id` logic as a `const`-like `pub fn build_id() -> &'static str`); `zerodeckd` drops `compile_time_build_id`.

- [ ] **Step 1: Write failing tests** — `release.rs`: `parse_rejects_unknown_schema`, `parse_rejects_empty_build_id`, `parse_rejects_unknown_fields`, `parse_rejects_oversize` (65537 bytes). `generation.rs`: `open_requires_regular_files_and_dirs` (missing `frontend/` → `LayoutError::Missing("frontend")`), `open_rejects_symlinked_component`, `open_rejects_non_executable_daemon` (mode `0644`), `open_rejects_group_writable_daemon` (`0775`). `fs.rs`: `atomic_write_mode_sets_mode_before_rename` (observe the final file is `0600` and no temp file remains).
- [ ] **Step 2: Run** `docker compose run --rm -T dev cargo test --locked -p zerodeck-protocol -p zerodeck-platform` — expect compile failures (types missing).
- [ ] **Step 3: Implement** the interfaces above. Layout checks are exactly today's union of `generation.rs::verify_generation`, `stage.rs` manifest checks and `zerodeckd/src/install.rs` checks; no new rules.
- [ ] **Step 4: Rewire callers** — `GenerationStore::verify_generation` wraps `GenerationLayout::open` after its path-escape check; `stage.rs` drops `ReleaseManifest`; `ManagedGeneration::resolve` uses `GenerationLayout::open` and keeps override validation; `resolve_loader_paths` returns a named struct instead of a 4-tuple.
- [ ] **Step 5: Gate the Python import check** in `zerodeckd --check-config` on `--managed-generation` (defect 9). Test `check_config_standalone_does_not_require_python_host` in `crates/zerodeckd/tests/daemon.rs` (spawn with `PYTHONPATH` pointing at an empty dir; expect exit 0).
- [ ] **Step 6: Run** `just check` — PASS.
- [ ] **Step 7: Commit** `refactor(platform): share release manifest and generation layout`.

### Task 2: Updater core reorganization (root, errors, schema files)

**Files:**
- Create: `crates/zerodeck-updater/src/{error,root}.rs`
- Modify: `crates/zerodeck-updater/src/{lib,state,request,boot,generation,activation,main}.rs`
- Delete: nothing yet (modules merge in later tasks)
- Test: unit tests in `state.rs`, `boot.rs`, `request.rs`

**Interfaces:**
- Produces: `UpdateRoot` (`root.rs`): `UpdateRoot::open(path: &Path) -> Result<UpdateRoot, UpdateError>` (existing, secure ancestors via `ensure_root_owned_ancestors`, no directory creation), `UpdateRoot::create(path: &Path) -> Result<UpdateRoot, UpdateError>` (creates `0755` root-owned tree), and path/store accessors: `path()`, `generations() -> GenerationStore`, `state() -> StateStore`, `catalog_state() -> CatalogStateStore`, `requests() -> ApplyRequestStore`, `boot_marker() -> SchemaFile<BootMarker>`, `maintenance_lock_path()`, `catalog_lock_path()` (`<root>/catalog.lock`), `config_path()` (`<root>/config.json`), `bin_dir()` (`<root>/bin`), `tuf_dir()`.
- Produces: `trait Schema: Serialize + DeserializeOwned { const SCHEMA: u32; fn schema(&self) -> u32; }` and `SchemaFile<T: Schema>` with `new(path, mode: u32)`, `read_optional() -> Result<Option<T>>`, `read_or_default() -> Result<T> where T: Default`, `write(&T)`, `remove()`; writes use `atomic_write_mode`. `StateStore = SchemaFile<UpdateState>`, `CatalogStateStore = SchemaFile<CatalogState>`; `ApplyRequestStore` wraps `SchemaFile<ApplyRequest>` (mode `0600`) and keeps `enqueue` (refuses when a request exists).
- Produces: `UpdateError` in `error.rs` (re-exported from `lib.rs`), `Core` variant removed in Task 5.

- [ ] **Step 1: Write failing tests** — `schema_file_rejects_unknown_schema`, `schema_file_missing_is_none`, `apply_request_is_0600` (the stored request is `0600` and was written through `atomic_write_mode`), `boot_marker_fails_closed_when_boot_id_unreadable` (defect 7: `BootRecoveryMarker::verify_with(boot_id_path)` returns `UpdateError::BootIdUnavailable` on a missing path), `update_root_open_does_not_create_directories` (defect 11).
- [ ] **Step 2: Run** `docker compose run --rm -T dev cargo test --locked -p zerodeck-updater --lib` — FAIL.
- [ ] **Step 3: Implement** `UpdateRoot`, `Schema`/`SchemaFile`, move errors to `error.rs`. Delete every `cfg(not(unix))` branch and add `#![cfg(unix)]` to `lib.rs`. Remove unused API: `acquire_maintenance_shared`, `SystemdService::{unit,endpoint}`, `Catalog::{verified_at,expires_at}`, `GenerationId: FromStr/AsRef`, `ApplyRequestStore::new`. Replace `#[allow(..)]` with `#[expect(.., reason = "..")]` where one remains.
- [ ] **Step 4: Run** the focused command and the existing updater suites — PASS (no behavior change apart from defects 7, 8, 11).
- [ ] **Step 5: Commit** `refactor(updater): introduce update root and schema file store`.

### Task 3: Test support and suite split

**Files:**
- Create: `crates/zerodeck-updater/tests/support/mod.rs`, `crates/zerodeck-updater/tests/{apply,cli}.rs`
- Modify: `crates/zerodeck-updater/tests/{catalog,stage,generation,service}.rs`
- Delete: `crates/zerodeck-updater/tests/activation.rs` (contents moved), `bootstrap_outcome_is_debug`, the `EXIT_REQUIRES_START == 75` test

**Interfaces:**
- Produces (in `support`): `TestDir::new(label) -> TestDir` (removed on drop), `GenerationFixture::new(fill: char).version(&str).build_id(&str).with_controller(bool).broken(Part).build(&GenerationStore) -> GenerationId`, `TufFixture` (moved from `tests/catalog.rs`: `TufFixture::new().await`, `.publish(&PublishSpec).await`, `.metadata_url()`, `.targets_url()`, `.trusted_root() -> PathBuf`), `MockSystemctl` with one ordered `events() -> Vec<Event>` log, `Harness::builder().running(bool).confirmed(GenerationId).staged(GenerationId).build()`.

- [ ] **Step 1: Move** helpers into `support` and tests into `apply.rs` (library fault tests) and `cli.rs` (tests using `CARGO_BIN_EXE_zerodeckctl`). The unit-file grep test is removed here and re-added as a lint in Task 14.
- [ ] **Step 2: Replace wall-clock assertions** in `tests/service.rs` (`elapsed < 500ms`, `< 1s`) with attempt counters on the mock; replace fixed sleeps with a `tokio::sync::Notify` from the mock lock holder.
- [ ] **Step 3: Rename** `boot_recovery_waits_for_a_shared_update_lock` to what it tests (exclusive lock).
- [ ] **Step 4: Run** `docker compose run --rm -T dev cargo test --locked -p zerodeck-updater` — PASS with the same test count minus the two deleted trivial tests.
- [ ] **Step 5: Commit** `test(updater): share fixtures and split activation suites`.

### Task 4: CLI cleanup and machine-readable status

**Files:**
- Modify: `crates/zerodeck-updater/src/{main,catalog,state,request}.rs`
- Test: `crates/zerodeck-updater/tests/cli.rs`

**Interfaces:**
- Produces: `Channel: clap::ValueEnum + Display` (`stable`, `testing`); `TransactionPhase`, `ApplyKind`, `LastResult` implement `Display` from their serde names; `zerodeckctl status [--json]` where JSON is `{"schema":1,"current":..,"last_confirmed":..,"rollback_target":..,"staged":..,"pending":{"phase":..,"candidate":..}|null,"quarantined":[..],"last_result":..,"last_error":..}`.
- Produces: `last_error` is stored with the update-root prefix replaced by `<root>` at write time; `redact_status_text`/`looks_like_path` are deleted.

- [ ] **Step 1: Write failing tests** — `status_json_reports_pending_phase`, `status_json_is_stable_schema` (exact key set), `cli_mutations_require_root` (non-root `upgrade`/`rollback`/`update`/`install` exit 1 with `requires root` and create no files; skipped automatically when the test runs as root), `channel_rejects_unknown_value`.
- [ ] **Step 2: Run** `... --test cli` — FAIL.
- [ ] **Step 3: Implement**: remove `channel_name`, `phase_name`, `apply_kind_name`, `crate_version` (use `running_updater_version`), `InvalidChannel`, `status_unavailable`; `enqueue_upgrade` is one function taking `Channel`; root check is one `fn require_root() -> Result<(), UpdateError>` called by every mutating command.
- [ ] **Step 4: Run** `... --test cli` and `just check` — PASS.
- [ ] **Step 5: Commit** `refactor(updater): simplify the controller CLI and add status --json`.

### Task 5: `tough` transport with idle timeouts; drop `zerodeck-core`

**Files:**
- Create: `crates/zerodeck-updater/src/transport.rs`
- Modify: `crates/zerodeck-updater/{Cargo.toml,src/catalog.rs,src/stage.rs,src/error.rs}`, root `Cargo.toml` (`aws-lc-rs` into `[workspace.dependencies]`), `Cargo.lock`
- Test: `crates/zerodeck-updater/tests/stage.rs`, `tests/catalog.rs`

**Interfaces:**
- Produces: `TransportLimits { connect: Duration, idle: Duration, max_bytes: u64 }` (defaults `10s`, `30s`, `512 MiB`); `HttpTransport::new(TransportLimits) -> Result<HttpTransport, UpdateError>` implementing `tough::Transport` (reqwest `connect_timeout` + `read_timeout`, no total `timeout`, `redirect::Policy::none()`, `http`/`https` only, user agent `zerodeckctl/<version>`, body capped at `max_bytes`).
- Produces: `Catalog::load(root: &UpdateRoot, config: &UpdateConfig-or-urls, now: SystemTime, transport: HttpTransport)` (exact param list may follow today's `load`); `Catalog::fetch_target(&self, release: &AuthorizedRelease) -> impl Stream<Item = Result<Bytes>>` via `Repository::read_target`.
- Produces: `stage_release(catalog: &Catalog, release: &AuthorizedRelease, root: &UpdateRoot, limits: StageLimits, fault: &dyn StageFault) -> Result<GenerationId, UpdateError>`; `StageLimits { max_archive_bytes: u64, max_expanded_bytes: u64, max_entries: u64 }` with today's default values copied from `zerodeck_core::ExtractLimits`/`DEFAULT_MAX_ARCHIVE_BYTES`.
- Removes: `zerodeck-core` dependency, `UpdateError::Core`, the custom digest/download code, `BoxedTransport` if `tough` accepts the transport directly.

- [ ] **Step 1: Write failing tests** — `stage_succeeds_for_slow_but_steady_body` (fixture server streams the archive in 64 KiB chunks every 100 ms for > 3 s with `idle = 1s`; expect success), `stage_fails_on_idle_body` (server stalls 2 s mid-body with `idle = 1s`; expect `UpdateError::Transport`), `stage_idle_failure_leaves_no_staging_after_sweep` (after the failure, `GenerationStore::sweep()` leaves no `.staging-*`; Review Focus 1), `stage_rejects_wrong_length_from_tuf` (served body one byte longer than the signed length).
- [ ] **Step 2: Run** `... --test stage` — FAIL.
- [ ] **Step 3: Implement** the transport and fetch targets through `tough`; extraction keeps today's extractor with these simplifications: drop `dir_paths` and `seen` (derive from `dirs ∪ files`), make `ensure_parent_dirs` a method, delete the `XHeader/GNULongName` branch if `tar` already consumes those headers (prove with a test archive containing a long name).
- [ ] **Step 4: Run** `... --test stage --test catalog`, then `just deny` — PASS; `cargo tree -p zerodeck-updater -i zerodeck-core` prints nothing.
- [ ] **Step 5: Commit** `fix(updater): stream targets through tough with idle timeouts`.

### Task 6: Crash-safe GC, cache verification and quarantine

**Files:**
- Modify: `crates/zerodeck-updater/src/{generation,stage,catalog,activation,main}.rs`
- Test: `crates/zerodeck-updater/tests/{generation,stage,catalog}.rs`

**Interfaces:**
- Produces: `GenerationStore::gc(&self, keep: &BTreeSet<GenerationId>) -> Result<(), UpdateError>` (removal = rename to `.trash-<uuid>`, sync parent, `remove_dir_all`); `GenerationStore::sweep(&self) -> Result<(), UpdateError>` (removes `.trash-*` and `.staging-*`; called under the exclusive maintenance lock before GC and at the start of staging under the catalog lock); `fn retained(state: &UpdateState, staged: Option<&GenerationId>) -> BTreeSet<GenerationId>` includes `last_confirmed`, `rollback_target`, pending previous/candidate, staged and every quarantined digest.
- Produces: `SelectionPolicy::quarantined(BTreeSet<GenerationId>)`; `Catalog::candidate` skips quarantined digests and returns the next eligible release.
- Produces: `zerodeckctl unquarantine <digest>` edits the list only (no generation lookup).

- [ ] **Step 1: Write failing tests** — `gc_crash_after_rename_leaves_no_partial_generation` (fault after rename, before delete: `generations/<id>` absent, `.trash-*` present, `sweep` removes it), `stage_restages_partial_cached_generation` (cached dir with `release.json` but no `bin/zerodeckd` is discarded and re-extracted, defect 3), `gc_keeps_quarantined_until_unquarantine`, `unquarantine_works_after_newer_release`, `selection_skips_quarantined_newest`, `gc_propagates_unreadable_catalog_state` (defect 10), `stage_enospc_keeps_current_and_sweeps_partial_tree` (Review Focus 2; `StageFault` injects `ENOSPC` while extracting).
- [ ] **Step 2: Run** `... --test generation --test stage --test catalog` — FAIL.
- [ ] **Step 3: Implement** as above; `cached_generation` calls `verify_generation` (full layout).
- [ ] **Step 4: Run** the focused suites — PASS.
- [ ] **Step 5: Commit** `fix(updater): make generation GC crash-safe and honor quarantine`.

### Task 7: Catalog lock separate from the maintenance lock

**Files:**
- Modify: `crates/zerodeck-updater/src/{activation,catalog,main}.rs`
- Test: `crates/zerodeck-updater/tests/apply.rs`

**Interfaces:**
- Consumes: `UpdateRoot::{catalog_lock_path, maintenance_lock_path}` (Task 2).
- Produces: `update` holds only the exclusive **catalog** lock while refreshing TUF metadata, downloading and extracting into `.staging-*`; it takes the exclusive **maintenance** lock only to rename the staged tree into `generations/` and persist the authorization. `apply` reads only `state/catalog.json` (no TUF datastore access) and waits for the maintenance lock up to `APPLY_BUDGET` (Task 8). `acquire_maintenance_exclusive` keeps its blocking-with-deadline form; the 30 s `APPLY_LOCK_WAIT` constant is deleted.

- [ ] **Step 1: Write failing tests** — `update_holding_catalog_lock_does_not_block_apply` (hold the catalog lock in the test; enqueue and apply succeed), `update_commit_waits_for_running_apply` (apply holds maintenance; update's commit waits and then completes without interleaving `current`), `apply_never_strands_request_on_busy` (lock held past the deadline: apply fails terminally, request moved to `last_result = busy`, next `upgrade` is accepted).
- [ ] **Step 2: Run** `... --test apply update_` — FAIL.
- [ ] **Step 3: Implement**; fix the comment that claimed `update` holds a shared guard.
- [ ] **Step 4: Run** `... --test apply` — PASS.
- [ ] **Step 5: Commit** `fix(updater): keep metadata checks from blocking activation`.

### Task 8: One revert path, readiness seam and timeout budget

**Files:**
- Create: `crates/zerodeck-updater/src/{systemd,readiness,apply}.rs` (from `service.rs` and `activation.rs`)
- Delete: `crates/zerodeck-updater/src/{service,activation}.rs`
- Modify: `crates/zerodeck-updater/src/{lib,main}.rs`, `deploy/systemd/zerodeck-apply.service`
- Test: `crates/zerodeck-updater/tests/{apply,service}.rs` (rename `service.rs` → `readiness.rs`)

**Interfaces:**
- Produces (`systemd.rs`): `trait Systemctl` with required methods only: `is_active`, `stop`, `start`, `invocation_id`, `start_wait(unit) -> UnitResult`, `daemon_reload`, `enable`, `disable`, `is_enabled` (no default "unsupported" bodies); `TokioSystemctl`; constants `STOP_TIMEOUT = 30s`, `START_TIMEOUT = 10s`, `ACTIVATION_DEADLINE = 60s`, `APPLY_BUDGET = 2 * (STOP + START + ACTIVATION_DEADLINE) + 30s` (= 230 s).
- Produces (`readiness.rs`): `SystemdService::begin_activation(&self) -> Result<InvocationBaseline, UpdateError>`; `verify_ready(service: &SystemdService, expected: &ReleaseIdentity, baseline: &InvocationBaseline, deadline: Duration) -> Result<(), UpdateError>`; one private `get_bounded(path: &str, limit: usize, auth: Option<&str>) -> Result<Bytes, UpdateError>` used by token, `/readyz` and frontend-asset probes. `ReleaseIdentity` is the single (version, build_id) type, built from `ReleaseManifest`.
- Produces (`apply.rs`): `enqueue_upgrade(root, catalog, channel, start: bool)`, `enqueue_rollback(root)`, `apply_pending(root, catalog, service, fault)`, `enum Recovery<'a> { Boot, AfterApply(&'a SystemdService) }`, `recover(root: &UpdateRoot, mode: Recovery<'_>) -> Result<RecoveryOutcome, UpdateError>` (boot mode also records the boot marker, replacing `recover_before_boot`), one private `revert(root, pending, service: Option<&SystemdService>)`.
- Modifies: `zerodeck-apply.service` `TimeoutStartSec=240`, `TimeoutStopSec=240`.

- [ ] **Step 1: Write failing tests** — `boot_recovery_restores_confirmed_candidate_when_current_differs` (defect 2: phase `Confirmed`, `current` = previous; after `recover(Boot)`, `current` = candidate, no pending, marker recorded), `apply_unit_timeout_covers_budget` (parse the embedded unit; `TimeoutStartSec >= APPLY_BUDGET`), `start_apply_reads_result_once` (mock `start_wait` returns once; no polling calls in the event log), `rollback_ignores_clock_floor` (Review Focus 3: clock earlier than the persisted floor; `rollback` applies, `upgrade` is refused), and keep every existing interrupted-transition test green.
- [ ] **Step 2: Run** `... --test apply --test readiness` — FAIL.
- [ ] **Step 3: Implement**: delete `enqueue_upgrade_with_channel`, `apply_pending_with`, `recover_with`, `recover_before_boot`, `RecoveryMode`, `write_phase`/`write_start_attempted` (one `update_pending(id, |p| ..)`), the duplicate `request.validate()` call, `ReadinessStatePoisoned`, `InvocationBaselineMissing`, `SystemctlUnsupported`. `zerodeckctl upgrade` calls `systemctl start --wait zerodeck-apply.service` without a client timeout and reads the result once.
- [ ] **Step 4: Run** the updater suites and `just check` — PASS; `wc -l crates/zerodeck-updater/src/*.rs` total is at least 600 lines below the starting 6,238.
- [ ] **Step 5: Commit** `refactor(updater): unify recovery and derive one apply budget`.

### Task 9: Controller config, SteamOS layout and embedded units

**Files:**
- Create: `crates/zerodeck-updater/src/{config,units}.rs`
- Modify: `crates/zerodeck-updater/src/main.rs`, `deploy/systemd/{zerodeck,zerodeck-apply,zerodeck-recovery,zerodeck-check}.service`, `deploy/systemd/zerodeck-check.timer`
- Test: unit tests in `config.rs`, `units.rs`; `tests/cli.rs`

**Interfaces:**
- Produces: `UpdateConfig { schema: u32 (1), metadata_url: Url, targets_url: Url, channel: Channel }` (`Schema` impl, `deny_unknown_fields`, file mode `0644`); `UpdateRoot::config() -> SchemaFile<UpdateConfig>`; `DEFAULT_ROOT = "/home/.zerodeck"`.
- Produces: `units::UNITS: [(&str /*file name*/, &str /*contents*/); 5]` via `include_str!("../../../deploy/systemd/<name>")`; `units::keep_list() -> String` with exactly: the five `/etc/systemd/system/<name>` paths, `/etc/systemd/system/*.wants/zerodeck*`, `/etc/atomic-update.conf.d/zerodeck.conf`.
- Units: every `ExecStart=` is `/home/.zerodeck/bin/zerodeckctl <cmd> --root /home/.zerodeck`; `ConditionPathExists=` points under `/home/.zerodeck`; `zerodeck-check.service` drops `EnvironmentFile=`.

- [ ] **Step 1: Write failing tests** — `config_rejects_unknown_fields`, `config_roundtrip`, `units_use_default_root` (every `ExecStart`/`Condition*` path starts with `/home/.zerodeck`), `keep_list_matches_units`, `update_reads_config_without_environment` (CLI test: `update --check-only` with only `config.json` present and a `TufFixture`; no `ZERODECK_UPDATE_*` vars).
- [ ] **Step 2: Run** `... --lib config units` and `... --test cli update_reads_config` — FAIL.
- [ ] **Step 3: Implement**; remove `environment_url`, `DEFAULT_READY_URL` stays the readiness endpoint; `--channel` defaults to the config's channel.
- [ ] **Step 4: Run** focused tests and `just check` — PASS.
- [ ] **Step 5: Commit** `feat(updater): persist controller config and move the root to /home`.

### Task 10: `install`, `uninstall` and `verify-archive`

**Files:**
- Create: `crates/zerodeck-updater/src/install.rs`, `crates/zerodeck-updater/tests/install.rs`, `deploy/update/install.sh`
- Delete: `crates/zerodeck-updater/src/bootstrap.rs`, `crates/zerodeck-updater/tests/bootstrap.rs`, `deploy/update/bootstrap.sh`
- Modify: `crates/zerodeck-updater/src/{lib,main}.rs`

**Interfaces:**
- Produces: `HostPaths { update_root: PathBuf, systemd_dir: PathBuf, keep_list_dir: PathBuf, os_release: PathBuf, homebrew: PathBuf, daemon_port: u16 }` with `HostPaths::system()` (`/home/.zerodeck`, `/etc/systemd/system`, `/etc/atomic-update.conf.d`, `/etc/os-release`, `/home/deck/homebrew`, `1337`).
- Produces: `InstallRequest { archive: PathBuf, sha256: GenerationId, trusted_root: PathBuf, trusted_root_sha256: String, config: UpdateConfig, start: bool }`; `async fn install(req: &InstallRequest, host: &HostPaths, systemd: &dyn Systemctl, service: &SystemdService) -> Result<InstallOutcome, UpdateError>`; `InstallOutcome { generation: GenerationId, changed: bool, started: bool }`; `async fn uninstall(host: &HostPaths, systemd: &dyn Systemctl, keep_generations: bool) -> Result<(), UpdateError>`; `UpdateError::InstallRefused(InstallRefusal)` with `InstallRefusal::{NotRoot, Decky { unit: String }, LegacyZeroDeck { exec_start: String }, MissingKeepList, DifferentGeneration { installed: GenerationId, requested: GenerationId }, PortInUse { port: u16 }}`.
- Produces: `fn verify_archive(path: &Path, limits: StageLimits) -> Result<ArchiveReport, UpdateError>` with `ArchiveReport { sha256: GenerationId, version: Version, build_id: String }`; hidden CLI `zerodeckctl verify-archive <file>` prints it as JSON.
- Behavior: refusals are checked before any mutation; mutations happen in this order and are journaled in memory for reverse rollback: update root tree → staged generation → controller copy (`<root>/bin/zerodeckctl`, `0755`) → `config.json` → trusted root into `tuf/` → `current` → units → keep-list (SteamOS only) → `daemon-reload` → enable `zerodeck-recovery.service` and `zerodeck.service` → with `--start`: start + `verify_ready` + confirm. Same digest already installed and confirmed → `changed: false`, nothing written.
- `install.sh` (≤ 80 lines, `set -euo pipefail`, shellcheck clean): `install.sh <tag> <sha256-of-SHA256SUMS.txt> [--start]` downloads `SHA256SUMS.txt`, `zerodeck-<tag>-linux-x86_64.tar.gz`, `zerodeckctl-<tag>-linux-x86_64` from the GitHub release into `mktemp -d`, verifies the sums file digest then each asset with `sha256sum -c`, and execs `sudo ./zerodeckctl install ...`; the trusted root and URLs are arguments it passes through.

- [ ] **Step 1: Write failing tests** (non-root, temp `HostPaths`, `MockSystemctl`) — `install_refuses_decky_unit`, `install_refuses_legacy_zerodeck_unit`, `install_refuses_steamos_without_keep_list` (`os_release` with `ID=steamos`), `install_refuses_wrong_archive_sha`, `install_refuses_wrong_root_sha`, `install_refuses_unsigned_root` (root JSON with `"sig":"00"`), `install_refuses_different_generation`, `install_refuses_port_in_use_and_names_it` (Review Focus 5: the test binds a free port and passes it as `HostPaths::daemon_port`), `install_is_idempotent_for_same_digest`, `install_rolls_back_on_failure_after_units` (mock `enable` fails: every created path is gone, units absent), `install_start_confirms_readiness`, `uninstall_preserves_homebrew`, `verify_archive_reports_identity`, `verify_archive_rejects_symlink_member`, `verify_archive_requires_controller`.
- [ ] **Step 2: Write privileged tests** in the same file, `#[ignore = "requires root; run with just test-privileged"]`: `privileged_install_creates_root_owned_tree` (root under `/opt/zerodeck-test-<uuid>`, asserts uid 0, modes `0755`/`0600` for state) and `privileged_install_refuses_user_writable_ancestor` (ancestor chowned to `ZERODECK_TEST_HOST_USER`).
- [ ] **Step 3: Run** `... --test install` — FAIL.
- [ ] **Step 4: Implement**; `GenerationLayout::open` now requires `bin/zerodeckctl` for managed generations (flag on the call used by the updater; `zerodeckd` keeps tolerating its absence until Task 12 ships it — then require it everywhere). Remove `--convert-existing-zerodeck`, `--import-inactive-decky`, `Bootstrap` CLI.
- [ ] **Step 5: Run** `... --test install`, `just test-privileged` — PASS.
- [ ] **Step 6: Commit** `feat(updater): install and uninstall from the controller`.

### Task 11: Controller promotion after confirmation

**Files:**
- Modify: `crates/zerodeck-updater/src/{apply,install}.rs`
- Test: `crates/zerodeck-updater/tests/apply.rs`

**Interfaces:**
- Produces: `fn promote_controller(root: &UpdateRoot, generation: &GenerationLayout, running: &Version) -> Result<bool, UpdateError>`: when `generation.manifest().version > running`, copy `bin/zerodeckctl` to `<root>/bin/.zerodeckctl.<uuid>`, `0755`, `sync_all`, rename over `<root>/bin/zerodeckctl`, sync dir. Called only after a successful confirmation with no pending transaction; never on rollback/revert.

- [ ] **Step 1: Write failing tests** — `confirmed_upgrade_promotes_newer_controller`, `same_version_does_not_replace_controller`, `rollback_never_downgrades_controller`, `promotion_crash_before_rename_keeps_old_controller` (fault hook between write and rename).
- [ ] **Step 2: Run** `... --test apply promote` — FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `feat(updater): promote the controller shipped with a confirmed generation`.

### Task 12: Dockerfile `package`/`dist` stages and pins

**Files:**
- Modify: `Dockerfile`, `justfile`, `compose.yaml` (daemon env merge), `.github/workflows/ci.yml` (`docker` job steps only)
- Delete: `deploy/update/package-release.sh`, `deploy/update/tests/{archive.py,release.sh,release-fixture.sh,bootstrap.sh,fixtures.sh}`, `deploy/update/tuf/loader-target.template.json`

**Interfaces:**
- Produces: Dockerfile stages `package` (debian bookworm-slim pinned by digest; inputs: `zerodeckd`, `zerodeckctl`, python host, frontend; writes `release.json` `{"schema":1,"version":"${VERSION}","build_id":"${ZERODECK_BUILD_ID}"}` with `jq -n`-free `printf`; tars with `tar --sort=name --mtime=@${SOURCE_DATE_EPOCH} --owner=0 --group=0 --numeric-owner -C <tree> . | gzip -n -9`; copies `install.sh`; writes `SHA256SUMS.txt`) and `dist` (`FROM scratch`, `COPY --from=package /out/ /`). Asset names: `zerodeck-${TAG}-linux-x86_64.tar.gz`, `zerodeckctl-${TAG}-linux-x86_64`, `install.sh`, `SHA256SUMS.txt`.
- Produces recipes: `release-assets tag` → `_release-assets` (`docker buildx build --platform linux/amd64 --target dist --build-arg TAG=.. --build-arg ZERODECK_BUILD_ID=${ZERODECK_BUILD_ID:-dev} --build-arg SOURCE_DATE_EPOCH=$(git log -1 --format=%ct) --output type=local,dest=dist/<tag> .`); `test-release-package archive` → `{{ dev }} just _test-release-package` (runs `cargo run --locked -q -p zerodeck-updater --bin zerodeckctl -- verify-archive <archive>`); `verify-managed-image` gets a private `_` form and restores `--format '{{{{.Architecture}}'` (no host python). `dist`/`dist-managed` recipes are removed.
- Pins: `ARG CARGO_CHEF_SHA256_AMD64`/`_ARM64` (verify the downloaded archive, not a sidecar); every `FROM` pinned `@sha256:` (obtain with `docker buildx imagetools inspect <ref> --format '{{json .Manifest.Digest}}'`); one global `ARG PNPM_VERSION=12.16.1` used by `frontend-builder`; `ARG ZERODECK_BUILD_ID` + validation moved after `cargo chef cook` and no `ENV`; `--locked` on cook and build; `zerodeckctl` removed from `runtime`; duplicate `SHELL` removed.

- [ ] **Step 1: Write the check first** — extend `_toolchain-sync` to fail on any `FROM` without `@sha256:` and on a second literal `pnpm@` outside `ARG PNPM_VERSION`; run `just lint-infra` — FAIL.
- [ ] **Step 2: Implement** the Dockerfile and recipes.
- [ ] **Step 3: Verify** — `just release-assets ci-local` produces the four assets; `just test-release-package dist/ci-local/zerodeck-ci-local-linux-x86_64.tar.gz` prints the identity JSON; running `just release-assets ci-local` twice yields identical `SHA256SUMS.txt`; `docker run --rm --entrypoint sh zerodeck:runtime -c 'test ! -e /usr/local/lib/zerodeck'` exits 0; a second `docker build` with a new `ZERODECK_BUILD_ID` reuses the `cargo chef cook` layer (`CACHED` in the log).
- [ ] **Step 4: Update** the `docker` CI job to `just release-assets ci-${RUN}` + `just test-release-package ...` + `just verify-managed-image ...`, values passed through `env:`.
- [ ] **Step 5: Run** `just ci` — PASS.
- [ ] **Step 6: Commit** `build(docker): package release assets from a dist stage`.

### Task 13: Release scripts fixed and linted

**Files:**
- Modify: `deploy/update/{publish-release,promote,verify-stable-evidence}.sh`, `deploy/update/tests/{publish,promotion,stable-gate}.sh`, `justfile` (`lint-infra`, `check-release`)
- Test: the three shell test files

**Interfaces:**
- Produces: `gh`/`curl` resolved with `command -v "${GH_BIN:-gh}"`; uploads via `gh api --method POST "https://uploads.github.com/repos/<repo>/releases/<id>/assets?name=<n>" --input <file> -H 'Content-Type: application/octet-stream'` (no token on argv); one verification pass after upload; `release_draft` and the three-way `sha256_file` fallback deleted (Linux `sha256sum` only); `verify-stable-evidence.sh` matches tag and version by equality and fails loudly on a missing option value.
- Produces: `promote.sh --dry-run` renders `{"request": {schema, tag, version, channel, build_id, target_path, sha256, length, asset_url, source_commit, evidence_sha256, run_url}}` and validates it; without `--dry-run` it dispatches that exact body.
- Produces recipes: `check-release` → `_check-release` (runs the three shell tests) added to `_check`; `lint-infra` adds `docker run --rm -v "$PWD:/repo:ro" -w /repo koalaman/shellcheck:v0.11.0 deploy/update/*.sh deploy/update/tests/*.sh`.

- [ ] **Step 1: Write failing tests** — `publish.sh`: a case that puts the fake `gh`/`curl` on `PATH` without `--gh`; a case asserting the token never appears in the fake's argv log. `stable-gate.sh`: evidence for `v0.1.0-rc.1` must not satisfy `v0.1.0`. `promotion.sh`: `--dry-run` output has exactly the twelve request keys under `request`.
- [ ] **Step 2: Run** `just check-release` — FAIL.
- [ ] **Step 3: Implement**; make shellcheck clean.
- [ ] **Step 4: Run** `just check-release` and `just lint-infra` — PASS.
- [ ] **Step 5: Commit** `fix(release): resolve tools via PATH and lint release scripts`.

### Task 14: Workflows call recipes only

**Files:**
- Modify: `.github/workflows/{ci,release-please,promote-tuf}.yml`, `justfile`, `.github/CODEOWNERS`
- Test: `just lint-infra` (actionlint, zizmor)

**Interfaces:**
- Produces recipes (public → private): `release-identity` (prints `tag`, `version`, `build_id` for `$GITHUB_OUTPUT`), `release-verify tag` (verify assets + attestations once), `release-publish tag release_id` (calls `publish-release.sh --publish`), `promote-request tag channel [--dry-run]`, `lint-units` (the former unit-file grep test: `zerodeck.service` has no `Wants=`/`Requires=` on the recovery unit; added to `lint-infra`).
- `ci.yml`: workflow `permissions: {}`, each job `permissions: contents: read`; `${{ }}` values only in `env:`; `check-release` runs in the `docker` job; old `updater-vm` job removed here (re-added in Task 18).
- `release-please.yml`: no workflow-level `concurrency`; publish job `environment: release`, `RELEASE_PLEASE_TOKEN` required (no fallback); build+push by digest only (`outputs: type=image,push-by-digest=true`), one provenance mechanism (`attest-build-provenance`), `cache-from: type=gha,scope=runtime`, no `cache-to`; tags `X.Y.Z`, `X.Y`, `latest` created with `docker buildx imagetools create` after `release-publish`; commit check fails closed on a non-SHA `target_commitish`; `release_id` taken from `upload_url` only.
- `promote-tuf.yml`: job `if: github.ref == 'refs/heads/main'`; steps call `just verify-stable-evidence`, `just test-release-package`, `just promote-request`; build-id self-comparison removed.
- `CODEOWNERS`: `/deploy/ @ygorsimoes`, `/deploy/update/evidence/ @ygorsimoes`, `/.github/workflows/ @ygorsimoes` (owners added later by the maintainer).

- [ ] **Step 1: Run** `just lint-infra` on the current tree and record its findings (baseline).
- [ ] **Step 2: Implement** recipes and workflows.
- [ ] **Step 3: Verify** — `grep -nE '^\s+(python3|bash deploy|gh |curl )' .github/workflows/*.yml` prints nothing; `just lint-infra` PASS; `just promote-request v0.0.0-test testing --dry-run` against the Task 12 fixture assets prints a valid request.
- [ ] **Step 4: Run** `just ci` — PASS.
- [ ] **Step 5: Commit** `ci: drive release and promotion through just recipes`.

### Task 15: `vm` container and pinned Arch image

**Files:**
- Modify: `Dockerfile` (new `vm` stage), `justfile`, `compose.yaml` (named volume `vm-cache` only), `.gitignore` (`.vm/`)

**Interfaces:**
- Produces: Dockerfile `vm` stage: `debian:bookworm-slim@sha256:..` with `qemu-system-x86 qemu-utils ovmf openssh-client cloud-image-utils python3 ca-certificates curl`, user `vm` (uid from `DEV_UID`), `ARG ARCH_IMAGE_URL`, `ARG ARCH_IMAGE_SHA256` (Global Constraints values), image tag `zerodeck-vm:local`.
- Produces recipes: `vm-image` → `_vm-image`: downloads `ARCH_IMAGE_URL` into the `vm-cache` volume as `arch-<sha>.qcow2` if missing, verifies SHA-256, fails closed (`exit 1`) on mismatch; prints the cached path. Recipes run the container with `docker run --rm --cap-drop ALL --security-opt no-new-privileges ...`, adding `--device /dev/kvm --group-add "$(stat -c %g /dev/kvm)"` only when `[ -w /dev/kvm ]`.

- [ ] **Step 1: Verify failure first** — `just vm-image` with a deliberately wrong `ARCH_IMAGE_SHA256` override exits 1 and leaves no cached file.
- [ ] **Step 2: Implement.**
- [ ] **Step 3: Verify** — `just vm-image` downloads once; a second run prints the cached path without downloading.
- [ ] **Step 4: Run** `just ci` — PASS.
- [ ] **Step 5: Commit** `build(vm): add a pinned QEMU container and Arch cloud image`.

### Task 16: Local TUF fixture generator

**Files:**
- Create: `crates/zerodeck-updater/examples/vm_fixture.rs`
- Modify: `crates/zerodeck-updater/tests/support/mod.rs` (expose the shared signing helpers to the example through a `#[path]` module or move them into the example and have tests call it — choose one, no duplication)

**Interfaces:**
- Produces: `cargo run --locked -p zerodeck-updater --example vm_fixture -- --out <dir> --release <archive>[:<channel>] ... --expires-in-days 7` writes `<dir>/metadata/` (root, timestamp, snapshot, targets), `<dir>/targets/linux-x86_64/zerodeck-<version>.tar.gz` (signed custom metadata `channel`, `version`, `build_id`, `min_updater`) and `<dir>/root.json` + `<dir>/root.json.sha256`.

- [ ] **Step 1: Write a failing test** in `tests/catalog.rs`: `vm_fixture_output_is_loadable` runs the example's library entry (`fn build_fixture(args) -> Result<()>` in the example, called via `std::process::Command::new(env!("CARGO"))`), loads the result with `Catalog::load`, and selects the release.
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement.** **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `test(updater): generate signed TUF fixtures for the VM gate`.

### Task 17: Python VM harness with the SteamOS-like profile

**Files:**
- Create: `deploy/vm/pyproject.toml` (ruff/pyright config identical to `hosts/python`, `dependencies = []`), `deploy/vm/zerodeck_vm/{__init__,__main__,cli,qemu,ssh,seed,guest,scenarios}.py`, `deploy/vm/tests/test_{seed,cli}.py`, `deploy/vm/README.md`
- Delete: `deploy/update/tests/vm.sh`, `deploy/update/tests/README.md`
- Modify: `justfile` (`test-vm`, `_py-lint`/`_py-test` cover `deploy/vm`)

**Interfaces:**
- Produces: `python -m zerodeck_vm --image <qcow2> --assets <dist/tag> [--candidate-assets <dist/tag2>] [--profile steamos-like|native] [--uefi] [--with-os-update] [--scenario NAME ...] [--work-dir DIR] [--boot-timeout S]`; exit `0`/`1`/`77`.
- `seed.py`: `build_seed(public_key: str, profile: Profile) -> tuple[str, str]` returns NoCloud `user-data`/`meta-data`: user `deck` uid 1000, `NOPASSWD` sudoers drop-in, `python` package, `/home/deck/homebrew/{plugins,services}`; for `steamos-like`: fstab line `/usr /usr none bind,ro 0 0`, `/etc/os-release` overridden with `ID=steamos`, `/etc/atomic-update.conf.d/` created, snapshots `/etc` → `/var/lib/zerodeck-vm/etc.orig` and `/var` → `/var/lib/zerodeck-vm/var.orig.tar` at first boot (excluding the snapshot dir itself).
- `qemu.py`: `Vm.start()`, `Vm.kill()` (hard power-off), `Vm.wait_ssh(timeout)`, `Vm.reboot()`; fresh overlay per run; SSH forward on a port QEMU binds (`hostfwd=tcp:127.0.0.1:0-:22` is not supported — pick a free port by binding and passing it, retry once on `EADDRINUSE`); TUF fixture served by `python3 -m http.server` in the container at `10.0.2.2:<port>`.
- `guest.py`: `status() -> dict` (runs `sudo /home/.zerodeck/bin/zerodeckctl status --json`), `ready() -> dict` (`/readyz` with the token), `main_pid_exe() -> str`, `invocation_id() -> str`.
- `scenarios.py` (each asserts `current` digest, running executable under `/home/.zerodeck/generations/<digest>/bin/`, `/readyz` version/build id, InvocationID change where expected, no pending transaction): `install`, `install-start`, `port-collision`, `update-upgrade` (candidate with a distinct build id from `--candidate-assets`), `offline-rollback` (QEMU NIC link down via monitor `set_link`), `unhealthy-candidate`, `corrupt-rollback`, `hard-poweroff-during-apply`, `sigkill-apply-helper`, `reboot-during-recovery`, `second-recovery`, `steamos-update` (steamos-like: restore `/etc` from snapshot plus files matching every keep-list glob, restore `/var` from snapshot, reboot; daemon healthy on the same generation; native: real `steamos-update` only with `--with-os-update`), `uninstall`. The selected scenario set is printed before the run.
- Produces recipe: `test-vm *args` → `_test-vm`: builds `zerodeck-vm:local`, ensures `vm-image`, builds release assets (`release-assets vm-a`, and a second build `vm-b` with `ZERODECK_BUILD_ID=vm-b`), generates the TUF fixture in the dev container, then runs the harness in the `vm` container.

- [ ] **Step 1: Write failing unit tests** — `test_seed_steamos_like_mounts_usr_read_only`, `test_seed_native_has_no_overrides`, `test_cli_prints_scenario_set`, `test_cli_exit_77_without_image`.
- [ ] **Step 2: Run** `just check-python` — FAIL.
- [ ] **Step 3: Implement** the package; strict pyright clean.
- [ ] **Step 4: Run** `just check-python` — PASS.
- [ ] **Step 5: Run the real gate** `just test-vm` (TCG locally, expect tens of minutes) — every scenario PASS; keep the work-dir logs for the PR description.
- [ ] **Step 6: Commit** `test(vm): run the updater gate on a SteamOS-like Arch VM`.

### Task 18: Real SteamOS path and CI job

**Files:**
- Modify: `justfile` (`steamos-vm-install`), `deploy/vm/zerodeck_vm/{cli,qemu}.py`, `deploy/vm/README.md`, `.github/workflows/ci.yml` (`updater-vm` job)

**Interfaces:**
- Produces: `just steamos-vm-install <steamdeck-recovery.img.bz2> [<disk.qcow2>]` → `_steamos-vm-install`: decompresses into the `vm-cache` volume, creates a 64 GiB qcow2 target, boots OVMF with the recovery image and the target as NVMe, VNC on `127.0.0.1:5900` (published `-p 127.0.0.1:5900:5900`), prints the README steps. Never committed; the README states the image comes from Valve's download page and must not be redistributed.
- Produces: harness `--uefi` (OVMF code read-only + per-run vars copy) and `--profile native`.
- `updater-vm` CI job: non-required, `timeout-minutes: 60`, `permissions: contents: read`, enables KVM (`echo 'KERNEL=="kvm", GROUP="kvm", MODE="0666"' | sudo tee /etc/udev/rules.d/99-kvm.rules` is the one allowed inline setup line, or a `just ci-enable-kvm` recipe — prefer the recipe), caches `vm-cache` by `ARCH_IMAGE_SHA256` with `actions/cache` pinned by SHA, runs `just test-vm`. Exit 77 fails the job.

- [ ] **Step 1: Write failing test** `test_cli_uefi_requires_ovmf` (missing OVMF path → exit 1 with a clear message).
- [ ] **Step 2: Implement**; **Step 3:** `just check-python`, `just lint-infra` — PASS.
- [ ] **Step 4: Commit** `ci: run the VM gate with KVM and document the real SteamOS path`.

### Task 19: Documentation

**Files:**
- Move: `deploy/update/COMPATIBILITY.md` → `deploy/update/evidence/TEMPLATE.md` (add rows: controller path, keep-list survival, `/home/.zerodeck` ownership after OS update)
- Modify: `deploy/update/RELEASE.md` (assets, publication order, promotion, owner actions, TUF role recovery; drop the installer retelling), `deploy/update/evidence/README.md`, `deploy/update/tuf/README.md`, `README.md` (On Steam Deck: install with `install.sh` or `zerodeckctl install`, `update`/`upgrade`/`rollback`/`uninstall` with full paths, VM gate commands), `AGENTS.md` (crate graph line `zerodeck-updater ── zerodeck-platform, zerodeck-protocol`; new recipes; VM gate rules), `docs/superpowers/specs/2026-11-26-update-and-upgrade-design.md` (one line at the top pointing to the amendment)

- [ ] **Step 1:** `grep -rn "/var/lib/zerodeck\|/usr/local/lib/zerodeck\|bootstrap.sh\|update.env\|managed.tar.gz\|test-updater-vm\|convert-existing\|import-inactive" --include=*.md --include=*.yml --include=*.rs --include=justfile .` lists only historical plan/spec files.
- [ ] **Step 2:** Edit docs; every command shown exists (`just --list` and `zerodeckctl --help` agree).
- [ ] **Step 3: Commit** `docs: describe the SteamOS layout, installer and VM gate`.

### Task 20: Final verification

- [ ] **Step 1:** `just ci` — exit 0.
- [ ] **Step 2:** `just test-vm` — exit 0 (or record the TCG run log and rely on the CI KVM job).
- [ ] **Step 3:** Update the PR #23 description: what changed, owner actions, PENDING hardware evidence; push only with the owner's go-ahead.
