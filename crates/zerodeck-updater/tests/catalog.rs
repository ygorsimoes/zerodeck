//! Signed release-channel verification, replay/expiry/clock protection and selection tests.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey};
use semver::Version;
use serde_json::{Value, json};
use url::Url;
use zerodeck_updater::{
    Catalog, CatalogState, CatalogStateStore, Channel, ChannelSource, GenerationId,
    SelectionPolicy, UpdateError,
};

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

/// Serves channel files from memory, keyed by URL.
#[derive(Default)]
struct MemorySource(HashMap<String, Vec<u8>>);

impl ChannelSource for MemorySource {
    async fn fetch(&self, url: &Url) -> Result<Vec<u8>, UpdateError> {
        self.0
            .get(url.as_str())
            .cloned()
            .ok_or_else(|| UpdateError::ChannelFetch {
                url: url.to_string(),
                reason: "not found".to_owned(),
            })
    }
}

const CHANNELS: &str = "https://channels.example.test/";
const RELEASES: &str = "https://releases.example.test/";

fn signing_key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn keys_document(keys: &[&SigningKey]) -> Vec<u8> {
    let keys: Vec<String> = keys
        .iter()
        .map(|key| hex::encode(key.verifying_key().to_bytes()))
        .collect();
    serde_json::to_vec(&json!({ "keys": keys })).expect("serialize keys")
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs()
}

fn digest(fill: char) -> String {
    std::iter::repeat_n(fill, 64).collect()
}

fn document(channel: &str, version: &str, sequence: u64, fill: char) -> Value {
    json!({
        "schema": 1,
        "channel": channel,
        "sequence": sequence,
        "expires": now() + 86_400,
        "version": version,
        "digest": digest(fill),
        "size": 1234,
        "build_id": format!("build-{version}"),
        "min_updater": "0.1.0",
        "target": format!("v{version}/zerodeck-v{version}-linux-x86_64.managed.tar.gz"),
    })
}

fn publish(source: &mut MemorySource, channel: &str, document: &Value, key: &SigningKey) {
    let bytes = serde_json::to_vec_pretty(document).expect("serialize document");
    let signature = hex::encode(key.sign(&bytes).to_bytes());
    source
        .0
        .insert(format!("{CHANNELS}{channel}.json"), bytes);
    source
        .0
        .insert(format!("{CHANNELS}{channel}.json.sig"), signature.into_bytes());
}

async fn load(
    root: &TestDir,
    source: &MemorySource,
    keys: &[u8],
    channel: Channel,
) -> Result<Catalog, UpdateError> {
    Catalog::load_with_source(
        &root.0,
        Url::parse(CHANNELS).expect("channels url"),
        Url::parse(RELEASES).expect("releases url"),
        keys,
        channel,
        source,
    )
    .await
}

fn policy() -> SelectionPolicy {
    SelectionPolicy::new(Version::parse("0.1.0").expect("updater version"))
}

fn generation(fill: char) -> GenerationId {
    GenerationId::new(digest(fill)).expect("valid digest")
}

#[tokio::test]
async fn a_signed_channel_authorizes_exactly_its_release() {
    let key = signing_key(1);
    let mut source = MemorySource::default();
    publish(&mut source, "stable", &document("stable", "0.2.0", 1, 'a'), &key);
    let root = TestDir::new("ok");

    let catalog = load(&root, &source, &keys_document(&[&key]), Channel::Stable)
        .await
        .expect("load signed channel");
    let release = catalog
        .candidate(&policy())
        .expect("select")
        .expect("a release is available");

    assert_eq!(release.version, Version::parse("0.2.0").unwrap());
    assert_eq!(release.digest, generation('a'));
    assert_eq!(release.size, 1234);
    assert_eq!(
        release.url.expect("download url").as_str(),
        "https://releases.example.test/v0.2.0/zerodeck-v0.2.0-linux-x86_64.managed.tar.gz"
    );
    assert_eq!(
        catalog
            .persisted_authorization()
            .expect("read state")
            .map(|persisted| persisted.digest),
        Some(generation('a')),
        "the selected release is persisted for the offline apply"
    );
}

#[tokio::test]
async fn any_pinned_key_may_sign() {
    let old = signing_key(1);
    let new = signing_key(2);
    let mut source = MemorySource::default();
    publish(&mut source, "stable", &document("stable", "0.2.0", 1, 'a'), &new);
    let root = TestDir::new("two-keys");

    load(&root, &source, &keys_document(&[&old, &new]), Channel::Stable)
        .await
        .expect("the second pinned key verifies the document");
}

