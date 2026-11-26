# syntax=docker/dockerfile:1

# Stages:
#   toolchain / dev   pinned build + lint + test environment used by `just` and CI (default workflow)
#   fuzz              dev + nightly Rust + cargo-fuzz
#   frontend-builder, chef, planner, rust-builder
#                     release build of the frontend bundle and `zerodeckd`
#   runtime           the hardened image that ships (`docker compose up`, ghcr.io)
#
# Rust builds happen on Debian bookworm on purpose: a binary linked against bookworm's older glibc
# runs on the newer glibc that SteamOS ships, while the reverse is not true.

FROM node:26-bookworm-slim AS node

FROM ghcr.io/astral-sh/uv:0.13.10 AS uv

FROM rust:1.100.0-slim-bookworm AS toolchain
ARG TARGETARCH
ARG CARGO_DENY_VERSION=0.22.1
ARG CARGO_DENY_SHA256_AMD64=13f7e825e9cb811c2f8de63f29b0f45851dfb53004029f0f2501af524728364d
ARG CARGO_DENY_SHA256_ARM64=0bac0838a1653ce693e4602b8634d15ca179edd2198e438af9c35ab1c551d509
ARG CARGO_LLVM_COV_VERSION=0.9.2
ARG CARGO_LLVM_COV_SHA256_AMD64=8d863ab2df1fb53d4f41052b3fa4627ac003d00c46a6388ce037d17472c504ef
ARG CARGO_LLVM_COV_SHA256_ARM64=dab1bfd1f25f7fcd3d6fc6abf9d71b40217a7a103e1da88c1a23111960aa4a40
ARG JUST_VERSION=1.59.0
ARG JUST_SHA256_AMD64=409044617b6f06517c1035e55ca38c8c856ff8b650e41bc2aa1ef82d5903a23e
ARG JUST_SHA256_ARM64=23871b1702bfd550fb5f901778fd4290ecf2ee034a45a889fbe96d89b188086d
ARG PNPM_VERSION=12.16.1
ARG PYRIGHT_VERSION=1.1.414
ARG RUFF_VERSION=0.17.6
SHELL ["/bin/bash", "-o", "pipefail", "-c"]

RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        cmake \
        curl \
        git \
        pkg-config \
        python3 \
        python3-venv \
        xz-utils \
    && rm -rf /var/lib/apt/lists/* \
    && rustup component add rustfmt clippy llvm-tools-preview

# Node.js + npm from the official image (same major as the frontend build), then pnpm and pyright.
COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=node /usr/local/lib/node_modules /usr/local/lib/node_modules
RUN ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
    && ln -s ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx \
    && npm install --global --no-fund --no-audit "pnpm@${PNPM_VERSION}" "pyright@${PYRIGHT_VERSION}" \
    && npm cache clean --force

# ruff through uv, installed into a shared tool dir so every user can run it.
COPY --from=uv /uv /uvx /usr/local/bin/
RUN UV_TOOL_DIR=/opt/uv-tools UV_TOOL_BIN_DIR=/usr/local/bin UV_PYTHON_DOWNLOADS=never \
    uv tool install "ruff==${RUFF_VERSION}"

# Prebuilt cargo-deny, cargo-llvm-cov and just, verified against SHA-256 digests pinned in this
# file (fail closed).
RUN case "${TARGETARCH:-amd64}" in \
        amd64) triple="x86_64-unknown-linux-musl"; deny_sha="${CARGO_DENY_SHA256_AMD64}"; just_sha="${JUST_SHA256_AMD64}"; cov_sha="${CARGO_LLVM_COV_SHA256_AMD64}" ;; \
        arm64) triple="aarch64-unknown-linux-musl"; deny_sha="${CARGO_DENY_SHA256_ARM64}"; just_sha="${JUST_SHA256_ARM64}"; cov_sha="${CARGO_LLVM_COV_SHA256_ARM64}" ;; \
        *) echo "Unsupported TARGETARCH: ${TARGETARCH}" >&2; exit 1 ;; \
    esac \
    && tmp="$(mktemp -d)" \
    && deny="cargo-deny-${CARGO_DENY_VERSION}-${triple}" \
    && curl -fsSL -o "${tmp}/deny.tar.gz" \
        "https://github.com/EmbarkStudios/cargo-deny/releases/download/${CARGO_DENY_VERSION}/${deny}.tar.gz" \
    && echo "${deny_sha}  ${tmp}/deny.tar.gz" | sha256sum -c - \
    && tar -xzf "${tmp}/deny.tar.gz" -C /usr/local/bin --strip-components=1 "${deny}/cargo-deny" \
    && curl -fsSL -o "${tmp}/just.tar.gz" \
        "https://github.com/casey/just/releases/download/${JUST_VERSION}/just-${JUST_VERSION}-${triple}.tar.gz" \
    && echo "${just_sha}  ${tmp}/just.tar.gz" | sha256sum -c - \
    && tar -xzf "${tmp}/just.tar.gz" -C /usr/local/bin just \
    && curl -fsSL -o "${tmp}/llvm-cov.tar.gz" \
        "https://github.com/taiki-e/cargo-llvm-cov/releases/download/v${CARGO_LLVM_COV_VERSION}/cargo-llvm-cov-${triple}.tar.gz" \
    && echo "${cov_sha}  ${tmp}/llvm-cov.tar.gz" | sha256sum -c - \
    && tar -xzf "${tmp}/llvm-cov.tar.gz" -C /usr/local/bin cargo-llvm-cov \
    && rm -rf "${tmp}" \
    && cargo deny --version && cargo llvm-cov --version && just --version && ruff --version && pyright --version && pnpm --version

# Day-to-day development image: `just <recipe>` runs here (see compose.yaml `dev`). The user takes
# the host UID/GID so files written into the bind-mounted checkout keep the right owner.
FROM toolchain AS dev
ARG DEV_UID=1000
ARG DEV_GID=1000
RUN groupadd --non-unique --gid "${DEV_GID}" dev \
    && useradd --non-unique --uid "${DEV_UID}" --gid "${DEV_GID}" --create-home --shell /bin/bash dev \
    && mkdir -p /workspace/target /workspace/node_modules /workspace/frontend/node_modules \
        /workspace/fuzz/target /home/dev/.cargo /home/dev/.local/share/pnpm \
    && chown -R dev:dev /workspace /home/dev
ENV CARGO_HOME=/home/dev/.cargo \
    PATH=/home/dev/.cargo/bin:/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    CARGO_TERM_COLOR=always \
    ZERODECK_IN_CONTAINER=1
USER dev
WORKDIR /workspace
CMD ["bash"]

# Fuzzing image: libFuzzer needs a nightly compiler, kept out of the everyday `dev` image.
FROM dev AS fuzz
ARG CARGO_FUZZ_VERSION=0.13.2
USER root
RUN CARGO_HOME=/usr/local/cargo rustup toolchain install nightly --profile minimal \
    && CARGO_HOME=/tmp/cargo-install cargo install --locked --root /usr/local \
        "cargo-fuzz@${CARGO_FUZZ_VERSION}" \
    && rm -rf /tmp/cargo-install
USER dev

FROM node AS frontend-builder
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
SHELL ["/bin/bash", "-o", "pipefail", "-c"]
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
    && echo "$(cut -d' ' -f1 "/tmp/${CHEF_ARCHIVE}.sha256")  /tmp/${CHEF_ARCHIVE}" | sha256sum -c - \
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

FROM debian:bookworm-slim AS runtime
# x-release-please-start-version
ARG VERSION=0.1.0
# x-release-please-end
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
    ZERODECK_ALLOW_NON_LOOPBACK=1 \
    SERVER_PORT=1337 \
    RUST_LOG=info

USER zerodeck
WORKDIR /homebrew
EXPOSE 1337
STOPSIGNAL SIGTERM

HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD ["/usr/local/bin/zerodeckd", "--healthcheck"]

ENTRYPOINT ["/usr/local/bin/zerodeckd"]
