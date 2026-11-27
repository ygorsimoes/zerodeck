#!/usr/bin/env bash
# Pure-dispatch promotion boundary: verify published bytes and provenance, then render the exact
# target descriptor. Signed TUF metadata is validated by the external TUF-on-CI repository, so this
# test intentionally contains no fake signatures or fake signed metadata.
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
source_sha="$(printf 'a%.0s' {1..40})"
assets="${tmp}/assets"
mkdir -p "${assets}"
fixture_release_assets "${assets}" "${version}" "${build_id}" 1 >/dev/null
managed="${assets}/zerodeck-${tag}-linux-x86_64.managed.tar.gz"
managed_sha="$(fixture_sha256 "${managed}")"

fake_gh="${tmp}/gh"
cat >"${fake_gh}" <<'SCRIPT'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"${FAKE_GH_LOG}"
[[ "${FAKE_GH_ATTEST_FAIL:-0}" == "1" ]] && exit 1
exit 0
SCRIPT
chmod 0755 "${fake_gh}"
export FAKE_GH_LOG="${tmp}/gh.log"
: >"${FAKE_GH_LOG}"

promote() {
    bash "${repo_root}/deploy/update/promote.sh" \
        --archive "${managed}" --sha256 "${managed_sha}" --tag "${tag}" \
        --version "${version}" --build-id "${build_id}" --min-updater 0.1.0 \
        --repository fixture/repo --source-sha "${source_sha}" \
        --signer-workflow fixture/repo/.github/workflows/release-please.yml \
        --gh "${fake_gh}" "$@"
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

promote >"${tmp}/verify.log"
grep -q "target: linux-x86_64/zerodeck-${version}.tar.gz" "${tmp}/verify.log"
grep -q "sha256: ${managed_sha}" "${tmp}/verify.log"
grep -q "source_sha: ${source_sha}" "${tmp}/verify.log"
grep -q -- "--repo fixture/repo" "${FAKE_GH_LOG}"
grep -q -- "--signer-workflow fixture/repo/.github/workflows/release-please.yml" "${FAKE_GH_LOG}"
grep -q -- "--source-digest ${source_sha}" "${FAKE_GH_LOG}"
grep -q -- "--source-ref refs/heads/main" "${FAKE_GH_LOG}"
grep -q -- "--deny-self-hosted-runners" "${FAKE_GH_LOG}"

# Provenance failure is fatal and produces no descriptor.
: >"${FAKE_GH_LOG}"
FAKE_GH_ATTEST_FAIL=1
export FAKE_GH_ATTEST_FAIL
expect_failure attestation-failure promote
unset FAKE_GH_ATTEST_FAIL
if grep -q '^target:' "${tmp}/attestation-failure.log"; then
    echo "error: failed attestation still emitted a descriptor" >&2
    exit 1
fi

# Digest, identity, channel and descriptor integrity are checked before dispatch.
expect_failure wrong-digest promote --sha256 "$(printf '0%.0s' {1..64})"
expect_failure wrong-build-id promote --build-id not-the-build
expect_failure stable-prerelease promote --version "0.2.0-beta.1" --target "linux-x86_64/zerodeck-0.2.0-beta.1.tar.gz"
expect_failure wrong-source-sha promote --source-sha not-a-commit
expect_failure wrong-signer promote --signer-workflow release-please.yml
expect_failure metadata-local-acceptance promote --metadata-dir "${tmp}/metadata" --output "${tmp}/repo"
[[ ! -e "${tmp}/repo" ]] || { echo "error: local metadata acceptance created a repository" >&2; exit 1; }

# The archive bytes must be the exact published managed asset.
cp "${managed}" "${tmp}/other.tar.gz"
printf 'changed bytes\n' >>"${tmp}/other.tar.gz"
expect_failure changed-archive bash "${repo_root}/deploy/update/promote.sh" \
    --archive "${tmp}/other.tar.gz" --sha256 "${managed_sha}" --tag "${tag}" \
    --version "${version}" --build-id "${build_id}" --min-updater 0.1.0 \
    --repository fixture/repo --source-sha "${source_sha}" \
    --signer-workflow fixture/repo/.github/workflows/release-please.yml \
    --gh "${fake_gh}"

echo "promotion tests passed"