#[tokio::test]
async fn an_untrusted_or_tampered_document_is_rejected() {
    let trusted = signing_key(1);
    let attacker = signing_key(9);
    let keys = keys_document(&[&trusted]);

    let mut forged = MemorySource::default();
    publish(&mut forged, "stable", &document("stable", "0.2.0", 1, 'a'), &attacker);
    let root = TestDir::new("forged");
    assert!(matches!(
        load(&root, &forged, &keys, Channel::Stable).await,
        Err(UpdateError::ChannelSignature(_))
    ));

    let mut tampered = MemorySource::default();
    publish(&mut tampered, "stable", &document("stable", "0.2.0", 1, 'a'), &trusted);
    let key = format!("{CHANNELS}stable.json");
    let altered = String::from_utf8(tampered.0[&key].clone())
        .expect("utf8")
        .replace(&digest('a'), &digest('b'));
    tampered.0.insert(key, altered.into_bytes());
    let root = TestDir::new("tampered");
    assert!(matches!(
        load(&root, &tampered, &keys, Channel::Stable).await,
        Err(UpdateError::ChannelSignature(_))
    ));
}

#[tokio::test]
async fn a_channel_document_cannot_be_replayed_on_another_channel() {
    let key = signing_key(1);
    let mut source = MemorySource::default();
    // A validly signed Stable document served under the Testing name.
    let stable = document("stable", "0.2.0", 1, 'a');
    publish(&mut source, "testing", &stable, &key);
    let root = TestDir::new("wrong-channel");

    assert!(matches!(
        load(&root, &source, &keys_document(&[&key]), Channel::Testing).await,
        Err(UpdateError::ChannelInvalid(_))
    ));
}

#[tokio::test]
async fn expired_documents_are_rejected() {
    let key = signing_key(1);
    let mut source = MemorySource::default();
    let mut expired = document("stable", "0.2.0", 1, 'a');
    expired["expires"] = json!(now() - 1);
    publish(&mut source, "stable", &expired, &key);
    let root = TestDir::new("expired");

    assert!(matches!(
        load(&root, &source, &keys_document(&[&key]), Channel::Stable).await,
        Err(UpdateError::AuthorizationExpired { .. })
    ));
}

#[tokio::test]
async fn an_older_sequence_is_a_replay_but_the_same_sequence_is_fine() {
    let key = signing_key(1);
    let keys = keys_document(&[&key]);
    let root = TestDir::new("sequence");

    let mut source = MemorySource::default();
    publish(&mut source, "stable", &document("stable", "0.2.0", 5, 'a'), &key);
    load(&root, &source, &keys, Channel::Stable).await.expect("sequence 5");
    load(&root, &source, &keys, Channel::Stable)
        .await
        .expect("re-reading the same sequence is allowed");

    publish(&mut source, "stable", &document("stable", "0.1.0", 4, 'b'), &key);
    assert!(matches!(
        load(&root, &source, &keys, Channel::Stable).await,
        Err(UpdateError::ChannelRollback {
            sequence: 4,
            floor: 5,
            ..
        })
    ));
}

#[tokio::test]
async fn a_clock_behind_the_last_verification_fails_closed() {
    let key = signing_key(1);
    let mut source = MemorySource::default();
    publish(&mut source, "stable", &document("stable", "0.2.0", 1, 'a'), &key);
    let root = TestDir::new("clock");
    let store = CatalogStateStore::new(root.0.join("state/catalog.json"));
    store
        .write(&CatalogState {
            last_verified_at: Some(now() + 3600),
            ..CatalogState::new()
        })
        .expect("seed state");

    assert!(matches!(
        load(&root, &source, &keys_document(&[&key]), Channel::Stable).await,
        Err(UpdateError::ClockRollback { .. })
    ));
}

#[tokio::test]
async fn unsafe_targets_and_bad_digests_are_rejected() {
    let key = signing_key(1);
    let keys = keys_document(&[&key]);
    for target in [
        "../etc/passwd",
        "/abs/zerodeck.tar.gz",
        "only-one-segment",
        "a/b/c",
        "v1/zero deck.tar.gz",
        "v1/..",
    ] {
        let mut source = MemorySource::default();
        let mut bad = document("stable", "0.2.0", 1, 'a');
        bad["target"] = json!(target);
        publish(&mut source, "stable", &bad, &key);
        let root = TestDir::new("target");
        assert!(
            matches!(
                load(&root, &source, &keys, Channel::Stable).await,
                Err(UpdateError::InvalidLoaderTarget { .. })
            ),
            "target `{target}` must be rejected"
        );
    }

    let mut source = MemorySource::default();
    let mut bad = document("stable", "0.2.0", 1, 'a');
    bad["digest"] = json!("not-a-digest");
    publish(&mut source, "stable", &bad, &key);
    let root = TestDir::new("digest");
    assert!(load(&root, &source, &keys, Channel::Stable).await.is_err());
}

