#!/usr/bin/env bash
# Verified administrative bootstrap for a managed ZeroDeck installation.
#
# Run only after obtaining the package hashes and the TUF root from an independent, trusted channel.
# Nothing is extracted or executed until both archives and the trusted root match caller-supplied
# SHA-256 digests. A new install never starts or enables anything unless `--start` is given.
#
#   sudo ./bootstrap.sh \
#     --package zerodeck-v0.1.0-linux-x86_64.tar.gz --sha256 <manual-sha256> \
#     --generation zerodeck-v0.1.0-linux-x86_64.managed.tar.gz --generation-sha256 <managed-sha256> \
#     --trusted-root root.json --trusted-root-sha256 <root-sha256> \
#     --metadata-url https://updates.example/loader/metadata/ \
#     --targets-url https://updates.example/loader/targets/ \
#     --start
set -euo pipefail

PROGRAM="bootstrap.sh"
DEFAULT_ROOT="/var/lib/zerodeck"
DEFAULT_CONTROLLER_ROOT="/usr/local/lib/zerodeck"
DEFAULT_SYSTEMD_DIR="/etc/systemd/system"
DEFAULT_CONFIG_DIR="/etc/zerodeck"
DEFAULT_DATA_DIR="/home/deck/homebrew"

package=""
package_sha=""
generation=""
generation_sha=""
trusted_root=""
trusted_root_sha=""
metadata_url=""
targets_url=""
root="${DEFAULT_ROOT}"
controller_root="${DEFAULT_CONTROLLER_ROOT}"
systemd_dir="${DEFAULT_SYSTEMD_DIR}"
config_dir="${DEFAULT_CONFIG_DIR}"
data_dir="${DEFAULT_DATA_DIR}"
systemctl="systemctl"
convert=0
import_decky=""
start=0
dry_run=0

usage() {
    cat >&2 <<EOF
usage: ${PROGRAM} --package <manual.tar.gz> --sha256 <hex>
                  --generation <managed.tar.gz> --generation-sha256 <hex>
                  --trusted-root <root.json> --trusted-root-sha256 <hex>
                  --metadata-url URL --targets-url URL
                  [--convert-existing-zerodeck] [--import-inactive-decky PATH]
                  [--start] [--dry-run]
                  [--root DIR] [--controller-root DIR] [--systemd-dir DIR]
                  [--config-dir DIR] [--data-dir DIR] [--systemctl PATH]

The package and generation hashes must come from outside the downloaded files (release notes,
signed metadata, or another trusted channel). The TUF root is installed as the trust anchor; this
script does not trust the first root it sees.
EOF
    exit 2
}

die() {
    echo "${PROGRAM}: error: $*" >&2
    exit 1
}

warn() {
    echo "${PROGRAM}: warning: $*" >&2
}

need_value() {
    [[ $# -ge 2 && -n "$2" ]] || die "$1 requires a value"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --package) need_value "$@"; package="$2"; shift 2 ;;
        --sha256) need_value "$@"; package_sha="$2"; shift 2 ;;
        --generation) need_value "$@"; generation="$2"; shift 2 ;;
        --generation-sha256) need_value "$@"; generation_sha="$2"; shift 2 ;;
        --trusted-root) need_value "$@"; trusted_root="$2"; shift 2 ;;
        --trusted-root-sha256) need_value "$@"; trusted_root_sha="$2"; shift 2 ;;
        --metadata-url) need_value "$@"; metadata_url="$2"; shift 2 ;;
        --targets-url) need_value "$@"; targets_url="$2"; shift 2 ;;
        --root) need_value "$@"; root="$2"; shift 2 ;;
        --controller-root) need_value "$@"; controller_root="$2"; shift 2 ;;
        --systemd-dir) need_value "$@"; systemd_dir="$2"; shift 2 ;;
        --config-dir) need_value "$@"; config_dir="$2"; shift 2 ;;
        --data-dir) need_value "$@"; data_dir="$2"; shift 2 ;;
        --systemctl) need_value "$@"; systemctl="$2"; shift 2 ;;
        --convert-existing-zerodeck) convert=1; shift ;;
        --import-inactive-decky) need_value "$@"; import_decky="$2"; shift 2 ;;
        --start) start=1; shift ;;
        --dry-run) dry_run=1; shift ;;
        --help|-h) usage ;;
        *) die "unknown argument: $1" ;;
    esac
