#!/usr/bin/env bash
# Promote already-published release bytes into a staged TUF repository tree.
#
# The release build job never has a signing key. This script runs after the immutable release is
# public and only accepts an archive whose SHA-256/length match the published asset, an attestation
# verification record produced from that archive, and a complete set of signed TUF metadata whose
# targets role is authorized by the out-of-band trusted root. It never creates or edits signatures.
set -euo pipefail

archive=""
expected_sha=""
tag=""
version=""
build_id=""
channel="stable"
min_updater=""
metadata_dir=""
trusted_root=""
trusted_root_sha=""
attestation=""
output=""
verify_only=0
target=""

usage() {
    cat >&2 <<'EOF'
usage: promote.sh --archive <published-managed.tar.gz> --sha256 <hex> --tag <tag>
                  --version <semver> --build-id <id> --min-updater <semver>
                  [--channel stable|testing]
                  --trusted-root <root.json> --trusted-root-sha256 <hex>
                  --attestation <verified-attestation.json>
                  [--metadata-dir <signed-metadata>] [--output <repo-dir>]
                  [--verify-only]

`--verify-only` checks the published bytes and attestation and prints the target descriptor without
touching a repository. The full mode additionally requires signed root/targets/snapshot/timestamp
metadata and stages the exact bytes plus metadata under --output. Signing happens outside this
repository; no private key or unsigned metadata is accepted here.
EOF
    exit 2
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --archive) archive="${2:-}"; shift 2 ;;
        --sha256) expected_sha="${2:-}"; shift 2 ;;
        --tag) tag="${2:-}"; shift 2 ;;
        --version) version="${2:-}"; shift 2 ;;
        --build-id) build_id="${2:-}"; shift 2 ;;
        --channel) channel="${2:-}"; shift 2 ;;
        --min-updater) min_updater="${2:-}"; shift 2 ;;
        --metadata-dir) metadata_dir="${2:-}"; shift 2 ;;
        --trusted-root) trusted_root="${2:-}"; shift 2 ;;
        --trusted-root-sha256) trusted_root_sha="${2:-}"; shift 2 ;;
        --attestation) attestation="${2:-}"; shift 2 ;;
        --output) output="${2:-}"; shift 2 ;;
        --target) target="${2:-}"; shift 2 ;;
        --verify-only) verify_only=1; shift ;;
        --help|-h) usage ;;
        *) usage ;;
    esac
done

[[ -n "${archive}" && -n "${expected_sha}" && -n "${tag}" && -n "${version}" ]] || usage
[[ -n "${build_id}" && -n "${min_updater}" && -n "${attestation}" ]] || usage
[[ "${channel}" == "stable" || "${channel}" == "testing" ]] || usage
[[ "${expected_sha}" =~ ^[0-9A-Fa-f]{64}$ ]] || { echo "promote: --sha256 must be a SHA-256 digest" >&2; exit 1; }
[[ "${version}" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$ ]] || {
    echo "promote: --version must be SemVer" >&2
    exit 1
}
[[ "${min_updater}" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$ ]] || {
    echo "promote: --min-updater must be SemVer" >&2
    exit 1
}
[[ -f "${archive}" ]] || { echo "promote: archive not found: ${archive}" >&2; exit 1; }
[[ -f "${attestation}" ]] || { echo "promote: attestation verification record not found: ${attestation}" >&2; exit 1; }
if [[ "${verify_only}" -eq 0 ]]; then
    [[ -n "${trusted_root}" && -n "${trusted_root_sha}" && -n "${metadata_dir}" && -n "${output}" ]] || usage
    [[ "${trusted_root_sha}" =~ ^[0-9A-Fa-f]{64}$ ]] || {
        echo "promote: --trusted-root-sha256 must be a SHA-256 digest" >&2
        exit 1
    }
    [[ -f "${trusted_root}" ]] || { echo "promote: trusted root not found: ${trusted_root}" >&2; exit 1; }
fi
expected_sha="$(printf '%s' "${expected_sha}" | tr '[:upper:]' '[:lower:]')"
if [[ -n "${trusted_root_sha}" ]]; then
    trusted_root_sha="$(printf '%s' "${trusted_root_sha}" | tr '[:upper:]' '[:lower:]')"
