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
: >"${FAKE_SYSTEMCTL_LOG}"

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

echo "bootstrap tests passed"
