#!/usr/bin/env bash
# Exercise release selection and evidence creation with real signature checks and a stand-in VM.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export FIXTURE_REPO_ROOT="$(cd "${here}/../../.." && pwd)"
# shellcheck source=fixtures.sh
source "${here}/fixtures.sh"

tmp="$(mktemp -d "${TMPDIR:-/tmp}/zerodeck-qualification.XXXXXX")"
trap 'rm -rf "${tmp}"' EXIT
export FIXTURE_PACKAGES="${tmp}/packages"
export FIXTURE_CHANNEL="${tmp}/channel"
mkdir -p "${FIXTURE_PACKAGES}" "${FIXTURE_CHANNEL}" "${tmp}/scripts/tests"
cp "${here}/qualify-stable.sh" "${tmp}/scripts/tests/"

fixture_release_assets "${FIXTURE_PACKAGES}" 0.1.0 fixture-old 1 >/dev/null
fixture_release_assets "${FIXTURE_PACKAGES}" 0.1.1 fixture-new 1 >/dev/null
export FIXTURE_OLD_SHA="$(fixture_sha256 "${FIXTURE_PACKAGES}/zerodeck-v0.1.0-linux-x86_64.managed.tar.gz")"
export FIXTURE_NEW_SHA="$(fixture_sha256 "${FIXTURE_PACKAGES}/zerodeck-v0.1.1-linux-x86_64.managed.tar.gz")"
openssl genpkey -algorithm ed25519 -out "${tmp}/key.pem"
openssl pkey -in "${tmp}/key.pem" -pubout -outform DER -out "${tmp}/pub.der"
python3 - "${tmp}/pub.der" <<'PY' >"${tmp}/scripts/trusted-root.json"
import json, pathlib, sys
print(json.dumps({"keys": [pathlib.Path(sys.argv[1]).read_bytes()[-32:].hex()]}))
PY
printf '{"schema":1,"channel":"stable","target":"v0.1.0/zerodeck-v0.1.0-linux-x86_64.managed.tar.gz","digest":"%s"}\n' \
    "${FIXTURE_OLD_SHA}" >"${FIXTURE_CHANNEL}/stable.json"
openssl pkeyutl -sign -rawin -inkey "${tmp}/key.pem" -in "${FIXTURE_CHANNEL}/stable.json" \
    -out "${tmp}/stable.sig"
python3 -c 'import pathlib,sys; print(pathlib.Path(sys.argv[1]).read_bytes().hex())' \
    "${tmp}/stable.sig" >"${FIXTURE_CHANNEL}/stable.json.sig"

cat >"${tmp}/gh" <<'SCRIPT'
#!/usr/bin/env bash
set -euo pipefail
case "$1/$2" in
  api/repos/fixture/repo/releases/tags/*)
    printf '{"tag_name":"%s","draft":false,"prerelease":false,"immutable":true}\n' "${2##*/}"
    ;;
  api/repos/fixture/repo/commits/*)
    printf 'a%.0s' {1..40}
    ;;
  api/repos/fixture/repo/git/matching-refs/heads/channels)
    printf 'b%.0s' {1..40}
    ;;
  api/repos/fixture/repo/contents\?*)
    printf '%s\n' stable.json stable.json.sig
    ;;
  api/repos/fixture/repo/contents/*)
    name="${2##*/}"
    cat "${FIXTURE_CHANNEL}/${name%%\?*}"
    ;;
  release/download)
    cp "${FIXTURE_PACKAGES}/$7" "$9/"
    ;;
  attestation/verify)
    [[ "${FIXTURE_REJECT_PROVENANCE:-0}" == 0 ]]
    ;;
  *)
    printf 'unexpected gh call: %s\n' "$*" >&2
    exit 1
    ;;
esac
SCRIPT
chmod 0755 "${tmp}/gh"
export GH_BIN="${tmp}/gh"

cat >"${tmp}/scripts/tests/vm-ci.sh" <<'SCRIPT'
#!/usr/bin/env bash
set -euo pipefail
[[ "$2" == v0.1.0 && "$4" == "${FIXTURE_NEW_SHA}" ]]
[[ -f "$1/zerodeck-v0.1.0-linux-x86_64.tar.gz" ]]
printf '%s  %s\n' "${FIXTURE_NEW_SHA}" "$3" | sha256sum --check --status
[[ "${FIXTURE_REJECT_VM:-0}" == 0 ]]
SCRIPT

qualify() {
    bash "${tmp}/scripts/tests/qualify-stable.sh" fixture/repo v0.1.1 "$1"
}

qualify "${tmp}/ok"
python3 - "${tmp}/ok/stable-vm.json" <<'PY'
import json, os, pathlib, sys
result = json.loads(pathlib.Path(sys.argv[1]).read_text())
assert result["sourceCommit"] == "a" * 40, result
assert result["archiveSha256"] == os.environ["FIXTURE_NEW_SHA"], result
assert result["baselineTag"] == "v0.1.0" and result["result"] == "passed", result
PY

# Missing trusted provenance, a VM failure or a changed channel document cannot mint evidence.
if FIXTURE_REJECT_PROVENANCE=1 qualify "${tmp}/no-provenance"; then exit 1; fi
if FIXTURE_REJECT_VM=1 qualify "${tmp}/no-vm"; then exit 1; fi
printf '\n' >>"${FIXTURE_CHANNEL}/stable.json"
if qualify "${tmp}/no-signature"; then exit 1; fi
for case in no-provenance no-vm no-signature; do
    [[ ! -e "${tmp}/${case}/stable-vm.json" ]]
done
echo "Stable qualification tests passed"
