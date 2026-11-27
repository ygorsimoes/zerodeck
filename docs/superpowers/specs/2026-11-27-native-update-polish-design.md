# Native Update Polish Design (PR #23 follow-up)

## Intent and why

PR #23 (`feat/native-loader-update`) delivers the native ZeroDeck updater: TUF-authorized
staging, crash-consistent generation switching, systemd apply/recovery, draft-first releases and a
VM gate. A full review found that it cannot ship as written: the release pipeline fails on its
default path, two updater bugs can make updates impossible or keep the daemon down, the install
layout likely does not survive a SteamOS update, the VM gate never runs, and roughly 3,000 lines
are duplicated, dead or test-only plumbing. The owner wants the same feature, correct, smaller,
idiomatic, fully dockerized and well tested, with a clear story for testing on SteamOS-like and
real SteamOS images. No hacks.

This design amends [the update design](2026-11-26-update-and-upgrade-design.md) for the loader
parts only. Everything there not contradicted here stays in force.

## Goals

- Every bug listed under "Defects to fix" is fixed and has a regression test.
- The install survives a SteamOS A/B update, by construction and by a VM scenario that models it.
- Bootstrap logic lives in Rust (`zerodeckctl install`); shell is limited to thin, shellchecked
  wrappers.
- Every recipe that needs tools beyond `docker` and `just` runs in a pinned container; CI calls
  only public `just` recipes; no inline programs in workflows.
- A reproducible Arch-based "SteamOS-like" VM gate runs for real in CI (KVM) and locally (TCG).
  The same harness also accepts a VM image installed from Valve's official recovery image.
- The release pipeline can produce, verify, publish and request promotion of a release without
  manual patching.
- The updater crate shrinks and is organized by responsibility; tests share support code and do
  not depend on wall-clock timing.

## Non-goals

- Decky plugin updates (separate plan `2026-11-26-decky-plugin-update.md`).
- Creating the external TUF-on-CI repository, its keys, GitHub environments or secrets: those need
  the owner's accounts and are documented as owner actions.
- Recording real Steam Deck evidence or promoting anything to Stable.
- Redistributing any SteamOS image.

## Anti-goals

- A test that is green while it skipped. A skip is reported as a skip and fails the job that is
  supposed to run it.
- A container that needs more privilege than AGENTS.md allows (no systemd-in-Docker).
- Keeping a feature only because it already exists in the PR when nothing can use it.

## Constraints

AGENTS.md applies in full (MIT clean room, crate graph, lints, security invariants, Docker-first
workflow, pinning, `just ci` before finishing). GPL projects (`decky-loader`, the beallio fork)
inspire ideas only; no code is copied. ZeroDeck has never published a release (no tags exist), so
there is no installed base to migrate.

## Approaches

**SteamOS image strategy (owner picked "Arch VM + real SteamOS").**

1. *Arch VM + real SteamOS (chosen).* CI and local runs boot a pinned Arch Linux cloud image
   (SteamOS 3 is Arch-based) inside a `vm` container with QEMU. Cloud-init provisions a
   SteamOS-like profile. Separately, a documented, semi-manual path turns Valve's recovery image
   into a qcow2 that the same harness drives. Trade-off: two images to describe, but CI gets a
   real gate and owners can rehearse on real SteamOS before hardware.
2. *Arch VM only.* Simpler, but nothing runs on actual SteamOS before the Deck.
3. *systemd-in-Docker.* Rejected: needs `CAP_SYS_ADMIN`/privileged and a writable cgroup tree
   (violates AGENTS.md), shares the host `boot_id` (the boot marker cannot be tested), and cannot
   power-cycle.

**Bootstrap.** Moving bootstrap into Rust was chosen over slimming the 1,019-line `bootstrap.sh`:
the controller already owns staging, `atomic_write`, readiness and systemd control, so the shell
version duplicates them with weaker guarantees (a fake root signed `"sig": "00"` passes its check;
`set -e` is silently off in its install functions).

## Decisions

### 1. Host layout that survives SteamOS updates

SteamOS keeps `/usr` read-only and replaced on update, keeps one `/var` per A/B slot (copied only
selectively), and keeps only allow-listed `/etc` files (SteamOS ≥ 3.6,
`/etc/atomic-update.conf.d/*.conf`, one path or glob per line). `/home` persists.

