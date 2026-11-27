# Updater VM gate

`vm.sh` is the disposable Linux/systemd gate for the managed loader. It boots a caller-provided
image under QEMU with a qcow2 overlay, installs the packaged release with the shipped
`bootstrap.sh`, and drives the production controller and systemd units over SSH.

The source image is never opened for writing. Every run uses a new overlay and QEMU is always
removed by an `EXIT` trap, including on failure or interrupt.

In CI the `vm` job runs it on KVM against a pinned Arch Linux cloud image
(`just test-updater-vm-ci dist <tag> <candidate>`, see `vm-ci.sh`). That is a generic systemd VM, not SteamOS;
SteamOS-specific behavior is recorded in [`../COMPATIBILITY.md`](../COMPATIBILITY.md). The harness
itself is exercised without QEMU by `vm.sh --self-test`.

## Requirements

- `bash`, `python3`, `qemu-system-x86_64`, `qemu-img`, `ssh`, `scp` on the host.
- An x86_64 Linux image that boots PID 1 as systemd and starts an SSH server. A generic cloud image
  with a known login key is sufficient; UEFI images may need `--firmware` or `--qemu-arg`.
- A private key accepted by that image. The login user must be root or have passwordless
  `sudo -n`.
- Release archives built by `just dist` (or equivalent): the manual archive and the managed
  generation archive must share the same version and build ID.
- For a real cross-version update, a managed archive from a genuinely newer release. Without it the
  harness repacks the installed generation into a distinct digest with the same release identity;
  that exercises apply, verification, rollback and crash recovery, but is **not** a version upgrade.
  The harness records this distinction in its log.

KVM is optional. Without it QEMU uses TCG and is significantly slower; allow several minutes per
boot and use `--scenario-timeout`.

## Usage

```bash
just test-updater-vm \
  /var/lib/libvirt/images/steamos-test.qcow2 \
  ~/.ssh/zerodeck-vm \
  deck \
  dist/zerodeck-v0.1.0-linux-x86_64.tar.gz \
  dist/zerodeck-v0.1.0-linux-x86_64.managed.tar.gz
```

The same command with the next release's managed archive:

```bash
just test-updater-vm \
  /var/lib/libvirt/images/steamos-test.qcow2 \
  ~/.ssh/zerodeck-vm \
  deck \
  dist/zerodeck-v0.1.0-linux-x86_64.tar.gz \
  dist/zerodeck-v0.1.0-linux-x86_64.managed.tar.gz \
  dist/zerodeck-v0.2.0-linux-x86_64.managed.tar.gz
```

For a pre-change image with an existing loader, add `--mode convert` (or leave the default
`--mode auto`). The conversion path retains the previous generation as the rollback target.

Run the script directly for image digests, checksum manifests, firmware, memory, port and logging
options:

```bash
bash deploy/update/tests/vm.sh \
  --image /images/steamos-test.qcow2 \
  --image-sha256 <expected-sha256> \
  --ssh-key ~/.ssh/zerodeck-vm --ssh-user deck \
  --manual dist/zerodeck-v0.1.0-linux-x86_64.tar.gz \
  --managed dist/zerodeck-v0.1.0-linux-x86_64.managed.tar.gz \
  --sha256sums dist/SHA256SUMS.txt \
  --candidate dist/zerodeck-v0.2.0-linux-x86_64.managed.tar.gz \
  --work-dir .vm-evidence/2026-11-26
```

`--help` lists every option. Important ones:

| Option | Meaning |
|---|---|
| `--image-sha256` | Fail before QEMU if the image does not match the supplied digest. |
| `--sha256sums` | Check the release archives against the adjacent `SHA256SUMS.txt`. |
| `--manual-sha256`, `--managed-sha256`, `--candidate-sha256` | Explicit trusted digests; they override/complete the manifest. |
| `--mode auto\|clean\|convert` | Auto-detect an existing ZeroDeck installation and pass `--convert-existing-zerodeck`, or force clean/convert. Use `convert` for a pre-change image with an old loader already installed. |
| `--work-dir` | Keep the overlay, console log, QEMU log and scenario output in this directory. |
| `--accel auto\|tcg\|kvm\|hvf` | Default picks KVM only when `/dev/kvm` is writable, otherwise TCG. |
| `--firmware` / `--qemu-arg` | UEFI/BIOS or other machine-specific QEMU setup. |
| `--boot-timeout`, `--scenario-timeout` | Boot and recovery waits in seconds. |
| `--keep-work` | Keep the default temporary directory instead of deleting it. |

