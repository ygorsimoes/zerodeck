#!/usr/bin/env bash
# Release-ID publication, failed-upload retry, concurrent-publish and immutable-release tests.
set -euo pipefail

tests_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${tests_dir}/../../.." && pwd)"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/zerodeck-publish-test.XXXXXX")"
trap 'if [[ "${ZERODECK_KEEP_TMP:-0}" == "1" ]]; then echo "kept test dir: ${tmp}" >&2; else rm -rf "${tmp}"; fi' EXIT

export FAKE_GH_STATE="${tmp}/gh-state"
export FAKE_GH_LOG="${tmp}/gh.log"
mkdir -p "${FAKE_GH_STATE}/files"
: >"${FAKE_GH_LOG}"

reset_release() {
    python3 - "${FAKE_GH_STATE}/release.json" <<'PY'
import json, sys
json.dump({"id": 42, "tag_name": "v1.2.3", "draft": True, "assets": []}, open(sys.argv[1], "w"))
PY
    rm -rf "${FAKE_GH_STATE}/files"
    mkdir -p "${FAKE_GH_STATE}/files"
    rm -f "${FAKE_GH_STATE}/failed-once" "${FAKE_GH_STATE}/uploaded-once"
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
input_path = None
header = ""
field = ""
url = None
index = 0
while index < len(rest):
    item = rest[index]
    if item == "--method":
        method = rest[index + 1]; index += 2
    elif item in ("-X",):
        method = rest[index + 1]; index += 2
    elif item == "--input":
        input_path = rest[index + 1]; index += 2
    elif item == "-H":
        header = rest[index + 1]; index += 2
    elif item in ("-f", "--raw-field"):
        field = rest[index + 1]; index += 2
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

prefix = "repos/fixture/repo/releases/"
if url.startswith(prefix) and "/assets" in url and method == "POST":
    if document["draft"] is not True:
        fail("cannot upload to a published release")
    if os.environ.get("FAKE_GH_FAIL_UPLOAD_ONCE") == "1" and not (state / "failed-once").exists():
        (state / "failed-once").write_text("1")
        fail("injected upload failure")
    name = url.split("name=", 1)[1]
    from urllib.parse import unquote
    name = unquote(name)
    next_id = max([asset["id"] for asset in document["assets"]] or [100]) + 1
    if input_path is None:
        fail("missing --input")
    shutil.copyfile(input_path, state / "files" / str(next_id))
    document["assets"].append({"id": next_id, "name": name})
    save(document)
    if os.environ.get("FAKE_GH_CONCURRENT_PUBLISH") == "1" and not (state / "uploaded-once").exists():
        (state / "uploaded-once").write_text("1")
        document["draft"] = False
        save(document)
    print("{}")
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

if url == "repos/fixture/repo/releases/42" and method == "PATCH":
    if field == "draft=false":
        document["draft"] = False
        save(document)
        print("{}")
        raise SystemExit(0)
    fail("unsupported patch")

fail(f"unsupported API call: {method} {url}")
PY
chmod 0755 "${fake_gh}"

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
        --repo fixture/repo --release-id 42 --tag v1.2.3 --dist "${dist}" --gh "${fake_gh}" "$@"
}

# Missing managed asset: refuse before any release mutation.
mkdir -p "${tmp}/missing"
cp "${dist}/${manual}" "${tmp}/missing/"
write_sums_missing() {
    python3 - "${tmp}/missing" "${manual}" <<'PY'
import hashlib, sys
from pathlib import Path
dist = Path(sys.argv[1])
name = sys.argv[2]
(dist / "SHA256SUMS.txt").write_text(
    f"{hashlib.sha256((dist/name).read_bytes()).hexdigest()}  {name}\n", encoding="utf-8"
)
PY
}
write_sums_missing
if bash "${repo_root}/deploy/update/publish-release.sh" \
    --repo fixture/repo --release-id 42 --tag v1.2.3 --dist "${tmp}/missing" \
    --gh "${fake_gh}" >"${tmp}/missing.log" 2>&1; then
    echo "error: missing managed asset was accepted" >&2
    exit 1
fi
grep -q "missing release asset" "${tmp}/missing.log" || {
    echo "error: missing asset did not fail with the expected diagnostics" >&2
    cat "${tmp}/missing.log" >&2
    exit 1
}
[[ "$(python3 -c 'import json; print(json.load(open("'"${FAKE_GH_STATE}"'/release.json"))["draft"])')" == "True" ]] || {
    echo "error: missing asset published the draft" >&2; exit 1;
}

# A failed upload leaves the draft unpublished and retryable by release ID.
FAKE_GH_FAIL_UPLOAD_ONCE=1
export FAKE_GH_FAIL_UPLOAD_ONCE
if publish >"${tmp}/first-upload.log" 2>&1; then
    echo "error: injected upload failure unexpectedly succeeded" >&2
    exit 1
fi
[[ "$(python3 -c 'import json; print(json.load(open("'"${FAKE_GH_STATE}"'/release.json"))["draft"])')" == "True" ]] || {
    echo "error: failed upload published the release" >&2; exit 1;
}
unset FAKE_GH_FAIL_UPLOAD_ONCE
publish >"${tmp}/retry.log"
[[ "$(python3 -c 'import json; print(json.load(open("'"${FAKE_GH_STATE}"'/release.json"))["draft"])')" == "True" ]] || {
    echo "error: retry published before --publish" >&2; exit 1;
}
publish --publish >"${tmp}/published.log"
[[ "$(python3 -c 'import json; print(json.load(open("'"${FAKE_GH_STATE}"'/release.json"))["draft"])')" == "False" ]] || {
    echo "error: --publish did not publish the release" >&2; exit 1;
}
grep -q "releases/42" "${FAKE_GH_LOG}" || { echo "error: release ID was not used" >&2; exit 1; }
if grep -q "releases/tags" "${FAKE_GH_LOG}"; then
    echo "error: draft publication used the published-only tag endpoint" >&2
    exit 1
fi

# Published release is immutable: changed local bytes must never rewrite its assets.
published_manual="$(python3 - "${FAKE_GH_STATE}/release.json" <<'PY'
import json, sys
document = json.load(open(sys.argv[1]))
asset = next(asset for asset in document["assets"] if asset["name"] == "zerodeck-v1.2.3-linux-x86_64.tar.gz")
print(asset["id"])
PY
)"
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
import hashlib, json, sys
from pathlib import Path
state = Path(sys.argv[1])
asset_id = int(sys.argv[2])
digest = hashlib.sha256((state / "files" / str(asset_id)).read_bytes()).hexdigest()
assert digest == hashlib.sha256(b"manual archive fixture\n").hexdigest(), digest
PY

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

echo "publish tests passed"
