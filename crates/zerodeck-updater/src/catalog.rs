//! Signed release-channel verification, selection and offline authorization checks.
//!
//! A channel (`stable` or `testing`) is one small JSON document plus a detached ed25519 signature,
//! both produced by the release automation. The client pins the public key(s) out-of-band, checks
//! the signature, expiry and a never-decreasing `sequence`, and only then treats the named archive
//! (identified by its SHA-256) as authorized.

use std::collections::BTreeSet;
use std::fs;
use std::future::Future;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signature, VerifyingKey};
use semver::Version;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::state::{CATALOG_STATE_SCHEMA, CatalogState, CatalogStateStore, SelectionRecord};
use crate::{GenerationId, UpdateError};

const STATE_DIR: &str = "state";
const CATALOG_STATE_FILE: &str = "catalog.json";
const METADATA_TIMEOUT: Duration = Duration::from_secs(60);
const METADATA_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Largest channel document or signature accepted from the network.
const MAX_CHANNEL_BYTES: usize = 16 * 1024;
/// Current channel document schema.
const CHANNEL_SCHEMA: u32 = 1;

/// Release channel signed into a loader target.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    /// Stable releases.
    #[default]
    Stable,
    /// Testing releases, selected only by an explicit Testing policy.
    Testing,
}

impl Channel {
    /// Parses the two signed channel names.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "stable" => Some(Self::Stable),
            "testing" => Some(Self::Testing),
            _ => None,
        }
    }

    /// The signed lowercase channel name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Testing => "testing",
        }
    }
}

impl std::fmt::Display for Channel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The release currently installed, used to prevent downgrades and version reuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledRelease {
    /// Installed runner version.
    pub version: Version,
    /// Installed generation digest.
    pub digest: GenerationId,
}

/// Selection inputs for [`Catalog::candidate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionPolicy {
    /// Only targets in this channel are eligible.
    pub channel: Channel,
    /// Whether prerelease `SemVer` versions may be selected.
    pub allow_prerelease: bool,
    /// The installed release, when one exists.
    pub installed: Option<InstalledRelease>,
    /// The running updater version used to enforce `min_updater`.
    pub updater_version: Version,
    /// Digests rejected by a failed trial; never selected until an operator releases them.
    pub quarantined: BTreeSet<GenerationId>,
}

impl SelectionPolicy {
    /// Creates a Stable-only policy that rejects prereleases.
    #[must_use]
    pub fn new(updater_version: Version) -> Self {
        Self {
            channel: Channel::Stable,
            allow_prerelease: false,
            installed: None,
            updater_version,
            quarantined: BTreeSet::new(),
        }
    }

    /// Skips these digests, so the next eligible release is selected instead.
    #[must_use]
    pub fn with_quarantined(mut self, digests: impl IntoIterator<Item = GenerationId>) -> Self {
        self.quarantined = digests.into_iter().collect();
        self
    }

    /// Selects only targets from `channel`.
    #[must_use]
    pub fn with_channel(mut self, channel: Channel) -> Self {
        self.channel = channel;
        self
    }

    /// Allows prerelease `SemVer` targets when `allow` is true.
    #[must_use]
    pub fn with_prereleases(mut self, allow: bool) -> Self {
        self.allow_prerelease = allow;
        self
    }

    /// Records the installed release for downgrade and version-reuse checks.
    #[must_use]
    pub fn with_installed(mut self, version: Version, digest: GenerationId) -> Self {
        self.installed = Some(InstalledRelease { version, digest });
        self
    }
}

/// A loader release authorized by the freshest verified channel document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizedRelease {
    /// Signed release channel.
    pub channel: Channel,
    /// Signed release version.
    pub version: Version,
    /// SHA-256 digest of the whole release archive, also the generation identity.
    pub digest: GenerationId,
    /// Signed archive length.
    pub size: u64,
    /// Build identifier embedded in the signed target and `release.json`.
    pub build_id: String,
    /// Minimum updater version declared by the signed target.
    pub min_updater: Version,
    /// Archive path relative to the releases base URL (`<tag>/<asset>`).
    pub target: String,
    /// Download location derived from the targets base URL.
    #[serde(skip)]
    pub url: Option<Url>,
}

