# ZeroDeck loader release, publication and TUF promotion runbook

This runbook is the operational contract for `zerodeckctl` releases. It covers the two release
assets, immutable GitHub publication, the separate TUF authorization boundary, first-install
bootstrap, conversion/import, and role recovery. Version numbers themselves belong to Release
Please; never edit `Cargo.toml`, `.release-please-manifest.json`, `hosts/python/pyproject.toml`,
`frontend/package.json`, or the `Dockerfile` release markers by hand.

## Assets and identities

Every release publishes two assets built from the same runtime image digest:

| Asset | Layout | Purpose |
|---|---|---|
| `zerodeck-<tag>-linux-x86_64.tar.gz` | one `zerodeck-<tag>-linux-x86_64/` wrapper with `bin/zerodeckd`, `bin/zerodeckctl`, `bootstrap.sh`, `release.json`, `python/`, `frontend/`, `systemd/`, `docs/`, `LICENSE`, `README.md` | manual/diagnostic install and bootstrap controller payload |
| `zerodeck-<tag>-linux-x86_64.managed.tar.gz` | archive root exactly `release.json`, `bin/zerodeckd`, `python/`, `frontend/` | TUF generation payload |

`release.json` is schema `1` and contains the workspace SemVer version and the non-empty
`ZERODECK_BUILD_ID`, which release automation sets to `<tag>-<commit-sha>`. The daemon refuses to
serve a generation whose manifest build ID or version differs from its compiled values. The
managed archive's SHA-256 is the generation identity; TUF target metadata covers that digest and
length separately. The signed target path is always
`linux-x86_64/zerodeck-<version>.tar.gz`, regardless of the uploaded asset name.

`just test-release-package <archive>` validates the exact archive bytes: safe tar members, no
symlinks, required files, no wrapper in the managed archive, release identity, executable modes,
checksum coverage, and the negative/missing-component and publication-retry scenarios. It is the
same test release automation runs before upload.

## Publication order

1. **Release Please creates a draft.** `release-please-config.json` sets `"draft": true`. It still
   owns versions, the tag, and the changelog.
2. **Build from the exact commit.** The release workflow checks out the tag and verifies
   `git rev-parse refs/tags/<tag>` equals the checked-out `HEAD`, then sets
   `ZERODECK_BUILD_ID=<tag>-<sha>`. The Dockerfile receives
   `ZERODECK_REQUIRE_BUILD_ID=1`, so a release build without an explicit identity fails.
3. **Push the image, package by digest.** The runtime image is pushed once and every later step
   uses `ghcr.io/<repo>@sha256:<digest>`, never a mutable tag. `just dist` extracts both assets from
   that image and writes `SHA256SUMS.txt`.
4. **Validate before upload.** Both archives pass `just test-release-package`; both archives are
   attested with GitHub build provenance.
5. **Upload and download-verify the draft.** `deploy/update/publish-release.sh` refuses a published
   release, compares any existing draft asset by downloading it, uploads a missing/mismatched draft
   asset, downloads every asset again, checks `SHA256SUMS.txt`, and rejects unexpected assets.
6. **Publish the immutable release.** Only after step 5 does the workflow run
   `publish-release.sh --publish` (`gh release edit --draft=false`). If the upload fails, the draft
   remains unpublished and can be retried against the same tag. A failed retry never changes a
   published digest; once published, the script only verifies and refuses mismatches.
7. **Promote the already-published bytes, separately.** `.github/workflows/promote-tuf.yml` starts
   on `release: published`, downloads the exact release assets, verifies `sha256sum -c`, validates
   both archives, verifies GitHub build provenance for the managed archive, and writes a promotion
   request. It then dispatches that request to the external TUF-on-CI repository. The release build
   job contains no TUF signing key and never rebuilds during promotion.

### Manual retry

Re-run the release workflow for the same draft tag:

```bash
gh workflow run release-please.yml -f tag=v0.1.0 \
  -f sha=<the exact 40-character release commit> -f publish=true
```

Release Please drafts do not create a git tag until publication, so a manual retry must name the
exact commit if the draft API does not expose a 40-character `target_commitish`. The workflow
refuses if the release is already published. To verify a draft without publishing, pass
`-f publish=false`. Retrying rebuilds the assets, but `publish-release.sh` only replaces assets while
the release is still a draft; a published release is never rewritten.

### Manual verification of a draft

```bash
gh release view v0.1.0 --json isDraft,tagName,assets
gh release download v0.1.0 --pattern 'zerodeck-v0.1.0-linux-x86_64*' --dir verify
gh release download v0.1.0 --pattern SHA256SUMS.txt --dir verify
(cd verify && sha256sum -c SHA256SUMS.txt)
just test-release-package verify/zerodeck-v0.1.0-linux-x86_64.tar.gz
just test-release-package verify/zerodeck-v0.1.0-linux-x86_64.managed.tar.gz
gh attestation verify verify/zerodeck-v0.1.0-linux-x86_64.managed.tar.gz --repo OWNER/zerodeck
```

