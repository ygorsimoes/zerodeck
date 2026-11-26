# AGENTS.md — ZeroDeck Contributor & Coding Agent Guide

This file defines mandatory rules, architectural invariants, and verification workflows for AI coding agents and automated contributors working in this repository.

## 1. Project Identity & Hard Legal Boundary

- **ZeroDeck** is a clean-room, **MIT-licensed** Rust replacement for `decky-loader`.
- **NEVER copy GPL-licensed code** (or any copyleft code) from `decky-loader` or external GPL projects. Implement against public wire/IPC contracts documented in `docs/architecture/wire-protocol.md` and `docs/superpowers/specs/`.
- All Rust dependencies must satisfy `deny.toml` (`MIT`, `Apache-2.0`, `BSD-2-Clause`, `BSD-3-Clause`, `ISC`, `Zlib`, `Unicode-3.0`, `CDLA-Permissive-2.0`; no wildcards; no unknown registries/git sources).

## 2. Repository Architecture & Crate Boundaries

Maintain the acyclic Cargo workspace dependency graph in `Cargo.toml`:

```text
zerodeckd
 ├── zerodeck-server ── zerodeck-core, zerodeck-cef, zerodeck-plugin-host, zerodeck-platform, zerodeck-protocol
 ├── zerodeck-cef ───── zerodeck-protocol
 ├── zerodeck-core ──── zerodeck-platform, zerodeck-plugin-host, zerodeck-protocol
 └── zerodeck-plugin-host ── zerodeck-platform, zerodeck-protocol
```

- `zerodeck-protocol` MUST have **zero** internal `zerodeck-*` dependencies.
- `zerodeck-platform` MUST depend only on `zerodeck-protocol` and OS/async primitives.
- Keep `hosts/python` (`zerodeck-py-host`) free of third-party runtime dependencies (`dependencies = []` in `hosts/python/pyproject.toml`).
- Keep `frontend` (`@zerodeck/frontend`) externalizing `react`, `react/jsx-runtime`, and `react-dom` to Steam CEF globals (`SP_REACT`, `SP_JSX`, `SP_REACTDOM`).

## 3. Toolchain & Code Standards

- **Rust:** Pinned to `1.100.0` (`rust-toolchain.toml`), Edition `2024`, formatted with `rustfmt.toml`.
  - Workspace lints (`Cargo.toml`) enable `unsafe_code = "warn"`, `missing_debug_implementations = "warn"`, `rust_2018_idioms`, `clippy::all`, and `clippy::pedantic`. CI runs `clippy` with `-D warnings`.
  - Minimize `unsafe`; any unavoidable `unsafe` block MUST include a clear `// SAFETY:` invariant comment.
  - Crates without `unsafe` declare `#![forbid(unsafe_code)]`; `zerodeck-cef` uses `deny` (Phase 4 may need a reviewed `#[allow]`); only `zerodeck-plugin-host` may contain `unsafe`.
- **Python (`hosts/python`):** Target `>=3.10`, line length `100`, `ruff` (`E`, `F`, `I`, `UP`, `B`), and `pyright` in `"strict"` mode.
- **Frontend (`frontend/`):** Use **`pnpm` exclusively** (`pnpm@12.16.1`, pinned by `packageManager` in the root `package.json`; workspace in `pnpm-workspace.yaml`; never create `package-lock.json` or `yarn.lock`). TypeScript `^7.0.2` strict mode + `@biomejs/biome` (`biome.json`: 2-space indent, single quotes in JS/TS, line width `100`).
- **Editor defaults:** Follow `.editorconfig` (LF, UTF-8, trailing newline, 4 spaces for `.rs`/`.py`/`.toml`/`justfile`, 2 spaces for `.ts`/`.json`/`.yml`/`.md`).

## 4. Non-Negotiable Security Invariants