#[tokio::test]
async fn unusable_trusted_keys_are_rejected() {
    let key = signing_key(1);
    let mut source = MemorySource::default();
    publish(&mut source, "stable", &document("stable", "0.2.0", 1, 'a'), &key);
    for keys in [&br#"{"keys":[]}"#[..], br#"{"keys":["zz"]}"#, b"not json"] {
        let root = TestDir::new("bad-keys");
        assert!(matches!(
            load(&root, &source, keys, Channel::Stable).await,
            Err(UpdateError::InvalidTrustedKeys(_))
        ));
    }
}

#[tokio::test]
async fn selection_applies_policy_to_the_signed_release() {
    let key = signing_key(1);
    let keys = keys_document(&[&key]);
    let root = TestDir::new("policy");

    // Prerelease: only with the Testing policy.
    let mut source = MemorySource::default();
    publish(&mut source, "testing", &document("testing", "0.3.0-rc.1", 1, 'c'), &key);
    let catalog = load(&root, &source, &keys, Channel::Testing).await.expect("load");
    let testing = policy().with_channel(Channel::Testing);
    assert!(catalog.preview_candidate(&testing).unwrap().is_none());
    assert!(
        catalog
            .preview_candidate(&testing.clone().with_prereleases(true))
            .unwrap()
            .is_some()
    );

    // A newer updater requirement hides the release.
    let mut source = MemorySource::default();
    let mut needs_new = document("stable", "0.4.0", 1, 'd');
    needs_new["min_updater"] = json!("9.0.0");
    publish(&mut source, "stable", &needs_new, &key);
    let min_root = TestDir::new("min");
    let catalog = load(&min_root, &source, &keys, Channel::Stable)
        .await
        .expect("load");
    assert!(catalog.preview_candidate(&policy()).unwrap().is_none());

    // Installed version: older/equal are skipped, same version with another digest conflicts.
    let mut source = MemorySource::default();
    publish(&mut source, "stable", &document("stable", "0.2.0", 1, 'a'), &key);
    let installed_root = TestDir::new("installed");
    let catalog = load(&installed_root, &source, &keys, Channel::Stable)
        .await
        .expect("load");
    let newer = Version::parse("0.3.0").unwrap();
    let same = Version::parse("0.2.0").unwrap();
    assert!(
        catalog
            .preview_candidate(&policy().with_installed(newer, generation('f')))
            .unwrap()
            .is_none(),
        "no downgrade"
    );
    assert!(
        catalog
            .preview_candidate(&policy().with_installed(same.clone(), generation('a')))
            .unwrap()
            .is_none(),
        "already installed"
    );
    assert!(matches!(
        catalog.preview_candidate(&policy().with_installed(same, generation('b'))),
        Err(UpdateError::VersionConflict { .. })
    ));

    // A quarantined digest is never selected.
    assert!(
        catalog
            .preview_candidate(&policy().with_quarantined([generation('a')]))
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn only_candidate_persists_and_a_changed_channel_drops_a_stale_authorization() {
    let key = signing_key(1);
    let keys = keys_document(&[&key]);
    let root = TestDir::new("persist");
    let mut source = MemorySource::default();
    publish(&mut source, "stable", &document("stable", "0.2.0", 1, 'a'), &key);

    let catalog = load(&root, &source, &keys, Channel::Stable).await.expect("load");
    assert!(catalog.preview_candidate(&policy()).unwrap().is_some());
    assert_eq!(
        catalog.persisted_authorization().unwrap(),
        None,
        "check-only must not stage anything"
    );

    let staged = catalog.candidate(&policy()).unwrap().expect("stage 0.2.0");
    let reloaded = load(&root, &source, &keys, Channel::Stable).await.expect("reload");
    assert_eq!(
        reloaded
            .persisted_authorization()
            .unwrap()
            .map(|release| release.digest),
        Some(staged.digest),
        "an unchanged channel keeps the staged authorization"
    );

    publish(&mut source, "stable", &document("stable", "0.3.0", 2, 'b'), &key);
    let moved = load(&root, &source, &keys, Channel::Stable).await.expect("load 0.3.0");
    assert_eq!(
        moved.persisted_authorization().unwrap(),
        None,
        "the channel no longer names the staged release"
    );
}

#[tokio::test]
async fn offline_catalogs_cannot_select() {
    let root = TestDir::new("offline");
    let offline = Catalog::open_offline(&root.0).expect("open offline");
    assert!(matches!(
        offline.candidate(&policy()),
        Err(UpdateError::CatalogNotLoaded | UpdateError::CatalogNotVerified)
    ));
}
