# Docker-first development, CI, and task runner — design

**Status:** approved 2026-11-24

## Intent

Docker is the default, recommended way to build, run, lint and test ZeroDeck, locally and in CI, so
that "it passes on my machine" means the same thing as "it passes in CI" and as close as practical
to what ships to the Steam Deck. Running natively stays possible as an opt-in.

## Goals

- One pinned toolchain image (`dev` stage) with every tool `just check` needs: Rust 1.100 (+rustfmt,
  clippy), cargo-deny, Node 26 + pnpm 12.16.1, Python 3 + ruff + pyright, just.
- Short `just` commands (`setup`, `check`, `fmt`, `lint`, `test`, `dev`, `shell`, `up`, `down`,
  `logs`, `smoke`, `fuzz`, `dist`) that run in the dev container by default and natively with
  `ZERODECK_NATIVE=1`. Each task's logic is written once.
- CI jobs run the same `just` recipes in the same image; required check names stay
  `rust`, `deny`, `python`, `frontend`, `docker`.
- Lint the infrastructure itself (actionlint, hadolint, `docker compose config`).
- Smoke-test the runtime image (container must become healthy) on every PR.
- Release tarball produced by a `just dist` recipe instead of inline workflow shell.

## Non-goals

- Changing the runtime image, its hardening, or the systemd deployment.
- Exercising the root → unprivileged privilege drop inside containers (needs a root daemon).
- Multi-arch release artifacts.

## Constraints

- Build on Debian bookworm (as today): an older glibc binary runs on newer SteamOS, not vice versa.
- Local containers use the host's native arch (Apple Silicon → arm64) because emulated Rust builds
  are very slow; `DOCKER_DEFAULT_PLATFORM=linux/amd64` gives exact Steam Deck arch. CI is amd64.
- Bind-mounted sources must not get root-owned files: the dev user takes the host UID/GID.
- Host `target/` and `node_modules/` (macOS binaries) must never be visible inside the container:
  shadow them with named volumes.
- Every downloaded binary is pinned and SHA-256 verified; every action pinned by commit SHA.

## Approaches considered

1. **Dev stage + compose service + dispatching justfile (chosen).** One image, one set of recipes,
   used identically by humans and CI.
2. Dev Containers (`devcontainer.json`) only: editor-centric, CI and CLI users still need a second
   path.
3. Keep CI on native runners and only add a local dev container: CI and local would keep drifting,
   which is the problem being solved.

## Design

- `Dockerfile`: `toolchain` base (rust bookworm + node copied from the node image + python3 + uv
  tools + cargo-deny + just), `dev` stage (non-root user with build-arg UID/GID, writable
  `CARGO_HOME`), `fuzz` stage (nightly + cargo-fuzz). Existing `frontend-builder`, `chef`,
  `rust-builder`, `runtime` stay; `linux-test` is removed (its job is done by `dev`).
- `compose.yaml`: `dev` service (profile `dev`) and `dev-fuzz`; `daemon` service that runs
  `zerodeckd` from source on port 1337; the hardened `zerodeckd` runtime service unchanged. Cache
  volumes are overridable via env so CI can map them to host paths cached by `actions/cache`.
- `justfile`: public recipes call `{{run}} just _<task>`, where `run` is
  `docker compose run --rm dev` or empty when native / already inside the container
  (`ZERODECK_IN_CONTAINER=1`).
- CI: a local composite action builds the dev image with the GHA build cache; jobs call
  `just ci-<area>`; Python version matrix runs the suite in `python:3.x-slim` containers.

## Left to the builder

Exact tool version pins, cache keys, recipe names of private helpers.
