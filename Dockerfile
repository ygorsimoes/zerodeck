# syntax=docker/dockerfile:1

FROM node:26-bookworm-slim AS frontend-builder
WORKDIR /app
RUN npm install -g pnpm@12.16.1
COPY pnpm-workspace.yaml pnpm-lock.yaml biome.json ./
COPY frontend/package.json frontend/
RUN --mount=type=cache,target=/root/.local/share/pnpm/store \
    pnpm --dir frontend install --frozen-lockfile
COPY frontend/ frontend/
RUN pnpm --dir frontend build

FROM rust:1-slim-bookworm AS chef
WORKDIR /app
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
        cmake \
        pkg-config \
        python3 \
    && rm -rf /var/lib/apt/lists/* \
    && cargo install --locked cargo-chef

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS rust-builder
COPY --from=planner /app/recipe.json recipe.json
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo chef cook --release --recipe-path recipe.json
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo build --release --bin zerodeckd

FROM rust-builder AS linux-test
COPY hosts/python/ hosts/python/
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo test --workspace \
    && PYTHONPATH=hosts/python python3 -m unittest discover -s hosts/python/tests -v
CMD ["sh", "-c", "cargo test --workspace && PYTHONPATH=hosts/python python3 -m unittest discover -s hosts/python/tests -v"]

FROM debian:bookworm-slim AS runtime
ARG VERSION=0.1.0
LABEL org.opencontainers.image.title="ZeroDeck" \
      org.opencontainers.image.description="Performance-first, security-hardened Steam Deck plugin loader daemon" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.source="https://github.com/ygorsimoes/zerodeck"

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        python3 \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 1000 zerodeck \
    && useradd --uid 1000 --gid 1000 --create-home --shell /usr/sbin/nologin zerodeck \
    && mkdir -p /homebrew /run/zerodeck /opt/zerodeck/python /opt/zerodeck/frontend \
    && chown -R zerodeck:zerodeck /homebrew /run/zerodeck /opt/zerodeck

COPY --from=rust-builder /app/target/release/zerodeckd /usr/local/bin/zerodeckd
COPY hosts/python/zerodeck_py_host /opt/zerodeck/python/zerodeck_py_host
COPY --from=frontend-builder /app/frontend/dist /opt/zerodeck/frontend

ENV HOME=/home/zerodeck \
    UNPRIVILEGED_PATH=/homebrew \
    PRIVILEGED_PATH=/homebrew \
    PYTHONPATH=/opt/zerodeck/python \
    SERVER_HOST=0.0.0.0 \
    SERVER_PORT=1337 \
    RUST_LOG=info

USER zerodeck
WORKDIR /homebrew
EXPOSE 1337
STOPSIGNAL SIGTERM

HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD ["/usr/local/bin/zerodeckd", "--healthcheck"]

ENTRYPOINT ["/usr/local/bin/zerodeckd"]
