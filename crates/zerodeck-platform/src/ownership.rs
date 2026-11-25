//! Ownership checks for code that the daemon runs as root.
//!
//! A `"root"` plugin must live in a tree that only root can change: otherwise the unprivileged
//! user could edit its code and have it executed with full privileges.

#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;

/// The `lstat` fields an ownership check looks at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryMeta {
    pub uid: u32,
    pub mode: u32,
    pub is_symlink: bool,
}

/// Why an entry is not safe to run as root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViolationKind {
    NotRootOwned {
        uid: u32,
    },
    WritableByOthers {
        mode: u32,
    },
    Symlink,
    /// The entry could not be inspected, so it cannot be shown to be safe.
    Unreadable(String),
}

/// The first entry of a tree that is not safe to run as root.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{path}` is not safe to run as root: {kind:?}")]
pub struct OwnershipViolation {
    pub path: PathBuf,
    pub kind: ViolationKind,
}

/// Returns the reason an entry is unsafe to run as root, or `None` when it is root-owned, not a
/// symlink, and neither group- nor world-writable.
#[must_use]
pub fn check_entry(meta: EntryMeta) -> Option<ViolationKind> {
    if meta.is_symlink {
        Some(ViolationKind::Symlink)
    } else if meta.uid != 0 {
        Some(ViolationKind::NotRootOwned { uid: meta.uid })
    } else if meta.mode & 0o022 != 0 {
        Some(ViolationKind::WritableByOthers {
            mode: meta.mode & 0o7777,
        })
    } else {
        None
    }
}

/// Walks `root` without following symlinks and returns the first entry that fails
/// [`check_entry`]. Entries that cannot be inspected fail closed.
#[cfg(unix)]
pub fn check_root_only_tree(root: &Path) -> Result<(), OwnershipViolation> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let meta = std::fs::symlink_metadata(&path).map_err(|err| unreadable(&path, &err))?;
        if let Some(kind) = check_entry(entry_meta(&meta)) {
            return Err(OwnershipViolation { path, kind });
        }
        if meta.is_dir() {
            let entries = std::fs::read_dir(&path).map_err(|err| unreadable(&path, &err))?;
            for entry in entries {
                pending.push(entry.map_err(|err| unreadable(&path, &err))?.path());
            }
        }
    }
    Ok(())
}

/// Extracts the fields [`check_entry`] needs from `lstat` metadata.
#[cfg(unix)]
#[must_use]
pub fn entry_meta(meta: &std::fs::Metadata) -> EntryMeta {
    use std::os::unix::fs::MetadataExt;

    EntryMeta {
        uid: meta.uid(),
        mode: meta.mode(),
        is_symlink: meta.file_type().is_symlink(),
    }
}

#[cfg(unix)]
fn unreadable(path: &Path, err: &std::io::Error) -> OwnershipViolation {
    OwnershipViolation {
        path: path.to_path_buf(),
        kind: ViolationKind::Unreadable(err.to_string()),
    }
}

