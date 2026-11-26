# TUF target shape

This directory contains no keys and no signed production metadata. TUF root/targets signing happens
in the external TUF-on-CI repository, under separate authority from the GitHub release job.

`loader-target.template.json` records the signed `targets.json` entry ZeroDeck clients expect:

- target path: `linux-x86_64/zerodeck-<version>.tar.gz` (the managed archive bytes, renamed);
- `length` and `hashes.sha256`: the whole managed archive, which is also the generation identity;
- `custom.channel`: `stable` or `testing`;
- `custom.version`: the same SemVer as the target name and `release.json`;
- `custom.build_id`: the daemon's embedded build ID and `release.json` build ID;
- `custom.min_updater`: the minimum `zerodeckctl` version allowed to activate the target.

`deploy/update/promote.sh --verify-only ...` prints a filled descriptor for the external signer.
The full `promote.sh` mode refuses to stage anything unless the root, targets, snapshot and
timestamp metadata are present, unexpired, signed by roles authorized in the out-of-band trusted
root, and the targets entry matches the already-published archive digest and length exactly.

Do not commit private keys, unsigned `targets.json`, or locally rebuilt archives here. The release
workflow only dispatches a promotion request; it never signs metadata or rewrites published bytes.
