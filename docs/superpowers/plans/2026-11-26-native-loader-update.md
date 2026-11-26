# Native ZeroDeck Update and Upgrade Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a verified native SteamOS update that preserves the last healthy ZeroDeck generation and can recover it without a network connection.

**Architecture:** `zerodeckctl` stages a TUF-authorized complete package. A stable systemd apply unit runs the transaction independently of the daemon and the caller; the recovery unit reconciles incomplete activation before the daemon starts on boot.

**Tech Stack:** Rust, `tough`, TUF-on-CI, reqwest/rustls, SHA-256, tar/gzip, `rustix`, systemd, GitHub Actions, QEMU.

**Spec:** [ZeroDeck and Decky Plugin Update Design](../specs/2026-11-26-update-and-upgrade-design.md), loader and shared sections.

## Global Constraints

- Rust `1.100.0`, Edition `2024`; Python `>=3.10`, stdlib only; frontend uses `pnpm@12.16.1` and Steam's React globals.
- MIT clean-room implementation; keep the Cargo graph acyclic and pass `cargo deny` on the actual lockfile.
- Keep `SafePath`, atomic writes, archive containment, fail-closed remote SHA-256 for managed updates, `0755` executables and the existing IPC, privilege, CORS and auth boundaries.
- No installs from SteamOS's read-only `/usr`; test a persistent root-controlled install root and systemd configuration across an actual SteamOS update before Stable.
- The first supported native distribution target is SteamOS `linux-x86_64` with systemd; Docker images are upgraded externally by digest.
- `just check` after each change; use `just ci` for changes to `Dockerfile`, `compose.yaml`, `justfile`, `.devcontainer/`, or `.github/**`; keep required CI job IDs stable.

## Review Focus

1. The active-reference rename succeeds but its directory sync fails: subsequent recovery observes disk and restores the last confirmed generation (Task L1).
2. A valid signed catalog offers incompatible architecture or updater version, or reuses a version label: no activation is prepared (Task L2).
3. The old process or an unrelated endpoint answers a probe: the expected service invocation and build must match (Task L3).
4. Reboot or `SIGKILL` between service stop and confirmation, including during rollback: a second recovery pass is safe (Task L4).
5. The release exists but uploads or promotion failed: retry and continue using the prior authorization until the exact candidate is complete (Task L5).

## File map

| Path | Responsibility |
|---|---|
| `crates/zerodeck-platform/src/maintenance.rs` | Shared advisory lock primitive; opener controls path and validates ownership once before use. |
| `crates/zerodeck-updater/src/{main,lib,state,generation,catalog,stage,service}.rs` | CLI, persisted update state, generation store, TUF selection, archive preparation, systemd activation. |
| `crates/zerodeckd/src/{main,install}.rs` and `crates/zerodeck-server/src/routes/mod.rs` | Managed layout resolution and private-to-local readiness contract; keep standalone startup working. |
| `deploy/systemd/{zerodeck,zerodeck-apply,zerodeck-recovery,zerodeck-check}.service`, `deploy/systemd/zerodeck-check.timer` | Daemon, apply/recover helpers and opt-in metadata checks. |
| `.github/workflows/release-please.yml`, `release-please-config.json`, `Dockerfile`, `justfile`, `deploy/update/` | Package layout, verified bootstrap, publish/promotion workflow and operations runbook. |
| `crates/zerodeck-updater/tests/`, `deploy/update/tests/`, `README.md` | Repeatable fault tests, VM/Deck release evidence and user commands. |

Focused Rust invocations below run in the Docker dev environment via
`docker compose run --rm dev cargo test --locked ...`; use the same test filter for the
failing and passing run. The current verification recipe is `just check`.

---

### Task L1: Durable state and generation switching

**Files:** `crates/zerodeck-platform/src/{lib,maintenance}.rs`,
`crates/zerodeck-updater/{Cargo.toml,src/{lib,state,generation}.rs,tests/generation.rs}`,
`Cargo.lock`.

