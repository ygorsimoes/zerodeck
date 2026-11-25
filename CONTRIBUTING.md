# Contributing to ZeroDeck

Thank you for helping build **ZeroDeck**! This guide covers everything you need to contribute cleanly, safely, and productively.

## 📜 Core Principles

1. **Clean-Room MIT Only:** ZeroDeck is licensed under the [MIT License](LICENSE). **Do not** copy, adapt, or paste GPL-licensed code from `decky-loader` or other copyleft sources. Work strictly from public protocols ([`docs/architecture/wire-protocol.md`](docs/architecture/wire-protocol.md)) and clean-room specs ([`docs/superpowers/specs/`](docs/superpowers/specs/)).
2. **Security-by-Default:** Never weaken path-jail checks (`SafePath`), atomic writes, Zip-Slip validation, SHA-256 enforcement, privilege dropping, or CORS/CSRF protections.
3. **Minimal `unsafe`:** Any `unsafe` Rust block must be strictly necessary and documented with a `// SAFETY:` comment explaining every upheld invariant.
4. **Be Respectful:** All participation is governed by our [Code of Conduct](CODE_OF_CONDUCT.md).

---

## 🛠 Development Setup

**Docker is the default way to develop ZeroDeck.** Every `just` recipe runs inside a pinned dev
container (the `dev` stage of the [`Dockerfile`](Dockerfile)): Rust `1.100.0`, cargo-deny, Node
`26` + pnpm `12.16.1`, Python 3 + ruff + pyright, and `just`. CI runs the **same recipes in the
same image**, so a green `just check` on your machine means a green CI. The toolchain is fixed in
the repository, so results don't depend on what is installed on your machine.

### Prerequisites

