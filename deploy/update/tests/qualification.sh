#!/usr/bin/env bash
# Stable qualification: baseline selection comes from immutable release history and attested VM
# evidence, never from the mutable channels branch. A fixture is only allowed for a first release.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export FIXTURE_REPO_ROOT="$(cd "${here}/../../.." && pwd)"
# shellcheck source=fixtures.sh
source "${here}/fixtures.sh"

tmp="$(mktemp -d "${TMPDIR:-/tmp}/zerodeck-qualification.XXXXXX")"
trap 'rm -rf "${tmp}"' EXIT
export FIXTURE_PACKAGES="${tmp}/packages"
export FIXTURE_RELEASES="${tmp}/releases.json"
mkdir -p "${FIXTURE_PACKAGES}" "${tmp}/scripts/tests"
cp "${here}/qualify-stable.sh" "${tmp}/scripts/tests/"
fixture_release_assets "${FIXTURE_PACKAGES}" 0.1.0 fixture-old 1 >/dev/null
fixture_release_assets "${FIXTURE_PACKAGES}" 0.1.1 fixture-middle 1 >/dev/null
fixture_release_assets "${FIXTURE_PACKAGES}" 0.1.2 fixture-new 1 >/dev/null
export FIXTURE_NEW_SHA
FIXTURE_NEW_SHA="$(fixture_sha256 "${FIXTURE_PACKAGES}/zerodeck-v0.1.2-linux-x86_64.managed.tar.gz")"
export FIXTURE_QUALIFIED_BASELINE="v0.1.0"
export FIXTURE_EXPECTED_BASELINE="v0.1.0"

cat >"${tmp}/gh" <<'SCRIPT'
#!/usr/bin/env bash
set -euo pipefail
case "$1/$2" in
  api/repos/fixture/repo/releases\?per_page=100)
    cat "${FIXTURE_RELEASES}"
    ;;
  api/repos/fixture/repo/releases/tags/*)
    tag="${2##*/}"
    printf '{"tag_name":"%s","draft":false,"prerelease":false,"immutable":true}\n' "${tag}"
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
      [[ " ${FIXTURE_QUALIFIED_BASELINE:-} " == *" ${tag} "* ]]
    else
      [[ "${FIXTURE_REJECT_PROVENANCE:-0}" == 0 ]]
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

cat >"${tmp}/scripts/tests/build-candidate.sh" <<'SCRIPT'
#!/usr/bin/env bash
set -euo pipefail
version="$1"
out="$2"
mkdir -p "${out}"
touch "${out}/zerodeck-v${version}-linux-x86_64.tar.gz"
touch "${out}/zerodeck-v${version}-linux-x86_64.managed.tar.gz"
SCRIPT

cat >"${tmp}/scripts/tests/vm-ci.sh" <<'SCRIPT'
#!/usr/bin/env bash
set -euo pipefail
baseline_dir="$1"
baseline_tag="$2"
candidate="$3"
candidate_sha="$4"
[[ "${baseline_tag}" == "${FIXTURE_EXPECTED_BASELINE}" ]]
[[ "${candidate_sha}" == "${FIXTURE_NEW_SHA}" ]]
[[ -f "${baseline_dir}/zerodeck-${baseline_tag}-linux-x86_64.managed.tar.gz" ]]
printf '%s  %s\n' "${FIXTURE_NEW_SHA}" "${candidate}" | sha256sum --check --status
[[ "${FIXTURE_REJECT_VM:-0}" == 0 ]]
SCRIPT

releases() {
    cat >"${FIXTURE_RELEASES}" <<JSON
[
  {"tag_name": "v9.9.9", "draft": false, "prerelease": false, "immutable": false},
  {"tag_name": "v0.1.2", "draft": false, "prerelease": false, "immutable": true},
  {"tag_name": "v0.1.1", "draft": false, "prerelease": false, "immutable": true},
  {"tag_name": "v0.1.0", "draft": false, "prerelease": false, "immutable": true}
]
JSON
}

qualify() {
    bash "${tmp}/scripts/tests/qualify-stable.sh" fixture/repo "$1" "$2"
}

releases
qualify v0.1.2 "${tmp}/ok"
python3 - "${tmp}/ok/stable-vm.json" <<'PY'
import json, os, pathlib, sys
result = json.loads(pathlib.Path(sys.argv[1]).read_text())
assert result["sourceCommit"] == "a" * 40, result
assert result["archiveSha256"] == os.environ["FIXTURE_NEW_SHA"], result
assert result["baselineTag"] == "v0.1.0", result
assert result["baselineKind"] == "release" and result["result"] == "passed", result
PY

# A first release uses a lower-version fixture only when no immutable stable release exists.
cat >"${FIXTURE_RELEASES}" <<'JSON'
[
  {"tag_name": "v0.1.0", "draft": false, "prerelease": false, "immutable": true}
]
JSON
FIXTURE_EXPECTED_BASELINE="v0.0.0" \
    FIXTURE_NEW_SHA="$(fixture_sha256 "${FIXTURE_PACKAGES}/zerodeck-v0.1.0-linux-x86_64.managed.tar.gz")" \
    qualify v0.1.0 "${tmp}/first"
python3 - "${tmp}/first/stable-vm.json" <<'PY'
import json, pathlib, sys
result = json.loads(pathlib.Path(sys.argv[1]).read_text())
assert result["baselineTag"] == "v0.0.0" and result["baselineKind"] == "fixture", result
PY

# No qualified previous Stable, provenance failure, or VM failure cannot mint evidence.
releases
if FIXTURE_QUALIFIED_BASELINE="" qualify v0.1.2 "${tmp}/none"; then exit 1; fi
if FIXTURE_REJECT_PROVENANCE=1 qualify v0.1.2 "${tmp}/no-provenance"; then exit 1; fi
if FIXTURE_REJECT_VM=1 qualify v0.1.2 "${tmp}/no-vm"; then exit 1; fi
for case in none no-provenance no-vm; do
    [[ ! -e "${tmp}/${case}/stable-vm.json" ]]
done
echo "Stable qualification tests passed"
