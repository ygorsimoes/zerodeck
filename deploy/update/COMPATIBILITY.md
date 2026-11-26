# ZeroDeck loader compatibility and release evidence

Copy this template to `deploy/update/evidence/<date>-<version>.md` for each release candidate and
fill every field from observed hardware. Do not mark Stable eligible from an unrecorded run.

## Release identity

| Field | Value |
|---|---|
| Release tag | |
| Loader version / build ID | |
| Manual asset SHA-256 | |
| Managed asset SHA-256 (generation identity) | |
| Signed TUF targets version | |
| TUF root version / out-of-band fingerprint | |
| Runtime image digest | |
| Source commit | |
| Release workflow run URL | |
| Promotion request / TUF signing run URL | |

## Host

| Field | Value |
|---|---|
| Device model | Steam Deck LCD / OLED / other |
| SteamOS build | |
| Steam client version | |
| Kernel | |
| Root filesystem read-only | yes / no |
| Install root filesystem | |
| Free space before / after | |
| Existing Decky state | none / inactive import / converted |
| Existing ZeroDeck state | clean / converted |

## Lifecycle results

| Scenario | Result | Observed identity | Duration | Notes |
|---|---|---|---|---|
| Clean bootstrap without `--start` | | | | |
| Clean bootstrap with `--start` | | | | |
| Upgrade via `zerodeckctl update` + `upgrade` | | | | |
| Offline rollback | | | | |
| Reboot during apply | | | | |
| `SIGKILL` of apply helper | | | | |
| Failed candidate recovery | | | | |
| Unusable rollback copy | | | | |
| SteamOS update survival (read-only root) | | | | |
| Conversion from existing ZeroDeck | | | | |
| Import inactive Decky tree | | | | |
| Running Decky conflict refusal | | | | |

## Plugin inventory and compatibility

| Plugin (id) | Source/channel | Manifest version | Backend result | Game Mode frontend result | Data preserved | Notes |
|---|---|---|---|---|---|---|
| | | | | | | |

Record backend, frontend and data preservation separately. A loader-only upgrade must leave the
installed plugin set and `pluginOrder` unchanged.

## Resource measurements

| Metric | Idle | Update | Apply | Rollback |
|---|---|---|---|---|
| Peak daemon RSS (MiB) | | | | |
| Peak temporary disk (MiB) | | | | |
| Bytes downloaded | | | | |
| Stop-to-ready (s) | | | | |
| Stop-to-recovered (s) | | | | |

## Problems and disposition

| Severity | Description | Evidence | Owner | Resolution |
|---|---|---|---|---|
| | | | | |

## Sign-off

- Operator:
- Independent reviewer:
- Date:
- Stable eligible: yes / no (reason):
