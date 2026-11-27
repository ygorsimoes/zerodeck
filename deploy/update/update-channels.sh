#!/usr/bin/env bash
# Keep the signed `stable` and `testing` channel documents in step with published releases.
#
#   update-channels.sh <repository> <channels-dir> <signing-key.pem>
#
# Stable follows the newest published, immutable, non-prerelease release that already has a VM
# qualification attestation for its exact managed archive. It never reads the mutable channels
# branch to choose a target, so a rewrite there cannot redirect signing or downgrade Stable.
# Testing follows the newest published immutable release of any kind, qualified or not.
set -euo pipefail

[[ $# -eq 3 ]] || {
    echo "usage: update-channels.sh <owner/repo> <channels-dir> <signing-key.pem>" >&2
    exit 2
}
repository="$1"
dir="$2"
key="$3"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
gh_bin="$(command -v -- "${GH_BIN:-gh}")"
work="$(mktemp -d "${TMPDIR:-/tmp}/zerodeck-channels.XXXXXX")"
trap 'rm -rf "${work}"' EXIT
mkdir -p "${dir}"

releases_file="${work}/releases.json"
"${gh_bin}" api "repos/${repository}/releases?per_page=100" >"${releases_file}"

# Published immutable release tags. Stable excludes prereleases and sorts by SemVer; Testing keeps
# the API's newest-first order, which includes prereleases.
release_tags() {
    local channel="$1"
    python3 - "${channel}" "${releases_file}" <<'PY'
import json
import re
import sys

channel, path = sys.argv[1:3]
version_pattern = re.compile(r"^v(\d+)\.(\d+)\.(\d+)(?:\+[0-9A-Za-z.-]+)?$")
entries = []
for release in json.load(open(path, encoding="utf-8")):
    if release.get("draft") or release.get("immutable") is not True:
        continue
    tag = release.get("tag_name", "")
    prerelease = bool(release.get("prerelease")) or "-" in tag
    if channel == "stable":
        match = version_pattern.fullmatch(tag)
        if not match or prerelease:
            continue
        entries.append((tuple(map(int, match.groups())), tag))
    else:
        entries.append(((0, 0, 0), tag))
for _, tag in sorted(entries, reverse=True):
    print(tag)
PY
}

sign_release() {
    local channel="$1" tag="$2" sequence="$3" asset source_sha
    asset="zerodeck-${tag}-linux-x86_64.managed.tar.gz"
    "${gh_bin}" release download "${tag}" --repo "${repository}" --pattern "${asset}" \
        --dir "${work}" --clobber >&2 || return
    source_sha="$("${gh_bin}" api "repos/${repository}/commits/${tag}" --jq .sha)" || return
    bash "${here}/sign-channel.sh" \
        --archive "${work}/${asset}" --tag "${tag}" --channel "${channel}" \
        --repository "${repository}" --source-sha "${source_sha}" \
        --sequence "${sequence}" --key "${key}" --out "${dir}"
}

changed=0
sequence="$(date +%s)"
stable_tags="$(release_tags stable)"
testing_tags="$(release_tags testing)"
stable_signed=0
while IFS= read -r tag; do
    [[ -n "${tag}" ]] || continue
    if sign_release stable "${tag}" "${sequence}"; then
        stable_signed=1
        changed=1
        break
    else
        result=$?
        [[ "${result}" -eq 33 ]] || exit "${result}"
        echo "update-channels: ${tag} has no verified Stable VM evidence yet" >&2
    fi
done <<<"${stable_tags}"
if [[ "${stable_signed}" -eq 0 ]]; then
    echo "update-channels: no qualified Stable release is available; leaving Stable unchanged" >&2
fi

testing_tag="$(sed -n '1p' <<<"${testing_tags}")"
if [[ -z "${testing_tag}" ]]; then
    echo "update-channels: no published immutable release for Testing; leaving it alone" >&2
else
    sign_release testing "${testing_tag}" "${sequence}"
    changed=1
fi
echo "update-channels: changed=${changed}"