- Update root moves from `/var/lib/zerodeck` to **`/home/.zerodeck`** (root-owned `0755`, a
  sibling of `/home/deck`; its ancestors `/` and `/home` are root-owned, so
  `check_secure_ancestors` still holds). It contains `bin/zerodeckctl`, `current`, `generations/`,
  `state/`, `tuf/`, `config.json` and the maintenance lock.
- The controller moves from `/usr/local/lib/zerodeck/bin/zerodeckctl` to
  `/home/.zerodeck/bin/zerodeckctl`. No PATH link is installed (`/usr/local/bin` is read-only);
  docs use the full path.
- Units stay in `/etc/systemd/system` as regular files. `install` writes
  `/etc/atomic-update.conf.d/zerodeck.conf` listing the five unit files
  (`zerodeck{,-apply,-recovery,-check}.service`, `zerodeck-check.timer`), the enablement symlinks
  (`/etc/systemd/system/*.wants/zerodeck*`) and that file itself. Without the symlinks the units
  survive but come back disabled.
- On SteamOS (`/etc/os-release` `ID=steamos`) `install` refuses when `/etc/atomic-update.conf.d`
  is absent (pre-3.6). On other systemd distros the keep-list is skipped.
- The unprivileged data path (`/home/deck/homebrew`) is unchanged.
- `DEFAULT_ROOT`, unit `ExecStart=` paths, docs and tests change together. Unit files remain the
  single source in `deploy/systemd/` and are embedded in `zerodeckctl` with `include_str!`.

### 2. Configuration loaded by the controller

`sudo zerodeckctl update` must work without environment variables (sudo strips them).
`install` writes `<root>/config.json` (schema 1: `metadata_url`, `targets_url`, `channel`)
atomically; every command reads it. `ZERODECK_UPDATE_*` environment variables and
`/etc/zerodeck/update.env` are removed. URLs may be `http` or `https`: TUF signatures, not the
transport, are the trust boundary, and the file is root-owned. `--channel` on `update`/`upgrade`
defaults to the configured channel; selecting Testing still requires the explicit flag or config.

### 3. `zerodeckctl install` and `uninstall` replace `bootstrap.sh`

```
sudo ./zerodeckctl install --archive <managed.tar.gz> --sha256 <hex> \
     --trusted-root <root.json> --trusted-root-sha256 <hex> \
     --metadata-url <url> --targets-url <url> [--start]
sudo /home/.zerodeck/bin/zerodeckctl uninstall [--keep-generations]
```

- `install` verifies both digests (supplied out of band), loads the root with `tough` (a real
  signature check), stages the archive with the same extractor and limits as `update`, copies
  itself to `<root>/bin/zerodeckctl` (`0755`, root-owned), writes config, units and keep-list
  atomically, `daemon-reload`s, enables `zerodeck-recovery.service` and `zerodeck.service`, and
  with `--start` starts and confirms readiness exactly like an activation.
- Refusals before any mutation: not root; running or installed Decky (`plugin_loader.service`,
  `decky-loader.service`, or port 1337 bound by another process); a legacy unmanaged
  `zerodeck.service` whose `ExecStart` is not the managed controller; SteamOS without keep-list
  support; an existing install with a different generation (use `update`/`upgrade` instead).
- Re-running with the same archive digest is a no-op success (repeatable). A failure before
  confirmation rolls back everything `install` created, from an in-memory list of created paths,
  and leaves the system as it found it.
- `uninstall` stops and disables the units, removes units, keep-list and the update root, and never
  touches `/home/deck/homebrew`.
- **Removed:** `--convert-existing-zerodeck` and `--import-inactive-decky` (no installed base
  exists; `import.json` has no reader). The bootstrap `--check/--prepare/--confirm` subcommand is
  replaced by `install`, which has no dry-run mode (defect 11 is closed by removing it).
- **Controller upgrades** *(builder-check call)*: the generation archive also carries
  `bin/zerodeckctl`. After a generation is confirmed, the apply step replaces
  `<root>/bin/zerodeckctl` with the generation's copy by atomic rename when that copy's version is
  newer, never during a pending transaction, and never downgrades on rollback. TUF `min_updater`
  stays: selection refuses a target that needs a newer controller than the running one, so a
  release that raises `min_updater` must follow a release that ships that controller.
- `deploy/update/install.sh` (≤ 80 lines, shellcheck clean) is the optional convenience
  wrapper: given a tag and the expected `SHA256SUMS.txt` digest, it downloads the release assets
  into a `mktemp -d`, verifies them, and execs `zerodeckctl install`. No embedded Python.

