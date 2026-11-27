#!/usr/bin/env python3
"""Validate ZeroDeck release archives or produce deliberately broken copies for tests.

Release archives are treated as untrusted input: only regular files and directories are accepted,
no absolute or parent-traversing names, no duplicate entries and no links. Managed archives contain
one immutable loader generation at the archive root. Manual archives keep the historical
`zerodeck-<tag>-linux-<arch>/` wrapper so an operator can inspect or copy the files by hand.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import sys
import tarfile
import tempfile
from pathlib import Path, PurePosixPath

SEMVER = re.compile(r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$")
MANAGED_SUFFIX = ".managed.tar.gz"
REQUIRED_UNITS = (
    "zerodeck.service",
    "zerodeck-apply.service",
    "zerodeck-recovery.service",
    "zerodeck-check.service",
    "zerodeck-check.timer",
)
MANAGED_TOP_LEVEL = {"release.json", "bin", "python", "frontend"}


class InvalidArchive(Exception):
    pass


def fail(message: str) -> None:
    raise InvalidArchive(message)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def normalized(name: str) -> str:
    if name.startswith("/"):
        fail(f"absolute archive path: {name!r}")
    path = PurePosixPath(name)
    parts = [part for part in path.parts if part not in ("", ".")]
    if any(part == ".." for part in parts):
        fail(f"parent-traversing archive path: {name!r}")
    if not parts:
        fail(f"empty archive path: {name!r}")
    return "/".join(parts)


def open_archive(path: Path) -> tuple[tarfile.TarFile, dict[str, tarfile.TarInfo]]:
    try:
        archive = tarfile.open(path, "r:gz")
    except (tarfile.TarError, OSError) as error:
        fail(f"cannot open gzip tar archive {path}: {error}")
    members: dict[str, tarfile.TarInfo] = {}
    try:
        for member in archive.getmembers():
            if member.issym() or member.islnk():
                fail(f"links are not allowed in release archives: {member.name!r}")
            if not (member.isfile() or member.isdir()):
                fail(f"unsupported archive entry type for {member.name!r}")
            name = normalized(member.name)
            if name in members:
                fail(f"duplicate archive entry: {name!r}")
            if member.isdir() and member.mode & 0o022:
                fail(f"directory {name!r} is group/other writable ({member.mode:o})")
            if member.isfile() and member.mode & 0o022:
                fail(f"file {name!r} is group/other writable ({member.mode:o})")
            members[name] = member
    except InvalidArchive:
        archive.close()
        raise
    except (tarfile.TarError, OSError) as error:
        archive.close()
        fail(f"cannot read gzip tar archive {path}: {error}")
    return archive, members


def require_file(members: dict[str, tarfile.TarInfo], name: str, mode: int | None = None) -> None:
    member = members.get(name)
    if member is None:
        fail(f"missing required file: {name}")
    if not member.isfile():
        fail(f"required path is not a regular file: {name}")
    if mode is not None and (member.mode & 0o777) != mode:
        fail(f"{name} has mode {member.mode & 0o777:o}, expected {mode:o}")


def require_dir(members: dict[str, tarfile.TarInfo], name: str) -> None:
    member = members.get(name)
    if member is None:
        fail(f"missing required directory: {name}")
    if not member.isdir():
        fail(f"required path is not a directory: {name}")


def require_nonempty_tree(members: dict[str, tarfile.TarInfo], prefix: str) -> None:
    require_dir(members, prefix)
    if not any(name.startswith(f"{prefix}/") and member.isfile() for name, member in members.items()):
        fail(f"required directory has no files: {prefix}")


def read_release_json(path: Path, members: dict[str, tarfile.TarInfo], archive: tarfile.TarFile, prefix: str = "") -> dict:
    name = f"{prefix}release.json" if prefix else "release.json"
    member = members.get(name)
    if member is None or not member.isfile():
        fail(f"missing required release manifest: {name}")
    extracted = archive.extractfile(member)
    if extracted is None:
        fail(f"cannot read release manifest: {name}")
    try:
        document = json.load(extracted)
    except json.JSONDecodeError as error:
        fail(f"{name} is not valid JSON: {error}")
    if not isinstance(document, dict):
        fail(f"{name} must be a JSON object")
    if document.get("schema") != 1:
        fail(f"{name} must declare schema 1, found {document.get('schema')!r}")
    version = document.get("version")
    if not isinstance(version, str) or not SEMVER.match(version):
        fail(f"{name} has an invalid SemVer version: {version!r}")
    build_id = document.get("build_id")
    if not isinstance(build_id, str) or not build_id.strip():
        fail(f"{name} has an empty build_id")
    return document


def expected_version_from_name(archive: Path) -> str | None:
    name = archive.name
    match = re.match(r"^zerodeck-(.+)-linux-[A-Za-z0-9_]+(?:\.managed)?\.tar\.gz$", name)
    if not match:
        return None
    tag = match.group(1)
    if tag.startswith("v") and SEMVER.match(tag[1:]):
        return tag[1:]
    return None


def workspace_version() -> str | None:
    root = Path(__file__).resolve().parents[3]
    cargo = root / "Cargo.toml"
    if not cargo.is_file():
        return None
    in_workspace = False
    for line in cargo.read_text(encoding="utf-8").splitlines():
        if line.strip() == "[workspace.package]":
            in_workspace = True
            continue
        if in_workspace and line.startswith("["):
            break
        if in_workspace:
            match = re.match(r'^version\s*=\s*"([^"]+)"', line)
            if match:
                return match.group(1)
    return None


def check_identity(archive: Path, document: dict) -> tuple[str, str]:
    expected = os.environ.get("ZERODECK_EXPECTED_VERSION", "").strip()
    if not expected:
        expected = expected_version_from_name(archive) or workspace_version() or ""
    if expected and document["version"] != expected:
        fail(f"release.json version {document['version']} does not match expected {expected}")
    expected_build_id = os.environ.get("ZERODECK_EXPECTED_BUILD_ID", "").strip()
    if expected_build_id and document["build_id"] != expected_build_id:
        fail(
            f"release.json build_id {document['build_id']!r} does not match expected "
            f"{expected_build_id!r}"
        )
    return document["version"], document["build_id"]


def read_sibling_identity(archive: Path) -> tuple[str, str] | None:
    if archive.name.endswith(MANAGED_SUFFIX):
        sibling = archive.with_name(archive.name[: -len(MANAGED_SUFFIX)] + ".tar.gz")
        prefix = ""
    else:
        sibling = archive.with_name(archive.name[: -len(".tar.gz")] + MANAGED_SUFFIX)
        prefix = ""
    if not sibling.is_file():
        return None
    sibling_archive, sibling_members = open_archive(sibling)
    try:
        if not sibling.name.endswith(MANAGED_SUFFIX):
            wrapper = next(iter({name.split("/", 1)[0] for name in sibling_members}))
            prefix = f"{wrapper}/"
        document = read_release_json(sibling, sibling_members, sibling_archive, prefix=prefix)
        check_identity(sibling, document)
        return document["version"], document["build_id"]
    finally:
        sibling_archive.close()


def validate_checksums(archive: Path) -> None:
    if os.environ.get("ZERODECK_SKIP_CHECKSUMS") == "1":
        return
    sums = archive.parent / "SHA256SUMS.txt"
    if not sums.is_file():
        fail(f"missing checksum manifest next to archive: {sums}")
    entries: dict[str, str] = {}
    for line in sums.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line:
            continue
        parts = line.split(None, 1)
        if len(parts) != 2 or not re.fullmatch(r"[0-9a-f]{64}", parts[0]):
            fail(f"malformed checksum line: {line!r}")
        name = parts[1].lstrip("*")
        if name in entries:
            fail(f"duplicate checksum entry: {name}")
        entries[name] = parts[0]
    if archive.name not in entries:
        fail(f"checksum manifest does not cover {archive.name}")
    for name, digest in entries.items():
        candidate = archive.parent / name
        if not candidate.is_file():
            fail(f"checksum manifest references a missing file: {name}")
        if sha256_file(candidate) != digest:
            fail(f"checksum mismatch for {name}")
    if entries[archive.name] != sha256_file(archive):
        fail(f"checksum mismatch for {archive.name}")


def validate_managed(archive: Path, members: dict[str, tarfile.TarInfo], handle: tarfile.TarFile) -> tuple[str, str]:
    top_level = {name.split("/", 1)[0] for name in members}
    if top_level != MANAGED_TOP_LEVEL:
        fail(
            "managed archive must contain exactly "
            f"{sorted(MANAGED_TOP_LEVEL)} at the archive root, found {sorted(top_level)}"
        )
    require_dir(members, "bin")
    require_file(members, "bin/zerodeckd", 0o755)
    require_nonempty_tree(members, "python")
    require_nonempty_tree(members, "frontend")
    document = read_release_json(archive, members, handle)
    version, build_id = check_identity(archive, document)
    sibling = read_sibling_identity(archive)
    if sibling is not None and sibling != (version, build_id):
        fail(
            "manual and managed archives were not built from the same inputs: "
            f"managed={version}/{build_id} manual={sibling[0]}/{sibling[1]}"
        )
    return version, build_id


def validate_manual(archive: Path, members: dict[str, tarfile.TarInfo], handle: tarfile.TarFile) -> tuple[str, str]:
    top_level = {name.split("/", 1)[0] for name in members}
    if len(top_level) != 1:
        fail(f"manual archive must contain exactly one wrapper directory, found {sorted(top_level)}")
    wrapper = next(iter(top_level))
    if not re.fullmatch(r"zerodeck-.+-linux-[A-Za-z0-9_]+", wrapper):
        fail(f"unexpected manual archive wrapper directory: {wrapper!r}")
    if not members[wrapper].isdir():
        fail(f"manual archive wrapper is not a directory: {wrapper!r}")
    require_dir(members, f"{wrapper}/bin")
    require_file(members, f"{wrapper}/bin/zerodeckd", 0o755)
    require_file(members, f"{wrapper}/bin/zerodeckctl", 0o755)
    require_file(members, f"{wrapper}/bootstrap.sh", 0o755)
    require_file(members, f"{wrapper}/trusted-root.json")
    require_file(members, f"{wrapper}/LICENSE")
    require_file(members, f"{wrapper}/README.md")
    require_file(members, f"{wrapper}/docs/RELEASE.md")
    require_file(members, f"{wrapper}/docs/COMPATIBILITY.md")
    require_nonempty_tree(members, f"{wrapper}/python")
    require_nonempty_tree(members, f"{wrapper}/frontend")
    require_dir(members, f"{wrapper}/systemd")
    for unit in REQUIRED_UNITS:
        require_file(members, f"{wrapper}/systemd/{unit}")
    document = read_release_json(archive, members, handle, prefix=f"{wrapper}/")
    version, build_id = check_identity(archive, document)
    sibling = read_sibling_identity(archive)
    if sibling is not None and sibling != (version, build_id):
        fail(
            "manual and managed archives were not built from the same inputs: "
            f"manual={version}/{build_id} managed={sibling[0]}/{sibling[1]}"
        )
    return version, build_id


def validate(archive: Path, mode: str) -> tuple[str, str]:
    handle, members = open_archive(archive)
    try:
        if mode == "managed":
            identity = validate_managed(archive, members, handle)
        elif mode == "manual":
            identity = validate_manual(archive, members, handle)
        else:
            fail(f"unknown archive mode: {mode!r}")
    finally:
        handle.close()
    validate_checksums(archive)
    return identity


def copy_members(
    source: tarfile.TarFile,
    destination: tarfile.TarFile,
    members: dict[str, tarfile.TarInfo],
    replacement: dict[str, bytes] | None = None,
    drop: set[str] | None = None,
    extra: dict[str, bytes] | None = None,
) -> None:
    replacement = replacement or {}
    drop = drop or set()
    for name, member in members.items():
        if name in drop:
            continue
        data = replacement.get(name)
        if data is not None:
            member.size = len(data)
            destination.addfile(member, __import__("io").BytesIO(data))
        elif member.isfile():
            extracted = source.extractfile(member)
            if extracted is None:
                fail(f"cannot read {name} while mutating archive")
            destination.addfile(member, extracted)
        else:
            destination.addfile(member)
    for name, data in (extra or {}).items():
        info = tarfile.TarInfo(name)
        info.size = len(data)
        info.mode = 0o644
        destination.addfile(info, __import__("io").BytesIO(data))


def mutate(source: Path, destination: Path, kind: str, mode: str) -> None:
    handle, members = open_archive(source)
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tarfile.open(destination, "w:gz") as output:
        if kind == "missing-release":
            prefix = next(iter({name.split("/", 1)[0] for name in members}))
            drop = {"release.json"} if mode == "managed" else {f"{prefix}/release.json"}
            copy_members(handle, output, members, drop=drop)
        elif kind == "missing-controller":
            prefix = next(iter({name.split("/", 1)[0] for name in members}))
            copy_members(handle, output, members, drop={f"{prefix}/bin/zerodeckctl"})
        elif kind == "missing-component":
            drop = {"frontend"} if mode == "managed" else set()
            if mode == "manual":
                prefix = next(iter({name.split("/", 1)[0] for name in members}))
                drop = {
                    name
                    for name in members
                    if name == f"{prefix}/frontend" or name.startswith(f"{prefix}/frontend/")
                }
            copy_members(handle, output, members, drop=drop)
        elif kind == "mixed-wrapper":
            extra = {"unexpected-wrapper/release.json": b'{"schema":1,"build_id":"x","version":"1.0.0"}\n'}
            copy_members(handle, output, members, extra=extra)
        elif kind == "build-id-mismatch":
            prefix = "" if mode == "managed" else next(iter({name.split("/", 1)[0] for name in members})) + "/"
            name = f"{prefix}release.json"
            document = read_release_json(source, members, handle, prefix=prefix)
            document["build_id"] = "tampered-build-id"
            copy_members(
                handle,
                output,
                members,
                replacement={name: json.dumps(document, sort_keys=True).encode() + b"\n"},
            )
        elif kind == "link":
            prefix = "" if mode == "managed" else next(iter({name.split("/", 1)[0] for name in members})) + "/"
            copy_members(handle, output, members)
            info = tarfile.TarInfo(f"{prefix}evil-link")
            info.type = tarfile.SYMTYPE
            info.linkname = "/etc/passwd"
            output.addfile(info)
        elif kind == "device":
            prefix = "" if mode == "managed" else next(iter({name.split("/", 1)[0] for name in members})) + "/"
            copy_members(handle, output, members)
            info = tarfile.TarInfo(f"{prefix}evil-device")
            info.type = tarfile.CHRTYPE
            info.devmajor = 1
            info.devminor = 3
            output.addfile(info)
        elif kind == "duplicate":
            prefix = "" if mode == "managed" else next(iter({name.split("/", 1)[0] for name in members})) + "/"
            copy_members(handle, output, members)
            info = tarfile.TarInfo(f"{prefix}release.json")
            data = b'{"schema":1,"build_id":"duplicate","version":"0.1.0"}\n'
            info.size = len(data)
            info.mode = 0o644
            output.addfile(info, __import__("io").BytesIO(data))
        elif kind == "parent":
            prefix = "" if mode == "managed" else next(iter({name.split("/", 1)[0] for name in members})) + "/"
            copy_members(handle, output, members)
            info = tarfile.TarInfo(f"{prefix}../escape")
            info.size = 0
            info.mode = 0o644
            output.addfile(info, __import__("io").BytesIO(b""))
        else:
            fail(f"unknown mutation: {kind}")
    handle.close()


def main(argv: list[str]) -> int:
    if len(argv) < 3:
        print("usage: archive.py validate <archive> <managed|manual>", file=sys.stderr)
        print("       archive.py identity <archive> <managed|manual>", file=sys.stderr)
        print("       archive.py mutate <source> <destination> <kind> <mode>", file=sys.stderr)
        return 2
    command = argv[1]
    try:
        if command == "validate":
            archive = Path(argv[2]).resolve()
            if not archive.is_file():
                fail(f"archive does not exist: {archive}")
            version, build_id = validate(archive, argv[3])
            print(f"release archive valid: {archive.name} version={version} build_id={build_id}")
            return 0
        if command == "identity":
            archive = Path(argv[2]).resolve()
            handle, members = open_archive(archive)
            try:
                prefix = ""
                if argv[3] == "manual":
                    wrapper = next(iter({entry.split("/", 1)[0] for entry in members}))
                    prefix = f"{wrapper}/"
                document = read_release_json(archive, members, handle, prefix=prefix)
            finally:
                handle.close()
            print(document["version"])
            print(document["build_id"])
            return 0
        if command == "mutate":
            mutate(Path(argv[2]).resolve(), Path(argv[3]).resolve(), argv[4], argv[5])
            return 0
    except InvalidArchive as error:
        print(f"release archive invalid: {error}", file=sys.stderr)
        return 1
    print(f"unknown command: {command}", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
