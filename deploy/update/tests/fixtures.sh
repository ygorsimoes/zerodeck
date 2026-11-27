#!/usr/bin/env bash
# Shared fixtures for the release/publish/signing/bootstrap shell tests.
set -euo pipefail
# macOS bsdtar otherwise stores AppleDouble `._*` files beside copied fixtures.
export COPYFILE_DISABLE=1

fixture_sha256() {
    python3 - "$1" <<'PY'
import hashlib, sys
print(hashlib.sha256(open(sys.argv[1], "rb").read()).hexdigest())
PY
}

fixture_sha256_text() {
    python3 - "$1" <<'PY'
import hashlib, sys
print(hashlib.sha256(sys.argv[1].encode()).hexdigest())
PY
}

fixture_trusted_root() {
    local destination="$1"
    # A pinned ed25519 public key (the matching private key does not exist anywhere).
    printf '{"keys": ["%s"]}\n' "$(printf '11%.0s' {1..32})" >"${destination}"
}

fixture_fake_controller() {
    local destination="$1"
    cat >"${destination}" <<'SCRIPT'
#!/usr/bin/env bash
set -euo pipefail
root=""
command=""
generation=""
check=0
convert=0
prepare=0
confirm=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --root) root="$2"; shift 2 ;;
        bootstrap) command=bootstrap; shift ;;
        --generation) generation="$2"; shift 2 ;;
        --check) check=1; shift ;;
        --prepare) prepare=1; shift ;;
        --confirm) confirm=1; shift ;;
        --convert-existing-zerodeck) convert=1; shift ;;
        *) shift ;;
    esac
done
[[ "${command}" == bootstrap ]] || { echo "unsupported command" >&2; exit 2; }
[[ -n "${root}" ]] || { echo "missing root" >&2; exit 2; }
state="${root}/state/update.json"
if [[ "${check}" -eq 1 ]]; then
    [[ -n "${generation}" ]] || { echo "missing generation" >&2; exit 2; }
    if [[ -f "${state}" ]] && grep -q '"pending"[[:space:]]*:[[:space:]]*{' "${state}"; then
        echo "bootstrap refused: pending activation" >&2
        exit 1
    fi
    echo "bootstrap check passed"
    exit 0
fi
if [[ "${confirm}" -eq 1 ]]; then
    python3 - "${state}" <<'PY2'
import json
import sys

path = sys.argv[1]
try:
    document = json.load(open(path, encoding="utf-8"))
except (OSError, json.JSONDecodeError) as error:
    raise SystemExit(f"cannot confirm bootstrap: {error}")
pending = document.get("pending")
if not isinstance(pending, dict):
    raise SystemExit("cannot confirm bootstrap without a prepared transaction")
candidate = pending["candidate"]
if document.get("current") is None:
    document["current"] = candidate
previous = pending.get("previous")
document["last_confirmed"] = candidate
if isinstance(previous, str) and previous and previous != candidate:
    document["rollback_target"] = previous
document["pending"] = None
document["last_result"] = "bootstrapped"
document["last_error"] = None
with open(path, "w", encoding="utf-8") as handle:
    json.dump(document, handle)
    handle.write("\n")
PY2
    exit 0
fi
[[ -n "${generation}" ]] || { echo "missing generation" >&2; exit 2; }
if [[ -f "${state}" ]] && grep -q '"pending"[[:space:]]*:[[:space:]]*{' "${state}"; then
    echo "bootstrap refused: pending activation" >&2
    exit 1
fi
mkdir -p "${root}/state"
previous=""
if [[ -f "${root}/current" ]]; then
    previous="$(cat "${root}/current")"
fi
if [[ "${prepare}" -eq 1 ]]; then
    python3 - "${state}" "${generation}" "${previous}" "${convert}" <<'PY2'
import json
import sys

path, generation, previous, convert = sys.argv[1:5]
document = {"schema": 1, "last_confirmed": None, "rollback_target": None,
            "quarantined": [], "pending": None, "last_result": None, "last_error": None}
try:
    document = json.load(open(path, encoding="utf-8"))
except (OSError, json.JSONDecodeError):
    pass