### 4. Release assets

- `zerodeck-<tag>-linux-x86_64.tar.gz`: the managed generation (`release.json`, `bin/zerodeckd`,
  `bin/zerodeckctl`, `python/`, `frontend/`), the TUF target. The `.managed` suffix and the separate manual tarball
  with a duplicated payload are dropped.
- `zerodeckctl-<tag>-linux-x86_64`: the controller binary.
- `install.sh`, `SHA256SUMS.txt`.
- Assets are produced by a Dockerfile `dist` stage (`FROM scratch`, exported with
  `docker buildx build --target dist --output type=local,dest=dist`). A `package` stage writes the
  archive deterministically (`tar --sort=name --mtime=@${SOURCE_DATE_EPOCH} --owner=0 --group=0
  --numeric-owner | gzip -n`) and `release.json` from the same `ZERODECK_BUILD_ID` ARG the binary
  was compiled with. `package-release.sh`, `archive.py`, `docker create/cp` and host `python3` go
  away. `zerodeckctl` leaves the runtime image.
- `zerodeckctl verify-archive <file>` (hidden subcommand) validates an archive with the production
  extractor and identity checks; `just test-release-package` calls it. Mutation cases move into
  Rust tests.

### 5. Updater fixes and simplification

Defects to fix, each with a failing test first:

1. Download uses a total 120 s timeout → custom `tough::Transport` over reqwest with connect and
   per-read (idle) timeouts plus the size cap, used for metadata and the target archive. Targets
   are fetched through `tough` (`read_target`), which verifies length and hash while streaming;
   the separate digest/download code and the `zerodeck-core` dependency go away (the updater then
   depends on `zerodeck-platform` only; the extract limits move into the updater).
2. Boot recovery returns `StaleRequest` for `Confirmed` with a mismatched `current`, so the daemon
   never starts → fall through to revert/reconcile.
3. GC deletes generations in place → rename to `.trash-<uuid>`, sync, delete; sweep `.trash-*` and
   `.staging-*`; a cached generation is accepted only after full `verify_generation`.
4. Quarantine: GC keeps quarantined digests until `unquarantine`, which only edits the list;
   selection skips quarantined digests and falls back to the next eligible release.
5. The check timer holds the exclusive lock during network fetches → catalog refresh and download
   run under a separate catalog lock and the shared maintenance guard; only the commit step takes
   the exclusive guard. `apply` waits for the lock within one derived budget.
6. Timeouts: one budget derived from shared constants (stop + start + readiness for candidate and
   for revert). The unit files stay static; a Rust test parses the embedded
   `zerodeck-apply.service` and fails unless its `TimeoutStartSec` covers the budget. The client
   drops its own timeout and poll loop: `systemctl start --wait` returns after `ExecStopPost`.
7. `BootRecoveryMarker` fails closed on Linux when `boot_id` cannot be read.
8. The apply request gets mode `0600` on the temp file before the rename.
9. `zerodeckd --check-config` requires an importable Python host only with `--managed-generation`.
10. `staged_generations` propagates catalog read errors instead of letting GC delete the candidate.
11. `install --check`-style dry runs do not create directories.

Simplification (behavior-preserving):

- One `revert` path for apply failure, boot recovery and post-apply recovery;
  `enum Recovery<'a> { Boot, AfterApply(&'a SystemdService) }` replaces `RecoveryMode` plus
  `Option`.
- One generic `SchemaFile<T>` for state, catalog state, boot marker and apply request.
- One release-manifest/layout verifier shared by the updater and `zerodeckd` (in
  `zerodeck-platform`, no new dependencies); one `ReleaseIdentity` type; one build-id helper.
- One bounded GET helper in readiness. `begin_activation()` returns the invocation baseline instead
  of storing it in `Arc<Mutex<Option<String>>>`.
- Remove single-caller wrappers (`*_with` variants, `enqueue_upgrade_with_channel`,
  `bootstrap_generation`), unused API, default "unsupported" trait methods, dead `cfg(not(unix))`
  code (the library becomes `#![cfg(unix)]`, which it already needs for `MaintenanceLock`, so
  native macOS builds keep working), `channel_name`/`phase_name` helpers
  (use `Display`/`clap::ValueEnum`), the bootstrap bool flags, `redact_status_text` (redact when
  writing `last_error`).
