use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use reqwest::Method;
use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::warn;
use url::Url;
use uuid::Uuid;
use zerodeck_platform::{PlatformError, PlatformPaths, SafePath, SafePathSet, atomic_write};
use zerodeck_protocol::{PackageManifest, PluginManifest};

use crate::blocking::run_blocking;
use crate::{
    CheckedHttpClient, CoreError, DEFAULT_MAX_REDIRECTS, ExtractLimits, InstallSource,
    extract_zip_safely_with_limits, send_checked_request, verify_sha256_policy,
};

/// Default maximum compressed plugin ZIP archive size accepted by the installer (128 MiB).
pub const DEFAULT_MAX_ZIP_DOWNLOAD_BYTES: usize = 128 * 1024 * 1024;

/// Default maximum size of a single `remote_binary` download (256 MiB).
pub const DEFAULT_MAX_REMOTE_BINARY_BYTES: usize = 256 * 1024 * 1024;

/// Source of a plugin archive to stage and install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallPluginSource {
    /// Download ZIP archive over HTTP/HTTPS subject to SSRF and SHA-256 policy.
    RemoteZip { url: String, sha256: Option<String> },
    /// Load ZIP archive from a local path inside [`PlatformPaths::filepicker_roots`].
    LocalZip {
        path: PathBuf,
        sha256: Option<String>,
        user_confirmed: bool,
    },
}

/// Request parameters for staging a plugin archive via [`prepare_staged_plugin`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPluginRequest {
    pub source: InstallPluginSource,
    pub directory_hint: Option<String>,
    pub expected_name: Option<String>,
    pub expected_version: Option<String>,
}

/// Summary returned after committing a staged plugin installation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallOutcome {
    pub name: String,
    pub directory: String,
    pub version: Option<String>,
    pub sha256: String,
    pub replaced_existing: bool,
}

/// Resource bounds enforced by [`prepare_staged_plugin`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallerLimits {
    pub max_zip_download_bytes: usize,
    pub max_remote_binary_bytes: usize,
    pub max_redirects: usize,
}

impl Default for InstallerLimits {
    fn default() -> Self {
        Self {
            max_zip_download_bytes: DEFAULT_MAX_ZIP_DOWNLOAD_BYTES,
            max_remote_binary_bytes: DEFAULT_MAX_REMOTE_BINARY_BYTES,
            max_redirects: DEFAULT_MAX_REDIRECTS,
        }
    }
}

/// Fully validated, extracted, and `remote_binary`-populated plugin waiting to be swapped into
/// `plugins/<directory>` by [`commit_staged_plugin`].
///
/// Dropping an uncommitted [`StagedPluginInstall`] automatically removes its staging directory.
#[derive(Debug)]
pub struct StagedPluginInstall {
    manifest: PluginManifest,
    package: Option<PackageManifest>,
    zip_sha256: String,
    wrapper_dir: Option<String>,
    directory_hint: Option<String>,
    staging_root: PathBuf,
    plugin_stage_dir: PathBuf,
    committed: bool,
}

impl StagedPluginInstall {
    /// Parsed `plugin.json` manifest of the staged plugin.
    #[must_use]
    pub fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    /// Optional parsed `package.json` manifest of the staged plugin.
    #[must_use]
    pub fn package(&self) -> Option<&PackageManifest> {
        self.package.as_ref()
    }

    /// Lowercase hex SHA-256 digest of the source plugin ZIP archive.
    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.zip_sha256
    }

    /// Single top-level archive wrapper directory unwrapped during staging, if any.
    #[must_use]
    pub fn wrapper_dir(&self) -> Option<&str> {
        self.wrapper_dir.as_deref()
    }

    /// Derives a sanitized default plugin directory name from `directory_hint` (when present),
    /// otherwise the unwrapped single top-level folder name, otherwise `manifest.name`.
    #[must_use]
    pub fn suggested_directory(&self) -> String {
        if let Some(hint) = self.directory_hint.as_deref() {
            return sanitize_directory_name(hint);
        }
        if let Some(wrapper) = self.wrapper_dir.as_deref() {
            return sanitize_directory_name(wrapper);
        }
        sanitize_directory_name(&self.manifest.name)
    }
}

impl Drop for StagedPluginInstall {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_dir_all(&self.staging_root);
        }
    }
}

#[derive(Debug)]
struct StagingDirGuard {
    path: PathBuf,
    active: bool,
}

