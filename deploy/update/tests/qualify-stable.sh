#!/usr/bin/env bash
# Qualify the exact, already published managed archive before it can enter Stable.
set -euo pipefail

[[ $# -eq 3 ]] || { echo "usage: qualify-stable.sh <repo> <tag> <output-dir>" >&2; exit 2; }
repo="$1"
tag="$2"
out="$3"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
gh_bin="$(command -v -- "${GH_BIN:-gh}")"
tag_pattern='^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(\+[0-9A-Za-z.-]+)?$'
[[ "${tag}" =~ ${tag_pattern} ]] && [[ "${tag}" != v0.0.0 ]] || {
    echo "qualify-stable: expected a stable version with a possible predecessor" >&2
    exit 2
}

download_verified() {
    local version_tag="$1" dest="$2"
    shift 2
    local release source_sha pattern asset
    release="$("${gh_bin}" api "repos/${repo}/releases/tags/${version_tag}")" || return
    python3 - "${version_tag}" "${release}" <<'PY' || return
import json
import sys

tag, raw = sys.argv[1:]
release = json.loads(raw)
if (
    release.get("tag_name") != tag
    or release.get("draft")
    or release.get("prerelease")
    or release.get("immutable") is not True
):
    raise SystemExit(f"qualify-stable: {tag} is not a published immutable stable release")
PY
    source_sha="$("${gh_bin}" api "repos/${repo}/commits/${version_tag}" --jq .sha)" || return
    [[ "${source_sha}" =~ ^[0-9a-f]{40}$ ]] || return 1
    mkdir -p "${dest}" || return
    for pattern in "$@"; do
        asset="zerodeck-${version_tag}-linux-x86_64${pattern}.tar.gz"
        "${gh_bin}" release download "${version_tag}" --repo "${repo}" \
            --pattern "${asset}" --dir "${dest}" >&2 || return
        "${gh_bin}" attestation verify "${dest}/${asset}" \
            --repo "${repo}" --signer-workflow "${repo}/.github/workflows/release-please.yml" \
            --source-digest "${source_sha}" --source-ref refs/heads/main \
            --deny-self-hosted-runners >/dev/null || return
    done
    printf '%s\n' "${source_sha}"
}

candidate_dir="${out}/candidate"
source_sha="$(download_verified "${tag}" "${candidate_dir}" .managed)"
candidate="${candidate_dir}/zerodeck-${tag}-linux-x86_64.managed.tar.gz"
digest="$(sha256sum "${candidate}" | cut -d' ' -f1)"

baseline_dir="${out}/baseline"
baseline_kind="release"
baseline=""
channels_commit="$("${gh_bin}" api "repos/${repo}/git/matching-refs/heads/channels" \
    --jq '.[] | select(.ref == "refs/heads/channels") | .object.sha')"
if [[ -n "${channels_commit}" ]]; then
    [[ "${channels_commit}" =~ ^[0-9a-f]{40}$ ]] || exit 1
    entries="$("${gh_bin}" api "repos/${repo}/contents?ref=${channels_commit}" \
        --jq '.[] | select(.name == "stable.json" or .name == "stable.json.sig") | .name')"
    if [[ -n "${entries}" ]]; then
        mkdir -p "${baseline_dir}"
        for file in stable.json stable.json.sig; do
            "${gh_bin}" api "repos/${repo}/contents/${file}?ref=${channels_commit}" \
                -H 'Accept: application/vnd.github.raw+json' >"${baseline_dir}/${file}"
        done
        baseline="$(python3 - "${tag}" "${tag_pattern}" "${baseline_dir}" \
            "${here}/../trusted-root.json" <<'PY'
import json
from pathlib import Path
import re
import subprocess
import sys

tag, pattern, directory, trust = sys.argv[1:]
root = Path(directory)
document = root / "stable.json"
signature = root / "stable.sig"
signature.write_bytes(bytes.fromhex((root / "stable.json.sig").read_text()))
pubkey = root / "stable-key.der"
for key in json.loads(Path(trust).read_text())["keys"]:
    # SubjectPublicKeyInfo prefix for a raw Ed25519 public key.
    pubkey.write_bytes(bytes.fromhex("302a300506032b6570032100" + key))
    verified = subprocess.run(
        [
            "openssl", "pkeyutl", "-verify", "-rawin", "-pubin", "-keyform", "DER",
            "-inkey", pubkey, "-in", document, "-sigfile", signature,
        ],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False,
    )
    if verified.returncode == 0:
        break
else:
    raise SystemExit("qualify-stable: cannot authenticate previous Stable channel")

def version(value):
    match = re.fullmatch(pattern, value)
    return tuple(map(int, match.groups()[:3])) if match else ()

data = json.loads(document.read_text())
previous = data["target"].split("/", 1)[0]
digest = data["digest"]
if (
    data.get("schema") != 1 or data.get("channel") != "stable"
    or not version(previous) or not re.fullmatch(r"[0-9a-f]{64}", digest)
    or data["target"] != f"{previous}/zerodeck-{previous}-linux-x86_64.managed.tar.gz"
):
    raise SystemExit("qualify-stable: invalid previous Stable channel document")
if version(previous) > version(tag):
    raise SystemExit("qualify-stable: candidate version is older than Stable")
if version(previous) < version(tag):
    print(previous, digest)
PY
        )"
    fi
fi

if [[ -n "${baseline}" ]]; then
    read -r baseline_tag baseline_digest <<<"${baseline}"
    download_verified "${baseline_tag}" "${baseline_dir}" "" .managed >/dev/null || exit 1
    printf '%s  %s\n' "${baseline_digest}" \
        "${baseline_dir}/zerodeck-${baseline_tag}-linux-x86_64.managed.tar.gz" \
        | sha256sum --check --status
else
    baseline_tag="v0.0.0"
    baseline_kind="fixture"
    bash "${here}/build-candidate.sh" "${baseline_tag#v}" "${baseline_dir}" all
fi

bash "${here}/vm-ci.sh" "${baseline_dir}" "${baseline_tag}" "${candidate}" "${digest}"
printf '%s  %s\n' "${digest}" "${candidate}" | sha256sum --check --status
python3 - "${out}/stable-vm.json" "${tag}" "${source_sha}" "${digest}" \
    "${baseline_tag}" "${baseline_kind}" <<'PY'
import json
import sys

path, tag, source, sha256, baseline, kind = sys.argv[1:]
with open(path, "w", encoding="utf-8") as output:
    json.dump({
        "tag": tag,
        "sourceCommit": source,
        "archiveSha256": sha256,
        "baselineTag": baseline,
        "baselineKind": kind,
        "result": "passed",
    }, output)
PY
