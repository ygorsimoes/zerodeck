#!/usr/bin/env bash
# TUF promotion boundary tests: unsigned/expired/unauthorized metadata and missing attestations must
# never stage bytes, and a failed promotion must leave a previously staged catalog untouched.
set -euo pipefail

tests_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${tests_dir}/../../.." && pwd)"
export FIXTURE_REPO_ROOT="${repo_root}"
# shellcheck source=fixtures.sh
source "${tests_dir}/fixtures.sh"

tmp="$(mktemp -d "${TMPDIR:-/tmp}/zerodeck-promotion-test.XXXXXX")"
trap 'if [[ "${ZERODECK_KEEP_TMP:-0}" == "1" ]]; then echo "kept test dir: ${tmp}" >&2; else rm -rf "${tmp}"; fi' EXIT

version="0.1.0"
build_id="fixture-build-v0.1.0-deadbeef"
tag="v${version}"
assets="${tmp}/assets"
mkdir -p "${assets}"
fixture_release_assets "${assets}" "${version}" "${build_id}" 1 >/dev/null
managed="${assets}/zerodeck-${tag}-linux-x86_64.managed.tar.gz"
trusted_root="${assets}/trusted-root.json"
trusted_root_sha="$(fixture_sha256 "${trusted_root}")"
managed_sha="$(fixture_sha256 "${managed}")"
managed_size="$(wc -c <"${managed}" | tr -d ' ')"
target="linux-x86_64/zerodeck-${version}.tar.gz"
attestation="${tmp}/attestation.json"
printf '{"verified":true,"sha256":"%s"}\n' "${managed_sha}" >"${attestation}"

write_metadata() {
    local directory="$1" expires="$2" signatures="$3" digest="$4" length="$5"
    mkdir -p "${directory}"
    cp "${trusted_root}" "${directory}/root.json"
    python3 - "${directory}" "${expires}" "${signatures}" "${digest}" "${length}" \
        "${target}" "${version}" "${build_id}" <<'PY'
import json, sys
from pathlib import Path

directory, expires, signatures, digest, length, target, version, build_id = sys.argv[1:9]
expires = expires

def write(role, keyid, signed, signed_by):
    document = {"signed": signed, "signatures": []}
    if signed_by:
        document["signatures"].append({"keyid": keyid, "sig": "00"})
    (Path(directory) / f"{role}.json").write_text(
        json.dumps(document, indent=2) + "\n", encoding="utf-8"
    )

write(
    "targets",
    "loader-targets-1",
    {
        "_type": "targets",
        "version": 2,
        "expires": expires,
        "targets": {
            target: {
                "length": int(length),
                "hashes": {"sha256": digest},
                "custom": {
                    "channel": "stable",
                    "version": version,
                    "build_id": build_id,
                    "min_updater": "0.1.0",
                },
            }
        },
    },
    signatures,
)
write("snapshot", "loader-targets-1", {"_type": "snapshot", "version": 2, "expires": expires}, signatures)
write(
    "timestamp",
    "loader-targets-1",
    {"_type": "timestamp", "version": 2, "expires": expires},
    signatures,
)
PY
}