**Interfaces:** `MaintenanceLock::try_shared(path: &Path) -> io::Result<MaintenanceLock>`
and `MaintenanceLock::try_exclusive(path: &Path) -> io::Result<MaintenanceLock>` return
RAII guards, with `WouldBlock` on a conflicting owner. `GenerationId` is a validated
lowercase SHA-256 hex string. `GenerationStore::current(&self) -> Result<Option<GenerationId>, UpdateError>`,
`switch_to(&self, id: &GenerationId) -> Result<(), UpdateError>` and
`StateStore::{read(&self) -> Result<UpdateState, UpdateError>, write(&self, state: &UpdateState) -> Result<(), UpdateError>}`
are used by L2–L4. `UpdateState` has schema `1`, `last_confirmed`, `rollback_target`,
quarantined digests, and at most one pending transaction `{id, previous, candidate, phase, was_running}`.
`last_confirmed` is the healthy active generation and `rollback_target` is the most
recent distinct healthy generation. A newly confirmed apply moves the old
`last_confirmed` to `rollback_target`; a reverted trial does not change either.

- [ ] Add `generation_recovers_from_rename_sync_error`: inject an error after the rename;
  assert `current()` can read the candidate and replaying the persisted previous reference
  returns the previous generation. Assert repeated reconciliation does not change it.
- [ ] Add `maintenance_guard_blocks_exclusive_during_shared`: separate handles may share;
  an exclusive guard conflicts until all shared handles close. A guard is not inherited by
  a spawned worker.
- [ ] Run `docker compose run --rm dev cargo test --locked -p zerodeck-updater --test generation`; expect missing transaction/lock interfaces on the red run.
- [ ] Implement the interfaces above. Sync file contents and relevant directory entries;
  use one same-directory atomic rename for the active reference. Reject unknown state
  schema, missing generations and paths escaping the owned root. Never infer success from
  a failed sync. Keep `last_confirmed`, `rollback_target` and every pending generation
  during GC.
- [ ] Rerun the focused command; both tests pass. Commit `feat(updater): persist generation transactions`.

### Task L2: Verified selection and preparation

**Files:** `crates/zerodeck-updater/src/{catalog,stage,state}.rs`,
`crates/zerodeck-updater/tests/{catalog,stage}.rs`, `crates/zerodeck-updater/Cargo.toml`,
`Cargo.lock`, `deny.toml` (only if an allowed dependency needs an explicit clarification).

**Interfaces:** `Catalog::candidate(&self, policy: &SelectionPolicy) -> Result<Option<AuthorizedRelease>, UpdateError>`
returns an existing target authorized for `linux-x86_64`. `AuthorizedRelease` records
channel, SemVer, archive digest and size, build ID and minimum updater version.
`async fn stage_release(release: &AuthorizedRelease, store: &GenerationStore) -> Result<GenerationId, UpdateError>`
returns only after full extraction validation and durability. `release.json` inside the
package records the build ID; the TUF target records the digest of the whole archive.
`Catalog::authorize_staged(&self, release: &AuthorizedRelease, now: SystemTime) -> Result<(), UpdateError>`
checks the freshest locally trusted, unexpired authorization and the verified clock floor,
without fetching over the network. L4 calls it again immediately before a new activation.

- [ ] Add `catalog_rejects_wrong_arch_floor_version_reuse_and_expiry` using signed local
  fixtures: a Stable valid `0.1.2` is selected over installed `0.1.1`; old/expired metadata,
  unsupported floor/arch, Testing-only and same-version different digest each fail closed
  or produce no eligible target. Assert a trusted-root rotation works without resetting
  the persisted metadata history. Assert an authorized offline apply works before
  expiry and fails if expired, superseded or the clock predates the persisted floor.
- [ ] Add `stage_keeps_current_on_truncation_limit_or_bad_digest`: assert no extraction for
  an incomplete/corrupt artifact, no file outside staging for invalid archive entries, no
  promotion under ENOSPC, and no rewrite of a verified complete cache hit.
- [ ] Run `docker compose run --rm dev cargo test --locked -p zerodeck-updater --test catalog --test stage`; expect failures until the contracts exist.
- [ ] Implement pinned-root `tough` loading with a persistent datastore, bounded transport,
  digest-aware cache and streaming archive writes. Verify before parsing or extraction;
  accept only expected relative paths and regular files/directories, reject duplicate
  entries and links, and enforce both expanded byte and entry limits. Revalidate cached
  candidate identity and authorization before requesting apply; local rollback does not use
  the network or change trusted metadata.
