#!/usr/bin/env bash
# Keep the signed `stable` and `testing` channel documents in step with the published releases.
#
#   update-channels.sh <repository> <channels-dir> <signing-key.pem>
#
# Idempotent and safe to run on a schedule: a channel is re-signed only when its newest immutable
# release changed or its document is within REFRESH_DAYS of expiring. Stable follows the newest
# qualified non-prerelease; Testing follows the newest release of any kind. Drafts and mutable
# releases are never signed.
set -euo pipefail

[[ $# -eq 3 ]] || {
    echo "usage: update-channels.sh <owner/repo> <channels-dir> <signing-key.pem>" >&2
    exit 2
}
repository="$1"
dir="$2"
key="$3"
refresh_days="${REFRESH_DAYS:-30}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
gh_bin="$(command -v -- "${GH_BIN:-gh}")"
work="$(mktemp -d "${TMPDIR:-/tmp}/zerodeck-channels.XXXXXX")"
trap 'rm -rf "${work}"' EXIT
mkdir -p "${dir}"

# Newest published, immutable release for a channel; prints its tag or nothing.
newest_tag() {
    local channel="$1" releases
    releases="$("${gh_bin}" api "repos/${repository}/releases?per_page=30")"
    python3 - "${channel}" "${releases}" <<'PY'
import json, sys
channel, releases = sys.argv[1], json.loads(sys.argv[2])
for release in releases:
    if release.get("draft") or release.get("immutable") is not True:
        continue
    if channel == "stable" and release.get("prerelease"):
        continue
    print(release["tag_name"])
    break
PY
}

# "<sequence> <tag> <expires>" of the signed document, or "0 - 0" when there is none.
current_state() {
    local file="${dir}/$1.json"
    if [[ -f "${file}" ]]; then
        python3 - "${file}" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
print(d["sequence"], d["target"].split("/")[0], d["expires"])
PY
    else
        echo "0 - 0"
    fi
}

changed=0
sign_release() {
    local channel="$1" tag="$2" sequence="$3" asset source_sha
    asset="zerodeck-${tag}-linux-x86_64.managed.tar.gz"
    "${gh_bin}" release download "${tag}" --repo "${repository}" --pattern "${asset}" \
        --dir "${work}" --clobber >&2 || return
    source_sha="$("${gh_bin}" api "repos/${repository}/commits/${tag}" --jq .sha)" || return
    bash "${here}/sign-channel.sh" \
        --archive "${work}/${asset}" --tag "${tag}" --channel "${channel}" \
        --repository "${repository}" --source-sha "${source_sha}" \
        --sequence "$((sequence + 1))" --key "${key}" --out "${dir}"
}

for channel in stable testing; do
    tag="$(newest_tag "${channel}")"
    if [[ -z "${tag}" ]]; then
        echo "update-channels: no published immutable release for ${channel}; leaving it alone"
        continue
    fi
    read -r sequence current_tag expires <<<"$(current_state "${channel}")"
    now="$(date +%s)"
    if [[ "${current_tag}" == "${tag}" ]] && ((expires - now > refresh_days * 86400)); then
        echo "update-channels: ${channel} already points at ${tag} and is fresh"
        continue
    fi

    if sign_release "${channel}" "${tag}" "${sequence}"; then
        changed=1
        continue
    else
        result=$?
        [[ "${channel}" == stable && "${result}" -eq 33 ]] || exit "${result}"
    fi

    # Keep publishing Testing and renew the current Stable document when it nears expiry.
    # Lack of evidence for a new release must not demote or prematurely expire Stable.
    if [[ "${current_tag}" == "-" || "${current_tag}" == "${tag}" ]] \
        || ((expires - now > refresh_days * 86400)); then
        continue
    fi
    if ! sign_release stable "${current_tag}" "${sequence}"; then
        echo "update-channels: could not renew current Stable ${current_tag}; leaving it unchanged" >&2
        continue
    fi
    changed=1
done
echo "update-channels: changed=${changed}"
