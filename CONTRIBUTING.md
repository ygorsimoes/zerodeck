# Contributing to ZeroDeck

Thank you for helping build **ZeroDeck**! This guide covers everything you need to contribute cleanly, safely, and productively.

## 📜 Core Principles

1. **Clean-Room MIT Only:** ZeroDeck is licensed under the [MIT License](LICENSE). **Do not** copy, adapt, or paste GPL-licensed code from `decky-loader` or other copyleft sources. Work strictly from public protocols ([`docs/architecture/wire-protocol.md`](docs/architecture/wire-protocol.md)) and clean-room specs ([`docs/superpowers/specs/`](docs/superpowers/specs/)).
2. **Security-by-Default:** Never weaken path-jail checks (`SafePath`), atomic writes, Zip-Slip validation, SHA-256 enforcement, privilege dropping, or CORS/CSRF protections.
3. **Minimal `unsafe`:** Any `unsafe` Rust block must be strictly necessary and documented with a `// SAFETY:` comment explaining every upheld invariant.
4. **Be Respectful:** All participation is governed by our [Code of Conduct](CODE_OF_CONDUCT.md).

---

## 🛠 Development Setup

### Prerequisites

| Tool | Required Version / Notes |
|---|---|
| **Rust** | `1.100.0` (managed automatically via [`rust-toolchain.toml`](rust-toolchain.toml) with `rustfmt` & `clippy`) |
| **`just`** | Task runner for unified checks (`just --list`) |
| **Node.js & pnpm** | Node `26+` and **`pnpm@12.16.1`** (do not use `npm` or `yarn`) |
| **Python & `uv`** | Python `>=3.10` + [`uv`](https://docs.astral.sh/uv/) for `ruff` and `pyright` |
| **`cargo-deny`** | `cargo install --locked cargo-deny` for license & advisory checks |
| **Docker** *(optional)* | For `just docker-test` and `just docker-up` |

### Useful Commands

```bash
# Install frontend workspace dependencies
pnpm install --frozen-lockfile

# Format Rust + Frontend code
just fmt

# Run ALL checks (Rust fmt/clippy/test, Python tests + ruff/pyright, cargo-deny,
# Frontend biome/tsc/build/tests). Without `uv` or `cargo-deny` installed, those
# steps print a warning and are skipped locally; CI always runs them.
just check

# Run a single group on its own
just py-lint   # ruff + strict pyright
just deny      # cargo deny check

# Validate Compose & run tests inside a Linux container
docker compose config --quiet
just docker-test
```

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
2. Run `just check` locally and confirm all checks pass.
3. Fill out the [Pull Request template](.github/pull_request_template.md) accurately.
4. Keep PRs focused and small enough to review clearly.

---

## 🚀 How Releases Work

Do **not** manually edit version numbers in `Cargo.toml`, `hosts/python/pyproject.toml`, `frontend/package.json`, `Dockerfile`, or `.release-please-manifest.json`.

When changes merge into `main`, [`.github/workflows/release-please.yml`](.github/workflows/release-please.yml) automatically:
1. Opens or updates a Release PR with the next semantic version and `CHANGELOG.md`.
2. When that Release PR is merged, creates the Git tag (`vX.Y.Z`), publishes the GitHub Release, pushes the container image to `ghcr.io/ygorsimoes/zerodeck`, and attaches `zerodeck-vX.Y.Z-linux-x86_64.tar.gz` + `SHA256SUMS.txt`.
