//! Signed TUF catalog selection, expiry, clock-floor and root-rotation tests.

use std::collections::HashMap;
use std::fs;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::Ed25519KeyPair;
use semver::Version;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tough::editor::signed::SignedRole;
use tough::key_source::{KeySource, LocalKeySource};
use tough::schema::decoded::{Decoded, Hex};
use tough::schema::key::Key;
use tough::schema::{
    Hashes, KeyHolder, Metafile, Role, RoleKeys, RoleType, Root, Signature, Snapshot, Target,
    Targets, Timestamp,
};
use tough::sign::Sign;
use tough::{FilesystemTransport, TargetName};
use url::Url;
use zerodeck_updater::{Catalog, Channel, GenerationId, SelectionPolicy, UpdateError};

const FAR_FUTURE: &str = "9999-01-01T00:00:00Z";
const EXPIRED: &str = "2020-01-01T00:00:00Z";
const AFTER_EXPIRY: u64 = 253_500_000_000;

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "zerodeck-catalog-{label}-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("create temp test dir");
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct TestKey {
    path: PathBuf,
    key: Key,
    id: Decoded<Hex>,
}

impl TestKey {
    fn generate(dir: &Path, name: &str) -> Self {
        let document = Ed25519KeyPair::generate_pkcs8v1(&SystemRandom::new())
            .expect("generate ed25519 test key");
        let path = dir.join(name);
        fs::write(&path, document.as_ref()).expect("write test key");
        let pair = Ed25519KeyPair::from_pkcs8(document.as_ref()).expect("parse generated test key");
        let key = pair.tuf_key();
        let id = key.key_id().expect("compute test key id");
        Self { path, key, id }
    }

    fn source(&self) -> Box<dyn KeySource> {
        Box::new(LocalKeySource {
            path: self.path.clone(),
        })
    }
}

fn role_keys(keys: &[&TestKey], threshold: u64) -> RoleKeys {
    RoleKeys {
        keyids: keys.iter().map(|key| key.id.clone()).collect(),
        threshold: NonZeroU64::new(threshold).expect("nonzero threshold"),
        _extra: HashMap::new(),
    }
}

fn root_metadata(
    version: u64,
    expires: &str,
    keys: &[&TestKey],
    root_keys: &[&TestKey],
    metadata_keys: &[&TestKey],
) -> Root {
    let mut key_map = HashMap::new();
    for key in keys {
        key_map.insert(key.id.clone(), key.key.clone());
    }
    let mut roles = HashMap::new();
    let root_threshold = u64::try_from(root_keys.len()).expect("root key count");
    roles.insert(RoleType::Root, role_keys(root_keys, root_threshold));
    for role in [RoleType::Snapshot, RoleType::Targets, RoleType::Timestamp] {
        roles.insert(role, role_keys(metadata_keys, 1));
    }
    Root {
        spec_version: "1.0.0".to_owned(),
        consistent_snapshot: false,
        version: NonZeroU64::new(version).expect("nonzero root version"),
        expires: expires.parse().expect("parse root expiry"),
        keys: key_map,
        roles,
        _extra: HashMap::new(),
    }
}

async fn sign_role<T>(role: T, root: &Root, keys: &[&TestKey]) -> SignedRole<T>
where
    T: Role + Serialize,
{
    let sources: Vec<Box<dyn KeySource>> = keys.iter().map(|key| key.source()).collect();
    SignedRole::new(
        role,
        &KeyHolder::Root(root.clone()),
        &sources,
        &SystemRandom::new(),
    )
    .await
    .expect("sign test metadata")
}

#[derive(Clone)]
struct TargetSpec {
    arch: &'static str,
    version: Version,
    channel: &'static str,
    build_id: String,
    min_updater: Version,
    bytes: Vec<u8>,
}

impl TargetSpec {
    fn new(
        arch: &'static str,
        version: &str,
        channel: &'static str,
        build_id: &str,
        min_updater: &str,
        seed: &str,
    ) -> Self {
        Self {
            arch,
            version: Version::parse(version).expect("parse target version"),
            channel,
            build_id: build_id.to_owned(),
            min_updater: Version::parse(min_updater).expect("parse minimum updater version"),
            bytes: format!("archive:{arch}:{version}:{build_id}:{seed}").into_bytes(),
        }
    }

    fn digest(&self) -> GenerationId {
        GenerationId::new(hex::encode(Sha256::digest(&self.bytes))).expect("valid target digest")
    }

    fn name(&self) -> String {
        format!("{}/zerodeck-{}.tar.gz", self.arch, self.version)
    }