impl StagingDirGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, active: true }
    }

    fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for StagingDirGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallTypeHint {
    Any,
    Remote,
    Local,
}

/// Parses a Decky/ZeroDeck install source string (`http(s)://`, `file://`, or local filesystem
/// path) alongside optional `install_type` (`"remote"`, `"local"`, or Decky `InstallType` codes
/// `0`/`1`/`2`).
pub fn parse_install_source(
    url_or_path: &str,
    sha256: Option<String>,
    install_type: Option<&Value>,
    user_confirmed: bool,
) -> Result<InstallPluginSource, CoreError> {
    let raw = url_or_path.trim();
    if raw.is_empty() || raw.contains('\0') {
        return Err(CoreError::InvalidInstallSource(
            "install source must not be empty or contain null bytes".to_owned(),
        ));
    }

    let hint = parse_install_type_hint(install_type)?;

    if let Ok(parsed) = Url::parse(raw) {
        return match parsed.scheme() {
            "http" | "https" => {
                if hint == InstallTypeHint::Local {
                    return Err(CoreError::InvalidInstallSource(
                        "remote HTTP(S) URL cannot be used with `local` install_type".to_owned(),
                    ));
                }
                Ok(InstallPluginSource::RemoteZip {
                    url: raw.to_owned(),
                    sha256,
                })
            }
            "file" => {
                if hint == InstallTypeHint::Remote {
                    return Err(CoreError::InvalidInstallSource(
                        "file:// URI cannot be used with `remote` install_type".to_owned(),
                    ));
                }
                let path = parsed.to_file_path().map_err(|()| {
                    CoreError::InvalidInstallSource(format!("invalid file:// URI `{raw}`"))
                })?;
                if path_contains_nul(&path) {
                    return Err(CoreError::InvalidInstallSource(
                        "file:// URI resolves to a path containing null bytes".to_owned(),
                    ));
                }
                Ok(InstallPluginSource::LocalZip {
                    path,
                    sha256,
                    user_confirmed,
                })
            }
            scheme if scheme.len() > 1 => Err(CoreError::InvalidInstallSource(format!(
                "unsupported install URI scheme `{scheme}`"
            ))),
            _ => parse_bare_local_path(raw, sha256, hint, user_confirmed),
        };
    }

    if raw.contains("://") {
        return Err(CoreError::InvalidInstallSource(format!(
            "malformed install URI `{raw}`"
        )));
    }

    parse_bare_local_path(raw, sha256, hint, user_confirmed)
}

fn parse_install_type_hint(install_type: Option<&Value>) -> Result<InstallTypeHint, CoreError> {
    match install_type {
        None | Some(Value::Null) => Ok(InstallTypeHint::Any),
        Some(Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "remote" => Ok(InstallTypeHint::Remote),
            "local" => Ok(InstallTypeHint::Local),
            other => Err(CoreError::InvalidInstallSource(format!(
                "unsupported install_type `{other}`"
            ))),
        },
        Some(Value::Number(num)) => match num.as_u64() {
            Some(0..=2) => Ok(InstallTypeHint::Any),
            _ => Err(CoreError::InvalidInstallSource(format!(
                "unsupported Decky InstallType code `{num}`"
            ))),
        },
        Some(other) => Err(CoreError::InvalidInstallSource(format!(
            "invalid install_type value `{other}`"
        ))),
    }
}

fn parse_bare_local_path(
    raw: &str,
    sha256: Option<String>,
    hint: InstallTypeHint,
    user_confirmed: bool,
) -> Result<InstallPluginSource, CoreError> {
    if hint == InstallTypeHint::Remote {
        return Err(CoreError::InvalidInstallSource(
            "local filesystem path cannot be used with `remote` install_type".to_owned(),
        ));
    }
    Ok(InstallPluginSource::LocalZip {
        path: PathBuf::from(raw),
        sha256,
        user_confirmed,
    })
}

fn path_contains_nul(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().contains(&0)
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy().contains('\0')
    }
}

