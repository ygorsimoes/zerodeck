//! Streamed, verified archive staging into an immutable generation.

use std::cell::Cell;
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use tar::EntryType;
use uuid::Uuid;
use zerodeck_core::{
    CheckedHttpClient, DEFAULT_MAX_ARCHIVE_BYTES, DEFAULT_MAX_REDIRECTS, ExtractLimits,
    OutboundHttpPolicy, send_checked_request,
};
use zerodeck_protocol::release::{RELEASE_MANIFEST, ReleaseManifest};

use crate::{AuthorizedRelease, GenerationId, GenerationStore, UpdateError};

/// Default maximum compressed archive bytes accepted while staging a loader release.
pub const DEFAULT_MAX_DOWNLOAD_BYTES: u64 = DEFAULT_MAX_ARCHIVE_BYTES;

const ARCHIVE_FILE: &str = "archive.tar.gz";
const TREE_DIR: &str = "tree";

/// Resource bounds enforced while downloading and extracting a release archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StageLimits {
    /// Maximum compressed bytes accepted from the archive server.
    pub max_download_bytes: u64,
    /// Maximum expanded bytes and entry count accepted from the archive.
    pub extract: ExtractLimits,
}

impl Default for StageLimits {
    fn default() -> Self {
        Self {
            max_download_bytes: DEFAULT_MAX_DOWNLOAD_BYTES,
            extract: ExtractLimits::default(),
        }
    }
}

/// Fault-injection seam for staging write and durability failures.
#[doc(hidden)]
pub trait StageFault: Send + Sync {
    /// Called before each archive write; returning an error aborts staging.
    fn before_write(&self, _path: &Path, _bytes: usize) -> io::Result<()> {
        Ok(())
    }
}

struct NoFault;

impl StageFault for NoFault {}

fn verify_manifest(
    manifest: &ReleaseManifest,
    release: &AuthorizedRelease,
) -> Result<(), UpdateError> {
    if manifest.build_id != release.build_id {
        return Err(UpdateError::ReleaseIdentityMismatch {
            field: "build_id",
            expected: release.build_id.clone(),
            found: manifest.build_id.clone(),
        });
    }
    if manifest.version != release.version.to_string() {
        return Err(UpdateError::ReleaseIdentityMismatch {
            field: "version",
            expected: release.version.to_string(),
            found: manifest.version.clone(),
        });
    }
    Ok(())
}

/// Downloads, verifies, extracts, and durably promotes a release generation.
///
/// The returned identifier is the SHA-256 digest of the whole release archive. The active
/// reference is never changed by this operation.
pub async fn stage_release(
    release: &AuthorizedRelease,
    store: &GenerationStore,
) -> Result<GenerationId, UpdateError> {
    let client = OutboundHttpPolicy::production()
        .with_timeouts(
            Some(Duration::from_secs(10)),
            Some(Duration::from_secs(120)),
        )
        .build_checked_client()?;
    stage_release_with(release, store, &client, StageLimits::default(), &NoFault).await
}

/// Staging seam used by fault tests; production callers use [`stage_release`].
#[doc(hidden)]
pub async fn stage_release_with(
    release: &AuthorizedRelease,
    store: &GenerationStore,
    client: &CheckedHttpClient,
    limits: StageLimits,
    fault: &dyn StageFault,
) -> Result<GenerationId, UpdateError> {
    if let Some(cached) = cached_generation(release, store)? {
        return Ok(cached);
    }
    if release.size > limits.max_download_bytes {
        return Err(UpdateError::DownloadTooLarge {
            limit: limits.max_download_bytes,
        });
    }

    let final_dir = store.generation_dir(&release.digest);
    let generations = final_dir
        .parent()
        .ok_or_else(|| UpdateError::Io {
            path: final_dir.clone(),
            source: io::Error::other("generation path has no parent directory"),
        })?
        .to_path_buf();
    let staging = final_dir.with_file_name(format!(".staging-{}", Uuid::new_v4()));
    fs::create_dir(&staging).map_err(|source| UpdateError::Io {
        path: staging.clone(),
        source,
    })?;
    if let Err(error) = set_mode(&staging, 0o700) {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }

    let outcome = stage_into(release, client, limits, fault, &staging).await;
    if let Err(error) = outcome {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }

    let tree = staging.join(TREE_DIR);
    match fs::rename(&tree, &final_dir) {
        Ok(()) => {}
        Err(error) if final_dir.is_dir() => {
            let _ = fs::remove_dir_all(&staging);
            if cached_generation(release, store)?.is_some() {
                return Ok(release.digest.clone());
            }
            return Err(UpdateError::Io {
                path: final_dir,
                source: error,
            });
        }
        Err(source) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(UpdateError::Io {
                path: final_dir,
                source,
            });
        }
    }

    let _ = fs::remove_dir_all(&staging);
    sync_directory(&generations)?;
    Ok(release.digest.clone())
}

