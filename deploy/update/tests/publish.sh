#!/usr/bin/env bash
# Publication retry, missing-asset and immutable-published-release tests using a fake `gh`.
set -euo pipefail

tests_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${tests_dir}/../../.." && pwd)"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/zerodeck-publish-test.XXXXXX")"
trap 'if [[ "${ZERODECK_KEEP_TMP:-0}" == "1" ]]; then echo "kept test dir: ${tmp}" >&2; else rm -rf "${tmp}"; fi' EXIT

export FAKE_GH_STATE="${tmp}/gh-state"
mkdir -p "${FAKE_GH_STATE}/assets/v1.2.3"
export FAKE_GH_LOG="${tmp}/gh.log"
: >"${FAKE_GH_LOG}"

fake_gh="${tmp}/gh"
cat >"${fake_gh}" <<'SCRIPT'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"${FAKE_GH_LOG}"
state="${FAKE_GH_STATE}"
release_meta="${state}/release.meta"
case "${1:-}" in
    api)
        [[ "${2:-}" == repos/*/releases/tags/* ]] || exit 2
        [[ -f "${release_meta}" ]] || exit 1
        draft="$(sed -n '1p' "${release_meta}")"
        id="$(sed -n '2p' "${release_meta}")"
        tag="$(sed -n '3p' "${release_meta}")"
        python3 - "${state}/assets/${tag}" "${draft}" "${id}" "${tag}" <<'PY'
import json, os, sys
directory, draft, release_id, tag = sys.argv[1:5]
assets = []
if os.path.isdir(directory):
    for index, name in enumerate(sorted(os.listdir(directory)), start=1):
        assets.append({"name": name, "id": index})
print(json.dumps({"draft": draft == "1", "id": int(release_id), "tag_name": tag, "assets": assets}))
PY
        ;;
    release)
        sub="${2:-}"
        tag="${3:-}"
        case "${sub}" in
            download)
                shift 3
                pattern=""
                directory=""
                while [[ $# -gt 0 ]]; do
                    case "$1" in
                        --repo) shift 2 ;;
                        --pattern) pattern="$2"; shift 2 ;;
                        --dir) directory="$2"; shift 2 ;;
                        --clobber) shift ;;
                        *) exit 2 ;;
                    esac
                done
                [[ -f "${state}/assets/${tag}/${pattern}" ]] || exit 1
                mkdir -p "${directory}"
                cp "${state}/assets/${tag}/${pattern}" "${directory}/${pattern}"
                ;;
            upload)
                shift 3
                file=""
                while [[ $# -gt 0 ]]; do
                    case "$1" in
                        --repo) shift 2 ;;
                        --clobber) shift ;;
                        *) file="$1"; shift ;;
                    esac
                done
                [[ -n "${file}" && -f "${file}" ]] || exit 1
                if [[ "${FAKE_GH_FAIL_UPLOAD_ONCE:-0}" == "1" && ! -e "${state}/failed-once" ]]; then
                    : >"${state}/failed-once"
                    exit 1
                fi
                name="$(basename "${file}")"
                mkdir -p "${state}/assets/${tag}"
                cp "${file}" "${state}/assets/${tag}/${name}"
                ;;
            edit)
                shift 3
                draft=1
                while [[ $# -gt 0 ]]; do
                    case "$1" in
                        --repo) shift 2 ;;
                        --draft=false) draft=0; shift ;;
                        *) exit 2 ;;
                    esac
                done
                printf '%s\n%s\n%s\n' "${draft}" "$(sed -n '2p' "${release_meta}")" "${tag}" >"${release_meta}"
                ;;
            *) exit 2 ;;
        esac
        ;;
    *) exit 2 ;;
esac
SCRIPT
chmod 0755 "${fake_gh}"
printf '1\n42\nv1.2.3\n' >"${FAKE_GH_STATE}/release.meta"

dist="${tmp}/dist"
mkdir -p "${dist}"
manual="zerodeck-v1.2.3-linux-x86_64.tar.gz"
managed="zerodeck-v1.2.3-linux-x86_64.managed.tar.gz"
printf 'manual archive fixture\n' >"${dist}/${manual}"
printf 'managed archive fixture\n' >"${dist}/${managed}"
python3 - "${dist}" "${manual}" "${managed}" <<'PY'
import hashlib, sys
from pathlib import Path
dist = Path(sys.argv[1])
lines = []
for name in sys.argv[2:]:
    lines.append(f"{hashlib.sha256((dist/name).read_bytes()).hexdigest()}  {name}\n")
(dist / "SHA256SUMS.txt").write_text("".join(lines), encoding="utf-8")
PY

publish() {
    bash "${repo_root}/deploy/update/publish-release.sh" \
        --repo fixture/repo --tag v1.2.3 --dist "${dist}" --gh "${fake_gh}" "$@"
}

# Missing managed asset: refuse before uploading or publishing.
mkdir -p "${tmp}/missing"
cp "${dist}/${manual}" "${tmp}/missing/"
cp "${dist}/SHA256SUMS.txt" "${tmp}/missing/"
if publish --dist "${tmp}/missing" >"${tmp}/missing.log" 2>&1; then
    echo "error: missing managed asset was published" >&2
    exit 1
fi
grep -q "missing release asset" "${tmp}/missing.log" || {
    echo "error: missing asset did not fail with the expected diagnostics" >&2
    cat "${tmp}/missing.log" >&2
    exit 1
}
[[ "$(sed -n '1p' "${FAKE_GH_STATE}/release.meta")" == "1" ]] || {
    echo "error: missing asset published the draft" >&2; exit 1;
}

# A failed upload leaves the draft unpublished and is retryable against the same draft.
FAKE_GH_FAIL_UPLOAD_ONCE=1
export FAKE_GH_FAIL_UPLOAD_ONCE
if publish >"${tmp}/first-upload.log" 2>&1; then
    echo "error: injected upload failure unexpectedly succeeded" >&2
    exit 1
fi
[[ "$(sed -n '1p' "${FAKE_GH_STATE}/release.meta")" == "1" ]] || {
    echo "error: failed upload published the release" >&2; exit 1;
}
unset FAKE_GH_FAIL_UPLOAD_ONCE
publish >"${tmp}/retry.log"
[[ "$(sed -n '1p' "${FAKE_GH_STATE}/release.meta")" == "1" ]] || {
    echo "error: retry changed the draft before --publish" >&2; exit 1;
}
publish --publish >"${tmp}/published.log"
[[ "$(sed -n '1p' "${FAKE_GH_STATE}/release.meta")" == "0" ]] || {
    echo "error: --publish did not publish the release" >&2; exit 1;
}
published_manual="$(python3 - "${FAKE_GH_STATE}/assets/v1.2.3/${manual}" <<'PY'
import hashlib, sys
print(hashlib.sha256(open(sys.argv[1], "rb").read()).hexdigest())
PY
)"

# A published release is immutable: changed local bytes must never rewrite its assets.
printf 'rebuilt with different bytes\n' >"${dist}/${manual}"
python3 - "${dist}" "${manual}" "${managed}" <<'PY'
import hashlib, sys
from pathlib import Path
dist = Path(sys.argv[1])
lines = []
for name in sys.argv[2:]:
    lines.append(f"{hashlib.sha256((dist/name).read_bytes()).hexdigest()}  {name}\n")
(dist / "SHA256SUMS.txt").write_text("".join(lines), encoding="utf-8")
PY
if publish --publish >"${tmp}/immutable.log" 2>&1; then
    echo "error: a published release accepted rebuilt bytes" >&2
    exit 1
fi
grep -q "refusing to modify the published release" "${tmp}/immutable.log" || {
    echo "error: published-release refusal did not explain itself" >&2
    cat "${tmp}/immutable.log" >&2
    exit 1
}
[[ "$(python3 - "${FAKE_GH_STATE}/assets/v1.2.3/${manual}" <<'PY'
import hashlib, sys
print(hashlib.sha256(open(sys.argv[1], "rb").read()).hexdigest())
PY
)" == "${published_manual}" ]] || {
    echo "error: published asset bytes changed" >&2
    exit 1
}

echo "publish tests passed"
