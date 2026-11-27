#!/usr/bin/env bash
# Release-ID publication, upload-host, failed-upload retry, concurrent-publish and immutable-release
# tests. The fake curl/gh pair models the real API split: JSON/release asset management on
# api.github.com and raw asset uploads on the release's uploads.github.com upload_url.
set -euo pipefail

tests_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${tests_dir}/../../.." && pwd)"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/zerodeck-publish-test.XXXXXX")"
trap 'if [[ "${ZERODECK_KEEP_TMP:-0}" == "1" ]]; then echo "kept test dir: ${tmp}" >&2; else rm -rf "${tmp}"; fi' EXIT

export FAKE_GH_STATE="${tmp}/gh-state"
export FAKE_GH_LOG="${tmp}/gh.log"
export FAKE_CURL_LOG="${tmp}/curl.log"
export GH_TOKEN="fixture-token"
mkdir -p "${FAKE_GH_STATE}/files"
: >"${FAKE_GH_LOG}"
: >"${FAKE_CURL_LOG}"

reset_release() {
    python3 - "${FAKE_GH_STATE}/release.json" <<'PY'
import json, sys
json.dump({
    "id": 42,
    "tag_name": "v1.2.3",
    "draft": True,
    "immutable": False,
    "upload_url": "https://uploads.github.com/repos/fixture/repo/releases/42/assets{?name,label}",
    "assets": [],
}, open(sys.argv[1], "w"))
PY
    rm -rf "${FAKE_GH_STATE}/files"
    mkdir -p "${FAKE_GH_STATE}/files"
    rm -f "${FAKE_GH_STATE}/failed-once" "${FAKE_GH_STATE}/uploaded-once"
    : >"${FAKE_GH_LOG}"
    : >"${FAKE_CURL_LOG}"
}
reset_release

fake_gh="${tmp}/gh"
cat >"${fake_gh}" <<'PY'
#!/usr/bin/env python3
import json
import os
import pathlib
import shutil
import sys

state = pathlib.Path(os.environ["FAKE_GH_STATE"])
log = pathlib.Path(os.environ["FAKE_GH_LOG"])
args = sys.argv[1:]
log.write_text(log.read_text() + " ".join(args) + "\n")

def fail(message=""):
    if message:
        print(message, file=sys.stderr)
    raise SystemExit(1)

def load():
    return json.load(open(state / "release.json", encoding="utf-8"))

def save(document):
    json.dump(document, open(state / "release.json", "w", encoding="utf-8"))

if not args or args[0] != "api":
    fail("unsupported gh command")
rest = args[1:]
method = "GET"
url = None
field_flag = ""
field = ""
index = 0
while index < len(rest):
    item = rest[index]
    if item == "--method":
        method = rest[index + 1]; index += 2
    elif item in ("-f", "--raw-field", "-F", "--field"):
        field_flag = item
        field = rest[index + 1]; index += 2
    elif item in ("-H", "--input"):
        index += 2
    elif item.startswith("-"):
        fail(f"unsupported flag {item}")
    else:
        url = item; index += 1
if url is None:
    fail("missing url")

document = load()

if url == "repos/fixture/repo/releases/42" and method == "GET":
    assets = []
    for asset in document["assets"]:
        path = state / "files" / str(asset["id"])
        assets.append({"id": asset["id"], "name": asset["name"], "size": path.stat().st_size})
    print(json.dumps({**document, "assets": assets}))
    raise SystemExit(0)

if url == "repos/fixture/repo/immutable-releases" and method == "GET":
    if os.environ.get("FAKE_IMMUTABLE_DISABLED") == "1":
        fail("immutable releases are disabled")
    print(json.dumps({"enabled": True, "enforced_by_owner": False}))
    raise SystemExit(0)

if url.startswith("repos/fixture/repo/releases/assets/") and method == "GET":
    asset_id = int(url.rsplit("/", 1)[1])
    path = state / "files" / str(asset_id)
    if not path.is_file():
        fail("asset file missing")
    sys.stdout.buffer.write(path.read_bytes())
    raise SystemExit(0)

if url.startswith("repos/fixture/repo/releases/assets/") and method == "DELETE":
    asset_id = int(url.rsplit("/", 1)[1])
    document["assets"] = [asset for asset in document["assets"] if asset["id"] != asset_id]
    save(document)
    (state / "files" / str(asset_id)).unlink(missing_ok=True)
    print("{}")
    raise SystemExit(0)

if "/releases/42/assets" in url and method == "POST":
    fail("asset upload must use the release upload_url on uploads.github.com")

if url == "repos/fixture/repo/releases/42" and method == "PATCH":
    if field_flag != "-F" or field != "draft=false":
        fail("publish patch must send a typed boolean with -F draft=false")
    document["draft"] = False
    document["immutable"] = True
    save(document)
    print("{}")
    raise SystemExit(0)

fail(f"unsupported API call: {method} {url}")
PY
chmod 0755 "${fake_gh}"

fake_curl="${tmp}/curl"
cat >"${fake_curl}" <<'PY'
#!/usr/bin/env python3
import json
import os
import pathlib
import shutil
import sys
import urllib.parse