if document.get("last_confirmed") and document.get("last_confirmed") != generation:
    document["rollback_target"] = document["last_confirmed"]
if convert == "1" and previous and previous != generation:
    document["rollback_target"] = previous
document["pending"] = {
    "id": "00000000-0000-4000-8000-000000000001",
    "previous": previous or None,
    "candidate": generation,
    "phase": "switched",
    "was_running": False,
    "start_attempted": False,
}
document["last_result"] = None
document["last_error"] = None
with open(path, "w", encoding="utf-8") as handle:
    json.dump(document, handle)
    handle.write("\n")
PY2
    printf '%s\n' "${generation}" >"${root}/current"
    echo "prepared"
    exit 0
fi
if [[ "${convert}" -eq 1 && -n "${previous}" && "${previous}" != "${generation}" ]]; then
    printf '{"schema":1,"last_confirmed":"%s","rollback_target":"%s","quarantined":[],"pending":null,"last_result":"bootstrapped","last_error":null}\n' \
        "${generation}" "${previous}" >"${state}"
else
    printf '{"schema":1,"last_confirmed":"%s","rollback_target":null,"quarantined":[],"pending":null,"last_result":"bootstrapped","last_error":null}\n' \
        "${generation}" >"${state}"
fi
printf '%s\n' "${generation}" >"${root}/current"
SCRIPT
    chmod 0755 "${destination}"
}

fixture_fake_systemctl() {
    local destination="$1"
    cat >"${destination}" <<'SCRIPT'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"${FAKE_SYSTEMCTL_LOG:-/dev/null}"
state_dir="${FAKE_SYSTEMCTL_STATE:-}"
enabled_file="${state_dir}/enabled"
mkdir -p "${state_dir}" 2>/dev/null || true
if [[ "${1:-}" == "show" ]]; then
    property="${2:-}"
    if [[ "${property}" == "-p" ]]; then
        property="${3:-}"
    fi
    property="${property#--property=}"
    case "${property}" in
        InvocationID) echo "fixture-invocation" ;;
        MainPID)
            if [[ "${FAKE_ZERODECK_ACTIVE:-0}" == "1" ]]; then echo "1234"; else echo "0"; fi
            ;;
        ActiveState)
            if [[ "${FAKE_ZERODECK_ACTIVE:-0}" == "1" ]]; then echo "active"; else echo "inactive"; fi
            ;;
        *) echo "" ;;
    esac
    exit 0
fi
if [[ "${1:-}" == "is-active" ]]; then
    unit="${3:-}"
    case "${unit}" in
        decky.service|plugin_loader.service)
            [[ "${FAKE_DECKY_ACTIVE:-0}" == "1" ]] && exit 0
            exit 3
            ;;
        zerodeck.service)
            [[ "${FAKE_ZERODECK_ACTIVE:-0}" == "1" ]] && exit 0
            exit 3
            ;;
    esac
    exit 3
fi
if [[ "${1:-}" == "is-enabled" ]]; then
    if [[ -f "${enabled_file}" ]] && grep -qx "${2:-}" "${enabled_file}"; then
        echo enabled
        exit 0
    fi
    echo disabled
    exit 1
fi
if [[ "${1:-}" == "enable" ]]; then
    shift
    for unit in "$@"; do
        grep -qx "${unit}" "${enabled_file}" 2>/dev/null || echo "${unit}" >>"${enabled_file}"
    done
    exit 0
fi
if [[ "${1:-}" == "disable" ]]; then
    shift
    for unit in "$@"; do
        if [[ -f "${enabled_file}" ]]; then
            grep -vx "${unit}" "${enabled_file}" >"${enabled_file}.tmp" || true
            mv "${enabled_file}.tmp" "${enabled_file}"
        fi
    done
    exit 0
fi
if [[ "${1:-}" == "start" ]]; then
    if [[ "${FAKE_START_FAIL:-0}" == "1" ]]; then
        exit 1
    fi
fi
exit 0
SCRIPT
    chmod 0755 "${destination}"
}