    fn target(&self) -> (TargetName, Target) {
        let custom = HashMap::from([
            ("channel".to_owned(), json!(self.channel)),
            (
                "version".to_owned(),
                Value::String(self.version.to_string()),
            ),
            ("build_id".to_owned(), json!(self.build_id)),
            (
                "min_updater".to_owned(),
                Value::String(self.min_updater.to_string()),
            ),
        ]);
        let target = Target {
            length: u64::try_from(self.bytes.len()).expect("target length"),
            hashes: Hashes {
                sha256: Sha256::digest(&self.bytes).to_vec().into(),
                _extra: HashMap::new(),
            },
            custom,
            _extra: HashMap::new(),
        };
        (
            TargetName::new(self.name()).expect("valid target name"),
            target,
        )
    }
}

struct PublishSpec<'a> {
    root_version: u64,
    metadata_version: u64,
    expires: &'a str,
    targets: Vec<TargetSpec>,
}

struct Fixture {
    _dir: TestDir,
    metadata: PathBuf,
    targets: PathBuf,
    old_key: TestKey,
    new_key: TestKey,
    trusted_root: Vec<u8>,
}

impl Fixture {
    async fn new() -> Self {
        let dir = TestDir::new("fixture");
        let key_dir = dir.0.join("keys");
        fs::create_dir(&key_dir).expect("create key dir");
        let old_key = TestKey::generate(&key_dir, "old.der");
        let new_key = TestKey::generate(&key_dir, "new.der");
        let metadata = dir.0.join("metadata");
        let targets = dir.0.join("targets");
        fs::create_dir(&metadata).expect("create metadata dir");
        fs::create_dir(&targets).expect("create targets dir");

        let root = root_metadata(1, FAR_FUTURE, &[&old_key], &[&old_key], &[&old_key]);
        let signed_root = sign_role(root.clone(), &root, &[&old_key]).await;
        signed_root
            .write(&metadata, false)
            .await
            .expect("write trusted root");
        let trusted_root = fs::read(metadata.join("1.root.json")).expect("read trusted root");

        Self {
            _dir: dir,
            metadata,
            targets,
            old_key,
            new_key,
            trusted_root,
        }
    }

    fn metadata_url(&self) -> Url {
        Url::from_directory_path(&self.metadata).expect("metadata base url")
    }

    fn targets_url(&self) -> Url {
        Url::from_directory_path(&self.targets).expect("targets base url")
    }

    #[allow(clippy::too_many_lines)]
    async fn publish(&self, spec: &PublishSpec<'_>) {
        let (root, root_signers, metadata_signers) = if spec.root_version == 1 {
            (
                root_metadata(
                    1,
                    FAR_FUTURE,
                    &[&self.old_key],
                    &[&self.old_key],
                    &[&self.old_key],
                ),
                vec![&self.old_key],
                vec![&self.old_key],
            )
        } else {
            (
                root_metadata(
                    2,
                    FAR_FUTURE,
                    &[&self.new_key],
                    &[&self.new_key],
                    &[&self.new_key],
                ),
                vec![&self.new_key],
                vec![&self.new_key],
            )
        };

        let mut signed_root = sign_role(root.clone(), &root, &root_signers).await;
        if spec.root_version != 1 {
            let old_pair =
                Ed25519KeyPair::from_pkcs8(&fs::read(&self.old_key.path).expect("read old key"))
                    .expect("parse old key");
            let canonical = root.canonical_form().expect("canonical root form");
            let signature = old_pair.sign(&canonical);
            signed_root = signed_root
                .add_old_signatures(vec![Signature {
                    keyid: self.old_key.id.clone(),
                    sig: signature.as_ref().to_vec().into(),
                }])
                .expect("cross-sign rotated root with the previous key");
        }
        signed_root
            .write(&self.metadata, false)
            .await
            .expect("write root metadata");

        let mut targets = HashMap::new();
        for target in &spec.targets {
            let (name, target) = target.target();
            targets.insert(name, target);
        }
        let metadata_version =
            NonZeroU64::new(spec.metadata_version).expect("nonzero metadata version");
        let targets_role = Targets {
            spec_version: "1.0.0".to_owned(),
            version: metadata_version,
            expires: spec.expires.parse().expect("parse targets expiry"),
            targets,
            delegations: None,
            _extra: HashMap::new(),
        };
        let signed_targets = sign_role(targets_role, &root, &metadata_signers).await;

        let snapshot_role = Snapshot {
            spec_version: "1.0.0".to_owned(),
            version: metadata_version,
            expires: spec.expires.parse().expect("parse snapshot expiry"),
            meta: HashMap::from([(
                "targets.json".to_owned(),
                Metafile {
                    length: Some(*signed_targets.length()),
                    hashes: Some(Hashes {
                        sha256: signed_targets.sha256().to_vec().into(),
                        _extra: HashMap::new(),
                    }),
                    version: metadata_version,
                    _extra: HashMap::new(),
                },
            )]),
            _extra: HashMap::new(),
        };
        let signed_snapshot = sign_role(snapshot_role, &root, &metadata_signers).await;

        let timestamp_role = Timestamp {
            spec_version: "1.0.0".to_owned(),
            version: metadata_version,
            expires: spec.expires.parse().expect("parse timestamp expiry"),
            meta: HashMap::from([(
                "snapshot.json".to_owned(),
                Metafile {
                    length: Some(*signed_snapshot.length()),
                    hashes: Some(Hashes {
                        sha256: signed_snapshot.sha256().to_vec().into(),
                        _extra: HashMap::new(),
                    }),
                    version: metadata_version,
                    _extra: HashMap::new(),
                },
            )]),
            _extra: HashMap::new(),
        };
        let signed_timestamp = sign_role(timestamp_role, &root, &metadata_signers).await;

        signed_targets
            .write(&self.metadata, false)
            .await
            .expect("write targets metadata");
        signed_snapshot
            .write(&self.metadata, false)
            .await
            .expect("write snapshot metadata");
        signed_timestamp
            .write(&self.metadata, false)
            .await
            .expect("write timestamp metadata");
    }
}