- [ ] Rerun the focused command and `just deny`; expect all tests and dependency checks to
  pass. Commit `feat(updater): stage TUF-authorized loader releases`.

### Task L3: One running generation and meaningful readiness

**Files:** `crates/zerodeckd/src/{main,install}.rs`, `crates/zerodeckd/tests/daemon.rs`,
`crates/zerodeck-server/src/{state.rs,routes/mod.rs}`, `crates/zerodeck-updater/src/service.rs`,
`crates/zerodeck-updater/tests/service.rs`.

**Interfaces:** internal daemon flag `--managed-generation <path>` resolves a concrete
root and binds `bin/zerodeckd`, `python/` and `frontend/` to it. Add local,
token-protected `GET /readyz` reporting `{version, build_id, python_host_ready}` without
internal paths; preserve `/healthz`. `async fn verify_ready(service: &SystemdService, expected: &ReleaseIdentity, deadline: Duration) -> Result<(), UpdateError>`
requires a new service invocation, matching version/build, expected frontend asset,
Python import and authenticated `utilities/ping`.

- [ ] Add `managed_daemon_uses_one_generation` with stale legacy assets available: all
  files and version response must come from the selected generation even after
  `current` changes. Add `readiness_rejects_old_invocation_and_wrong_build` with mock
  service and RPC responders that return `200` but wrong identity.
- [ ] Run `docker compose run --rm dev cargo test --locked -p zerodeckd --test daemon managed_daemon_uses_one_generation`, then the same prefix with `-p zerodeck-updater --test service`; expect failure on the old readiness behavior.
- [ ] Resolve paths once; for managed operation reject overrides selecting a different
  generation. Use `sd-notify` after the listener is ready. Implement the authenticated
  readiness checks with a 60-second activation deadline. A broken unrelated plugin
  enters its own failed state; it must not block readiness indefinitely or cause the
  loader health probe to fail by itself.
- [ ] Rerun both focused suites; verify the normal daemon tests still pass. Commit
  `feat(updater): verify managed daemon build and readiness`.

### Task L4: Managed apply, boot recovery, and CLI

**Files:** `crates/zerodeck-updater/src/{main,state,service}.rs`,
`crates/zerodeck-updater/tests/activation.rs`, `deploy/systemd/zerodeck.service`,
create `deploy/systemd/{zerodeck-apply,zerodeck-recovery,zerodeck-check}.service` and
`deploy/systemd/zerodeck-check.timer`.

**Interfaces:** `async fn apply_pending(store: &GenerationStore, state: &StateStore, catalog: &Catalog, service: &SystemdService) -> Result<(), UpdateError>`;
`async fn recover(store: &GenerationStore, state: &StateStore, service: &SystemdService, mode: RecoveryMode) -> Result<(), UpdateError>`,
with `RecoveryMode::{BeforeBootStart, AfterApply}`. `sudo zerodeckctl update` stages,
`sudo zerodeckctl upgrade` enqueues the prepared candidate in a root-owned single request
slot and asks systemd to run apply; `sudo zerodeckctl rollback` enqueues the last healthy
retained target through the same unit. `zerodeckctl status` reads a separate redacted
summary; `update --check-only` is optional timer behavior and does not activate.
Declare the native service tool as binary `zerodeckctl` in the updater crate manifest.

- [ ] Add `activation_recovers_after_every_interrupted_transition`: faults before/after persistent
  intent, service stop, switch, restart, readiness and confirmation. Assert old identity
  on pre-commit interruptions, new identity after a durable successful commit, no network
  during recovery, and a recoverable stopped state if there is no good generation.
- [ ] Add `activation_does_not_block_boot_recovery_or_interleave_with_plugins`: take a
  shared maintenance guard and expect `Busy`; after it releases, assert apply owns the
  exclusive guard, boot helper only changes disk state, and post-stop recovery can start
  the restored daemon after the failed process releases its guard. Assert an ordinary
  upgrade leaves an intentionally stopped service stopped and the candidate staged.