fi
expected_target="linux-x86_64/zerodeck-${version}.tar.gz"
if [[ -n "${target}" && "${target}" != "${expected_target}" ]]; then
    echo "promote: --target must be ${expected_target}" >&2
    exit 1
fi
target="${expected_target}"

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

actual_sha="$(sha256_file "${archive}")"
size="$(wc -c <"${archive}" | tr -d ' ')"
if [[ "${actual_sha}" != "${expected_sha}" ]]; then
    echo "promote: archive SHA-256 ${actual_sha} does not match the published digest ${expected_sha}" >&2
    exit 1
fi
if [[ "${verify_only}" -eq 0 && "$(sha256_file "${trusted_root}")" != "${trusted_root_sha}" ]]; then
    echo "promote: trusted root does not match the caller-supplied SHA-256" >&2
    exit 1
fi
if ! grep -Fq "${expected_sha}" "${attestation}"; then
    echo "promote: attestation verification record does not cover ${expected_sha}" >&2
    exit 1
fi
if ! grep -Eq '"verified"[[:space:]]*:[[:space:]]*true' "${attestation}"; then
    echo "promote: attestation verification record does not contain a successful verification" >&2
    exit 1
fi

python3 - "${archive}" "${version}" "${build_id}" "${channel}" "${min_updater}" "${tag}" <<'PY'
import json, os, re, sys, tarfile

archive, version, build_id, channel, min_updater, tag = sys.argv[1:7]
SEMVER = re.compile(r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$")
if not SEMVER.match(version):
    raise SystemExit(f"promote: invalid version: {version!r}")
if not SEMVER.match(min_updater):
    raise SystemExit(f"promote: invalid min_updater: {min_updater!r}")
if channel == "stable" and "-" in version:
    raise SystemExit("promote: a prerelease version cannot be promoted to the stable channel")
with tarfile.open(archive, "r:gz") as handle:
    try:
        member = handle.getmember("release.json")
    except KeyError:
        raise SystemExit("promote: archive has no release.json at its root")
    document = json.load(handle.extractfile(member))
if document.get("schema") != 1:
    raise SystemExit("promote: release.json must use schema 1")
if document.get("version") != version:
    raise SystemExit(f"promote: release.json version {document.get('version')!r} != {version!r}")
if document.get("build_id") != build_id:
    raise SystemExit(f"promote: release.json build_id {document.get('build_id')!r} != {build_id!r}")
print(f"promote: archive identity verified for {tag} ({version}, {build_id})")
PY

if [[ "${verify_only}" -eq 1 ]]; then
    cat <<EOF
target: ${target}
sha256: ${expected_sha}
length: ${size}
channel: ${channel}
version: ${version}
build_id: ${build_id}
min_updater: ${min_updater}
EOF
    exit 0
fi

[[ -n "${metadata_dir}" && -n "${output}" ]] || usage
[[ -d "${metadata_dir}" ]] || { echo "promote: signed metadata directory not found: ${metadata_dir}" >&2; exit 1; }
for role in root targets snapshot timestamp; do
    [[ -f "${metadata_dir}/${role}.json" ]] || {
        echo "promote: signed ${role}.json is required; refusing to promote without it" >&2
        exit 1
    }
done
if ! cmp -s "${trusted_root}" "${metadata_dir}/root.json"; then
    echo "promote: metadata root.json differs from the out-of-band trusted root" >&2
    exit 1
fi

python3 - "${metadata_dir}" "${target}" "${expected_sha}" "${size}" "${channel}" \
    "${version}" "${build_id}" "${min_updater}" <<'PY'
import json
import sys
import time

metadata_dir, target, sha, size, channel, version, build_id, min_updater = sys.argv[1:9]
now = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())