Do not publish until every command above succeeds and the source commit is the intended one.

## TUF signing roles and owners

The TUF root and all signatures live in the external TUF-on-CI repository, never in this
repository. This repository contains only the target template (`deploy/update/tuf/`), the
validation/promotion script (`deploy/update/promote.sh`), and the request workflow.

| Role | Authority | Typical owner | Threshold | Recovery |
|---|---|---|---|---|
| `root` | trust anchor; delegates every other role | offline security owner plus an independent recovery custodian | 2 of 3 offline keys | restore from the offline root backup, issue a new root version signed by the old quorum, rotate every delegated key, publish the root chain; never re-use a compromised key |
| `targets` | authorizes loader archives and build identity | release maintainer plus an independent reviewer | 2 of 2 (or 2 of 3) | sign a new targets version from a clean offline machine; remove the compromised key through a root update; never edit a published target digest |
| `snapshot` | pins the current targets version | CI automation owner | 1 of 1 online/HSM-backed | rotate the snapshot key via a root update, re-sign snapshot and timestamp, publish in order |
| `timestamp` | short-lived freshness marker | CI automation owner | 1 of 1 online/HSM-backed | rotate the timestamp key, re-sign immediately, publish; clients fail closed after expiry until fresh metadata is available |

Root key custody is deliberately separate from the GitHub release job. A release engineer can
publish a GitHub release without being able to authorize it in TUF; a TUF signer can authorize only
bytes whose published digest and provenance have already been verified.

## Promotion boundary

The request workflow needs these repository settings:

- variable `TUF_ON_CI_REPOSITORY` = `owner/tuf-repository`;
- variable `TUF_MIN_UPDATER` (optional, default `0.1.0`);
- secret `TUF_PROMOTION_TOKEN` with permission to dispatch workflows/events in that repository.

If either the repository or token is absent, the workflow fails instead of claiming success. The
external TUF repository downloads the immutable release assets, runs
`deploy/update/promote.sh` from the released tag, and signs the resulting target metadata.

Full promotion requires all of the following; any missing item is a hard failure:

```bash
bash deploy/update/promote.sh \
  --archive dist/zerodeck-v0.1.0-linux-x86_64.managed.tar.gz \
  --sha256 <published-managed-sha256> \
  --tag v0.1.0 --version 0.1.0 --build-id v0.1.0-<commit-sha> \
  --channel stable --min-updater 0.1.0 \
  --trusted-root root.json --trusted-root-sha256 <out-of-band-root-sha256> \
  --attestation attestation.json \
  --metadata-dir signed-metadata/ --output staged-tuf/
```

`signed-metadata/` must contain `root.json`, `targets.json`, `snapshot.json` and
`timestamp.json`. `promote.sh` checks that they are present, unexpired, signed by key IDs with the
required threshold in the trusted root, and that the targets entry names the exact target path with
the published SHA-256, length, channel, version, build ID and minimum updater version. It copies
the already-published archive into `staged-tuf/targets/linux-x86_64/zerodeck-<version>.tar.gz` and
the signed metadata into `staged-tuf/metadata/`, and refuses to overwrite a different staged digest.
It never creates a signature.

`--verify-only` prints the target descriptor without requiring signed metadata; it is how the
request workflow renders the external signer's input. Stable clients continue to resolve the
previous target until the new `targets.json`, `snapshot.json`, and `timestamp.json` are signed and
served together with the exact staged bytes.

Publish metadata in this order: root update (only during rotation), targets, snapshot, timestamp.
A failure before timestamp leaves clients on the previous, still-valid authorization. An expired
or superseded candidate never activates, even if the archive is already staged locally.

## First install bootstrap

Obtain the two archives, `SHA256SUMS.txt`, the current TUF root, and the root SHA-256 from channels
that do not share the download. Compare the fingerprint out of band, then run the packaged helper
as root:

```bash
sudo ./bootstrap.sh \
  --package zerodeck-v0.1.0-linux-x86_64.tar.gz \
  --sha256 <manual-sha256-from-trusted-channel> \
  --generation zerodeck-v0.1.0-linux-x86_64.managed.tar.gz \
  --generation-sha256 <managed-sha256-from-trusted-channel> \
  --trusted-root root.json \
  --trusted-root-sha256 <out-of-band-root-sha256> \
  --metadata-url https://updates.example/loader/metadata/ \
  --targets-url https://updates.example/loader/targets/ \
  --start
```

