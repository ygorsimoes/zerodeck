#!/usr/bin/env bash
# Disposable Linux/systemd VM gate for managed ZeroDeck updates.
#
# The harness boots a caller-supplied x86_64 Linux image under QEMU with a qcow2 overlay, so the
# source image is never written. It installs the supplied release with the packaged `bootstrap.sh`,
# then drives the production controller and systemd units over SSH through update, rollback and
# crash/recovery scenarios.
#
# Requirements: bash, python3, QEMU, qemu-img, ssh, scp, and an image that boots systemd with SSH.
# KVM is optional; without it QEMU uses TCG, which is much slower. A real image is not required to
# test the harness: `vm.sh --self-test` uses fake qemu/ssh binaries and checks command construction,
# checksum refusal, and cleanup/failure propagation.
#
# Exit status: 0 all scenarios passed; 1 a scenario or setup failed; 77 prerequisites are absent
# (`skipped: no qemu/image`). A skip is never reported as a pass.
set -euo pipefail

PROGRAM="vm.sh"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"
PYTHON="${PYTHON:-python3}"
ZERODECK_CTL="/home/.zerodeck/bin/zerodeckctl"
DEFAULT_GUEST_DIR_PREFIX="/var/tmp/zerodeck-vm"

image=""
image_sha256=""
ssh_key=""
ssh_user="root"
ssh_port="0"
manual=""
managed=""
candidate=""
sha256sums=""
manual_sha256=""
managed_sha256=""
candidate_sha256=""
install_mode="auto"
work_dir=""
keep_work=0
qemu_bin="${QEMU_SYSTEM_X86_64:-qemu-system-x86_64}"
qemu_img_bin="${QEMU_IMG:-qemu-img}"
ssh_bin="${SSH:-ssh}"
scp_bin="${SCP:-scp}"
memory_mib="2048"
cpus="2"
accel="auto"
machine="q35"
cpu="max"
firmware=""
boot_timeout="300"
scenario_timeout="300"
drive_cache="none"
self_test=0
extra_qemu_args=()

work=""
work_owned=0
overlay=""
qemu_pid=""
guest_dir=""
ssh_opts=()
scp_opts=()
results=()
last_invocation_id=""
image_source_digest=""

usage() {
    cat >&2 <<EOF
usage: ${PROGRAM} --image <bootable.img> --ssh-key <private-key> \\
                  --manual <manual.tar.gz> --managed <managed.tar.gz> \\
                  [--candidate <next-managed.tar.gz>] [options]

Required:
  --image PATH              Bootable x86_64 Linux image with systemd and SSH.
  --ssh-key PATH            Private key accepted by the image.
  --manual PATH             Manual release archive (contains bootstrap.sh and zerodeckctl).
  --managed PATH            Managed generation archive for the same release.

Optional:
  --ssh-user USER           SSH login user (default: root; uses sudo -n otherwise).
  --candidate PATH          Managed archive for a genuinely newer release. If omitted the harness
                            repacks the installed generation with a different digest and the same
                            release identity; that proves apply/rollback but not a version upgrade.
  --mode auto|clean|convert  Auto-detect an existing /home/.zerodeck/current and pass
                            --convert-existing-zerodeck when present (default: auto).
  --image-sha256 HEX        Required image digest; a mismatch fails before QEMU starts.
  --sha256sums PATH         SHA256SUMS.txt covering the release archives.
  --manual-sha256 HEX       Expected manual archive digest.
  --managed-sha256 HEX      Expected managed archive digest.
  --candidate-sha256 HEX    Expected candidate archive digest.
  --work-dir PATH           Keep logs and the overlay under PATH (not deleted).
  --qemu PATH               qemu-system-x86_64 binary (default: ${qemu_bin}).
  --qemu-img PATH           qemu-img binary (default: ${qemu_img_bin}).
  --ssh PATH                ssh binary (default: ${ssh_bin}).
  --scp PATH                scp binary (default: ${scp_bin}).
  --ssh-port PORT           Host SSH forward port (default: choose a free port).
  --memory MiB              Guest RAM (default: ${memory_mib}).
  --cpus N                  Guest vCPUs (default: ${cpus}).
  --accel auto|tcg|kvm|hvf  QEMU accelerator (default: auto; TCG is very slow).
  --machine NAME            QEMU machine (default: q35).
  --cpu MODEL               QEMU CPU model (default: max).
  --firmware PATH           Optional -bios/pflash firmware file.
  --drive-cache MODE        QEMU drive cache mode (default: none, for crash tests).
  --boot-timeout SECONDS    Wait for SSH after boot (default: ${boot_timeout}).
  --scenario-timeout SEC    Per-recovery wait (default: ${scenario_timeout}).
  --qemu-arg ARG            Extra QEMU argument, repeated as one argv element.
  --keep-work               Keep the default temporary work directory.
  --self-test               Run fake-qemu/fake-SSH harness tests (no image needed).
  --help                    Show this help.

The script never writes to the source image and never touches a real Steam Deck. It always removes
its QEMU process and temporary overlay with an EXIT trap; a skip exits 77, never 0.
EOF
    exit 2
}

log() {
    printf '[vm] %s\n' "$*"
}

die() {
    printf '%s: error: %s\n' "${PROGRAM}" "$*" >&2
    exit 1
}

skip() {
    printf 'skipped: %s\n' "$*" >&2
    exit 77
}

need_value() {
    [[ $# -ge 2 && -n "$2" ]] || die "$1 requires a value"
}

parse_args() {
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --image) need_value "$@"; image="$2"; shift 2 ;;
            --image-sha256) need_value "$@"; image_sha256="$2"; shift 2 ;;
            --ssh-key) need_value "$@"; ssh_key="$2"; shift 2 ;;
            --ssh-user) need_value "$@"; ssh_user="$2"; shift 2 ;;
            --ssh-port) need_value "$@"; ssh_port="$2"; shift 2 ;;
            --manual) need_value "$@"; manual="$2"; shift 2 ;;
            --managed) need_value "$@"; managed="$2"; shift 2 ;;
            --candidate) need_value "$@"; candidate="$2"; shift 2 ;;
            --mode) need_value "$@"; install_mode="$2"; shift 2 ;;
            --sha256sums) need_value "$@"; sha256sums="$2"; shift 2 ;;
            --manual-sha256) need_value "$@"; manual_sha256="$2"; shift 2 ;;
            --managed-sha256) need_value "$@"; managed_sha256="$2"; shift 2 ;;
            --candidate-sha256) need_value "$@"; candidate_sha256="$2"; shift 2 ;;
            --work-dir) need_value "$@"; work_dir="$2"; shift 2 ;;
            --keep-work) keep_work=1; shift ;;
            --qemu) need_value "$@"; qemu_bin="$2"; shift 2 ;;
            --qemu-img) need_value "$@"; qemu_img_bin="$2"; shift 2 ;;
            --ssh) need_value "$@"; ssh_bin="$2"; shift 2 ;;
            --scp) need_value "$@"; scp_bin="$2"; shift 2 ;;
            --memory) need_value "$@"; memory_mib="$2"; shift 2 ;;
            --cpus) need_value "$@"; cpus="$2"; shift 2 ;;
            --accel) need_value "$@"; accel="$2"; shift 2 ;;
            --machine) need_value "$@"; machine="$2"; shift 2 ;;
            --cpu) need_value "$@"; cpu="$2"; shift 2 ;;
            --firmware) need_value "$@"; firmware="$2"; shift 2 ;;
            --drive-cache) need_value "$@"; drive_cache="$2"; shift 2 ;;
            --boot-timeout) need_value "$@"; boot_timeout="$2"; shift 2 ;;
            --scenario-timeout) need_value "$@"; scenario_timeout="$2"; shift 2 ;;
            --qemu-arg) need_value "$@"; extra_qemu_args+=("$2"); shift 2 ;;
            --self-test) self_test=1; shift ;;
            --help|-h) usage ;;
            *) die "unknown argument: $1" ;;
        esac
    done
}

sha256_file() {
    "${PYTHON}" - "$1" <<'PY'
import hashlib
import sys

digest = hashlib.sha256()
with open(sys.argv[1], "rb") as handle:
    for chunk in iter(lambda: handle.read(1024 * 1024), b""):
        digest.update(chunk)
print(digest.hexdigest())
PY
}

lower_hex() {
    printf '%s' "$1" | tr '[:upper:]' '[:lower:]'
}

expected_from_sums() {
    local sums="$1" name="$2"
    "${PYTHON}" - "${sums}" "${name}" <<'PY'
import sys

sums, name = sys.argv[1:3]
for line in open(sums, encoding="utf-8"):
    line = line.strip()
    if not line:
        continue
    parts = line.split(None, 1)
    if len(parts) == 2 and parts[1].lstrip("*") == name:
        print(parts[0].lower())
        raise SystemExit(0)
raise SystemExit(f"{sums} does not cover {name}")
PY
}

check_digest() {
    local path="$1" expected="$2" label="$3"
    [[ -f "${path}" ]] || die "${label} not found: ${path}"
    local actual
    actual="$(sha256_file "${path}")"
    if [[ -n "${expected}" ]]; then
        expected="$(lower_hex "${expected}")"
        [[ "${expected}" =~ ^[0-9a-f]{64}$ ]] || die "${label} expected digest is not SHA-256: ${expected}"
        if [[ "${actual}" != "${expected}" ]]; then
            die "${label} SHA-256 mismatch: expected ${expected}, got ${actual}"
        fi
    fi
    printf '%s\n' "${actual}"
}