state = pathlib.Path(os.environ["FAKE_GH_STATE"])
log = pathlib.Path(os.environ["FAKE_CURL_LOG"])
args = sys.argv[1:]
log.write_text(log.read_text() + " ".join(args) + "\n")
url = args[-1]
input_path = None
header = []
index = 0
while index < len(args):
    if args[index] == "--data-binary":
        value = args[index + 1]
        if not value.startswith("@"):
            raise SystemExit("upload must stream a file with --data-binary @file")
        input_path = value[1:]
        index += 2
    elif args[index] == "--header":
        header.append(args[index + 1])
        index += 2
    else:
        index += 1
if input_path is None:
    raise SystemExit("missing --data-binary")
if not url.startswith("https://uploads.github.com/repos/fixture/repo/releases/42/assets?name="):
    raise SystemExit(f"upload used the wrong host/path: {url}")
if not any(value == "Authorization: Bearer fixture-token" for value in header):
    raise SystemExit("upload did not carry the token")
if not any(value == "Content-Type: application/octet-stream" for value in header):
    raise SystemExit("upload did not set the binary content type")
name = urllib.parse.unquote(url.split("name=", 1)[1])
document = json.load(open(state / "release.json", encoding="utf-8"))
if document["draft"] is not True:
    raise SystemExit("upload attempted against a published release")
if any(asset["name"] == name for asset in document["assets"]):
    raise SystemExit("upload attempted to clobber an existing asset")
if os.environ.get("FAKE_CURL_FAIL_ONCE") == "1" and not (state / "failed-once").exists():
    (state / "failed-once").write_text("1")
    raise SystemExit(1)
next_id = max([asset["id"] for asset in document["assets"]] or [100]) + 1
shutil.copyfile(input_path, state / "files" / str(next_id))
document["assets"].append({"id": next_id, "name": name})
json.dump(document, open(state / "release.json", "w", encoding="utf-8"))
if os.environ.get("FAKE_GH_CONCURRENT_PUBLISH") == "1" and not (state / "uploaded-once").exists():
    (state / "uploaded-once").write_text("1")
    document["draft"] = False
    document["immutable"] = True
    json.dump(document, open(state / "release.json", "w", encoding="utf-8"))
print("{}")
PY
chmod 0755 "${fake_curl}"

dist="${tmp}/dist"
mkdir -p "${dist}"
manual="zerodeck-v1.2.3-linux-x86_64.tar.gz"
managed="zerodeck-v1.2.3-linux-x86_64.managed.tar.gz"
printf 'manual archive fixture\n' >"${dist}/${manual}"
printf 'managed archive fixture\n' >"${dist}/${managed}"
write_sums() {
    python3 - "${dist}" "${manual}" "${managed}" <<'PY'
import hashlib, sys
from pathlib import Path
dist = Path(sys.argv[1])
lines = []
for name in sys.argv[2:]:
    lines.append(f"{hashlib.sha256((dist/name).read_bytes()).hexdigest()}  {name}\n")
(dist / "SHA256SUMS.txt").write_text("".join(lines), encoding="utf-8")
PY
}
write_sums

publish() {
    bash "${repo_root}/deploy/update/publish-release.sh" \
        --repo fixture/repo --release-id 42 --tag v1.2.3 --dist "${dist}" \
        --gh "${fake_gh}" --curl "${fake_curl}" "$@"
}

draft_state() {
    python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["draft"])' \
        "${FAKE_GH_STATE}/release.json"
}

# Missing managed asset: refuse before any release mutation.
mkdir -p "${tmp}/missing"
cp "${dist}/${manual}" "${tmp}/missing/"
python3 - "${tmp}/missing" "${manual}" <<'PY'
import hashlib, sys
from pathlib import Path
dist = Path(sys.argv[1])
name = sys.argv[2]
(dist / "SHA256SUMS.txt").write_text(
    f"{hashlib.sha256((dist/name).read_bytes()).hexdigest()}  {name}\n", encoding="utf-8"
)
PY
if bash "${repo_root}/deploy/update/publish-release.sh" \
    --repo fixture/repo --release-id 42 --tag v1.2.3 --dist "${tmp}/missing" \
    --gh "${fake_gh}" --curl "${fake_curl}" >"${tmp}/missing.log" 2>&1; then
    echo "error: missing managed asset was accepted" >&2
    exit 1
fi
grep -q "missing release asset" "${tmp}/missing.log" || {
    echo "error: missing asset did not fail with the expected diagnostics" >&2
    cat "${tmp}/missing.log" >&2
    exit 1
}
[[ "$(draft_state)" == "True" ]] || { echo "error: missing asset published the draft" >&2; exit 1; }

# The upload host and exact bytes are the release upload_url and local asset.
FAKE_CURL_FAIL_ONCE=1
export FAKE_CURL_FAIL_ONCE
if publish >"${tmp}/first-upload.log" 2>&1; then
    echo "error: injected upload failure unexpectedly succeeded" >&2
    exit 1
