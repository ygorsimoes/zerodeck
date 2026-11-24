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

ZeroDeck enforces defense-in-depth across:
- Path-jail canonicalization (`SafePath`) on plugin static/asset/data routes and filepicker roots
- Fail-closed SHA-256 verification for remote Store installs and `remote_binary` downloads
- Zip-Slip (`enclosed_name()`) and symlink rejection during archive extraction
- Ephemeral CSRF token auth (`X-Decky-Auth` / `?auth=`) and strict `https://steamloopback.host` CORS allowlisting
- Out-of-process plugin workers with explicit `uid`/`gid` privilege dropping and `socketpair()` / `0700` runtime directory isolation
- Automated dependency license & advisory auditing via `cargo-deny` (`deny.toml`)