async fn stage_into(
    release: &AuthorizedRelease,
    client: &CheckedHttpClient,
    limits: StageLimits,
    fault: &dyn StageFault,
    staging: &Path,
) -> Result<(), UpdateError> {
    let archive = staging.join(ARCHIVE_FILE);
    download(release, client, limits, fault, &archive).await?;

    let tree = staging.join(TREE_DIR);
    fs::create_dir(&tree).map_err(|source| UpdateError::Io {
        path: tree.clone(),
        source,
    })?;
    set_mode(&tree, 0o755)?;

    extract_archive(&archive, &tree, limits.extract, fault)?;
    let manifest = read_release_manifest(&tree)?;
    verify_manifest(&manifest, release)?;

    fs::remove_file(&archive).map_err(|source| UpdateError::Io {
        path: archive,
        source,
    })?;
    sync_directory(&tree)?;
    Ok(())
}

async fn download(
    release: &AuthorizedRelease,
    client: &CheckedHttpClient,
    limits: StageLimits,
    fault: &dyn StageFault,
    path: &Path,
) -> Result<(), UpdateError> {
    let url = release.url.clone().ok_or(UpdateError::MissingDownloadUrl)?;
    let mut file = create_private_file(path)?;

    let mut response = send_checked_request(
        client,
        reqwest::Method::GET,
        url.as_str(),
        reqwest::header::HeaderMap::new(),
        None,
        DEFAULT_MAX_REDIRECTS,
    )
    .await?;
    if !response.status().is_success() {
        return Err(UpdateError::DownloadStatus {
            url: url.to_string(),
            status: response.status().as_u16(),
        });
    }
    if let Some(length) = response.content_length()
        && length != release.size
    {
        return Err(UpdateError::SizeMismatch {
            url: url.to_string(),
            expected: release.size,
            actual: length,
        });
    }

    let mut hasher = Sha256::new();
    let mut written = 0u64;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|source| UpdateError::Download {
            url: url.to_string(),
            source: Box::new(source),
        })?
    {
        let chunk_len = u64::try_from(chunk.len()).map_err(|_| UpdateError::DownloadTooLarge {
            limit: release.size,
        })?;
        written = written
            .checked_add(chunk_len)
            .ok_or(UpdateError::DownloadTooLarge {
                limit: release.size,
            })?;
        if written > release.size || written > limits.max_download_bytes {
            return Err(UpdateError::DownloadTooLarge {
                limit: release.size.min(limits.max_download_bytes),
            });
        }
        fault
            .before_write(path, chunk.len())
            .map_err(|source| UpdateError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        hasher.update(&chunk);
        file.write_all(&chunk).map_err(|source| UpdateError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    }

    if written != release.size {
        return Err(UpdateError::SizeMismatch {
            url: url.to_string(),
            expected: release.size,
            actual: written,
        });
    }
    let actual = hex::encode(hasher.finalize());
    if actual != release.digest.as_str() {
        return Err(UpdateError::DigestMismatch {
            expected: release.digest.clone(),
            actual,
        });
    }
    file.sync_all().map_err(|source| UpdateError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

struct Extractor<'a> {
    archive_path: &'a Path,
    tree: &'a Path,
    limits: ExtractLimits,
    fault: &'a dyn StageFault,
    budget_tripped: Rc<Cell<bool>>,
    seen: BTreeSet<PathBuf>,
    files: BTreeSet<PathBuf>,
    dirs: BTreeSet<PathBuf>,
    dir_paths: Vec<PathBuf>,
    total: u64,
    count: usize,
}

impl<'a> Extractor<'a> {
    fn new(
        archive_path: &'a Path,
        tree: &'a Path,
        limits: ExtractLimits,
        fault: &'a dyn StageFault,
        budget_tripped: Rc<Cell<bool>>,
    ) -> Self {
        Self {
            archive_path,
            tree,
            limits,
            fault,
            budget_tripped,
            seen: BTreeSet::new(),
            files: BTreeSet::new(),
            dirs: BTreeSet::new(),
            dir_paths: Vec::new(),
            total: 0,
            count: 0,
        }
    }

    fn entry<R: Read>(&mut self, mut entry: tar::Entry<'_, R>) -> Result<(), UpdateError> {
        self.count = self.count.saturating_add(1);
        if self.count > self.limits.max_entries {
            return Err(UpdateError::TooManyArchiveEntries {
                limit: self.limits.max_entries,
            });
        }

        let entry_type = entry.header().entry_type();
        if matches!(
            entry_type,
            EntryType::XHeader
                | EntryType::XGlobalHeader
                | EntryType::GNULongName
                | EntryType::GNULongLink
        ) {
            return Ok(());
        }

        let raw_path = entry.path().map_err(|error| self.invalid(error))?;
        let relative = normalize_entry_path(&raw_path)?;
        if relative.as_os_str().is_empty() {
            return Ok(());
        }

        // tar-rs consumes local pax and GNU long-name/long-link headers before it returns the
        // entry they describe. Count those invisible metadata entries as well.
        let metadata_entries =
            metadata_entry_count(&mut entry).map_err(|error| self.invalid(error))?;
        self.count = self.count.saturating_add(metadata_entries);
        if self.count > self.limits.max_entries {
            return Err(UpdateError::TooManyArchiveEntries {
                limit: self.limits.max_entries,
            });
        }

        if entry_type.is_dir() {
            return self.directory(relative);
        }
        if entry_type.is_file() {
            return self.file(entry, relative);
        }
        Err(UpdateError::UnsafeArchiveEntry {
            path: relative.display().to_string(),
            reason: format!("unsupported archive entry type {entry_type:?}"),
        })
    }

    fn directory(&mut self, relative: PathBuf) -> Result<(), UpdateError> {
        let display = relative.display().to_string();
        if !self.seen.insert(relative.clone()) {
            return Err(UpdateError::DuplicateArchiveEntry { path: display });
        }
        if self.files.contains(&relative) {
            return Err(UpdateError::UnsafeArchiveEntry {
                path: display,
                reason: "directory entry replaces a file".to_owned(),
            });
        }
        ensure_parent_dirs(
            self.tree,
            relative.parent(),
            &self.files,
            &mut self.dirs,
            &mut self.dir_paths,
            self.limits.max_entries,
        )?;
        if !self.dirs.contains(&relative) {
            let path = self.tree.join(&relative);
            fs::create_dir(&path).map_err(|source| UpdateError::Io {
                path: path.clone(),
                source,
            })?;
            set_mode(&path, 0o755)?;
            self.dirs.insert(relative.clone());
            self.dir_paths.push(relative);
        }
        Ok(())
    }

    fn file<R: Read>(
        &mut self,
        entry: tar::Entry<'_, R>,
        relative: PathBuf,
    ) -> Result<(), UpdateError> {
        let display = relative.display().to_string();
        if !self.seen.insert(relative.clone()) {
            return Err(UpdateError::DuplicateArchiveEntry { path: display });
        }
        if self.dirs.contains(&relative) {
            return Err(UpdateError::UnsafeArchiveEntry {
                path: display,
                reason: "file entry replaces a directory".to_owned(),
            });
        }
        ensure_parent_dirs(
            self.tree,
            relative.parent(),
            &self.files,
            &mut self.dirs,
            &mut self.dir_paths,
            self.limits.max_entries,
        )?;

        // `Entry::size()` applies pax size overrides; `header().size()` is only the ustar
        // fallback and can disagree with the bytes tar-rs actually reads and skips.
        let size = entry.size();
        self.total = self
            .total
            .checked_add(size)
            .ok_or(UpdateError::ArchiveExpandedTooLarge {
                limit: self.limits.max_total_bytes,
            })?;
        if self.total > self.limits.max_total_bytes {
            return Err(UpdateError::ArchiveExpandedTooLarge {
                limit: self.limits.max_total_bytes,
            });
        }

        let path = self.tree.join(&relative);
        let mode = entry.header().mode().map_err(|error| self.invalid(error))?;
        let mut destination = create_private_file(&path)?;
        self.fault
            .before_write(&path, usize::try_from(size).unwrap_or(usize::MAX))
            .map_err(|source| UpdateError::Io {
                path: path.clone(),
                source,
            })?;
        let copied = io::copy(&mut entry.take(size), &mut destination)
            .map_err(|source| self.io_error(&path, source))?;
        if copied != size {
            return Err(self.invalid(format!("entry `{display}` is truncated")));
        }
        let permissions = if mode & 0o111 == 0 { 0o644 } else { 0o755 };
        set_mode(&path, permissions)?;
        destination.sync_all().map_err(|source| UpdateError::Io {
            path: path.clone(),
            source,
        })?;
        drop(destination);
        self.files.insert(relative);
        Ok(())
    }

    fn finish(self) -> Result<(), UpdateError> {
        for directory in self.dir_paths.into_iter().rev() {
            sync_directory(&self.tree.join(directory))?;
        }
        sync_directory(self.tree)
    }

    fn invalid(&self, error: impl std::fmt::Display) -> UpdateError {
        if self.budget_tripped.get() {
            return UpdateError::ArchiveExpandedTooLarge {
                limit: self.limits.max_total_bytes,
            };
        }
        UpdateError::InvalidArchive {
            path: self.archive_path.to_path_buf(),
            reason: error.to_string(),
        }
    }

    fn io_error(&self, path: &Path, source: io::Error) -> UpdateError {
        if self.budget_tripped.get() {
            return UpdateError::ArchiveExpandedTooLarge {
                limit: self.limits.max_total_bytes,
            };
        }
        UpdateError::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// Counts pax/GNU metadata entries that tar-rs consumes before returning the described entry.
fn metadata_entry_count<R: Read>(entry: &mut tar::Entry<'_, R>) -> io::Result<usize> {
    let has_pax = entry.pax_extensions()?.is_some();
    let mut count = usize::from(has_pax);
    let path = entry.path()?;
    let header_path = entry.header().path()?;
    if !has_pax && path != header_path {
        count = count.saturating_add(1);
    }
    let link = entry.link_name()?;
    let header_link = entry.header().link_name()?;
    if !has_pax && link != header_link {
        count = count.saturating_add(1);
    }
    Ok(count)
}

struct BudgetReader<R> {
    inner: R,
    remaining: u64,
    tripped: Rc<Cell<bool>>,
}

impl<R> BudgetReader<R> {
    fn new(inner: R, limit: u64) -> Self {
        Self {
            inner,
            remaining: limit,
            tripped: Rc::new(Cell::new(false)),
        }
    }

    fn tripped(&self) -> Rc<Cell<bool>> {
        Rc::clone(&self.tripped)
    }
}

impl<R: Read> Read for BudgetReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            let mut probe = [0u8; 1];
            return match self.inner.read(&mut probe) {
                Ok(0) => Ok(0),
                Ok(_) => {
                    self.tripped.set(true);
                    Err(io::Error::other(
                        "decompressed archive exceeds the configured byte budget",
                    ))
                }
                Err(error) => Err(error),
            };
        }
        let allowed = usize::try_from(self.remaining)
            .unwrap_or(usize::MAX)
            .min(buf.len());
        let read = self.inner.read(&mut buf[..allowed])?;
        let read_u64 = u64::try_from(read).map_err(|_| io::Error::other("read length overflow"))?;
        self.remaining = self.remaining.saturating_sub(read_u64);
        Ok(read)
    }
}

fn extract_archive(
    archive_path: &Path,
    tree: &Path,
    limits: ExtractLimits,
    fault: &dyn StageFault,
) -> Result<(), UpdateError> {
    let file = File::open(archive_path).map_err(|source| UpdateError::Io {
        path: archive_path.to_path_buf(),
        source,
    })?;
    let budget = BudgetReader::new(GzDecoder::new(file), limits.max_total_bytes);
    let budget_tripped = budget.tripped();
    let mut archive = tar::Archive::new(budget);
    let mut extractor = Extractor::new(archive_path, tree, limits, fault, budget_tripped);

    let entries = match archive.entries() {
        Ok(entries) => entries,
        Err(error) => return Err(extractor.invalid(error)),
    };
    for entry in entries {
        match entry {
            Ok(entry) => extractor.entry(entry)?,
            Err(error) => return Err(extractor.invalid(error)),
        }
    }
    extractor.finish()
}

fn normalize_entry_path(path: &Path) -> Result<PathBuf, UpdateError> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                let Some(part) = part.to_str() else {
                    return Err(UpdateError::UnsafeArchiveEntry {
                        path: path.display().to_string(),
                        reason: "non-UTF-8 path".to_owned(),
                    });
                };
                if part.is_empty() {
                    return Err(UpdateError::UnsafeArchiveEntry {
                        path: path.display().to_string(),
                        reason: "empty path component".to_owned(),
                    });
                }
                normalized.push(part);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(UpdateError::UnsafeArchiveEntry {
                    path: path.display().to_string(),
                    reason: "path is not relative and self-contained".to_owned(),
                });
            }
        }
    }
    Ok(normalized)
}