archive_identity() {
    "${PYTHON}" - "$1" "$2" <<'PY'
import json
import sys
import tarfile

path, mode = sys.argv[1:3]
with tarfile.open(path, "r:gz") as archive:
    names = archive.getnames()
    prefix = ""
    if mode == "manual":
        tops = {name.split("/", 1)[0] for name in names}
        if len(tops) != 1:
            raise SystemExit(f"manual archive has {len(tops)} top-level entries")
        prefix = next(iter(tops)) + "/"
    member = archive.getmember(prefix + "release.json")
    handle = archive.extractfile(member)
    if handle is None:
        raise SystemExit("release.json is not readable")
    document = json.load(handle)
print(document["version"])
print(document["build_id"])
PY
}

archive_size() {
    "${PYTHON}" - "$1" <<'PY'
import os
import sys
print(os.path.getsize(sys.argv[1]))
PY
}

validate_archive() {
    local archive="$1" mode="$2" version="$3" build_id="$4"
    ZERODECK_SKIP_CHECKSUMS=1 \
    ZERODECK_EXPECTED_VERSION="${version}" \
    ZERODECK_EXPECTED_BUILD_ID="${build_id}" \
        "${PYTHON}" "${SCRIPT_DIR}/archive.py" validate "${archive}" "${mode}" >/dev/null
}

free_port() {
    "${PYTHON}" - <<'PY'
import socket

with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
    sock.bind(("127.0.0.1", 0))
    print(sock.getsockname()[1])
PY
}

normalize_accel() {
    local requested="$1"
    case "${requested}" in
        auto)
            if [[ -w /dev/kvm ]]; then
                printf 'kvm\n'
            else
                printf 'tcg,thread=multi\n'
            fi
            ;;
        tcg) printf 'tcg,thread=multi\n' ;;
        kvm) printf 'kvm\n' ;;
        hvf) printf 'hvf\n' ;;
        *) die "unsupported accelerator: ${requested}" ;;
    esac
}

prepare_work() {
    if [[ -n "${work_dir}" ]]; then
        mkdir -p "${work_dir}"
        work="${work_dir}"
    else
        work="$(mktemp -d "${TMPDIR:-/tmp}/zerodeck-vm.XXXXXX")"
        work_owned=1
    fi
    mkdir -p "${work}/logs"
    log "work directory: ${work}"
}

qemu_alive() {
    [[ -n "${qemu_pid}" ]] && kill -0 "${qemu_pid}" 2>/dev/null
}

create_overlay() {
    local image_format
    image_format="$("${qemu_img_bin}" info --output=json "${image}" | "${PYTHON}" -c 'import json,sys; print(json.load(sys.stdin)["format"])')"
    overlay="${work}/overlay.qcow2"
    rm -f "${overlay}"
    "${qemu_img_bin}" create -f qcow2 -F "${image_format}" -b "${image}" "${overlay}" >/dev/null
    log "overlay ${overlay} (backing ${image}, format ${image_format})"
}