/// Sanitizes an arbitrary label into a safe directory name inside `plugins/`.
///
/// Guarantees only ASCII alphanumeric characters plus `-`, `_`, and `.` are kept, no `..`
/// sequences or path separators appear, the result never starts with `.`, and the returned string
/// is never empty.
#[must_use]
pub fn sanitize_directory_name(raw: &str) -> String {
    let mut cleaned = String::with_capacity(raw.len());
    let mut prev_dot = false;
    let mut prev_dash = false;

    for ch in raw.chars() {
        if ch == '.' {
            if !prev_dot {
                cleaned.push('.');
                prev_dot = true;
            }
            prev_dash = false;
        } else if ch.is_ascii_alphanumeric() || ch == '_' {
            cleaned.push(ch);
            prev_dot = false;
            prev_dash = false;
        } else if !prev_dash {
            cleaned.push('-');
            prev_dash = true;
            prev_dot = false;
        }
    }

    let trimmed = cleaned.trim_matches(['.', '-', '_']);
    if trimmed.is_empty() {
        "plugin".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Fetches, verifies, extracts, unwraps, and populates any `remote_binary` assets into an isolated
/// staging directory inside `paths.plugins_dir()`.
///
/// Never mutates an existing plugin installation; callers finalize the installation with
/// [`commit_staged_plugin`]. Any failure leaves no staging leftovers on disk.
pub async fn prepare_staged_plugin(
    paths: &PlatformPaths,
    client: &CheckedHttpClient,
    limits: &InstallerLimits,
    req: &InstallPluginRequest,
    allow_unverified: bool,
) -> Result<StagedPluginInstall, CoreError> {
    let directory_hint = normalize_directory_hint(req.directory_hint.as_deref())?;

    let (zip_bytes, zip_sha256) =
        fetch_and_verify_archive(paths, client, limits, &req.source, allow_unverified).await?;

    let plugins_dir = paths.plugins_dir();
    let expected_name = req.expected_name.clone();
    let expected_version = req.expected_version.clone();
    let (staging_root, plugin_stage_dir, wrapper_dir, manifest, package, mut guard) =
        run_blocking(move || {
            extract_into_staging(
                &plugins_dir,
                &zip_bytes,
                expected_name.as_deref(),
                expected_version.as_deref(),
            )
        })
        .await?;

    if let Some(pkg) = package.as_ref()
        && !pkg.remote_binary.is_empty()
    {
        stage_remote_binaries(
            &plugin_stage_dir,
            &pkg.remote_binary,
            client,
            limits,
            allow_unverified,
        )
        .await?;
    }

    guard.disarm();
    Ok(StagedPluginInstall {
        manifest,
        package,
        zip_sha256,
        wrapper_dir,
        directory_hint,
        staging_root,
        plugin_stage_dir,
        committed: false,
    })
}

/// Staging directory, unwrapped plugin directory, wrapper folder, manifests, and the guard that
/// removes the staging directory unless disarmed.
type ExtractedStaging = (
    PathBuf,
    PathBuf,
    Option<String>,
    PluginManifest,
    Option<PackageManifest>,
    StagingDirGuard,
);

/// Blocking half of [`prepare_staged_plugin`]: creates the staging directory, extracts and
/// unwraps the archive, and validates the manifests.
fn extract_into_staging(
    plugins_dir: &Path,
    zip_bytes: &[u8],
    expected_name: Option<&str>,
    expected_version: Option<&str>,
) -> Result<ExtractedStaging, CoreError> {
    ensure_dir_0755(plugins_dir)?;
    let canonical_plugins_dir = fs::canonicalize(plugins_dir).map_err(|source| CoreError::Io {
        path: plugins_dir.to_path_buf(),
        source,
    })?;
    let staging_root = canonical_plugins_dir.join(format!(".{}.extract-staging", Uuid::new_v4()));
    fs::create_dir(&staging_root).map_err(|source| CoreError::Io {
        path: staging_root.clone(),
        source,
    })?;
    set_dir_permissions(&staging_root)?;
    let guard = StagingDirGuard::new(staging_root.clone());

    let raw_dir = staging_root.join("raw");
    extract_zip_safely_with_limits(zip_bytes, &raw_dir, &ExtractLimits::default())?;

    let plugin_stage_dir = staging_root.join("plugin");
    let wrapper_dir = unwrap_extracted_plugin(&raw_dir, &plugin_stage_dir)?;
    set_dir_permissions(&plugin_stage_dir)?;

    let (manifest, package) =
        read_and_validate_manifests(&plugin_stage_dir, expected_name, expected_version)?;
    Ok((
        staging_root,
        plugin_stage_dir,
        wrapper_dir,
        manifest,
        package,
        guard,
    ))
}

/// Atomically moves a [`StagedPluginInstall`] into `target_dir`, parking and restoring any
/// existing installation if replacement fails.
pub fn commit_staged_plugin(
    mut staged: StagedPluginInstall,
    target_dir: &Path,
) -> Result<InstallOutcome, CoreError> {
    let (parent, dir_name) = split_target_dir(target_dir)?;
    ensure_dir_0755(&parent)?;
    let canonical_parent = fs::canonicalize(&parent).map_err(|source| CoreError::Io {
        path: parent,
        source,
    })?;
    let final_dest = canonical_parent.join(&dir_name);

    set_dir_permissions(&staged.plugin_stage_dir)?;

    let replaced_existing = swap_staged_into_place(
        &staged.plugin_stage_dir,
        &final_dest,
        &canonical_parent,
        &dir_name,
    )?;

    staged.committed = true;
    if let Err(err) = fs::remove_dir_all(&staged.staging_root) {
        warn!(
            path = %staged.staging_root.display(),
            error = %err,
            "failed to remove empty staging root after commit"
        );
    }

    Ok(InstallOutcome {
        name: staged.manifest.name.clone(),
        directory: dir_name,
        version: staged.package.as_ref().and_then(|pkg| pkg.version.clone()),
        sha256: staged.zip_sha256.clone(),
        replaced_existing,
    })
}

fn normalize_directory_hint(raw: Option<&str>) -> Result<Option<String>, CoreError> {
    let Some(trimmed) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if trimmed == "."
        || trimmed == ".."
        || trimmed.starts_with('.')
        || trimmed.contains("..")
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.contains('\0')
    {
        return Err(CoreError::InvalidInstallSource(format!(
            "invalid directory_hint `{trimmed}`"
        )));
    }
    let sanitized = sanitize_directory_name(trimmed);
    if sanitized.is_empty() {
        return Err(CoreError::InvalidInstallSource(format!(
            "invalid directory_hint `{trimmed}`"
        )));
    }
    Ok(Some(sanitized))
}

async fn fetch_and_verify_archive(
    paths: &PlatformPaths,
    client: &CheckedHttpClient,
    limits: &InstallerLimits,
    source: &InstallPluginSource,
    allow_unverified: bool,
) -> Result<(Vec<u8>, String), CoreError> {
    match source {
        InstallPluginSource::RemoteZip { url, sha256 } => {
            client.policy().validate_url(url)?;
            if !has_non_empty_sha256(sha256.as_deref()) && !allow_unverified {
                return Err(CoreError::MissingSha256);
            }
            let bytes = download_bounded_bytes(
                client,
                url,
                limits.max_zip_download_bytes,
                limits.max_redirects,
            )
            .await?;
            let policy_source = InstallSource::Remote {
                sha256: sha256.clone(),
            };
            run_blocking(move || {
                let digest = verify_sha256_policy(&bytes, &policy_source, allow_unverified)?;
                Ok((bytes, digest))
            })
            .await
        }
        InstallPluginSource::LocalZip {
            path,
            sha256,
            user_confirmed,
        } => {
            let safe_set = SafePathSet::from_existing(paths.filepicker_roots())?;
            let canonical_path = safe_set.resolve(path)?;
            if !has_non_empty_sha256(sha256.as_deref()) && !user_confirmed && !allow_unverified {
                return Err(CoreError::ConfirmationRequired);
            }
            let policy_source = InstallSource::LocalFile {
                sha256: sha256.clone(),
                user_confirmed: *user_confirmed,
            };
            let max_bytes = limits.max_zip_download_bytes;
            run_blocking(move || {
                let bytes = read_bounded_local_file(&canonical_path, max_bytes)?;
                let digest = verify_sha256_policy(&bytes, &policy_source, allow_unverified)?;
                Ok((bytes, digest))
            })
            .await
        }
    }
}

fn has_non_empty_sha256(sha256: Option<&str>) -> bool {
    sha256.map(str::trim).is_some_and(|s| !s.is_empty())
}

fn read_bounded_local_file(path: &Path, max_bytes: usize) -> Result<Vec<u8>, CoreError> {
    let meta = fs::metadata(path).map_err(|source| CoreError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if !meta.is_file() {
        return Err(CoreError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "local install source is not a regular file",
            ),
        });
    }
    if meta.len() > max_bytes as u64 {
        return Err(CoreError::DownloadSizeExceeded { max_bytes });
    }

    let file = File::open(path).map_err(|source| CoreError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let cap = usize::try_from(meta.len())
        .unwrap_or(max_bytes)
        .min(max_bytes);
    let mut buf = Vec::with_capacity(cap);
    let mut limited = file.take((max_bytes as u64).saturating_add(1));
    limited
        .read_to_end(&mut buf)
        .map_err(|source| CoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    if buf.len() > max_bytes {
        return Err(CoreError::DownloadSizeExceeded { max_bytes });
    }
    Ok(buf)
}

async fn download_bounded_bytes(
    client: &CheckedHttpClient,
    url: &str,
    max_bytes: usize,
    max_redirects: usize,
) -> Result<Vec<u8>, CoreError> {
    let mut response = send_checked_request(
        client,
        Method::GET,
        url,
        HeaderMap::new(),
        None,
        max_redirects,
    )
    .await?;

    let status = response.status();
    if !status.is_success() {
        return Err(CoreError::HttpRequest(format!(
            "unexpected HTTP status {status} downloading `{}`",
            response.url()
        )));
    }

    if let Some(content_len) = response.content_length()
        && content_len > max_bytes as u64
    {
        return Err(CoreError::DownloadSizeExceeded { max_bytes });
    }

    let cap = response
        .content_length()
        .and_then(|len| usize::try_from(len).ok())
        .map_or(0, |len| len.min(max_bytes).min(1024 * 1024));
    let mut buf = Vec::with_capacity(cap);

    while let Some(chunk) = response.chunk().await.map_err(|err| {
        if err.is_timeout() {
            CoreError::HttpTimeout(format!(
                "timed out reading response body from `{url}`: {err}"
            ))
        } else {
            CoreError::HttpRequest(format!("failed reading response body from `{url}`: {err}"))
        }
    })? {
        if chunk.len() > max_bytes.saturating_sub(buf.len()) {
            return Err(CoreError::DownloadSizeExceeded { max_bytes });
        }
        buf.extend_from_slice(&chunk);
    }

    Ok(buf)
}

fn unwrap_extracted_plugin(
    raw_dir: &Path,
    plugin_stage_dir: &Path,
) -> Result<Option<String>, CoreError> {
    let io_err = |path: &Path| {
        let path = path.to_path_buf();
        move |source| CoreError::Io { path, source }
    };

    let root_manifest = raw_dir.join("plugin.json");
    match fs::symlink_metadata(&root_manifest) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_file() {
                return Err(CoreError::MissingPluginManifest);
            }
            let macosx_dir = raw_dir.join("__MACOSX");
            if let Ok(macosx_meta) = fs::symlink_metadata(&macosx_dir) {
                if macosx_meta.is_dir() {
                    let _ = fs::remove_dir_all(&macosx_dir);
                } else {
                    let _ = fs::remove_file(&macosx_dir);
                }
            }
            fs::rename(raw_dir, plugin_stage_dir).map_err(io_err(plugin_stage_dir))?;
            return Ok(None);
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(CoreError::Io {
                path: root_manifest,
                source,
            });
        }
    }

    let mut top_level = Vec::new();
    for entry_res in fs::read_dir(raw_dir).map_err(io_err(raw_dir))? {
        let entry = entry_res.map_err(io_err(raw_dir))?;
        let name = entry.file_name();
        if name == "__MACOSX" {
            continue;
        }
        top_level.push(entry);
    }

    if top_level.len() != 1 {
        return Err(CoreError::MissingPluginManifest);
    }

    let wrapper = &top_level[0];
    let wrapper_path = wrapper.path();
    let wrapper_meta = fs::symlink_metadata(&wrapper_path).map_err(io_err(&wrapper_path))?;
    if wrapper_meta.file_type().is_symlink() || !wrapper_meta.is_dir() {
        return Err(CoreError::MissingPluginManifest);
    }

    let wrapped_manifest = wrapper_path.join("plugin.json");
    let Ok(manifest_meta) = fs::symlink_metadata(&wrapped_manifest) else {
        return Err(CoreError::MissingPluginManifest);
    };
    if manifest_meta.file_type().is_symlink() || !manifest_meta.is_file() {
        return Err(CoreError::MissingPluginManifest);
    }

    let wrapper_name = wrapper.file_name().to_string_lossy().into_owned();
    fs::rename(&wrapper_path, plugin_stage_dir).map_err(io_err(plugin_stage_dir))?;
    let _ = fs::remove_dir_all(raw_dir);
    Ok(Some(wrapper_name))
}

fn read_and_validate_manifests(
    plugin_dir: &Path,
    expected_name: Option<&str>,
    expected_version: Option<&str>,
) -> Result<(PluginManifest, Option<PackageManifest>), CoreError> {
    let plugin_json_path = plugin_dir.join("plugin.json");
    let plugin_raw = fs::read_to_string(&plugin_json_path).map_err(|source| CoreError::Io {
        path: plugin_json_path.clone(),
        source,
    })?;
    let manifest: PluginManifest =
        serde_json::from_str(&plugin_raw).map_err(|source| CoreError::ManifestParse {
            path: plugin_json_path,
            source,
        })?;
    if manifest.name.trim().is_empty() {
        return Err(CoreError::InvalidInstallSource(
            "plugin.json `name` must not be empty".to_owned(),
        ));
    }

    let package_json_path = plugin_dir.join("package.json");
    let package = match fs::symlink_metadata(&package_json_path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_file() {
                return Err(CoreError::Io {
                    path: package_json_path,
                    source: std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "package.json is not a regular file",
                    ),
                });
            }
            let pkg_raw =
                fs::read_to_string(&package_json_path).map_err(|source| CoreError::Io {
                    path: package_json_path.clone(),
                    source,
                })?;
            Some(
                serde_json::from_str::<PackageManifest>(&pkg_raw).map_err(|source| {
                    CoreError::ManifestParse {
                        path: package_json_path,
                        source,
                    }
                })?,
            )
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => {
            return Err(CoreError::Io {
                path: package_json_path,
                source,
            });
        }
    };

    if let Some(expected) = expected_name.filter(|s| !s.is_empty())
        && expected != manifest.name
    {
        return Err(CoreError::ManifestMismatch {
            field: "name",
            expected: expected.to_owned(),
            actual: manifest.name,
        });
    }

    if let Some(expected) = expected_version.filter(|s| !s.is_empty())
        && let Some(actual) = package
            .as_ref()
            .and_then(|pkg| pkg.version.as_deref())
            .filter(|s| !s.is_empty())
        && actual != expected
    {
        return Err(CoreError::ManifestMismatch {
            field: "version",
            expected: expected.to_owned(),
            actual: actual.to_owned(),
        });
    }

    Ok((manifest, package))
}

