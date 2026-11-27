#!/usr/bin/env bash
# Bootstrap refusal, conversion and import tests. No root, real Deck, systemd or network required.
set -euo pipefail

tests_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${tests_dir}/../../.." && pwd)"
export FIXTURE_REPO_ROOT="${repo_root}"
# shellcheck source=fixtures.sh
source "${tests_dir}/fixtures.sh"

tmp="$(mktemp -d "${TMPDIR:-/tmp}/zerodeck-bootstrap-test.XXXXXX")"
trap 'if [[ "${ZERODECK_KEEP_TMP:-0}" == "1" ]]; then echo "kept test dir: ${tmp}" >&2; else rm -rf "${tmp}"; fi' EXIT

systemctl="${tmp}/fake-systemctl"
fixture_fake_systemctl "${systemctl}"
base_for() { echo "${1%/var/lib/zerodeck}"; }
export FAKE_SYSTEMCTL_LOG="${tmp}/systemctl.log"
export FAKE_SYSTEMCTL_STATE="${tmp}/systemctl-state"
mkdir -p "${FAKE_SYSTEMCTL_STATE}"
: >"${FAKE_SYSTEMCTL_LOG}"
: >"${FAKE_SYSTEMCTL_STATE}/enabled"

assets="${tmp}/assets"
mkdir -p "${assets}"
version="0.1.0"
build_id="fixture-build-v0.1.0-deadbeef"
manual="${assets}/zerodeck-v${version}-linux-x86_64.tar.gz"
managed="${assets}/zerodeck-v${version}-linux-x86_64.managed.tar.gz"
fixture_release_assets "${assets}" "${version}" "${build_id}" 1 >/dev/null
trusted_root="${assets}/trusted-root.json"
manual_sha="$(fixture_sha256 "${manual}")"
managed_sha="$(fixture_sha256 "${managed}")"
root_sha="$(fixture_sha256 "${trusted_root}")"

new_install() {
    local name="$1"
    local base="${tmp}/${name}"
    mkdir -p "${base}"
    printf '%s\t%s\t%s\t%s\n' \
        "${base}/var/lib/zerodeck" \
        "${base}/usr/local/lib/zerodeck" \
        "${base}/etc/systemd/system" \
        "${base}/etc/zerodeck"
}

# run_bootstrap <base> <manual> <manual-sha> <managed> <managed-sha> <root> <root-sha> [extra...]
run_bootstrap() {
    local base="$1" manual_archive="$2" manual_digest="$3" managed_archive="$4" \
        managed_digest="$5" root_file="$6" root_digest="$7"
    shift 7
    bash "${repo_root}/deploy/update/bootstrap.sh" \
        --package "${manual_archive}" --sha256 "${manual_digest}" \
        --generation "${managed_archive}" --generation-sha256 "${managed_digest}" \
        --trusted-root "${root_file}" --trusted-root-sha256 "${root_digest}" \
        --root "${base}/var/lib/zerodeck" \
        --controller-root "${base}/usr/local/lib/zerodeck" \
        --systemd-dir "${base}/etc/systemd/system" \
        --config-dir "${base}/etc/zerodeck" \
        --bin-link "${base}/usr/local/bin/zerodeckctl" \
        --data-dir "${base}/home/deck/homebrew" \
        --systemctl "${systemctl}" \
        --metadata-url "https://updates.example.test/metadata/" \
        --targets-url "https://updates.example.test/targets/" \
        "$@"
}

expect_failure() {
    local label="$1"
    shift
    if "$@" >"${tmp}/${label}.log" 2>&1; then
        echo "error: ${label} unexpectedly succeeded" >&2
        cat "${tmp}/${label}.log" >&2
        exit 1
    fi
}

# ── Bad package hash: refuse before extracting or touching the host ────────────────────────────
read -r root controller units config < <(new_install bad-hash)
expect_failure bad-hash run_bootstrap "$(base_for "${root}")" "${manual}" \
    "$(printf '0%.0s' {1..64})" "${managed}" "${managed_sha}" "${trusted_root}" "${root_sha}"
[[ ! -e "${controller}/bin/zerodeckctl" ]] || { echo "bad hash installed a controller" >&2; exit 1; }

# ── Missing or invalid trusted root ───────────────────────────────────────────────────────────
read -r root controller units config < <(new_install missing-root)
expect_failure missing-root run_bootstrap "$(base_for "${root}")" "${manual}" "${manual_sha}" \
    "${managed}" "${managed_sha}" "${tmp}/does-not-exist.json" "${root_sha}"