start_qemu() {
    qemu_alive && die "QEMU is already running"
    rm -f "${work}/qemu.pid" "${work}/monitor.sock"
    local accel_spec
    accel_spec="$(normalize_accel "${accel}")"
    qemu_args=(
        -name "zerodeck-updater-vm"
        -machine "${machine}"
        -accel "${accel_spec}"
        -cpu "${cpu}"
        -m "${memory_mib}"
        -smp "${cpus}"
        -drive "file=${overlay},if=virtio,format=qcow2,cache=${drive_cache}"
        -netdev "user,id=net0,hostfwd=tcp:127.0.0.1:${ssh_port}-:22"
        -device "virtio-net-pci,netdev=net0"
        -display none
        -serial "file:${work}/logs/console.log"
        -serial "file:${work}/logs/markers.log"
        -monitor "unix:${work}/monitor.sock,server=on,wait=off"
    )
    if [[ -n "${firmware}" ]]; then
        qemu_args+=(-bios "${firmware}")
    fi
    if [[ ${#extra_qemu_args[@]} -gt 0 ]]; then
        qemu_args+=("${extra_qemu_args[@]}")
    fi
    printf '%s\n' "${qemu_args[*]}" >"${work}/logs/qemu-command.log"
    log "starting QEMU (accel ${accel_spec}; TCG is much slower than KVM)"
    "${qemu_bin}" "${qemu_args[@]}" >>"${work}/logs/qemu.log" 2>&1 &
    qemu_pid=$!
    sleep 1
    if ! qemu_alive; then
        tail -n 40 "${work}/logs/qemu.log" >&2 || true
        die "QEMU exited during startup; see ${work}/logs/qemu.log"
    fi
}

wait_ssh() {
    local deadline=$((SECONDS + boot_timeout))
    log "waiting for SSH on 127.0.0.1:${ssh_port} (timeout ${boot_timeout}s)"
    while (( SECONDS < deadline )); do
        if ! qemu_alive; then
            tail -n 60 "${work}/logs/qemu.log" >&2 || true
            die "QEMU exited before SSH became reachable"
        fi
        if "${ssh_bin}" "${ssh_opts[@]}" "${ssh_user}@127.0.0.1" true </dev/null >/dev/null 2>&1; then
            log "SSH is reachable"
            return 0
        fi
        sleep 2
    done
    tail -n 80 "${work}/logs/console.log" >&2 || true
    die "timed out waiting for SSH after ${boot_timeout}s"
}

wait_ssh_down() {
    local deadline=$((SECONDS + 60))
    while (( SECONDS < deadline )); do
        if ! "${ssh_bin}" "${ssh_opts[@]}" "${ssh_user}@127.0.0.1" true </dev/null >/dev/null 2>&1; then
            return 0
        fi
        sleep 1
    done
    return 1
}

guest_cmd() {
    local quoted
    printf -v quoted '%q ' "$@"
    "${ssh_bin}" "${ssh_opts[@]}" "${ssh_user}@127.0.0.1" "${quoted}" </dev/null
}

guest_root_cmd() {
    if [[ "${ssh_user}" == "root" ]]; then
        guest_cmd "$@"
    else
        guest_cmd sudo -n "$@"
    fi
}

guest_script_impl() {
    local prefix="$1"
    shift
    local script
    script="$(cat)"
    local quoted=""
    if [[ $# -gt 0 ]]; then
        printf -v quoted '%q ' "$@"
    fi
    "${ssh_bin}" "${ssh_opts[@]}" "${ssh_user}@127.0.0.1" "${prefix}bash -s -- ${quoted}" <<<"${script}"
}

guest_script() {
    guest_script_impl "" "$@"
}

guest_root_script() {
    if [[ "${ssh_user}" == "root" ]]; then
        guest_script_impl "" "$@"
    else
        guest_script_impl "sudo -n " "$@"
    fi
}

upload() {
    local source="$1" destination="$2"
    "${scp_bin}" "${scp_opts[@]}" "${source}" "${ssh_user}@127.0.0.1:${destination}"
}

hard_poweroff() {
    if qemu_alive; then
        log "killing QEMU (hard power loss)"
        kill -9 "${qemu_pid}" 2>/dev/null || true
    fi
    wait "${qemu_pid}" 2>/dev/null || true
    qemu_pid=""
}

stop_qemu() {
    if qemu_alive; then
        log "requesting guest poweroff"
        guest_root_cmd systemctl poweroff >/dev/null 2>&1 || true
        local deadline=$((SECONDS + 30))
        while qemu_alive && (( SECONDS < deadline )); do
            sleep 1
        done
        if qemu_alive; then
            kill -TERM "${qemu_pid}" 2>/dev/null || true
            sleep 2
        fi
        if qemu_alive; then
            kill -9 "${qemu_pid}" 2>/dev/null || true
        fi
        wait "${qemu_pid}" 2>/dev/null || true
    fi
    qemu_pid=""
}

cleanup() {
    local status=$?
    trap - EXIT INT TERM
    if [[ "${status}" != "0" ]] && qemu_alive; then
        # Leave the guest's view of the failure in the log; best effort, bounded.
        printf '[vm] guest journal after the failure:\n' >&2
        guest_root_cmd journalctl --no-pager -n 60 -u zerodeck.service -u zerodeck-apply.service \
            -u zerodeck-recovery.service >&2 2>&1 || true
    fi
    if qemu_alive; then
        kill -TERM "${qemu_pid}" 2>/dev/null || true
        local deadline=$((SECONDS + 10))
        while qemu_alive && (( SECONDS < deadline )); do
            sleep 1
        done
        if qemu_alive; then
            kill -9 "${qemu_pid}" 2>/dev/null || true
        fi
        wait "${qemu_pid}" 2>/dev/null || true
    fi
    if [[ -n "${image_source_digest}" && -f "${image}" ]]; then
        local digest
        digest="$(sha256_file "${image}" 2>/dev/null || true)"
        if [[ "${digest}" != "${image_source_digest}" ]]; then
            printf '[vm] error: source image changed during the run\n' >&2
            [[ "${status}" == "0" ]] && status=1
        fi
    fi
    if [[ -n "${work}" && -d "${work}" ]]; then
        if [[ "${work_owned}" == "1" && "${keep_work}" != "1" ]]; then
            rm -rf "${work}"
        else
            printf '[vm] logs kept in %s\n' "${work}" >&2
        fi
    fi
    exit "${status}"
}

guest_preflight() {
    guest_root_script <<'SCRIPT'
set -euo pipefail
[[ "$(ps -p 1 -o comm= 2>/dev/null)" == "systemd" ]] || {
    echo "guest: PID 1 is not systemd" >&2
    exit 1
}
for tool in python3 systemctl sha256sum; do
    command -v "${tool}" >/dev/null 2>&1 || {
        echo "guest: required tool is missing: ${tool}" >&2
        exit 1
    }
done
mkdir -p /home/deck/homebrew/services /home/deck/homebrew/plugins
if ! id deck >/dev/null 2>&1; then
    if command -v useradd >/dev/null 2>&1; then
        useradd --create-home --shell /bin/bash deck 2>/dev/null || true
    fi
fi
if id deck >/dev/null 2>&1; then
    chown -R deck:deck /home/deck/homebrew 2>/dev/null || true
fi
chmod 0755 /home/deck/homebrew
printf 'guest: systemd, python3, and /home/deck/homebrew are ready\n'
SCRIPT
}

extract_bootstrap() {
    local archive="$1" destination="$2"
    "${PYTHON}" - "${archive}" "${destination}" <<'PY'
import sys
import tarfile

archive_path, destination = sys.argv[1:3]
with tarfile.open(archive_path, "r:gz") as archive:
    matches = [
        member for member in archive.getmembers()
        if member.isfile() and member.name.endswith("/bootstrap.sh")
    ]
    if len(matches) != 1:
        raise SystemExit(f"manual archive must contain exactly one bootstrap.sh, found {len(matches)}")
    handle = archive.extractfile(matches[0])
    if handle is None:
        raise SystemExit("bootstrap.sh is not readable")
    with open(destination, "wb") as output:
        output.write(handle.read())
PY
    chmod 0755 "${destination}"
}

write_trusted_root() {
    # A pinned ed25519 public key (the matching private key does not exist anywhere).
    printf '{"keys": ["%s"]}\n' "$(printf '11%.0s' {1..32})" >"$1"
}

install_bootstrap() {
    local manual_digest="$1" managed_digest="$2" root_digest="$3" mode="$4"
    local bootstrap_args=(
        --package manual.tar.gz
        --sha256 "${manual_digest}"
        --generation generation.managed.tar.gz
        --generation-sha256 "${managed_digest}"
        --trusted-root trusted-root.json
        --trusted-root-sha256 "${root_digest}"
        --metadata-url "http://127.0.0.1:9/metadata/"
        --targets-url "http://127.0.0.1:9/targets/"
    )
    if [[ "${mode}" == "convert" ]]; then
        bootstrap_args+=(--convert-existing-zerodeck)
    fi
    bootstrap_args+=(--start)
    upload "${manual}" "${guest_dir}/manual.tar.gz"
    upload "${managed}" "${guest_dir}/generation.managed.tar.gz"
    upload "${work}/bootstrap.sh" "${guest_dir}/bootstrap.sh"
    upload "${work}/trusted-root.json" "${guest_dir}/trusted-root.json"
    guest_root_script <<'SCRIPT' "${guest_dir}" "${bootstrap_args[@]}"
set -euo pipefail
dir="$1"
shift
cd "${dir}"
bash ./bootstrap.sh "$@"
SCRIPT
}

resolve_install_mode() {
    case "${install_mode}" in
        clean|convert) return 0 ;;
        auto)
            if guest_root_script <<'SCRIPT'
set -e
for path in \
    /home/.zerodeck/current \
    /home/.zerodeck/state/update.json \
    /home/.zerodeck/bin/zerodeckctl \
    /etc/systemd/system/zerodeck.service; do
    if [[ -e "${path}" ]]; then
        exit 0
    fi
done
exit 1
SCRIPT
            then
                install_mode="convert"
            else
                install_mode="clean"
            fi
            log "auto-detected install mode: ${install_mode}"
            ;;
        *) die "--mode must be auto, clean or convert" ;;
    esac
}

stage_generation() {
    local archive="$1" digest="$2" label="$3"
    upload "${archive}" "${guest_dir}/candidate.tar.gz"
    guest_root_script <<'SCRIPT' "${guest_dir}/candidate.tar.gz" "${digest}" "${label}"
set -euo pipefail
archive="$1"
digest="$2"
label="$3"
actual="$("${PYTHON:-python3}" - "${archive}" <<'PY'
import hashlib
import sys

digest = hashlib.sha256()
with open(sys.argv[1], "rb") as handle:
    for chunk in iter(lambda: handle.read(1024 * 1024), b""):
        digest.update(chunk)
print(digest.hexdigest())
PY
)"
[[ "${actual}" == "${digest}" ]] || {
    echo "${label}: archive digest mismatch: expected ${digest}, got ${actual}" >&2
    exit 1
}
destination="/home/.zerodeck/generations/${digest}"
if [[ -e "${destination}" ]]; then
    [[ -f "${destination}/release.json" ]] || {
        echo "${label}: existing generation is incomplete" >&2
        exit 1
    }
    echo "${label}: generation ${digest} already staged"
    exit 0
fi
staging="$(mktemp -d "/home/.zerodeck/generations/.vm-stage.XXXXXX")"
trap 'rm -rf "${staging}"' EXIT
python3 - "${archive}" "${staging}/tree" <<'PY'
import os
import sys
import tarfile
from pathlib import PurePosixPath

archive_path, destination = sys.argv[1:3]
with tarfile.open(archive_path, "r:gz") as archive:
    seen = set()
    for member in archive.getmembers():
        if member.issym() or member.islnk():
            raise SystemExit(f"candidate contains a link: {member.name!r}")
        if not (member.isfile() or member.isdir()):
            raise SystemExit(f"candidate contains an unsupported entry: {member.name!r}")
        path = PurePosixPath(member.name)
        if path.is_absolute() or any(part == ".." for part in path.parts):
            raise SystemExit(f"candidate contains an unsafe path: {member.name!r}")
        name = "/".join(part for part in path.parts if part not in ("", "."))
        if not name or name in seen:
            raise SystemExit(f"candidate contains a duplicate path: {member.name!r}")
        seen.add(name)
    os.makedirs(destination, mode=0o755)
    for member in archive.getmembers():
        name = "/".join(
            part for part in PurePosixPath(member.name).parts if part not in ("", ".")
        )
        target = os.path.join(destination, *name.split("/"))
        if member.isdir():
            os.makedirs(target, mode=0o755, exist_ok=True)
            os.chmod(target, 0o755)
            continue
        os.makedirs(os.path.dirname(target), mode=0o755, exist_ok=True)
        source = archive.extractfile(member)
        if source is None:
            raise SystemExit(f"candidate cannot read {member.name!r}")
        with open(target, "wb") as output:
            for chunk in iter(lambda: source.read(1024 * 1024), b""):
                output.write(chunk)
        os.chmod(target, 0o755 if member.mode & 0o111 else 0o644)
PY
chown -R root:root "${staging}/tree"
find "${staging}/tree" -type d -exec chmod 0755 {} +
find "${staging}/tree" -type f -exec chmod 0644 {} +
chmod 0755 "${staging}/tree/bin/zerodeckd"
mv "${staging}/tree" "${destination}"
echo "${label}: staged generation ${digest}"
SCRIPT
}

write_catalog_authorization() {
    local digest="$1" version="$2" build_id="$3" size="$4" target="$5"
    guest_root_script <<'SCRIPT' "${digest}" "${version}" "${build_id}" "${size}" "${target}"
set -euo pipefail
digest="$1"
version="$2"
build_id="$3"
size="$4"
target="$5"
python3 - "/home/.zerodeck/state/catalog.json" "${digest}" "${version}" "${build_id}" "${size}" "${target}" <<'PY'
import json
import os
import sys
import time

path, digest, version, build_id, size, target = sys.argv[1:7]
now = int(time.time())
document = {
    "schema": 1,
    "last_verified_at": now - 60,
    "expires_at": now + 86_400,
    "authorization": {
        "channel": "stable",
        "version": version,
        "digest": digest,
        "size": int(size),
        "build_id": build_id,
        "min_updater": "0.1.0",
        "target": target,
    },
    # `upgrade` rechecks the selection that produced the authorization (channel, updater version).
    "selection": {"channel": "stable", "updater_version": "0.1.0"},
}
temporary = path + ".tmp"
with open(temporary, "w", encoding="utf-8") as handle:
    json.dump(document, handle, indent=2)
    handle.write("\n")
    handle.flush()
    os.fsync(handle.fileno())
os.chmod(temporary, 0o600)
os.replace(temporary, path)
directory = os.open(os.path.dirname(path), os.O_RDONLY)
try:
    os.fsync(directory)
finally:
    os.close(directory)
PY
chown root:root /home/.zerodeck/state/catalog.json
chmod 0600 /home/.zerodeck/state/catalog.json
SCRIPT
}

install_dropin() {
    local unit="$1" payload="$2"
    guest_root_script <<'SCRIPT' "${unit}" "${payload}"
set -euo pipefail
unit="$1"
payload="$2"
mkdir -p "/etc/systemd/system/${unit}.d"
printf '%s\n' "${payload}" >"/etc/systemd/system/${unit}.d/vm-gate.conf"
chmod 0644 "/etc/systemd/system/${unit}.d/vm-gate.conf"
systemctl daemon-reload
# The scenarios cut power right after this; without a sync the drop-in would not survive it.
sync
SCRIPT
}

remove_dropin() {
    local unit="$1"
    guest_root_script <<'SCRIPT' "${unit}"
set -euo pipefail
unit="$1"
rm -f "/etc/systemd/system/${unit}.d/vm-gate.conf"
rmdir "/etc/systemd/system/${unit}.d" 2>/dev/null || true
systemctl daemon-reload
SCRIPT
}

run_upgrade() {
    guest_root_cmd "${ZERODECK_CTL}" upgrade
}

start_upgrade_background() {
    guest_root_script <<'SCRIPT'
set -euo pipefail
nohup /home/.zerodeck/bin/zerodeckctl upgrade \
    >/var/tmp/zerodeck-upgrade.log 2>&1 &
echo $!
SCRIPT
}

wait_for_apply_phase() {
    local phase="$1" timeout="$2"
    guest_root_script <<'SCRIPT' "${phase}" "${timeout}"
set -euo pipefail
phase="$1"
deadline=$((SECONDS + $2))
while (( SECONDS < deadline )); do
    state="/home/.zerodeck/state/update.json"
    if [[ -f "${state}" ]] && grep -q "\"phase\": \"${phase}\"" "${state}"; then
        echo "apply reached phase ${phase}"
        exit 0
    fi
    sleep 0.1
done
echo "timed out waiting for apply phase ${phase}" >&2
exit 1
SCRIPT
}

wait_for_recovered() {
    local expected="$1" timeout="$2"
    guest_root_script <<'SCRIPT' "${expected}" "${timeout}"
set -euo pipefail
expected="$1"
deadline=$((SECONDS + $2))
while (( SECONDS < deadline )); do
    if [[ "$(systemctl show -p ActiveState --value zerodeck.service 2>/dev/null || true)" == "active" ]]; then
        pid="$(systemctl show -p MainPID --value zerodeck.service 2>/dev/null || true)"
        current="$(cat /home/.zerodeck/current 2>/dev/null || true)"
        if [[ "${pid}" =~ ^[0-9]+$ && "${pid}" -gt 0 && "${current}" == "${expected}" ]] && python3 - <<'PY' 2>/dev/null
import json
import sys
import urllib.request

try:
    document = json.load(open("/home/.zerodeck/state/update.json", encoding="utf-8"))
except (OSError, json.JSONDecodeError):
    raise SystemExit(1)
if document.get("pending") is not None:
    raise SystemExit(1)
with urllib.request.urlopen("http://127.0.0.1:1337/auth/token", timeout=5) as response:
    token = response.read().decode("utf-8").strip()
request = urllib.request.Request(
    "http://127.0.0.1:1337/readyz", headers={"X-Decky-Auth": token}
)
with urllib.request.urlopen(request, timeout=5) as response:
    ready = json.loads(response.read())
raise SystemExit(0 if ready.get("python_host_ready") is True else 1)
PY
        then
            echo "recovered to ${expected}"
            exit 0
        fi
    fi
    sleep 1
done
echo "timed out waiting for recovery to ${expected}" >&2
exit 1
SCRIPT
}

kill_apply_helper() {
    guest_root_script <<'SCRIPT'
set -euo pipefail
pid="$(systemctl show -p MainPID --value zerodeck-apply.service 2>/dev/null || true)"
[[ "${pid}" =~ ^[0-9]+$ && "${pid}" -gt 0 ]] || {
    echo "cannot SIGKILL apply helper: no MainPID" >&2
    exit 1
}
kill -9 "${pid}"
echo "SIGKILL sent to zerodeck-apply.service MainPID ${pid}"
SCRIPT
}

read_readyz() {
    guest_script <<'SCRIPT'
set -euo pipefail
python3 - <<'PY'
import json
import urllib.request

base = "http://127.0.0.1:1337"
with urllib.request.urlopen(f"{base}/auth/token", timeout=10) as response:
    token = response.read().decode("utf-8").strip()
request = urllib.request.Request(f"{base}/readyz", headers={"X-Decky-Auth": token})
with urllib.request.urlopen(request, timeout=10) as response:
    document = json.loads(response.read())
print(json.dumps({
    "version": document.get("version"),
    "build_id": document.get("build_id"),
    "python_host_ready": document.get("python_host_ready"),
    "invocation_id": document.get("invocation_id"),
}))
PY
SCRIPT
}

verify_guest() {
    local expected_digest="$1" expected_version="$2" expected_build="$3" label="$4"
    local expect_invocation="${5:-any}"
    local output ready invocation
    output="$(guest_root_script <<'SCRIPT' "${expected_digest}" "${expected_version}" "${expected_build}"
set -euo pipefail
expected="$1"
version="$2"
build_id="$3"
current="$(cat /home/.zerodeck/current)"
[[ "${current}" == "${expected}" ]] || {
    echo "current digest mismatch: expected ${expected}, got ${current}" >&2
    exit 1
}
systemctl is-active --quiet zerodeck.service || {
    echo "zerodeck.service is not active" >&2
    exit 1
}
pid="$(systemctl show -p MainPID --value zerodeck.service)"
[[ "${pid}" =~ ^[0-9]+$ && "${pid}" -gt 0 ]] || {
    echo "zerodeck.service has no MainPID" >&2
    exit 1
}
exe="$(readlink -f "/proc/${pid}/exe")"
[[ "${exe}" == "/home/.zerodeck/generations/${expected}/bin/zerodeckd" ]] || {
    echo "running binary is ${exe}, expected generation ${expected}" >&2
    exit 1
}
installed_sha="$(sha256sum "/home/.zerodeck/generations/${expected}/bin/zerodeckd" | awk '{print $1}')"
running_sha="$(sha256sum "/proc/${pid}/exe" | awk '{print $1}')"
[[ "${installed_sha}" == "${running_sha}" ]] || {
    echo "running binary digest does not match the installed generation" >&2
    exit 1
}
python3 - "${expected}" <<'PY'
import json
import sys

path = "/home/.zerodeck/state/update.json"
try:
    document = json.load(open(path, encoding="utf-8"))
except (OSError, json.JSONDecodeError) as error:
    raise SystemExit(f"state is unreadable: {error}")
if document.get("pending") is not None:
    raise SystemExit("a transaction is still pending")
if document.get("last_confirmed") != sys.argv[1]:
    raise SystemExit(
        f"last_confirmed is {document.get('last_confirmed')!r}, expected {sys.argv[1]!r}"
    )
PY
[[ "$(stat -c '%u' /home/.zerodeck/current)" == "0" ]] || {
    echo "current is not root-owned" >&2
    exit 1
}
invocation="$(systemctl show -p InvocationID --value zerodeck.service)"
[[ -n "${invocation}" ]] || {
    echo "zerodeck.service has an empty InvocationID" >&2
    exit 1
}
printf 'current=%s\nexe=%s\ninvocation=%s\n' "${current}" "${exe}" "${invocation}"
SCRIPT
)"
    invocation="$(printf '%s\n' "${output}" | sed -n 's/^invocation=//p')"
    [[ -n "${invocation}" ]] || die "${label}: missing InvocationID"
    case "${expect_invocation}" in
        yes)
            [[ -z "${last_invocation_id}" || "${last_invocation_id}" != "${invocation}" ]] || \
                die "${label}: service InvocationID did not change"
            ;;
        no)
            [[ -z "${last_invocation_id}" || "${last_invocation_id}" == "${invocation}" ]] || \
                die "${label}: service restarted when it should not have"
            ;;
        any) ;;
        *) die "invalid InvocationID expectation: ${expect_invocation}" ;;
    esac
    last_invocation_id="${invocation}"
    ready="$(read_readyz)"
    "${PYTHON}" - "${ready}" "${expected_version}" "${expected_build}" "${invocation}" <<'PY'
