# Performance Budgets

These are engineering budgets for the Phase 4 release candidate, not Steam Deck measurements. The
authoritative Deck numbers, when available, belong in `deploy/update/COMPATIBILITY.md`.

## Budgets

| Metric | Budget | Notes |
|---|---:|---|
| Daemon launch to first `/healthz` reply, p95 | **≤ 150 ms** | release build, empty plugin directory |
| Idle daemon RSS | **≤ 16 MiB** | no plugin workers, CEF disabled for isolation |
| Idle CPU | **≤ 1.0% of one core** | measured over 5 s after a 1 s settle |
| Release binary | **no regression without a recorded reason** | report bytes in the PR |
| CEF context change to confirmed QAM, p95 | **≤ 2 s** | controlled simulated CDP, not Steam startup time |
| First interactive panel with an active integration, p95 | **≤ 250 ms** | simulated host |
| 100 CEF reinjections | **no unbounded attempts, connections, listeners or memory growth** | supervisor soak test |

Steam startup, plugin import time and the Deck's thermal state are external to these budgets and are
reported separately during hardware qualification.

## Method

- `ZERODECK_NATIVE=1 just perf` builds the release daemon on the host and runs
  `deploy/perf/measure.sh`; running it in Docker measures the container, not the host.
- The script starts the daemon with `ZERODECK_DISABLE_CEF=1`, a temporary home and an empty plugin
  directory. It reports startup p95 over 20 runs, idle RSS, idle CPU and binary size. Cold macOS
  process starts are visible in the samples; 20 runs keep one outlier from becoming the headline.
- Budget breaches warn by default. Set `ZERODECK_PERF_STRICT=1` to fail the run.
- CEF bookkeeping is covered by
  `cargo test -p zerodeck-cef one_hundred_context_changes_do_not_accumulate_attempts`.
- Compare like with like: same host, release profile, empty plugin directory and no concurrent
  builds. Record the host, commit and binary digest alongside any number used in a release note.

The budgets are deliberately not part of required CI: shared runners vary too much for stable
millisecond thresholds. A release candidate records the measurement in its notes; a regression must
be explained or fixed before Stable for Steam Deck.
