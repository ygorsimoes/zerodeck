#!/usr/bin/env bash
# Fail closed unless a filled hardware evidence record authorizes this exact Stable release.
#
# The evidence file lives on the default branch (reviewed by pull request), not in the release tag
# or an uploaded artifact, so a release engineer cannot edit it into a published release.
set -euo pipefail

tag=""
version=""
build_id=""
sha256=""
evidence=""

usage() {
    cat >&2 <<'EOF'
usage: verify-stable-evidence.sh --tag <vX.Y.Z> --version <X.Y.Z> --build-id <id>
                                --sha256 <managed-archive-sha256> --evidence <file.md>

The evidence record must be a completed deploy/update/COMPATIBILITY.md copy with:
  - no remaining PENDING value;
  - the exact release tag, version, build ID and managed SHA-256;
  - an independent reviewer and date;
  - the exact line `Stable eligible: PASS`.
EOF
    exit 2
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --tag) tag="${2:-}"; shift 2 ;;
        --version) version="${2:-}"; shift 2 ;;
        --build-id) build_id="${2:-}"; shift 2 ;;
        --sha256) sha256="${2:-}"; shift 2 ;;
        --evidence) evidence="${2:-}"; shift 2 ;;
        --help|-h) usage ;;
        *) usage ;;
    esac
done
[[ -n "${tag}" && -n "${version}" && -n "${build_id}" && -n "${sha256}" && -n "${evidence}" ]] || usage

[[ "${tag}" =~ ^v[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$ ]] || {
    echo "stable-gate: not a release SemVer tag: ${tag}" >&2
    exit 1
}
[[ "${version}" == "${tag#v}" ]] || {
    echo "stable-gate: version ${version} does not match tag ${tag}" >&2
    exit 1
}
[[ "${sha256}" =~ ^[0-9A-Fa-f]{64}$ ]] || {
    echo "stable-gate: --sha256 must be a SHA-256 digest" >&2
    exit 1
}
[[ -f "${evidence}" ]] || {
    echo "stable-gate: no hardware evidence record exists at ${evidence}" >&2
    echo "stable-gate: Stable stays blocked until a reviewed evidence file is committed" >&2
    exit 1
}

python3 - "${evidence}" "${tag}" "${version}" "${build_id}" "${sha256}" <<'PY'
import re
import sys

path, tag, version, build_id, sha256 = sys.argv[1:6]
text = open(path, encoding="utf-8").read()

def normalized(value):
    return value.replace("**", "").replace("`", "").strip()

def table_value(label):
    pattern = re.compile(
        r"^\|\s*" + re.escape(label) + r"\s*\|\s*(.*?)\s*\|\s*$",
        re.MULTILINE,
    )
    match = pattern.search(text)
    return normalized(match.group(1)) if match else ""


def field_value(label):
    pattern = re.compile(
        r"^\s*(?:[-*]\s*)?" + re.escape(label) + r":\s*(.*?)\s*$",
        re.MULTILINE,
    )
    match = pattern.search(text)
    return normalized(match.group(1)) if match else ""

failures = []
if re.search(r"\bPENDING\b", text):
    failures.append("the evidence record still contains PENDING values")
eligible_pattern = r"^\s*(?:[-*]\s*)?Stable eligible:\s*PASS\s*$"
if not re.search(eligible_pattern, text, re.MULTILINE):
    failures.append("the exact line `Stable eligible: PASS` is missing")

if table_value("Release tag") != tag:
    failures.append(f"`Release tag` does not record {tag}")
identity = [part.strip() for part in table_value("Loader version / build ID").split("/", 1)]
if identity != [version, build_id]:
    failures.append(f"`Loader version / build ID` does not record {version} / {build_id}")
digest = table_value("Managed asset SHA-256 (generation identity)")
if sha256.lower() not in digest.lower():
    failures.append("`Managed asset SHA-256 (generation identity)` does not record the release digest")
if field_value("Operator") in ("", "PENDING"):
    failures.append("no operator is recorded")
if field_value("Independent reviewer") in ("", "PENDING"):
    failures.append("no independent reviewer is recorded")
if field_value("Date") in ("", "PENDING"):
    failures.append("no sign-off date is recorded")

if failures:
    for failure in failures:
        print(f"stable-gate: {failure}", file=sys.stderr)
    raise SystemExit(1)
print(f"stable-gate: {tag} ({version}, {build_id}) has a complete PASS evidence record")
PY