import json
import sys

ready = json.loads(sys.argv[1])
version, build_id, invocation = sys.argv[2:5]
if ready.get("version") != version or ready.get("build_id") != build_id:
    raise SystemExit(
        f"/readyz identity {ready!r} does not match generation {version}/{build_id}"
    )
if ready.get("python_host_ready") is not True:
    raise SystemExit(f"/readyz reports python_host_ready={ready.get('python_host_ready')!r}")
if ready.get("invocation_id") != invocation:
    raise SystemExit(
        f"/readyz invocation {ready.get('invocation_id')!r} != systemd InvocationID {invocation!r}"
    )
PY
    results+=("PASS ${label}: digest=${expected_digest} version=${expected_version} build=${expected_build}")
    log "PASS ${label}: ${output//$'\n'/; }"
}

scenario_bootstrap() {
    log "scenario: ${install_mode} bootstrap and running identity"
    install_bootstrap "${manual_digest}" "${managed_digest}" "${trusted_root_digest}" \
        "${install_mode}"
    verify_guest "${managed_digest}" "${managed_version}" "${managed_build}" "bootstrap"
    local status
    status="$(guest_root_cmd "${ZERODECK_CTL}" status)"
    printf '%s\n' "${status}" | grep -q "^active: ${managed_digest}$"
    if [[ "${install_mode}" == "convert" ]]; then
        printf '%s\n' "${status}" | grep -Eq '^rollback: ([0-9a-f]{64}|none)$'
    else
        printf '%s\n' "${status}" | grep -q "^rollback: none$"
    fi
}