async fn stage_remote_binaries(
    plugin_stage_dir: &Path,
    specs: &[zerodeck_protocol::RemoteBinarySpec],
    client: &CheckedHttpClient,
    limits: &InstallerLimits,
    allow_unverified: bool,
) -> Result<(), CoreError> {
    let bin_dir = plugin_stage_dir.join("bin");
    match fs::symlink_metadata(&bin_dir) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
            return Err(CoreError::Io {
                path: bin_dir,
                source: std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "plugin bin path exists and is not a directory",
                ),
            });
        }
        Ok(_) => set_dir_permissions(&bin_dir)?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            ensure_dir_0755(&bin_dir)?;
        }
        Err(source) => {
            return Err(CoreError::Io {
                path: bin_dir,
                source,
            });
        }
    }

    let bin_jail = SafePath::new(&bin_dir)?;

    for spec in specs {
        validate_single_filename(&spec.name, bin_jail.root())?;
        client.policy().validate_url(&spec.url)?;
        if !has_non_empty_sha256(spec.sha256hash.as_deref()) && !allow_unverified {
            return Err(CoreError::MissingSha256);
        }

        let bin_bytes = download_bounded_bytes(
            client,
            &spec.url,
            limits.max_remote_binary_bytes,
            limits.max_redirects,
        )
        .await?;
        let policy_source = InstallSource::Remote {
            sha256: spec.sha256hash.clone(),
        };
        let jail = bin_jail.clone();
        let name = spec.name.clone();
        run_blocking(move || {
            verify_sha256_policy(&bin_bytes, &policy_source, allow_unverified)?;
            write_remote_binary(&jail, &name, &bin_bytes)
        })
        .await?;
    }

    Ok(())
}

