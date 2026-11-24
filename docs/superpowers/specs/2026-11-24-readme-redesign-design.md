# README & Brand Redesign — Short Design

**Date:** 2026-11-24
**Status:** Awaiting approval
**Branch / PR:** `feat/release-please-and-oss-docs` (ygorsimoes/zerodeck#7)

## Intent & why

The current README and banner don't sell the project. The banner reads as a generic dark SaaS
template (grid, blurred circles, Tailwind sky→indigo→orange gradient, half-empty canvas, system-font
text). The README over-claims (comparison table presents unbuilt features as done; "faster" with no
numbers), takes unsourced shots at Decky Loader, and can't decide who it's talking to.

The new README must cover the whole project and **convince three audiences at once**: Steam Deck
users, contributors, and people judging whether this is a serious project. It must make clear that
ZeroDeck is **fast and modern**, respect Decky Loader while carrying a confident "I'm here to
succeed you" tone, and include simple tutorials for install, local run, and update.

## Goals

- Confident, premium first impression: new banner + mark.
- "Fast/native/modern" backed by **real, reproducible numbers**, not adjectives.
- Every capability claim carries an honest status (✅ shipped / 🚧 in progress).
- Simple, copy-pasteable tutorials for what works today; the Steam Deck flow is visible as planned.
- Clear on-ramp for contributors.

## Non-goals

- Building an installer/update script (`install.sh`) — separate future work (approach C, rejected for now).
- Benchmarking against Decky Loader on device — listed as "coming", not measured here.
- Changing `CONTRIBUTING.md`, `SECURITY.md`, or code.

## Anti-goals (failure even if it "works")

- A tutorial that doesn't actually work, or implies ZeroDeck runs on a Deck today.
- Negative, unsourced claims about Decky Loader's internals.
- Numbers that weren't measured, or a release badge/links that point at nothing.

## Approaches considered (install/update section)

| | Approach | Trade-off | Decision |
|---|---|---|---|
| A | Document the future Deck install as if it existed | Looks complete, but misleading | Rejected |
| **B** | Document what works today (Docker, release tarball, from source) + a "On Steam Deck" section with install/update/uninstall flow marked **Phase 4** | Honest, still shows direction | **Chosen by user** |
| C | B + a real `install.sh` (install/update/uninstall, systemd, SHA-256) | Real tutorial, but new code, bigger than a README PR, and risks breaking Decky on port 1337 | Deferred |

## Decided by the user

- **Banner direction:** "Manifesto" — big typographic wordmark, one accent color, own mark.
- **Mark:** "Ícone" — slashed zero inside a rounded orange (`#FF5A1F`) tile; zero ring `#111`,
  slash cream `#F2EFE8`, with a knockout gap where the slash crosses the ring.
- **Tagline:** headline **"Native. Fast. Built in Rust."** (`Rust` in accent), subline
  **"The next-generation plugin loader for Steam Deck."**
- **Install section:** approach B.
- **Tone:** respectful to Decky ("built the ecosystem"), confident successor framing.

## README structure (proposed, no objections raised)

1. Banner (`<picture>` dark/light) · one-line pitch · badges: CI, Rust, MIT (Release badge only once a release exists)
2. Short status callout: pre-alpha; runs on desktop today, Steam Deck support lands in Phase 4
3. **Why ZeroDeck** — credits Decky for the ecosystem, positions ZeroDeck as the next step;
   feature table with per-row ✅/🚧 (no Decky column of negative claims)
4. **Performance** — measured numbers + method + one command to reproduce; "on-device Decky comparison: coming"
5. **Quick start** — Docker (one command) · release tarball (once published) · from source
6. **On Steam Deck** — Install / Update / Uninstall, planned flow, marked Phase 4
7. **Configuration** (collapsed) — flags & env vars
8. **How it works** — diagram + crate map
9. **Roadmap** — phases with checkboxes
10. **Contributing** — where to start, good-first-issue link, `just check`
11. **FAQ** — Will my plugins work? Can I run it alongside Decky? Affiliated with Valve/Decky?
12. **License & acknowledgements** (thanks to the Decky Loader team)

## Constraints

- Measured today (macOS arm64, release build, empty plugin dir): binary **8.8 MB**, idle RSS
  **~8 MB**, `/healthz` ready in **< 0.3 s**. README must state platform/method; re-measure on Linux
  x86_64 (Docker) if feasible and prefer those numbers.
- Feature status must match code: shipped = HTTP/WS server, auth token, Steam-only CORS, plugin
  discovery, `SafePath`, `atomic_write`, Zip-Slip + SHA-256 primitives, Python host IPC skeleton;
  in progress = process supervisor, `socketpair` IPC, privilege drop, plugin routes, CEF injection.
- Banner text must be converted to vector paths so it renders identically on every OS (Inter, SIL
  OFL). This needs Inter + `fonttools` downloaded into the scratchpad — ask before downloading.
- Links into `CONTRIBUTING.md` depend on its emoji headings (e.g. `## 🛠 Development Setup` →
  `#-development-setup`); verify every anchor resolves on GitHub after the rewrite.
- Follow `.editorconfig` (2-space Markdown), keep `just check` green.

## Left to the builder

- Exact copy of each section (English), FAQ answers, roadmap checkbox granularity.
- Light-theme banner palette (base: bg `#F2EFE8`, ink `#111`).
- Whether to also add `docs/assets/logo.svg` (tile mark alone) for avatar/social preview — builder's call: yes, it's cheap.
- Replace `docs/assets/banner-{dark,light}.svg` in place.