scenario_port_collision() {
    log "scenario: port collision fails closed and a clean start recovers"
    install_dropin zerodeck.service $'[Service]\nRestart=no\nTimeoutStartSec=15'
    guest_root_script <<'SCRIPT'
set -euo pipefail
systemctl stop zerodeck.service
python3 -m http.server 1337 --bind 127.0.0.1 >/var/tmp/zerodeck-port-blocker.log 2>&1 &
echo $! >/var/tmp/zerodeck-port-blocker.pid
bound=0
for _ in {1..50}; do
    if python3 - <<'PY' 2>/dev/null
import socket
with socket.create_connection(("127.0.0.1", 1337), timeout=1):
    pass
PY
    then
        bound=1
        break
    fi
    sleep 0.1
done
if [[ "${bound}" -eq 0 ]]; then
    echo "port blocker never bound 127.0.0.1:1337" >&2
    kill "$(cat /var/tmp/zerodeck-port-blocker.pid)" 2>/dev/null || true
    exit 1
fi
if systemctl start zerodeck.service; then
    echo "zerodeck.service unexpectedly started while 1337 was occupied" >&2
    kill "$(cat /var/tmp/zerodeck-port-blocker.pid)" 2>/dev/null || true
    exit 1
fi
if systemctl is-active --quiet zerodeck.service; then
    echo "zerodeck.service is active despite the collision" >&2
    kill "$(cat /var/tmp/zerodeck-port-blocker.pid)" 2>/dev/null || true
    exit 1
fi
kill "$(cat /var/tmp/zerodeck-port-blocker.pid)" 2>/dev/null || true
wait "$(cat /var/tmp/zerodeck-port-blocker.pid)" 2>/dev/null || true
rm -f /var/tmp/zerodeck-port-blocker.pid
SCRIPT
    remove_dropin zerodeck.service
    guest_root_script <<'SCRIPT'
set -euo pipefail
systemctl start zerodeck.service
for _ in {1..30}; do
    if systemctl is-active --quiet zerodeck.service; then
        exit 0
    fi
    sleep 1
done
echo "zerodeck.service did not become active after the port was released" >&2
exit 1
SCRIPT
    verify_guest "${managed_digest}" "${managed_version}" "${managed_build}" \
        "port-collision-recovered" yes
}

scenario_successful_update() {
    log "scenario: successful update"
    stage_generation "${candidate_archive}" "${candidate_digest}" "candidate"
    write_catalog_authorization "${candidate_digest}" "${candidate_version}" "${candidate_build}" \
        "${candidate_size}" "linux-x86_64/zerodeck-${candidate_version}.tar.gz"
    local started=$SECONDS
    run_upgrade
    local elapsed=$((SECONDS - started))
    verify_guest "${candidate_digest}" "${candidate_version}" "${candidate_build}" "update" yes
    local status
    status="$(guest_root_cmd "${ZERODECK_CTL}" status)"
    printf '%s\n' "${status}" | grep -q "^rollback: ${managed_digest}$"
    results+=("PASS update stop-to-ready: ${elapsed}s")
}

restore_generation_from_archive() {
    local archive="$1" digest="$2"
    guest_root_script <<'SCRIPT' "${archive}" "${digest}"
set -euo pipefail
archive="$1"
digest="$2"
destination="/home/.zerodeck/generations/${digest}"
rm -rf "${destination}"
staging="$(mktemp -d "/home/.zerodeck/generations/.vm-stage.XXXXXX")"
trap 'rm -rf "${staging}"' EXIT
python3 - "${archive}" "${staging}/tree" <<'PY'
import os
import sys
import tarfile
from pathlib import PurePosixPath

archive_path, destination = sys.argv[1:3]
with tarfile.open(archive_path, "r:gz") as archive:
    for member in archive.getmembers():
        if member.issym() or member.islnk() or not (member.isfile() or member.isdir()):
            raise SystemExit(f"unsafe generation entry: {member.name!r}")
        path = PurePosixPath(member.name)
        if path.is_absolute() or any(part == ".." for part in path.parts):
            raise SystemExit(f"unsafe generation path: {member.name!r}")
    os.makedirs(destination, mode=0o755)
    for member in archive.getmembers():
        name = "/".join(
            part for part in PurePosixPath(member.name).parts if part not in ("", ".")
        )
        target = os.path.join(destination, *name.split("/"))
        if member.isdir():
            os.makedirs(target, mode=0o755, exist_ok=True)
            continue
        os.makedirs(os.path.dirname(target), mode=0o755, exist_ok=True)
        source = archive.extractfile(member)
        if source is None:
            raise SystemExit("generation entry is unreadable")
        with open(target, "wb") as output:
            output.write(source.read())
        os.chmod(target, 0o755 if member.mode & 0o111 else 0o644)
PY
chown -R root:root "${staging}/tree"
find "${staging}/tree" -type d -exec chmod 0755 {} +
find "${staging}/tree" -type f -exec chmod 0644 {} +
chmod 0755 "${staging}/tree/bin/zerodeckd"
mv "${staging}/tree" "${destination}"
SCRIPT
}

rollback_refusal_case() {
    local label="$1" mutation="$2"
    guest_root_script <<'SCRIPT' "${managed_digest}" "${label}" "${mutation}"
set -euo pipefail
generation="$1"
label="$2"
mutation="$3"
directory="/home/.zerodeck/generations/${generation}"
case "${mutation}" in
    missing) rm -rf "${directory}" ;;
    release) rm -f "${directory}/release.json" ;;
    *) echo "unknown rollback mutation: ${mutation}" >&2; exit 2 ;;
esac
log="/var/tmp/zerodeck-bad-rollback-${mutation}.log"
if /home/.zerodeck/bin/zerodeckctl rollback >"${log}" 2>&1; then
    echo "rollback unexpectedly succeeded with ${label}" >&2
    exit 1
fi
grep -qi "corrupt\\|missing\\|generation" "${log}" || {
    echo "rollback failure did not explain ${label}" >&2
    cat "${log}" >&2
    exit 1
}
SCRIPT
}

scenario_corrupt_rollback() {
    log "scenario: missing/corrupt rollback generation refuses before stopping"
    rollback_refusal_case "missing-generation" "missing"
    verify_guest "${candidate_digest}" "${candidate_version}" "${candidate_build}" \
        "missing-rollback-refused" no
    restore_generation_from_archive "${guest_dir}/generation.managed.tar.gz" "${managed_digest}"
    rollback_refusal_case "missing-release-json" "release"
    verify_guest "${candidate_digest}" "${candidate_version}" "${candidate_build}" \
        "corrupt-rollback-refused" no
    restore_generation_from_archive "${guest_dir}/generation.managed.tar.gz" "${managed_digest}"
    local started=$SECONDS
    guest_root_cmd "${ZERODECK_CTL}" rollback
    local elapsed=$((SECONDS - started))
    verify_guest "${managed_digest}" "${managed_version}" "${managed_build}" "offline-rollback" yes
    results+=("PASS rollback stop-to-ready: ${elapsed}s")
}

make_fixture_candidate() {
    local source="$1" destination="$2" mode="$3"
    "${PYTHON}" - "${source}" "${destination}" "${mode}" <<'PY'
import gzip
import io
import json
import os
import sys
import tarfile
import tempfile

source, destination, mode = sys.argv[1:4]
with tempfile.TemporaryDirectory() as temporary:
    with tarfile.open(source, "r:gz") as archive:
        archive.extractall(temporary)
    if mode == "unhealthy":
        manifest_path = os.path.join(temporary, "release.json")
        with open(manifest_path, encoding="utf-8") as handle:
            manifest = json.load(handle)
        parts = manifest["version"].split(".")
        if len(parts) < 3:
            raise SystemExit(f"cannot bump fixture version {manifest['version']!r}")
        parts[2] = str(int(parts[2]) + 1)
        manifest["version"] = ".".join(parts[:3])
        with open(manifest_path, "w", encoding="utf-8") as handle:
            json.dump(manifest, handle, separators=(",", ":"))
            handle.write("\n")
    entries = []
    for base, dirs, files in os.walk(temporary):
        for name in dirs + files:
            path = os.path.join(base, name)
            entries.append((os.path.relpath(path, temporary), path))
    # Different gzip metadata and member order make this a distinct archive while preserving the
    # exact managed layout and (for the healthy variant) release identity.
    entries.sort(reverse=True)
    raw = io.BytesIO()
    with gzip.GzipFile(fileobj=raw, mode="wb", mtime=1 if mode == "healthy" else 2) as compressed:
        with tarfile.open(fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT) as archive:
            for name, path in entries:
                info = archive.gettarinfo(path, arcname=name.replace(os.sep, "/"))
                info.uid = 0
                info.gid = 0
                info.uname = ""
                info.gname = ""
                info.mtime = 0
                if info.isfile():
                    with open(path, "rb") as handle:
                        archive.addfile(info, handle)
                else:
                    archive.addfile(info)
    with open(destination, "wb") as output:
        output.write(raw.getvalue())
PY
    chmod 0644 "${destination}"
}

scenario_unhealthy_candidate() {
    log "scenario: unhealthy candidate rolls back to the previous generation"
    local unhealthy="${work}/unhealthy.managed.tar.gz"
    make_fixture_candidate "${managed}" "${unhealthy}" "unhealthy"
    local unhealthy_digest unhealthy_identity unhealthy_version unhealthy_build
    unhealthy_digest="$(check_digest "${unhealthy}" "" "unhealthy candidate")"
    unhealthy_identity="$(archive_identity "${unhealthy}" managed)"
    unhealthy_version="$(printf '%s\n' "${unhealthy_identity}" | sed -n '1p')"
    unhealthy_build="$(printf '%s\n' "${unhealthy_identity}" | sed -n '2p')"
    validate_archive "${unhealthy}" managed "${unhealthy_version}" "${unhealthy_build}"
    stage_generation "${unhealthy}" "${unhealthy_digest}" "unhealthy-candidate"
    write_catalog_authorization "${unhealthy_digest}" "${unhealthy_version}" "${unhealthy_build}" \
        "$(archive_size "${unhealthy}")" "linux-x86_64/zerodeck-${unhealthy_version}.tar.gz"
    if run_upgrade; then
        die "unhealthy candidate unexpectedly activated"
    fi
    verify_guest "${managed_digest}" "${managed_version}" "${managed_build}" "unhealthy-rollback" yes
    guest_root_script <<'SCRIPT' "${unhealthy_digest}"
set -euo pipefail
digest="$1"
python3 - "/home/.zerodeck/state/update.json" "${digest}" <<'PY'
import json
import sys

document = json.load(open(sys.argv[1], encoding="utf-8"))
if sys.argv[2] not in document.get("quarantined", []):
    raise SystemExit("failed candidate was not quarantined")
PY
SCRIPT
}

