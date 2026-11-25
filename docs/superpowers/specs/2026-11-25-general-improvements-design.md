# General hardening, performance, and cleanup pass — design

**Status:** approved 2026-11-25

## Intent

Before Phase 4 (Steam/CEF integration) starts, take one deliberate pass over the whole repository
and fix, tighten, organize, and clean everything a careful reviewer would flag, following the
project's own rules (`AGENTS.md`) and mainstream Rust / Python / TypeScript practice. The owner's
brief is "everything you find to improve, fix, understand, adjust, organize, and clean, always
following good practice." The audit behind this document read every crate, the Python worker host,
the frontend, the Dockerfile, compose, the justfile, the workflows, and the existing specs; `just
check` passed cleanly before any change.

## Goals

- Fix the real defects found in the audit, each with a regression test written first.
- Close the security gaps that let a misconfiguration or a non-root local user reach root.
- Stop blocking the async runtime with filesystem and CPU-heavy work.
- Make the code easier to navigate: smaller modules, tests next to what they test, less
  boilerplate, one copy of shared helpers.
- Strengthen the safety nets: privileged-path tests, coverage, scheduled fuzzing, stricter lints.
- Keep every public wire contract Decky plugins rely on unchanged.

## Non-goals

- Phase 4 work (CEF injection, Quick Access Menu UI, install/update script, `updater/*`).
- Broad systemd sandboxing (`ProtectKernelTunables`, `ProtectSystem=strict`, …): root plugins
  (TDP/governor tools) legitimately write `/sys` and `/proc/sys`, and the restrictions would be
  inherited by them.
- Moving `plugins/` out of `~/homebrew` (see Open decisions).
- Multi-arch release artifacts.

## Anti-goals (a failure even if it "works")

- Any existing Decky plugin, or the `@decky/api` wire format, breaks.
- Any `AGENTS.md` invariant is weakened (SafePath, atomic writes, Zip-Slip, `0755`, fail-closed
  SHA-256, uid/gid drop, socketpair IPC, CORS + auth token).
- A behavior change lands without a test that pins it.
- Versions are bumped by hand, or the crate dependency graph gains a cycle or a new internal edge
  into `zerodeck-protocol`.

## Constraints

- MIT clean-room: implement from public contracts only; no GPL code.
- New Rust dependencies must pass `deny.toml`; new tools are pinned in the `Dockerfile` (version
  `ARG` + SHA-256 for downloaded binaries); new Actions are pinned by commit SHA.
- CI jobs call `just` recipes; required job ids `rust`, `deny`, `python`, `frontend`, `docker` stay.
- Conventional Commits; Release Please owns versions.
- After each PR: `just check`; PRs touching `Dockerfile`, `compose.yaml`, `justfile`, or
  `.github/**` run `just ci`.

## Approaches considered

1. **One PR with everything.** Fastest to open; very hard to review, and any single problem blocks
   all of it.
2. **One PR per theme, one commit per item (chosen).** Four reviewable PRs, highest-risk first;
   each item is a Conventional Commit with its test, so `git log` reads as the change list and a
   bad item can be reverted alone.
3. **One PR per item (~25 PRs).** Maximum isolation, but heavy overhead for a single maintainer and
   lots of rebasing between dependent items.

The owner handed the "how" to the builder; approach 2 was picked because it fits the existing
Release Please / Conventional Commits flow and lets security fixes ship before the refactors.

## Design

Four PRs, in order. Each item lists the problem (with evidence), the fix, and how it is tested.
Docs that describe a changed behavior (`docs/architecture/wire-protocol.md`, `README.md`,
`AGENTS.md`) are updated in the same commit as the behavior.

### PR 1 — `fix`: security and robustness

**S1. Unprivileged workers outlive a crashed daemon.**
- Evidence: `crates/zerodeck-plugin-host/src/supervisor.rs` `build_isolated_command` sets
  `PR_SET_PDEATHSIG(SIGTERM)` *before* `setresgid`/`setresuid`. Linux clears the parent-death
  signal when effective/fs uid or gid change, so every worker that drops to `deck` loses it. The
  systemd unit uses `KillMode=process` + `Restart=always`, so after a daemon crash the old workers
  survive and the restarted daemon spawns duplicates. The Phase 2 spec required orphan-free crashes.