Before extracting anything, the helper verifies all three SHA-256 digests and parses the root. It
then validates both archives and refuses if:

- `decky.service` or `plugin_loader.service` is active (Decky files are never stopped or rewritten);
- a ZeroDeck install exists and `--convert-existing-zerodeck` was not supplied;
- `zerodeck.service` is running and `--start` was not supplied;
- `zerodeckctl bootstrap --check` reports a pending activation, an unconsumed apply request,
  corrupt state, or an unhealthy previous generation;
- the manual and managed archives disagree on version/build ID, a required component is missing, or
  a path/mode is unsafe.

The helper installs the verified generation under `/var/lib/zerodeck/generations/<sha256>`, the
controller at `/usr/local/lib/zerodeck/bin/zerodeckctl`, units in `/etc/systemd/system`, the
trusted root and update endpoints in `/etc/zerodeck/`, and initializes `state/update.json` through
`zerodeckctl bootstrap`. It does not create or modify `/home/deck/homebrew` during a clean install.
Without `--start` it installs units but neither enables nor starts anything.

### Conversion of an existing ZeroDeck install

```bash
sudo ./bootstrap.sh ... --convert-existing-zerodeck --start
```

Before replacing anything, the helper snapshots the controller, units, unit drop-ins,
`/etc/zerodeck` files, `current`, and update/apply state under
`/var/lib/zerodeck/backup/bootstrap-<timestamp>-<pid>/`. The previous generation is retained as the
rollback target. If start/readiness fails, the old controller, units, drop-ins, config, `current`
and state are restored and the previous service is restarted when it was running. Data under
`/home/deck/homebrew` is never rewritten. Keep the backup until the managed generation has been
confirmed on the deck.

A pending activation blocks controller replacement: the helper uses the new controller's
`bootstrap --check` before touching the installed controller.

### Importing an inactive Decky tree

```bash
sudo ./bootstrap.sh ... --import-inactive-decky /path/to/inactive/homebrew --start
```

The source must not have an active Decky service, and the target data directory must be empty; the
helper copies with `cp -a` without changing ownership and never writes into an existing plugin
tree. It records `state/import.json` with `root_plugins: "blocked"`. Imported code is not promoted
into a root-controlled tree, is not chowned to root, and any root plugin must be installed
explicitly with a verified package before it can be trusted. There is no implicit trust-on-import.

### Recovery of a failed conversion

1. Inspect `zerodeckctl status` and the backup `MANIFEST`.
2. The helper normally restores automatically. If interrupted before it can, copy the backed-up
   controller, units, drop-ins, config, `current`, and `state/update.json` back from the backup,
   run `systemctl daemon-reload`, and start the previous unit only if it was previously running.
3. Keep the orphaned new generation directory for diagnosis; `current` and state select the old
   generation. Remove it only after the old service is confirmed.

## Updater upgrades

`min_updater` in the signed target prevents an old controller from activating a newer generation.
To replace the controller itself, use the verified bootstrap helper with
`--convert-existing-zerodeck`; it refuses during a pending activation or when the persisted state
cannot be read. Do not replace `/usr/local/lib/zerodeck/bin/zerodeckctl` by hand while an apply is
pending. An offline rollback uses the retained generation and does not consult TUF or the network.

```bash
sudo zerodeckctl update --check-only
sudo zerodeckctl update
sudo zerodeckctl upgrade            # stopped service: add --start for an explicit first start
sudo zerodeckctl status
sudo zerodeckctl rollback           # offline, retained confirmed generation
sudo zerodeckctl unquarantine <generation-digest>  # only after review
```

## Drills and staging evidence

Run the following in a fork or staging repository; never use production signing keys for a drill.

- **Staging release:** create a draft, build from an exact commit, package by image digest, run the
  package test, upload/verify twice, publish once. Record the asset digests and attestation output.
- **Retry:** inject an upload failure (the local `deploy/update/tests/publish.sh` does this), confirm
  the draft remains unpublished, retry the same tag, and confirm the published digest never changes.
- **Expiry:** use short-lived staging metadata; confirm `zerodeckctl update` fails closed after
  expiry while `rollback` still works offline from a retained generation.
- **Rotation:** add a new root key and re-sign root/targets/snapshot/timestamp in staging; confirm an
  installed client accepts the root chain update, keeps its metadata history, and rejects an old or
  unknown root. Record the out-of-band root fingerprint transition.
- **Promotion failure:** leave the signed targets entry absent or mismatched; confirm
  `promote.sh` refuses and Stable still resolves to the previous target digest.

The local shell tests exercise the refusal/retry/expiry/rotation boundary logic without keys:
`deploy/update/tests/{bootstrap,publish,promotion}.sh`. Hardware evidence is recorded in
`deploy/update/COMPATIBILITY.md`.