prepare_pending_crash() {
    local archive="$1" digest="$2"
    stage_generation "${archive}" "${digest}" "crash-candidate"
    local version build size
    local identity
    identity="$(archive_identity "${archive}" managed)"
    version="$(printf '%s\n' "${identity}" | sed -n '1p')"
    build="$(printf '%s\n' "${identity}" | sed -n '2p')"
    size="$(archive_size "${archive}")"
    write_catalog_authorization "${digest}" "${version}" "${build}" "${size}" \
        "linux-x86_64/zerodeck-${version}.tar.gz"
    install_dropin zerodeck.service $'[Service]\nExecStartPre=/bin/sleep 30'
    start_upgrade_background >/dev/null
    wait_for_apply_phase "verifying" "${scenario_timeout}"
}

assert_quarantined() {
    local digest="$1" label="$2"
    guest_root_script <<'SCRIPT' "${digest}" "${label}"
set -euo pipefail
digest="$1"
label="$2"
python3 - "/home/.zerodeck/state/update.json" "${digest}" "${label}" <<'PY'
import json
import sys

path, digest, label = sys.argv[1:4]
document = json.load(open(path, encoding="utf-8"))
if digest not in document.get("quarantined", []):
    raise SystemExit(f"{label}: started candidate was not quarantined")
if not document.get("last_error"):
    raise SystemExit(f"{label}: crash reason was not recorded")
PY
SCRIPT
}

scenario_hard_poweroff() {
    log "scenario: hard poweroff during apply"
    # The updater refuses a new digest under an installed version, so every crash scenario
    # activates the genuinely newer candidate and clears its quarantine afterwards.
    local crash="${candidate_archive}" crash_digest="${candidate_digest}"
    prepare_pending_crash "${crash}" "${crash_digest}"
    hard_poweroff
    start_qemu
    wait_ssh
    wait_for_recovered "${managed_digest}" "${scenario_timeout}"
    remove_dropin zerodeck.service
    assert_quarantined "${crash_digest}" "hard-poweroff-recovered"
    guest_root_cmd "${ZERODECK_CTL}" unquarantine "${crash_digest}" >/dev/null
    verify_guest "${managed_digest}" "${managed_version}" "${managed_build}" "hard-poweroff-recovered" yes
}

scenario_sigkill_apply() {
    log "scenario: SIGKILL of the independent apply helper"
    local crash="${candidate_archive}" crash_digest="${candidate_digest}"
    prepare_pending_crash "${crash}" "${crash_digest}"
    kill_apply_helper
    wait_for_recovered "${managed_digest}" "${scenario_timeout}"
    remove_dropin zerodeck.service
    assert_quarantined "${crash_digest}" "sigkill-recovered"
    guest_root_cmd "${ZERODECK_CTL}" unquarantine "${crash_digest}" >/dev/null
    verify_guest "${managed_digest}" "${managed_version}" "${managed_build}" "sigkill-recovered" yes
}

scenario_reboot_during_recovery() {
    log "scenario: reboot during boot recovery"
    local crash="${candidate_archive}" crash_digest="${candidate_digest}"
    # systemd applies TimeoutStartSec to ExecStartPre as well as ExecStart. The packaged recovery
    # unit has TimeoutStartSec=60, so a 60 s pre-delay would time out and recover-boot would never
    # run. Override the timeout to 180 s and keep the observable window at 20 s.
    install_dropin zerodeck-recovery.service \
        $'[Service]\nTimeoutStartSec=180\nExecStartPre=/bin/sleep 20'
    prepare_pending_crash "${crash}" "${crash_digest}"
    hard_poweroff
    start_qemu
    wait_ssh
    local boot_before boot_after
    boot_before="$(guest_cmd cat /proc/sys/kernel/random/boot_id)"
    [[ -n "${boot_before}" ]] || die "guest did not report a boot ID"
    guest_root_script <<'SCRIPT'
set -euo pipefail
deadline=$((SECONDS + 120))
while (( SECONDS < deadline )); do
    state="$(systemctl show -p ActiveState --value zerodeck-recovery.service 2>/dev/null || true)"
    if [[ "${state}" == "activating" ]]; then
        echo "observed zerodeck-recovery.service activating"
        exit 0
    fi
    sleep 0.1
done
echo "did not observe boot recovery activating before it completed" >&2
exit 1
SCRIPT
    guest_root_script <<'SCRIPT'
set -euo pipefail
nohup systemctl reboot --force >/var/tmp/zerodeck-reboot.log 2>&1 &
exit 0
SCRIPT
    wait_ssh_down
    wait_ssh
    boot_after="$(guest_cmd cat /proc/sys/kernel/random/boot_id)"
    [[ -n "${boot_after}" && "${boot_after}" != "${boot_before}" ]] || \
        die "guest boot ID did not change after the reboot trigger"
    wait_for_recovered "${managed_digest}" "${scenario_timeout}"
    remove_dropin zerodeck-recovery.service
    remove_dropin zerodeck.service
    guest_root_cmd "${ZERODECK_CTL}" unquarantine "${crash_digest}" >/dev/null
    verify_guest "${managed_digest}" "${managed_version}" "${managed_build}" "reboot-recovery" yes
}

scenario_second_recovery() {
    log "scenario: second recovery pass is idempotent"
    local before after
    before="$(guest_root_script <<'SCRIPT'
set -euo pipefail
sha256sum /home/.zerodeck/current /home/.zerodeck/state/update.json
SCRIPT
)"
    guest_root_cmd "${ZERODECK_CTL}" recover-boot >/dev/null
    guest_root_cmd "${ZERODECK_CTL}" recover-after-apply >/dev/null
    guest_root_cmd "${ZERODECK_CTL}" recover-boot >/dev/null
    guest_root_cmd "${ZERODECK_CTL}" recover-after-apply >/dev/null
    after="$(guest_root_script <<'SCRIPT'
set -euo pipefail
sha256sum /home/.zerodeck/current /home/.zerodeck/state/update.json
SCRIPT
)"
    [[ "${before}" == "${after}" ]] || die "second recovery pass changed current/state"
    verify_guest "${managed_digest}" "${managed_version}" "${managed_build}" "second-recovery" no
}

run_scenario() {
    local name="$1" function="$2"
    local started=$SECONDS
    log "=== ${name} ==="
    "${function}"
    local elapsed=$((SECONDS - started))
    results+=("PASS ${name} (${elapsed}s)")
}

choose_ports() {
    if [[ "${ssh_port}" == "0" ]]; then
        ssh_port="$(free_port)"
    fi
}

prepare_release_inputs() {
    [[ -f "${manual}" ]] || die "manual archive not found: ${manual}"
    [[ -f "${managed}" ]] || die "managed archive not found: ${managed}"
    if [[ -z "${sha256sums}" ]]; then
        if [[ -f "$(dirname "${manual}")/SHA256SUMS.txt" ]]; then
            sha256sums="$(cd "$(dirname "${manual}")" && pwd)/SHA256SUMS.txt"
        elif [[ -f "$(dirname "${managed}")/SHA256SUMS.txt" ]]; then
            sha256sums="$(cd "$(dirname "${managed}")" && pwd)/SHA256SUMS.txt"
        fi
    fi
    if [[ -n "${sha256sums}" ]]; then
        [[ -f "${sha256sums}" ]] || die "checksum manifest not found: ${sha256sums}"
        log "checking release archives against ${sha256sums}"
        if [[ -z "${manual_sha256}" ]]; then
            manual_sha256="$(expected_from_sums "${sha256sums}" "$(basename "${manual}")")" || die "manual archive is not covered by ${sha256sums}"
        fi
        if [[ -z "${managed_sha256}" ]]; then
            managed_sha256="$(expected_from_sums "${sha256sums}" "$(basename "${managed}")")" || die "managed archive is not covered by ${sha256sums}"
        fi
        if [[ -n "${candidate}" && -z "${candidate_sha256}" ]]; then
            if candidate_sha256="$(expected_from_sums "${sha256sums}" "$(basename "${candidate}")" 2>/dev/null)"; then
                :
            else
                candidate_sha256=""
                log "warning: ${sha256sums} does not cover the candidate; computing its digest"
            fi
        fi
    else
        log "warning: no SHA256SUMS.txt found; release digests are computed locally"
    fi
    manual_digest="$(check_digest "${manual}" "${manual_sha256}" "manual package")"
    managed_digest="$(check_digest "${managed}" "${managed_sha256}" "managed generation")"
    local manual_identity managed_identity
    manual_identity="$(archive_identity "${manual}" manual)"
    managed_identity="$(archive_identity "${managed}" managed)"
    manual_version="$(printf '%s\n' "${manual_identity}" | sed -n '1p')"
    manual_build="$(printf '%s\n' "${manual_identity}" | sed -n '2p')"
    managed_version="$(printf '%s\n' "${managed_identity}" | sed -n '1p')"
    managed_build="$(printf '%s\n' "${managed_identity}" | sed -n '2p')"
    [[ "${manual_version}" == "${managed_version}" && "${manual_build}" == "${managed_build}" ]] || \
        die "manual and managed archives have different release identities"
    validate_archive "${manual}" manual "${manual_version}" "${manual_build}"
    validate_archive "${managed}" managed "${managed_version}" "${managed_build}"

    if [[ -n "${candidate}" ]]; then
        [[ -f "${candidate}" ]] || die "candidate archive not found: ${candidate}"
        candidate_digest="$(check_digest "${candidate}" "${candidate_sha256}" "candidate generation")"
        candidate_archive="${candidate}"
        local candidate_identity
        candidate_identity="$(archive_identity "${candidate}" managed)"
        candidate_version="$(printf '%s\n' "${candidate_identity}" | sed -n '1p')"
        candidate_build="$(printf '%s\n' "${candidate_identity}" | sed -n '2p')"
        validate_archive "${candidate}" managed "${candidate_version}" "${candidate_build}"
        # Avoid a non-stdlib semver dependency: compare numeric components for the supported release
        # shape and require a strictly newer candidate.
        "${PYTHON}" - "${managed_version}" "${candidate_version}" <<'PY'
import re
import sys

def parts(value):
    match = re.match(r"^(\d+)\.(\d+)\.(\d+)", value)
    if not match:
        raise SystemExit(f"unsupported fixture version: {value}")
    return tuple(int(part) for part in match.groups())

if parts(sys.argv[2]) <= parts(sys.argv[1]):
    raise SystemExit(
        f"candidate version {sys.argv[2]} is not newer than installed {sys.argv[1]}"
    )
PY
        candidate_size="$(archive_size "${candidate}")"
    else
        log "no --candidate supplied; repacking the installed generation for apply/rollback tests"
        candidate_archive="${work}/fixture-candidate.managed.tar.gz"
        make_fixture_candidate "${managed}" "${candidate_archive}" "healthy"
        candidate_digest="$(check_digest "${candidate_archive}" "" "fixture candidate")"
        local candidate_identity
        candidate_identity="$(archive_identity "${candidate_archive}" managed)"
        candidate_version="$(printf '%s\n' "${candidate_identity}" | sed -n '1p')"
        candidate_build="$(printf '%s\n' "${candidate_identity}" | sed -n '2p')"
        validate_archive "${candidate_archive}" managed "${candidate_version}" "${candidate_build}"
        candidate_size="$(archive_size "${candidate_archive}")"
    fi
    [[ "${candidate_digest}" != "${managed_digest}" ]] || \
        die "candidate generation digest equals the installed generation"
    if [[ -n "${candidate_sha256}" && -z "${candidate}" ]]; then
        die "--candidate-sha256 requires --candidate"
    fi
}

