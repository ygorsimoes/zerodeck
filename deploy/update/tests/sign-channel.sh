#!/usr/bin/env bash
# Channel signing: provenance and identity checks run before anything is signed, and the written
# document verifies with the matching public key.
set -euo pipefail

tests_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${tests_dir}/../../.." && pwd)"
export FIXTURE_REPO_ROOT="${repo_root}"
# shellcheck source=fixtures.sh
source "${tests_dir}/fixtures.sh"

tmp="$(mktemp -d "${TMPDIR:-/tmp}/zerodeck-sign-test.XXXXXX")"
trap 'rm -rf "${tmp}"' EXIT

version="0.1.0"
build_id="fixture-build-v0.1.0-deadbeef"
tag="v${version}"
source_sha="$(printf 'a%.0s' {1..40})"
assets="${tmp}/assets"
mkdir -p "${assets}"
fixture_release_assets "${assets}" "${version}" "${build_id}" 1 >/dev/null
managed="${assets}/zerodeck-${tag}-linux-x86_64.managed.tar.gz"
managed_sha="$(fixture_sha256 "${managed}")"

key="${tmp}/key.pem"
openssl genpkey -algorithm ed25519 -out "${key}" 2>/dev/null
openssl pkey -in "${key}" -pubout -out "${tmp}/pub.pem" 2>/dev/null

fake_gh="${tmp}/gh"
cat >"${fake_gh}" <<'SCRIPT'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"${FAKE_GH_LOG}"
[[ "${FAKE_GH_ATTEST_FAIL:-0}" == "1" ]] && exit 1
exit 0
SCRIPT
chmod 0755 "${fake_gh}"
export FAKE_GH_LOG="${tmp}/gh.log"
: >"${FAKE_GH_LOG}"

expect_failure() {
    local label="$1"
    shift
    if "$@" >"${tmp}/${label}.log" 2>&1; then
        echo "error: ${label} unexpectedly succeeded" >&2
        cat "${tmp}/${label}.log" >&2
        exit 1
    fi
}

export GH_BIN="${fake_gh}"

sign_ok() {
    bash "${repo_root}/deploy/update/sign-channel.sh" \
        --archive "${managed}" --tag "${tag}" --channel "${1:-stable}" --repository fixture/repo \
        --source-sha "${source_sha}" --sequence 7 --key "${key}" --out "${2:-${tmp}/out}"
}

sign_ok stable "${tmp}/out" >"${tmp}/ok.log"
document="${tmp}/out/stable.json"
python3 - "${document}" "${managed_sha}" "${build_id}" "${tag}" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
assert d["schema"] == 1 and d["channel"] == "stable" and d["sequence"] == 7, d
assert d["version"] == "0.1.0" and d["digest"] == sys.argv[2] and d["build_id"] == sys.argv[3], d
assert d["target"] == f"{sys.argv[4]}/zerodeck-{sys.argv[4]}-linux-x86_64.managed.tar.gz", d
assert d["expires"] > 0 and d["min_updater"] == "0.1.0", d
PY
python3 -c "import sys; open(sys.argv[2],'wb').write(bytes.fromhex(open(sys.argv[1]).read().strip()))" \
    "${document}.sig" "${tmp}/sig.bin"
openssl pkeyutl -verify -rawin -pubin -inkey "${tmp}/pub.pem" -in "${document}" -sigfile "${tmp}/sig.bin" >/dev/null
grep -q "attestation verify .*--repo fixture/repo .*--source-digest ${source_sha}" "${FAKE_GH_LOG}"
grep -q -- "--signer-workflow fixture/repo/.github/workflows/release-please.yml" "${FAKE_GH_LOG}"

# A failed attestation signs nothing.
rm -rf "${tmp}/out2"
FAKE_GH_ATTEST_FAIL=1 expect_failure attest sign_ok stable "${tmp}/out2"
[[ ! -e "${tmp}/out2/stable.json" ]]

# A prerelease never enters Stable; a tampered or mismatched archive never enters any channel.
prerelease_tag="v0.1.0-rc.1"
expect_failure prerelease bash "${repo_root}/deploy/update/sign-channel.sh" \
    --archive "${managed}" --tag "${prerelease_tag}" --channel stable --repository fixture/repo \
    --source-sha "${source_sha}" --sequence 1 --key "${key}" --out "${tmp}/out3"
expect_failure wrong-version bash "${repo_root}/deploy/update/sign-channel.sh" \
    --archive "${managed}" --tag "v9.9.9" --channel testing --repository fixture/repo \
    --source-sha "${source_sha}" --sequence 1 --key "${key}" --out "${tmp}/out4"
[[ ! -e "${tmp}/out3/stable.json" && ! -e "${tmp}/out4/testing.json" ]]
expect_failure bad-sha bash "${repo_root}/deploy/update/sign-channel.sh" \
    --archive "${managed}" --tag "${tag}" --channel stable --repository fixture/repo \
    --source-sha "short" --sequence 1 --key "${key}" --out "${tmp}/out5"
echo "sign-channel tests passed"
