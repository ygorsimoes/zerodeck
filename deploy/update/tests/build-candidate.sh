#!/usr/bin/env bash
# Build a managed generation archive for a *newer* version, for the VM update scenarios.
#
#   build-candidate.sh <version> <output-dir>
#
# The updater rightly refuses a different digest under an installed version, and the daemon refuses
# a generation whose manifest differs from its compiled version, so a real update test needs a
# second build. This copies the committed tree to a scratch directory, bumps only the workspace
# version there (never in the working tree), builds the runtime image from it and packages the
# managed archive. Needs docker and git.
set -euo pipefail

[[ $# -eq 2 && "$1" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
    echo "usage: build-candidate.sh <x.y.z> <output-dir>" >&2
    exit 2
}
version="$1"
out="$(mkdir -p "$2" && cd "$2" && pwd)"
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
scratch="$(mktemp -d "${TMPDIR:-/tmp}/zerodeck-candidate.XXXXXX")"
trap 'rm -rf "${scratch}"' EXIT

git -C "${repo_root}" archive HEAD | tar -x -C "${scratch}"
python3 - "${scratch}" "${version}" <<'PY'
import re, sys
root, version = sys.argv[1:3]
path = f"{root}/Cargo.toml"
text = open(path).read()
text, count = re.subn(r'^version = "[^"]+"( # x-release-please-version)', f'version = "{version}"\\1', text, count=1, flags=re.M)
if count != 1:
    raise SystemExit("candidate: cannot find the workspace version line")
open(path, "w").write(text)
# Cargo.lock lists every workspace crate with the workspace version.
path = f"{root}/Cargo.lock"
lock = open(path).read()
lock = re.sub(r'(name = "zerodeck[^"]*"\nversion = )"[^"]+"', rf'\1"{version}"', lock)
open(path, "w").write(lock)
PY

image="zerodeck:candidate-${version}"
build_id="candidate-${version}"
docker build --target runtime --tag "${image}" \
    --build-arg "ZERODECK_BUILD_ID=${build_id}" --build-arg ZERODECK_REQUIRE_BUILD_ID=1 "${scratch}"
ZERODECK_BUILD_ID="${build_id}" bash "${scratch}/deploy/update/package-release.sh" \
    managed "v${version}" "${image}" "${out}"
echo "candidate archive: ${out}/zerodeck-v${version}-linux-x86_64.managed.tar.gz"
