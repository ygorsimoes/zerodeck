//! Pinned-root TUF catalog selection and offline authorization checks.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tough::schema::Target;
use tough::{
    HttpTransportBuilder, Limits, Repository, RepositoryLoader, TargetName, Transport,
    TransportError, TransportStream,
};
use url::Url;

use crate::state::{CATALOG_STATE_SCHEMA, CatalogState, CatalogStateStore, SelectionRecord};
use crate::{GenerationId, UpdateError};

/// Architecture directory used by the first supported native loader distribution.
pub const LOADER_TARGET_ARCH: &str = "linux-x86_64";

const TUF_DATASTORE_DIR: &str = "tuf";
const STATE_DIR: &str = "state";
const CATALOG_STATE_FILE: &str = "catalog.json";
const METADATA_TIMEOUT: Duration = Duration::from_secs(60);
const METADATA_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

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
        }
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

/// A loader release authorized by the freshest locally trusted TUF metadata.
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
    /// Expected TUF target path relative to the targets base URL.
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

/// Pinned-root TUF catalog plus its locally persisted verification state.
#[derive(Debug, Clone)]
pub struct Catalog {
    repository: Option<Repository>,
    targets_url: Option<Url>,
    state: CatalogStateStore,
    verified_at: u64,
    expires_at: u64,
}

impl Catalog {
    /// Loads trusted TUF metadata over the bounded production HTTP transport.
    ///
    /// `trusted_root` must be supplied out-of-band; this method never trusts the first root it
    /// sees.
    pub async fn load(
        update_root: impl AsRef<Path>,
        metadata_base_url: Url,
        targets_base_url: Url,
        trusted_root: &[u8],
    ) -> Result<Self, UpdateError> {
        let transport = HttpTransportBuilder::new()
            .timeout(METADATA_TIMEOUT)
            .connect_timeout(METADATA_CONNECT_TIMEOUT)
            .tries(3)
            .build();
        Self::load_with_transport(
            update_root,
            metadata_base_url,
            targets_base_url,
            trusted_root,
            transport,
        )
        .await
    }

    /// Loads trusted metadata using `transport`.
    ///
    /// This exists so offline tests can use [`tough::FilesystemTransport`]; production callers use
    /// [`Self::load`] and `tough`'s verified HTTP transport.
    #[doc(hidden)]
    pub async fn load_with_transport<T>(
        update_root: impl AsRef<Path>,
        metadata_base_url: Url,
        targets_base_url: Url,
        trusted_root: &[u8],
        transport: T,
    ) -> Result<Self, UpdateError>
    where
        T: Transport + Send + Sync + 'static,
    {
        let update_root = update_root.as_ref();
        fs::create_dir_all(update_root).map_err(|source| UpdateError::Io {
            path: update_root.to_path_buf(),
            source,
        })?;
        let update_root = fs::canonicalize(update_root).map_err(|source| UpdateError::Io {
            path: update_root.to_path_buf(),
            source,
        })?;
        let metadata_base_url = directory_url(metadata_base_url)?;
        let targets_base_url = directory_url(targets_base_url)?;
        let datastore = update_root.join(TUF_DATASTORE_DIR);
        fs::create_dir_all(&datastore).map_err(|source| UpdateError::Io {
            path: datastore.clone(),
            source,
        })?;
        let datastore = fs::canonicalize(&datastore).map_err(|source| UpdateError::Io {
            path: datastore.clone(),
            source,
        })?;
        if !datastore.starts_with(&update_root) {
            return Err(UpdateError::PathEscape {
                path: datastore,
                root: update_root,
            });
        }
        let state_store =
            CatalogStateStore::new(update_root.join(STATE_DIR).join(CATALOG_STATE_FILE));

        let previous = state_store.read()?;
        let now = now_seconds()?;
        if let Some(floor) = previous.last_verified_at
            && now < floor
        {
            return Err(UpdateError::ClockRollback { now, floor });
        }

        let limits = Limits {
            max_root_updates: 32,
            ..Limits::default()
        };
        let repository =
            RepositoryLoader::new(&trusted_root, metadata_base_url, targets_base_url.clone())
                .datastore(&datastore)
                .limits(limits)
                .transport(BoxedTransport(Arc::new(transport)))
                .load()
                .await?;
        sync_datastore(&datastore)?;

        let expires_at = earliest_expiry(&repository)?;
        if expires_at <= now {
            return Err(UpdateError::AuthorizationExpired {
                expires: expires_at,
            });
        }
        let authorization = previous.authorization.filter(|authorization| {
            repository_authorizes(&repository, &targets_base_url, authorization)
        });
        let selection = previous.selection.filter(|_| authorization.is_some());
        let state = CatalogState {
            schema: CATALOG_STATE_SCHEMA,
            last_verified_at: Some(now),
            expires_at: Some(expires_at),
            authorization,
            selection,
        };
        state_store.write(&state)?;

        Ok(Self {
            repository: Some(repository),
            targets_url: Some(targets_base_url),
            state: state_store,
            verified_at: now,
            expires_at,
        })
    }

