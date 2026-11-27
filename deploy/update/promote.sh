#!/usr/bin/env bash
# Verify already-published loader bytes and render the exact TUF target descriptor.
#
# The signed-metadata trust boundary lives in the external TUF-on-CI repository, which runs a real
# TUF client (`tough` or equivalent) against its out-of-band root. This script deliberately does not
# parse, count or "verify" signatures itself: it verifies the published archive identity and GitHub
# build provenance, then emits the descriptor the external signer must authorize. No private key or
# unsigned metadata is accepted here.
set -euo pipefail

archive=""
expected_sha=""
tag=""
version=""
build_id=""
channel="stable"
min_updater=""
repository=""
source_sha=""
signer_workflow=""
source_ref="refs/heads/main"
gh_bin="${GH_BIN:-gh}"
target=""

usage() {
    cat >&2 <<'EOF'
usage: promote.sh --archive <published-managed.tar.gz> --sha256 <hex> --tag <tag>
                  --version <semver> --build-id <id> --min-updater <semver>
                  [--channel stable|testing]
                  --repository <owner/repo> --source-sha <40-char-commit>
                  --signer-workflow <owner/repo/.github/workflows/file.yml>
                  [--source-ref refs/heads/main] [--target <path>] [--gh <path>]

The script verifies the archive digest and `release.json`, and runs `gh attestation verify` with the
exact repository, signer workflow, source commit and source ref before printing the target
descriptor. Signed TUF metadata, snapshot/timestamp hash chains, expiry and version monotonicity are
validated by the external TUF-on-CI repository, not by this script.
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
        --repository) repository="${2:-}"; shift 2 ;;
        --source-sha) source_sha="${2:-}"; shift 2 ;;
        --signer-workflow) signer_workflow="${2:-}"; shift 2 ;;
        --source-ref) source_ref="${2:-}"; shift 2 ;;
        --target) target="${2:-}"; shift 2 ;;
        --gh) gh_bin="${2:-}"; shift 2 ;;
        --help|-h) usage ;;
        *) usage ;;
    esac
done

[[ -n "${archive}" && -n "${expected_sha}" && -n "${tag}" && -n "${version}" ]] || usage
[[ -n "${build_id}" && -n "${min_updater}" && -n "${repository}" ]] || usage
[[ -n "${source_sha}" && -n "${signer_workflow}" ]] || usage
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
[[ "${source_sha}" =~ ^[0-9a-f]{40}$ ]] || { echo "promote: --source-sha must be a 40-character lowercase commit" >&2; exit 1; }
[[ "${signer_workflow}" == */.github/workflows/*.yml ]] || {
    echo "promote: --signer-workflow must name a repository workflow file" >&2
    exit 1
}
[[ -f "${archive}" ]] || { echo "promote: archive not found: ${archive}" >&2; exit 1; }
# `command -v` resolves a bare name through PATH; `[[ -x name ]]` only checks the current directory.
gh_bin="$(command -v -- "${gh_bin}")" || { echo "promote: gh executable not found" >&2; exit 1; }
expected_sha="$(printf '%s' "${expected_sha}" | tr '[:upper:]' '[:lower:]')"
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

python3 - "${archive}" "${version}" "${build_id}" "${channel}" <<'PY'
import json, re, sys, tarfile

archive, version, build_id, channel = sys.argv[1:5]
SEMVER = re.compile(r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$")
if not SEMVER.match(version):
    raise SystemExit(f"promote: invalid version: {version!r}")
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
PY

# This is the actual provenance check: exact repository, signer workflow, source commit/ref and
# artifact digest. Do not replace it with parsing free-form attestation output.
"${gh_bin}" attestation verify "${archive}" \
    --repo "${repository}" \
    --signer-workflow "${signer_workflow}" \
    --source-digest "${source_sha}" \
    --source-ref "${source_ref}" \
    --deny-self-hosted-runners

cat <<EOF
target: ${target}
sha256: ${expected_sha}
length: ${size}
channel: ${channel}
version: ${version}
build_id: ${build_id}
min_updater: ${min_updater}
source_repository: ${repository}
source_sha: ${source_sha}
source_ref: ${source_ref}
signer_workflow: ${signer_workflow}
EOF
