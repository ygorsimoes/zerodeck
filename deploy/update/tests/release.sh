#!/usr/bin/env bash
# Validate the exact release archive passed to publication and exercise the packaging failure modes.
#
#   bash deploy/update/tests/release.sh dist/zerodeck-v0.1.0-linux-x86_64.tar.gz
#   ZERODECK_EXPECTED_VERSION=0.1.0 ZERODECK_EXPECTED_BUILD_ID=v0.1.0-abc \
#     bash deploy/update/tests/release.sh dist/zerodeck-v0.1.0-linux-x86_64.managed.tar.gz
#
# `--validate-only` validates one archive and skips the mutation/publish/promotion simulations; the
# promotion script uses that mode after it has independently verified the published source bytes.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
tests_dir="${repo_root}/deploy/update/tests"
python="${PYTHON:-python3}"

usage() {
    echo "usage: $0 [--validate-only] <archive>" >&2
    exit 2
}

validate_only=0
if [[ "${1:-}" == "--validate-only" ]]; then
    validate_only=1
    shift
fi
[[ $# -eq 1 ]] || usage
archive="$1"
[[ -f "${archive}" ]] || { echo "release archive not found: ${archive}" >&2; exit 1; }
archive="$(cd "$(dirname "${archive}")" && pwd)/$(basename "${archive}")"
name="$(basename "${archive}")"

case "${name}" in
    *.managed.tar.gz) mode=managed ;;
    *.tar.gz) mode=manual ;;
    *) echo "unsupported release asset name: ${name}" >&2; exit 1 ;;
esac

tmp="$(mktemp -d "${TMPDIR:-/tmp}/zerodeck-release-test.XXXXXX")"
trap 'rm -rf "${tmp}"' EXIT

write_checksums() {
    "${python}" - "$1" <<'PY'
import hashlib
import sys
from pathlib import Path

archive = Path(sys.argv[1])
digest = hashlib.sha256(archive.read_bytes()).hexdigest()
(archive.parent / "SHA256SUMS.txt").write_text(f"{digest}  {archive.name}\n", encoding="utf-8")
PY
}

validate() {
    "${python}" "${tests_dir}/archive.py" validate "$@"
}

identity_output="$("${python}" "${tests_dir}/archive.py" identity "${archive}" "${mode}")" || {
    echo "release archive identity is unreadable: ${archive}" >&2
    exit 1
}
expected_version="$(printf '%s\n' "${identity_output}" | sed -n '1p')"
expected_build_id="$(printf '%s\n' "${identity_output}" | sed -n '2p')"
expected_version="${ZERODECK_EXPECTED_VERSION:-${expected_version}}"
expected_build_id="${ZERODECK_EXPECTED_BUILD_ID:-${expected_build_id}}"
export ZERODECK_EXPECTED_VERSION="${expected_version}"
export ZERODECK_EXPECTED_BUILD_ID="${expected_build_id}"

# The exact bytes that would be published (or were published) must match their checksum manifest.
validate "${archive}" "${mode}"

if [[ "${validate_only}" -eq 1 ]]; then
    exit 0
fi

# ── Deliberately broken archives must fail closed ───────────────────────────────────────────────
negative_dir="${tmp}/negative"
mkdir -p "${negative_dir}"
case "${mode}" in
    managed) mutations=(missing-release missing-component mixed-wrapper build-id-mismatch) ;;
    manual) mutations=(missing-release missing-controller missing-component build-id-mismatch) ;;
esac

for mutation in "${mutations[@]}"; do
    destination="${negative_dir}/${mutation}/${name}"
    mkdir -p "$(dirname "${destination}")"
    "${python}" "${tests_dir}/archive.py" mutate "${archive}" "${destination}" "${mutation}" "${mode}"
    write_checksums "${destination}"
    if validate "${destination}" "${mode}" >"${tmp}/${mutation}.log" 2>&1; then
        echo "error: ${mutation} archive unexpectedly passed validation" >&2
        exit 1
    fi
    printf 'release archive rejected (%s): %s\n' "${mutation}" "$(tail -n 1 "${tmp}/${mutation}.log")"
done

# ── Publication and TUF promotion boundaries ────────────────────────────────────────────────────
if [[ "${ZERODECK_RELEASE_SKIP_SIMULATIONS:-0}" != "1" ]]; then
    bash "${tests_dir}/publish.sh"
    bash "${tests_dir}/promotion.sh"
fi

echo "release package test passed: ${name} (${mode}, version ${expected_version}, build ${expected_build_id})"
