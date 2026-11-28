#!/usr/bin/env bash
# Build the two release assets from one immutable runtime image.
#
#   ZERODECK_BUILD_ID=v0.1.0-abc123 just dist v0.1.0 ghcr.io/owner/repo@sha256:...
#
# `manual` keeps the historical `zerodeck-<tag>-linux-<arch>/` wrapper and adds the controller,
# systemd units, bootstrap helper and documentation. `managed` is the TUF generation payload: its
# archive root is exactly `release.json`, `bin/`, `python/` and `frontend/`, because the archive
# digest is the generation identity consumed by `zerodeckctl`.
set -euo pipefail

usage() {
    cat >&2 <<'EOF'
usage: package-release.sh <manual|managed|all> <tag> <runtime-image> [output-dir]

The runtime image must be addressed by digest in release automation. ZERODECK_BUILD_ID must be
set for a real `v*` release; local development defaults to `dev`.
EOF
    exit 2
}

[[ $# -ge 3 && $# -le 4 ]] || usage
mode="$1"
tag="$2"
image="$3"
outdir="${4:-dist}"
case "${mode}" in
    manual|managed|all) ;;
    *) usage ;;
esac

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${repo_root}"

version="$(awk '
    /^\[workspace\.package\]/ { in_package = 1; next }
    in_package && /^\[/ { exit }
    in_package && /^version[[:space:]]*=/ {
        gsub(/["[:space:]]/, "", $3);
        print $3;
        exit
    }
' Cargo.toml)"
if [[ ! "${version}" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$ ]]; then
    echo "error: cannot read a SemVer workspace version from Cargo.toml" >&2
    exit 1
fi
if [[ "${tag}" =~ ^v[0-9]+\.[0-9]+\.[0-9]+ ]]; then
    tag_version="${tag#v}"
    if [[ "${tag_version}" != "${version}" ]]; then
        echo "error: tag ${tag} does not match workspace version ${version}" >&2
        exit 1
    fi
fi

build_id="${ZERODECK_BUILD_ID:-dev}"
if [[ -z "${build_id//[[:space:]]/}" ]]; then
    echo "error: ZERODECK_BUILD_ID must not be empty" >&2
    exit 1
fi
if [[ "${tag}" == v* && "${build_id}" == "dev" ]]; then
    echo "error: a real release build must set ZERODECK_BUILD_ID explicitly" >&2
    exit 1
fi

arch="$(docker image inspect --format '{{.Architecture}}' "${image}")"
case "${arch}" in
    amd64) arch_name=x86_64 ;;
    arm64) arch_name=aarch64 ;;
    *) echo "error: unsupported image architecture: ${arch}" >&2; exit 1 ;;
esac
if [[ "${mode}" == "managed" && "${arch_name}" != "x86_64" ]]; then
    echo "error: managed generations are only supported for linux-x86_64, found ${arch_name}" >&2
    exit 1
fi

mkdir -p "${outdir}"
outdir="$(cd "${outdir}" && pwd)"
work="$(mktemp -d "${outdir}/.package.XXXXXX")"
container=""
cleanup() {
    if [[ -n "${container}" ]]; then
        docker rm -f "${container}" >/dev/null 2>&1 || true
    fi
    rm -rf "${work}"
}
trap cleanup EXIT

# Reject an incomplete frontend before it can become a release generation.
verify_frontend_bundle() {
    local directory="$1"
    local file
    for file in index.js fallback.js; do
        if [[ ! -s "${directory}/${file}" ]]; then
            echo "error: frontend bundle is missing or empty: ${file}" >&2
            exit 1
        fi
        local syntax_check="${work}/frontend-${file}.mjs"
        cp "${directory}/${file}" "${syntax_check}"
        if ! node --check "${syntax_check}" >/dev/null 2>&1; then
            echo "error: frontend bundle does not parse as an ES module: ${file}" >&2
            exit 1
        fi
    done
}