| Tool | Notes |
|---|---|
| **Docker** | Docker Engine or Docker Desktop, with Compose v2.20 or newer (`docker compose`) |
| **[`just`](https://just.systems)** | 1.39 or newer (`brew install just`, `cargo install just`, or your package manager) |

That's all. Run `just` to list every recipe.

### Everyday commands

```bash
just setup          # once (and after toolchain changes): build the dev image, install frontend deps
just check          # core checks: fmt, clippy, tests, Python lint/tests, cargo-deny, frontend
just ci             # everything CI runs except fuzzing: check + Python matrix + lint-infra + smoke
                    # + test-privileged (root-only tests, always in the root container)
just fmt            # format Rust + frontend code
just lint           # lints only (no tests)
just test           # tests only (Rust, Python worker host, frontend)
just dev            # run zerodeckd from source on 127.0.0.1:1337 (home: ./.dev-homebrew)
just shell          # shell inside the dev container; `just <recipe>` works there too
just clean          # drop the dev containers and their cache volumes
```

Focused checks:

```bash
just check-rust     # rustfmt, clippy -D warnings, cargo test
just deny           # deny.toml: licenses, advisories, sources
just check-python   # ruff, strict pyright, unittest
just py-test-on 3.10  # Python worker host tests on one interpreter (official python image)
just py-matrix      # ...on every supported interpreter: 3.10, 3.11, 3.12, 3.13
just check-frontend # biome, tsc, vite build, node tests
just lint-infra     # version pins in sync, compose config, actionlint + zizmor (workflows), hadolint
just fuzz zip_extract 120   # fuzz one untrusted-input parser (nightly, in its own image)
just fuzz-all 30    # every fuzz target, 30 s each
```

The shipped runtime image:

```bash
just up / just down / just logs   # hardened container on 127.0.0.1:1337
just smoke                        # build, wait until healthy, probe /healthz, stop
just dist v0.0.0-local            # release tarball + SHA256SUMS into dist/ (as the release job does)
```

### Editor integration (Dev Containers)

[`.devcontainer/devcontainer.json`](.devcontainer/devcontainer.json) opens the repository inside
the same `dev` service, for VS Code ("Reopen in Container"), GitHub Codespaces or any Dev
Containers client. rust-analyzer, ruff, pyright and Biome then run against the pinned toolchain,
and `just <recipe>` in its terminal runs directly (it is already inside the container).

### How it works

- Your checkout is bind-mounted at `/workspace`; `target/`, `node_modules/` and the cargo/pnpm
  caches live in named Docker volumes, so host builds (e.g. macOS binaries) never mix with the
  container's Linux ones and rebuilds stay incremental. `just clean` drops those volumes.
- The container user takes your UID/GID, so files it writes (formatting, `frontend/dist`) stay
  yours. Dev containers run with no Linux capabilities and `no-new-privileges`.
- Each public recipe forwards to a private `_recipe`: in Docker by default, directly when
  `ZERODECK_NATIVE=1` is set or when already inside the container (`ZERODECK_IN_CONTAINER=1`).
- Containers use your machine's native architecture for speed. The Steam Deck is `x86_64`; to
  check on exactly that architecture (slower under emulation on Apple Silicon), run
  `DOCKER_DEFAULT_PLATFORM=linux/amd64 just check`. CI always runs on `x86_64`.
- Rust is built on Debian bookworm on purpose: a binary linked against its older glibc runs on
  SteamOS's newer one, but not the other way around.

### CI

[`.github/workflows/ci.yml`](.github/workflows/ci.yml) runs one `just` recipe per job, inside the
`dev` image built by [`.github/actions/dev-image`](.github/actions/dev-image/action.yml) (layer
cache + cargo caches). Jobs marked **required** gate merges into `main`.

| Job | Recipe | Required |
|---|---|:---:|
| `rust` | `just check-rust` | ✅ |
| `deny` | `just deny` | ✅ |
| `python` (after `python-tests`) | `just check-python` (+ `just py-test-on <3.10–3.13>`) | ✅ |
| `frontend` | `just check-frontend` | ✅ |
| `docker` | runtime image build, `just smoke --no-build`, `just dist` dry run | ✅ |
| `infra` | `just lint-infra` | |
| `fuzz` | `just fuzz-all 30` | |

### Updating tools and dependencies

- **Dependabot** opens weekly PRs (after a 7-day cooldown) for Cargo, npm, GitHub Actions
  (workflows and `.github/actions/*`) and Docker base images (including `uv`; not `rust`, see below).
- **Tools pinned in the `Dockerfile`** (cargo-deny, just, ruff, pyright, cargo-fuzz) are bumped
  by hand: change the `ARG …_VERSION` and, for downloaded binaries, the matching
  `ARG …_SHA256_*` digests from the release's checksum files. The build fails closed on a mismatch.
- **Rust toolchain:** change `rust-toolchain.toml`, every `rust:<version>` image in the
  `Dockerfile`, and `rust-version` in `Cargo.toml` together. **pnpm:** `package.json`
  (`packageManager`) and the `Dockerfile`. **just:** `JUST_VERSION` and the `just@` pins in
  `.github/`. `just lint-infra` fails when any of these drift apart.
- GitHub Actions are pinned by full commit SHA with the version in a comment, and workflow
  permissions are granted per job (`zizmor` enforces it).

### Running natively (optional)

You can run the same recipes on your own toolchain by setting `ZERODECK_NATIVE=1`:

```bash
ZERODECK_NATIVE=1 just check
```

Then you need Rust (via `rustup`; [`rust-toolchain.toml`](rust-toolchain.toml) selects the
version), Node `26+` with `pnpm@12.16.1` (never `npm` or `yarn`), Python `>=3.10`, and optionally
[`uv`](https://docs.astral.sh/uv/) (ruff/pyright) and `cargo-deny`. Without `uv` or `cargo-deny`
those steps print a warning and are skipped, which is one reason Docker is the recommended path.
Recipes that are Docker by nature (`lint-infra`, `py-test-on`, `py-matrix`, `smoke`, `up`, `dist`)
still need Docker.

---

## 🔀 Pull Request & Commit Conventions

This repository uses **Squash Merging** and **Conventional Commits** to power automated releases via [Release Please](https://github.com/googleapis/release-please).

### PR Titles (Enforced by CI)

Every PR title must follow [Conventional Commits](https://www.conventionalcommits.org/), validated automatically by [`.github/workflows/pr-lint.yml`](.github/workflows/pr-lint.yml):

```text
<type>(<optional scope>): <short imperative summary>
```

Common types:
- `feat(server): add plugin hot-reload endpoint` → triggers a feature release bump (`✨ Features`)
- `fix(core): reject symlink escapes in archive extraction` → triggers a patch bump (`🐛 Bug Fixes`)
- `perf(platform): avoid extra stat syscall in SafePath` → triggers a patch bump (`⚡ Performance Improvements`)
- `docs(readme): document systemd deployment flags` → updates `📚 Documentation`
- `refactor`, `test`, `build`, `ci`, `chore` → kept out of user-facing release notes
- Breaking changes (pre-1.0 minor bump): append `!` (e.g., `feat(protocol)!: ...`) or include a `BREAKING CHANGE:` footer.

### Before Opening a PR

1. Search [existing issues](https://github.com/ygorsimoes/zerodeck/issues) and review [`docs/architecture/wire-protocol.md`](docs/architecture/wire-protocol.md) if touching wire/IPC contracts.
2. Run `just check` (in Docker, the default) and confirm all checks pass. If you touched the `Dockerfile`, `compose.yaml`, the `justfile` or `.github/`, run `just ci`.
3. Fill out the [Pull Request template](.github/pull_request_template.md) accurately.
4. Keep PRs focused and small enough to review clearly.

---

## 🚀 How Releases Work

Do **not** manually edit version numbers in `Cargo.toml`, `hosts/python/pyproject.toml`, `frontend/package.json`, `Dockerfile`, or `.release-please-manifest.json`.

When changes merge into `main`, [`.github/workflows/release-please.yml`](.github/workflows/release-please.yml) automatically:
1. Opens or updates a Release PR with the next semantic version and `CHANGELOG.md`.
2. When that Release PR is merged, creates the Git tag (`vX.Y.Z`), publishes the GitHub Release, pushes the container image to `ghcr.io/ygorsimoes/zerodeck`, and attaches `zerodeck-vX.Y.Z-linux-x86_64.tar.gz` + `SHA256SUMS.txt`.