[[ ! -e "${controller}/bin/zerodeckctl" ]] || { echo "missing root installed a controller" >&2; exit 1; }

printf 'not json\n' >"${tmp}/invalid-root.json"
invalid_root_sha="$(fixture_sha256 "${tmp}/invalid-root.json")"
read -r root controller units config < <(new_install invalid-root)
expect_failure invalid-root run_bootstrap "$(base_for "${root}")" "${manual}" "${manual_sha}" \
    "${managed}" "${managed_sha}" "${tmp}/invalid-root.json" "${invalid_root_sha}"
[[ ! -e "${controller}/bin/zerodeckctl" ]] || { echo "invalid root installed a controller" >&2; exit 1; }

# ── Running Decky refuses and leaves Decky's files untouched ──────────────────────────────────
decky_home="${tmp}/decky-home"
mkdir -p "${decky_home}/plugins/example"
printf 'decky data\n' >"${decky_home}/plugins/example/main.py"
decky_before="$(fixture_sha256 "${decky_home}/plugins/example/main.py")"
read -r root controller units config < <(new_install decky-active)
FAKE_DECKY_ACTIVE=1
export FAKE_DECKY_ACTIVE
expect_failure decky-active run_bootstrap \
    "$(base_for "${root}")" "${manual}" "${manual_sha}" "${managed}" "${managed_sha}" \
    "${trusted_root}" "${root_sha}" \
    --data-dir "${decky_home}" --import-inactive-decky "${decky_home}"
[[ "$(fixture_sha256 "${decky_home}/plugins/example/main.py")" == "${decky_before}" ]] || {
    echo "running Decky files changed" >&2; exit 1;
}
[[ ! -e "${controller}/bin/zerodeckctl" ]] || { echo "Decky conflict installed a controller" >&2; exit 1; }
unset FAKE_DECKY_ACTIVE

# ── Missing managed component refuses before install ─────────────────────────────────────────
fixture_incomplete_managed "${assets}" "${version}" "${build_id}" >/dev/null
incomplete_sha="$(fixture_sha256 "${managed}")"
read -r root controller units config < <(new_install missing-component)
expect_failure missing-component run_bootstrap "$(base_for "${root}")" "${manual}" "${manual_sha}" \
    "${managed}" "${incomplete_sha}" "${trusted_root}" "${root_sha}"
[[ ! -e "${controller}/bin/zerodeckctl" ]] || { echo "missing component installed a controller" >&2; exit 1; }
fixture_release_assets "${assets}" "${version}" "${build_id}" 1 >/dev/null
manual_sha="$(fixture_sha256 "${manual}")"
managed_sha="$(fixture_sha256 "${managed}")"
root_sha="$(fixture_sha256 "${trusted_root}")"

# ── Pending activation refuses controller replacement ────────────────────────────────────────
read -r root controller units config < <(new_install pending)
mkdir -p "${controller}/bin" "${units}" "${root}/state"
printf 'old-controller\n' >"${controller}/bin/zerodeckctl"
chmod 0755 "${controller}/bin/zerodeckctl"
cp "${repo_root}/deploy/systemd/zerodeck.service" "${units}/zerodeck.service"
printf '{"schema":1,"pending":{"id":"00000000-0000-4000-8000-000000000001","previous":null,"candidate":"%064d","phase":"prepared","was_running":false,"start_attempted":false}}\n' 0 \
    >"${root}/state/update.json"
printf '%064d\n' 0 >"${root}/current"
controller_before="$(fixture_sha256 "${controller}/bin/zerodeckctl")"
state_before="$(fixture_sha256 "${root}/state/update.json")"
expect_failure pending run_bootstrap "$(base_for "${root}")" "${manual}" "${manual_sha}" \
    "${managed}" "${managed_sha}" "${trusted_root}" "${root_sha}" --convert-existing-zerodeck
[[ "$(fixture_sha256 "${controller}/bin/zerodeckctl")" == "${controller_before}" ]] || {
    echo "pending activation replaced the controller" >&2; exit 1;
}
[[ "$(fixture_sha256 "${root}/state/update.json")" == "${state_before}" ]] || {
    echo "pending activation changed persisted state" >&2; exit 1;
}