main() {
    parse_args "$@"
    if [[ "${self_test}" == "1" ]]; then
        run_self_test
        exit 0
    fi
    [[ -n "${image}" ]] || usage
    [[ -n "${ssh_key}" ]] || usage
    [[ -n "${manual}" ]] || usage
    [[ -n "${managed}" ]] || usage
    case "${install_mode}" in
        auto|clean|convert) ;;
        *) die "--mode must be auto, clean or convert" ;;
    esac

    for tool in "${qemu_bin}" "${qemu_img_bin}" "${ssh_bin}" "${scp_bin}" "${PYTHON}"; do
        command -v "${tool}" >/dev/null 2>&1 || skip "no qemu/image (${tool} not found)"
    done
    [[ -f "${image}" ]] || skip "no qemu/image (image not found: ${image})"
    [[ -f "${ssh_key}" ]] || die "SSH key not found: ${ssh_key}"

    if [[ -n "${image_sha256}" ]]; then
        image_sha256="$(lower_hex "${image_sha256}")"
        [[ "${image_sha256}" =~ ^[0-9a-f]{64}$ ]] || die "--image-sha256 must be 64 hex characters"
    fi
    image_source_digest="$(check_digest "${image}" "${image_sha256}" "image")"
    log "image SHA-256: ${image_source_digest}"

    prepare_work
    trap cleanup EXIT INT TERM
    prepare_release_inputs
    choose_ports
    prepare_ssh
    create_overlay
    start_qemu
    wait_ssh
    guest_preflight
    resolve_install_mode
    guest_dir="$(guest_cmd mktemp -d "${DEFAULT_GUEST_DIR_PREFIX}.XXXXXX")"
    [[ -n "${guest_dir}" ]] || die "could not create a guest work directory"
    log "guest work directory: ${guest_dir}"
    extract_bootstrap "${manual}" "${work}/bootstrap.sh"
    write_trusted_root "${work}/trusted-root.json"
    trusted_root_digest="$(check_digest "${work}/trusted-root.json" "" "trusted root")"

    run_scenario "bootstrap" scenario_bootstrap
    run_scenario "port-collision" scenario_port_collision
    run_scenario "successful-update" scenario_successful_update
    run_scenario "corrupt-rollback" scenario_corrupt_rollback
    run_scenario "unhealthy-candidate" scenario_unhealthy_candidate
    run_scenario "hard-poweroff-during-apply" scenario_hard_poweroff
    run_scenario "sigkill-apply-helper" scenario_sigkill_apply
    run_scenario "reboot-during-recovery" scenario_reboot_during_recovery
    run_scenario "second-recovery" scenario_second_recovery

    stop_qemu
    local source_after
    source_after="$(sha256_file "${image}")"
    [[ "${source_after}" == "${image_source_digest}" ]] || die "source image changed during the run"
    printf '\nVM gate summary:\n'
    printf '  %s\n' "${results[@]}"
    printf '  source image unchanged: %s\n' "${image_source_digest}"
    if [[ "${keep_work}" != "1" && "${work_owned}" == "1" ]]; then
        log "removing work directory ${work}"
    fi
}

prepare_ssh() {
    ssh_opts=(
        -i "${ssh_key}"
        -p "${ssh_port}"
        -o BatchMode=yes
        -o IdentitiesOnly=yes
        -o StrictHostKeyChecking=no
        -o UserKnownHostsFile=/dev/null
        -o LogLevel=ERROR
        -o ConnectTimeout=10
        -o ServerAliveInterval=5
        -o ServerAliveCountMax=3
    )
    scp_opts=(
        -i "${ssh_key}"
        -P "${ssh_port}"
        -o BatchMode=yes
        -o IdentitiesOnly=yes
        -o StrictHostKeyChecking=no
        -o UserKnownHostsFile=/dev/null
        -o LogLevel=ERROR
        -o ConnectTimeout=10
    )
}

# ── Self-test: fake qemu/ssh proves command construction, refusal and cleanup ─────────────────────

selftest_tmp=""

run_self_test() {
    local tmp
    selftest_tmp="$(mktemp -d "${TMPDIR:-/tmp}/zerodeck-vm-selftest.XXXXXX")"
    tmp="${selftest_tmp}"
    trap 'if [[ "${ZERODECK_SELFTEST_KEEP:-0}" != "1" ]]; then rm -rf "${selftest_tmp}"; else echo "self-test dir kept: ${selftest_tmp}" >&2; fi' EXIT
    log "self-test: creating fake image and release fixtures"
    mkdir -p "${tmp}/bin" "${tmp}/assets" "${tmp}/tmp"
    : >"${tmp}/image.raw"
    printf 'fake-image\n' >"${tmp}/image.raw"
    printf 'fake-key\n' >"${tmp}/key"
    chmod 0600 "${tmp}/key"

    "${PYTHON}" - "${tmp}/assets" <<'PY'
import hashlib
import io
import json
import os
import tarfile
import sys

root = sys.argv[1]
for mode, name in (("managed", "fixture.managed.tar.gz"), ("manual", "fixture.tar.gz")):
    path = os.path.join(root, name)
    with tarfile.open(path, "w:gz") as archive:
        def add_dir(name):
            info = tarfile.TarInfo(name)
            info.type = tarfile.DIRTYPE
            info.mode = 0o755
            archive.addfile(info)

        def add(name, data, mode=0o644):
            info = tarfile.TarInfo(name)
            info.size = len(data)
            info.mode = mode
            archive.addfile(info, io.BytesIO(data))

        prefix = "zerodeck-fixture-linux-x86_64/" if mode == "manual" else ""
        if mode == "manual":
            for directory in (prefix.rstrip("/"), prefix + "bin", prefix + "python",
                              prefix + "frontend", prefix + "systemd", prefix + "docs"):
                add_dir(directory)
        else:
            for directory in ("bin", "python", "frontend"):
                add_dir(directory)
        add(prefix + "release.json", json.dumps({"schema": 1, "version": "0.1.0", "build_id": "fixture-build"}).encode())
        add(prefix + "bin/zerodeckd", b"#!/bin/sh\nexit 0\n", 0o755)
        add(prefix + "python/host.py", b"# fixture\n")
        add(prefix + "frontend/index.js", b"export default {};\n")
        if mode == "manual":
            add(prefix + "bin/zerodeckctl", b"#!/bin/sh\nexit 0\n", 0o755)
            add(prefix + "bootstrap.sh", b"#!/bin/sh\nexit 0\n", 0o755)
            add(prefix + "trusted-root.json", b'{"keys": []}\n')
            add(prefix + "systemd/zerodeck.service", b"# fixture\n")
            add(prefix + "systemd/zerodeck-apply.service", b"# fixture\n")
            add(prefix + "systemd/zerodeck-recovery.service", b"# fixture\n")
            add(prefix + "systemd/zerodeck-check.service", b"# fixture\n")
            add(prefix + "systemd/zerodeck-check.timer", b"# fixture\n")
            add(prefix + "docs/RELEASE.md", b"# fixture\n")
            add(prefix + "docs/COMPATIBILITY.md", b"# fixture\n")
            add(prefix + "LICENSE", b"fixture\n")
            add(prefix + "README.md", b"# fixture\n")
    digest = hashlib.sha256(open(path, "rb").read()).hexdigest()
    with open(os.path.join(root, name + ".sha256"), "w", encoding="utf-8") as handle:
        handle.write(digest + "\n")
PY
    local manual_sha managed_sha
    manual_sha="$(sha256_file "${tmp}/assets/fixture.tar.gz")"
    managed_sha="$(sha256_file "${tmp}/assets/fixture.managed.tar.gz")"
    printf '%s  fixture.tar.gz\n%s  fixture.managed.tar.gz\n' "${manual_sha}" "${managed_sha}" \
        >"${tmp}/assets/SHA256SUMS.txt"

    cat >"${tmp}/bin/qemu-img" <<'SCRIPT'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"${VM_SELFTEST_LOG}.qemu-img"
if [[ "${1:-}" == "info" ]]; then
    printf '{"format":"raw"}\n'
    exit 0
fi
if [[ "${1:-}" == "create" ]]; then
    for last in "$@"; do :; done
    touch "${last}"
    exit 0
fi
exit 1
SCRIPT
    cat >"${tmp}/bin/qemu-system-x86_64" <<'SCRIPT'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"${VM_SELFTEST_LOG}.qemu"
trap 'exit 0' TERM INT
while :; do sleep 0.2; done
SCRIPT
    cat >"${tmp}/bin/ssh" <<'SCRIPT'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"${VM_SELFTEST_LOG}.ssh"
# Consume a scripted stdin without executing it, but make the running-identity assertion fail.
script="$(cat)"
case "$*" in
    *" true") exit 0 ;;
    *"mktemp -d /var/tmp/zerodeck-vm."*) printf '/var/tmp/fake-guest-dir\n'; exit 0 ;;
    *"bash -s --"*)
        if [[ "$*" == *"--package manual.tar.gz"* ]]; then
            generation="$(sed -n 's/.*--generation \([^ ]*\).*/\1/p' <<<"$*" | head -n 1)"
            if [[ "${generation}" != *.managed.tar.gz ]]; then
                printf 'fake bootstrap rejected generation=%s\n' "${generation}" >&2
                exit 1
            fi
            actual_mode="clean"
            if [[ "$*" == *"--convert-existing-zerodeck"* ]]; then
                actual_mode="convert"
            fi
            if [[ -n "${VM_SELFTEST_EXPECT_MODE:-}" && "${actual_mode}" != "${VM_SELFTEST_EXPECT_MODE}" ]]; then
                printf 'fake bootstrap mode=%s expected=%s\n' \
                    "${actual_mode}" "${VM_SELFTEST_EXPECT_MODE}" >&2
                exit 1
            fi
            printf '%s\n' "$*" >>"${VM_SELFTEST_LOG}.argv"
            printf '%s\n' "${generation}" >>"${VM_SELFTEST_LOG}.generation"
            if [[ "${actual_mode}" == "convert" ]]; then
                printf 'convert-ok\n' >>"${VM_SELFTEST_LOG}.convert"
            fi
        fi
        if grep -q "cat /home/.zerodeck/current" <<<"${script}"; then
            printf 'fake current digest mismatch\n' >&2
            exit 1
        fi
        exit 0
        ;;
    *) exit 85 ;;
