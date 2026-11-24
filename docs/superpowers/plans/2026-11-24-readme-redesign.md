# README & Brand Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the README and banner with a confident, honest, full-project README and a new "Manifesto" brand (tile mark + outlined wordmark).

**Architecture:** Three hand-authored SVG assets in `docs/assets/` (logo tile, dark banner, light banner) with all text converted to vector paths by a throwaway scratchpad script; a single rewritten `README.md` that uses them through `<picture>`; performance numbers measured from the release binary and the Docker runtime image.

**Tech Stack:** SVG, GitHub-flavored Markdown, Python `fonttools` (scratchpad only, never committed), Inter font (SIL OFL), Docker.

**Spec:** `docs/superpowers/specs/2026-11-24-readme-redesign-design.md`

## Global Constraints

- Brand colors: accent `#FF5A1F`; dark bg `#0E0E10`, dark ink `#F2EFE8`, dark muted `#8A867E`; light bg `#F2EFE8`, light ink `#111111`, light accent `#E8531A`, light muted `#7A776F`.
- Mark: slashed zero inside rounded orange tile (`rx` ≈ 24% of side), ring `#111`, slash `#F2EFE8`, knockout gap where slash crosses ring.
- Headline copy, exact: `Native. Fast. Built in Rust.` (`Rust` in accent). Subline, exact: `The next-generation plugin loader for Steam Deck.`
- No `<text>` elements in any committed SVG; every SVG has `role="img"` and an `aria-label`.
- No negative claims about Decky Loader internals. Every capability claim carries ✅ (shipped) or 🚧 (in progress), matching the spec's Constraints list.
- No Release badge and no claim that releases/tarballs/GHCR images exist until `gh release list` is non-empty.
- Steam Deck install/update/uninstall is presented as the planned Phase 4 flow, never as working today.
- Markdown: 2-space indent, LF, trailing newline (`.editorconfig`). English copy.
- Do not commit or push without the user's explicit OK. `fonttools`/Inter live only in the scratchpad.

## Review Focus

- GitHub renders SVG via `<img>` sandbox: external fonts, `<use href>` to other files, and CSS variables are unreliable — assets must be self-contained (inline defs, literal colors). Checked in Task 1 Step 4.
- Narrow viewports (mobile GitHub app): banner at `width="100%"` must keep headline legible — headline ≥ 21 units in an 880-wide viewBox. Checked in Task 1 Step 4.
- Light vs dark `prefers-color-scheme` fallback: the `<img>` fallback must be the dark banner (GitHub default dark readers) and both sources must exist. Checked in Task 3 Step 3.
- Broken relative links/anchors (`CONTRIBUTING.md#-development-setup`, `deploy/systemd`, `docs/architecture/wire-protocol.md`). Checked in Task 3 Step 3.
- Copy-pasted tutorial commands that don't work on a clean checkout (Docker path, from-source path). Checked in Task 3 Step 4.

---

### Task 1: Brand assets (logo + banners, outlined text)

**Files:**
- Create: `docs/assets/logo.svg` (152×152 tile mark only)
- Replace: `docs/assets/banner-dark.svg`, `docs/assets/banner-light.svg` (viewBox `0 0 880 240`)
- Scratch only: `<scratchpad>/brand/outline.py`, `<scratchpad>/brand/Inter-*.ttf`, `<scratchpad>/brand/venv/`

**Interfaces:**
- Produces: the three file paths above, consumed by Task 3's `<picture>` block and logo references.