# ── Failed conversion restores the old controller, units, overrides and data ──────────────────
read -r root controller units config < <(new_install failed-conversion)
data_dir="$(base_for "${root}")/home/deck/homebrew"
old_generation="$(printf 'a%.0s' {1..64})"
mkdir -p "${controller}/bin" "${units}/zerodeck.service.d" "${root}/state" "${data_dir}/plugins"
printf 'old-controller\n' >"${controller}/bin/zerodeckctl"
chmod 0755 "${controller}/bin/zerodeckctl"
printf 'old controller file\n' >"${controller}/old-marker"
printf '[Service]\nEnvironment=OLD_UNIT=1\n' >"${units}/zerodeck.service"
printf '[Service]\nEnvironment=USER_OVERRIDE=1\n' >"${units}/zerodeck.service.d/override.conf"
printf '%s\n' "${old_generation}" >"${root}/current"
printf '{"schema":1,"last_confirmed":"%s","rollback_target":null,"quarantined":[],"pending":null,"last_result":"old","last_error":null}\n' \
    "${old_generation}" >"${root}/state/update.json"
printf 'zerodeck.service\n' >"${FAKE_SYSTEMCTL_STATE}/enabled"
printf 'plugin data\n' >"${data_dir}/plugins/keep.py"
controller_before="$(fixture_sha256 "${controller}/bin/zerodeckctl")"
unit_before="$(fixture_sha256 "${units}/zerodeck.service")"
override_before="$(fixture_sha256 "${units}/zerodeck.service.d/override.conf")"
current_before="$(fixture_sha256 "${root}/current")"
state_before="$(fixture_sha256 "${root}/state/update.json")"
data_before="$(fixture_sha256 "${data_dir}/plugins/keep.py")"
FAKE_ZERODECK_ACTIVE=1
FAKE_START_FAIL=1
export FAKE_ZERODECK_ACTIVE FAKE_START_FAIL
expect_failure failed-conversion run_bootstrap \
    "$(base_for "${root}")" "${manual}" "${manual_sha}" "${managed}" "${managed_sha}" \
    "${trusted_root}" "${root_sha}" --convert-existing-zerodeck --start
[[ "$(fixture_sha256 "${controller}/bin/zerodeckctl")" == "${controller_before}" ]] || {
    echo "failed conversion did not restore the controller" >&2; exit 1;
}
[[ "$(fixture_sha256 "${units}/zerodeck.service")" == "${unit_before}" ]] || {
    echo "failed conversion did not restore the old unit" >&2; exit 1;
}
[[ "$(fixture_sha256 "${units}/zerodeck.service.d/override.conf")" == "${override_before}" ]] || {
    echo "failed conversion changed the administrator override" >&2; exit 1;
}
[[ "$(fixture_sha256 "${root}/current")" == "${current_before}" ]] || {
    echo "failed conversion did not restore current" >&2; exit 1;
}
[[ "$(fixture_sha256 "${root}/state/update.json")" == "${state_before}" ]] || {
    echo "failed conversion did not restore state" >&2; exit 1;
}
[[ "$(fixture_sha256 "${data_dir}/plugins/keep.py")" == "${data_before}" ]] || {
    echo "failed conversion changed data" >&2; exit 1;
}
backup="$(find "${root}/backup" -mindepth 1 -maxdepth 1 -type d -print -quit)"
[[ -n "${backup}" && -f "${backup}/MANIFEST" ]] || { echo "no recoverable backup was kept" >&2; exit 1; }
grep -qx "zerodeck.service" "${FAKE_SYSTEMCTL_STATE}/enabled" || {
    echo "failed conversion did not restore the previously enabled service" >&2; exit 1;
}
unset FAKE_ZERODECK_ACTIVE FAKE_START_FAIL

# ── A clean install without --start installs but never enables or starts ─────────────────────
: >"${FAKE_SYSTEMCTL_LOG}"
read -r root controller units config < <(new_install clean)
run_bootstrap "$(base_for "${root}")" "${manual}" "${manual_sha}" "${managed}" "${managed_sha}" \
    "${trusted_root}" "${root_sha}" >"${tmp}/clean.log"
