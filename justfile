# ZeroDeck task runner. Run `just` to list recipes.
#
# Docker is the default: each recipe below runs inside the pinned `dev` container (see compose.yaml
# and the `dev` stage in the Dockerfile), the same image CI uses. To run on this machine's own
# toolchain instead, set ZERODECK_NATIVE=1 (e.g. `ZERODECK_NATIVE=1 just check`).
#
# Each public recipe forwards to a private `_recipe` that holds the actual commands, so the logic is
# written once and runs identically in the container, natively, and in CI.

set shell := ["bash", "-euo", "pipefail", "-c"]

# Build the dev image as the host user, so files written into the checkout keep their owner.
export DEV_UID := `id -u`
export DEV_GID := `id -g`

# `true` when recipes must run here: opted in, or already inside the dev container.
native := if env("ZERODECK_NATIVE", "") =~ '^(1|true|yes)$' { "true" } else if env("ZERODECK_IN_CONTAINER", "") != "" { "true" } else { "false" }

# Prefix that runs a command in the dev container (`-T` without a terminal, as in CI).
dev := if native == "true" { "" } else { "docker compose run --rm $( [ -t 0 ] && [ -t 1 ] || echo -T ) dev" }

# Python versions the worker host supports: 3.10 is the `requires-python` floor; the rest cover
# SteamOS and newer hosts.
python_versions := "3.10 3.11 3.12 3.13"

[private]
default:
    @just --list --unsorted

# ─── Everyday ────────────────────────────────────────────────────────────────────────────────────

# Build the dev image and install frontend dependencies (rerun after toolchain changes)
[group('dev')]
setup:
    {{ if native == "true" { "true" } else { "docker compose build dev" } }}
    {{ dev }} just _fe-deps

# Core checks before a PR: formatting, lints, tests, licenses (CI jobs rust/deny/python/frontend)
[group('dev')]
check:
    {{ dev }} just _check

# Everything CI runs except fuzzing and coverage: check + Python matrix + infra lint + smoke +
# root-only tests
[group('dev')]
ci: check py-matrix lint-infra smoke test-privileged

# Format Rust and frontend code
[group('dev')]
fmt:
    {{ dev }} just _fmt

# Lint everything without running tests
[group('dev')]
lint:
    {{ dev }} just _fmt-check _clippy _doc _py-lint _fe-lint

# Run every test suite (Rust, Python worker host, frontend)
[group('dev')]
test:
    {{ dev }} just _cargo-test _py-test _fe-test

# Run zerodeckd from source on 127.0.0.1:1337, using ./.dev-homebrew as its home
[group('dev')]
dev:
    {{ if native == "true" { "UNPRIVILEGED_PATH=\"$PWD/.dev-homebrew\" PYTHONPATH=hosts/python cargo run --bin zerodeckd" } else { "docker compose run --rm --service-ports daemon" } }}

# Open a shell in the dev container (`just` works inside it, natively)
[group('dev')]
shell:
    docker compose run --rm dev bash

# Remove dev containers and their cached volumes (target, cargo, node_modules)
[group('dev')]
clean:
    docker compose --profile dev --profile fuzz --profile privileged down --volumes --remove-orphans

# ─── Focused checks (one per CI job) ─────────────────────────────────────────────────────────────

# Rust: rustfmt, clippy (-D warnings), rustdoc (-D warnings), tests
[group('checks')]
check-rust:
    {{ dev }} just _fmt-check _clippy _doc _cargo-test

# Rust test coverage report (HTML + lcov) into ./coverage (open coverage/html/index.html)
[group('checks')]
coverage:
    {{ dev }} just _coverage

# Licenses, advisories and sources (deny.toml)
[group('checks')]
deny:
    {{ dev }} just _deny

# Python worker host: ruff, strict pyright, unit tests
[group('checks')]
check-python:
    {{ dev }} just _py-lint _py-test

# Frontend: biome, tsc, build, tests
[group('checks')]
check-frontend:
    {{ dev }} just _fe-lint _fe-build _fe-test

# Python worker host tests on one interpreter, in the official python image (e.g. `just py-test-on 3.10`)
[group('checks')]
py-test-on version:
    docker run --rm --read-only --tmpfs /tmp --user "$DEV_UID:$DEV_GID" \
        -e PYTHONDONTWRITEBYTECODE=1 -e PYTHONPATH=/src/hosts/python \
        -v "$PWD/hosts/python:/src/hosts/python:ro" -w /src \
        "python:{{ version }}-slim-bookworm" \
        python -m unittest discover -s hosts/python/tests -v

# Python worker host tests on every supported interpreter
[group('checks')]
py-matrix:
    for v in {{ python_versions }}; do just py-test-on "$v"; done