promote() {
    bash "${repo_root}/deploy/update/promote.sh" \
        --archive "${managed}" --sha256 "${managed_sha}" --tag "${tag}" \
        --version "${version}" --build-id "${build_id}" --min-updater 0.1.0 \
        --trusted-root "${trusted_root}" --trusted-root-sha256 "${trusted_root_sha}" \
        --attestation "${attestation}" "$@"
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

# A verify-only check never writes a repository and reports the exact published digest.
promote --verify-only >"${tmp}/verify.log"
grep -q "sha256: ${managed_sha}" "${tmp}/verify.log"
grep -q "length: ${managed_size}" "${tmp}/verify.log"
[[ ! -e "${tmp}/repo" ]] || { echo "verify-only created a repository" >&2; exit 1; }

# Attestation and digest gates.
expect_failure missing-attestation promote --verify-only --attestation "${tmp}/missing.json"
printf '{"verified":false,"sha256":"%s"}\n' "${managed_sha}" >"${tmp}/unverified.json"
expect_failure unverified-attestation promote --verify-only --attestation "${tmp}/unverified.json"
printf '{"verified":true,"sha256":"%s"}\n' "$(printf '0%.0s' {1..64})" >"${tmp}/wrong-attestation.json"
expect_failure wrong-attestation promote --verify-only --attestation "${tmp}/wrong-attestation.json"

# Missing signed metadata never stages anything.
expect_failure missing-metadata promote --metadata-dir "${tmp}/no-metadata" --output "${tmp}/repo"
[[ ! -e "${tmp}/repo" ]] || { echo "missing metadata created a repository" >&2; exit 1; }

metadata="${tmp}/metadata"
write_metadata "${metadata}" "2099-01-01T00:00:00Z" 1 "${managed_sha}" "${managed_size}"

# A previous known catalog remains untouched while promotion is refused.
repo="${tmp}/repo"
mkdir -p "${repo}/targets/linux-x86_64"
printf 'previous release bytes\n' >"${repo}/targets/linux-x86_64/zerodeck-0.0.9.tar.gz"
mkdir -p "${repo}/metadata"
printf 'previous signed targets\n' >"${repo}/metadata/targets.json"
previous_target_sha="$(fixture_sha256 "${repo}/targets/linux-x86_64/zerodeck-0.0.9.tar.gz")"
previous_metadata_sha="$(fixture_sha256 "${repo}/metadata/targets.json")"

cp -a "${metadata}" "${tmp}/unsigned"
python3 - "${tmp}/unsigned/targets.json" <<'PY'
import json, sys
path = sys.argv[1]
document = json.load(open(path, encoding="utf-8"))
document["signatures"] = []
json.dump(document, open(path, "w", encoding="utf-8"))
PY
expect_failure unsigned promote --metadata-dir "${tmp}/unsigned" --output "${repo}"
[[ "$(fixture_sha256 "${repo}/targets/linux-x86_64/zerodeck-0.0.9.tar.gz")" == "${previous_target_sha}" ]] || {
    echo "refused unsigned promotion changed the previous target" >&2; exit 1;
}
[[ "$(fixture_sha256 "${repo}/metadata/targets.json")" == "${previous_metadata_sha}" ]] || {
    echo "refused unsigned promotion changed the previous metadata" >&2; exit 1;
}

cp -a "${metadata}" "${tmp}/expired"
write_metadata "${tmp}/expired" "2000-01-01T00:00:00Z" 1 "${managed_sha}" "${managed_size}"
expect_failure expired promote --metadata-dir "${tmp}/expired" --output "${repo}"

cp -a "${metadata}" "${tmp}/unauthorized"
python3 - "${tmp}/unauthorized/targets.json" <<'PY'
import json, sys
path = sys.argv[1]
document = json.load(open(path, encoding="utf-8"))
document["signatures"] = [{"keyid": "untrusted-key", "sig": "00"}]
json.dump(document, open(path, "w", encoding="utf-8"))
PY
expect_failure unauthorized promote --metadata-dir "${tmp}/unauthorized" --output "${repo}"

cp -a "${metadata}" "${tmp}/wrong-digest"
python3 - "${tmp}/wrong-digest/targets.json" "${managed_sha}" "${version}" "${build_id}" <<'PY'
import json, sys
path, digest, version, build_id = sys.argv[1:5]
document = json.load(open(path, encoding="utf-8"))
target = f"linux-x86_64/zerodeck-{version}.tar.gz"
document["signed"]["targets"][target]["hashes"]["sha256"] = "0" * 64
json.dump(document, open(path, "w", encoding="utf-8"))
PY
expect_failure wrong-digest promote --metadata-dir "${tmp}/wrong-digest" --output "${repo}"

# The full promotion copies the exact published bytes and signed metadata.
promote --metadata-dir "${metadata}" --output "${repo}" >"${tmp}/promote.log"
[[ "$(fixture_sha256 "${repo}/targets/${target}")" == "${managed_sha}" ]] || {
    echo "promoted target bytes differ from the published archive" >&2; exit 1;
}
for role in root targets snapshot timestamp; do
    cmp "${metadata}/${role}.json" "${repo}/metadata/${role}.json" >/dev/null || {
        echo "promoted ${role} metadata differs" >&2; exit 1;
    }
done
[[ "$(fixture_sha256 "${repo}/targets/linux-x86_64/zerodeck-0.0.9.tar.gz")" == "${previous_target_sha}" ]] || {
    echo "successful promotion removed the previous catalog target" >&2; exit 1;
}

# Re-promoting the same published bytes is idempotent; a different staged digest is refused.
promote --metadata-dir "${metadata}" --output "${repo}" >/dev/null
printf 'different staged bytes\n' >"${repo}/targets/${target}"
expect_failure different-staged promote --metadata-dir "${metadata}" --output "${repo}"

# A rotated trusted root must authorize a new targets key; the old key must stop working.
rotated_root="${tmp}/root2.json"
python3 - "${rotated_root}" <<'PY'
import json, sys
document = {
    "signed": {
        "_type": "root",
        "version": 2,
        "expires": "2099-01-01T00:00:00Z",
        "keys": {
            "loader-root-2": {"keytype": "ed25519", "scheme": "ed25519", "keyval": {"public": "root-2"}},
            "loader-targets-2": {"keytype": "ed25519", "scheme": "ed25519", "keyval": {"public": "targets-2"}},
        },
        "roles": {
            "root": {"keyids": ["loader-root-2"], "threshold": 1},
            "targets": {"keyids": ["loader-targets-2"], "threshold": 1},
            "snapshot": {"keyids": ["loader-targets-2"], "threshold": 1},
            "timestamp": {"keyids": ["loader-targets-2"], "threshold": 1},
        },
    },
    "signatures": [{"keyid": "loader-root-2", "sig": "00"}],
}
json.dump(document, open(sys.argv[1], "w", encoding="utf-8"))
PY
rotated_sha="$(fixture_sha256 "${rotated_root}")"
cp -a "${metadata}" "${tmp}/rotated"
cp "${rotated_root}" "${tmp}/rotated/root.json"
python3 - "${tmp}/rotated" <<'PY'
import json, pathlib, sys
directory = pathlib.Path(sys.argv[1])
for role in ("targets", "snapshot", "timestamp"):
    path = directory / f"{role}.json"
    document = json.load(open(path, encoding="utf-8"))
    document["signatures"] = [{"keyid": "loader-targets-2", "sig": "00"}]
    json.dump(document, open(path, "w", encoding="utf-8"))
PY
bash "${repo_root}/deploy/update/promote.sh" \
    --archive "${managed}" --sha256 "${managed_sha}" --tag "${tag}" \
    --version "${version}" --build-id "${build_id}" --min-updater 0.1.0 \
    --trusted-root "${rotated_root}" --trusted-root-sha256 "${rotated_sha}" \
    --attestation "${attestation}" --metadata-dir "${tmp}/rotated" \
    --output "${tmp}/repo-rotated" >/dev/null
[[ "$(fixture_sha256 "${tmp}/repo-rotated/targets/${target}")" == "${managed_sha}" ]] || {
    echo "rotated-root promotion did not stage the published bytes" >&2; exit 1;
}
expect_failure old-key-after-rotation bash "${repo_root}/deploy/update/promote.sh" \
    --archive "${managed}" --sha256 "${managed_sha}" --tag "${tag}" \
    --version "${version}" --build-id "${build_id}" --min-updater 0.1.0 \
    --trusted-root "${rotated_root}" --trusted-root-sha256 "${rotated_sha}" \
    --attestation "${attestation}" --metadata-dir "${metadata}" \
    --output "${tmp}/repo-old-key"

echo "promotion tests passed"