/// Writes one verified `remote_binary` into the `bin/` jail as an executable regular file.
fn write_remote_binary(bin_jail: &SafePath, name: &str, bytes: &[u8]) -> Result<(), CoreError> {
    let target = bin_jail.root().join(name);
    if let Ok(meta) = fs::symlink_metadata(&target)
        && (meta.file_type().is_symlink() || meta.is_dir())
    {
        return Err(CoreError::Platform(PlatformError::PathEscape {
            candidate: PathBuf::from(name),
            root: bin_jail.root().to_path_buf(),
        }));
    }

    atomic_write(&target, bytes)?;
    set_executable_permissions(&target)?;
    let _ = bin_jail.resolve(name)?;
    Ok(())
}

fn validate_single_filename(name: &str, jail_root: &Path) -> Result<(), CoreError> {
    let invalid = name.is_empty()
        || name == "."
        || name == ".."
        || name.starts_with('.')
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0');
    if invalid {
        return Err(CoreError::Platform(PlatformError::PathEscape {
            candidate: PathBuf::from(name),
            root: jail_root.to_path_buf(),
        }));
    }

    let mut components = Path::new(name).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => Ok(()),
        _ => Err(CoreError::Platform(PlatformError::PathEscape {
            candidate: PathBuf::from(name),
            root: jail_root.to_path_buf(),
        })),
    }
}