done

[[ -n "${package}" ]] || usage
[[ -n "${generation}" ]] || usage
[[ -n "${trusted_root}" ]] || usage
[[ -n "${metadata_url}" && -n "${targets_url}" ]] || usage
[[ "${package_sha}" =~ ^[0-9A-Fa-f]{64}$ ]] || die "--sha256 must be a 64-character SHA-256 hex digest"
[[ "${generation_sha}" =~ ^[0-9A-Fa-f]{64}$ ]] || die "--generation-sha256 must be a SHA-256 digest"
[[ "${trusted_root_sha}" =~ ^[0-9A-Fa-f]{64}$ ]] || die "--trusted-root-sha256 must be a SHA-256 digest"
[[ "${generation}" == *.managed.tar.gz ]] || die "the generation must be the .managed.tar.gz asset"
[[ -f "${package}" ]] || die "package not found: ${package}"
[[ -f "${generation}" ]] || die "managed generation not found: ${generation}"
[[ -f "${trusted_root}" ]] || die "trusted root not found: ${trusted_root}"
[[ -x "${systemctl}" ]] || die "systemctl not executable: ${systemctl}"
command -v python3 >/dev/null 2>&1 || die "python3 is required"
if [[ -n "${metadata_url}" && ! "${metadata_url}" =~ ^https?:// ]]; then
    die "--metadata-url must be an http(s) URL"
fi
if [[ -n "${targets_url}" && ! "${targets_url}" =~ ^https?:// ]]; then
    die "--targets-url must be an http(s) URL"
fi

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        python3 - "$1" <<'PY'
import hashlib, sys
print(hashlib.sha256(open(sys.argv[1], "rb").read()).hexdigest())
PY
    fi
}

check_digest() {
    local file="$1" expected="$2" label="$3" actual expected_lower
    actual="$(sha256_file "${file}")"
    expected_lower="$(printf '%s' "${expected}" | tr '[:upper:]' '[:lower:]')"
    if [[ "${actual}" != "${expected_lower}" ]]; then
        die "${label} SHA-256 mismatch: expected ${expected_lower}, got ${actual}"
    fi
}

# Destructive installation paths require root. Tests and operators may redirect every path below a
# temporary prefix; that never touches a system location, so it needs no privilege.
custom_paths=0
[[ "${root}" != "${DEFAULT_ROOT}" ]] && custom_paths=1
[[ "${controller_root}" != "${DEFAULT_CONTROLLER_ROOT}" ]] && custom_paths=1
[[ "${systemd_dir}" != "${DEFAULT_SYSTEMD_DIR}" ]] && custom_paths=1
[[ "${config_dir}" != "${DEFAULT_CONFIG_DIR}" ]] && custom_paths=1
if [[ "$(id -u)" -ne 0 && "${custom_paths}" -eq 0 ]]; then
    die "bootstrap must run as root (or redirect every install path with --root/--controller-root/--systemd-dir/--config-dir)"
fi

# ── Verify before extraction or execution ──────────────────────────────────────────────────────
check_digest "${package}" "${package_sha}" "manual package"
check_digest "${generation}" "${generation_sha}" "managed generation"
check_digest "${trusted_root}" "${trusted_root_sha}" "trusted root"

python3 - "${trusted_root}" <<'PY'
import json
import sys
import time

path = sys.argv[1]
try:
    document = json.load(open(path, encoding="utf-8"))
except (OSError, json.JSONDecodeError) as error:
    raise SystemExit(f"trusted root is not valid JSON: {error}")
signed = document.get("signed")
if not isinstance(signed, dict) or signed.get("_type") != "root":
    raise SystemExit("trusted root is not a TUF root document")
if not isinstance(signed.get("version"), int) or signed["version"] < 1:
    raise SystemExit("trusted root has an invalid version")
if signed.get("expires") and signed["expires"] <= time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()):
    raise SystemExit("trusted root is expired")
roles = signed.get("roles")
keys = signed.get("keys")
if not isinstance(roles, dict) or not isinstance(keys, dict) or not keys:
    raise SystemExit("trusted root has no keys or roles")
root_role = roles.get("root")
targets_role = roles.get("targets")
if not isinstance(root_role, dict) or not isinstance(targets_role, dict):
    raise SystemExit("trusted root does not define root and targets roles")
for role_name, role in (("root", root_role), ("targets", targets_role)):
    keyids = role.get("keyids")
    threshold = role.get("threshold")
    if not isinstance(keyids, list) or not keyids or threshold not in (1, 2, 3, len(keyids)):
        raise SystemExit(f"trusted root {role_name} role is incomplete")
    if any(keyid not in keys for keyid in keyids):
        raise SystemExit(f"trusted root {role_name} role references an unknown key")
signatures = document.get("signatures")
if not isinstance(signatures, list) or not signatures:
    raise SystemExit("trusted root is unsigned")
PY

ensure_private_dir() {
    local path="$1" mode="$2"
    mkdir -p "${path}"
    chmod "${mode}" "${path}"
    if [[ "$(id -u)" -eq 0 ]]; then
        chown root:root "${path}"
    fi
}

ensure_owned_tree() {
    local path="$1"
    [[ "$(id -u)" -eq 0 ]] || return 0
    local owner mode
    owner="$(python3 - "${path}" <<'PY'
import os, sys
print(os.stat(sys.argv[1]).st_uid)
PY
)"
    [[ "${owner}" == "0" ]] || die "${path} must be owned by root, found uid ${owner}"
    mode="$(python3 - "${path}" <<'PY'
import os, sys
print(os.stat(sys.argv[1]).st_mode & 0o777)
PY
)"
    if (( (mode & 8#022) != 0 )); then
        die "${path} must not be group/other writable (mode ${mode})"
    fi
}

ensure_install_path() {
    local path="$1"
    [[ -L "${path}" ]] && die "install path must not be a symlink: ${path}"
    if [[ -e "${path}" ]]; then
        ensure_owned_tree "${path}"
    fi
}

verify_root_owned_tree() {
    local path="$1"
    [[ "$(id -u)" -eq 0 ]] || return 0
    python3 - "${path}" <<'PY'
import os, sys
root = sys.argv[1]
for base, dirs, files in os.walk(root):
    for name in [base, *[os.path.join(base, entry) for entry in dirs + files]]:
        metadata = os.lstat(name)
        if os.path.islink(name):
            raise SystemExit(f"existing generation contains a symlink: {name}")
        if metadata.st_uid != 0:
            raise SystemExit(f"existing generation path is not root-owned: {name}")
        if metadata.st_mode & 0o022:
            raise SystemExit(f"existing generation path is group/other writable: {name}")
PY
}

ensure_install_path "${root}"
ensure_install_path "${controller_root}"
ensure_install_path "${systemd_dir}"
ensure_install_path "${config_dir}"
for path in generations state tuf staging backup import; do
    [[ -L "${root}/${path}" ]] && die "update root child must not be a symlink: ${root}/${path}"
done
if [[ "${dry_run}" -eq 0 ]]; then
    ensure_private_dir "${root}" 0755
    ensure_private_dir "${root}/generations" 0755
    ensure_private_dir "${root}/state" 0700
    ensure_private_dir "${root}/tuf" 0700
    ensure_private_dir "${root}/staging" 0700
    ensure_private_dir "${root}/backup" 0700
    ensure_owned_tree "${root}"
    work="${root}/.bootstrap-$$"
else
    # A dry run must not create or chmod any system path.
    work="$(mktemp -d "${TMPDIR:-/tmp}/zerodeck-bootstrap-dry-run.XXXXXX")"
fi
umask 077
mkdir -p "${work}"
cleanup() {
    rm -rf "${work}"
}
trap cleanup EXIT

extract_archive() {
    local archive="$1" destination="$2" label="$3"
    python3 - "${archive}" "${destination}" "${label}" <<'PY'
import os
import sys
import tarfile
from pathlib import PurePosixPath

archive, destination, label = sys.argv[1:4]
try:
    handle = tarfile.open(archive, "r:gz")
except (tarfile.TarError, OSError) as error:
    raise SystemExit(f"{label} is not a readable gzip tar archive: {error}")
seen = set()
try:
    for member in handle.getmembers():
        if member.issym() or member.islnk():
            raise SystemExit(f"{label} contains a link: {member.name!r}")
        if not (member.isfile() or member.isdir()):
            raise SystemExit(f"{label} contains an unsupported entry: {member.name!r}")
        name = str(PurePosixPath(member.name))
        if name.startswith("/") or any(part == ".." for part in PurePosixPath(name).parts):
            raise SystemExit(f"{label} contains an unsafe path: {member.name!r}")
        normalized = "/".join(part for part in PurePosixPath(name).parts if part not in ("", "."))
        if not normalized or normalized in seen:
            raise SystemExit(f"{label} contains an empty or duplicate path: {member.name!r}")
        seen.add(normalized)
    os.makedirs(destination, mode=0o755)
    for member in handle.getmembers():
        name = "/".join(part for part in PurePosixPath(str(PurePosixPath(member.name))).parts if part not in ("", "."))
        target = os.path.join(destination, *name.split("/"))
        if member.isdir():
            os.makedirs(target, mode=0o755, exist_ok=True)
            os.chmod(target, 0o755)
            continue
        os.makedirs(os.path.dirname(target), mode=0o755, exist_ok=True)
        extracted = handle.extractfile(member)
        if extracted is None:
            raise SystemExit(f"{label} cannot read {member.name!r}")
        with open(target, "wb") as output:
            for chunk in iter(lambda: extracted.read(1024 * 1024), b""):
                output.write(chunk)
        mode = 0o755 if (member.mode & 0o111) else 0o644
        os.chmod(target, mode)
finally:
    handle.close()
PY
}

extract_archive "${package}" "${work}/manual" "manual package"
extract_archive "${generation}" "${work}/managed" "managed generation"


if ! python3 - "${work}/manual" "${work}/managed" >"${work}/identity" <<'PY'
import json
import os
import re
import sys

manual, managed = sys.argv[1:3]
SEMVER = re.compile(r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$")

def fail(message):
    raise SystemExit(message)

def require_file(root, relative, executable=False):
    path = os.path.join(root, relative)
    if not os.path.isfile(path) or os.path.islink(path):
        fail(f"required file is missing: {path}")
    if executable and not os.access(path, os.X_OK):
        fail(f"required executable is not executable: {path}")

def require_tree(root, relative):
    path = os.path.join(root, relative)
    if not os.path.isdir(path) or os.path.islink(path):
        fail(f"required directory is missing: {path}")
    if not any(files for _base, _dirs, files in os.walk(path)):
        fail(f"required directory has no files: {path}")

def identity(root, prefix=""):
    path = os.path.join(root, prefix, "release.json")
    if not os.path.isfile(path):
        fail(f"missing release manifest: {path}")
    document = json.load(open(path, encoding="utf-8"))
    if document.get("schema") != 1:
        fail(f"{path} must use schema 1")
    if not isinstance(document.get("version"), str) or not SEMVER.match(document["version"]):
        fail(f"{path} has an invalid version")
    if not isinstance(document.get("build_id"), str) or not document["build_id"].strip():
        fail(f"{path} has an empty build_id")
    return document

entries = [entry for entry in os.listdir(manual) if entry not in (".", "..")]
if len(entries) != 1 or not os.path.isdir(os.path.join(manual, entries[0])):
    fail(f"manual package must contain one wrapper directory, found {sorted(entries)}")
wrapper = entries[0]
if not re.fullmatch(r"zerodeck-.+-linux-[A-Za-z0-9_]+", wrapper):
    fail(f"unexpected manual package wrapper: {wrapper!r}")
manual_root = os.path.join(manual, wrapper)
require_file(manual_root, "bin/zerodeckd", executable=True)
require_file(manual_root, "bin/zerodeckctl", executable=True)
require_file(manual_root, "bootstrap.sh", executable=True)
for unit in (
    "zerodeck.service",
    "zerodeck-apply.service",
    "zerodeck-recovery.service",
    "zerodeck-check.service",
    "zerodeck-check.timer",
):
    require_file(manual_root, f"systemd/{unit}")
require_tree(manual_root, "python")
require_tree(manual_root, "frontend")
manual_identity = identity(manual_root)

top = {entry for entry in os.listdir(managed) if entry not in (".", "..")}
if top != {"release.json", "bin", "python", "frontend"}:
    fail(f"managed generation has unexpected root entries: {sorted(top)}")
require_file(managed, "bin/zerodeckd", executable=True)
require_tree(managed, "python")
require_tree(managed, "frontend")
managed_identity = identity(managed)
if manual_identity["version"] != managed_identity["version"] or manual_identity["build_id"] != managed_identity["build_id"]:
    fail(
        "manual and managed assets were not built from the same inputs: "
        f"{manual_identity} vs {managed_identity}"
    )
print(manual_identity["version"])
print(manual_identity["build_id"])
PY
then
    die "manual and managed assets failed layout or identity validation"
fi
identity_version="$(sed -n '1p' "${work}/identity")"
identity_build_id="$(sed -n '2p' "${work}/identity")"
if [[ -z "${identity_version}" || -z "${identity_build_id}" ]]; then
    die "could not read the release identity from the verified archives"
fi
wrapper="$(python3 - "${work}/manual" <<'PY'
import os, sys
root = sys.argv[1]
entries = [entry for entry in os.listdir(root) if entry not in (".", "..")]
print(entries[0])
PY
)"
manual_root="${work}/manual/${wrapper}"
digest="$(printf '%s' "${generation_sha}" | tr '[:upper:]' '[:lower:]')"
generation_dir="${root}/generations/${digest}"
controller_bin="${controller_root}/bin/zerodeckctl"
import_root="${root}/import"
units=(zerodeck.service zerodeck-apply.service zerodeck-recovery.service zerodeck-check.service zerodeck-check.timer)

decky_active() {
    local unit
    for unit in decky.service decky-loader.service plugin_loader.service; do
        if "${systemctl}" is-active --quiet "${unit}" >/dev/null 2>&1; then
            return 0
        fi
    done
    return 1
}

service_active() {
    "${systemctl}" is-active --quiet zerodeck.service >/dev/null 2>&1
}

validate_decky_import() {
    [[ -n "${import_decky}" ]] || return 0
    [[ -d "${import_decky}" ]] || die "inactive Decky path is not a directory: ${import_decky}"
    [[ -L "${import_root}" ]] && die "the import staging path must not be a symlink: ${import_root}"
    if [[ -e "${import_root}" && -n "$(ls -A "${import_root}" 2>/dev/null)" ]]; then
        die "${import_root} already contains staged imports; refusing to overwrite it. \
             Resolve or remove the staged import after review."
    fi
}

if decky_active; then
    die "Decky Loader is running (decky.service/decky-loader.service/plugin_loader.service is \
         active). Stop it explicitly first; this script never stops or rewrites a running Decky \
         installation."
fi
validate_decky_import

existing=0
for unit in "${units[@]}"; do
    [[ -e "${systemd_dir}/${unit}" ]] && existing=1
done
[[ -e "${controller_bin}" ]] && existing=1
[[ -e "${root}/current" || -e "${root}/state/update.json" ]] && existing=1
if [[ "${existing}" -eq 1 && "${convert}" -eq 0 ]]; then
    die "an existing ZeroDeck installation was found under ${root} or ${controller_root}; \
         pass --convert-existing-zerodeck to retain its data and previous generation"
fi

for path in \
    "${controller_bin}" \
    "${root}/current" \
    "${root}/state/update.json" \
    "${root}/state/apply.json" \
    "${config_dir}/trusted-root.json" \
    "${config_dir}/update.env"; do
    [[ -L "${path}" ]] && die "refusing to replace a symlinked installation path: ${path}"
done
for unit in "${units[@]}"; do
    [[ -L "${systemd_dir}/${unit}" ]] && die "refusing to replace a symlinked unit: ${systemd_dir}/${unit}"
done

was_active=0
if service_active; then
    was_active=1
    if [[ "${start}" -eq 0 ]]; then
        die "zerodeck.service is running; pass --start to stop and restart it as part of the \
             conversion, or stop it explicitly before bootstrapping"
    fi
fi

if [[ "${dry_run}" -eq 1 ]]; then
    echo "${PROGRAM}: verified ${package} and ${generation}"
    echo "${PROGRAM}: would install generation ${digest} (version ${identity_version}, build ${identity_build_id})"
    echo "${PROGRAM}: would install controller to ${controller_bin}"
    echo "${PROGRAM}: would install units to ${systemd_dir}"
    echo "${PROGRAM}: start requested: ${start}"
    exit 0
fi

# ── Install the generation under its verified archive digest ───────────────────────────────────
generation_existed=0
if [[ -L "${generation_dir}" ]]; then
    die "generation path must not be a symlink: ${generation_dir}"
fi
if [[ -d "${generation_dir}" ]]; then
    generation_existed=1
    verify_root_owned_tree "${generation_dir}"
    python3 - "${generation_dir}/release.json" "${identity_version}" "${identity_build_id}" <<'PY'
import json, sys
path, version, build_id = sys.argv[1:4]
try:
    document = json.load(open(path, encoding="utf-8"))
except (OSError, json.JSONDecodeError) as error:
    raise SystemExit(f"existing generation manifest is unreadable: {error}")
if document.get("schema") != 1 or document.get("version") != version or document.get("build_id") != build_id:
    raise SystemExit("an existing generation directory has a different identity; refusing to overwrite it")
PY
else
    staged_generation="${root}/generations/.bootstrap-${digest}-$$"
    rm -rf "${staged_generation}"
    mv "${work}/managed" "${staged_generation}"
    if [[ "$(id -u)" -eq 0 ]]; then
        chown -R root:root "${staged_generation}"
    fi
    find "${staged_generation}" -type d -exec chmod 0755 {} +
    find "${staged_generation}" -type f -exec chmod 0644 {} +
    chmod 0755 "${staged_generation}/bin/zerodeckd"
    mv "${staged_generation}" "${generation_dir}"
fi

temp_controller="${manual_root}/bin/zerodeckctl"
check_arguments=(--root "${root}" bootstrap --generation "${digest}" --check)
if [[ "${convert}" -eq 1 ]]; then
    check_arguments+=(--convert-existing-zerodeck)
fi
if ! "${temp_controller}" "${check_arguments[@]}"; then
    if [[ "${generation_existed}" -eq 0 ]]; then
        rm -rf "${generation_dir}"
    fi
    die "the new controller refused the bootstrap plan (pending activation, corrupt state, or an \
         unsafe existing installation); nothing was replaced"
fi

# ── Snapshot everything the conversion can replace, then install ──────────────────────────────
backup_dir="${root}/backup/bootstrap-$(date -u +%Y%m%dT%H%M%SZ)-$$"
mkdir -p "${backup_dir}/systemd" "${backup_dir}/config" "${backup_dir}/root"
chmod 0700 "${backup_dir}"
[[ -e "${controller_bin}" ]] && cp -a "${controller_bin}" "${backup_dir}/controller"
for unit in "${units[@]}"; do
    if [[ -e "${systemd_dir}/${unit}" ]]; then
        cp -a "${systemd_dir}/${unit}" "${backup_dir}/systemd/${unit}"
    fi
done
if [[ -d "${systemd_dir}/zerodeck.service.d" ]]; then
    cp -a "${systemd_dir}/zerodeck.service.d" "${backup_dir}/systemd/dropins"
fi
for name in current state/update.json state/apply.json; do
    if [[ -e "${root}/${name}" ]]; then
        mkdir -p "$(dirname "${backup_dir}/root/${name}")"
        cp -a "${root}/${name}" "${backup_dir}/root/${name}"
    fi
done
for name in trusted-root.json update.env; do
    if [[ -e "${config_dir}/${name}" ]]; then
        cp -a "${config_dir}/${name}" "${backup_dir}/config/${name}"
    fi
done
# Restore exact enablement, not merely unit files: `systemctl enable` owns the *.wants links.
: >"${backup_dir}/enabled.tsv"
for unit in "${units[@]}"; do
    if state="$("${systemctl}" is-enabled "${unit}" 2>/dev/null)"; then
        rc=0
    else
        rc=$?
        state="${state:-disabled}"
    fi
    printf '%s\t%s\t%s\n' "${unit}" "${rc}" "${state}" >>"${backup_dir}/enabled.tsv"
done
chmod 0600 "${backup_dir}/enabled.tsv"
{
    printf 'version=%s\n' "${identity_version}"
    printf 'build_id=%s\n' "${identity_build_id}"
    printf 'generation=%s\n' "${digest}"
    printf 'was_active=%s\n' "${was_active}"
    printf 'converted=%s\n' "${existing}"
} >"${backup_dir}/MANIFEST"
chmod 0600 "${backup_dir}/MANIFEST"

if [[ "${was_active}" -eq 1 ]]; then
    "${systemctl}" stop zerodeck.service || die "cannot stop the existing zerodeck.service"
fi

install_controller() {
    mkdir -p "$(dirname "${controller_bin}")"
    chmod 0755 "$(dirname "${controller_bin}")"
    local staged="${controller_bin}.new.$$"
    install -m 0755 "${temp_controller}" "${staged}"
    mv "${staged}" "${controller_bin}"
    if [[ "$(id -u)" -eq 0 ]]; then
        chown root:root "${controller_bin}"
    fi
}

install_config() {
    mkdir -p "${config_dir}"
    chmod 0755 "${config_dir}"
    local staged="${config_dir}/.trusted-root.$$"
    install -m 0644 "${trusted_root}" "${staged}"
    mv "${staged}" "${config_dir}/trusted-root.json"
    local env_staged="${config_dir}/.update.env.$$"
    {
        echo "# Generated by ZeroDeck bootstrap.sh; keep this file root-owned."
        echo "ZERODECK_UPDATE_TRUSTED_ROOT=${config_dir}/trusted-root.json"
        [[ -n "${metadata_url}" ]] && echo "ZERODECK_UPDATE_METADATA_URL=${metadata_url}"
        [[ -n "${targets_url}" ]] && echo "ZERODECK_UPDATE_TARGETS_URL=${targets_url}"
    } >"${env_staged}"
    chmod 0644 "${env_staged}"
    mv "${env_staged}" "${config_dir}/update.env"
    if [[ "$(id -u)" -eq 0 ]]; then
        chown -R root:root "${config_dir}/trusted-root.json" "${config_dir}/update.env"
    fi
}

install_units() {
    local unit staged
    mkdir -p "${systemd_dir}"
    chmod 0755 "${systemd_dir}"
    for unit in "${units[@]}"; do
        [[ -L "${systemd_dir}/${unit}" ]] && die "refusing to replace a symlinked unit: ${systemd_dir}/${unit}"
        staged="${systemd_dir}/.${unit}.new.$$"
        install -m 0644 "${manual_root}/systemd/${unit}" "${staged}"
        mv "${staged}" "${systemd_dir}/${unit}"
        if [[ "$(id -u)" -eq 0 ]]; then
            chown root:root "${systemd_dir}/${unit}"
        fi
    done
}

rollback_install() {
    local reason="$1"
    set +e
    "${systemctl}" stop zerodeck.service >/dev/null 2>&1
    local unit
    for unit in "${units[@]}"; do
        rm -f "${systemd_dir}/${unit}"
        if [[ -e "${backup_dir}/systemd/${unit}" ]]; then
            cp -a "${backup_dir}/systemd/${unit}" "${systemd_dir}/${unit}"
        fi
    done
    if [[ -f "${backup_dir}/enabled.tsv" ]]; then
        while IFS=$'\t' read -r unit rc state; do
            if [[ "${rc}" == "0" && "${state}" == "enabled" ]]; then
                "${systemctl}" enable "${unit}" >/dev/null 2>&1 || \
                    warn "could not restore enabled state for ${unit} (${state})"
            else
                "${systemctl}" disable "${unit}" >/dev/null 2>&1 || true
            fi
        done <"${backup_dir}/enabled.tsv"
    else
        for unit in "${units[@]}"; do
            "${systemctl}" disable "${unit}" >/dev/null 2>&1 || true
        done
    fi
    if [[ -e "${backup_dir}/controller" ]]; then
        mkdir -p "$(dirname "${controller_bin}")"
        cp -a "${backup_dir}/controller" "${controller_bin}"
    else
        rm -f "${controller_bin}"
    fi
    local name
    for name in trusted-root.json update.env; do
        rm -f "${config_dir}/${name}"
        if [[ -e "${backup_dir}/config/${name}" ]]; then
            cp -a "${backup_dir}/config/${name}" "${config_dir}/${name}"
        fi
    done
    for name in current state/update.json state/apply.json; do
        rm -f "${root}/${name}"
        if [[ -e "${backup_dir}/root/${name}" ]]; then
            mkdir -p "$(dirname "${root}/${name}")"
            cp -a "${backup_dir}/root/${name}" "${root}/${name}"
        fi
    done
    if [[ "${generation_existed}" -eq 0 ]]; then
        rm -rf "${generation_dir}"
    fi
    "${systemctl}" daemon-reload >/dev/null 2>&1
    if [[ "${was_active}" -eq 1 ]]; then
        "${systemctl}" start zerodeck.service >/dev/null 2>&1 || \
            warn "previous zerodeck.service did not restart; inspect it manually"
    fi
    set -e
    echo "${PROGRAM}: rolled back to the previous installation after: ${reason}" >&2
}

if ! install_controller; then
    rollback_install "controller installation failed"
    exit 1
fi
if ! install_config; then
    rollback_install "trusted root or update environment installation failed"
    exit 1
fi
if ! "${controller_bin}" --root "${root}" bootstrap --generation "${digest}" \
        $( [[ "${convert}" -eq 1 ]] && echo --convert-existing-zerodeck ); then
    rollback_install "controller bootstrap failed"
    exit 1
fi
chmod 0600 "${root}/state/update.json" 2>/dev/null || true
chmod 0644 "${root}/current" 2>/dev/null || true

if ! install_units; then
    rollback_install "systemd unit installation failed"
    exit 1
fi
if ! "${systemctl}" daemon-reload; then
    rollback_install "systemctl daemon-reload failed"
    exit 1
fi

if [[ -n "${import_decky}" ]]; then
    imported_at="$(date -u +%Y%m%dT%H%M%SZ)"
    staged_import="${import_root}/decky-${imported_at}-$$"
    if ! mkdir -p "${import_root}" "${staged_import}" \
        || ! chmod 0700 "${import_root}" "${staged_import}" \
        || ! cp -a "${import_decky}/." "${staged_import}/"; then
        rm -rf "${staged_import}"
        rollback_install "inactive Decky import failed"
        exit 1
    fi
    if [[ "$(id -u)" -eq 0 ]]; then
        chown -R root:root "${import_root}"
    fi
    python3 - "${root}/state/import.json" "${import_decky}" "${staged_import}" \
        "${data_dir}" "${imported_at}" <<'PY'
import json, os, sys
path, source, staged, expected_data, imported_at = sys.argv[1:6]
document = {
    "schema": 1,
    "source": os.path.abspath(source),
    "staged_path": os.path.abspath(staged),
    "expected_data_root": os.path.abspath(expected_data),
    "imported_at": imported_at,
    "copy_only": True,
    "root_plugins": "blocked",
    "activated": False,
    "adoption": "review the staged copy, then install individual plugins with a verified package; imported root plugins are never activated from this staging tree",
}
temp = path + ".tmp"
with open(temp, "w", encoding="utf-8") as handle:
    json.dump(document, handle, indent=2)
    handle.write("\n")
os.replace(temp, path)
PY
    chmod 0644 "${root}/state/import.json"
    echo "${PROGRAM}: staged inactive Decky import at ${staged_import}; nothing was activated"
    echo "${PROGRAM}: imported root plugins are blocked and the host data root ${data_dir} was not created"
fi

if [[ "${start}" -eq 1 ]]; then
    if ! "${systemctl}" enable zerodeck-recovery.service zerodeck.service; then
        rollback_install "could not enable the managed units"
        exit 1
    fi
    # `start` already waits for a Type=notify unit to send READY=1; `--wait` additionally blocks
    # until the long-running daemon exits, which is never the intent here.
    if ! "${systemctl}" start zerodeck.service; then
        rollback_install "zerodeck.service did not start"
        exit 1
    fi
    active=0
    for (( attempt = 0; attempt < 15; attempt++ )); do
        if "${systemctl}" is-active --quiet zerodeck.service; then
            active=1
            break
        fi
        sleep 1
    done
    if [[ "${active}" -eq 0 ]]; then
        rollback_install "zerodeck.service did not become active"
        exit 1
    fi
fi

echo "${PROGRAM}: installed generation ${digest} (version ${identity_version}, build ${identity_build_id})"
echo "${PROGRAM}: controller ${controller_bin}"
echo "${PROGRAM}: previous installation backup ${backup_dir}"
if [[ "${start}" -eq 1 ]]; then
    echo "${PROGRAM}: zerodeck.service started"
else
    echo "${PROGRAM}: units installed but not enabled or started; rerun with --start or use systemctl"
fi

