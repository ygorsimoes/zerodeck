# syntax=docker/dockerfile:1

FROM node:26-bookworm-slim AS frontend-builder
WORKDIR /app
RUN npm install -g pnpm@12.16.1
COPY package.json pnpm-workspace.yaml pnpm-lock.yaml biome.json ./
COPY frontend/package.json frontend/
RUN --mount=type=cache,target=/root/.local/share/pnpm/store \
    pnpm install --frozen-lockfile
COPY frontend/ frontend/
RUN pnpm --dir frontend build

FROM rust:1.100.0-slim-bookworm AS chef
ARG TARGETARCH
ARG CARGO_CHEF_VERSION=0.1.78
WORKDIR /app
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        cmake \
        curl \
        pkg-config \
        xz-utils \
    && rm -rf /var/lib/apt/lists/* \
    && case "${TARGETARCH:-amd64}" in \
        amd64) CHEF_TARGET="x86_64-unknown-linux-musl" ;; \
        arm64) CHEF_TARGET="aarch64-unknown-linux-musl" ;; \
        *) echo "Unsupported TARGETARCH: ${TARGETARCH}" >&2; exit 1 ;; \
       esac \
    && CHEF_ARCHIVE="cargo-chef-${CHEF_TARGET}.tar.xz" \
    && CHEF_URL="https://github.com/LukeMathWalker/cargo-chef/releases/download/v${CARGO_CHEF_VERSION}" \
    && curl -fsSL "${CHEF_URL}/${CHEF_ARCHIVE}" -o "/tmp/${CHEF_ARCHIVE}" \
    && curl -fsSL "${CHEF_URL}/${CHEF_ARCHIVE}.sha256" -o "/tmp/${CHEF_ARCHIVE}.sha256" \
    && (cd /tmp && sha256sum -c "${CHEF_ARCHIVE}.sha256") \
    && tar --strip-components=1 -xJf "/tmp/${CHEF_ARCHIVE}" \
        -C /usr/local/cargo/bin "${CHEF_ARCHIVE%.tar.xz}/cargo-chef" \
    && rm -f "/tmp/${CHEF_ARCHIVE}" "/tmp/${CHEF_ARCHIVE}.sha256"

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
RUN apt-get update && apt-get install -y --no-install-recommends python3 \
    && rm -rf /var/lib/apt/lists/*
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
