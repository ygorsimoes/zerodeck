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

## Dispatch contract for the external TUF-on-CI repository

`promote-tuf.yml` sends one `repository_dispatch` to `vars.TUF_ON_CI_REPOSITORY` using
`secrets.TUF_PROMOTION_TOKEN` (a token that can create dispatch events on that repository only).
GitHub allows at most 10 top-level `client_payload` properties, so the whole request is nested:

```json
{
  "event_type": "zerodeck-loader-release",
  "client_payload": { "request": { "schema": 1, "tag": "v0.1.0", "...": "..." } }
}
```

`request` fields (schema `1`; the same document is attached to the run as `promotion-request.json`):

| Field | Meaning |
|---|---|
| `tag`, `version`, `build_id` | release tag, SemVer and the daemon's embedded build ID |
| `channel` | `stable` or `testing` |
| `min_updater` | minimum `zerodeckctl` version allowed to activate the target |
| `target` | target path, `linux-x86_64/zerodeck-<version>.tar.gz` |
| `sha256`, `length` | digest and size of the published managed archive |
| `source_repository`, `source_sha`, `source_ref`, `signer_workflow` | the provenance identity the archive was attested with |
| `source_release`, `source_release_immutable` | URL of the immutable GitHub release |
| `stable_evidence` | repo-relative evidence record for `stable`, otherwise `null` |
| `promotion_source_sha`, `promotion_source_ref`, `tuf_repository` | the commit the promotion ran from and the receiving repository |

The receiver must, before proposing a signing change:

1. download the managed archive from `source_release` and recompute `sha256`/`length`;
2. check `gh attestation verify` against `source_repository`, `signer_workflow`, `source_sha` and
   `source_ref`;
3. add the `target` entry with `custom.{channel,version,build_id,min_updater}` as in
   `loader-target.template.json`, and open the TUF-on-CI signing change for the `targets` role.

It must treat `request` as untrusted input until steps 1 and 2 succeed. A dispatch never signs
anything; signing stays with the offline `targets` key holders.