generation_dir="${root}/generations/${managed_sha}"
[[ -x "${controller}/bin/zerodeckctl" ]] || { echo "controller missing" >&2; exit 1; }
[[ -f "${generation_dir}/bin/zerodeckd" ]] || { echo "generation missing" >&2; exit 1; }
[[ "$(cat "${root}/current")" == "${managed_sha}" ]] || { echo "current not set" >&2; exit 1; }
[[ "$(fixture_sha256 "${config}/trusted-root.json")" == "${root_sha}" ]] || {
    echo "trusted root not installed" >&2; exit 1;
}
grep -q "ZERODECK_UPDATE_TRUSTED_ROOT=${config}/trusted-root.json" "${config}/update.env" || {
    echo "update.env missing trusted root" >&2; exit 1;
}
for unit in zerodeck.service zerodeck-apply.service zerodeck-recovery.service zerodeck-check.service zerodeck-check.timer; do
    [[ -f "${units}/${unit}" ]] || { echo "unit missing: ${unit}" >&2; exit 1; }
done
if grep -qE '(^| )(start|enable)( |$)' "${FAKE_SYSTEMCTL_LOG}"; then
    echo "clean install without --start started or enabled a unit" >&2
    cat "${FAKE_SYSTEMCTL_LOG}" >&2
    exit 1
fi

# ── Dry run validates without creating any installation path ──────────────────────────────────
read -r root controller units config < <(new_install dry-run)
run_bootstrap "$(base_for "${root}")" "${manual}" "${manual_sha}" "${managed}" "${managed_sha}" \
    "${trusted_root}" "${root_sha}" --dry-run >"${tmp}/dry-run.log"
[[ ! -e "${root}" && ! -e "${controller}" && ! -e "${units}" && ! -e "${config}" ]] || {
    echo "dry run created installation paths" >&2; exit 1;
}

# ── Import copies an inactive Decky tree and records blocked root plugins ────────────────────
read -r root controller units config < <(new_install import)
import_source="${tmp}/inactive-decky"
mkdir -p "${import_source}/plugins/example"
printf 'decky plugin\n' >"${import_source}/plugins/example/main.py"
run_bootstrap "$(base_for "${root}")" "${manual}" "${manual_sha}" "${managed}" "${managed_sha}" \
    "${trusted_root}" "${root_sha}" --import-inactive-decky "${import_source}" >"${tmp}/import.log"
root_real="$(cd "${root}" && pwd)"
data_dir="$(cd "$(base_for "${root}")" && pwd)/home/deck/homebrew"
[[ ! -e "${data_dir}" ]] || { echo "Decky import created the host data root" >&2; exit 1; }
staged="$(python3 - "${root_real}/state/import.json" <<'PY2'
import json, sys
print(json.load(open(sys.argv[1], encoding="utf-8"))["staged_path"])
PY2
)"
[[ "${staged}" == "${root_real}/import/"* ]] || { echo "Decky import was not staged under the root" >&2; exit 1; }
cmp "${import_source}/plugins/example/main.py" "${staged}/plugins/example/main.py" >/dev/null || {
    echo "Decky import did not copy the plugin tree into staging" >&2; exit 1;
}
python3 - "${root_real}/state/import.json" "${data_dir}" <<'PY2'
import json, sys
document = json.load(open(sys.argv[1], encoding="utf-8"))
assert document["schema"] == 1
assert document["copy_only"] is True
assert document["root_plugins"] == "blocked"
assert document["activated"] is False
assert document["expected_data_root"] == sys.argv[2]
PY2

# ── A running ZeroDeck service is not silently converted without --start ─────────────────────
read -r root controller units config < <(new_install running-no-start)
old_generation="$(printf 'b%.0s' {1..64})"
mkdir -p "${controller}/bin" "${units}" "${root}/state"
printf 'old-controller\n' >"${controller}/bin/zerodeckctl"
chmod 0755 "${controller}/bin/zerodeckctl"
printf '%s\n' "${old_generation}" >"${root}/current"
printf '{"schema":1,"last_confirmed":"%s","pending":null}\n' "${old_generation}" >"${root}/state/update.json"
controller_before="$(fixture_sha256 "${controller}/bin/zerodeckctl")"
FAKE_ZERODECK_ACTIVE=1
export FAKE_ZERODECK_ACTIVE
expect_failure running-no-start run_bootstrap \
    "$(base_for "${root}")" "${manual}" "${manual_sha}" "${managed}" "${managed_sha}" \
    "${trusted_root}" "${root_sha}" --convert-existing-zerodeck
[[ "$(fixture_sha256 "${controller}/bin/zerodeckctl")" == "${controller_before}" ]] || {
    echo "running service was silently replaced" >&2; exit 1;
}

