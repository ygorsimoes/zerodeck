<!--
  Thank you for contributing to ZeroDeck!
  Please fill out the relevant sections below to help reviewers understand and verify your changes quickly.
-->

## ✨ Summary

<!-- Briefly explain WHAT changed, WHY it is needed, and any key architectural decisions. -->

-

## 🔗 Related Issues

<!-- Link related issues using keywords like "Closes #123" or "Relates to #456". -->

-

## 🏷️ Type of Change

- [ ] 🐛 **Bug fix** — non-breaking fix resolving an unexpected behavior
- [ ] ✨ **New feature** — non-breaking capability or enhancement
- [ ] ♻️ **Refactor / Cleanup** — code quality improvement with no behavior change
- [ ] ⚡ **Performance** — measurable latency, memory, or build-time improvement
- [ ] 🔒 **Security hardening** — auth, path traversal, sandboxing, or dependency audit fix
- [ ] 🐳 **Docker / CI / Tooling** — workflow, container, or build pipeline update
- [ ] 📚 **Documentation** — guides, architecture specs, comments, or templates
- [ ] 💥 **Breaking change** — changes wire protocol, IPC schema, or public APIs

## 🧩 Affected Areas

- [ ] 🦀 **Rust Workspace** (`crates/*`)
- [ ] 🐍 **Python Plugin Host** (`hosts/python`)
- [ ] 🎮 **Frontend / TypeScript** (`frontend`)
- [ ] 🐳 **Docker / Compose** (`Dockerfile`, `compose.yaml`)
- [ ] ⚙️ **CI / Automation** (`.github/*`, `justfile`, `deny.toml`, `.devcontainer/`)

## 🧪 Verification & Quality Checklist

> [!TIP]
> Run `just check` before pushing: it validates Rust, Python, cargo-deny and the frontend inside the same Docker image CI uses. Run `just ci` for the full CI set (Python matrix, infra lint, runtime smoke test).

- [ ] 🦀 **Rust:** `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace` pass (Rust `1.100.0`)
- [ ] 🐍 **Python:** `unittest`, `ruff check hosts/python`, and `pyright --project hosts/python` (strict) pass
- [ ] 🎮 **Frontend:** `pnpm --dir frontend exec biome check .`, `tsc --noEmit`, `build`, and `node --test` pass
- [ ] 🐳 **Docker / CI:** `just ci` passes (when `Dockerfile`, `compose.yaml`, `justfile`, `.devcontainer/` or `.github/**` changed)
- [ ] 🛡️ **Safety & Licensing:** Any `unsafe` code is minimized and documented with `// SAFETY:` invariants; clean-room **MIT** boundaries are strictly respected (no GPL code copied)

## 📸 Screenshots, Logs, or Benchmarks (Optional)

<!-- Attach screenshots, benchmark numbers, or CI run highlights if applicable. -->
