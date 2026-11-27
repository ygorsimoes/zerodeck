<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/banner-dark.svg" />
  <source media="(prefers-color-scheme: light)" srcset="docs/assets/banner-light.svg" />
  <img alt="ZeroDeck — Native. Fast. Built in Rust. The next-generation plugin loader for Steam Deck." src="docs/assets/banner-dark.svg" width="100%" />
</picture>

<br />
<br />

**ZeroDeck is a native, Rust-powered plugin loader for the Steam Deck,<br />built to run the Decky plugins you already use, on a lighter and safer core.**

[![CI](https://img.shields.io/github/actions/workflow/status/ygorsimoes/zerodeck/ci.yml?branch=main&style=flat-square&label=CI)](https://github.com/ygorsimoes/zerodeck/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/rust-1.100-FF5A1F?style=flat-square&logo=rust&logoColor=white)](rust-toolchain.toml)
[![License: MIT](https://img.shields.io/badge/license-MIT-111111?style=flat-square)](LICENSE)
[![Status: pre-alpha](https://img.shields.io/badge/status-pre--alpha-8A867E?style=flat-square)](#roadmap)

[Why](#why-zerodeck) · [Performance](#performance) · [Quick start](#quick-start) · [Steam Deck](#on-steam-deck) · [How it works](#how-it-works) · [Roadmap](#roadmap) · [Contributing](#contributing) · [FAQ](#faq)

</div>

> [!IMPORTANT]
> **ZeroDeck is pre-alpha.** The daemon runs on Linux and macOS today and already uses Decky's
> wire format, but it can't load plugins into Steam yet. That lands in
> [Phase 4](#roadmap). Until then, keep Decky Loader on your Deck.

## Why ZeroDeck

[Decky Loader](https://github.com/SteamDeckHomebrew/decky-loader) built the Steam Deck plugin
ecosystem, and every plugin author and user owes it a lot. ZeroDeck is the next step: the same
plugin contracts, rebuilt from scratch as a native daemon.

- **Native.** A single Rust binary on `tokio` and `axum`. No bundled interpreter in the core.
- **Fast.** Ready to serve in milliseconds, idling at a few megabytes of RAM. [See the numbers](#performance).
- **Compatible.** Targets the contracts plugins already use (`plugin.json`, `package.json`,
  `@decky/api`, `@decky/ui`, `main.py` and the `decky` module), so existing plugins run unmodified.
- **Hardened by default.** Path jails, atomic writes, verified downloads and per-plugin isolation
  are part of the design, not add-ons.
- **MIT, clean-room.** Written only from public protocols and plugin contracts. No Decky Loader code
  is copied, and [`cargo-deny`](deny.toml) blocks copyleft dependencies in CI.

| Capability | Status |
|---|:---:|
| HTTP + WebSocket server using Decky's message format, with an ephemeral auth token | ✅ |
| CORS locked to Steam's UI origin (`https://steamloopback.host`) | ✅ |
| Plugin discovery from `plugin.json` / `package.json` (legacy and ES module plugins) | ✅ |
| `SafePath` jail: canonicalized paths that can't escape their root, symlinks included | ✅ |
| Atomic settings writes (temp file + `fsync` + rename): no corrupted JSON on power loss | ✅ |
| Zip-Slip-safe extraction and fail-closed SHA-256 verification for downloads | ✅ |
| Python worker with `decky` / `decky_plugin`, `settings`, and `helpers` compatibility | ✅ |
| Supervised plugin processes with restart, backoff, and crash-loop guard | ✅ |
| Private plugin IPC over inherited `socketpair()` FDs (no `/tmp` sockets) | ✅ |
| Plugins drop to the `deck` user unless they declare `"root"` | ✅ |
| Plugin static, asset and data routes behind the path jail | ✅ |
| SSRF-guarded `/fetch` CORS proxy, `FilePicker`, and staged ZIP/`remote_binary` installer | ✅ |
| WebSocket `FULL_SYNC` deduplication and `"debug"` plugin hot-reload | ✅ |
| Injection into Steam's UI and the Quick Access Menu | 🚧 |

✅ shipped and tested · 🚧 in progress (see [Roadmap](#roadmap))

## Performance

| Metric | `zerodeckd` |
|---|---|
| Release binary | **8.4 MiB** |
| Time from launch to first `/healthz` reply | **~13 ms** |
| Memory at idle (RSS) | **~8 MB** |

Measured on an Apple M4 (macOS, arm64) with a release build and an empty plugin directory;
median of five runs. Steam Deck numbers and a side-by-side with Decky Loader on the same device
are coming with [Phase 4](#roadmap).

Reproduce it yourself with a native release build (outside Docker, so no VM or container overhead
is measured):

```bash
cargo build --release
ls -lh target/release/zerodeckd
time (./target/release/zerodeckd & until curl -sf http://127.0.0.1:1337/healthz; do :; done)
ps -o rss= -p "$(pgrep -n zerodeckd)"   # KiB
```

## Quick start

ZeroDeck runs on any Linux or macOS machine today. Pick whichever path suits you.

### With Docker

The fastest way to try it. Runs the hardened container (read-only filesystem, all capabilities
dropped, unprivileged user).

```bash
git clone https://github.com/ygorsimoes/zerodeck.git
cd zerodeck
docker compose up -d --build            # or: just up
curl http://127.0.0.1:1337/healthz      # {"status":"ok"}
```

- **Network exposure:** the container binds `0.0.0.0` internally (`ZERODECK_ALLOW_NON_LOOPBACK=1`), so always publish the port on loopback only (`-p 127.0.0.1:1337:1337`, as `compose.yaml` does); publishing it on all interfaces would expose the auth token to your network.
- **Update:** `git pull && docker compose up -d --build`
- **Stop:** `docker compose down` (or `just down`)
- **Logs:** `docker compose logs -f zerodeckd` (or `just logs`)

Plugins go in `.dev-homebrew/plugins/` next to the repository. To use another host port, set
`ZERODECK_PORT` (for example `ZERODECK_PORT=1338 just up`).

### From source

With Docker and [`just`](https://just.systems) installed, `just dev` builds and runs `zerodeckd`
from your checkout inside the pinned dev container, the same toolchain CI uses. Nothing else
needs to be installed.

```bash
git clone https://github.com/ygorsimoes/zerodeck.git
cd zerodeck
just dev                                # listens on 127.0.0.1:1337, home in ./.dev-homebrew
```

Prefer your own toolchain? You need Rust (the toolchain in
[`rust-toolchain.toml`](rust-toolchain.toml) installs itself through `rustup`):

```bash
cargo run --release --bin zerodeckd     # listens on 127.0.0.1:1337
```

- **Update:** `git pull`, then `just dev` again (or `cargo build --release`)

### See it discover a plugin

`just dev`, `just up` and `docker compose up` all use `.dev-homebrew/` in the repository as the
daemon's home, so drop a plugin there and start the daemon:

```bash
mkdir -p .dev-homebrew/plugins/hello
echo '{"name": "Hello", "author": "you"}' > .dev-homebrew/plugins/hello/plugin.json
just dev
```

On your own toolchain, point `UNPRIVILEGED_PATH` at it instead:
`UNPRIVILEGED_PATH=.dev-homebrew cargo run --release --bin zerodeckd`.

In another terminal:

```bash
TOKEN=$(curl -s http://127.0.0.1:1337/auth/token)
curl -s -X POST -H "X-Decky-Auth: $TOKEN" http://127.0.0.1:1337/methods/get_plugins
# {"result":["Hello"],"success":true}
```

### From a release

Starting with the first release, every
[GitHub release](https://github.com/ygorsimoes/zerodeck/releases) ships a manual `linux-x86_64`
tarball (daemon and controller binaries, Python worker, frontend bundle, systemd units and
documentation) and a root-level `.managed.tar.gz` generation archive for signed-channel updates,
both with `SHA256SUMS.txt`, plus a container image at `ghcr.io/ygorsimoes/zerodeck`. Both archives
carry signed build-provenance attestations, and the image also ships an SBOM. Verify them with the
GitHub CLI:

```bash
gh attestation verify zerodeck-v*-linux-x86_64.tar.gz --repo ygorsimoes/zerodeck
gh attestation verify zerodeck-v*-linux-x86_64.managed.tar.gz --repo ygorsimoes/zerodeck
```

A managed first install or conversion is deliberately explicit: `bootstrap.sh` requires the archive
SHA-256 digests from the release, refuses a running Decky or an unreviewed existing ZeroDeck install,
and starts nothing without `--start`. After that, updates follow the signed `stable` channel on
their own. See [`deploy/update/RELEASE.md`](deploy/update/RELEASE.md) for the exact commands,
conflict behavior, conversion backup and recovery, and how channels are signed.

<details>
<summary><strong>Configuration</strong></summary>
<br />

| Flag | Environment variable | Default | Description |
|---|---|---|---|
| `--host` | `SERVER_HOST` | `127.0.0.1` | Address to bind the HTTP/WebSocket server to |
| `--allow-non-loopback` | `ZERODECK_ALLOW_NON_LOOPBACK` | off | Allow binding to a non-loopback address (exposes the auth token to that network) |
| `--allow-unsafe-root-plugins` | `ZERODECK_ALLOW_UNSAFE_ROOT_PLUGINS` | off | Start `"root"` plugins whose files a non-root user can modify (development only; set it in a systemd drop-in, which only root can) |
| `--port` | `SERVER_PORT` | `1337` | Port to listen on |
| `--check-config` | | | Validate paths, settings and plugin manifests, then exit |
| `--healthcheck` | | | Probe `/healthz` on a running daemon and exit `0` or `1` |
| | `UNPRIVILEGED_PATH` (or `DECKY_HOME`) | `<user home>/homebrew` | Root for `settings/`, `data/` and `logs/` |
| | `DECKY_USER_HOME` | `$HOME` | User home used for the default paths |
| | `PRIVILEGED_PATH` | `UNPRIVILEGED_PATH` | Root for `plugins/` and `services/` |
| | `ZERODECK_FRONTEND_DIR` | auto-detected | Directory containing the `@zerodeck/frontend` bundle |
| | `PYTHONPATH` | auto-detected | Override search path containing `zerodeck_py_host` |
| | `RUST_LOG` | `info` | Log filter, e.g. `zerodeck_server=debug` |

Without `ZERODECK_FRONTEND_DIR` / `PYTHONPATH`, the frontend and `zerodeck_py_host` are searched
in `/opt/zerodeck/{frontend,python}` and next to the binary (`<exe>/../{frontend,python}`). Debug builds also try the working directory
and the source tree; release builds never do, because those can be writable by a non-root user. If
`zerodeck_py_host` cannot be imported, the daemon still starts and serves the UI, logs one error,
and every backend plugin shows as `Failed` with the reason in `failure_reason`.

Plugins flagged `"root"` keep root privileges, so a root daemon only starts them from a tree that
only root can modify: `plugins/` and every file and directory of the plugin must be owned by root,
must not be group- or world-writable, and must not be symlinks. Store installs already produce such
trees. Otherwise the plugin shows as `Failed` with the offending path in `failure_reason`. For local
development you can skip the check with `--allow-unsafe-root-plugins` /
`ZERODECK_ALLOW_UNSAFE_ROOT_PLUGINS=1`, set in the daemon's own command line or a systemd drop-in.
It is deliberately not a `loader.json` setting: `settings/` belongs to the unprivileged user, who
must not be able to grant themselves root.

</details>

## On Steam Deck

> [!NOTE]
> 🚧 **Implemented; real Steam Deck qualification is still being recorded.** The managed installer
> and updater are covered by unit, fault-injection, packaging and VM tests (a KVM VM runs in CI),
> but SteamOS-specific behavior is only confirmed on hardware, tracked in
> [`deploy/update/COMPATIBILITY.md`](deploy/update/COMPATIBILITY.md).

- **Install:** a verified `bootstrap.sh` installs `zerodeckd` as a managed systemd service that uses
  your existing `~/homebrew/plugins`.
- **Update:** merging the Release Please PR publishes an immutable release and CI signs the channel
  automatically. On the Deck, `zerodeckctl update` verifies the signed channel and stages the
  release; `zerodeckctl upgrade` activates it explicitly, with offline rollback to the retained
  generation. Nothing is signed or promoted by hand.
- **Recovery:** an interrupted activation restores the previous generation or leaves a recoverable
  stopped state; `zerodeckctl recover` reconciles a request left by an interrupted helper.

Want it sooner? [Help build it](#contributing).

## How it works

```mermaid
flowchart LR
    Steam["Steam UI (CEF)"] <-->|"HTTP + WebSocket"| Daemon["zerodeckd (Rust)"]
    Daemon <-->|"JSON lines over socketpair"| Host["Python worker"]
    Host --> Plugin["Decky plugin (main.py)"]
    Daemon --> FS[("~/homebrew")]
```

`zerodeckd` talks to Steam's UI over the same HTTP and WebSocket protocol Decky plugins expect,
and runs each plugin's Python backend in its own worker process. The full protocol (WebSocket
frames, IPC messages, manifests and environment variables) is documented in
[`docs/architecture/wire-protocol.md`](docs/architecture/wire-protocol.md).

<details>
<summary><strong>Repository layout</strong></summary>
<br />

| Path | What it does |
|---|---|
| [`crates/zerodeckd`](crates/zerodeckd) | The daemon binary: CLI, logging, debug watcher, graceful shutdown |
| [`crates/zerodeck-server`](crates/zerodeck-server) | HTTP/WebSocket server, auth token, CORS, `/fetch` proxy, static/plugin routes, `FULL_SYNC` cache |
| [`crates/zerodeck-core`](crates/zerodeck-core) | Settings, plugin lifecycle, installer, `FilePicker`, outbound HTTP policy, debug hot-reload |
| [`crates/zerodeck-plugin-host`](crates/zerodeck-plugin-host) | Plugin runtime trait and Python worker launcher |
| [`crates/zerodeck-cef`](crates/zerodeck-cef) | Chrome DevTools Protocol client for Steam's UI |
| [`crates/zerodeck-platform`](crates/zerodeck-platform) | Paths, `SafePath` jail, atomic writes, process identity |
| [`crates/zerodeck-protocol`](crates/zerodeck-protocol) | Wire and IPC message types, manifest schemas |
| [`hosts/python`](hosts/python) | `zerodeck-py-host`, the stdlib-only Python plugin worker |
| [`frontend`](frontend) | `@zerodeck/frontend`, the TypeScript bundle injected into Steam |
| [`deploy/systemd`](deploy/systemd) | `zerodeck.service` unit template |
| [`fuzz`](fuzz) | cargo-fuzz targets for the untrusted-input parsers (separate workspace) |
| [`Dockerfile`](Dockerfile) · [`compose.yaml`](compose.yaml) | Pinned dev/CI toolchain image, fuzz image, and the hardened runtime image |
| [`justfile`](justfile) | Every development, CI and release task (`just --list`) |
| [`.devcontainer`](.devcontainer) | Dev Containers / Codespaces setup on top of the `dev` image |
| [`docs`](docs) | Wire protocol reference and design records |

</details>

## Roadmap

- [x] **Phase 1 · Foundation:** workspace, wire protocol types, path jail, atomic settings,
  safe archive extraction, plugin discovery, HTTP/WebSocket server, CI and releases
- [x] **Phase 2 · Plugin host:** privilege drop, `socketpair()` IPC, process supervisor,
  `zerodeck-py-host` parity (`settings`, `helpers`, v0/v1), and async `/ws` RPC wiring
- [x] **Phase 3 · Full server:** `FULL_SYNC`, `/fetch` proxy, plugin static and data routes, file
  picker, staged ZIP/`remote_binary` installer, and debug hot-reload
- [ ] **Phase 4 · Steam integration:** CEF injection, crash recovery, Quick Access Menu UI,
  one-command install and update on Steam Deck

## Contributing

ZeroDeck is early, which means the architecture is still open to good ideas and most of the
interesting work is still ahead. Rust, Python and TypeScript are all welcome.

1. Read [`CONTRIBUTING.md`](CONTRIBUTING.md) for setup, commit conventions and the clean-room rules.
2. Pick something from
   [`good first issue`](https://github.com/ygorsimoes/zerodeck/issues?q=is%3Aissue+is%3Aopen+label%3A%22good+first+issue%22)
   or a 🚧 row in the table above.
3. Make sure everything passes before opening a pull request. With Docker and `just` installed,
   this runs the checks inside the same pinned container CI uses:

```bash
just setup   # once: builds the dev image and installs frontend dependencies
just check   # formatting, lints, rustdoc, tests and licenses
just ci      # optional: everything CI runs (adds the Python matrix, infra lint, smoke test,
             # root-only `just test-privileged`)
just coverage  # optional: Rust coverage report in ./coverage/html
```

Found a vulnerability? Report it privately as described in [`SECURITY.md`](SECURITY.md). Everyone
taking part is expected to follow the [Code of Conduct](CODE_OF_CONDUCT.md). AI coding agents
should also read [`AGENTS.md`](AGENTS.md).

## FAQ

<details>
<summary><strong>Will my Decky plugins work?</strong></summary>
<br />

ZeroDeck already discovers plugins, runs their Python backends, and serves their routes locally,
though injecting plugins into Steam's UI lands in [Phase 4](#roadmap).

</details>

<details>
<summary><strong>Can I run ZeroDeck alongside Decky Loader?</strong></summary>
<br />

Not on the same port: both listen on `127.0.0.1:1337`. For testing, run ZeroDeck on a desktop or
pass `--port` to pick another one.

</details>

<details>
<summary><strong>Why rewrite it instead of contributing to Decky Loader?</strong></summary>
<br />

Moving the core to a native language, changing the process model and relicensing under MIT are
foundational changes, not patches. ZeroDeck keeps the plugin contracts so the ecosystem Decky
built carries over.

</details>

<details>
<summary><strong>Is ZeroDeck affiliated with Valve or Decky Loader?</strong></summary>
<br />

No. ZeroDeck is an independent project.

</details>

## License

ZeroDeck is released under the [MIT License](LICENSE).

Thanks to the [Decky Loader](https://github.com/SteamDeckHomebrew/decky-loader) team and every
plugin author: you built the ecosystem ZeroDeck is here to carry forward.