def load(role):
    path = f"{metadata_dir}/{role}.json"
    try:
        document = json.load(open(path, encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise SystemExit(f"promote: {role}.json is invalid: {error}")
    signed = document.get("signed")
    if not isinstance(signed, dict) or signed.get("_type") != role:
        raise SystemExit(f"promote: {role}.json is not a {role} role")
    if not document.get("signatures"):
        raise SystemExit(f"promote: {role}.json is unsigned")
    if not isinstance(signed.get("version"), int) or signed["version"] < 1:
        raise SystemExit(f"promote: {role}.json has an invalid version")
    if signed.get("expires") and signed["expires"] <= now:
        raise SystemExit(f"promote: {role}.json is expired")
    return document, signed

root, root_signed = load("root")
targets, targets_signed = load("targets")
snapshot, snapshot_signed = load("snapshot")
timestamp, timestamp_signed = load("timestamp")

roles = root_signed.get("roles")
keys = root_signed.get("keys")
if not isinstance(roles, dict) or not isinstance(keys, dict):
    raise SystemExit("promote: trusted root has no roles/keys")

def authorize(role_name, document):
    role = roles.get(role_name)
    if not isinstance(role, dict):
        raise SystemExit(f"promote: trusted root has no {role_name} role")
    keyids = set(role.get("keyids", []))
    threshold = role.get("threshold")
    if not isinstance(keyids, set) or not keyids or not isinstance(threshold, int) or threshold < 1:
        raise SystemExit(f"promote: trusted root {role_name} role is incomplete")
    if any(keyid not in keys for keyid in keyids):
        raise SystemExit(f"promote: trusted root {role_name} role names an unknown key")
    verified = 0
    for signature in document.get("signatures", []):
        keyid = signature.get("keyid")
        if keyid in keyids and signature.get("sig"):
            verified += 1
    if verified < threshold:
        raise SystemExit(
            f"promote: {role_name} metadata has {verified} authorized signatures, needs {threshold}"
        )

authorize("root", root)
authorize("targets", targets)
authorize("snapshot", snapshot)
authorize("timestamp", timestamp)

entries = targets_signed.get("targets")
entry = entries.get(target) if isinstance(entries, dict) else None
if not isinstance(entry, dict):
    raise SystemExit(f"promote: signed targets do not include {target}")
hashes = entry.get("hashes", {})
if hashes.get("sha256") != sha:
    raise SystemExit("promote: signed target SHA-256 does not match the published bytes")
if entry.get("length") != int(size):
    raise SystemExit("promote: signed target length does not match the published bytes")
custom = entry.get("custom", {})
expected_custom = {
    "channel": channel,
    "version": version,
    "build_id": build_id,
    "min_updater": min_updater,
}
for key, value in expected_custom.items():
    if custom.get(key) != value:
        raise SystemExit(f"promote: signed target custom.{key} is {custom.get(key)!r}, expected {value!r}")
print("promote: signed metadata authorizes the exact published archive")
PY

target_path="${output}/targets/${target}"
metadata_path="${output}/metadata"
if [[ -e "${target_path}" ]]; then
    existing="$(sha256_file "${target_path}")"
    if [[ "${existing}" != "${expected_sha}" ]]; then
        echo "promote: refusing to overwrite a different already-staged target at ${target_path}" >&2
        exit 1
    fi
fi
mkdir -p "$(dirname "${target_path}")" "${metadata_path}"
if [[ ! -e "${target_path}" ]]; then
    cp "${archive}" "${target_path}.tmp"
    chmod 0644 "${target_path}.tmp"
    mv "${target_path}.tmp" "${target_path}"
fi
for role in root targets snapshot timestamp; do
    cp "${metadata_dir}/${role}.json" "${metadata_path}/${role}.json.tmp"
    chmod 0644 "${metadata_path}/${role}.json.tmp"
    mv "${metadata_path}/${role}.json.tmp" "${metadata_path}/${role}.json"
done
python3 - "${output}/promotion.json" "${tag}" "${target}" "${expected_sha}" "${size}" \
    "${channel}" "${version}" "${build_id}" "${min_updater}" <<'PY'
import json, sys
path, tag, target, sha, size, channel, version, build_id, min_updater = sys.argv[1:10]
document = {
    "schema": 1,
    "source_tag": tag,
    "target": target,
    "sha256": sha,
    "length": int(size),
    "channel": channel,
    "version": version,
    "build_id": build_id,
    "min_updater": min_updater,
}
with open(path + ".tmp", "w", encoding="utf-8") as handle:
    json.dump(document, handle, indent=2)
    handle.write("\n")
import os
os.replace(path + ".tmp", path)
PY
echo "promote: staged ${target} (${size} bytes) and signed metadata under ${output}"
