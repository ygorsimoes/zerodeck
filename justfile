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

check: fmt-check lint test py-test fe-check