fn split_target_dir(target_dir: &Path) -> Result<(PathBuf, String), CoreError> {
    let invalid = || {
        CoreError::InvalidInstallSource(format!(
            "invalid plugin install target directory `{}`",
            target_dir.display()
        ))
    };

    let dir_name = target_dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(invalid)?
        .to_owned();

    if dir_name.is_empty()
        || dir_name == "."
        || dir_name == ".."
        || dir_name.starts_with('.')
        || dir_name.contains('/')
        || dir_name.contains('\\')
        || dir_name.contains('\0')
    {
        return Err(invalid());
    }

    let parent = target_dir
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);

    Ok((parent, dir_name))
}

fn swap_staged_into_place(
    plugin_stage_dir: &Path,
    final_dest: &Path,
    canonical_parent: &Path,
    dir_name: &str,
) -> Result<bool, CoreError> {
    let io_err = |path: &Path| {
        let path = path.to_path_buf();
        move |source| CoreError::Io { path, source }
    };

    let existing_meta = match fs::symlink_metadata(final_dest) {
        Ok(meta) => Some(meta),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => {
            return Err(CoreError::Io {
                path: final_dest.to_path_buf(),
                source,
            });
        }
    };

    let Some(existing_meta) = existing_meta else {
        fs::rename(plugin_stage_dir, final_dest).map_err(io_err(final_dest))?;
        return Ok(false);
    };

    let backup = canonical_parent.join(format!(".{dir_name}.previous-{}", Uuid::new_v4()));
    fs::rename(final_dest, &backup).map_err(io_err(final_dest))?;

    if let Err(source) = fs::rename(plugin_stage_dir, final_dest) {
        let _ = fs::rename(&backup, final_dest);
        return Err(CoreError::Io {
            path: final_dest.to_path_buf(),
            source,
        });
    }

    let removed = if existing_meta.is_dir() {
        fs::remove_dir_all(&backup)
    } else {
        fs::remove_file(&backup)
    };
    if let Err(err) = removed {
        warn!(path = %backup.display(), error = %err, "failed to remove previous plugin directory");
    }

    Ok(true)
}