# ── A partial custom-path override is refused before any writes ───────────────────────────────
read -r root controller units config < <(new_install partial-paths)
expect_failure partial-paths bash "${repo_root}/deploy/update/bootstrap.sh" \
    --package "${manual}" --sha256 "${manual_sha}" \
    --generation "${managed}" --generation-sha256 "${managed_sha}" \
    --trusted-root "${trusted_root}" --trusted-root-sha256 "${root_sha}" \
    --root "${root}" \
    --systemctl "${systemctl}" \
    --metadata-url "https://updates.example.test/metadata/" \
    --targets-url "https://updates.example.test/targets/"
[[ ! -e "${root}" ]] || { echo "partial paths created the update root" >&2; exit 1; }

# ── --start waits for the authenticated readiness identity and then confirms ───────────────────
start_ready_script="${tmp}/ready-server.py"
cat >"${start_ready_script}" <<'PY'
import http.server
import json
import sys
import threading

version, build_id = sys.argv[1:3]
port_file = sys.argv[3]

class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        if self.path == "/auth/token":
            body = b"fixture-ready-token"
        elif self.path == "/readyz":
            if self.headers.get("X-Decky-Auth") != "fixture-ready-token":
                self.send_response(403)
                self.end_headers()
                return
            body = json.dumps({
                "version": version,
                "build_id": build_id,
                "python_host_ready": True,
                "invocation_id": "fixture-invocation",
            }).encode()
        else:
            self.send_response(404)
            self.end_headers()
            return
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
with open(port_file, "w", encoding="utf-8") as handle:
    handle.write(str(server.server_address[1]))
server.serve_forever()
PY
ready_port_file="${tmp}/ready-port"
python3 "${start_ready_script}" "${version}" "${build_id}" "${ready_port_file}" &
ready_pid=$!
for _ in {1..100}; do
    [[ -s "${ready_port_file}" ]] && break
    sleep 0.05
done
[[ -s "${ready_port_file}" ]] || { kill "${ready_pid}" 2>/dev/null || true; echo "readiness fixture did not start" >&2; exit 1; }
ready_port="$(cat "${ready_port_file}")"
read -r root controller units config < <(new_install start-success)
base="$(base_for "${root}")"
FAKE_ZERODECK_ACTIVE=1
export FAKE_ZERODECK_ACTIVE
run_bootstrap "${base}" "${manual}" "${manual_sha}" "${managed}" "${managed_sha}" \
    "${trusted_root}" "${root_sha}" --start \
    --ready-url "http://127.0.0.1:${ready_port}" >"${tmp}/start-success.log"
unset FAKE_ZERODECK_ACTIVE
kill "${ready_pid}" 2>/dev/null || true
wait "${ready_pid}" 2>/dev/null || true
[[ -L "${base}/usr/local/bin/zerodeckctl" ]] || {
    echo "--start did not install the root-owned controller PATH link" >&2; exit 1;
}
[[ "$(readlink "${base}/usr/local/bin/zerodeckctl")" == "${controller}/bin/zerodeckctl" ]] || {
    echo "controller PATH link points at the wrong target" >&2; exit 1;
}
python3 - "${root}/state/update.json" "${managed_sha}" <<'PY'
import json, sys
document = json.load(open(sys.argv[1], encoding="utf-8"))
assert document["last_confirmed"] == sys.argv[2], document
assert document["pending"] is None, document
PY
grep -q "zerodeck.service started" "${tmp}/start-success.log" || {
    echo "--start did not report a started service" >&2; exit 1;
}

# ── Repeat bootstrap reuses the intentional controller link; dry-run accepts an installed host ──
read -r root controller units config < <(new_install repeat-install)
base="$(base_for "${root}")"
run_bootstrap "${base}" "${manual}" "${manual_sha}" "${managed}" "${managed_sha}" \
    "${trusted_root}" "${root_sha}" >"${tmp}/repeat-first.log"
repeat_link="${base}/usr/local/bin/zerodeckctl"
[[ -L "${repeat_link}" ]] || { echo "repeat test: first install has no controller link" >&2; exit 1; }
run_bootstrap "${base}" "${manual}" "${manual_sha}" "${managed}" "${managed_sha}" \
    "${trusted_root}" "${root_sha}" --convert-existing-zerodeck \
    >"${tmp}/repeat-second.log"
