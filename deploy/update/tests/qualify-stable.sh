#!/usr/bin/env bash
# Qualify the exact, already published managed archive before it can enter Stable.
set -euo pipefail

[[ $# -eq 3 ]] || { echo "usage: qualify-stable.sh <repo> <tag> <output-dir>" >&2; exit 2; }
repo="$1"
tag="$2"
out="$3"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
gh_bin="$(command -v -- "${GH_BIN:-gh}")"
tag_pattern='^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(\+[0-9A-Za-z.-]+)?$'
[[ "${tag}" =~ ${tag_pattern} ]] && [[ "${tag}" != v0.0.0 ]] || {
    echo "qualify-stable: expected a stable version with a possible predecessor" >&2
    exit 2
}

download_verified() {
    local version_tag="$1" dest="$2"
    shift 2
    local release source_sha pattern asset
    release="$("${gh_bin}" api "repos/${repo}/releases/tags/${version_tag}")" || return
    python3 - "${version_tag}" "${release}" <<'PY' || return
import json
import sys

tag, raw = sys.argv[1:]
release = json.loads(raw)
if (
    release.get("tag_name") != tag
    or release.get("draft")
    or release.get("prerelease")
    or release.get("immutable") is not True
):
    raise SystemExit(f"qualify-stable: {tag} is not a published immutable stable release")
PY
    source_sha="$("${gh_bin}" api "repos/${repo}/commits/${version_tag}" --jq .sha)" || return
    [[ "${source_sha}" =~ ^[0-9a-f]{40}$ ]] || return 1
    mkdir -p "${dest}" || return
    for pattern in "$@"; do
        asset="zerodeck-${version_tag}-linux-x86_64${pattern}.tar.gz"
        "${gh_bin}" release download "${version_tag}" --repo "${repo}" \
            --pattern "${asset}" --dir "${dest}" >&2 || return
        "${gh_bin}" attestation verify "${dest}/${asset}" \
            --repo "${repo}" --signer-workflow "${repo}/.github/workflows/release-please.yml" \
            --source-digest "${source_sha}" --source-ref refs/heads/main \
            --deny-self-hosted-runners >/dev/null || return
    done
    printf '%s\n' "${source_sha}"
}

candidate_dir="${out}/candidate"
source_sha="$(download_verified "${tag}" "${candidate_dir}" .managed)"
candidate="${candidate_dir}/zerodeck-${tag}-linux-x86_64.managed.tar.gz"
digest="$(sha256sum "${candidate}" | cut -d' ' -f1)"

# Pick the baseline from immutable release history. Stable is only a baseline when its exact
# archive carries a qualification attestation; the mutable channels branch is never consulted.
releases_file="${out}/releases.json"
"${gh_bin}" api "repos/${repo}/releases?per_page=100" >"${releases_file}"
stable_candidates="$(python3 - "${tag}" "${tag_pattern}" "${releases_file}" <<'PY'
import json
import re
import sys

candidate, pattern, path = sys.argv[1:4]
def version(value):
    match = re.fullmatch(pattern, value)
    return tuple(map(int, match.groups()[:3])) if match else ()

current = version(candidate)
entries = []
for release in json.load(open(path, encoding="utf-8")):
    if release.get("draft") or release.get("prerelease") or release.get("immutable") is not True:
        continue
    tag = release.get("tag_name", "")
    previous = version(tag)
    if previous and previous < current:
        entries.append((previous, tag))
for _, tag in sorted(entries, reverse=True):
    print(tag)
PY
)"
baseline_dir="${out}/baseline"
baseline_tag=""
if [[ -n "${stable_candidates}" ]]; then
    while IFS= read -r previous; do
        [[ -n "${previous}" ]] || continue
        if ! download_verified "${previous}" "${baseline_dir}" "" .managed >/dev/null; then
            continue
        fi
        previous_archive="${baseline_dir}/zerodeck-${previous}-linux-x86_64.managed.tar.gz"
        if "${gh_bin}" attestation verify "${previous_archive}" \
            --repo "${repo}" \
            --signer-workflow "${repo}/.github/workflows/qualify-stable.yml" \
            --source-ref refs/heads/main \
            --predicate-type "https://github.com/${repo}/attestations/stable-vm/v1" \
            --deny-self-hosted-runners >/dev/null 2>&1; then
            baseline_tag="${previous}"
            break
        fi
    done <<<"${stable_candidates}"
    if [[ -z "${baseline_tag}" ]]; then
        echo "qualify-stable: no previous Stable release has VM qualification evidence" >&2
        echo "qualify-stable: qualify that release first; a fixture is only allowed for the first release" >&2
        exit 1
    fi
    baseline_kind="release"
else
    baseline_tag="v0.0.0"
    baseline_kind="fixture"
    bash "${here}/build-candidate.sh" "${baseline_tag#v}" "${baseline_dir}" all
fi

bash "${here}/vm-ci.sh" "${baseline_dir}" "${baseline_tag}" "${candidate}" "${digest}"
printf '%s  %s\n' "${digest}" "${candidate}" | sha256sum --check --status
python3 - "${out}/stable-vm.json" "${tag}" "${source_sha}" "${digest}" \
    "${baseline_tag}" "${baseline_kind}" <<'PY'
import json
import sys

path, tag, source, sha256, baseline, kind = sys.argv[1:]
with open(path, "w", encoding="utf-8") as output:
    json.dump({
        "tag": tag,
        "sourceCommit": source,
        "archiveSha256": sha256,
        "baselineTag": baseline,
        "baselineKind": kind,
        "result": "passed",
    }, output)
PY
