# PR 4 — Safety nets (coverage, scheduled fuzzing, rustdoc, docs) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add coverage reporting, nightly fuzzing, a rustdoc gate, and a final documentation pass (I2–I5).

**Architecture:** Every new check is a `just` recipe forwarding to a private `_recipe`, run in the
pinned dev image; CI only calls recipes. New tools are pinned in the `Dockerfile` with version
`ARG`s and SHA-256 digests; new Actions are pinned by commit SHA.

**Tech Stack:** Docker, just, GitHub Actions, cargo-llvm-cov, cargo-fuzz.

**Spec:** `docs/superpowers/specs/2026-11-25-general-improvements-design.md` (PR 4 section, I2–I5)

**Branch:** `ci/safety-nets` from `main` after PR 3 is merged.

## Global Constraints

- Downloaded binaries: version `ARG` + per-arch SHA-256, fail closed (`sha256sum -c`).
- Actions pinned by full commit SHA with the version in a trailing comment; workflow-level `permissions` read-only or `{}`, per-job `permissions` declared; `persist-credentials: false`.
- Required check job ids `rust`, `deny`, `python`, `frontend`, `docker` unchanged; new jobs are not required.
- No third-party coverage/reporting service (nothing leaves the CI run except artifacts).
- This PR touches `Dockerfile`, `justfile`, `.github/**`: finish with `just ci`; `just lint-infra` must pass (actionlint, zizmor, hadolint).

## Review Focus

1. Coverage instrumentation must not leak into the normal `target/` used by `just check` (stale instrumented artifacts) — Task 1 writes to `target/llvm-cov-target` (cargo-llvm-cov default) and the check is re-run after.
2. The nightly fuzz job must not run on forks without the cache (schedule runs only on the default branch) — Task 2 guards with `if: github.repository == 'ygorsimoes/zerodeck'`.
3. A fuzz crash must leave a downloadable reproducer — Task 2 uploads `fuzz/artifacts/**` on failure.
4. Rustdoc warnings from private items must not block (`--no-deps`, default visibility) — Task 3 verifies on the current tree first.
5. Docs must describe every flag/env/recipe added in PRs 1–3 — Task 4 checklist.

---

### Task 1: Coverage (I2)

**Files:**
- Modify: `Dockerfile` (`toolchain` stage: `rustup component add llvm-tools-preview`; `ARG CARGO_LLVM_COV_VERSION`, `ARG CARGO_LLVM_COV_SHA256_AMD64`, `ARG CARGO_LLVM_COV_SHA256_ARM64`; download the `cargo-llvm-cov-<triple>.tar.gz` release asset the same way as `cargo-deny`)
- Modify: `justfile` (`coverage` → `_coverage`), `.github/workflows/ci.yml` (job `coverage`), `AGENTS.md` §6/§7

**Interfaces:**
- Produces: `just coverage` → `_coverage`: `cargo llvm-cov --workspace --locked --html --output-dir target/coverage` then `cargo llvm-cov report --lcov --output-path target/coverage/lcov.info`.

- [ ] **Step 1:** Look up the latest cargo-llvm-cov release and its published SHA-256 for both triples; pin them.
- [ ] **Step 2:** Add the recipe and CI job (`actions/upload-artifact` pinned by SHA, artifact `coverage`, path `target/coverage`).
- [ ] **Step 3:** `just setup && just coverage` → `target/coverage/html/index.html` exists; `just check` → PASS afterwards.
- [ ] **Step 4: Commit** — `ci: add cargo-llvm-cov coverage recipe and artifact job`

### Task 2: Nightly fuzzing (I3)

**Files:**
- Create: `.github/workflows/fuzz-nightly.yml`
- Modify: `justfile` (`_fuzz`/`_fuzz-all` accept an optional corpus dir: keep `fuzz/corpus/<target>` as libFuzzer's default), `fuzz/.gitignore` (corpus + artifacts ignored)

**Interfaces:**
- Produces: workflow `Fuzz (nightly)` with `on: schedule: - cron: "17 3 * * *"` and `workflow_dispatch`; `permissions: {}` at workflow level; job `fuzz-nightly` with `contents: read`, `if: github.repository == 'ygorsimoes/zerodeck'`, steps: checkout (`persist-credentials: false`), `./.github/actions/dev-image` (`target: fuzz`, `cargo-cache: fuzz`), `actions/cache` for `fuzz/corpus` (key `fuzz-corpus-${{ github.run_id }}`, restore-keys `fuzz-corpus-`), `run: just fuzz-all 300`, upload `fuzz/artifacts/**` with `if: failure()`.

- [ ] **Step 1:** Write the workflow; `just lint-infra` → PASS (actionlint + zizmor).
- [ ] **Step 2:** Trigger once with `workflow_dispatch` after merge (manual check noted in PR).
- [ ] **Step 3: Commit** — `ci: run every fuzz target nightly with a cached corpus`

### Task 3: Rustdoc gate (I4)

**Files:**
- Modify: `justfile` (`_doc`; add to `_check` and `check-rust`), fix any warnings it surfaces

**Interfaces:**
- Produces: `_doc: RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked`.

- [ ] **Step 1:** Run the command in `just shell`; fix broken intra-doc links / warnings.
- [ ] **Step 2:** Add the recipe; `just check` → PASS.
- [ ] **Step 3: Commit** — `ci: fail checks on rustdoc warnings`

### Task 4: Documentation pass (I5)

**Files:**
- Modify: `README.md`, `docs/architecture/wire-protocol.md`, `AGENTS.md`, `SECURITY.md`, `CONTRIBUTING.md`

- [ ] **Step 1: Checklist** — each item present and accurate:
  - README: `--allow-non-loopback`/`ZERODECK_ALLOW_NON_LOOPBACK`; `--allow-unsafe-root-plugins`/`ZERODECK_ALLOW_UNSAFE_ROOT_PLUGINS` (root-only, systemd drop-in); container examples with `-p 127.0.0.1:1337:1337`; recipes `test-privileged`, `coverage`.
  - wire-protocol.md: `session` param, `failure_reason`, `PluginBlockedError`, `TooManyPendingCalls`, WS limits, static headers/304, `/fetch` 502 vs 504, bind policy.
  - AGENTS.md: graph edge server→platform, `forbid(unsafe_code)` rule, `privileged_*` test rule, new recipes in §6/§7.
  - SECURITY.md: threat model for root plugins and `PRIVILEGED_PATH`; non-loopback bind risk.
  - CONTRIBUTING.md: how to run privileged tests and coverage locally.
- [ ] **Step 2:** Mark the design doc status `implemented 2026-MM-DD` with links to the four PRs.
- [ ] **Step 3:** `just ci` → PASS.
- [ ] **Step 4: Commit** — `docs: document hardening, limits and new developer recipes`

### Task 5: Final verification and PR

- [ ] **Step 1:** `just ci` passes.
- [ ] **Step 2:** Open PR `ci: coverage, nightly fuzzing and rustdoc gate`.
