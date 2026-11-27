# ZeroDeck loader release, publication and channel runbook

This runbook is the operational contract for `zerodeckctl` releases. It covers the two release
assets, immutable GitHub publication, the automatically signed update channels, first-install
bootstrap, and conversion/import. Version numbers themselves belong to Release
Please; never edit `Cargo.toml`, `.release-please-manifest.json`, `hosts/python/pyproject.toml`,
`frontend/package.json`, or the `Dockerfile` release markers by hand.

## Assets and identities

Every release publishes two assets built from the same runtime image digest:

| Asset | Layout | Purpose |
|---|---|---|
| `zerodeck-<tag>-linux-x86_64.tar.gz` | one `zerodeck-<tag>-linux-x86_64/` wrapper with `bin/zerodeckd`, `bin/zerodeckctl`, `bootstrap.sh`, `release.json`, `python/`, `frontend/`, `systemd/`, `docs/`, `LICENSE`, `README.md` | manual/diagnostic install and bootstrap controller payload |
| `zerodeck-<tag>-linux-x86_64.managed.tar.gz` | archive root exactly `release.json`, `bin/zerodeckd`, `python/`, `frontend/` | generation payload followed by `zerodeckctl update` |

`release.json` is schema `1` and contains the workspace SemVer version and the non-empty
`ZERODECK_BUILD_ID`, which release automation sets to `<tag>-<commit-sha>`. The daemon refuses to
serve a generation whose manifest build ID or version differs from its compiled values. The
managed archive's SHA-256 is the generation identity; the signed channel document names that digest
and the archive's length and release path (`<tag>/<asset name>`).

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
7. **Channels follow automatically.** Right after publishing (and once a day as a safety net), the
   `Channels` workflow runs `just update-channels`: it finds the newest published, immutable release per
   channel, re-verifies the archive digest, `release.json` and GitHub build provenance, and writes
   `stable.json` / `testing.json` plus a detached ed25519 signature to the `channels` branch.
   Nothing is promoted by hand: merging the Release Please PR is the only human step. A release
   whose version has a prerelease suffix only reaches Testing.

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

## Channels and signing

`zerodeckctl update` follows one small signed document per channel, served from the `channels`
branch (`https://raw.githubusercontent.com/<owner>/<repo>/channels/<channel>.json` and
`<channel>.json.sig`):

```json
{
  "schema": 1, "channel": "stable", "sequence": 7, "expires": 1767225600,
  "version": "0.2.0", "digest": "<managed archive sha256>", "size": 123456,
  "build_id": "v0.2.0-<commit>", "min_updater": "0.1.0",
  "target": "v0.2.0/zerodeck-v0.2.0-linux-x86_64.managed.tar.gz"
}
```

The client pins one or more ed25519 public keys (`/etc/zerodeck/trusted-root.json`, shipped in the
manual archive as `trusted-root.json`) and rejects a document unless the signature verifies over the
exact bytes, the channel name matches, it has not expired and its `sequence` is not lower than the
last one it accepted (replay and freeze protection). The archive is then downloaded from the release
and must match `digest` and `size` before anything is extracted.

Signing is automatic and lives only in `.github/workflows/channels.yml`:

- the private key is the `CHANNEL_SIGNING_KEY` secret of the `release-signing` environment, which is
  restricted to protected branches; no person holds it and nothing is signed on a laptop;
- `deploy/update/sign-channel.sh` refuses to sign unless the archive digest, `release.json` identity
  and GitHub build provenance (exact repository, signer workflow, source commit) verify;
- Stable also requires a VM attestation for that same managed archive digest from
  `Qualify Stable` on `main`. Qualification always updates from the newest published Stable
  release that already has VM evidence; a lower-version fixture is allowed only when no previous
  immutable stable release exists. The channels job selects the newest qualified Stable release
  directly from immutable release history. It never reads the mutable `channels` branch to choose
  a target, so rewriting that branch cannot redirect signing or downgrade Stable. A published
  candidate can enter Testing while Stable waits for a passing result. The release workflow
  dispatches qualification after publication; a passing qualification asks the channels workflow
  to sign immediately. If it fails, rerun `Qualify Stable` for the same tag. If no release has a
  qualified result, the channels run leaves Stable unchanged, so qualify the intended release
  before its 90-day document expires. The qualification
  workflow enforces the baseline from immutable release history, and `sign-channel.sh` trusts only
  attestations from that workflow on `main`;
- documents are valid for 90 days and the daily run re-signs the selected release, so a channel
  that has a qualified target does not expire. A release that was never made immutable or is still
  a draft is never signed.

