# TUF target shape

This directory contains no keys and no signed production metadata. TUF root/targets signing and
metadata verification happen in the external TUF-on-CI repository, under separate authority from
the GitHub release job.

`loader-target.template.json` records the signed `targets.json` entry ZeroDeck clients expect:

- target path: `linux-x86_64/zerodeck-<version>.tar.gz` (the managed archive bytes, renamed);
- `length` and `hashes.sha256`: the whole managed archive, which is also the generation identity;
- `custom.channel`: `stable` or `testing`;
- `custom.version`: the same SemVer as the target name and `release.json`;
- `custom.build_id`: the daemon's embedded build ID and `release.json` build ID;
- `custom.min_updater`: the minimum `zerodeckctl` version allowed to activate the target.

`deploy/update/promote.sh` verifies the already-published archive digest, `release.json`, channel
and GitHub build provenance (exact repository, signer workflow, source commit/ref and artifact
digest), then prints the filled descriptor for the external signer. It deliberately does **not**
parse or count signatures; the external TUF-on-CI repository runs a real TUF client to verify the
signed root/targets/snapshot/timestamp chain, expiry, version monotonicity and the exact target
entry before serving it.

Do not commit private keys, unsigned `targets.json`, locally fabricated signature records, or
locally rebuilt archives here. The release workflow only dispatches a promotion request with the
published digest and provenance identity; it never signs metadata or rewrites published bytes.
