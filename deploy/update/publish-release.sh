#!/usr/bin/env bash
# Upload and verify release assets against an existing Release Please draft, then optionally publish.
#
# The script never rebuilds anything. It refuses to modify a published release, compares any
# existing asset by downloading it, and only clears the draft after every expected asset has been
# downloaded back and matched against `SHA256SUMS.txt`.
#
#   deploy/update/publish-release.sh --repo owner/repo --tag v0.1.0 --dist dist
#   deploy/update/publish-release.sh --repo owner/repo --tag v0.1.0 --dist dist --publish
set -euo pipefail

repo=""
tag=""
dist=""
publish=0
gh_bin="${GH_BIN:-gh}"

usage() {
    cat >&2 <<'EOF'
usage: publish-release.sh --repo <owner/repo> --tag <tag> --dist <dir> [--publish] [--gh <path>]

The release must already exist as a draft (created by Release Please). A retry targets the same
draft; a published release is only ever verified, never rewritten.
EOF
    exit 2
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --repo) repo="${2:-}"; shift 2 ;;
        --tag) tag="${2:-}"; shift 2 ;;
        --dist) dist="${2:-}"; shift 2 ;;
        --publish) publish=1; shift ;;
        --gh) gh_bin="${2:-}"; shift 2 ;;
        --help|-h) usage ;;
        *) usage ;;
    esac
done
[[ -n "${repo}" && -n "${tag}" && -n "${dist}" ]] || usage
[[ -d "${dist}" ]] || { echo "publish: dist directory not found: ${dist}" >&2; exit 1; }
[[ -x "${gh_bin}" ]] || { echo "publish: gh executable not found: ${gh_bin}" >&2; exit 1; }

manual="zerodeck-${tag}-linux-x86_64.tar.gz"
managed="zerodeck-${tag}-linux-x86_64.managed.tar.gz"
checksums="SHA256SUMS.txt"
assets=("${manual}" "${managed}" "${checksums}")
for asset in "${assets[@]}"; do
    [[ -f "${dist}/${asset}" ]] || { echo "publish: missing release asset: ${dist}/${asset}" >&2; exit 1; }
done

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

python3 - "${dist}/${checksums}" "${manual}" "${managed}" <<'PY'
import hashlib
import re
import sys
from pathlib import Path

manifest = Path(sys.argv[1])
expected = set(sys.argv[2:])
entries = {}
for line in manifest.read_text(encoding="utf-8").splitlines():
    line = line.strip()
    if not line:
        continue
    parts = line.split(None, 1)
    if len(parts) != 2 or not re.fullmatch(r"[0-9a-f]{64}", parts[0]):
        raise SystemExit(f"publish: malformed checksum line: {line!r}")
    name = parts[1].lstrip("*")
    if name in entries:
        raise SystemExit(f"publish: duplicate checksum entry: {name}")
    entries[name] = parts[0]
missing = expected - set(entries)
if missing:
    raise SystemExit(f"publish: SHA256SUMS.txt does not cover {sorted(missing)}")
for name in expected:
    path = manifest.parent / name
    digest = hashlib.sha256(path.read_bytes()).hexdigest()
    if digest != entries[name]:
        raise SystemExit(f"publish: local checksum mismatch for {name}")
PY

tmp="$(mktemp -d "${TMPDIR:-/tmp}/zerodeck-publish.XXXXXX")"
trap 'rm -rf "${tmp}"' EXIT

release_json="$("${gh_bin}" api "repos/${repo}/releases/tags/${tag}")" || {
    echo "publish: release ${tag} does not exist; create the Release Please draft first" >&2
    exit 1
}
python3 - "${release_json}" >"${tmp}/release.meta" <<'PY'
import json, sys
document = json.loads(sys.argv[1])
print("1" if document.get("draft") else "0")
print(document.get("id", ""))
print(document.get("tag_name", ""))
for asset in document.get("assets", []):
    print(f"asset\t{asset.get('name')}\t{asset.get('id')}")
