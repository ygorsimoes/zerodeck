# ZeroDeck and Decky Plugin Update Design

## Intent and release order

Deliver reliable native ZeroDeck updates and Decky-compatible plugin updates before a full
Game Mode release. `update` checks and prepares; `upgrade` activates explicitly. Background
checks are opt-in, and no scheduled job activates an update in the first release. The CLI
works before QAM exists. Complete the native loader update first, then headless plugin
updates; qualify plugin frontends separately on real hardware. Neither operation implicitly
updates the other.

## Approaches

- **Native ZeroDeck:** choose TUF with `tough` and TUF-on-CI plus ZeroDeck-owned activation.
  GitHub attestation alone gives provenance without fresh update authorization; delegating
  the native lifecycle to `systemd-sysupdate` adds an experimental host dependency. Docker
  and distro-managed installations continue using their external package manager.
- **Plugins:** choose the official Decky Stable/Testing sources plus an explicit sideload
  path. Mirroring adds delays, redistribution obligations, and operational load;
  sideload-only loses store discovery. The loader's TUF catalog does not authorize Decky
  plugins. Use Decky's public docs and black-box contracts; do not copy GPL/AGPL code.
- **Disk layout:** use immutable version directories and one active reference for the
  loader. For a managed plugin, keep its actual directory name and swap the whole tree
  with a retained sibling. This preserves existing `DECKY_PLUGIN_DIR`, assets and custom
  binaries, including for plugins that cannot tolerate directory aliases. Test the native
  atomic exchange where available and the recoverable two-rename fallback.

## Global constraints

- Rust `1.100.0`, Edition `2024`; Python `>=3.10`, stdlib only; frontend uses `pnpm@12.16.1` and Steam's React globals.
- MIT clean-room implementation; keep the Cargo graph acyclic and pass `cargo deny` on the actual lockfile.
- Keep `SafePath`, atomic writes, archive containment, fail-closed remote SHA-256 for managed updates, `0755` executables and the existing IPC, privilege, CORS and auth boundaries.
- No installs from SteamOS's read-only `/usr`; test a persistent root-controlled install root and systemd configuration across an actual SteamOS update before Stable.
- The first supported native distribution target is SteamOS `linux-x86_64` with systemd; Docker images are upgraded externally by digest.
- `just check` after each change; use `just ci` for changes to `Dockerfile`, `compose.yaml`, `justfile`, `.devcontainer/`, or `.github/**`; keep required CI job IDs stable.

## Loader distribution and local activation

Versioning remains with Release Please. Use draft → package and smoke test by exact commit
and OCI digest → attest and upload → publish immutable release → authorize the digest in
TUF. A failed draft/promotion can be retried by exact version and digest. Use separate
signing authority for TUF root/targets, and CI/KMS automation for expiring online metadata.
Stage a root rotation, expiration, and recovery drill before Stable. Bootstrap the trusted
root out-of-band as part of a verified initial install; no trust-on-first-use. A Stable
client never silently opts into Testing, an older release, or a differently hashed package
reusing the same version. An offline upgrade of an already staged artifact is allowed only
while its locally verified authorization is unexpired and has not been superseded by
newer trusted metadata. Reject a clock earlier than the persisted last successful
verification time. Local recovery is independent of the clock and network.

Ship an independent Rust binary, `zerodeckctl`. `update` validates TUF metadata and prepares
the full payload; `upgrade` requests a privileged apply; `status` reads installed and pending
state; `rollback` explicitly selects a retained, previously confirmed generation. Keep the
control binary and journal outside app generations. Store root-owned code, update state,
staging and cache apart from user data. `/var/lib/zerodeck` is the initial host layout to
qualify, with systemd units in `/etc/systemd/system`; an alternate persistent root must
pass the same ownership, local-filesystem, and SteamOS survival checks. Do not read release
files from a user-writable ancestor. The managed launcher resolves one concrete generation
and fixes binary, Python and frontend paths to that generation for the process lifetime.
Offer a clean install and an explicit, reversible conversion of existing ZeroDeck
installs. Preserve the unprivileged data path, legacy code and administrator overrides
until the managed generation is confirmed. Treat a running Decky installation as a port/path
conflict; never stop it or rewrite its files on the user's behalf. Import from an
inactive Decky installation by copy with explicit confirmation, without promoting
imported privileged code into a trusted root tree automatically.
The package's build identity is the embedded build ID, not a digest of an archive containing
that ID. Record the archive digest separately.

