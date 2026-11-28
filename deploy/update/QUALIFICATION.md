# Release Qualification: Implementation, Testing and Steam Deck

This document separates **implementation completion** from **hardware qualification**. It does not
weaken the existing signed-channel and VM qualification rules.

## 1. Implementation complete (simulated environment)

A Phase 4 change is implementation-complete when:

- `just check` passes; changes to `Dockerfile`, `compose.yaml`, `justfile`, `.devcontainer/` or
  `.github/**` also pass `just ci`.
- The frontend functional/strict tests pass. The clean-room ABI, `@decky/api` v2, ESM/legacy bundle
  loading, asset URLs, unload/reload and CEF confirmation are covered.
- The CEF supervisor tests pass, including stale confirmations, context changes, bounded backoff,
  safe mode and the 100-reinjection soak.
- `ZERODECK_NATIVE=1 just perf` records a budget report (see
  [`docs/architecture/performance-budget.md`](../../docs/architecture/performance-budget.md)).
- The release package contains non-empty, parseable `frontend/index.js` and
  `frontend/fallback.js` for both manual and managed archives.
- The QAM host limitation is recorded: it is an in-page panel until the Steam React-tree tab adapter
  is hardware-qualified.

No Steam Deck is required for this section, for CI, or for publishing to the Testing channel.

## 2. Testing channel

Testing does **not** require hardware evidence. A release may enter Testing after:

- implementation completion above;
- the existing immutable release, signed channel, VM qualification and provenance rules in
  [`RELEASE.md`](RELEASE.md) are satisfied;
- the manual and managed archive digests are published in the release.

Testing is explicitly experimental. Users opt in; no stability claim is made for Game Mode.

## 3. Stable for Steam Deck

Stable for Steam Deck requires the **same managed archive digest** that was tested on hardware:

- the checklist in [`COMPATIBILITY.md`](COMPATIBILITY.md) is filled from a real SteamOS device;
- clean bootstrap, `zerodeckctl update` + `upgrade`, offline rollback, interrupted apply, recovery,
  port collision, SteamOS update survival and a loader-only upgrade are recorded;
- plugin inventory and backend/frontend results are recorded per plugin, including Game Mode;
- CEF/QAM evidence covers injection, re-injection after a Steam restart, safe mode and the actual
  panel rendering;
- installation, update and uninstall preserve `/home/deck/homebrew`, plugin order and plugin data.

The existing VM qualification remains the general Stable gate. Steam Deck Stable additionally
requires the hardware evidence above. If hardware evidence is missing or fails, the release stays in
Testing; the code is not blocked or reverted solely for lack of a device.

## 4. Safety invariants

- Installation and updates use verified archives, signed channels, atomic generations and rollback;
  no UI action promotes a plugin to `"root"`.
- The QAM only calls `loader/enable_plugin` and `loader/disable_plugin`. Root execution still
  requires the plugin's `"root"` flag and a root-controlled `PRIVILEGED_PATH`; the unprivileged
  settings file cannot grant root.
- Uninstalling ZeroDeck must not delete `/home/deck/homebrew` or Decky files. The supported manual
  uninstall procedure is in [`RELEASE.md`](RELEASE.md#safe-uninstall).

## 5. Evidence and sign-off

Record the release tag, source commit, manual and managed digests, build IDs, channel sequence and
the hardware checklist in `deploy/update/evidence/<tag>.md` (create the directory when the first
device run happens). Keep raw logs or command output alongside it.

| Role | Required for Testing | Required for Stable (Deck) |
|---|---|---|
| Implementation owner | CI + simulated evidence | same |
| Release automation | immutable release + signed channel | same digest |
| VM qualification | existing Stable rule | same |
| Hardware operator | not required | checklist completed |