[[ "$(readlink "${repeat_link}")" == "${controller}/bin/zerodeckctl" ]] || {
    echo "repeat bootstrap did not retain the intentional controller link" >&2; exit 1;
}
run_bootstrap "${base}" "${manual}" "${manual_sha}" "${managed}" "${managed_sha}" \
    "${trusted_root}" "${root_sha}" --convert-existing-zerodeck --dry-run \
    >"${tmp}/repeat-dry-run.log"
grep -q "would install generation" "${tmp}/repeat-dry-run.log" || {
    echo "dry run on an installed host did not validate" >&2; exit 1;
}

# ── A regular file at the controller link path fails before any partial installation ──────────
read -r root controller units config < <(new_install link-conflict)
base="$(base_for "${root}")"
old_generation="$(printf 'd%.0s' {1..64})"
mkdir -p "${controller}/bin" "${units}" "${root}/state" "$(dirname "${base}/usr/local/bin/zerodeckctl")"
printf 'old-controller\n' >"${controller}/bin/zerodeckctl"
chmod 0755 "${controller}/bin/zerodeckctl"
printf '[Service]\nEnvironment=OLD_UNIT=1\n' >"${units}/zerodeck.service"
printf '%s\n' "${old_generation}" >"${root}/current"
printf '{"schema":1,"last_confirmed":"%s","pending":null}\n' "${old_generation}" \
    >"${root}/state/update.json"
printf 'not-our-link\n' >"${base}/usr/local/bin/zerodeckctl"
controller_before="$(fixture_sha256 "${controller}/bin/zerodeckctl")"
current_before="$(fixture_sha256 "${root}/current")"
state_before="$(fixture_sha256 "${root}/state/update.json")"
unit_before="$(fixture_sha256 "${units}/zerodeck.service")"
FAKE_ZERODECK_ACTIVE=1
export FAKE_ZERODECK_ACTIVE
: >"${FAKE_SYSTEMCTL_LOG}"
expect_failure link-conflict run_bootstrap "${base}" "${manual}" "${manual_sha}" \
    "${managed}" "${managed_sha}" "${trusted_root}" "${root_sha}" \
    --convert-existing-zerodeck --start
unset FAKE_ZERODECK_ACTIVE
grep -q "non-symlink controller path" "${tmp}/link-conflict.log" || {
    echo "controller link conflict did not explain itself" >&2
    cat "${tmp}/link-conflict.log" >&2
    exit 1
}
[[ "$(fixture_sha256 "${controller}/bin/zerodeckctl")" == "${controller_before}" ]] || {
    echo "controller link conflict replaced the old controller" >&2; exit 1;
}
[[ "$(fixture_sha256 "${root}/current")" == "${current_before}" ]] || {
    echo "controller link conflict changed current" >&2; exit 1;
}
[[ "$(fixture_sha256 "${root}/state/update.json")" == "${state_before}" ]] || {
    echo "controller link conflict changed state" >&2; exit 1;
}
[[ "$(fixture_sha256 "${units}/zerodeck.service")" == "${unit_before}" ]] || {
    echo "controller link conflict changed the unit" >&2; exit 1;
}
if grep -qE '(^| )stop( |$)' "${FAKE_SYSTEMCTL_LOG}"; then
    echo "controller link conflict stopped the running service before refusing" >&2
    cat "${FAKE_SYSTEMCTL_LOG}" >&2
    exit 1
fi
[[ "$(cat "${base}/usr/local/bin/zerodeckctl")" == "not-our-link" ]] || {
    echo "controller link conflict overwrote the foreign file" >&2; exit 1;
}

# SteamOS keeps only allow-listed /etc files across an OS update: bootstrap registers the units.
read -r root controller units config < <(new_install keep-list)
base="$(base_for "${root}")"
keep_dir="${base}/etc/atomic-update.conf.d"
mkdir -p "${keep_dir}"
run_bootstrap "${base}" "${manual}" "${manual_sha}" "${managed}" "${managed_sha}" \
    "${trusted_root}" "${root_sha}" --keep-list-dir "${keep_dir}" >"${tmp}/keep-list.log"