- `zerodeckctl status --json` for machines (the VM harness), human output by default.
- Module layout: `config`, `systemd` (trait + real impl), `readiness`, `catalog`, `stage`,
  `generation`, `state` (generic store, request, boot marker), `apply` (enqueue/apply/revert),
  `install`, `error`. `#[expect(.., reason)]` instead of `#[allow]`.

### 6. Tests

- `tests/support/mod.rs`: `TestDir`, generation fixture builder, signed TUF fixture repo (moved
  from `tests/catalog.rs`), a `Harness` builder and one ordered event log for the mock systemctl.
- `tests/activation.rs` splits into `apply.rs` (library faults), `cli.rs` (through
  `CARGO_BIN_EXE_zerodeckctl`) and `install.rs`. The unit-file grep test becomes part of
  `lint-infra`.
- No wall-clock assertions: count attempts or use explicit notification.
- New cases for every defect above, plus interrupted GC, leftover staging, quarantined newest
  target, apply busy while update holds the catalog lock, install refusal matrix, install rollback
  and idempotence, uninstall preserving homebrew.
- Root-only install paths run in `privileged-test` (`privileged_*`).
- The shell tests for bootstrap and packaging, `fixtures.sh` (a bash re-implementation of the
  controller) and `vm.sh --self-test` are deleted; what they covered moves to Rust tests or the
  real VM gate.
- `publish-release.sh`, `promote.sh` and `verify-stable-evidence.sh` stay shell *(builder-check
  call)*: they orchestrate `gh` against GitHub and have no controller logic. They are slimmed,
  shellchecked and keep focused tests (`publish.sh`, `promotion.sh`, `stable-gate.sh`) that must
  also exercise the default `PATH` lookup of `gh`/`curl`.

### 7. VM gate (dockerized)

- Dockerfile `vm` stage: Debian bookworm with pinned `qemu-system-x86`, `qemu-utils`, `ovmf`,
  `openssh-client`, `cloud-image-utils`, `python3`; runs as an unprivileged user. Recipes use
  `docker run` (like `py-test-on`), adding `--device /dev/kvm --group-add <gid>` only when the host
  has a writable `/dev/kvm`; otherwise TCG.
- Harness rewritten as a stdlib Python package `deploy/vm/zerodeck_vm` (ruff + strict pyright,
  same rules as `hosts/python`), replacing the 1,870-line `vm.sh`.
- `just vm-image` downloads the Arch cloud image pinned by dated URL and SHA-256 (Dockerfile/justfile
  ARGs) into a cache volume, verifying fail closed. Arch mirrors keep dated images about three
  months; bumping the pin is a documented one-line change, and CI caches the image by digest.
- Per run: fresh qcow2 overlay, ephemeral SSH key, NoCloud seed that creates `deck` (uid 1000,
  NOPASSWD sudo for the harness only), `/home/deck/homebrew`, Python 3, and the **SteamOS-like
  profile**: `/usr` bind-mounted read-only via fstab, `/etc/os-release` marked `ID=steamos`
  in a test overlay, and `/etc/atomic-update.conf.d/` present.
- `zerodeck-vm-fixture` (a `zerodeck-updater` example built in the dev container) generates a real
  signed TUF repository for the two archives; the container serves it on the QEMU user network
  (`10.0.2.2`). `update` and `upgrade` therefore run end to end.
- Scenarios: install without/with `--start`; port collision; update + upgrade to a genuinely
  different build (the fixture builds a second generation with a distinct build ID); offline
  rollback (network detached); unhealthy candidate; corrupt rollback target; hard power-off during
  apply (QEMU killed); `SIGKILL` of the apply helper; reboot during recovery; second recovery pass;
  **simulated SteamOS update** (reset `/etc` to the provisioned snapshot plus files matched by the
  keep-list, wipe `/var` back to its provisioned snapshot, reboot) after which the daemon must come
  back healthy on the same generation; uninstall.
- Scenario sets are per profile and always printed. The `steamos-like` profile runs the simulated
  SteamOS update. The `native` profile (real SteamOS) does not simulate it; it runs a real
  `steamos-update` only with `--with-os-update` (needs network), and otherwise reports that the
  scenario is outside the selected set *(builder-check call)*.
- The guest is always x86_64. The `vm` stage builds for the host architecture (Debian ships
  `qemu-system-x86` for arm64), so Apple Silicon runs plain TCG without emulating QEMU itself;
  release binaries for the VM are built `--platform linux/amd64`. `test-vm` is not part of
  `just ci` (minutes under TCG); it is the `updater-vm` CI job, run locally on demand
  *(builder-check call)*.