fn ensure_parent_dirs(
    tree: &Path,
    parent: Option<&Path>,
    files: &BTreeSet<PathBuf>,
    dirs: &mut BTreeSet<PathBuf>,
    dir_paths: &mut Vec<PathBuf>,
    max_entries: usize,
) -> Result<(), UpdateError> {
    let Some(parent) = parent else {
        return Ok(());
    };
    let mut current = PathBuf::new();
    for component in parent.components() {
        let Component::Normal(part) = component else {
            return Err(UpdateError::UnsafeArchiveEntry {
                path: parent.display().to_string(),
                reason: "parent path is not self-contained".to_owned(),
            });
        };
        current.push(part);
        if files.contains(&current) {
            return Err(UpdateError::UnsafeArchiveEntry {
                path: current.display().to_string(),
                reason: "a file is used as a directory".to_owned(),
            });
        }
        if !dirs.contains(&current) {
            if dirs.len() >= max_entries {
                return Err(UpdateError::TooManyArchiveEntries { limit: max_entries });
            }
            let path = tree.join(&current);
            fs::create_dir(&path).map_err(|source| UpdateError::Io {
                path: path.clone(),
                source,
            })?;
            set_mode(&path, 0o755)?;
            dirs.insert(current.clone());
            dir_paths.push(current.clone());
        }
    }
    Ok(())
}