- [ ] Run `docker compose run --rm dev cargo test --locked -p zerodeck-updater --test activation`; expect red before wiring the orchestrator.
- [ ] Implement root-only enqueue and the once-per-boot recovery unit ordered before the
  daemon. Its execution is distinct from the apply unit's `ExecStopPost`; apply restarts
  must not re-enter boot recovery while holding the lock. Recheck `authorize_staged`
  under the operation lock before stopping for a new release; explicit restoration
  of `rollback_target` does not require it. Record whether the service was running,
  honor operator-stopped service state, and quarantine a failed digest. Verify enabled
  timers only check and recovery never touches plugin data.
- [ ] Rerun the focused suite and `just check`; expect PASS. Commit
  `feat(updater): activate and recover zero deck with systemd`.

### Task L5: Build, publish, and bootstrap the trust chain

**Files:** `.github/workflows/release-please.yml`, `release-please-config.json`,
`Dockerfile`, `justfile`, `deploy/update/{bootstrap,tests/release.sh,RELEASE.md}`.

**Interfaces:** `just dist <tag> <image>` still produces the `.tar.gz` and checksums;
the managed archive adds `release.json` and separately packages the controller/bootstrap
files for the supported target. `just test-release-package <archive>` checks the exact
bytes passed to publication. The runbook documents owner and recovery process for each
TUF signing role, publishing order, first trust bootstrap, and updater version upgrades.
The verified administrative bootstrap exposes `--convert-existing-zerodeck` and
`--import-inactive-decky <path>` as explicit alternatives to clean installation.

- [ ] Write the packaging test for missing/mixed components, build ID mismatch, missing
  asset and a failed upload/retry. The published Stable catalog must resolve to the
  previous known digest until the new package and attestations are present and authorized.
- [ ] Add bootstrap scenarios for failed conversion, controller replacement during a
  pending activation, and a running Decky install. Assert the original ZeroDeck unit,
  overrides and data remain recoverable; assert Decky's service and files are unchanged
  when bootstrap refuses the conflict.
- [ ] Run `just dist ci-updater` against the locally built runtime and run
  `bash deploy/update/tests/release.sh <archive>` with the printed path; the managed
  layout assertions fail before the packaging change.
- [ ] Keep version selection with Release Please. Prepare drafts, build from the exact
  commit, package by OCI digest (not a mutable tag), verify assets/provenance, then
  publish. Make upload retries target the same draft without overwriting a released
  digest. Configure a separate TUF-on-CI promotion for already published bytes with no
  release-signing key in the build job. Bootstrap with externally verified trust
  material; never run a downloaded privileged installer before verification. Preserve
  the existing unprivileged data root during conversion, copy code into the managed
  root only when ownership/trust checks permit, and keep the old units until the
  managed generation is confirmed. Import an inactive Decky tree by copy; mark
  imported root plugins blocked until explicit verified installation.
- [ ] Rerun the packaging test and `just ci`; record a staging release, retry, expiry,
  and rotation drill. Commit `build(release): publish and authorize complete immutable releases`.

### Task L6: VM and Steam Deck release gate

**Files:** `deploy/update/tests/{vm.sh,README.md}`, `justfile`, `README.md`.

**Interfaces:** `just test-updater-vm <image>` runs a disposable Linux systemd VM
against locally served fixtures and returns failure if service identity or disk recovery
is incorrect. The Deck checklist records versions, filesystems, invocation identity,
durations, memory, bytes and peak temporary storage.

- [ ] Write VM cases for hard power-off during apply, `SIGKILL` of the independent apply
  helper, an unusable candidate, an unhealthy rollback copy, a port collision and a
  reboot during recovery; validate the actual running binary and HTTP/RPC replies.
- [ ] Exercise the harness against the pre-change image; expect failure on interrupted
  activation. Wire the public recipe to its private implementation and use it from CI.
- [ ] Rerun the VM cases and `just ci`. On a real Deck, demonstrate a full install,
  upgrade, rollback offline, unchanged plugin inventory and persistence across a
  SteamOS update with a read-only root. Compare gzip and Zstandard on that Linux payload
  before adopting a new format. Block Stable when the hardware evidence is absent.
- [ ] Document CLI syntax, errors, rollback workflow and limits. Commit
  `test(updater): gate native release on VM and Steam Deck recovery`.
