#!/usr/bin/env bash
# Upload and verify release assets by immutable release ID, then optionally publish the draft.
#
# GitHub's `releases/tags/<tag>` endpoint only resolves published releases; Release Please drafts
# have no git tag. Every lookup here therefore uses the release ID. Uploads go through the asset API
# without `--clobber`, with a draft-status re-check immediately before each mutation. The caller must
# provide a repository/workflow concurrency group and immutable releases must be enabled.
set -euo pipefail

repo=""
release_id=""
tag=""
dist=""
publish=0
gh_bin="${GH_BIN:-gh}"
curl_bin="${CURL_BIN:-curl}"

usage() {
    cat >&2 <<'EOF'
usage: publish-release.sh --repo <owner/repo> --release-id <id> --tag <tag> --dist <dir>
                          [--publish] [--gh <path>] [--curl <path>]

The release must already exist as a draft (created by Release Please). A retry targets the same
release ID; a published release is only ever verified, never rewritten. Do not use a tag for draft
lookup.
EOF
    exit 2
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --repo) repo="${2:-}"; shift 2 ;;
        --release-id) release_id="${2:-}"; shift 2 ;;
        --tag) tag="${2:-}"; shift 2 ;;
        --dist) dist="${2:-}"; shift 2 ;;
        --publish) publish=1; shift ;;
        --gh) gh_bin="${2:-}"; shift 2 ;;
        --curl) curl_bin="${2:-}"; shift 2 ;;
        --help|-h) usage ;;
        *) usage ;;
    esac
done
[[ -n "${repo}" && -n "${release_id}" && -n "${tag}" && -n "${dist}" ]] || usage
[[ "${release_id}" =~ ^[0-9]+$ ]] || { echo "publish: --release-id must be numeric" >&2; exit 1; }
[[ -d "${dist}" ]] || { echo "publish: dist directory not found: ${dist}" >&2; exit 1; }
[[ -x "${gh_bin}" ]] || { echo "publish: gh executable not found: ${gh_bin}" >&2; exit 1; }
[[ -x "${curl_bin}" ]] || { echo "publish: curl executable not found: ${curl_bin}" >&2; exit 1; }

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

fetch_release() {
    "${gh_bin}" api "repos/${repo}/releases/${release_id}" >"${tmp}/release.json" || {
        echo "publish: release id ${release_id} is not accessible" >&2
        return 1
    }
    python3 - "${tmp}/release.json" >"${tmp}/release.meta" <<'PY'
import json, sys
document = json.load(open(sys.argv[1], encoding="utf-8"))
print("draft\t" + ("1" if document.get("draft") else "0"))
print("tag\t" + str(document.get("tag_name", "")))
print("id\t" + str(document.get("id", "")))
print("upload_url\t" + str(document.get("upload_url", "")))
print("immutable\t" + ("1" if document.get("immutable") else "0"))
for asset in document.get("assets", []):
    print(f"asset\t{asset.get('name')}\t{asset.get('id')}\t{asset.get('size', '')}")
PY
}

release_draft() {
    fetch_release || return 1
    [[ "$(awk -F'\t' '$1 == "draft" {print $2}' "${tmp}/release.meta")" == "1" ]]
}

assert_expected_release() {
    fetch_release || return 1
    local remote_tag
    remote_tag="$(awk -F'\t' '$1 == "tag" {print $2}' "${tmp}/release.meta")"
    [[ "${remote_tag}" == "${tag}" ]] || {
        echo "publish: release id ${release_id} has tag ${remote_tag}, expected ${tag}" >&2
        return 1
    }
}

assert_draft_before_mutation() {
    assert_expected_release || return 1
    if [[ "$(awk -F'\t' '$1 == "draft" {print $2}' "${tmp}/release.meta")" != "1" ]]; then
        echo "publish: release ${tag} is no longer a draft; refusing any asset mutation" >&2
        return 1
    fi
}

assert_immutable_releases_enabled() {
    local policy
    policy="$("${gh_bin}" api "repos/${repo}/immutable-releases" 2>/dev/null)" || {
        echo "publish: cannot verify immutable releases; enable them and grant admin read access" >&2
        echo "publish: refusing to publish a mutable release" >&2
        return 1
    }
    python3 -c '
import json, sys
document = json.load(sys.stdin)
raise SystemExit(0 if document.get("enabled") is True else 1)
' <<<"${policy}" || {
        echo "publish: immutable releases are not enabled for ${repo}; refusing to publish" >&2
        return 1
    }
}

asset_id() {
    local name="$1"
    awk -F'\t' -v name="${name}" '$1 == "asset" && $2 == name {print $3}' "${tmp}/release.meta"
}