async fn load_catalog(fixture: &Fixture, update_root: &Path) -> Catalog {
    Catalog::load_with_transport(
        update_root,
        fixture.metadata_url(),
        fixture.targets_url(),
        &fixture.trusted_root,
        FilesystemTransport,
    )
    .await
    .expect("load signed catalog")
}

fn after_epoch(seconds: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds)
}

fn generation_id(fill: char) -> GenerationId {
    GenerationId::new(fill.to_string().repeat(64)).expect("valid generation id")
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn catalog_rejects_wrong_arch_floor_version_reuse_and_expiry() {
    let fixture = Fixture::new().await;
    let update_root = TestDir::new("update-root");

    let stable_012 = TargetSpec::new(
        "linux-x86_64",
        "0.1.2",
        "stable",
        "build-0.1.2",
        "0.1.0",
        "stable-012",
    );
    let stable_013_floor = TargetSpec::new(
        "linux-x86_64",
        "0.1.3",
        "stable",
        "build-0.1.3",
        "9.0.0",
        "stable-013-floor",
    );
    let prerelease_014 = TargetSpec::new(
        "linux-x86_64",
        "0.1.4-beta.1",
        "stable",
        "build-0.1.4-beta.1",
        "0.1.0",
        "stable-014-beta",
    );
    let testing_020 = TargetSpec::new(
        "linux-x86_64",
        "0.2.0",
        "testing",
        "build-0.2.0",
        "0.1.0",
        "testing-020",
    );
    let wrong_arch_900 = TargetSpec::new(
        "linux-aarch64",
        "9.0.0",
        "stable",
        "build-9.0.0",
        "0.1.0",
        "wrong-arch",
    );

    fixture
        .publish(&PublishSpec {
            root_version: 1,
            metadata_version: 1,
            expires: FAR_FUTURE,
            targets: vec![
                stable_012.clone(),
                stable_013_floor,
                prerelease_014,
                testing_020,
                wrong_arch_900,
            ],
        })
        .await;
    let catalog = load_catalog(&fixture, &update_root.0).await;

    let updater = Version::parse("1.0.0").expect("parse updater version");
    let installed = Version::parse("0.1.1").expect("parse installed version");
    let policy =
        SelectionPolicy::new(updater.clone()).with_installed(installed.clone(), generation_id('c'));
    let before_expiry = SystemTime::now() + Duration::from_secs(3600);

    let candidate = catalog
        .candidate(&policy)
        .expect("select stable candidate")
        .expect("a stable candidate is eligible");
    assert_eq!(candidate.channel, Channel::Stable);
    assert_eq!(
        candidate.version,
        Version::parse("0.1.2").expect("parse expected version")
    );
    assert_eq!(candidate.digest, stable_012.digest());
    assert_eq!(
        candidate.size,
        u64::try_from(stable_012.bytes.len()).expect("archive size")
    );
    assert_eq!(candidate.build_id, "build-0.1.2");
    assert_eq!(
        candidate.min_updater,
        Version::parse("0.1.0").expect("parse minimum updater")
    );
    assert_eq!(candidate.target, stable_012.name());

    catalog
        .authorize_staged(&candidate, before_expiry)
        .expect("a staged release remains authorized before expiry");
    let err = catalog
        .authorize_staged(&candidate, after_epoch(AFTER_EXPIRY))
        .expect_err("expired authorization must fail closed");
    assert!(
        matches!(err, UpdateError::AuthorizationExpired { .. }),
        "unexpected expiry error: {err:?}"
    );

    let earlier = SystemTime::now()
        .checked_sub(Duration::from_secs(3600))
        .expect("one hour before now");
    let err = catalog
        .authorize_staged(&candidate, earlier)
        .expect_err("a clock before the persisted floor must fail closed");
    assert!(
        matches!(err, UpdateError::ClockRollback { .. }),
        "unexpected clock error: {err:?}"
    );

    let offline = Catalog::open_offline(&update_root.0).expect("open persisted authorization");
    offline
        .authorize_staged(&candidate, before_expiry)
        .expect("an offline apply uses the persisted authorization");
    assert!(
        matches!(
            offline.candidate(&policy),
            Err(UpdateError::CatalogNotLoaded)
        ),
        "offline authorization must not select without trusted metadata"
    );

    let reuse = SelectionPolicy::new(updater.clone()).with_installed(
        Version::parse("0.1.2").expect("parse reused version"),
        generation_id('d'),
    );
    let err = catalog
        .candidate(&reuse)
        .expect_err("a reused version with another digest must conflict");
    assert!(
        matches!(err, UpdateError::VersionConflict { .. }),
        "unexpected conflict error: {err:?}"
    );

    let new_floor = SelectionPolicy::new(Version::parse("0.0.1").expect("parse old updater"))
        .with_installed(installed.clone(), generation_id('c'));
    assert!(
        catalog
            .candidate(&new_floor)
            .expect("floor check must not fail")
            .is_none(),
        "a target below the updater floor must not be eligible"
    );

    let newer_install = SelectionPolicy::new(updater.clone()).with_installed(
        Version::parse("2.0.0").expect("parse newer install"),
        generation_id('e'),
    );
    assert!(
        catalog
            .candidate(&newer_install)
            .expect("installed comparison must not fail")
            .is_none(),
        "the catalog must never auto-select below the installed version"
    );

    let testing_only = Fixture::new().await;
    let testing_root = TestDir::new("testing-root");
    let testing_target = TargetSpec::new(
        "linux-x86_64",
        "0.1.2",
        "testing",
        "build-testing-0.1.2",
        "0.1.0",
        "testing-only",
    );
    testing_only
        .publish(&PublishSpec {
            root_version: 1,
            metadata_version: 1,
            expires: FAR_FUTURE,
            targets: vec![testing_target],
        })
        .await;
    let testing_catalog = load_catalog(&testing_only, &testing_root.0).await;
    let stable_policy = SelectionPolicy::new(updater.clone());
    assert!(
        testing_catalog
            .candidate(&stable_policy)
            .expect("stable selection must not fail")
            .is_none(),
        "a Stable policy must not select a Testing-only catalog"
    );
    let testing_policy = SelectionPolicy::new(updater.clone()).with_channel(Channel::Testing);
    let testing_candidate = testing_catalog
        .candidate(&testing_policy)
        .expect("testing selection must not fail")
        .expect("an explicit Testing policy selects the target");
    assert_eq!(testing_candidate.channel, Channel::Testing);

    let expired_fixture = Fixture::new().await;
    let expired_root = TestDir::new("expired-root");
    expired_fixture
        .publish(&PublishSpec {
            root_version: 1,
            metadata_version: 1,
            expires: EXPIRED,
            targets: vec![TargetSpec::new(
                "linux-x86_64",
                "0.1.2",
                "stable",
                "build-expired",
                "0.1.0",
                "expired",
            )],
        })
        .await;
    let err = Catalog::load_with_transport(
        &expired_root.0,
        expired_fixture.metadata_url(),
        expired_fixture.targets_url(),
        &expired_fixture.trusted_root,
        FilesystemTransport,
    )
    .await
    .expect_err("expired metadata must not be trusted");
    assert!(
        matches!(err, UpdateError::Catalog(_)),
        "unexpected expired-metadata error: {err:?}"
    );

    let v1_timestamp =
        fs::read(fixture.metadata.join("timestamp.json")).expect("read v1 timestamp");
    fixture
        .publish(&PublishSpec {
            root_version: 2,
            metadata_version: 2,
            expires: FAR_FUTURE,
            targets: vec![
                stable_012.clone(),
                TargetSpec::new(
                    "linux-x86_64",
                    "0.1.3",
                    "stable",
                    "build-0.1.3",
                    "0.1.0",
                    "stable-013",
                ),
            ],
        })
        .await;
    let rotated = load_catalog(&fixture, &update_root.0).await;
    rotated
        .authorize_staged(&candidate, before_expiry)
        .expect("an unchanged target stays authorized across a root rotation");

    let newer = rotated
        .candidate(&policy)
        .expect("select rotated candidate")
        .expect("the rotated catalog offers a newer stable release");
    assert_eq!(
        newer.version,
        Version::parse("0.1.3").expect("parse newer version")
    );
    let err = rotated
        .authorize_staged(&candidate, before_expiry)
        .expect_err("a newly selected release supersedes the staged authorization");
    assert!(
        matches!(err, UpdateError::ReleaseSuperseded { .. }),
        "unexpected supersession error: {err:?}"
    );
    rotated
        .authorize_staged(&newer, before_expiry)
        .expect("the newest release is authorized");

    let v2_timestamp =
        fs::read(fixture.metadata.join("timestamp.json")).expect("read v2 timestamp");
    fs::write(fixture.metadata.join("timestamp.json"), &v1_timestamp)
        .expect("replay old timestamp");
    Catalog::load_with_transport(
        &update_root.0,
        fixture.metadata_url(),
        fixture.targets_url(),
        &fixture.trusted_root,
        FilesystemTransport,
    )
    .await
    .expect_err("a replayed older timestamp must not reset trusted metadata");

    fs::write(fixture.metadata.join("timestamp.json"), &v2_timestamp)
        .expect("restore newest timestamp");
    let restored = load_catalog(&fixture, &update_root.0).await;
    let restored_candidate = restored
        .candidate(&policy)
        .expect("select after restoring metadata")
        .expect("the newest release stays eligible");
    assert_eq!(
        restored_candidate.version,
        Version::parse("0.1.3").expect("parse restored version")
    );
}

#[tokio::test]
async fn preview_candidate_does_not_supersede_a_staged_authorization() {
    let fixture = Fixture::new().await;
    let update_root = TestDir::new("preview-root");
    let stable_012 = TargetSpec::new(
        "linux-x86_64",
        "0.1.2",
        "stable",
        "build-0.1.2",
        "0.1.0",
        "preview-012",
    );
    let stable_013 = TargetSpec::new(
        "linux-x86_64",
        "0.1.3",
        "stable",
        "build-0.1.3",
        "0.1.0",
        "preview-013",
    );
    fixture
        .publish(&PublishSpec {
            root_version: 1,
            metadata_version: 1,
            expires: FAR_FUTURE,
            targets: vec![stable_012.clone()],
        })
        .await;
    let catalog = load_catalog(&fixture, &update_root.0).await;
    let policy = SelectionPolicy::new(Version::parse("1.0.0").expect("updater version"))
        .with_installed(
            Version::parse("0.1.1").expect("installed version"),
            generation_id('c'),
        );
    let staged = catalog
        .candidate(&policy)
        .expect("select staged release")
        .expect("0.1.2 is eligible");
    assert_eq!(staged.digest, stable_012.digest());
    let before_expiry = SystemTime::now() + Duration::from_secs(3600);

    fixture
        .publish(&PublishSpec {
            root_version: 1,
            metadata_version: 2,
            expires: FAR_FUTURE,
            targets: vec![stable_012.clone(), stable_013.clone()],
        })
        .await;
    let reloaded = load_catalog(&fixture, &update_root.0).await;
    let preview = reloaded
        .preview_candidate(&policy)
        .expect("preview newer release")
        .expect("0.1.3 is eligible");
    assert_eq!(preview.digest, stable_013.digest());

    let offline = Catalog::open_offline(&update_root.0).expect("open persisted authorization");
    let persisted = offline
        .persisted_authorization()
        .expect("read persisted authorization")
        .expect("staged authorization remains");
    assert_eq!(persisted.digest, staged.digest);
    offline
        .authorize_staged(&staged, before_expiry)
        .expect("check-only must not invalidate the staged release");
    assert!(matches!(
        offline.authorize_staged(&preview, before_expiry),
        Err(UpdateError::ReleaseSuperseded { .. })
    ));

    let selected = reloaded
        .candidate(&policy)
        .expect("persisting selection succeeds")
        .expect("0.1.3 is eligible");
    assert_eq!(selected.digest, stable_013.digest());
    assert_eq!(
        Catalog::open_offline(&update_root.0)
            .expect("open after selection")
            .persisted_authorization()
            .expect("read selected authorization")
            .expect("selection is persisted")
            .digest,
        stable_013.digest()
    );
}
