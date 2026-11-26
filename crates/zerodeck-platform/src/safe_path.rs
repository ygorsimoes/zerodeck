use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::PlatformError;

/// Canonicalized single-root directory jail that prevents traversal and symlink escapes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafePath {
    canonical_root: PathBuf,
}

impl SafePath {
    /// Creates a new [`SafePath`] jail anchored at `root`.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, PlatformError> {
        let root_ref = root.as_ref();
        let canonical_root = fs::canonicalize(root_ref).map_err(|source| PlatformError::Io {
            path: root_ref.to_path_buf(),
            source,
        })?;
        Ok(Self { canonical_root })
    }

    /// Returns the canonicalized root directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.canonical_root
    }

    /// Resolves `relative` inside this jail, rejecting traversal, absolute paths, and escaping symlinks.
    pub fn resolve(&self, relative: impl AsRef<Path>) -> Result<PathBuf, PlatformError> {
        let rel = relative.as_ref();
        for component in rel.components() {
            match component {
                Component::Prefix(_) | Component::RootDir | Component::ParentDir => {
                    return Err(PlatformError::PathEscape {
                        candidate: rel.to_path_buf(),
                        root: self.canonical_root.clone(),
                    });
                }
                Component::CurDir | Component::Normal(_) => {}
            }
        }

        let joined = self.canonical_root.join(rel);
        let canonical = fs::canonicalize(&joined).map_err(|source| PlatformError::Io {
            path: joined,
            source,
        })?;

        if !canonical.starts_with(&self.canonical_root) {
            return Err(PlatformError::PathEscape {
                candidate: canonical,
                root: self.canonical_root.clone(),
            });
        }

        Ok(canonical)
    }
}

/// Multi-root canonical path jail (e.g. for `filepicker` browsing across `$HOME` and `/run/media`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafePathSet {
    roots: Vec<PathBuf>,
}

impl SafePathSet {
    /// Creates a [`SafePathSet`] requiring all provided roots to exist and canonicalize.
    pub fn new(roots: Vec<PathBuf>) -> Result<Self, PlatformError> {
        if roots.is_empty() {
            return Err(PlatformError::EmptyRootSet);
        }
        let mut canonical_roots = Vec::with_capacity(roots.len());
        for root in roots {
            let canonical = fs::canonicalize(&root)
                .map_err(|source| PlatformError::Io { path: root, source })?;
            canonical_roots.push(canonical);
        }
        Ok(Self {
            roots: canonical_roots,
        })
    }

    /// Creates a [`SafePathSet`] from whichever configured roots currently exist on disk.
    pub fn from_existing(roots: Vec<PathBuf>) -> Result<Self, PlatformError> {
        let canonical_roots: Vec<PathBuf> = roots
            .into_iter()
            .filter_map(|root| fs::canonicalize(root).ok())
            .collect();
        if canonical_roots.is_empty() {
            return Err(PlatformError::EmptyRootSet);
        }
        Ok(Self {
            roots: canonical_roots,
        })
    }

    /// Resolves an arbitrary candidate path and verifies it falls inside at least one allowed root.
    pub fn resolve(&self, candidate: impl AsRef<Path>) -> Result<PathBuf, PlatformError> {
        let cand_ref = candidate.as_ref();
        let canonical = fs::canonicalize(cand_ref).map_err(|source| PlatformError::Io {
            path: cand_ref.to_path_buf(),
            source,
        })?;

        if self.roots.iter().any(|root| canonical.starts_with(root)) {
            Ok(canonical)
        } else {
            Err(PlatformError::PathOutsideRoots {
                candidate: canonical,
                roots: self.roots.clone(),
            })
        }
    }
}