- [ ] **Step 1: Get tooling (requires user OK to download).** Inter static TTFs (Black 900, Bold 700, Regular 400) from the official `rsms/inter` GitHub release; `fonttools` into a scratchpad venv. Put downloads in a fresh empty directory.
- [ ] **Step 2: Write `outline.py`** — `outline(text: str, font_path: str, size: float, x: float, y: float, letter_spacing: float = 0) -> str` returns an SVG `<path d=…>` string using `fontTools.pens.svgPathPen.SVGPathPen` + `TransformPen` (flip Y, scale `size / unitsPerEm`, advance by glyph width + letter_spacing). Kerning may be ignored.
- [ ] **Step 3: Compose the three SVGs** from the approved mockup geometry (`.superpowers/brainstorm/38467-1795551691/content/logo-marks.html`, option C): tile at x=56 y=44 size 152; wordmark `ZeroDeck` Inter Black 88, letter-spacing −4, at (236,132); headline Bold 21 at (240,176); subline Regular 15 at (240,204). Inline the mask with a unique id per file. Literal colors only.
- [ ] **Step 4: Verify.**
  Run: `grep -c '<text' docs/assets/*.svg` → Expected: `0` for each file.
  Run: `xmllint --noout docs/assets/*.svg` → Expected: no output.
  Run: `qlmanage -t -s 1200 -o <scratchpad> docs/assets/*.svg` and view each PNG → Expected: matches mockup; also view at 375 px width (browser pane, mobile preset) — headline readable.

### Task 2: Measure performance numbers

**Files:**
- Create: none committed; results recorded into Task 3's Performance section.

**Interfaces:**
- Produces: `binary_size_mb`, `idle_rss_mb`, `ready_ms`, platform string, and the exact reproduce command, for Task 3.

- [ ] **Step 1: Linux numbers via Docker (preferred).** `just docker-build`; binary size via `docker create` + `docker cp` of `/usr/local/bin/zerodeckd` + `ls -l`; run with an empty homebrew volume, poll `/healthz` for ready time, `docker stats --no-stream` after 5 s idle for memory. Run 3 times, take the median.
- [ ] **Step 2: Fallback if Docker is unavailable:** macOS arm64 release build numbers already measured (8.8 MB, ~8 MB RSS, < 0.3 s), labeled as macOS.
- [ ] **Step 3: Write down the one-line reproduce command** that goes in the README.

### Task 3: Rewrite README.md

**Files:**
- Replace: `README.md`

**Interfaces:**
- Consumes: Task 1 asset paths; Task 2 numbers.

- [ ] **Step 1: Write the README** with exactly the 12 sections from the spec ("README structure"), in that order. Feature table rows and statuses:
  ✅ HTTP/WebSocket server with ephemeral auth token · ✅ Steam-only CORS · ✅ plugin discovery (`plugin.json`/`package.json`) · ✅ `SafePath` path jail (primitive) · ✅ atomic settings writes · ✅ Zip-Slip checks + fail-closed SHA-256 (primitives) · ✅ Python host + `decky` module skeleton · 🚧 plugin process supervisor · 🚧 `socketpair()` IPC · 🚧 per-plugin privilege drop · 🚧 plugin static/data routes · 🚧 CEF injection into Steam.
  Quick start: Docker (`just docker-up` + `curl …/healthz`), from source (clone, `pnpm install --frozen-lockfile`, `just build-frontend`, `cargo run --bin zerodeckd`), release tarball paragraph marked "from the first release". On Steam Deck: Install / Update / Uninstall subsections, each one sentence + 🚧 Phase 4.
  FAQ answers: plugins — target is unmodified Decky plugins, not yet loadable; alongside Decky — not today (both use port 1337); affiliation — none with Valve or Decky Loader.
- [ ] **Step 2: Verify claims against code.** For each ✅ row, `grep` the symbol (`SafePath`, `atomic_write`, `enclosed_name`, `MissingSha256`, `discover_plugins`, `csrf_middleware`, `steamloopback.host`) → each must hit in `crates/`. Run `gh release list` → if empty, confirm no Release badge.
- [ ] **Step 3: Verify links.** Every relative link target exists (`ls` each path); anchors into `CONTRIBUTING.md` match its headings; `<picture>` has both sources and the dark fallback. Render on GitHub preview (push to the PR branch only with user OK) or the local browser pane.
- [ ] **Step 4: Verify tutorials.** Run the from-source commands and `curl http://127.0.0.1:1337/healthz` → Expected `{"status":"ok"}`. Run `docker compose config --quiet` → no output.
- [ ] **Step 5: Run `just check`** → Expected: all green (required by AGENTS.md).
- [ ] **Step 6: Ask the user to commit** (`docs(readme): …`) on `feat/release-please-and-oss-docs`.