impl AuthorizedRelease {
    fn same_identity(&self, other: &Self) -> bool {
        self.channel == other.channel
            && self.version == other.version
            && self.digest == other.digest
            && self.size == other.size
            && self.build_id == other.build_id
            && self.min_updater == other.min_updater
            && self.target == other.target
    }
}

/// The signed channel document (the bytes that carry the ed25519 signature).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChannelDocument {
    schema: u32,
    channel: Channel,
    sequence: u64,
    expires: u64,
    version: Version,
    digest: String,
    size: u64,
    build_id: String,
    min_updater: Version,
    target: String,
}

/// Public keys the client trusts, pinned out-of-band (`/etc/zerodeck/trusted-root.json`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustedKeys {
    keys: Vec<String>,
}

/// Where the two channel files come from: HTTPS in production, memory in tests.
pub trait ChannelSource {
    /// Fetches at most [`MAX_CHANNEL_BYTES`] from `url`.
    fn fetch(&self, url: &Url) -> impl Future<Output = Result<Vec<u8>, UpdateError>> + Send;
}

/// Bounded HTTPS fetcher for channel files.
#[derive(Debug, Clone)]
pub struct HttpChannelSource {
    client: reqwest::Client,
}

impl HttpChannelSource {
    /// Builds a client with connect and total timeouts.
    pub fn new() -> Result<Self, UpdateError> {
        let client = reqwest::Client::builder()
            .timeout(METADATA_TIMEOUT)
            .connect_timeout(METADATA_CONNECT_TIMEOUT)
            .build()
            .map_err(|error| UpdateError::ChannelFetch {
                url: String::new(),
                reason: error.to_string(),
            })?;
        Ok(Self { client })
    }
}