download_asset() {
    local name="$1" directory="$2"
    assert_expected_release || return 1
    local id
    id="$(asset_id "${name}")"
    [[ -n "${id}" ]] || return 1
    mkdir -p "${directory}"
    "${gh_bin}" api "repos/${repo}/releases/assets/${id}" \
        -H "Accept: application/octet-stream" >"${directory}/${name}" || return 1
    [[ -f "${directory}/${name}" ]]
}

verify_remote() {
    local name="$1" expected="$2" directory
    directory="${tmp}/verify-${name}"
    mkdir -p "${directory}"
    download_asset "${name}" "${directory}" || return 1
    [[ "$(sha256_file "${directory}/${name}")" == "${expected}" ]]
}

assert_remote_asset_set() {
    local allowed actual
    allowed="$(printf '%s\n' "${assets[@]}" | sort)"
    actual="$(awk -F'\t' '$1 == "asset" {print $2}' "${tmp}/release.meta" | sort)"
    if [[ "${actual}" != "${allowed}" ]]; then
        echo "publish: release asset set is not exactly the release assets" >&2
        echo "  expected: ${allowed//$'\n'/, }" >&2
        echo "  found:    ${actual//$'\n'/, }" >&2
        return 1
    fi
}

verify_all() {
    local asset expected
    assert_expected_release || return 1
    for asset in "${assets[@]}"; do
        expected="$(sha256_file "${dist}/${asset}")"
        verify_remote "${asset}" "${expected}" || {
            echo "publish: ${asset} does not match the local bytes" >&2
            return 1
        }
    done
}

upload_asset() {
    local name="$1" file="$2" encoded upload_url base token url
    upload_url="$(awk -F'\t' '$1 == "upload_url" {print $2}' "${tmp}/release.meta")"
    [[ "${upload_url}" == https://uploads.github.com/* ]] || {
        echo "publish: release upload_url is not uploads.github.com: ${upload_url}" >&2
        return 1
    }
    token="${GH_TOKEN:-${GITHUB_TOKEN:-}}"
    [[ -n "${token}" ]] || { echo "publish: GH_TOKEN is required for asset upload" >&2; return 1; }
    encoded="$(python3 -c 'import sys, urllib.parse; print(urllib.parse.quote(sys.argv[1], safe=""))' "${name}")"
    base="${upload_url%%\{*}"
    url="${base}?name=${encoded}"
    "${curl_bin}" --fail --silent --show-error --request POST \
        --header "Authorization: Bearer ${token}" \
        --header "Content-Type: application/octet-stream" \
        --data-binary "@${file}" "${url}" >/dev/null
}

delete_asset() {
    local id="$1"
    "${gh_bin}" api --method DELETE "repos/${repo}/releases/assets/${id}" >/dev/null
}

assert_expected_release
assert_immutable_releases_enabled
if [[ "$(awk -F'\t' '$1 == "draft" {print $2}' "${tmp}/release.meta")" == "0" ]]; then
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
    assert_draft_before_mutation || exit 1
    id="$(asset_id "${asset}")"
    if [[ -n "${id}" ]]; then
        if verify_remote "${asset}" "${expected}"; then
            echo "publish: ${asset} already uploaded with matching bytes"
            continue
        fi
        assert_draft_before_mutation || exit 1
        echo "publish: replacing mismatched draft asset ${asset}"
        delete_asset "${id}"
    fi
    assert_draft_before_mutation || exit 1
    echo "publish: uploading ${asset} to draft ${tag}"
    upload_asset "${asset}" "${dist}/${asset}"
done

# Re-read the release, verify every byte, and refuse any unexpected asset before publication.
assert_draft_before_mutation || exit 1
assert_remote_asset_set
verify_all
echo "publish: draft ${tag} verified with exactly the expected assets"

if [[ "${publish}" -eq 1 ]]; then
    assert_draft_before_mutation || exit 1
    assert_immutable_releases_enabled || exit 1
    "${gh_bin}" api --method PATCH "repos/${repo}/releases/${release_id}" \
        -F draft=false >/dev/null
    fetch_release || exit 1
    [[ "$(awk -F'\t' '$1 == "draft" {print $2}' "${tmp}/release.meta")" == "0" ]] || {
        echo "publish: release ${tag} is still a draft after the publish request" >&2
        exit 1
    }
    [[ "$(awk -F'\t' '$1 == "immutable" {print $2}' "${tmp}/release.meta")" == "1" ]] || {
        echo "publish: published release is not immutable; repository immutable releases are not enforced" >&2
        exit 1
    }
    assert_remote_asset_set
    verify_all
    echo "publish: published ${tag} and verified the final immutable asset digests"
else
    echo "publish: draft ${tag} left unpublished; rerun with --publish after attestations are recorded"
fi