# Lint the infrastructure: version pins in sync, compose, workflows (actionlint, zizmor), Dockerfile (hadolint)
[group('checks')]
lint-infra: _toolchain-sync
    docker compose config --quiet
    docker run --rm -v "$PWD:/repo:ro" -w /repo rhysd/actionlint:1.7.12 -color
    docker run --rm -v "$PWD:/repo:ro" -w /repo ghcr.io/zizmorcore/zizmor:1.33.1 --offline --no-progress .
    docker run --rm -i -v "$PWD/.hadolint.yaml:/hadolint.yaml:ro" hadolint/hadolint:v2.15.1 \
        hadolint --config /hadolint.yaml - < Dockerfile

# Root-only tests (`privileged_*`: privilege drop, parent-death signal) in the root container
# (always Docker, even with ZERODECK_NATIVE=1; inside a container it runs directly and needs root)
[group('checks')]
test-privileged:
    {{ if env("ZERODECK_IN_CONTAINER", "") != "" { "" } else { "docker compose run --rm $( [ -t 0 ] && [ -t 1 ] || echo -T ) privileged-test" } }} just _test-privileged

# Fuzz one target, e.g. `just fuzz zip_extract 120` (targets: fuzz/fuzz_targets/)
[group('checks')]
fuzz target seconds="60":
    {{ if native == "true" { "" } else { "docker compose run --rm $( [ -t 0 ] && [ -t 1 ] || echo -T ) fuzz" } }} just _fuzz {{ target }} {{ seconds }}

# Fuzz every target for the given number of seconds each
[group('checks')]
fuzz-all seconds="30":
    {{ if native == "true" { "" } else { "docker compose run --rm $( [ -t 0 ] && [ -t 1 ] || echo -T ) fuzz" } }} just _fuzz-all {{ seconds }}

# ─── Runtime image (what ships) ──────────────────────────────────────────────────────────────────

# Build the hardened runtime image (zerodeck:runtime)
[group('runtime')]
build:
    docker compose build zerodeckd

# Start the runtime container in the background on 127.0.0.1:1337
[group('runtime')]
up:
    mkdir -p .dev-homebrew
    docker compose up -d --build --wait zerodeckd

# Stop the runtime container
[group('runtime')]
down:
    docker compose down

# Follow the runtime container's logs
[group('runtime')]
logs:
    docker compose logs -f zerodeckd

# Start the runtime image, wait until healthy, probe /healthz from the host, stop it
[group('runtime')]
smoke *flags="--build":
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p .dev-homebrew
    trap 'docker compose down --timeout 5 >/dev/null 2>&1 || true' EXIT
    if ! docker compose up -d {{ flags }} --wait --wait-timeout 60 zerodeckd; then
        docker compose logs zerodeckd
        exit 1
    fi
    curl -fsS --retry 5 --retry-connrefused --retry-delay 1 \
        "http://127.0.0.1:${ZERODECK_PORT:-1337}/healthz"
    echo
    echo "smoke: zerodeckd is healthy"

# Package the manual/diagnostic tarball, the managed TUF generation archive and SHA256SUMS from one
# runtime image into dist/. Set ZERODECK_BUILD_ID for a real release.
[group('runtime')]
dist tag image="zerodeck:runtime":
    just _dist all "{{ tag }}" "{{ image }}"

# Package only the managed generation archive (archive root: release.json, bin, python, frontend)
[group('runtime')]
dist-managed tag image="zerodeck:runtime":
    just _dist managed "{{ tag }}" "{{ image }}"

# Validate the exact bytes of a release archive, its checksums and the failure/retry boundaries
[group('runtime')]
test-release-package archive:
    just _test-release-package "{{ archive }}"

[private]
_dist mode tag image:
    bash deploy/update/package-release.sh "{{ mode }}" "{{ tag }}" "{{ image }}" dist

[private]
_test-release-package archive:
    bash deploy/update/tests/release.sh "{{ archive }}"

# ─── Implementations (run where the public recipe decided: container or native) ─────────────────

[private]
_check: _fmt-check _clippy _doc _cargo-test _py-lint _py-test _deny _fe-lint _fe-build _fe-test _release-scripts

[private]
_fmt: _fe-deps
    cargo fmt --all
    pnpm --dir frontend exec biome format --write .

[private]
_fmt-check:
    cargo fmt --all --check

[private]
_clippy:
    cargo clippy --workspace --all-targets --locked -- -D warnings

[private]
_doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked

[private]
_cargo-test:
    cargo test --workspace --locked

[private]
_test-privileged:
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ "$(id -u)" -ne 0 ]]; then
        echo "error: privileged tests require root; run \`just test-privileged\` from the host" >&2
        exit 1
    fi
    cargo test --workspace --locked -- --ignored privileged_