- Fix: set the death signal after the credential change; immediately after, compare `getppid()`
  with the daemon pid captured before `fork` and `_exit` if the parent already died (closes the
  race). *(Builder's call:)* the unit keeps `KillMode=process`. `mixed` would also SIGKILL helper
  processes plugins deliberately leave running, and a Phase 4 self-updater may restart the service
  from a child process; with the death signal fixed, workers already exit with the daemon, and their
  process groups are still killed on clean shutdown.
- Test: a Linux test that spawns a worker, SIGKILLs a stand-in parent, and asserts the worker exits.
  The unprivileged variant needs root, so it is `#[ignore]`d and run by the new privileged job (I1).

**S2. `FULL_SYNC` cache is shared across sessions.**
- Evidence: `WsSyncCache` lives in `AppState` and is keyed by `(id, route, args)`; the frontend
  restarts ids at 1 on every page load (`frontend/src/wsrouter.ts`). After a Steam UI reload, a call
  whose twin from the previous session is still unacknowledged returns the stale reply without
  running. `evict_id` from one client also evicts another client's entries.
- Fix: scope the cache by session. `WSRouter` creates one session id per instance
  (`crypto.randomUUID()`) and sends it as `/ws?auth=…&session=…`; reconnects reuse it, page loads get
  a new one. The server keys entries by `(session, id, route, args)` and evicts only within the
  session. *(Builder's call:)* a connection without `session` uses one shared legacy namespace, i.e.
  today's behavior, so older bundles and the Phase 3 reconnect test keep replay dedup across
  sockets; only clients that send a session get isolation. Document in `wire-protocol.md`.
- Tests: two sessions with identical `(id, route, args)` both execute; reconnect with the same
  session still dedupes; `RECEIVED_RESPONSE` in session A leaves session B's entry.

**S3. Binding to a non-loopback address exposes root to the LAN.**
- Evidence: `--host` / `SERVER_HOST` accepts any address; `host_guard` trusts the attacker-controlled
  `Host` header and `/auth/token` is public, so `curl -H 'Host: localhost' http://<deck>:1337/auth/token`
  yields the token and `loader/install_plugin` can install a `"root"` plugin.
- Fix: refuse to start when the resolved bind address is not loopback unless explicitly allowed
  with `--allow-non-loopback` / `ZERODECK_ALLOW_NON_LOOPBACK=1`; log a prominent warning when allowed.
  *(Builder's call:)* the runtime `Dockerfile` stage and the compose `daemon` service set the
  variable, because a container must bind `0.0.0.0` to be reachable at all and its exposure is
  decided by `-p`; the image runs as a non-root user. The README's container instructions always use
  `-p 127.0.0.1:1337:1337` and say why. Document in README and `wire-protocol.md`.
- Tests: unit tests for the bind decision (loopback v4/v6, `localhost`, `0.0.0.0`, LAN IP, with and
  without the opt-in); `just smoke` keeps passing.

**S4. Development fallbacks in the production binary.**
- Evidence: `crates/zerodeckd/src/main.rs` resolves the Python host and frontend from the current
  directory (`hosts/python`, `frontend/dist`) and from the compile-time `CARGO_MANIFEST_DIR`. Under
  systemd the working directory is `~deck/homebrew/services`, which `deck` can write, so a missing
  `/opt/zerodeck/python` lets a non-root user plant code that root plugins run.
- Fix: keep `PYTHONPATH` / `ZERODECK_FRONTEND_DIR`, `/opt/zerodeck/...`, and `<exe>/../...` in all
  builds; compile the cwd and `CARGO_MANIFEST_DIR` candidates only under `cfg(debug_assertions)`.
  *(Builder's call:)* a missing Python host does not stop the daemon (with `Restart=always` that
  would crash-loop and leave no UI). It logs one error at startup, and every backend plugin spawn
  fails fast with a clear `PluginFailed` reason instead of retrying with backoff.
- Tests: unit tests on the candidate list per build profile (extract the list into a pure function).

**S5. `plugins/` may be writable by the unprivileged user.**
- Evidence: plugins run as root when flagged `"root"`, from `<PRIVILEGED_PATH>/plugins`, which
  defaults to `~deck/homebrew/plugins`. Nothing checks who owns that directory.
- Fix (decided by the owner: refuse, with a developer opt-in):
  - Before every spawn of a `"root"` plugin (startup, enable, reload, install, hot-reload), when the
    daemon runs as root, verify the plugin's directory tree: every directory and file is owned by
    uid 0 and none is group- or world-writable; symlinks are not followed. The check also covers
    `plugins_dir` itself. On failure the plugin does not start: its state is `Failed` with a reason
    that names the offending path, and the reason reaches `loader/get_plugins` / logs.
  - Opt-in for development: `developer.allow_unsafe_root_plugins: true` in `loader.json` skips the
    check (logged as a warning on each spawn). It is off by default and, unlike
    `developer.allow_unverified_installs` (which stays UI-settable, as today), `loader/set_setting`
    rejects it: a key that grants root to `deck`-writable code must only be set by editing
    `loader.json` on disk.
  - Plugins without the `"root"` flag are unaffected; no ownership is changed automatically.
  - The installer already produces root-owned `0755`/`0644` trees, so store installs pass.
  - Ancestors of `PRIVILEGED_PATH` writable by a non-root user only produce one startup warning
    (on a stock Deck `~` and `~/homebrew` are `deck`-owned); closing that fully is the open decision
    below.
- Tests: pure-function tests for the tree check (owner, mode bits, symlink, nested file) with
  injected metadata; manager test that a failing check yields `Failed` and the opt-in bypasses it;
  test that `set_setting` rejects the opt-in key; a privileged test exercises real ownership.

**S6. Two plugins with the same `name` collide.**
- Evidence: `discover_plugins` accepts duplicate `plugin.json` names in different directories; the
  supervisor keys workers by name, so the second spawn replaces the first worker, and routes resolve
  whichever comes first.
- Fix: discovery keeps the first directory (sorted order), logs the conflict, and skips the rest;
  `find_plugin_by_id` behavior is unchanged for unique names.
- Test: two directories with one name → one plugin discovered, warning logged.

**S7. Static responses are missing hardening headers and common types.**
- Fix: add `X-Content-Type-Options: nosniff` to every file response; extend the MIME table (`gif`,
  `ico`, `avif`, `ttf`, `otf`, `woff`, `map`, `txt`, `mp3`, `ogg`, `wav`, `mp4`, `webm`); send
  `Last-Modified` and answer `If-Modified-Since` with `304` (no ETag hashing — avoids reading files).
  `/plugins/{name}/data/*` stays public as decided in Phase 3 (Decky parity); its rationale is written
  into `wire-protocol.md`.
- Tests: router tests for headers, new types, and `304`.

**I1 (lands with PR 1). Privileged test job.**
- The Docker-first design listed the root → user privilege drop as untestable in containers. Add a
  compose service `privileged-test` (dev image, `user: root`, only the capabilities the drop needs:
  `SETUID`, `SETGID`, `CHOWN`, `KILL`, `DAC_OVERRIDE`, `FOWNER`; `no-new-privileges` kept), a
  `just test-privileged` recipe forwarding to `_test-privileged` (`cargo test -- --ignored privileged_`),
  and a non-required CI job `privileged`. Tests named `privileged_*` are `#[ignore]` and assert
  `euid == 0` up front.

### PR 2 — `perf`: stop blocking the runtime

**P1. Blocking work on tokio worker threads.** There is no `spawn_blocking` anywhere. Move to
`tokio::task::spawn_blocking`: ZIP extraction + unwrap + manifest read + `commit_staged_plugin`
(installer), SHA-256 of downloaded bytes, `remote_binary` file writes, file picker listing, and
settings persistence. Settings: replace the `std::Mutex<SettingsManager>` held across `fsync` with a
`tokio::sync::Mutex` and perform the write in `spawn_blocking`, keeping writes ordered; the
synchronous `set_setting` becomes `async`. Test: existing installer/settings tests pass; add a test
that a WS `utilities/ping` is answered while a large install is in progress.

**P2. Unbounded work per WebSocket connection.** The goal is bounding tasks and memory a buggy or
hostile client can create, not throttling plugin workers. *(Builder's call:)* the socket loop never
blocks on the limit (keepalive pongs must keep flowing). Each connection tracks its pending calls
(in flight + waiting for a slot); up to 64 run at once (configurable in `ServerConfig`) and the rest
wait in a bounded queue of 1024. A call that arrives when the queue is full, `FULL_SYNC` replays
included, gets an immediate `Error` reply (`TooManyPendingCalls`). Tests for both limits and for
pongs still being processed while the queue is full.

**P3. `/fetch` request body and error mapping.** Pass the collected `Bytes` to `reqwest::Body`
without `to_vec()`. Map timeouts from `reqwest::Error::is_timeout()` (carry a typed variant through
`CoreError`) instead of searching the message text. Tests: existing `/fetch` tests + timeout → 504.

**P4. Release profile.** Add `[profile.release]` with `lto = "thin"`, `codegen-units = 1`,
`strip = "debuginfo"`; keep `panic = "unwind"` (a panicking task must not abort the daemon). Record
binary size before/after in the PR description.

**P5. Idle wakeups from the debug watcher.** The mtime sweep ticks every 2 s even with no `"debug"`
plugins. Pause the sweep while nothing is tracked; resume on `PluginsChanged`. (*Builder's call:*
inotify stays set up: an idle inotify descriptor causes no wakeups.) Test: watcher with zero debug plugins performs no sweeps over a paused-time window.

**P6. Small allocations.** `SettingsManager::commit_map` serializes the map without cloning it twice.

### PR 3 — `refactor`: organization and cleanup

**C1. Tests next to code.** Move the 35 tests in `crates/zerodeck-core/src/lib.rs` (2.7k lines) out:
tests of private helpers into `#[cfg(test)] mod tests` in their module; tests of the public API into
`crates/zerodeck-core/tests/*.rs` grouped by area (installer, manager, filepicker, watcher, http
policy). Shared fixtures move to a small test-support module. No test is deleted or weakened.

**C2. Split `router.rs` (1.5k lines).** Into `routes/static_files.rs`, `routes/legacy.rs`,
`ws/socket.rs` (connection loop), `ws/dispatch.rs` (route table), `ws/args.rs` (argument parsers).
A helper `reply(id, Result<impl Serialize, CoreError>) -> WsServerMessage` replaces the repeated
`match` blocks in `dispatch_*`. Same treatment for `zerodeck-server/src/lib.rs` tests if needed.

**C3. One `lock_unpoisoned`.** Move it to `zerodeck-platform` (`sync` module) and use it from core,
plugin-host, and server (server already depends on platform transitively; add the direct edge, which
the graph allows).

**C4. Constructors.** `AppState::new`/`with_plugins` (static backend, test-only use) and
`build_default_http_client(...).expect(...)` become a single fallible builder; `PluginManager`'s
three constructors become `PluginManager::builder()`. No `expect` on runtime paths.

**C5. `unsafe` policy.** `#![forbid(unsafe_code)]` in every crate except `zerodeck-plugin-host`,
which keeps the workspace `warn` plus its documented `// SAFETY:` block, and `zerodeck-cef`, which
gets `#![deny(unsafe_code)]` (*builder's call:* Phase 4 CDP work may need a reviewed `#[allow]`,
which `forbid` would not permit).

**C6. Dependencies.** Align `tokio-tungstenite` with the version axum uses (0.29) to drop the
duplicate, unless 0.30 is required by a used API; `tower-http` 0.6 comes from `reqwest` and stays.
Narrow `tokio` features per crate where cheap.

**C7. Python lint.** Extend ruff to `E, F, I, UP, B, SIM, RUF, ASYNC, C4, PIE` and fix findings.

**C8. Python worker correctness.**
- A synchronous `_main` runs in `asyncio.to_thread` and cannot be cancelled, so `asyncio.run` waits
  for it at exit and the worker hangs until SIGKILL. Run synchronous `_main` in a dedicated daemon
  thread so shutdown completes within the daemon's stop timeout.
- `settings.SettingsManager.commit` fsyncs the parent directory after `os.replace`, matching Rust's
  `atomic_write`.
- Tests for both.

**C9. Frontend origin.** `frontend/src/index.ts` hard-codes `http://127.0.0.1:1337`. Derive the
daemon origin from `import.meta.url` (the bundle is served by the daemon) for both `/auth/token` and
the WebSocket base URL, falling back to the current constant. Test the derivation.

**C10. Small cleanups found on the way.** `SafePathSet::resolve` error reports every root rather
than only the first; remove dead or test-only public API (`with_http_for_tests` becomes `#[cfg(test)]`
or moves to test support).

### PR 4 — `ci`/`docs`: safety nets

**I2. Coverage.** Pin `cargo-llvm-cov` in the `dev` image (version `ARG` + SHA-256), add
`just coverage` (HTML + lcov into `target/coverage`), and a non-required CI job that uploads the
report as an artifact. No third-party coverage service.

**I3. Scheduled fuzzing.** A `schedule` (nightly) trigger running `just fuzz-all 300` with the corpus
cached between runs; crashes upload as artifacts. The 30 s PR smoke fuzz stays.

**I4. Rustdoc.** `_check` gains `cargo doc --workspace --no-deps` with `RUSTDOCFLAGS="-D warnings"`.

**I5. Docs.** README configuration table (new flag/env), `wire-protocol.md` (session parameter,
headers, bind policy), `AGENTS.md` (new recipes, privileged job, `forbid(unsafe_code)` rule),
`SECURITY.md` (threat model note on `PRIVILEGED_PATH` ownership).

## Open decisions

- **Location of `plugins/`.** Root executes code from a tree whose ancestors `deck` owns. Options:
  (a) keep `~/homebrew/plugins` (Decky-compatible) with S5's ownership fix + warning; (b) move
  `PRIVILEGED_PATH` to a root-owned location such as `/var/lib/zerodeck` and keep a compatibility
  symlink. This design implements (a) plus the per-spawn root check in S5; (b) is a separate
  decision for Phase 4's installer.

## Left to the builder

- Exact module and test-file names, helper signatures, and default limits other than those stated.
- Commit granularity inside an item when one item needs several steps.
- Whether a refactor in PR 3 is better done before a fix in PR 2 when they touch the same lines
  (rebase order), as long as each PR stays green.
