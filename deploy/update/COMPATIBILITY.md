# ZeroDeck loader compatibility and release evidence

Copy this template to `deploy/update/evidence/<tag>.md` (for example `v0.1.0.md`) for each release
candidate and fill every field from observed hardware. **Never mark Stable eligible from an unrecorded run.**
Every field below starts as **PENDING**; replace it only with an observed value or `PASS`/`FAIL`
plus the evidence reference (log, screenshot, command output or trace).

**Current status: Deck evidence is PENDING. No Steam Deck or real SteamOS image run is recorded in
this repository.** The VM harness is documented in [`tests/README.md`](tests/README.md) and its
fake-qemu/fake-SSH self-test runs in `just check`; a real image run is still PENDING.

## Release identity

| Field | Value |
|---|---|
| Release tag | PENDING |
| Loader version / build ID | PENDING |
| Manual asset SHA-256 | PENDING |
| Managed asset SHA-256 (generation identity) | PENDING |
| Signed TUF targets version | PENDING |
| TUF root version / out-of-band fingerprint | PENDING |
| Runtime image digest | PENDING |
| Source commit | PENDING |
| Release workflow run URL | PENDING |
| Promotion request / TUF signing run URL | PENDING |

## Host, Steam and invocation identity

| Field | Value |
|---|---|
| Device model | PENDING (Steam Deck LCD / OLED / other) |
| SteamOS build | PENDING |
| Steam client version and build | PENDING |
| Kernel (`uname -r`) | PENDING |
| SteamOS update channel | PENDING (stable / beta / preview) |
| Invocation identity | PENDING (user, uid/gid, `sudo`/root shell/SSH, terminal or Desktop Mode) |
| Install root | PENDING (`/home/.zerodeck`, controller, units, config) |
| Homebrew/data root | PENDING (`/home/deck/homebrew`) |
| Existing Decky state | PENDING (none / inactive import / converted) |
| Existing ZeroDeck state | PENDING (clean / converted) |
| SELinux/immutable/overlay state | PENDING |
| Clock/timezone and NTP state | PENDING |

Do not infer the invocation identity from the desktop session. Record the exact command, the
effective uid/gid, whether `sudo` was used, and whether it ran from Game Mode, Desktop Mode or SSH.

## Filesystems and mounts

| Mount | Filesystem | Options | Read-only? | Free before / after |
|---|---|---|---|---|
| `/` (rootfs) | PENDING | PENDING | PENDING | PENDING |
| `/home` | PENDING | PENDING | PENDING | PENDING |
| `/var` | PENDING | PENDING | PENDING | PENDING |
| `/home/.zerodeck` | PENDING | PENDING | PENDING | PENDING |
| `/home/deck/homebrew` | PENDING | PENDING | PENDING | PENDING |
| Temporary directory used by staging | PENDING | PENDING | PENDING | PENDING |

Record `findmnt`/`mount` output and confirm whether the root filesystem is read-only. Bootstrap
must not change rootfs writability. A read-only root is a pass only when the managed install,
update and rollback all still work through the documented writable locations.

## Lifecycle results

| Scenario | Result | Observed active digest / identity | Duration | Evidence |
|---|---|---|---|---|
| Clean bootstrap without `--start` | PENDING | PENDING | PENDING | PENDING |
| Clean bootstrap with `--start` | PENDING | PENDING | PENDING | PENDING |
| Update via `zerodeckctl update` + `upgrade` | PENDING | PENDING | PENDING | PENDING |
| Offline rollback (network unavailable) | PENDING | PENDING | PENDING | PENDING |
| Reboot during apply | PENDING | PENDING | PENDING | PENDING |
| `SIGKILL` of apply helper | PENDING | PENDING | PENDING | PENDING |
| Failed/unhealthy candidate recovery | PENDING | PENDING | PENDING | PENDING |
| Missing or corrupt rollback generation | PENDING | PENDING | PENDING | PENDING |
| Port collision on 1337 | PENDING | PENDING | PENDING | PENDING |
| Second recovery pass is idempotent | PENDING | PENDING | PENDING | PENDING |
| SteamOS update survival (read-only root) | PENDING | PENDING | PENDING | PENDING |
| Loader-only upgrade leaves plugin inventory unchanged | PENDING | PENDING | PENDING | PENDING |
| Conversion from existing ZeroDeck | PENDING | PENDING | PENDING | PENDING |
| Import inactive Decky tree | PENDING | PENDING | PENDING | PENDING |
| Running Decky conflict refusal | PENDING | PENDING | PENDING | PENDING |

