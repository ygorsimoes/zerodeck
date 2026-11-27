//! Validation of an immutable managed loader generation directory.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use thiserror::Error;
use zerodeck_protocol::release::{
    MAX_RELEASE_MANIFEST_BYTES, RELEASE_MANIFEST, ReleaseManifest, ReleaseManifestError,
};

const DAEMON: &str = "bin/zerodeckd";
const CONTROLLER: &str = "bin/zerodeckctl";
const PYTHON_DIR: &str = "python";
const FRONTEND_DIR: &str = "frontend";

/// Why a generation directory is not a valid managed layout.
#[derive(Debug, Error)]
pub enum LayoutError {
    /// A required entry is absent.
    #[error("generation is missing `{0}`")]
    Missing(&'static str),
    /// An entry has the wrong type, is a symlink, or escapes the generation root.
    #[error("generation entry `{0}` is not a plain file or directory inside the generation")]
    BadEntry(&'static str),
    /// An executable is not executable or is writable by group or others.
    #[error("generation executable `{0}` must be executable and not group/world writable")]
    BadMode(&'static str),
    /// The release manifest is invalid.
    #[error(transparent)]
    Manifest(#[from] ReleaseManifestError),
    /// An I/O error occurred while inspecting the generation.
    #[error("inspect `{path}`: {source}")]
    Io {
        /// Path being inspected.
        path: PathBuf,
        /// Underlying error.
        source: std::io::Error,
    },
}

/// A validated generation directory and the paths it owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationLayout {
    root: PathBuf,
    daemon: PathBuf,
    controller: Option<PathBuf>,
    python_dir: PathBuf,
    frontend_dir: PathBuf,
    manifest: ReleaseManifest,
}

impl GenerationLayout {
    /// Canonicalizes and validates `root`: no symlinks, required entries present, executables
    /// not group/world writable, release manifest valid.
    ///
    /// # Errors
    ///
    /// Returns a [`LayoutError`] naming the first violated rule.
    pub fn open(root: &Path) -> Result<Self, LayoutError> {
        let root = fs::canonicalize(root).map_err(|source| LayoutError::Io {
            path: root.to_path_buf(),
            source,
        })?;
        let daemon = regular_file(&root, DAEMON)?.ok_or(LayoutError::Missing(DAEMON))?;
        check_executable(&daemon, DAEMON)?;
        let controller = regular_file(&root, CONTROLLER)?;
        if let Some(path) = &controller {
            check_executable(path, CONTROLLER)?;
        }
        let python_dir = directory(&root, PYTHON_DIR)?;
        let frontend_dir = directory(&root, FRONTEND_DIR)?;
        let manifest_path =
            regular_file(&root, RELEASE_MANIFEST)?.ok_or(LayoutError::Missing("release.json"))?;
        let size = fs::metadata(&manifest_path)
            .map_err(|source| LayoutError::Io {
                path: manifest_path.clone(),
                source,
            })?
            .len();
        if size > MAX_RELEASE_MANIFEST_BYTES {
            return Err(ReleaseManifestError::TooLarge.into());
        }
        let bytes = fs::read(&manifest_path).map_err(|source| LayoutError::Io {
            path: manifest_path,
            source,
        })?;
        let manifest = ReleaseManifest::parse(&bytes)?;
        Ok(Self {
            root,
            daemon,
            controller,
            python_dir,
            frontend_dir,
            manifest,
        })
    }

    /// Canonical generation root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The packaged `bin/zerodeckd`.
    #[must_use]
    pub fn daemon(&self) -> &Path {
        &self.daemon
    }

    /// The packaged `bin/zerodeckctl`, when the generation ships one.
    #[must_use]
    pub fn controller(&self) -> Option<&Path> {
        self.controller.as_deref()
    }

    /// The directory containing `zerodeck_py_host`.
    #[must_use]
    pub fn python_dir(&self) -> &Path {
        &self.python_dir
    }

    /// The packaged frontend bundle.
    #[must_use]
    pub fn frontend_dir(&self) -> &Path {
        &self.frontend_dir
    }

    /// The release identity recorded in `release.json`.
    #[must_use]
    pub fn manifest(&self) -> &ReleaseManifest {
        &self.manifest
    }
}

fn inspect(path: &Path) -> Result<Option<fs::Metadata>, LayoutError> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(meta)),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(LayoutError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn regular_file(root: &Path, relative: &'static str) -> Result<Option<PathBuf>, LayoutError> {
    let path = root.join(relative);
    match inspect(&path)? {
        None => Ok(None),
        Some(meta) if meta.is_file() && !meta.file_type().is_symlink() => Ok(Some(path)),
        Some(_) => Err(LayoutError::BadEntry(relative)),
    }
}

fn directory(root: &Path, relative: &'static str) -> Result<PathBuf, LayoutError> {
    let path = root.join(relative);
    match inspect(&path)? {
        None => Err(LayoutError::Missing(relative)),
        Some(meta) if meta.is_dir() && !meta.file_type().is_symlink() => Ok(path),
        Some(_) => Err(LayoutError::BadEntry(relative)),
    }
}

fn check_executable(path: &Path, name: &'static str) -> Result<(), LayoutError> {
    let mode = fs::metadata(path)
        .map_err(|source| LayoutError::Io {
            path: path.to_path_buf(),
            source,
        })?
        .permissions()
        .mode();
    if mode & 0o111 == 0 || mode & 0o022 != 0 {
        return Err(LayoutError::BadMode(name));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::{Path, PathBuf};

    use uuid::Uuid;

    use super::{GenerationLayout, LayoutError};

    struct Tree(PathBuf);

    impl Tree {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("zerodeck-layout-{}", Uuid::new_v4()));
            fs::create_dir_all(root.join("bin")).expect("bin");
            fs::create_dir_all(root.join("python")).expect("python");
            fs::create_dir_all(root.join("frontend")).expect("frontend");
            fs::write(root.join("bin/zerodeckd"), b"#!/bin/sh\n").expect("daemon");
            fs::set_permissions(
                root.join("bin/zerodeckd"),
                fs::Permissions::from_mode(0o755),
            )
            .expect("chmod");
            fs::write(
                root.join("release.json"),
                br#"{"schema":1,"version":"0.1.0","build_id":"b"}"#,
            )
            .expect("manifest");
            Self(root)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn open_accepts_a_complete_layout() {
        let tree = Tree::new();
        let layout = GenerationLayout::open(tree.path()).expect("valid layout");
        assert_eq!(layout.manifest().build_id, "b");
        assert!(layout.controller().is_none());
    }

    #[test]
    fn open_requires_regular_files_and_dirs() {
        let tree = Tree::new();
        fs::remove_dir(tree.path().join("frontend")).expect("remove");
        assert!(matches!(
            GenerationLayout::open(tree.path()),
            Err(LayoutError::Missing("frontend"))
        ));
    }

    #[test]
    fn open_rejects_symlinked_component() {
        let tree = Tree::new();
        fs::remove_dir(tree.path().join("python")).expect("remove");
        symlink(tree.path().join("frontend"), tree.path().join("python")).expect("link");
        assert!(matches!(
            GenerationLayout::open(tree.path()),
            Err(LayoutError::BadEntry("python"))
        ));
    }

    #[test]
    fn open_rejects_non_executable_daemon() {
        let tree = Tree::new();
        fs::set_permissions(
            tree.path().join("bin/zerodeckd"),
            fs::Permissions::from_mode(0o644),
        )
        .expect("chmod");
        assert!(matches!(
            GenerationLayout::open(tree.path()),
            Err(LayoutError::BadMode("bin/zerodeckd"))
        ));
    }

    #[test]
    fn open_rejects_group_writable_daemon() {
        let tree = Tree::new();
        fs::set_permissions(
            tree.path().join("bin/zerodeckd"),
            fs::Permissions::from_mode(0o775),
        )
        .expect("chmod");
        assert!(matches!(
            GenerationLayout::open(tree.path()),
            Err(LayoutError::BadMode("bin/zerodeckd"))
        ));
    }
}