PY
draft="$(sed -n '1p' "${tmp}/release.meta")"
release_id="$(sed -n '2p' "${tmp}/release.meta")"
remote_tag="$(sed -n '3p' "${tmp}/release.meta")"
[[ "${remote_tag}" == "${tag}" ]] || { echo "publish: release tag mismatch: ${remote_tag}" >&2; exit 1; }
[[ -n "${release_id}" ]] || { echo "publish: release has no id" >&2; exit 1; }

download_remote() {
    local name="$1" directory="$2"
    "${gh_bin}" release download "${tag}" --repo "${repo}" --pattern "${name}" \
        --dir "${directory}" --clobber
}

verify_remote() {
    local name="$1" expected="$2" directory="${tmp}/verify-${1}"
    mkdir -p "${directory}"
    download_remote "${name}" "${directory}" || return 1
    [[ -f "${directory}/${name}" ]] || return 1
    local actual
    actual="$(sha256_file "${directory}/${name}")"
    [[ "${actual}" == "${expected}" ]]
}

verify_all() {
    local asset expected
    for asset in "${assets[@]}"; do
        expected="$(sha256_file "${dist}/${asset}")"
        verify_remote "${asset}" "${expected}" || {
            echo "publish: ${asset} does not match the local bytes after upload" >&2
            return 1
        }
    done
}

assert_remote_asset_set() {
    local allowed
    allowed="$(printf '%s\n' "${assets[@]}" | sort)"
    local actual
    actual="$(awk -F'\t' '$1 == "asset" {print $2}' "${tmp}/release.meta" | sort)"
    if [[ "${actual}" != "${allowed}" ]]; then
        echo "publish: draft asset set is not exactly the release assets" >&2
        echo "  expected: ${allowed//$'\n'/, }" >&2
        echo "  found:    ${actual//$'\n'/, }" >&2
        return 1
    fi
}

if [[ "${draft}" == "0" ]]; then
    if assert_remote_asset_set && verify_all; then
        echo "publish: release ${tag} is already published and its assets match the local bytes"
        exit 0
    fi
    echo "publish: refusing to modify the published release ${tag}; its assets do not match" >&2
    echo "publish: a published digest is immutable; create a new version instead" >&2
    exit 1
fi

for asset in "${manual}" "${managed}" "${checksums}"; do
    expected="$(sha256_file "${dist}/${asset}")"
    if verify_remote "${asset}" "${expected}"; then
        echo "publish: ${asset} already uploaded with matching bytes"
        continue
    fi
    echo "publish: uploading ${asset} to draft ${tag}"
    "${gh_bin}" release upload "${tag}" --repo "${repo}" "${dist}/${asset}" --clobber
done

# Re-read the draft (uploads may have changed the asset list), verify every byte, and refuse any
# unexpected asset that would otherwise become part of the published release.
release_json="$("${gh_bin}" api "repos/${repo}/releases/tags/${tag}")"
python3 - "${release_json}" >"${tmp}/release.meta" <<'PY'
import json, sys
document = json.loads(sys.argv[1])
print("1" if document.get("draft") else "0")
print(document.get("id", ""))
print(document.get("tag_name", ""))
for asset in document.get("assets", []):
    print(f"asset\t{asset.get('name')}\t{asset.get('id')}")
PY
[[ "$(sed -n '1p' "${tmp}/release.meta")" == "1" ]] || {
    echo "publish: release ${tag} was published concurrently; refusing to continue" >&2
    exit 1
}
assert_remote_asset_set
verify_all
echo "publish: draft ${tag} verified with exactly the expected assets"

if [[ "${publish}" -eq 1 ]]; then
    "${gh_bin}" release edit "${tag}" --repo "${repo}" --draft=false
    echo "publish: published ${tag}"
else
    echo "publish: draft ${tag} left unpublished; rerun with --publish after attestations are recorded"
fi
