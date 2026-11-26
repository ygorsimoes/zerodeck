# Security Policy

Security is a foundational pillar of **ZeroDeck**. Because `zerodeckd` manages privileged plugin execution, local HTTP/WebSocket endpoints, archive extraction, and Steam CEF injection, we take every vulnerability report seriously.

## Supported Versions

During pre-1.0 (`0.x.y`) development, security fixes target the latest minor/patch release on `main`.

| Version | Supported |
|---|---|
| Latest `0.x.y` / `main` | ✅ Yes |
| Older snapshots | ❌ No |

## Reporting a Vulnerability

**Please do not report security vulnerabilities through public GitHub issues or pull requests.**

Instead, report vulnerabilities privately via **[GitHub Security Advisories](https://github.com/ygorsimoes/zerodeck/security/advisories/new)**.

Please include as much of the following as possible:
- Affected crate, component, or route (e.g., `zerodeck-core`, `zerodeck-server`, `zerodeck-py-host`)
- Reproduction steps or a minimal proof-of-concept
- Potential security impact (e.g., path traversal, privilege escalation, Zip-Slip, auth/CORS bypass)
- Suggested mitigation or patch, if available

## What to Expect

- **Acknowledgment:** Within 72 hours of submission.
- **Triage & Assessment:** Confirmation of scope, severity, and remediation plan.
- **Coordinated Disclosure:** A fix is prepared, verified against regression tests, and released before public advisory details are disclosed.

## Security Design Scope

ZeroDeck enforces defense-in-depth across the following areas.

**In place today** (covered by tests and CI):
- Path-jail canonicalization (`SafePath`): canonicalized paths that cannot escape their root, symlinks included (across plugin `dist/`, `assets/`, `data/`, frontend static bundles, `file://` sideloads, and `FilePicker` roots)
- Atomic settings writes (temp file + `fsync` + rename)
- Zip-Slip (`enclosed_name()`), symlink, size, and `UMask=0077` permission (`0755`/`0644`) checks during archive extraction, with staged atomic swaps and rollback on failed installs/upgrades
- Fail-closed SHA-256 verification for remote plugin archives and `remote_binary` downloads (unless explicitly disabled via `developer.allow_unverified_installs`)
- Out-of-process plugin workers with explicit `uid`/`gid` privilege dropping for plugins without the `"root"` flag
- Root plugins only start from root-only trees: before each spawn of a `"root"` plugin, a root daemon checks that `plugins/` and the whole plugin directory are owned by uid 0, not group- or world-writable, and free of symlinks, and refuses the spawn otherwise (development opt-in `--allow-unsafe-root-plugins` / `ZERODECK_ALLOW_UNSAFE_ROOT_PLUGINS`, read only from the daemon's own command line or environment, which only root controls; never from `loader.json`, which the unprivileged user owns; see the known limitation below for crash restarts)
- Private plugin IPC over inherited `socketpair()` FDs with sanitized `env_clear()` worker environments (no predictable `/tmp` socket paths)
- Ephemeral auth token (`X-Decky-Auth` / `?auth=`) on mutating, IPC, `/ws` and `/fetch` endpoints, compared in constant time, plus strict `https://steamloopback.host` CORS and loopback `Host` DNS-rebinding protection
- SSRF-hardened outbound HTTP policy (`OutboundHttpPolicy` + `CheckedHttpClient`): blocks loopback and link-local/metadata addresses (including DNS-rebinding pinning, IPv4-mapped IPv6 forms, and up to 5 redirect hops), strips sensitive headers across cross-origin redirects, and blocks hop-by-hop / auth `X-Decky-Header-*` translations on `/fetch`
- Fuzzing of the untrusted-input parsers (WebSocket frames, worker IPC, manifests, ZIP extraction) on every pull request
- Supply chain: `cargo-deny` license, advisory and source policy (`deny.toml`); GitHub Actions pinned by commit SHA and audited with `zizmor`; toolchain binaries pinned by SHA-256; release images and tarballs ship with signed build-provenance attestations and an SBOM

**Known limitation: ancestors of `PRIVILEGED_PATH`.** Root plugins run from
`<PRIVILEGED_PATH>/plugins`, which defaults to `~deck/homebrew/plugins`. On stock SteamOS `~deck`
and `~deck/homebrew` are owned by `deck`, so the unprivileged user can rename or replace the
`plugins/` directory between the ownership check and the spawn. The daemon logs one warning at
startup listing the non-root-writable ancestors. Moving `PRIVILEGED_PATH` to a root-owned location
(such as `/var/lib/zerodeck`) is an open decision for the Phase 4 installer. The check runs on every
spawn the daemon requests (startup, enable, reload, install, debug hot-reload), but crash restarts
by the worker supervisor (with backoff) re-exec the plugin without re-checking its tree; this is
tracked with the same `PRIVILEGED_PATH` open decision.

**Planned** (see the [roadmap](README.md#roadmap)):
- Steam CEF debugging hardening and crash recovery ([Phase 4](README.md#roadmap))
