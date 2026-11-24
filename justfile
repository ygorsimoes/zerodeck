set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

fmt:
    cargo fmt --all
    pnpm --dir frontend exec biome format --write .

fmt-check:
    cargo fmt --all --check

lint:
    cargo clippy --workspace --all-targets -- -D warnings

test:
    cargo test --workspace

py-test:
    PYTHONPATH=hosts/python python3 -m unittest discover -s hosts/python/tests -v

# ruff + strict pyright, as in CI. Skipped with a warning when `uv` is not installed.
py-lint:
    @if command -v uv >/dev/null 2>&1; then \
        uv tool run ruff check hosts/python && uv tool run pyright --project hosts/python; \
    else \
        echo "warning: uv not found; skipping ruff/pyright (CI still runs them)" >&2; \
    fi

# License, advisory, and source policy from deny.toml. Skipped with a warning without cargo-deny.
deny:
    @if command -v cargo-deny >/dev/null 2>&1; then \
        cargo deny check; \
    else \
        echo "warning: cargo-deny not found; skipping deny.toml checks (CI still runs them)" >&2; \
    fi

# Fuzz one target (see `cargo +nightly fuzz list`); needs nightly and cargo-fuzz.
fuzz target seconds="60":
    cargo +nightly fuzz run {{target}} -- -max_total_time={{seconds}}

build-frontend:
    pnpm --dir frontend build

fe-check:
    pnpm --dir frontend exec biome check .
    pnpm --dir frontend exec tsc --noEmit
    pnpm --dir frontend build
    node --test frontend/tests/wsrouter.test.ts

docker-build:
    docker build --target runtime -t zerodeck:runtime .

docker-test:
    docker build --target linux-test -t zerodeck:test .

docker-up:
    docker compose up -d --build

docker-down:
    docker compose down

check: fmt-check lint test py-test py-lint deny fe-check