The qualification VM runs generic Linux with systemd. SteamOS survival and Game Mode compatibility
are recorded independently in [COMPATIBILITY.md](COMPATIBILITY.md).

What this protects: a compromised download host cannot substitute bytes (digest and length are
signed), nobody can replay an older channel, and a push to the `channels` branch alone cannot forge
a release. What it does not: someone who can modify workflows on `main` or read environment secrets
can sign. Keep `main` protected and the `release-signing` environment restricted.

**Rotating the key.** Managed `update` and `upgrade` replace the daemon generation; they never change
the pinned `/etc/zerodeck/trusted-root.json`. Key rotation is an administrative bootstrap operation:

1. Publish a manual archive with both the current and replacement public keys. Keep signing the
   channels with the current key during this transition.
2. On each installation, independently verify the manual and matching managed archive's build
   provenance and SHA-256, then run the packaged bootstrap with `--convert-existing-zerodeck`
   and the verified digests. Pass `--start` if the existing daemon is running. This updates the
   pinned keys without resetting the catalog's replay protection.
3. After confirming that the target installations trust the replacement key, change the CI signing
   secret and verify that a client from step 2 can check the newly signed channel. Publish and deploy
   a manual archive without the retired key to remove the old trust.

If a key leaks, replace the CI secret immediately. Re-bootstrap affected installations with an
independently verified manual archive that omits the leaked key; do not use the compromised
channel to establish replacement trust.

## First install bootstrap

Obtain the two archives and `SHA256SUMS.txt` from the release and compare the archive digests with
the release page, then run the packaged helper as root (the channel public key and the channel and
release URLs come from the manual archive and the project's defaults):

```bash
sudo ./bootstrap.sh \
  --package zerodeck-v0.1.0-linux-x86_64.tar.gz \
  --sha256 <manual-sha256-from-trusted-channel> \
  --generation zerodeck-v0.1.0-linux-x86_64.managed.tar.gz \
  --generation-sha256 <managed-sha256-from-trusted-channel> \
  --start
```

The helper first copies the package, generation and trusted keys into a root-only work directory
and verifies those copies. No caller-controlled path is re-opened for extraction or execution, so a
file swapped after a hash check cannot be installed. Before extracting anything it also verifies the
archive SHA-256 digests and parses the keys file. It refuses if an install path or any of its ancestors is
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

`min_updater` in the signed channel document prevents an old controller from activating a newer generation.
To replace the controller itself, use the verified bootstrap helper with
`--convert-existing-zerodeck`; it refuses during a pending activation or when the persisted state
cannot be read. Do not replace `/home/.zerodeck/bin/zerodeckctl` by hand while an apply is
pending. An offline rollback uses the retained generation and does not consult the channel or the network.

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

## Verification

- **VM crash gate:** CI job `vm` boots the packaged release in a KVM VM (pinned Arch Linux cloud
  image) and drives update, rollback, hard poweroff, `SIGKILL` of the apply helper, port collision,
  corrupt rollback, reboot during recovery and a second recovery pass:
  `just test-updater-vm-ci dist <tag> <candidate>`, where the candidate comes from
  `just build-vm-candidate 0.1.1 <dir>` (a second build with a bumped version, because the updater
  rightly refuses a new digest under an installed version). To run it against another image use
  `just test-updater-vm <image> <key> <user> <manual> <managed> [<next-managed>]`; it exits `77`
  with `skipped: no qemu/image` when the host cannot run it, and a skip is not a pass. The update
  scenarios write the catalog authorization directly instead of fetching a channel; the channel
  verifier is covered by the Rust catalog tests and the signing script by
  `deploy/update/tests/sign-channel.sh`. See [`tests/README.md`](tests/README.md).
- **Steam Deck:** [`COMPATIBILITY.md`](COMPATIBILITY.md) lists what can only be observed on
  hardware (SteamOS-update survival, plugin inventory, resource use). Fill it in as you run them;
  unexecuted items stay **PENDING**. It informs you but does not gate publication.
- **Retry:** `deploy/update/tests/publish.sh` injects an upload failure, confirms the draft remains
  unpublished and that a retry never changes a published digest.
- **Expiry and freeze:** an expired or replayed (lower `sequence`) channel document makes
  `zerodeckctl update` fail closed while `rollback` still works offline from a retained generation.

The local shell tests exercise bootstrap refusal/conversion/import, release-ID upload retry and
channel signing with provenance refusal: `deploy/update/tests/{bootstrap,publish,sign-channel}.sh`.
Rust catalog tests cover signature, expiry, replay and selection. The VM gate is
`deploy/update/tests/vm.sh`; its fake-QEMU self-test is part of `just check`. Hardware observations are
recorded in `deploy/update/COMPATIBILITY.md`.