keep_file="${keep_dir}/zerodeck.conf"
[[ -f "${keep_file}" ]] || { echo "bootstrap did not write ${keep_file}" >&2; exit 1; }
for entry in \
    /etc/systemd/system/zerodeck.service \
    /etc/systemd/system/zerodeck-apply.service \
    /etc/systemd/system/zerodeck-recovery.service \
    /etc/systemd/system/zerodeck-check.service \
    /etc/systemd/system/zerodeck-check.timer \
    '/etc/systemd/system/*.wants/zerodeck*' \
    /etc/atomic-update.conf.d/zerodeck.conf; do
    grep -qxF "${entry}" "${keep_file}" || {
        echo "keep-list is missing ${entry}" >&2; cat "${keep_file}" >&2; exit 1;
    }
done

# Without the SteamOS keep-list directory nothing is written for it.
read -r root controller units config < <(new_install no-keep-list)
base="$(base_for "${root}")"
run_bootstrap "${base}" "${manual}" "${manual_sha}" "${managed}" "${managed_sha}" \
    "${trusted_root}" "${root_sha}" --keep-list-dir "${base}/etc/atomic-update.conf.d" \
    >"${tmp}/no-keep-list.log"
[[ ! -e "${base}/etc/atomic-update.conf.d" ]] || {
    echo "bootstrap created the SteamOS keep-list directory on a host without it" >&2; exit 1;
}

# ── Inputs are copied and verified before use: swapping the source cannot change the install ────
# Add a large member so the extraction window is observable, then replace the caller's files as
# soon as the verified copies are complete. The installation must still come from the copies.
fixture_blob="${tmp}/large-blob"
python3 - "${fixture_blob}" <<'PY'
import os, sys
with open(sys.argv[1], "wb") as handle:
    handle.write(os.urandom(16 * 1024 * 1024))
PY
swap_assets="${tmp}/swap-assets"
mkdir -p "${swap_assets}"
fixture_release_assets "${swap_assets}" "${version}" "${build_id}" 1 >/dev/null
for archive in "${swap_assets}/zerodeck-v${version}-linux-x86_64.tar.gz" \
    "${swap_assets}/zerodeck-v${version}-linux-x86_64.managed.tar.gz"; do
    unpack="${archive}.unpack"
    mkdir -p "${unpack}"
    tar -xzf "${archive}" -C "${unpack}"
    if [[ "${archive}" == *.managed.tar.gz ]]; then
        cp "${fixture_blob}" "${unpack}/python/blob.bin"
        tar -C "${unpack}" -czf "${archive}" bin python frontend release.json
    else
        cp "${fixture_blob}" "${unpack}/zerodeck-v${version}-linux-x86_64/python/blob.bin"
        tar -C "${unpack}" -czf "${archive}" "zerodeck-v${version}-linux-x86_64"
    fi
    rm -rf "${unpack}"
done
swap_manual="${swap_assets}/zerodeck-v${version}-linux-x86_64.tar.gz"
swap_managed="${swap_assets}/zerodeck-v${version}-linux-x86_64.managed.tar.gz"
swap_manual_sha="$(fixture_sha256 "${swap_manual}")"
swap_managed_sha="$(fixture_sha256 "${swap_managed}")"
read -r root controller units config < <(new_install swap-after-verify)
base="$(base_for "${root}")"
run_bootstrap "${base}" "${swap_manual}" "${swap_manual_sha}" "${swap_managed}" \
    "${swap_managed_sha}" "${trusted_root}" "${root_sha}" >"${tmp}/swap.log" 2>&1 &
swap_pid=$!
marker=""
for _ in {1..2000}; do
    marker="$(find "${root}" -maxdepth 2 -name '.inputs-verified' -print -quit 2>/dev/null || true)"
    [[ -n "${marker}" ]] && break
    sleep 0.005
done
if [[ -z "${marker}" ]]; then
    wait "${swap_pid}" || true
    echo "bootstrap did not reach the verified-copy marker" >&2
    cat "${tmp}/swap.log" >&2
    exit 1
fi
printf 'swapped after verification\n' >"${swap_manual}"
printf 'swapped after verification\n' >"${swap_managed}"
printf 'not json\n' >"${swap_assets}/trusted-root.json"
printf 'not json\n' >"${trusted_root}"
if ! wait "${swap_pid}"; then
    echo "bootstrap failed after the source was swapped; it re-opened the untrusted path" >&2
    cat "${tmp}/swap.log" >&2
    exit 1
fi
[[ -f "${root}/generations/${swap_managed_sha}/python/blob.bin" ]] || {
    echo "install did not use the verified generation copy" >&2; exit 1;
}

echo "bootstrap tests passed"