# fixture_release_assets <root> <version> <build-id> <complete:0|1>
# Creates manual and managed archives plus the trusted root. Prints the two archive paths.
fixture_release_assets() {
    local root="$1" version="$2" build_id="$3" complete="$4"
    local wrapper="zerodeck-v${version}-linux-x86_64"
    local manual="${root}/${wrapper}"
    local managed="${root}/managed"
    local generation="${root}/generation"
    mkdir -p "${manual}/bin" "${manual}/python/zerodeck_py_host" "${manual}/frontend" \
        "${manual}/systemd" "${manual}/docs" "${generation}/bin" \
        "${generation}/python/zerodeck_py_host" "${generation}/frontend"

    printf '#!/usr/bin/env bash\nexit 0\n' >"${manual}/bin/zerodeckd"
    fixture_fake_controller "${manual}/bin/zerodeckctl"
    printf '# fixture\n' >"${manual}/python/zerodeck_py_host/__init__.py"
    printf 'export default {};\n' >"${manual}/frontend/index.js"
    cp "${FIXTURE_REPO_ROOT}/deploy/systemd/"*.service \
        "${FIXTURE_REPO_ROOT}/deploy/systemd/"*.timer "${manual}/systemd/"
    cp "${FIXTURE_REPO_ROOT}/deploy/update/bootstrap.sh" "${manual}/bootstrap.sh"
    fixture_trusted_root "${manual}/trusted-root.json"
    cp "${FIXTURE_REPO_ROOT}/deploy/update/RELEASE.md" "${manual}/docs/RELEASE.md"
    cp "${FIXTURE_REPO_ROOT}/deploy/update/COMPATIBILITY.md" \
        "${manual}/docs/COMPATIBILITY.md"
    cp "${FIXTURE_REPO_ROOT}/LICENSE" "${FIXTURE_REPO_ROOT}/README.md" "${manual}/"
    printf '{"schema":1,"build_id":"%s","version":"%s"}\n' "${build_id}" "${version}" \
        >"${manual}/release.json"
    chmod 0755 "${manual}/bin/zerodeckd" "${manual}/bin/zerodeckctl" "${manual}/bootstrap.sh"
    chmod 0644 "${manual}/release.json" "${manual}"/systemd/* "${manual}"/docs/* \
        "${manual}/LICENSE" "${manual}/README.md"

    cp "${manual}/bin/zerodeckd" "${generation}/bin/zerodeckd"
    cp -a "${manual}/python/." "${generation}/python/"
    if [[ "${complete}" == "1" ]]; then
        cp -a "${manual}/frontend/." "${generation}/frontend/"
    fi
    printf '{"schema":1,"build_id":"%s","version":"%s"}\n' "${build_id}" "${version}" \
        >"${generation}/release.json"
    chmod 0755 "${generation}/bin/zerodeckd"
    chmod 0644 "${generation}/release.json"

    (
        cd "${root}"
        tar -czf "${wrapper}.tar.gz" "${wrapper}"
        tar -C "${generation}" -czf "${wrapper}.managed.tar.gz" \
            bin python frontend release.json
    )
    fixture_trusted_root "${root}/trusted-root.json"
    echo "${root}/${wrapper}.tar.gz"
    echo "${root}/${wrapper}.managed.tar.gz"
}

# fixture_incomplete_managed <root> <version> <build-id>
# Replaces the managed archive with one missing the frontend component.
fixture_incomplete_managed() {
    local root="$1" version="$2" build_id="$3"
    local wrapper="zerodeck-v${version}-linux-x86_64"
    local managed="${root}/incomplete"
    mkdir -p "${managed}/bin" "${managed}/python/zerodeck_py_host"
    printf '#!/usr/bin/env bash\nexit 0\n' >"${managed}/bin/zerodeckd"
    chmod 0755 "${managed}/bin/zerodeckd"
    printf '# fixture\n' >"${managed}/python/zerodeck_py_host/__init__.py"
    printf '{"schema":1,"build_id":"%s","version":"%s"}\n' "${build_id}" "${version}" \
        >"${managed}/release.json"
    rm -f "${root}/${wrapper}.managed.tar.gz"
    tar -C "${managed}" -czf "${root}/${wrapper}.managed.tar.gz" \
        bin python release.json
    echo "${root}/${wrapper}.managed.tar.gz"
}
