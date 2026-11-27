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
2. **Build from the exact commit.** Draft releases have no git tag. The release workflow builds from
   Release Please's `sha` output (or `GITHUB_SHA` on a push), checks out that commit, and verifies
   `HEAD` matches it. It sets `ZERODECK_BUILD_ID=<tag>-<sha>`; the Dockerfile receives
   `ZERODECK_REQUIRE_BUILD_ID=1`, so a release build without an explicit identity fails.
3. **Push the image, package by digest.** The runtime image is pushed once and every later step
   uses `ghcr.io/<repo>@sha256:<digest>`, never a mutable tag. `just dist` extracts both assets from
   that image and writes `SHA256SUMS.txt`.
4. **Validate before upload.** Both archives pass `just test-release-package`; both archives are
   attested with GitHub build provenance.
5. **Upload and download-verify the draft by release ID.** `deploy/update/publish-release.sh` takes
   the numeric release ID (from Release Please's `upload_url` or the releases list), refuses a
   published release, re-checks draft status before every asset mutation, uploads without a
   clobber, downloads every asset again, checks `SHA256SUMS.txt`, and rejects unexpected assets.
   Never use `releases/tags/<tag>` for a draft.
6. **Publish the immutable release.** Only after step 5 and an explicit `gh attestation verify` for
   both archives does the workflow publish that release ID. If the upload fails, the draft remains
   unpublished and can be retried by the same release ID. A failed retry never changes a published
   digest; once published, the script only verifies and refuses mismatches. Workflow concurrency
   plus GitHub immutable releases are required; a human publishing at the same instant is an
   unavoidable external race. `publish-release.sh` reads
   `GET /repos/<owner>/<repo>/immutable-releases` (admin read) before any mutation, requires
   `enabled: true`, and re-checks the published release's `immutable` field afterward. Configure
   `RELEASE_PLEASE_TOKEN` with admin read (or grant the release token that permission); the script
   fails clearly rather than publishing a mutable release.
7. **Promote the already-published bytes, manually and separately.** There is deliberately no
   `release: published` trigger. Publishing a GitHub release never authorizes Stable. A maintainer
   dispatches `.github/workflows/promote-tuf.yml` from `main`; the job uses the protected
   `production` environment (required reviewers), checks out the default branch (never the tag's
   scripts), requires the release to be published **and** immutable, verifies that the asset set is
   exactly the two archives plus `SHA256SUMS.txt`, downloads and re-verifies them, validates both
   archives, verifies GitHub build provenance for the managed archive, and writes a promotion
   request. Only then does it dispatch that request to the external TUF-on-CI repository. The
   release build job contains no TUF signing key and never rebuilds during promotion.

   For `channel=stable`, the job additionally runs
   `deploy/update/verify-stable-evidence.sh` against the reviewed default-branch record
   `deploy/update/evidence/<tag>.md`. That record must contain no `PENDING` values, the exact release
   tag/version/build ID/managed SHA-256, an operator, an independent reviewer, a date, and the exact
   line `Stable eligible: PASS`. Absent or incomplete evidence fails the job closed; the previous
   Stable target remains authorized. Stable is never promoted automatically.

### Manual retry

Re-run the release workflow for the same draft tag:

```bash
gh workflow run release-please.yml -f tag=v0.1.0 \
  -f release_id=<the numeric draft release ID> -f publish=true
```

Release Please drafts do not create a git tag until publication. A manual retry should provide the
numeric draft release ID; if it is omitted, the workflow finds the draft by `tag_name` in the
releases list (never `releases/tags`). The workflow builds exactly `GITHUB_SHA` and uses
`github.sha`/`github.ref` as the provenance identity, so dispatch the retry from the exact release
commit on `main`; it refuses if the Release Please/tag target differs. It cannot rebuild an
arbitrary input commit. The workflow refuses if the release is already published. To verify a draft
without publishing, pass `-f publish=false`. Retrying rebuilds the assets, but
`publish-release.sh` only replaces assets while the release is still a draft; a published release
is never rewritten.

### Manual verification of a draft

Find the draft's numeric ID from the releases list, then download by ID; drafts have no tag:

```bash
release_id="$(gh api 'repos/OWNER/zerodeck/releases?per_page=100' \
  --jq '.[] | select(.tag_name == "v0.1.0" and .draft) | .id')"
gh api "repos/OWNER/zerodeck/releases/${release_id}" --jq '{id,tag_name,draft,assets}'
gh api "repos/OWNER/zerodeck/releases/${release_id}" \
  --jq '.assets[] | "\(.id)\t\(.name)"' |
while IFS=$'\t' read -r asset_id asset_name; do
  gh api "repos/OWNER/zerodeck/releases/assets/${asset_id}" \
    -H 'Accept: application/octet-stream' >"verify/${asset_name}"
done
just test-release-package verify/<downloaded-archive>
gh attestation verify verify/zerodeck-v0.1.0-linux-x86_64.managed.tar.gz \
  --repo OWNER/zerodeck \
  --signer-workflow OWNER/zerodeck/.github/workflows/release-please.yml \
  --source-digest <release-commit-sha> --source-ref refs/heads/main
```

Do not publish until every command above succeeds and the source commit is the intended one.

## TUF signing roles and owners

The TUF root and all signatures live in the external TUF-on-CI repository, never in this
repository. This repository contains only the target template (`deploy/update/tuf/`), the
published-bytes/provenance verifier (`deploy/update/promote.sh`), and the request workflow. Signed
metadata verification is performed by the external TUF-on-CI client, not by local signature
counting or free-form "verified" records.

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

- environment `production` with required reviewers and a protected `main` branch;
- variable `TUF_ON_CI_REPOSITORY` = `owner/tuf-repository`;
- variable `TUF_MIN_UPDATER` (optional, default `0.1.0`);
- secret `TUF_PROMOTION_TOKEN` with permission to dispatch workflows/events in that repository.

Promote manually after the immutable release and, for Stable, after the hardware evidence record is
merged to `main`:

```bash
# Testing: no hardware evidence required.
gh workflow run promote-tuf.yml -f tag=v0.1.0 -f channel=testing

# Stable: requires the production environment approval and deploy/update/evidence/v0.1.0.md.
gh workflow run promote-tuf.yml -f tag=v0.1.0 -f channel=stable
```

If either the repository or token is absent, the workflow fails instead of claiming success. There
is deliberately **no local metadata acceptance** in this repository: `promote.sh` verifies the exact
published archive and GitHub build provenance, then emits a target descriptor. The external
TUF-on-CI repository is the trust boundary that loads its out-of-band root, verifies the signed
root/targets/snapshot/timestamp metadata with a real TUF client (`tough` or equivalent), checks
expiry, version monotonicity and the snapshot↔targets/timestamp↔snapshot hash chains, and only then
serves the already-published bytes. Do not hand-roll signature parsing or fake "verified" records.

The request workflow runs this check; any missing digest, identity, provenance or channel assertion
is a hard failure:

```bash
bash deploy/update/promote.sh \
  --archive dist/zerodeck-v0.1.0-linux-x86_64.managed.tar.gz \
  --sha256 <published-managed-sha256> \
  --tag v0.1.0 --version 0.1.0 --build-id v0.1.0-<commit-sha> \
  --channel stable --min-updater 0.1.0 \
  --repository OWNER/zerodeck --source-sha <release-commit-sha> \
  --signer-workflow OWNER/zerodeck/.github/workflows/release-please.yml \
  --source-ref refs/heads/main
```

`promote.sh` runs `gh attestation verify` with that exact repository, signer workflow, source
commit, source ref and artifact digest, and rejects prerelease versions on Stable. The external
signer repeats or independently verifies provenance and then validates the signed metadata. Stable
clients continue to resolve the previous target until the new `targets.json`, `snapshot.json`, and
`timestamp.json` are signed and served together with the exact published bytes.

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

The helper first copies the package, generation and trusted root into a root-only work directory
and verifies those copies. No caller-controlled path is re-opened for extraction or execution, so a
file swapped after a hash check cannot be installed. Before extracting anything it also verifies all
three SHA-256 digests and parses the root. It refuses if an install path or any of its ancestors is
a symlink, non-root-owned, or group/other-writable. `/usr/local` is read-only on SteamOS, so no PATH
link is installed unless `--bin-link` asks for one; call `/home/.zerodeck/bin/zerodeckctl` directly. It then validates both
archives and refuses if:

- `decky.service` or `plugin_loader.service` is active (Decky files are never stopped or rewritten);
- a ZeroDeck install exists and `--convert-existing-zerodeck` was not supplied;
- `zerodeck.service` is running and `--start` was not supplied;
- `zerodeckctl bootstrap --check` reports a pending activation, an unconsumed apply request,
  corrupt state, or an unhealthy previous generation;
- the manual and managed archives disagree on version/build ID, a required component is missing, or
  a path/mode is unsafe.

The helper installs the verified generation under `/home/.zerodeck/generations/<sha256>`, the
controller at `/home/.zerodeck/bin/zerodeckctl`, units in `/etc/systemd/system`, the trusted root and
update endpoints in `/etc/zerodeck/`, and (when `/etc/atomic-update.conf.d` exists, as on SteamOS
3.6+) `/etc/atomic-update.conf.d/zerodeck.conf`, which lists the units, their `*.wants` enablement
symlinks and itself so they survive an OS update (`--keep-list-dir` overrides the location), and prepares `state/update.json` through `zerodeckctl bootstrap --prepare`. It
fsyncs the complete generation tree before switching `current`. It does not create or modify
`/home/deck/homebrew` during a clean install. Without `--start` it installs units but neither
enables nor starts anything, and immediately runs `bootstrap --confirm` because no process exists
to probe.

With `--start`, the helper waits for `ActiveState=active` and a non-zero `MainPID`, then polls the
authenticated `/readyz` endpoint (`--ready-url`, default `http://127.0.0.1:1337`) until the
version, build ID, `python_host_ready` and systemd `INVOCATION_ID` all match. Only then does it run
`bootstrap --confirm`. A start, readiness or confirmation failure restores the backup, removes or
restores the controller link, and restarts the previous unit when it was running.

### Conversion of an existing ZeroDeck install

```bash
sudo ./bootstrap.sh ... --convert-existing-zerodeck --start
```

Before replacing anything, the helper snapshots the controller, units, unit drop-ins,
`/etc/zerodeck` files, `current`, and update/apply state under
`/home/.zerodeck/backup/bootstrap-<timestamp>-<pid>/`. The previous generation is retained as the
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

The source must not have an active Decky service, and the helper never writes into
`/home/deck/homebrew`. It copies the tree with `cp -a` into a root-only staging directory
(`/home/.zerodeck/import/decky-<timestamp>-<pid>/`, mode `0700`) and records `state/import.json`
with the staged path, `root_plugins: "blocked"`, `activated: false`, and the expected host data
root. Imported code is not on any plugin search path and cannot be activated implicitly; review the
staged copy and install individual plugins with a verified package before adopting them. The host
data root is left untouched, so the unprivileged plugin process never receives a root-owned
`0700` home. There is no implicit trust-on-import.

### Recovery of a failed conversion

1. Inspect `zerodeckctl status` and the backup `MANIFEST`.
2. The helper normally restores automatically. If interrupted before it can, copy the backed-up
   controller, units, drop-ins, config, `current`, and `state/update.json` back from the backup,
   run `systemctl daemon-reload`, and start the previous unit only if it was previously running.
3. Keep the orphaned new generation directory for diagnosis; `current` and state select the old
   generation. Remove it only after the old service is confirmed.

## Updater upgrades

After a confirmation, recovery or explicit rollback completes under the exclusive lock, the
controller collects generations that are not `current`, `last_confirmed`, `rollback_target`, a
pending candidate, or a currently authorized staged candidate. The active reference is read from
disk, so stale state cannot cause deletion of the running generation.

`min_updater` in the signed target prevents an old controller from activating a newer generation.
To replace the controller itself, use the verified bootstrap helper with
`--convert-existing-zerodeck`; it refuses during a pending activation or when the persisted state
cannot be read. Do not replace `/home/.zerodeck/bin/zerodeckctl` by hand while an apply is
pending. An offline rollback uses the retained generation and does not consult TUF or the network.

```bash
sudo zerodeckctl update --check-only
sudo zerodeckctl update
sudo zerodeckctl upgrade            # stopped service: add --start for an explicit first start
sudo zerodeckctl upgrade --channel testing   # explicit Testing opt-in
sudo zerodeckctl status
sudo zerodeckctl rollback           # offline, retained confirmed generation
sudo zerodeckctl recover            # reconcile an interrupted activation
sudo zerodeckctl recover --discard-request  # clear a stale request when nothing is pending
sudo zerodeckctl unquarantine <generation-digest>  # only after review
```

A terminal pre-intent apply failure (stale request, quarantined candidate, missing generation,
authorization expired/superseded, stopped service without `--start`, or a malformed request)
removes its own request so later upgrades are not wedged. That includes a maintenance lock that
stays held for the whole wait: the request is discarded (the reason is in
`journalctl -u zerodeck-apply.service`) and `sudo zerodeckctl upgrade` must be run again. If a
request remains after an interrupted helper, run `sudo systemctl start zerodeck-apply.service` to
retry it, or
`sudo zerodeckctl recover --discard-request` after confirming no apply is running. The updater
rechecks the persisted selection channel, the signed release channel, the installed version and
`min_updater` under the exclusive lock; Stable refuses a Testing selection, a downgrade, a reused
version with a different digest, or a release that requires a newer controller.

## Drills and staging evidence

Run the following in a fork or staging repository; never use production signing keys for a drill.

- **VM crash gate:** boot a disposable image with `just test-updater-vm <image> <key> <user>
  <manual> <managed> [<next-managed>]` (or call `deploy/update/tests/vm.sh` directly). It verifies
  the image and release digests, installs with the packaged bootstrap, and drives update, rollback,
  hard poweroff, `SIGKILL` of the apply helper, port collision, corrupt rollback, reboot during
  recovery and a second recovery pass. It exits `77` with `skipped: no qemu/image` when the host
  cannot run it; a skip is not a pass. Use `--work-dir` to preserve evidence. `--mode auto`
  converts an existing pre-change installation; `--mode clean|convert` forces the choice. The
  harness self-test (fake QEMU/SSH) runs in `just check`. Update scenarios use a local fixture
  authorization rather than a live TUF repository; TUF fetch/selection is covered by the Rust
  catalog and staging tests. See [`tests/README.md`](tests/README.md).
- **Steam Deck hardware gate:** record every field in
  [`COMPATIBILITY.md`](COMPATIBILITY.md) from observed hardware: exact Steam/SteamOS/build,
  filesystems and read-only root, invocation identity, online update, offline rollback,
  SteamOS-update survival, unchanged plugin inventory in a loader-only upgrade, peak RSS, bytes,
  temporary disk and stop-to-ready/stop-to-recovered timings. Every unexecuted field is
  **PENDING**. Do not mark Stable eligible from a VM run alone.
- **Staging release:** create a draft, build from an exact commit, package by image digest, run the
  package test, upload/verify twice, publish once. Record the asset digests and attestation output.
- **Stable gate:** dispatch `promote-tuf.yml` with `channel=stable` from `main`. Confirm the
  `production` environment requested approval, that the job rejected a missing/PENDING evidence
  record, and that it accepted only the reviewed `deploy/update/evidence/<tag>.md` with
  `Stable eligible: PASS`. A `channel=testing` dispatch must not require hardware evidence.
- **Retry:** inject an upload failure (the local `deploy/update/tests/publish.sh` does this), confirm
  the draft remains unpublished, retry the same release ID from the exact release commit, and
  confirm the published digest never changes.
- **Expiry:** use short-lived staging metadata; confirm the external TUF client and
  `zerodeckctl update` fail closed after expiry while `rollback` still works offline from a retained
  generation.
- **Rotation:** add a new root key and re-sign root/targets/snapshot/timestamp in the external TUF
  repository; confirm an installed client accepts the root chain update, keeps its metadata history,
  and rejects an old or unknown root. Record the out-of-band root fingerprint transition.
- **Promotion failure:** leave the signed targets entry absent or mismatched; confirm the external
  TUF client refuses to serve it and Stable still resolves to the previous target digest. Do not
  accept locally fabricated signature records in this repository.

The local shell tests exercise bootstrap refusal/conversion/import, release-ID upload retry and
provenance/descriptor refusal without keys: `deploy/update/tests/{bootstrap,publish,promotion}.sh`.
Rust catalog tests cover TUF expiry/rotation/selection. The VM gate is
`deploy/update/tests/vm.sh`; its fake-QEMU self-test is part of `just check`. Hardware evidence is
recorded in `deploy/update/COMPATIBILITY.md`, with every unexecuted item **PENDING**.