/// Lists `path` (symlinks resolved) and its ancestors that a non-root user owns or that are
/// group- or world-writable. Entries that cannot be inspected are skipped.
#[cfg(unix)]
#[must_use]
pub fn non_root_writable_ancestors(path: &Path) -> Vec<PathBuf> {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    resolved
        .ancestors()
        .filter(|dir| !dir.as_os_str().is_empty())
        .filter(|dir| {
            std::fs::symlink_metadata(dir).is_ok_and(|meta| {
                let meta = entry_meta(&meta);
                meta.uid != 0 || meta.mode & 0o022 != 0
            })
        })
        .map(Path::to_path_buf)
        .collect()
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;

    use uuid::Uuid;

    use super::*;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!("zerodeck-{label}-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).expect("create temp test dir");
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn meta(uid: u32, mode: u32) -> EntryMeta {
        EntryMeta {
            uid,
            mode,
            is_symlink: false,
        }
    }

    fn running_as_root() -> bool {
        rustix::process::geteuid().as_raw() == 0
    }

    #[test]
    fn root_owned_0755_dir_and_0644_file_pass() {
        assert_eq!(check_entry(meta(0, 0o040_755)), None);
        assert_eq!(check_entry(meta(0, 0o100_644)), None);
        assert_eq!(check_entry(meta(0, 0o100_700)), None);
    }

    #[test]
    fn non_root_owner_is_violation() {
        assert_eq!(
            check_entry(meta(1000, 0o040_755)),
            Some(ViolationKind::NotRootOwned { uid: 1000 })
        );
    }

    #[test]
    fn group_or_world_writable_is_violation() {
        for mode in [0o775, 0o757, 0o100_664, 0o040_777] {
            assert_eq!(
                check_entry(meta(0, mode)),
                Some(ViolationKind::WritableByOthers {
                    mode: mode & 0o7777
                }),
                "mode {mode:o}"
            );
        }
    }

    #[test]
    fn symlink_is_violation() {
        let link = EntryMeta {
            uid: 0,
            mode: 0o120_777,
            is_symlink: true,
        };
        assert_eq!(check_entry(link), Some(ViolationKind::Symlink));
    }

    #[test]
    fn violation_message_names_the_path() {
        let violation = OwnershipViolation {
            path: PathBuf::from("/home/deck/homebrew/plugins/demo/main.py"),
            kind: ViolationKind::NotRootOwned { uid: 1000 },
        };
        let message = violation.to_string();
        assert!(
            message.contains("/home/deck/homebrew/plugins/demo/main.py"),
            "{message}"
        );
        assert!(message.contains("NotRootOwned"), "{message}");
    }

    #[test]
    fn tree_owned_by_current_user_is_reported_when_not_root() {
        if running_as_root() {
            eprintln!("skipping: the temp tree is root-owned when tests run as root");
            return;
        }
        let dir = TestDir::new("ownership-tree");
        fs::create_dir_all(dir.0.join("nested")).expect("create nested dir");
        fs::write(dir.0.join("nested/main.py"), b"pass").expect("write file");

        let uid = rustix::process::geteuid().as_raw();
        let violation = check_root_only_tree(&dir.0).expect_err("user-owned tree must fail");
        assert_eq!(violation.path, dir.0);
        assert_eq!(violation.kind, ViolationKind::NotRootOwned { uid });
    }

    #[test]
    #[ignore = "requires root; run with just test-privileged"]
    fn privileged_root_owned_tree_passes_and_nested_violations_are_named() {
        use std::os::unix::fs::PermissionsExt;

        assert_eq!(rustix::process::geteuid().as_raw(), 0);
        let dir = TestDir::new("ownership-root-tree");
        let nested = dir.0.join("py_modules");
        fs::create_dir_all(&nested).expect("create nested dir");
        for path in [&dir.0, &nested] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod dir");
        }
        let file = nested.join("lib.py");
        fs::write(&file, b"pass").expect("write file");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).expect("chmod file");
        check_root_only_tree(&dir.0).expect("root-owned 0755/0644 tree passes");

        fs::set_permissions(&file, fs::Permissions::from_mode(0o664)).expect("chmod g+w");
        let violation = check_root_only_tree(&dir.0).expect_err("group-writable file");
        assert_eq!(violation.path, file);
        assert_eq!(
            violation.kind,
            ViolationKind::WritableByOthers { mode: 0o664 }
        );
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).expect("chmod back");

        let link = nested.join("link.py");
        std::os::unix::fs::symlink(&file, &link).expect("create symlink");
        let violation = check_root_only_tree(&dir.0).expect_err("nested symlink");
        assert_eq!(violation.path, link);
        assert_eq!(violation.kind, ViolationKind::Symlink);
    }

    #[test]
    fn missing_tree_fails_closed() {
        let dir = TestDir::new("ownership-missing");
        let missing = dir.0.join("absent");
        let violation = check_root_only_tree(&missing).expect_err("missing tree must fail");
        assert_eq!(violation.path, missing);
        assert!(matches!(violation.kind, ViolationKind::Unreadable(_)));
    }

    #[test]
    fn non_root_writable_ancestors_lists_user_owned_temp_dir() {
        if running_as_root() {
            eprintln!("skipping: the temp dir is root-owned when tests run as root");
            return;
        }
        let dir = TestDir::new("ownership-ancestors");
        let leaf = dir.0.join("homebrew");
        fs::create_dir_all(&leaf).expect("create leaf");

        let flagged = non_root_writable_ancestors(&leaf);
        let leaf = fs::canonicalize(&leaf).expect("canonical leaf");
        assert!(flagged.contains(&leaf), "{flagged:?}");
        assert!(
            flagged.contains(&leaf.parent().expect("parent").to_path_buf()),
            "{flagged:?}"
        );
        assert!(!flagged.contains(&PathBuf::from("/")), "{flagged:?}");
    }
}