[private]
_release-scripts:
    bash deploy/update/tests/release-fixture.sh
    bash deploy/update/tests/bootstrap.sh
    bash deploy/update/tests/publish.sh
    bash deploy/update/tests/promotion.sh

[private]
_coverage:
    cargo llvm-cov --workspace --locked --html --output-dir coverage
    cargo llvm-cov report --lcov --output-path coverage/lcov.info

[private]
_py-test:
    PYTHONDONTWRITEBYTECODE=1 PYTHONPATH=hosts/python python3 -m unittest discover -s hosts/python/tests -v

# The dev image ships pinned ruff + pyright; natively, fall back to `uv tool run`, else skip.
[private]
_py-lint:
    #!/usr/bin/env bash
    set -euo pipefail
    if command -v ruff >/dev/null 2>&1 && command -v pyright >/dev/null 2>&1; then
        ruff check hosts/python
        pyright --project hosts/python
    elif [[ -z "${ZERODECK_IN_CONTAINER:-}" ]] && command -v uv >/dev/null 2>&1; then
        uv tool run ruff check hosts/python
        uv tool run pyright --project hosts/python
    elif [[ -z "${ZERODECK_IN_CONTAINER:-}" ]]; then
        echo "warning: ruff/pyright (or uv) not found; skipping Python lint (\`just check\` in Docker runs it)" >&2
    else
        echo "error: ruff/pyright missing from the dev image" >&2
        exit 1
    fi

[private]
_deny:
    #!/usr/bin/env bash
    set -euo pipefail
    if command -v cargo-deny >/dev/null 2>&1; then
        cargo deny --locked check
    elif [[ -z "${ZERODECK_IN_CONTAINER:-}" ]]; then
        echo "warning: cargo-deny not found; skipping deny.toml checks (\`just check\` in Docker runs it)" >&2
    else
        echo "error: cargo-deny missing from the dev image" >&2
        exit 1
    fi

# Versions that must agree across files (Dependabot and humans only ever bump one of them).
[private]
_toolchain-sync:
    #!/usr/bin/env bash
    set -euo pipefail
    fail=0
    expect() { # <what> <expected> <actual...>
        local what="$1" want="$2"; shift 2
        for got in "$@"; do
            if [[ "${got}" != "${want}" ]]; then
                echo "error: ${what}: expected ${want}, found ${got}" >&2
                fail=1
            fi
        done
    }
    rust="$(sed -n 's/^channel = "\(.*\)"/\1/p' rust-toolchain.toml)"
    expect "Dockerfile rust image vs rust-toolchain.toml" "${rust}" \
        $(grep -oE '^FROM rust:[0-9.]+' Dockerfile | cut -d: -f2)
    expect "Cargo.toml rust-version vs rust-toolchain.toml" "${rust%.*}" \
        "$(sed -n 's/^rust-version = "\(.*\)"/\1/p' Cargo.toml)"
    pnpm="$(sed -n 's/.*"packageManager": "pnpm@\([^"]*\)".*/\1/p' package.json)"
    expect "Dockerfile pnpm vs package.json packageManager" "${pnpm}" \
        $(grep -oE 'pnpm@[0-9][0-9.]*' Dockerfile | cut -d@ -f2) \
        "$(sed -n 's/^ARG PNPM_VERSION=//p' Dockerfile)"
    expect "just in workflows vs Dockerfile JUST_VERSION" "$(sed -n 's/^ARG JUST_VERSION=//p' Dockerfile)" \
        $(grep -rhoE 'just@[0-9][0-9.]*' .github | cut -d@ -f2)
    [[ "${fail}" -eq 0 ]] && echo "toolchain pins in sync (rust ${rust}, pnpm ${pnpm})"
    exit "${fail}"

[private]
_fe-deps:
    pnpm install --frozen-lockfile --config.confirmModulesPurge=false

[private]
_fe-lint: _fe-deps
    pnpm --dir frontend exec biome check .
    pnpm --dir frontend exec tsc --noEmit

[private]
_fe-build: _fe-deps
    pnpm --dir frontend build

[private]
_fe-test: _fe-deps
    node --test frontend/tests/

[private]
_fuzz target seconds:
    cargo +nightly fuzz run --target "$(rustc +nightly -vV | sed -n 's/^host: //p')" {{ target }} -- -max_total_time={{ seconds }}

[private]
_fuzz-all seconds:
    #!/usr/bin/env bash
    set -euo pipefail
    host="$(rustc +nightly -vV | sed -n 's/^host: //p')"
    for target in $(cargo +nightly fuzz list); do
        echo "::group::fuzz ${target}"
        cargo +nightly fuzz run --target "${host}" "${target}" -- -max_total_time={{ seconds }}
        echo "::endgroup::"
    done