stage_manual=""
if [[ "${mode}" == "manual" || "${mode}" == "all" ]]; then
    stage_manual="${work}/manual/zerodeck-${tag}-linux-${arch_name}"
    mkdir -p "${stage_manual}/bin" "${stage_manual}/python" "${stage_manual}/frontend" \
        "${stage_manual}/systemd" "${stage_manual}/docs"
    container="$(docker create "${image}")"
    docker cp "${container}:/usr/local/bin/zerodeckd" "${stage_manual}/bin/zerodeckd"
    docker cp "${container}:/usr/local/lib/zerodeck/bin/zerodeckctl" \
        "${stage_manual}/bin/zerodeckctl"
    docker cp "${container}:/opt/zerodeck/python/." "${stage_manual}/python/"
    docker cp "${container}:/opt/zerodeck/frontend/." "${stage_manual}/frontend/"
    docker rm -f "${container}" >/dev/null
    container=""
    cp deploy/systemd/*.service deploy/systemd/*.timer "${stage_manual}/systemd/"
    cp deploy/update/bootstrap.sh "${stage_manual}/bootstrap.sh"
    cp deploy/update/trusted-root.json "${stage_manual}/trusted-root.json"
    cp deploy/update/RELEASE.md "${stage_manual}/docs/RELEASE.md"
    cp deploy/update/COMPATIBILITY.md "${stage_manual}/docs/COMPATIBILITY.md"
    cp deploy/update/QUALIFICATION.md "${stage_manual}/docs/QUALIFICATION.md"
    cp docs/architecture/performance-budget.md "${stage_manual}/docs/PERFORMANCE.md"
    cp LICENSE README.md "${stage_manual}/"
    chmod 0755 "${stage_manual}/bin/zerodeckd" "${stage_manual}/bin/zerodeckctl" \
        "${stage_manual}/bootstrap.sh"
    chmod 0644 "${stage_manual}"/systemd/* "${stage_manual}"/docs/* \
        "${stage_manual}/LICENSE" "${stage_manual}/README.md" "${stage_manual}/trusted-root.json"
    verify_frontend_bundle "${stage_manual}/frontend"
fi

stage_managed=""
if [[ "${mode}" == "managed" || "${mode}" == "all" ]]; then
    if [[ "${arch_name}" != "x86_64" ]]; then
        echo "warning: skipping the managed archive on ${arch_name}; TUF targets are linux-x86_64" >&2
    else
        stage_managed="${work}/managed"
        mkdir -p "${stage_managed}/bin"
        if [[ -n "${stage_manual}" ]]; then
            cp -a "${stage_manual}/bin/zerodeckd" "${stage_managed}/bin/zerodeckd"
            cp -a "${stage_manual}/python" "${stage_managed}/python"
            cp -a "${stage_manual}/frontend" "${stage_managed}/frontend"
        else
            container="$(docker create "${image}")"
            docker cp "${container}:/usr/local/bin/zerodeckd" "${stage_managed}/bin/zerodeckd"
            docker cp "${container}:/opt/zerodeck/python/." "${stage_managed}/python/"
            docker cp "${container}:/opt/zerodeck/frontend/." "${stage_managed}/frontend/"
            docker rm -f "${container}" >/dev/null
            container=""
        fi
        chmod 0755 "${stage_managed}/bin/zerodeckd"
        verify_frontend_bundle "${stage_managed}/frontend"
    fi
fi

write_manifest() {
    local destination="$1"
    python3 - "${destination}/release.json" "${build_id}" "${version}" <<'PY'
import json, os, sys
path, build_id, version = sys.argv[1:4]
with open(path + ".tmp", "w", encoding="utf-8") as handle:
    json.dump({"schema": 1, "build_id": build_id, "version": version}, handle, sort_keys=True)
    handle.write("\n")
os.replace(path + ".tmp", path)
PY
    chmod 0644 "${destination}/release.json"
}

make_archive() {
    local directory="$1"
    local prefix="$2"
    local destination="$3"
    "${PYTHON:-python3}" - "${directory}" "${prefix}" "${destination}" <<'PY'
import gzip
import os
import sys
import tarfile

directory, prefix, destination = sys.argv[1:4]
entries = []
for root, dirs, files in os.walk(directory):
    dirs.sort()
    relative_root = os.path.relpath(root, directory)
    if relative_root == ".":
        relative_root = ""
    if relative_root:
        entries.append((relative_root, None))
    for name in sorted(files):
        relative = os.path.join(relative_root, name) if relative_root else name
        entries.append((relative, os.path.join(root, name)))

entries.sort(key=lambda item: item[0])
temp = destination + ".tmp"
with open(temp, "wb") as raw:
    with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0, compresslevel=9) as gz:
        with tarfile.open(fileobj=gz, mode="w", format=tarfile.PAX_FORMAT) as archive:
            for relative, path in entries:
                name = f"{prefix}{relative}" if prefix else relative
                info = tarfile.TarInfo(name)
                info.uid = 0
                info.gid = 0
                info.uname = "root"
                info.gname = "root"
                info.mtime = 0
                if path is None:
                    info.type = tarfile.DIRTYPE
                    info.mode = 0o755
                    archive.addfile(info)
                else:
                    info.type = tarfile.REGTYPE
                    info.mode = os.stat(path).st_mode & 0o777
                    info.size = os.path.getsize(path)
                    with open(path, "rb") as handle:
                        archive.addfile(info, handle)
os.replace(temp, destination)
PY
}

manual_archive="${outdir}/zerodeck-${tag}-linux-${arch_name}.tar.gz"
managed_archive="${outdir}/zerodeck-${tag}-linux-${arch_name}.managed.tar.gz"
produced=()

if [[ -n "${stage_manual}" ]]; then
    write_manifest "${stage_manual}"
    rm -f "${manual_archive}"
    make_archive "${work}/manual" "" "${manual_archive}"
    produced+=("$(basename "${manual_archive}")")
fi
if [[ -n "${stage_managed}" ]]; then
    write_manifest "${stage_managed}"
    rm -f "${managed_archive}"
    make_archive "${stage_managed}" "" "${managed_archive}"
    produced+=("$(basename "${managed_archive}")")
fi

if [[ "${#produced[@]}" -eq 0 ]]; then
    echo "error: no release asset was produced" >&2
    exit 1
fi

"${PYTHON:-python3}" - "${outdir}" "${produced[@]}" <<'PY'
import hashlib
import sys
from pathlib import Path

outdir = Path(sys.argv[1])
lines = []
for name in sys.argv[2:]:
    digest = hashlib.sha256((outdir / name).read_bytes()).hexdigest()
    lines.append(f"{digest}  {name}\n")
(outdir / "SHA256SUMS.txt").write_text("".join(lines), encoding="utf-8")
PY

for asset in "${produced[@]}"; do
    echo "dist/${asset}"
done