- Exit status: 0 pass, 1 fail, 77 skip. The CI `updater-vm` job (non-required, KVM) treats 77 as
  failure. `_test-updater-vm-ci` is removed.
- Real SteamOS path: `just steamos-vm-install <steamdeck-recovery.img.bz2>` (image downloaded by
  the owner from Valve; never committed or redistributed) boots the recovery image with OVMF and
  a VNC display bound to `127.0.0.1` so the owner performs the interactive re-image into a qcow2;
  `deploy/vm/README.md` lists the in-guest steps (set the `deck` password, enable `sshd`, add the
  harness key and a NOPASSWD drop-in). `just test-vm --image <qcow2> --uefi --profile native` then
  runs the same scenarios without the SteamOS-like overrides.

### 8. Docker and CI

- Dockerfile: cargo-chef SHA-256 pinned via ARGs; base images pinned by digest; one
  `ARG PNPM_VERSION`; `ZERODECK_BUILD_ID` declared after `cargo chef cook` (no dependency
  rebuilds per CI run); `--locked` on cargo build/cook; `shellcheck` pinned in the toolchain;
  `zerodeckctl` not in `runtime`; new `package`/`dist` and `vm` stages.
- `lint-infra` adds shellcheck for `deploy/**/*.sh` and ruff/pyright for `deploy/vm`.
- Every workflow step is one `just` call. New public recipes (each forwarding to a private
  `_recipe`): `release-assets`, `release-verify`, `release-publish`, `promote-request`,
  `check-release`, `vm-image`, `test-vm`, `steamos-vm-install`.
- `ci.yml`: workflow `permissions: {}`, `contents: read` per job; the release-script/Rust install
  checks run via `just check-release` inside an existing job; `updater-vm` runs the real gate with
  KVM and caches the Arch image. Required job ids `rust`, `deny`, `python`, `frontend`, `docker`
  are unchanged.
- `release-please.yml`: drop workflow-level concurrency (keep the job group); publish job uses
  `environment: release` and requires `RELEASE_PLEASE_TOKEN` (no `GITHUB_TOKEN` fallback); image
  pushed by digest and tagged (`X.Y.Z`, `X.Y`, `latest`) only after the release is published; one
  provenance mechanism; cache `scope=runtime` read-only; one verification pass after upload; the
  commit check fails closed; `gh` and `curl` resolved through `PATH`; the token never on argv.
- `promote-tuf.yml`: `if: github.ref == 'refs/heads/main'` plus a documented environment branch
  policy; the dispatch nests the request under one `client_payload.request` key, whose schema is
  the contract for the external receiver *(builder-check call)*: `schema` (1), `tag`, `version`,
  `channel`, `build_id`, `target_path`, `sha256`, `length`, `asset_url`, `source_commit`,
  `evidence_sha256`, `run_url`. `just promote-request --dry-run` renders and validates it without
  dispatching, so CI tests it end to end on a packaged fixture; the
  build-ID self-comparison and unused `loader-target.template.json` are removed;
  `verify-stable-evidence.sh` uses exact tag/version equality.
- `CODEOWNERS` covers `deploy/update/evidence/`, `.github/workflows/` and `deploy/`.

### 9. Docs

- `deploy/update/COMPATIBILITY.md` becomes `deploy/update/evidence/TEMPLATE.md` and adds rows for
  the controller location and keep-list survival.
- `RELEASE.md` is cut to assets, publication order, promotion, owner actions and TUF role recovery;
  it no longer retells the installer.
- `deploy/update/tests/README.md` is replaced by `deploy/vm/README.md`.
- `README.md` On Steam Deck section updated; AGENTS.md crate graph adds
  `zerodeck-updater ── zerodeck-platform, zerodeck-protocol` and documents the new recipes.

## Owner actions (cannot be done from the repository)

- Create the TUF-on-CI repository, generate root/targets keys, add the `repository_dispatch`
  receiver workflow there.
- Create GitHub environments `release` and `production` (deployment branch policy `main`, required
  reviewers), the `RELEASE_PLEASE_TOKEN` secret with admin read for immutable releases, and enable
  immutable releases.
- Run the real SteamOS VM path and the Deck checklist.

## Left to the builder

Exact module boundaries inside the updater, test names, the Arch image date to pin, QEMU
memory/CPU defaults, the deterministic tar invocation details, and how the harness structures
scenarios internally, as long as the behavior above holds.