impl ChannelSource for HttpChannelSource {
    async fn fetch(&self, url: &Url) -> Result<Vec<u8>, UpdateError> {
        let fail = |reason: String| UpdateError::ChannelFetch {
            url: url.to_string(),
            reason,
        };
        let mut response = self
            .client
            .get(url.clone())
            .send()
            .await
            .map_err(|error| fail(error.to_string()))?;
        if !response.status().is_success() {
            return Err(fail(format!("HTTP status {}", response.status())));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| fail(error.to_string()))?
        {
            if body.len() + chunk.len() > MAX_CHANNEL_BYTES {
                return Err(fail(format!("response exceeds {MAX_CHANNEL_BYTES} bytes")));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

/// Signed channel catalog plus its locally persisted verification state.
#[derive(Debug, Clone)]
pub struct Catalog {
    release: Option<AuthorizedRelease>,
    state: CatalogStateStore,
    verified_at: u64,
    expires_at: u64,
}

impl Catalog {
    /// Loads and verifies `channel` over HTTPS.
    ///
    /// `trusted_keys` is the pinned keys document; this method never trusts a key it fetched.
    pub async fn load(
        update_root: impl AsRef<Path>,
        channels_base_url: Url,
        releases_base_url: Url,
        trusted_keys: &[u8],
        channel: Channel,
    ) -> Result<Self, UpdateError> {
        let source = HttpChannelSource::new()?;
        Self::load_with_source(
            update_root,
            channels_base_url,
            releases_base_url,
            trusted_keys,
            channel,
            &source,
        )
        .await
    }

    /// Loads `channel` from `source`; tests use an in-memory source.
    #[doc(hidden)]
    pub async fn load_with_source<S: ChannelSource>(
        update_root: impl AsRef<Path>,
        channels_base_url: Url,
        releases_base_url: Url,
        trusted_keys: &[u8],
        channel: Channel,
        source: &S,
    ) -> Result<Self, UpdateError> {
        let update_root = update_root.as_ref();
        fs::create_dir_all(update_root).map_err(|source| UpdateError::Io {
            path: update_root.to_path_buf(),
            source,
        })?;
        let update_root = fs::canonicalize(update_root).map_err(|source| UpdateError::Io {
            path: update_root.to_path_buf(),
            source,
        })?;
        let state_store =
            CatalogStateStore::new(update_root.join(STATE_DIR).join(CATALOG_STATE_FILE));

        let previous = state_store.read()?;
        let now = now_seconds()?;
        if let Some(floor) = previous.last_verified_at
            && now < floor
        {
            return Err(UpdateError::ClockRollback { now, floor });
        }

        let keys = parse_trusted_keys(trusted_keys)?;
        let channels_base_url = directory_url(channels_base_url)?;
        let releases_base_url = directory_url(releases_base_url)?;
        let document_url = join(&channels_base_url, &format!("{channel}.json"))?;
        let signature_url = join(&channels_base_url, &format!("{channel}.json.sig"))?;
        let document_bytes = source.fetch(&document_url).await?;
        let signature_bytes = source.fetch(&signature_url).await?;
        verify_signature(&keys, &document_bytes, &signature_bytes)?;

        let document: ChannelDocument = serde_json::from_slice(&document_bytes)
            .map_err(|error| UpdateError::ChannelInvalid(format!("malformed document: {error}")))?;
        if document.schema != CHANNEL_SCHEMA {
            return Err(UpdateError::ChannelInvalid(format!(
                "unsupported schema {}",
                document.schema
            )));
        }
        if document.channel != channel {
            return Err(UpdateError::ChannelInvalid(format!(
                "document is for `{}`, requested `{channel}`",
                document.channel
            )));
        }
        if document.expires <= now {
            return Err(UpdateError::AuthorizationExpired {
                expires: document.expires,
            });
        }
        let floor = previous
            .channel_sequences
            .get(channel.as_str())
            .copied()
            .unwrap_or(0);
        if document.sequence < floor {
            return Err(UpdateError::ChannelRollback {
                channel: channel.as_str(),
                sequence: document.sequence,
                floor,
            });
        }
        let release = release_from_document(&document, &releases_base_url)?;

        let authorization = previous
            .authorization
            .filter(|authorization| authorization.same_identity(&release));
        let selection = previous.selection.filter(|_| authorization.is_some());
        let mut channel_sequences = previous.channel_sequences;
        channel_sequences.insert(channel.as_str().to_owned(), document.sequence);
        state_store.write(&CatalogState {
            schema: CATALOG_STATE_SCHEMA,
            last_verified_at: Some(now),
            expires_at: Some(document.expires),
            authorization,
            selection,
            channel_sequences,
        })?;

        Ok(Self {
            release: Some(release),
            state: state_store,
            verified_at: now,
            expires_at: document.expires,
        })
    }

    /// Opens only the persisted authorization state for an offline apply.
    ///
    /// This never reads channel files or the network. Use [`Self::authorize_staged`] before an
    /// activation; [`Self::candidate`] requires a channel loaded with [`Self::load`].
    pub fn open_offline(update_root: impl AsRef<Path>) -> Result<Self, UpdateError> {
        let requested = update_root.as_ref();
        let root = fs::canonicalize(requested).map_err(|source| UpdateError::Io {
            path: requested.to_path_buf(),
            source,
        })?;
        // Parsing is deferred to the authorization check. Rollback never consults the catalog, so
        // corrupt or absent persisted metadata must not block an offline rollback; an upgrade
        // still fails closed when `authorize_staged` reads the document.
        let state = CatalogStateStore::new(root.join(STATE_DIR).join(CATALOG_STATE_FILE));
        let (verified_at, expires_at) = match state.read() {
            Ok(persisted) => (
                persisted.last_verified_at.unwrap_or(0),
                persisted.expires_at.unwrap_or(0),
            ),
            Err(_) => (0, 0),
        };
        Ok(Self {
            release: None,
            state,
            verified_at,
            expires_at,
        })
    }

    /// Returns the time of the last successful trusted metadata verification.
    #[must_use]
    pub fn verified_at(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(self.verified_at)
    }

    /// Returns the earliest expiry time of the trusted metadata roles.
    #[must_use]
    pub fn expires_at(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(self.expires_at)
    }

    /// Selects the highest eligible signed target for `policy`.
    ///
    /// A selected release is persisted as the freshest local authorization. Returning `None`
    /// leaves any previously selected authorization intact.
    pub fn candidate(
        &self,
        policy: &SelectionPolicy,
    ) -> Result<Option<AuthorizedRelease>, UpdateError> {
        let now = now_seconds()?;
        let state = self.state.read()?;
        self.ensure_fresh(&state, now)?;

        let selected = self.select(policy)?;
        if let Some(release) = &selected {
            let mut next = state;
            next.authorization = Some(release.clone());
            next.selection = Some(SelectionRecord {
                channel: policy.channel,
                updater_version: policy.updater_version.clone(),
                installed_version: policy
                    .installed
                    .as_ref()
                    .map(|installed| installed.version.clone()),
                installed_digest: policy
                    .installed
                    .as_ref()
                    .map(|installed| installed.digest.clone()),
            });
            self.state.write(&next)?;
        }
        Ok(selected)
    }

    /// Selects without persisting a new authorization.
    ///
    /// `update --check-only` uses this so merely reporting a newer release cannot supersede the
    /// release that is already staged for activation.
    pub fn preview_candidate(
        &self,
        policy: &SelectionPolicy,
    ) -> Result<Option<AuthorizedRelease>, UpdateError> {
        let now = now_seconds()?;
        let state = self.state.read()?;
        self.ensure_fresh(&state, now)?;
        self.select(policy)
    }

    /// Returns the release selected by the freshest trusted metadata, without network access.
    pub fn persisted_authorization(&self) -> Result<Option<AuthorizedRelease>, UpdateError> {
        Ok(self.state.read()?.authorization)
    }

    /// Returns the persisted selection policy that produced the current authorization.
    pub fn persisted_selection(&self) -> Result<Option<SelectionRecord>, UpdateError> {
        Ok(self.state.read()?.selection)
    }

    /// Revalidates a staged release against the freshest local authorization without network access.
    pub fn authorize_staged(
        &self,
        release: &AuthorizedRelease,
        now: SystemTime,
    ) -> Result<(), UpdateError> {
        let now = unix_seconds(now)?;
        let state = self.state.read()?;
        let floor = state
            .last_verified_at
            .ok_or(UpdateError::CatalogNotVerified)?;
        if now < floor {
            return Err(UpdateError::ClockRollback { now, floor });
        }
        let expires = state.expires_at.ok_or(UpdateError::CatalogNotVerified)?;
        if now >= expires {
            return Err(UpdateError::AuthorizationExpired { expires });
        }

        match state.authorization {
            Some(authorization) if authorization.same_identity(release) => Ok(()),
            Some(_) => Err(UpdateError::ReleaseSuperseded {
                version: release.version.clone(),
                digest: release.digest.clone(),
            }),
            None => Err(UpdateError::ReleaseNotAuthorized {
                version: release.version.clone(),
                digest: release.digest.clone(),
            }),
        }
    }

    fn ensure_fresh(&self, state: &CatalogState, now: u64) -> Result<(), UpdateError> {
        let floor = state
            .last_verified_at
            .ok_or(UpdateError::CatalogNotVerified)?;
        if now < floor {
            return Err(UpdateError::ClockRollback { now, floor });
        }
        let expires = state.expires_at.ok_or(UpdateError::CatalogNotVerified)?;
        if now >= expires {
            return Err(UpdateError::AuthorizationExpired { expires });
        }
        if floor != self.verified_at || expires != self.expires_at {
            return Err(UpdateError::StaleCatalog);
        }
        Ok(())
    }

    fn select(&self, policy: &SelectionPolicy) -> Result<Option<AuthorizedRelease>, UpdateError> {
        let release = self.release.as_ref().ok_or(UpdateError::CatalogNotLoaded)?;
        if release.channel != policy.channel
            || (!release.version.pre.is_empty() && !policy.allow_prerelease)
            || release.min_updater > policy.updater_version
            || policy.quarantined.contains(&release.digest)
        {
            return Ok(None);
        }
        if let Some(installed) = &policy.installed {
            if release.version < installed.version {
                return Ok(None);
            }
            if release.version == installed.version {
                if release.digest != installed.digest {
                    return Err(UpdateError::VersionConflict {
                        version: release.version.clone(),
                        installed: installed.digest.clone(),
                        candidate: release.digest.clone(),
                    });
                }
                return Ok(None);
            }
        }
        Ok(Some(release.clone()))
    }
}

fn parse_trusted_keys(bytes: &[u8]) -> Result<Vec<VerifyingKey>, UpdateError> {
    let invalid = |reason: String| UpdateError::InvalidTrustedKeys(reason);
    let document: TrustedKeys = serde_json::from_slice(bytes)
        .map_err(|error| invalid(format!("malformed keys document: {error}")))?;
    if document.keys.is_empty() {
        return Err(invalid("the keys document lists no key".to_owned()));
    }
    document
        .keys
        .iter()
        .map(|key| {
            let raw: [u8; 32] = hex::decode(key)
                .ok()
                .and_then(|raw| raw.try_into().ok())
                .ok_or_else(|| invalid(format!("`{key}` is not a 32-byte hex key")))?;
            VerifyingKey::from_bytes(&raw).map_err(|error| invalid(error.to_string()))
        })
        .collect()
}

fn verify_signature(
    keys: &[VerifyingKey],
    document: &[u8],
    signature_hex: &[u8],
) -> Result<(), UpdateError> {
    let invalid = |reason: &str| UpdateError::ChannelSignature(reason.to_owned());
    let raw: [u8; 64] = hex::decode(String::from_utf8_lossy(signature_hex).trim())
        .ok()
        .and_then(|raw| raw.try_into().ok())
        .ok_or_else(|| invalid("the signature is not 64 bytes of hex"))?;
    let signature = Signature::from_bytes(&raw);
    keys.iter()
        .any(|key| key.verify_strict(document, &signature).is_ok())
        .then_some(())
        .ok_or_else(|| invalid("no trusted key verifies the channel document"))
}

fn release_from_document(
    document: &ChannelDocument,
    releases_base_url: &Url,
) -> Result<AuthorizedRelease, UpdateError> {
    let invalid = |reason: String| UpdateError::InvalidLoaderTarget {
        target: document.target.clone(),
        reason,
    };
    let safe_segment = |segment: &str| {
        !segment.is_empty()
            && segment != "."
            && segment != ".."
            && segment
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    };
    let segments: Vec<&str> = document.target.split('/').collect();
    if segments.len() != 2 || !segments.iter().all(|segment| safe_segment(segment)) {
        return Err(invalid("target must be `<tag>/<asset>`".to_owned()));
    }
    if document.build_id.trim().is_empty() {
        return Err(invalid("build_id is empty".to_owned()));
    }
    let digest = GenerationId::new(document.digest.clone())?;
    let url = join(releases_base_url, &document.target)?;
    Ok(AuthorizedRelease {
        channel: document.channel,
        version: document.version.clone(),
        digest,
        size: document.size,
        build_id: document.build_id.clone(),
        min_updater: document.min_updater.clone(),
        target: document.target.clone(),
        url: Some(url),
    })
}

fn join(base: &Url, path: &str) -> Result<Url, UpdateError> {
    base.join(path).map_err(|error| UpdateError::InvalidBaseUrl {
        url: base.to_string(),
        reason: error.to_string(),
    })
}

fn directory_url(url: Url) -> Result<Url, UpdateError> {
    if url.as_str().ends_with('/') {
        return Ok(url);
    }
    let original = url.to_string();
    let mut text = original.clone();
    text.push('/');
    Url::parse(&text).map_err(|error| UpdateError::InvalidBaseUrl {
        url: original,
        reason: error.to_string(),
    })
}

fn now_seconds() -> Result<u64, UpdateError> {
    unix_seconds(SystemTime::now())
}

fn unix_seconds(time: SystemTime) -> Result<u64, UpdateError> {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| UpdateError::ClockBeforeEpoch)
}

#[cfg(test)]
mod tests {
    use url::Url;

    use super::{Channel, directory_url};

    #[test]
    fn channel_parses_only_the_signed_loader_channels() {
        assert_eq!(Channel::parse("stable"), Some(Channel::Stable));
        assert_eq!(Channel::parse("testing"), Some(Channel::Testing));
        assert_eq!(Channel::parse("Stable"), None);
        assert_eq!(Channel::parse("nightly"), None);
    }

    #[test]
    fn base_urls_are_normalized_as_directories() {
        let base = Url::parse("https://updates.example.test/loader").expect("valid base");
        let normalized = directory_url(base).expect("normalize base url");
        assert_eq!(normalized.as_str(), "https://updates.example.test/loader/");
        assert_eq!(
            normalized
                .join("linux-x86_64/zerodeck-0.1.2.tar.gz")
                .expect("join target")
                .as_str(),
            "https://updates.example.test/loader/linux-x86_64/zerodeck-0.1.2.tar.gz"
        );
    }
}