A release target lives at `linux-x86_64/zerodeck-<version>.tar.gz` in TUF. Its signed `custom`
metadata records `channel` (`stable` or `testing`), `version` (SemVer), `build_id`, and
`min_updater` (SemVer). TUF hashes and length cover the whole archive; that archive digest is
the generation identity. Inside the archive, `release.json` schema `1` records `build_id` and
`version` so the client can verify they match the signed target without hashing the archive
against itself. Selection reads only `stable` unless the operator opts into `testing`, rejects
prerelease SemVer by default, and never auto-selects a version below the installed one. Updater replacement is an explicit,
verified administrative bootstrap operation in v1: signed targets declare the minimum
updater version, and a bootstrap replacement must be refused during a pending activation
or if it cannot read the persisted format. Keep the existing `tar.gz` format; gate optional
Zstandard adoption on measurement with the Linux package on the Deck.

The managed transaction records the previously confirmed generation and candidate before
stopping the active service. Validate and sync the staged tree before activation. Change
`current` with a same-filesystem rename and directory sync. Startup confirmation is
version/build-aware and checks frontend assets, Python import, authenticated RPC and
systemd readiness within 60 seconds; no unrelated plugin is required to start successfully.
On failure retain the failing digest and recover the previous generation offline. The host
contract is `/usr/local/lib/zerodeck/bin/zerodeckctl` for the controller, with
`/var/lib/zerodeck` for `current`, `generations/`, update state, staging, the TUF datastore and
the maintenance lock. `current` is a root-owned file containing the active generation digest.
The daemon service executes a hidden `zerodeckctl exec-current --root /var/lib/zerodeck` wrapper,
which validates the selected generation and `exec`s `<generation>/bin/zerodeckd
--managed-generation <generation>`, so systemd's main PID is the daemon and no stable path has
to be repointed inside a writable tree. Managed apply runs in a dedicated systemd unit with
post-stop recovery; a separate ordered oneshot runs before the daemon on boot. A kill or reboot before confirmation reverts rather
than silently reporting success. Post-stop recovery and boot recovery must distinguish
whether to restart the daemon or only restore the reference. When no good generation exists
(e.g. failed first install), leave the service stopped, preserve state, and report the error.
If a managed service is already stopped, a normal upgrade leaves it stopped and the
candidate prepared; a first install explicitly requests startup. Offline recovery may
restore the active reference without starting a deliberately stopped service.
Local rollback preserves the trusted catalog's high-water marks. No incompatible loader
state migration ships without an independently tested recovery procedure.

## Plugins: catalog, staging, and activation