Never weaken or bypass these invariants:
- **Path-jail canonicalization:** All plugin static/asset/data routes and file operations must validate paths through `SafePath` (`zerodeck-platform`).
- **Atomic writes:** Config/state persistence must use temp file + `sync_all()` + `rename()` (`atomic_write`).
- **Archive & binary safety:** Enforce `enclosed_name()` Zip-Slip checks, `0755` (never `0777`) permissions, and fail-closed SHA-256 verification on remote downloads unless explicitly overridden by developer opt-in.
- **Privilege & IPC isolation:** Drop `uid`/`gid` to the unprivileged host user for plugins without the `"root"` flag; use inherited Unix `socketpair()` FDs or per-plugin `0700` directories (never predictable `/tmp` socket paths).
- **CORS & Auth:** Restrict CORS to `https://steamloopback.host` and protect mutating/IPC endpoints with the ephemeral `X-Decky-Auth` / `?auth=` token.

## 5. Commits, Releases & PR Workflow

- Use **Conventional Commits** (`feat`, `fix`, `perf`, `refactor`, `docs`, `style`, `test`, `build`, `ci`, `chore`, `revert`) — enforced on PR titles by `.github/workflows/pr-lint.yml`.
- Versioning is automated by **Release Please** (`.release-please-manifest.json`, `release-please-config.json`, `.github/workflows/release-please.yml`).
  - Do **not** manually bump versions in `Cargo.toml`, `.release-please-manifest.json`, `hosts/python/pyproject.toml`, `frontend/package.json`, or `Dockerfile` unless fixing release automation itself.
  - Preserve `# x-release-please-version`, `# x-release-please-start-version`, and `# x-release-please-end` markers in `Cargo.toml` and `Dockerfile`.

## 6. Docker-First Workflow

- The pinned `dev` image (`Dockerfile` stage `dev`, `compose.yaml` service `dev`) is the canonical environment. `just <recipe>` runs inside it by default; `ZERODECK_NATIVE=1` opts into the host toolchain, and recipes run natively automatically inside the container (`ZERODECK_IN_CONTAINER=1`).
- Add new tasks as a public recipe that forwards to a private `_recipe` (see `justfile`), so the logic exists once and works in Docker, natively, and in CI.
- CI jobs must call `just` recipes rather than inline tool commands. Keep the required check job ids `rust`, `deny`, `python`, `frontend`, `docker` stable.
- Pin new tools in the `Dockerfile` with a version `ARG` and, for downloaded binaries, a SHA-256 digest (fail closed). Pin GitHub Actions by full commit SHA (version in a trailing comment), declare `permissions` per job (workflow level stays read-only or `{}`), and check out with `persist-credentials: false`.
- Versions duplicated across files must move together: Rust (`rust-toolchain.toml`, every `rust:` image in the `Dockerfile`, `rust-version` in `Cargo.toml`), pnpm (`package.json` `packageManager`, `Dockerfile`), just (`JUST_VERSION`, `just@` pins in `.github/`). `just lint-infra` enforces this.
- Keep the dev services unprivileged (`cap_drop: ALL`, `no-new-privileges`) and keep the host's `target/`/`node_modules/` out of containers (named volumes). `.devcontainer/` reuses the compose `dev` service; do not fork a second environment.
- The compose `privileged-test` service (`just test-privileged`, always run in Docker even with `ZERODECK_NATIVE=1`) is the only root container: it keeps `no-new-privileges`, adds back only the capabilities the privilege drop needs, and uses its own `target/`/cargo volumes. Root-only tests are named `privileged_*`, marked `#[ignore = "requires root; run with just test-privileged"]`, start with `assert_eq!(rustix::process::geteuid().as_raw(), 0)`, and drop to the user named by `ZERODECK_TEST_HOST_USER` (compose sets `dev`).

## 7. Required Verification Before Finishing

Always run after your final edit and verify all pass:

```bash
just check
```

If you modify `Dockerfile`, `compose.yaml`, `justfile`, `.devcontainer/`, or `.github/**`, run the full CI set instead (it includes `just check`, the Python version matrix, `just lint-infra`, `just smoke` and `just test-privileged`):

```bash
just ci
```
