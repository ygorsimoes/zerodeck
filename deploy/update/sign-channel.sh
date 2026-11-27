#!/usr/bin/env bash
# Verify one published loader release and write its signed channel document.
#
#   sign-channel.sh --archive zerodeck-v0.1.0-linux-x86_64.managed.tar.gz --tag v0.1.0 \
#     --channel stable --repository owner/repo --source-sha <40-hex> --sequence 3 \
#     --key signing-key.pem --out channels/
#
# The archive digest, `release.json` identity and GitHub build provenance are checked first; only
# then is `<out>/<channel>.json` written and signed (ed25519, detached hex in `<channel>.json.sig`).
# The private key is only ever read from `--key`; release automation supplies it from a protected
# environment secret.
set -euo pipefail

archive="" tag="" channel="" repository="" source_sha="" sequence="" key="" out=""
min_updater="0.1.0"
expires_days="90"
signer_workflow=""
gh_bin="${GH_BIN:-gh}"
openssl_bin="${OPENSSL:-openssl}"

usage() {
    sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//' >&2
    echo "options: --min-updater <semver> --expires-days <n> --signer-workflow <owner/repo/.github/workflows/f.yml>" >&2
    exit 2
}
die() {
    echo "sign-channel: $*" >&2
    exit 1
}

while [[ $# -gt 0 ]]; do
    [[ $# -ge 2 ]] || usage
    case "$1" in
        --archive) archive="$2" ;;
        --tag) tag="$2" ;;
        --channel) channel="$2" ;;
        --repository) repository="$2" ;;
        --source-sha) source_sha="$2" ;;
        --sequence) sequence="$2" ;;
        --key) key="$2" ;;
        --out) out="$2" ;;
        --min-updater) min_updater="$2" ;;
        --expires-days) expires_days="$2" ;;
        --signer-workflow) signer_workflow="$2" ;;
        *) usage ;;
    esac
    shift 2
done

[[ -n "${archive}" && -n "${tag}" && -n "${channel}" && -n "${repository}" ]] || usage
[[ -n "${source_sha}" && -n "${sequence}" && -n "${key}" && -n "${out}" ]] || usage
[[ "${channel}" == stable || "${channel}" == testing ]] || die "--channel must be stable or testing"
[[ "${tag}" =~ ^v[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$ ]] || die "--tag must be a v<SemVer> tag"
[[ "${source_sha}" =~ ^[0-9a-f]{40}$ ]] || die "--source-sha must be a 40-character commit"
[[ "${sequence}" =~ ^[0-9]+$ && "${expires_days}" =~ ^[0-9]+$ ]] || die "--sequence/--expires-days must be integers"
[[ -f "${archive}" && -f "${key}" ]] || die "archive or key not found"
version="${tag#v}"
if [[ "${channel}" == stable && "${version}" == *-* ]]; then
    die "a prerelease cannot be signed into the stable channel"
fi
signer_workflow="${signer_workflow:-${repository}/.github/workflows/release-please.yml}"
gh_bin="$(command -v -- "${gh_bin}")" || die "gh executable not found"
openssl_bin="$(command -v -- "${openssl_bin}")" || die "openssl executable not found"

sha256="$(python3 - "${archive}" <<'PY'
import hashlib, sys
print(hashlib.sha256(open(sys.argv[1], "rb").read()).hexdigest())
PY
)"
size="$(wc -c <"${archive}" | tr -d ' ')"

# The archive must be the managed generation of exactly this tag.
build_id="$(python3 - "${archive}" "${version}" <<'PY'
import json, sys, tarfile
archive, version = sys.argv[1:3]
with tarfile.open(archive, "r:gz") as handle:
    try:
        document = json.load(handle.extractfile(handle.getmember("release.json")))
    except KeyError:
        raise SystemExit("sign-channel: archive has no release.json at its root")
if document.get("schema") != 1 or document.get("version") != version:
    raise SystemExit(f"sign-channel: release.json is not schema 1 for version {version}")
build_id = document.get("build_id", "")
if not build_id.strip():
    raise SystemExit("sign-channel: release.json has no build_id")
print(build_id)
PY
)"

# Provenance: exact repository, signer workflow, source commit and artifact digest.
"${gh_bin}" attestation verify "${archive}" \
    --repo "${repository}" \
    --signer-workflow "${signer_workflow}" \
    --source-digest "${source_sha}" \
    --deny-self-hosted-runners >/dev/null

mkdir -p "${out}"
document="${out}/${channel}.json"
python3 - "${document}" "${channel}" "${sequence}" "${expires_days}" "${version}" "${sha256}" \
    "${size}" "${build_id}" "${min_updater}" "${tag}" "$(basename "${archive}")" <<'PY'
import json, sys, time
(path, channel, sequence, days, version, digest, size, build_id, min_updater, tag, asset) = sys.argv[1:12]
document = {
    "schema": 1,
    "channel": channel,
    "sequence": int(sequence),
    "expires": int(time.time()) + int(days) * 86400,
    "version": version,
    "digest": digest,
    "size": int(size),
    "build_id": build_id,
    "min_updater": min_updater,
    "target": f"{tag}/{asset}",
}
with open(path, "w", encoding="utf-8") as handle:
    json.dump(document, handle, indent=2)
    handle.write("\n")
PY

# Detached ed25519 signature over the exact document bytes, stored as hex.
"${openssl_bin}" pkeyutl -sign -rawin -inkey "${key}" -in "${document}" -out "${document}.sig.bin"
python3 - "${document}.sig.bin" "${document}.sig" <<'PY'
import os, sys
raw = open(sys.argv[1], "rb").read()
if len(raw) != 64:
    raise SystemExit("sign-channel: expected a 64-byte ed25519 signature")
open(sys.argv[2], "w").write(raw.hex() + "\n")
os.remove(sys.argv[1])
PY
echo "sign-channel: signed ${channel} -> ${tag} (sequence ${sequence}, sha256 ${sha256})"
