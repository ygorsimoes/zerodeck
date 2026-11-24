# Docker-first Development Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the pinned dev container the default way to build, lint, test and run ZeroDeck locally and in CI.

**Architecture:** A `dev` Dockerfile stage holds the whole toolchain; `compose.yaml` runs it with the source bind-mounted and caches in named volumes; the `justfile` dispatches every public recipe into that container (or runs natively with `ZERODECK_NATIVE=1`); CI jobs call the same recipes.

**Tech Stack:** Docker BuildKit, Compose v2, just 1.59, GitHub Actions.

**Spec:** `docs/superpowers/specs/2026-11-24-docker-first-dev-design.md`

## Global Constraints

- Required CI check names stay exactly: `rust`, `deny`, `python`, `frontend`, `docker`.
- Tool pins: cargo-deny 0.22.1, just 1.59.0, uv 0.13.10, ruff 0.17.6, pyright 1.1.414, cargo-fuzz 0.13.2, actionlint 1.7.12, hadolint v2.15.1, pnpm 12.16.1, Node 26, Rust 1.100.0.
- Downloaded binaries are SHA-256 verified; actions pinned by commit SHA.
- Release Please markers in `Dockerfile` stay intact; no version bumps.
- `docker compose config --quiet` and `just check` must pass.

## Review Focus

- Linux host with UID ≠ 1000: files written by `just fmt` must be owned by the host user.
- macOS host with an existing `node_modules`/`target`: container must not load host binaries.
- `just check` run inside the dev shell must not try to start Docker again (recursion).
- `ZERODECK_NATIVE=1 just check` without `uv`/`cargo-deny` keeps warning-and-skip behaviour.
- Runtime container must reach `healthy` under the hardened compose settings (`just smoke`).

---

### Task 1: Dev and fuzz images

**Files:** Modify `Dockerfile`, `.dockerignore`

- [ ] Add `toolchain` stage from `rust:1.100.0-slim-bookworm` with node 26 copied from `node:26-bookworm-slim`, pnpm, pyright (npm), uv + ruff, cargo-deny and just (checksummed), python3.
- [ ] Add `dev` stage: user `dev` with `DEV_UID`/`DEV_GID` build args, `CARGO_HOME=/home/dev/.cargo`, `ZERODECK_IN_CONTAINER=1`, `WORKDIR /workspace`, pre-created cache dirs owned by `dev`.
- [ ] Add `fuzz` stage: nightly toolchain + `cargo install --locked cargo-fuzz@0.13.2`.
- [ ] Remove `linux-test` stage.
- [ ] Verify: `docker build --target dev .` and `--target runtime` succeed.

### Task 2: Compose services

**Files:** Modify `compose.yaml`

- [ ] `dev` (profile `dev`): build target `dev`, image `zerodeck-dev:local`, bind `.:/workspace`, named volumes over `target`, `node_modules`, `frontend/node_modules`, `fuzz/target`, cargo home, pnpm store; volume names overridable by env for CI.
- [ ] `daemon` (profile `dev`): same image, `cargo run --bin zerodeckd`, port `127.0.0.1:1337`, `.dev-homebrew` mounted.
- [ ] `fuzz` (profile `fuzz`): target `fuzz`.
- [ ] Keep hardened `zerodeckd` runtime service; drop `linux-test`.
- [ ] Verify: `docker compose config --quiet`.

### Task 3: Justfile

**Files:** Modify `justfile`

- [ ] `run` variable dispatches into `docker compose run --rm dev` unless `ZERODECK_NATIVE=1` or `ZERODECK_IN_CONTAINER=1`.
- [ ] Public: `setup check fmt lint test dev shell up down logs smoke fuzz dist clean` plus CI groups `ci-rust ci-deny ci-python ci-frontend ci-infra py-test-on`.
- [ ] Verify: `just check` passes in Docker; `ZERODECK_NATIVE=1 just --dry-run check` shows no docker call.

### Task 4: CI

**Files:** Modify `.github/workflows/ci.yml`, `.github/workflows/release-please.yml`; Create `.github/actions/dev-image/action.yml`

- [ ] Composite action builds `dev` image with GHA cache and the runner UID.
- [ ] Jobs `rust`, `deny`, `python` (+ `python-tests` matrix via `just py-test-on`), `frontend`, `docker` (runtime build + `just smoke`), `infra`, `fuzz` all call `just`.
- [ ] Release tarball via `just dist`.
- [ ] Verify: `just ci-infra` (actionlint + hadolint + compose config).

### Task 5: Docs

**Files:** Modify `README.md`, `CONTRIBUTING.md`, `AGENTS.md`

- [ ] Docker as the recommended path, native as opt-in; command table.
- [ ] Final: `just check`, `docker compose config --quiet`, `just smoke`.