The script computes release digests itself and passes them to `bootstrap.sh`; when a checksum
manifest or explicit digest is supplied, a mismatch fails closed. The source image digest is
re-checked in the cleanup trap on success and failure, and must be unchanged.

For a pre-change systemd image that already has an old ZeroDeck controller, units or
`/home/.zerodeck/current`, `--mode auto` (the default) detects that installation and passes
`--convert-existing-zerodeck`; `--mode convert` forces that path. A clean image uses `--mode clean`.

## Scenarios

Each scenario verifies the active `current` digest, the running `zerodeckd` executable under
`/home/.zerodeck/generations/<digest>/bin/`, `/readyz` version/build ID, a non-empty systemd
`InvocationID` (and whether it changed for scenarios that must restart the service), root ownership
of `current`, `last_confirmed`, absence of a pending transaction, and `zerodeck.service` state.

| Scenario | What is proven |
|---|---|
| `bootstrap` | Clean or converted install with the packaged bootstrap and systemd readiness. |
| `port-collision` | A daemon that cannot bind 1337 does not start; releasing the port recovers. |
| `successful-update` | Staged candidate applies through `zerodeck-apply.service`; ready identity and `current` switch; rollback target is retained. With a real `--candidate` this is a cross-version update. |
| `corrupt-rollback` | A missing generation directory and a generation without `release.json` are both refused before the service is stopped; restoring the generation allows an offline rollback. |
| `unhealthy-candidate` | A candidate that passes layout validation but cannot serve is quarantined and the previous generation is restored. |
| `hard-poweroff-during-apply` | QEMU `SIGKILL` while the apply transaction is `verifying`; reboot recovery restores the previous generation. |
| `sigkill-apply-helper` | `SIGKILL` of the independent `zerodeckctl apply` process; `ExecStopPost` recovery restores the previous generation. |
| `reboot-during-recovery` | A guest reboot while `zerodeck-recovery.service` is running; the next boot reconciles disk state. |
| `second-recovery` | Repeated boot/after-apply recovery passes do not change `current` or persisted state. |

The reboot scenario gives `zerodeck-recovery.service` a 20 s `ExecStartPre` delay so the watcher can
observe it `activating`, and overrides the packaged 60 s `TimeoutStartSec` to 180 s: systemd applies
the start timeout to `ExecStartPre` as well as `ExecStart`, so a delay equal to the timeout would
make `recover-boot` never run.

The update scenarios use a local fixture authorization in `state/catalog.json` rather than a live
channel. Channel signature, expiry, replay and selection are covered by the Rust catalog tests, and
channel signing by `sign-channel.sh`. The VM gate focuses on privileged systemd activation and crash recovery.
The harness never fetches a real release, never publishes anything, and never contacts Steam.

## Exit codes and evidence

- `0` — every scenario passed.
- `1` — setup or a scenario failed.
- `77` — prerequisites are missing. The message starts with `skipped:` and includes
  `no qemu/image`. A skip is **not** a pass and must not be copied into release evidence as one.

With `--work-dir`, preserve `qemu.log`, `console.log`, `qemu-command.log` and the scenario output as
evidence. Do not commit private keys or images. The harness does not log the auth token or key
material; guest commands and QEMU arguments are recorded without secrets.

## Self-test

```bash
just test-updater-vm-selftest
# or
bash deploy/update/tests/vm.sh --self-test
```

This uses fake `qemu-system-x86_64`, `qemu-img`, `ssh` and `scp` binaries. It proves that a missing
QEMU exits `77` with `skipped: no qemu/image`, a wrong image digest fails before QEMU starts, the
overlay drive and SSH forward are constructed, a mismatching guest identity propagates as a
failure, QEMU is killed, the temporary work directory is removed, and the source image is
byte-identical afterwards. It is part of `just check`/`just ci` through `_release-scripts`.

## Steam Deck checklist

The VM gate cannot show SteamOS-specific behavior. Record the exact Steam/SteamOS/build identity,
filesystems, invocation identity, on-device update and offline rollback, SteamOS-update survival
with a read-only root, unchanged plugin inventory for a loader-only upgrade, peak RSS, bytes,
temporary disk and stop-to-ready/stop-to-recovered timings from observed hardware in
[`../COMPATIBILITY.md`](../COMPATIBILITY.md). Unexecuted items stay **PENDING**.