fi
[[ "$(draft_state)" == "True" ]] || { echo "error: failed upload published the release" >&2; exit 1; }
unset FAKE_CURL_FAIL_ONCE
publish >"${tmp}/retry.log"
grep -q "https://uploads.github.com/repos/fixture/repo/releases/42/assets?name=" "${FAKE_CURL_LOG}" || {
    echo "error: upload did not use uploads.github.com upload_url" >&2
    cat "${FAKE_CURL_LOG}" >&2
    exit 1
}
grep -q "Authorization: Bearer fixture-token" "${FAKE_CURL_LOG}" || {
    echo "error: upload did not carry the token" >&2; exit 1;
}
if grep -q "releases/42/assets" "${FAKE_GH_LOG}"; then
    echo "error: asset upload went through api.github.com instead of uploads.github.com" >&2
    cat "${FAKE_GH_LOG}" >&2
    exit 1
fi
python3 - "${FAKE_GH_STATE}" "${dist}" "${manual}" "${managed}" <<'PY'
import json, sys
from pathlib import Path
state, dist = Path(sys.argv[1]), Path(sys.argv[2])
document = json.load(open(state / "release.json", encoding="utf-8"))
for name in sys.argv[3:]:
    asset = next(asset for asset in document["assets"] if asset["name"] == name)
    assert (state / "files" / str(asset["id"])).read_bytes() == (dist / name).read_bytes()
PY
publish --publish >"${tmp}/published.log"
[[ "$(draft_state)" == "False" ]] || { echo "error: --publish did not publish the release" >&2; exit 1; }
grep -q -- "-F draft=false" "${FAKE_GH_LOG}" || {
    echo "error: publish PATCH did not use a typed boolean" >&2; cat "${FAKE_GH_LOG}" >&2; exit 1;
}
if grep -q -- "-f draft=false" "${FAKE_GH_LOG}"; then
    echo "error: publish PATCH used a string boolean" >&2
    exit 1
fi
python3 - "${FAKE_GH_STATE}/release.json" <<'PY'
import json, sys
document = json.load(open(sys.argv[1]))
assert document["immutable"] is True
PY

# Published release is immutable: changed local bytes must never rewrite its assets.
published_manual="$(python3 -c 'import json,sys; print(next(a["id"] for a in json.load(open(sys.argv[1]))["assets"] if a["name"] == "zerodeck-v1.2.3-linux-x86_64.tar.gz"))' "${FAKE_GH_STATE}/release.json")"
printf 'rebuilt with different bytes\n' >"${dist}/${manual}"
write_sums
if publish --publish >"${tmp}/immutable.log" 2>&1; then
    echo "error: published release accepted rebuilt bytes" >&2; exit 1
fi
grep -q "refusing to modify the published release" "${tmp}/immutable.log" || {
    echo "error: published-release refusal did not explain itself" >&2
    cat "${tmp}/immutable.log" >&2
    exit 1
}
python3 - "${FAKE_GH_STATE}" "${published_manual}" <<'PY'
import hashlib, sys
from pathlib import Path
state = Path(sys.argv[1])
asset_id = int(sys.argv[2])
digest = hashlib.sha256((state / "files" / str(asset_id)).read_bytes()).hexdigest()
assert digest == hashlib.sha256(b"manual archive fixture\n").hexdigest(), digest
PY

# Immutable releases must be enabled; refuse before upload when the repository API says no.
reset_release
write_sums
FAKE_IMMUTABLE_DISABLED=1
export FAKE_IMMUTABLE_DISABLED
if publish >"${tmp}/mutable.log" 2>&1; then
    echo "error: publish proceeded with immutable releases disabled" >&2
    exit 1
fi
grep -q "immutable releases" "${tmp}/mutable.log" || {
    echo "error: immutable-release refusal did not explain itself" >&2
    cat "${tmp}/mutable.log" >&2
    exit 1
}
unset FAKE_IMMUTABLE_DISABLED

# A concurrent human publish invalidates the draft before the next mutation; fail loudly.
reset_release
write_sums
FAKE_GH_CONCURRENT_PUBLISH=1
export FAKE_GH_CONCURRENT_PUBLISH
if publish --publish >"${tmp}/concurrent.log" 2>&1; then
    echo "error: concurrent publication was not detected" >&2
    exit 1
fi
grep -q "no longer a draft" "${tmp}/concurrent.log" || {
    echo "error: concurrent publication did not fail with a draft-race diagnostic" >&2
    cat "${tmp}/concurrent.log" >&2
    exit 1
}
unset FAKE_GH_CONCURRENT_PUBLISH

# Without --gh/--curl the tools are resolved through PATH (the way the workflows call the script).
reset_release
write_sums
path_bin="${tmp}/path-bin"
mkdir -p "${path_bin}"
cp "${fake_gh}" "${path_bin}/gh"
cp "${fake_curl}" "${path_bin}/curl"
PATH="${path_bin}:${PATH}" bash "${repo_root}/deploy/update/publish-release.sh" \
    --repo fixture/repo --release-id 42 --tag v1.2.3 --dist "${dist}" >"${tmp}/path.log" 2>&1 || {
    echo "error: publish-release.sh could not resolve gh/curl through PATH" >&2
    cat "${tmp}/path.log" >&2
    exit 1
}

echo "publish tests passed"