fn ensure_dir_0755(path: &Path) -> Result<(), CoreError> {
    let mut missing = Vec::new();
    let mut cursor = path;
    while fs::symlink_metadata(cursor).is_err() {
        missing.push(cursor.to_path_buf());
        match cursor.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => cursor = parent,
            _ => break,
        }
    }

    for dir_to_create in missing.iter().rev() {
        match fs::create_dir(dir_to_create) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(source) => {
                return Err(CoreError::Io {
                    path: dir_to_create.clone(),
                    source,
                });
            }
        }
        set_dir_permissions(dir_to_create)?;
    }

    let meta = fs::metadata(path).map_err(|source| CoreError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if !meta.is_dir() {
        return Err(CoreError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "expected directory path, found non-directory",
            ),
        });
    }

    set_dir_permissions(path)
}

fn set_dir_permissions(path: &Path) -> Result<(), CoreError> {
    #[cfg(unix)]
    {
        set_unix_mode(path, 0o755)?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn set_executable_permissions(path: &Path) -> Result<(), CoreError> {
    #[cfg(unix)]
    {
        set_unix_mode(path, 0o755)?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(unix)]
fn set_unix_mode(path: &Path, mode: u32) -> Result<(), CoreError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|source| CoreError::Io {
        path: path.to_path_buf(),
        source,
    })
}
