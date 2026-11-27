#!/usr/bin/env bash
# Exercise the public release.sh path with a synthetic but complete pair of assets and no
# ZERODECK_EXPECTED_* environment. This is what catches archive-derived identity regressions in
# `just check`/`just ci` without building a runtime image.
set -euo pipefail

tests_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${tests_dir}/../../.." && pwd)"
export FIXTURE_REPO_ROOT="${repo_root}"
# shellcheck source=fixtures.sh
source "${tests_dir}/fixtures.sh"

tmp="$(mktemp -d "${TMPDIR:-/tmp}/zerodeck-release-fixture.XXXXXX")"
trap 'rm -rf "${tmp}"' EXIT
assets="${tmp}/assets"
mkdir -p "${assets}"
fixture_release_assets "${assets}" "0.1.0" "fixture-release-build" 1 >/dev/null
manual="${assets}/zerodeck-v0.1.0-linux-x86_64.tar.gz"
managed="${assets}/zerodeck-v0.1.0-linux-x86_64.managed.tar.gz"
python3 - "${assets}" "$(basename "${manual}")" "$(basename "${managed}")" <<'PY'
import hashlib
import sys
from pathlib import Path
root = Path(sys.argv[1])
lines = []
for name in sys.argv[2:]:
    lines.append(f"{hashlib.sha256((root / name).read_bytes()).hexdigest()}  {name}\n")
(root / "SHA256SUMS.txt").write_text("".join(lines), encoding="utf-8")
PY

unset ZERODECK_EXPECTED_VERSION ZERODECK_EXPECTED_BUILD_ID
ZERODECK_RELEASE_SKIP_SIMULATIONS=1 bash "${tests_dir}/release.sh" "${manual}"
ZERODECK_RELEASE_SKIP_SIMULATIONS=1 bash "${tests_dir}/release.sh" "${managed}"

# Explicit overrides still win, including the build-ID mismatch refusal.
ZERODECK_EXPECTED_VERSION=0.1.0 ZERODECK_EXPECTED_BUILD_ID=fixture-release-build \
    ZERODECK_RELEASE_SKIP_SIMULATIONS=1 bash "${tests_dir}/release.sh" "${managed}"
if ZERODECK_EXPECTED_BUILD_ID=not-the-build ZERODECK_RELEASE_SKIP_SIMULATIONS=1 \
    bash "${tests_dir}/release.sh" "${managed}" >/dev/null 2>&1; then
    echo "error: explicit build-ID override mismatch was accepted" >&2
    exit 1
fi

echo "release fixture test passed"
