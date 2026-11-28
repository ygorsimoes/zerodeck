#!/usr/bin/env bash
# Measures the release daemon against the Phase 4 engineering budgets. This is a smoke budget, not
# a Steam Deck benchmark: run it natively (`ZERODECK_NATIVE=1 just perf`) for host-representative
# numbers. CEF/Steam timing is covered by the supervisor soak test and hardware qualification.
set -euo pipefail

binary="${1:-target/release/zerodeckd}"
runs="${ZERODECK_PERF_RUNS:-20}"
port="${ZERODECK_PERF_PORT:-1339}"
strict="${ZERODECK_PERF_STRICT:-0}"
startup_budget_ms="${ZERODECK_PERF_STARTUP_MS:-150}"
rss_budget_kib="${ZERODECK_PERF_RSS_KIB:-16384}"
cpu_budget_percent="${ZERODECK_PERF_CPU_PERCENT:-1.0}"

if [[ ! -x "${binary}" ]]; then
    echo "error: release binary not found: ${binary}" >&2
    exit 1
fi

home="$(mktemp -d "${TMPDIR:-/tmp}/zerodeck-perf.XXXXXX")"
log="${home}/daemon.log"
pid=""
samples=()

cleanup() {
    if [[ -n "${pid}" ]] && kill -0 "${pid}" 2>/dev/null; then
        kill -TERM "${pid}" 2>/dev/null || true
        wait "${pid}" 2>/dev/null || true
    fi
    rm -rf "${home}"
}
trap cleanup EXIT

mkdir -p "${home}/home" "${home}/homebrew"

now_ms() {
    python3 -c 'import time; print(int(time.time() * 1000))'
}

wait_health() {
    local deadline
    deadline=$(( $(now_ms) + 10000 ))
    while (( $(now_ms) < deadline )); do
        if curl -fsS "http://127.0.0.1:${port}/healthz" >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.01
    done
    return 1
}

start_daemon() {
    env -i \
        PATH="${PATH}" \
        HOME="${home}/home" \
        DECKY_USER_HOME="${home}/home" \
        UNPRIVILEGED_PATH="${home}/homebrew" \
        PRIVILEGED_PATH="${home}/homebrew" \
        ZERODECK_DISABLE_CEF=1 \
        RUST_LOG=error \
        "${binary}" --host 127.0.0.1 --port "${port}" >"${log}" 2>&1 &
    pid=$!
}

stop_daemon() {
    kill -TERM "${pid}" 2>/dev/null || true
    wait "${pid}" 2>/dev/null || true
    pid=""
}

for ((run = 1; run <= runs; run++)); do
    started=$(now_ms)
    start_daemon
    if ! wait_health; then
        echo "error: daemon did not become healthy within 10s" >&2
        cat "${log}" >&2
        exit 1
    fi
    finished=$(now_ms)
    samples+=("$((finished - started))")
    stop_daemon
done

startup_p95="$(python3 - "${samples[@]}" <<'PY'
import sys
values = sorted(int(value) for value in sys.argv[1:])
index = max(0, min(len(values) - 1, int(round(0.95 * (len(values) - 1)))))
print(values[index])
PY
)"

start_daemon
wait_health
sleep 1
rss_before="$(ps -o rss= -p "${pid}" | tr -d ' ')"
cpu_before="$(ps -o %cpu= -p "${pid}" | tr -d ' ')"
sleep 5
rss_after="$(ps -o rss= -p "${pid}" | tr -d ' ')"
cpu_after="$(ps -o %cpu= -p "${pid}" | tr -d ' ')"
stop_daemon
size_bytes="$(wc -c < "${binary}" | tr -d ' ')"
size_mib="$(python3 -c "print(f'{$size_bytes / 1048576:.2f}')")"
rss_mib="$(python3 - "${rss_after}" <<'PY'
import sys
print(f"{int(sys.argv[1]) / 1024:.2f}")
PY
)"

printf '%s\n' \
    "ZeroDeck release budget (smoke, not a Deck benchmark)" \
    "  startup to /healthz p95 : ${startup_p95} ms (budget ${startup_budget_ms} ms)" \
    "  idle RSS                : ${rss_mib} MiB (budget $((rss_budget_kib / 1024)) MiB)" \
    "  idle CPU (one core)     : ${cpu_after}% (budget ${cpu_budget_percent}%)" \
    "  RSS after 5 s idle      : ${rss_before} -> ${rss_after} KiB" \
    "  release binary          : ${size_mib} MiB" \
    "  CEF reinjection soak    : cargo test -p zerodeck-cef one_hundred_context_changes"

failed=0
python3 - "${startup_p95}" "${startup_budget_ms}" <<'PY' || failed=1
import sys
raise SystemExit(0 if int(sys.argv[1]) <= int(sys.argv[2]) else 1)
PY
python3 - "${rss_after}" "${rss_budget_kib}" <<'PY' || failed=1
import sys
raise SystemExit(0 if int(sys.argv[1]) <= int(sys.argv[2]) else 1)
PY
python3 - "${cpu_after}" "${cpu_budget_percent}" <<'PY' || failed=1
import sys
raise SystemExit(0 if float(sys.argv[1]) <= float(sys.argv[2]) else 1)
PY

if (( failed != 0 )); then
    echo "warning: one or more budgets were exceeded (smoke environment)" >&2
    if [[ "${strict}" == "1" ]]; then
        exit 1
    fi
fi
