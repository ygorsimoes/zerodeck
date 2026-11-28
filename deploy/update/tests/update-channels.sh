#!/usr/bin/env bash
# Channel updates: Stable follows the newest qualified immutable release without trusting the
# mutable channels branch; Testing follows the newest immutable release.
set -euo pipefail

tests_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${tests_dir}/../../.." && pwd)"
export FIXTURE_REPO_ROOT="${repo_root}"
# shellcheck source=fixtures.sh
source "${tests_dir}/fixtures.sh"

tmp="$(mktemp -d "${TMPDIR:-/tmp}/zerodeck-update-channels.XXXXXX")"
trap 'rm -rf "${tmp}"' EXIT
export FIXTURE_PACKAGES="${tmp}/packages"
export FIXTURE_RELEASES="${tmp}/releases.json"
mkdir -p "${FIXTURE_PACKAGES}"
fixture_release_assets "${FIXTURE_PACKAGES}" 0.1.0 fixture-old 1 >/dev/null
fixture_release_assets "${FIXTURE_PACKAGES}" 0.1.1 fixture-new 1 >/dev/null
openssl genpkey -algorithm ed25519 -out "${tmp}/key.pem"
openssl pkey -in "${tmp}/key.pem" -pubout -out "${tmp}/pub.pem" 2>/dev/null

cat >"${tmp}/gh" <<'SCRIPT'
#!/usr/bin/env bash
set -euo pipefail
case "$1/$2" in
  api/repos/fixture/repo/releases\?per_page=100)
    cat "${FIXTURE_RELEASES}"
    ;;
  api/repos/fixture/repo/commits/*)
    printf 'a%.0s' {1..40}
    ;;
  release/download)
    tag="$3"
    dir=""
    for ((i = 4; i <= $#; i++)); do
      if [[ "${!i}" == "--dir" ]]; then
        j=$((i + 1))
        dir="${!j}"
      fi
    done
    cp "${FIXTURE_PACKAGES}/zerodeck-${tag}-linux-x86_64.managed.tar.gz" "${dir}/"
    ;;
  attestation/verify)
    archive="$3"
    shift 3
    workflow=""
    for ((i = 1; i <= $#; i++)); do
      if [[ "${!i}" == "--signer-workflow" ]]; then
        j=$((i + 1))
        workflow="${!j}"
      fi
    done
    [[ "${archive}" =~ /zerodeck-(v[0-9.]+)-linux-x86_64 ]] || exit 1
    tag="${BASH_REMATCH[1]}"
    if [[ "${workflow}" == *"/qualify-stable.yml" ]]; then
      [[ " ${FIXTURE_QUALIFIED:-} " == *" ${tag} "* ]]
    fi
    ;;
  *)
    printf 'unexpected gh call: %s\n' "$*" >&2
    exit 1
    ;;
esac
SCRIPT
chmod 0755 "${tmp}/gh"
export GH_BIN="${tmp}/gh"

releases() {
    cat >"${FIXTURE_RELEASES}" <<JSON
[
  {"tag_name": "v0.2.0", "draft": false, "prerelease": false, "immutable": false},
  {"tag_name": "v0.1.1", "draft": false, "prerelease": false, "immutable": true},
  {"tag_name": "v0.1.0", "draft": false, "prerelease": false, "immutable": true}
]
JSON
}

run_channels() {
    local out="$1"
    rm -rf "$out"
    mkdir -p "$out"
    bash "${repo_root}/deploy/update/update-channels.sh" fixture/repo "$out" "${tmp}/key.pem" >&2
}

document_field() {
    python3 - "$1" "$2" <<'PY'
import json
import sys
print(json.load(open(sys.argv[1]))[sys.argv[2]])
PY
}

verify_signature() {
    python3 -c 'import sys; open(sys.argv[2],"wb").write(bytes.fromhex(open(sys.argv[1]).read().strip()))' \
        "$1.sig" "${tmp}/sig.bin"
    openssl pkeyutl -verify -rawin -pubin -inkey "${tmp}/pub.pem" -in "$1" -sigfile "${tmp}/sig.bin" >/dev/null
}

releases
FIXTURE_QUALIFIED="v0.1.0" run_channels "${tmp}/fallback"
verify_signature "${tmp}/fallback/stable.json"
verify_signature "${tmp}/fallback/testing.json"
[[ "$(document_field "${tmp}/fallback/stable.json" target)" == \
   "v0.1.0/zerodeck-v0.1.0-linux-x86_64.managed.tar.gz" ]]
[[ "$(document_field "${tmp}/fallback/testing.json" target)" == \
   "v0.1.1/zerodeck-v0.1.1-linux-x86_64.managed.tar.gz" ]]

FIXTURE_QUALIFIED="v0.1.0 v0.1.1" run_channels "${tmp}/newest"
[[ "$(document_field "${tmp}/newest/stable.json" target)" == \
   "v0.1.1/zerodeck-v0.1.1-linux-x86_64.managed.tar.gz" ]]

FIXTURE_QUALIFIED="" run_channels "${tmp}/none"
[[ ! -e "${tmp}/none/stable.json" ]]
verify_signature "${tmp}/none/testing.json"

# A mutable release is never a candidate for either channel.
python3 - "${FIXTURE_RELEASES}" <<'PY'
import json
import sys
path = sys.argv[1]
releases = json.load(open(path))
releases[0]["tag_name"] = "v9.9.9"
json.dump(releases, open(path, "w"))
PY
FIXTURE_QUALIFIED="v0.1.0" run_channels "${tmp}/mutable"
[[ "$(document_field "${tmp}/mutable/stable.json" target)" == \
   "v0.1.0/zerodeck-v0.1.0-linux-x86_64.managed.tar.gz" ]]
[[ "$(document_field "${tmp}/mutable/testing.json" target)" == \
   "v0.1.1/zerodeck-v0.1.1-linux-x86_64.managed.tar.gz" ]]
echo "update-channels tests passed"