    /// Opens only the persisted authorization state for an offline apply.
    ///
    /// This never reads trusted metadata or the network. Use [`Self::authorize_staged`] before an
    /// activation; [`Self::candidate`] requires metadata loaded with [`Self::load`].
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
        // Tolerate a corrupt document so rollback can construct this handle; selection and
        // authorization still parse it and fail closed.
        let (verified_at, expires_at) = match state.read() {
            Ok(persisted) => (
                persisted.last_verified_at.unwrap_or(0),
                persisted.expires_at.unwrap_or(0),
            ),
            Err(_) => (0, 0),
        };
        Ok(Self {
            repository: None,
            targets_url: None,
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
        let repository = self
            .repository
            .as_ref()
            .ok_or(UpdateError::CatalogNotLoaded)?;
        let targets_url = self
            .targets_url
            .as_ref()
            .ok_or(UpdateError::CatalogNotLoaded)?;
        let mut eligible: BTreeMap<Version, AuthorizedRelease> = BTreeMap::new();
        for (name, target) in repository.all_targets() {
            let Some(release) = release_from_target(name, target, targets_url)? else {
                continue;
            };
            if release.channel != policy.channel {
                continue;
            }
            if !release.version.pre.is_empty() && !policy.allow_prerelease {
                continue;
            }
            if release.min_updater > policy.updater_version {
                continue;
            }
            if let Some(installed) = &policy.installed {
                if release.version < installed.version {
                    continue;
                }
                if release.version == installed.version {
                    if release.digest != installed.digest {
                        return Err(UpdateError::VersionConflict {
                            version: release.version,
                            installed: installed.digest.clone(),
                            candidate: release.digest,
                        });
                    }
                    continue;
                }
            }
            if let Some(existing) = eligible.get(&release.version) {
                return Err(UpdateError::VersionConflict {
                    version: release.version,
                    installed: existing.digest.clone(),
                    candidate: release.digest,
                });
            }
            eligible.insert(release.version.clone(), release);
        }
        Ok(eligible.into_values().next_back())
    }
}

fn release_from_target(
    name: &TargetName,
    target: &Target,
    targets_url: &Url,
) -> Result<Option<AuthorizedRelease>, UpdateError> {
    let resolved = name.resolved();
    let Some(rest) = resolved.strip_prefix(LOADER_TARGET_ARCH) else {
        return Ok(None);
    };
    let Some(rest) = rest.strip_prefix('/') else {
        return Ok(None);
    };
    if name.raw() != resolved {
        return Err(UpdateError::InvalidLoaderTarget {
            target: name.raw().to_owned(),
            reason: "target path resolves to a different name".to_owned(),
        });
    }

    let Some(file_version) = rest
        .strip_prefix("zerodeck-")
        .and_then(|value| value.strip_suffix(".tar.gz"))
    else {
        return Ok(None);
    };
    if file_version.contains('/') {
        return Ok(None);
    }
    let version =
        Version::parse(file_version).map_err(|error| UpdateError::InvalidLoaderTarget {
            target: resolved.to_owned(),
            reason: format!("invalid SemVer in target name: {error}"),
        })?;

    let channel = custom_string(target, "channel", resolved).and_then(|value| {
        Channel::parse(value).ok_or_else(|| UpdateError::InvalidLoaderTarget {
            target: resolved.to_owned(),
            reason: format!("unsupported channel `{value}`"),
        })
    })?;
    let custom_version =
        Version::parse(custom_string(target, "version", resolved)?).map_err(|error| {
            UpdateError::InvalidLoaderTarget {
                target: resolved.to_owned(),
                reason: format!("invalid custom version: {error}"),
            }
        })?;
    if custom_version != version {
        return Err(UpdateError::InvalidLoaderTarget {
            target: resolved.to_owned(),
            reason: format!(
                "custom version `{custom_version}` does not match target name `{version}`"
            ),
        });
    }
    let build_id = custom_string(target, "build_id", resolved)?;
    if build_id.trim().is_empty() {
        return Err(UpdateError::InvalidLoaderTarget {
            target: resolved.to_owned(),
            reason: "custom build_id is empty".to_owned(),
        });
    }
    let min_updater =
        Version::parse(custom_string(target, "min_updater", resolved)?).map_err(|error| {
            UpdateError::InvalidLoaderTarget {
                target: resolved.to_owned(),
                reason: format!("invalid minimum updater version: {error}"),
            }
        })?;
    let digest = GenerationId::new(hex::encode(target.hashes.sha256.as_ref()))?;
    let url = targets_url
        .join(resolved)
        .map_err(|error| UpdateError::InvalidLoaderTarget {
            target: resolved.to_owned(),
            reason: format!("cannot build target URL: {error}"),
        })?;

    Ok(Some(AuthorizedRelease {
        channel,
        version,
        digest,
        size: target.length,
        build_id: build_id.to_owned(),
        min_updater,
        target: resolved.to_owned(),
        url: Some(url),
    }))
}

fn custom_string<'a>(
    target: &'a Target,
    key: &str,
    target_name: &str,
) -> Result<&'a str, UpdateError> {
    target
        .custom
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| UpdateError::InvalidLoaderTarget {
            target: target_name.to_owned(),
            reason: format!("signed target is missing string custom field `{key}`"),
        })
}