fn cached_generation(
    release: &AuthorizedRelease,
    store: &GenerationStore,
) -> Result<Option<GenerationId>, UpdateError> {
    let directory = store.generation_dir(&release.digest);
    match fs::symlink_metadata(&directory) {
        Ok(metadata) => {
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(UpdateError::CorruptGeneration(release.digest.clone()));
            }
            verify_manifest(&read_release_manifest(&directory)?, release)?;
            Ok(Some(release.digest.clone()))
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(UpdateError::Io {
            path: directory,
            source,
        }),
    }
}

fn read_release_manifest(directory: &Path) -> Result<ReleaseManifest, UpdateError> {
    let path = directory.join(RELEASE_MANIFEST);
    let metadata = fs::symlink_metadata(&path).map_err(|source| UpdateError::Io {
        path: path.clone(),
        source,
    })?;
    if !metadata.file_type().is_file() {
        return Err(UpdateError::InvalidReleaseManifest(format!(
            "`{}` is not a regular file",
            path.display()
        )));
    }
    let bytes = fs::read(&path).map_err(|source| UpdateError::Io {
        path: path.clone(),
        source,
    })?;
    ReleaseManifest::parse(&bytes)
        .map_err(|error| UpdateError::InvalidReleaseManifest(error.to_string()))
}

fn create_private_file(path: &Path) -> Result<File, UpdateError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(|source| UpdateError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn set_mode(path: &Path, mode: u32) -> Result<(), UpdateError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|source| {
            UpdateError::Io {
                path: path.to_path_buf(),
                source,
            }
        })?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), UpdateError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| UpdateError::Io {
            path: path.to_path_buf(),
            source,
        })
}
