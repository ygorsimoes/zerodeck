#!/usr/bin/env bash
# Run the updater VM gate against a pinned Arch Linux cloud image (systemd, SSH, passwordless sudo).
#
#   deploy/update/tests/vm-ci.sh <dist-dir> <tag> <candidate-managed-archive>
#
# Needs qemu-system-x86_64, qemu-img, cloud-localds (cloud-image-utils), curl, ssh-keygen and the
# archives written by `just dist <tag>` plus a newer managed archive from `just build-vm-candidate`. The image digest is pinned here and verified before use.
# This is a generic systemd VM, not SteamOS: it proves update, rollback and crash recovery, while
# SteamOS-specific persistence is recorded separately in COMPATIBILITY.md.
set -euo pipefail

IMAGE_URL="https://geo.mirror.pkgbuild.com/images/v20261115.625457/Arch-Linux-x86_64-cloudimg-20261115.625457.qcow2"
IMAGE_SHA256="dbd02830f6eb0fe07ef6e5edaddbae8b577ce4296d9e79118904704cce0a1f60"
GUEST_USER="arch"

[[ $# -eq 3 ]] || {
    echo "usage: vm-ci.sh <dist-dir> <tag> <candidate-managed-archive>" >&2
    exit 2
}
dist="$1"
tag="$2"
candidate="$3"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
work="$(mktemp -d "${TMPDIR:-/tmp}/zerodeck-vm-ci.XXXXXX")"
trap 'rm -rf "${work}"' EXIT

curl --fail --silent --show-error --location --retry 3 --output "${work}/base.qcow2" "${IMAGE_URL}"
printf '%s  %s\n' "${IMAGE_SHA256}" "${work}/base.qcow2" | sha256sum --check --status || {
    echo "error: the downloaded Arch image does not match its pinned SHA-256" >&2
    exit 1
}

ssh-keygen -q -t ed25519 -N '' -f "${work}/key"
cat >"${work}/user-data" <<YAML
#cloud-config
ssh_authorized_keys:
  - $(cat "${work}/key.pub")
YAML
printf 'instance-id: zerodeck-vm-ci\nlocal-hostname: zerodeck-vm\n' >"${work}/meta-data"
cloud-localds "${work}/seed.img" "${work}/user-data" "${work}/meta-data"

exec bash "${here}/vm.sh" \
    --image "${work}/base.qcow2" --image-sha256 "${IMAGE_SHA256}" \
    --ssh-key "${work}/key" --ssh-user "${GUEST_USER}" \
    --manual "${dist}/zerodeck-${tag}-linux-x86_64.tar.gz" \
    --managed "${dist}/zerodeck-${tag}-linux-x86_64.managed.tar.gz" \
    --candidate "${candidate}" \
    --accel kvm \
    --qemu-arg -drive --qemu-arg "file=${work}/seed.img,if=virtio,format=raw,readonly=on"