For each lifecycle row record the actual `/home/.zerodeck/current` digest, the `zerodeckd`
executable resolved from the unit's `MainPID`, `/readyz` version/build ID, and the service state
before and after. A rollback is offline only if the network was disabled or unavailable and the
operation still used the retained generation.

## Plugin inventory and compatibility

Record the inventory before and after a loader-only upgrade. The installed plugin set,
`pluginOrder`, enabled/disabled state and managed paths must be unchanged.

| Plugin (id) | Source/channel | Manifest version | Backend result | Game Mode frontend result | Data preserved | Notes |
|---|---|---|---|---|---|---|
| PENDING | PENDING | PENDING | PENDING | PENDING | PENDING | PENDING |

Report package/backend/frontend results separately. A loader-only upgrade is not evidence that a
plugin's Game Mode frontend loaded; test it explicitly.

## Resource measurements

| Metric | Idle | Update (fetch/stage) | Apply | Rollback |
|---|---|---|---|---|
| Peak daemon RSS (MiB) | PENDING | PENDING | PENDING | PENDING |
| Peak temporary disk (MiB) | PENDING | PENDING | PENDING | PENDING |
| Bytes downloaded | PENDING | PENDING | PENDING | PENDING |
| Stop-to-ready (s) | PENDING | PENDING | PENDING | PENDING |
| Stop-to-recovered (s) | PENDING | PENDING | PENDING | PENDING |

Record the measurement method (for example `systemd-cgtop`, `/proc/<pid>/status` `VmHWM`,
`du` peak, `df` before/after, and timestamps from `systemctl show`/journal). Do not substitute
guessed targets for observations.

## VM harness evidence

| Field | Value |
|---|---|
| Harness self-test (`just test-updater-vm-selftest`) | PASS — fake qemu/ssh, no real image |
| Real image path / digest | PENDING |
| QEMU version, accelerator (TCG/KVM/HVF), machine/CPU | PENDING |
| VM scenarios result | PENDING |
| VM stop-to-ready / stop-to-recovered | PENDING |
| VM source image unchanged | PENDING |
| VM evidence directory | PENDING |

The self-test only proves command construction, checksum refusal, failure propagation and cleanup.
It is **not** a substitute for a real image run or Steam Deck hardware evidence.

## Compression comparison before changing the release format

Compare gzip and Zstandard on the actual Linux payload (same source commit, same build identity)
before adopting a new archive compression format.

| Metric | gzip | Zstandard |
|---|---|---|
| Managed archive bytes | PENDING | PENDING |
| Decompression time (s) | PENDING | PENDING |
| Peak decompression RSS (MiB) | PENDING | PENDING |
| Staging bytes written | PENDING | PENDING |
| Compatibility/verification result | PENDING | PENDING |

## Problems and disposition

| Severity | Description | Evidence | Owner | Resolution |
|---|---|---|---|---|
| PENDING | PENDING | PENDING | PENDING | PENDING |

## Sign-off

- Operator: PENDING
- Independent reviewer: PENDING
- Date: PENDING
- Stable eligible: PENDING

Reason: hardware evidence not recorded. Replace the exact line above with
`Stable eligible: PASS` only after every value in this file is observed; the promotion workflow
refuses any remaining `PENDING` value.