esac
SCRIPT
    cat >"${tmp}/bin/scp" <<'SCRIPT'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"${VM_SELFTEST_LOG}.scp"
exit 0
SCRIPT
    chmod 0755 "${tmp}/bin/"*
    export VM_SELFTEST_LOG="${tmp}/selftest"

    expect_status() {
        local expected="$1" label="$2"
        shift 2
        local log="${tmp}/${label}.log"
        set +e
        "$@" >"${log}" 2>&1
        local status=$?
        set -e
        if [[ "${status}" != "${expected}" ]]; then
            echo "self-test ${label}: expected exit ${expected}, got ${status}" >&2
            cat "${log}" >&2
            exit 1
        fi
    }

    local common=(
        --image "${tmp}/image.raw"
        --ssh-key "${tmp}/key"
        --manual "${tmp}/assets/fixture.tar.gz"
        --managed "${tmp}/assets/fixture.managed.tar.gz"
        --sha256sums "${tmp}/assets/SHA256SUMS.txt"
        --qemu "${tmp}/bin/qemu-system-x86_64"
        --qemu-img "${tmp}/bin/qemu-img"
        --ssh "${tmp}/bin/ssh"
        --scp "${tmp}/bin/scp"
        --mode clean
        --boot-timeout 5
        --scenario-timeout 5
    )

    if sed '/^# ── Self-test/,$d' "${SCRIPT_DIR}/vm.sh" \
        | grep -nE '(systemctl|\$\{systemctl\}|\$SYSTEMCTL).*--wait' \
        || grep -nE '(systemctl|\$\{systemctl\}|\$SYSTEMCTL).*--wait' \
            "${REPO_ROOT}/deploy/update/bootstrap.sh"; then
        echo "self-test: blocking --wait on a systemctl call is not allowed" >&2
        exit 1
    fi
    for sample in \
        'systemctl start --wait zerodeck.service' \
        '"${systemctl}" start --wait zerodeck.service' \
        'systemctl --wait start zerodeck.service'; do
        if ! grep -qE '(systemctl|\$\{systemctl\}|\$SYSTEMCTL).*--wait' <<<"${sample}"; then
            echo "self-test: --wait guard missed: ${sample}" >&2
            exit 1
        fi
    done

    local image_before
    image_before="$(sha256_file "${tmp}/image.raw")"

    # Missing QEMU is a skip, never a pass.
    expect_status 77 missing-qemu \
        bash "${SCRIPT_DIR}/vm.sh" "${common[@]}" --qemu "${tmp}/missing-qemu"
    grep -q "skipped: no qemu/image" "${tmp}/missing-qemu.log"

    # A wrong image digest must fail before QEMU is started.
    expect_status 1 image-digest \
        bash "${SCRIPT_DIR}/vm.sh" "${common[@]}" --image-sha256 "$(printf '0%.0s' {1..64})"
    if [[ -s "${VM_SELFTEST_LOG}.qemu" ]]; then
        echo "self-test: QEMU ran despite an image digest mismatch" >&2
        exit 1
    fi

    # A fake guest returns a mismatching current digest; the harness must fail, kill QEMU, and remove
    # its temporary work directory while leaving the source image byte-identical.
    expect_status 1 fake-guest-failure \
        env TMPDIR="${tmp}/tmp" VM_SELFTEST_EXPECT_MODE=clean \
        bash "${SCRIPT_DIR}/vm.sh" "${common[@]}"
    grep -q "fake current digest mismatch" "${tmp}/fake-guest-failure.log" || {
        echo "self-test: fake guest failed before the running-identity check" >&2
        cat "${tmp}/fake-guest-failure.log" >&2
        exit 1
    }
    local managed_scp_line managed_scp_destination managed_scp_path generation
    managed_scp_line="$(grep '\.managed\.tar\.gz' "${VM_SELFTEST_LOG}.scp" | tail -n 1 || true)"
    [[ -n "${managed_scp_line}" ]] || {
        echo "self-test: managed archive was not uploaded with a .managed.tar.gz name" >&2
        exit 1
    }
    managed_scp_destination="${managed_scp_line##* }"
    managed_scp_path="${managed_scp_destination#*:}"
    generation="$(tail -n 1 "${VM_SELFTEST_LOG}.generation")"
    [[ "$(basename "${managed_scp_path}")" == "${generation}" ]] || {
        echo "self-test: scp destination basename does not match --generation" >&2
        exit 1
    }
    grep -q -- "-drive file=.*overlay.qcow2" "${VM_SELFTEST_LOG}.qemu" || {
        echo "self-test: QEMU did not receive the overlay drive" >&2
        exit 1
    }
    grep -q -- "-netdev user,id=net0,hostfwd=tcp:127.0.0.1:" "${VM_SELFTEST_LOG}.qemu" || {
        echo "self-test: QEMU did not receive the SSH/API forwards" >&2
        exit 1
    }
    command -v pgrep >/dev/null 2>&1 || {
        echo "self-test: pgrep is required to verify the fake QEMU is gone" >&2
        exit 1
    }
    if pgrep -f "${tmp}/bin/qemu-system-x86_64" >/dev/null 2>&1; then
        echo "self-test: fake QEMU survived the harness trap" >&2
        pkill -f "${tmp}/bin/qemu-system-x86_64" || true
        exit 1
    fi
    if compgen -G "${tmp}/tmp/zerodeck-vm.*" >/dev/null; then
        echo "self-test: harness left a temporary work directory behind" >&2
        exit 1
    fi
    [[ "$(sha256_file "${tmp}/image.raw")" == "${image_before}" ]] || {
        echo "self-test: source image changed" >&2
        exit 1
    }

    rm -f "${VM_SELFTEST_LOG}.convert"
    expect_status 1 convert-argument \
        env TMPDIR="${tmp}/tmp" VM_SELFTEST_EXPECT_MODE=convert \
        bash "${SCRIPT_DIR}/vm.sh" "${common[@]}" --mode convert
    grep -qx "convert-ok" "${VM_SELFTEST_LOG}.convert" || {
        echo "self-test: convert mode did not pass --convert-existing-zerodeck" >&2
        exit 1
    }
    tail -n 1 "${VM_SELFTEST_LOG}.argv" | grep -q -- "--convert-existing-zerodeck" || {
        echo "self-test: convert argv was not propagated to the fake bootstrap" >&2
        exit 1
    }
    if compgen -G "${tmp}/tmp/zerodeck-vm.*" >/dev/null; then
        echo "self-test: convert run left a temporary work directory behind" >&2
        exit 1
    fi
    printf 'self-test passed: qemu command, overlay, checksum refusal, convert mode and trap cleanup\n'
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
    main "$@"
fi