fn repository_authorizes(
    repository: &Repository,
    targets_url: &Url,
    authorization: &AuthorizedRelease,
) -> bool {
    repository
        .all_targets()
        .find(|(name, _)| name.raw() == authorization.target)
        .and_then(|(name, target)| {
            release_from_target(name, target, targets_url)
                .ok()
                .flatten()
        })
        .is_some_and(|release| release.same_identity(authorization))
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

fn sync_datastore(path: &Path) -> Result<(), UpdateError> {
    let entries = fs::read_dir(path).map_err(|source| UpdateError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| UpdateError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let file_type = entry.file_type().map_err(|source| UpdateError::Io {
            path: entry.path(),
            source,
        })?;
        if file_type.is_file() {
            File::open(entry.path())
                .and_then(|file| file.sync_all())
                .map_err(|source| UpdateError::Io {
                    path: entry.path(),
                    source,
                })?;
        } else if !file_type.is_dir() {
            return Err(UpdateError::Io {
                path: entry.path(),
                source: io::Error::other("unexpected entry in TUF datastore"),
            });
        }
    }
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| UpdateError::Io {
            path: path.to_path_buf(),
            source,
        })
}

fn earliest_expiry(repository: &Repository) -> Result<u64, UpdateError> {
    let expires = [
        repository.root().signed.expires,
        repository.timestamp().signed.expires,
        repository.snapshot().signed.expires,
        repository.targets().signed.expires,
    ]
    .into_iter()
    .min()
    .ok_or(UpdateError::CatalogNotVerified)?;
    u64::try_from(expires.as_second()).map_err(|_| UpdateError::ClockBeforeEpoch)
}

fn now_seconds() -> Result<u64, UpdateError> {
    unix_seconds(SystemTime::now())
}

fn unix_seconds(time: SystemTime) -> Result<u64, UpdateError> {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| UpdateError::ClockBeforeEpoch)
}

#[derive(Debug, Clone)]
struct BoxedTransport(Arc<dyn Transport + Send + Sync>);

#[tough::async_trait]
impl Transport for BoxedTransport {
    async fn fetch(&self, url: Url) -> Result<TransportStream, TransportError> {
        self.0.fetch(url).await
    }
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