Use the documented [Stable](https://plugins.deckbrew.xyz/plugins) and
[Testing](https://testing.deckbrew.xyz/plugins) catalog surfaces and a separately verified
public artifact contract. `3.3.1` Stable is the initial Decky Loader reference;
`4.0.0-pre1` is exploratory. Do not assume the newer update API or artifact URL shape
from release notes. Keep catalog IDs scoped to source and channel; record the displayed
version and manifest version separately. Use validated upstream ordering when available;
show an explicit choice for incomparable legacy versions. A reused version with a
different digest is a conflict; do not replace a plugin silently. No automatic association
of an existing plugin by name alone. Adopt an existing plugin only by an explicit
operation selecting a verified catalog entry and exercising the usual activation.
The legacy sideload RPC cannot overwrite a catalog-managed plugin unless the owner
explicitly detaches it first. Sideloading and public RPC shapes already documented
in `wire-protocol.md` remain supported. Missing `package.json` or `version` remains allowed
for legacy sideloads. Store-specific deviations must be backed by observed contracts.
Entries without a usable archive hash or required `remote_binary` hash are reported as
unavailable for managed activation; this gap does not block the remainder of the catalog.

Prepare the plugin archive and every `remote_binary` before quiescing the live plugin.
Bound concurrency, bytes, expansion and disk usage. Preserve the current routable plugin
until activation starts. Back up managed `settings/` and `data/` after quiescing, in a
root-controlled transaction directory; keep the managed paths and permissions compatible.
If a checkpoint cannot be made safely, do not begin a managed activation. Restore
the checkpoint on a failed *pending* trial only with established exclusive ownership of
the data writes; stopping the worker alone does not rule out detached helpers, another
loader, or an external editor. With unchanged managed data, restore the previous code
without rewriting data. With changed data and no exclusive writer evidence, disable the
affected plugin and retain both copies. Hooks can change unmanaged files, services or remote
state: do not promise to revert those effects. A post-confirmation code downgrade does not
restore historical data by default. Data restoration is explicitly selected if needed.

Journal a durable operation ID and expected active generation. Repeated identical mutation
requests return the existing result; an ID reused for a different request fails. Revalidate
current generation, origin, privilege and space under the per-plugin lock before swap.
Serialize lifecycle operations for the same plugin and make activation conflict with a
loader-wide upgrade. Use versioned bundles/assets for managed frontends while preserving
legacy route aliases; unload the old frontend or request a controlled reload where the
plugin cannot be cleanly removed. Existing workers receive `_unload`, never `_uninstall`
for replacement. Keep `pluginOrder`, disabled state and paths. Privilege escalation needs
administrative approval bound to the selected digest, via a root-only local admin
channel independent of the Steam WebSocket token. A root plugin must pass ownership
validation on every spawn, including restarts, and run from a root-controlled tree; a
writable checkout cannot be implicitly promoted or used as an administrator-approved source.

Treat installed code, backend readiness and frontend readiness as separate facts. At
minimum, backend confirmation requires import, migration, IPC readiness and a bounded
startup observation. Use the existing 15-second supervisor startup deadline by
default, followed by a 2-second observation without crash or early `_main` failure.
Long-running `_main` is valid. Frontend-only
and disabled plugins may be staged and installed, but stay unchecked until the relevant
runtime is available. A managed Game Mode trial is only fully confirmed when its frontend
also acknowledges the expected generation. A definite frontend failure during the original
pending activation uses the same code and data recovery policy as a backend failure.
An offline/headless install may complete the package/backend result and retain the prior
code while frontend verification is deferred.
On the first eligible frontend startup, loss of CEF context or restart leaves the frontend
unchecked and retryable; a definite load failure disables that plugin and retains the prior
code and any recovery data. Do not automatically restore an old checkpoint after writes
from an already active backend. Once a healthy generation is restored, other plugins must
remain available even if this plugin cannot restart.

## Verification and release gates

- Offline fixtures and fault injection cover transport interruption, wrong digest, expired
  metadata, mismatched architecture, ENOSPC, failed sync, process kill or VM reboot at each
  activation boundary, duplicate requests, wrong worker/build acknowledgement, and offline
  recovery with both healthy and unusable rollback copies.
- Plugin matrix: ESM/IIFE, Python v0/v1 and frontend-only, `remote_binary`, wrapped/unwrapped
  ZIP, legacy versions, same-name conflicts, disabled plugin, root flag increase, failed
  migration, hung worker, bad JS, assets with chunks, and managed data restoration.
- Steam Deck Stable hardware acceptance: bootstrap without changing rootfs writability,
  update/rollback without network, SteamOS-update survival, and an unchanged plugin set in
  a loader-only upgrade. Record Steam/SteamOS/build and report plugin compatibility by
  package/backend/Game Mode separately. Measure peak memory, bytes, temporary disk and the
  stop-to-ready or stop-to-recovered interval. Publish results, not guessed targets.
- No automatic activation, fleet API, mirroring, binary deltas, cyclic crate dependencies,
  or broad compatibility claims in v1. Leave the precise CDN, archive compression after
  measurement, and the choice of representative upstream plugins to the implementer.

## References

- [TUF specification](https://theupdateframework.github.io/specification/latest/) and [TUF-on-CI](https://github.com/theupdateframework/tuf-on-ci)
- [GitHub immutable releases](https://docs.github.com/en/code-security/concepts/supply-chain-security/immutable-releases)
- [systemd service readiness](https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html) and [Linux `fsync`](https://man7.org/linux/man-pages/man2/fsync.2.html)
- [SteamOS configuration persistence](https://blogs.igalia.com/berto/2025/02/05/keeping-your-system-wide-configuration-files-intact-after-updating-steamos/)
- [Decky Loader releases](https://github.com/SteamDeckHomebrew/decky-loader/releases), [Store guide](https://wiki.deckbrew.xyz/en/user-guide/plugin-store), [plugin template](https://github.com/SteamDeckHomebrew/decky-plugin-template), and [`wire-protocol.md`](../../architecture/wire-protocol.md)
