//! OS path, canonical path-jail, atomic persistence, and privilege abstractions for `ZeroDeck`.

mod error;
mod fs;
mod paths;
mod privilege;
mod safe_path;

pub use error::PlatformError;
pub use fs::atomic_write;
pub use paths::PlatformPaths;
pub use privilege::PrivilegeTarget;
#[cfg(unix)]
pub use privilege::{ProcessIdentity, current_process_identity};
pub use safe_path::{SafePath, SafePathSet};

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;

    use uuid::Uuid;

    use crate::{
        PlatformError, PlatformPaths, PrivilegeTarget, SafePath, SafePathSet, atomic_write,
    };

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

    #[test]
    fn safe_path_rejects_parent_traversal_and_absolute_paths() {
        let dir = TestDir::new("safepath");
        let jail = SafePath::new(&dir.0).expect("init jail");

        let err_parent = jail
            .resolve("../secret")
            .expect_err("must reject parent traversal");
        assert!(matches!(err_parent, PlatformError::PathEscape { .. }));

        let err_abs = jail
            .resolve("/etc/passwd")
            .expect_err("must reject absolute path");
        assert!(matches!(err_abs, PlatformError::PathEscape { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn safe_path_rejects_symlink_pointing_outside_root() {
        let root = TestDir::new("jail-root");
        let outside = TestDir::new("jail-outside");
        let secret_file = outside.0.join("secret.txt");
        fs::write(&secret_file, b"top-secret").expect("write outside file");

        let link_path = root.0.join("escape.txt");
        std::os::unix::fs::symlink(&secret_file, &link_path).expect("create symlink");

        let jail = SafePath::new(&root.0).expect("init jail");
        let err = jail
            .resolve("escape.txt")
            .expect_err("must reject symlink escaping root");
        assert!(matches!(err, PlatformError::PathEscape { .. }));
    }

    #[test]
    fn safe_path_set_allows_multiple_roots_and_blocks_outside() {
        let home_root = TestDir::new("fp-home");
        let media_root = TestDir::new("fp-media");
        let outside = TestDir::new("fp-outside");

        let media_file = media_root.0.join("plugin.zip");
        let outside_file = outside.0.join("other.zip");
        fs::write(&media_file, b"zip").expect("write media file");
        fs::write(&outside_file, b"nope").expect("write outside file");

        let set = SafePathSet::new(vec![home_root.0.clone(), media_root.0.clone()])
            .expect("init SafePathSet");
        let ok = set.resolve(&media_file).expect("media root allowed");
        assert_eq!(ok, fs::canonicalize(&media_file).expect("canonical media"));

        let err = set
            .resolve(&outside_file)
            .expect_err("outside path must fail");
        assert!(matches!(err, PlatformError::PathEscape { .. }));
    }

    #[test]
    fn atomic_write_replaces_existing_file_without_leaving_temp_files() {
        let dir = TestDir::new("atomic");
        let target = dir.0.join("loader.json");

        atomic_write(&target, br#"{"v":1}"#).expect("first write");
        atomic_write(&target, br#"{"v":2}"#).expect("second write");

        let content = fs::read_to_string(&target).expect("read target");
        assert_eq!(content, r#"{"v":2}"#);

        let leftovers: Vec<_> = fs::read_dir(&dir.0)
            .expect("read dir")
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert_eq!(leftovers.len(), 0);
    }

    #[test]
    fn platform_paths_respects_unprivileged_and_privileged_overrides() {
        let env = HashMap::from([
            ("UNPRIVILEGED_PATH", "/tmp/zd-unpriv"),
            ("PRIVILEGED_PATH", "/tmp/zd-priv"),
            ("DECKY_USER_HOME", "/home/tester"),
            ("HOME", "/home/tester"),
        ]);
        let paths = PlatformPaths::from_lookup(|k| env.get(k).map(|v| (*v).to_owned()));

        assert_eq!(paths.plugins_dir(), PathBuf::from("/tmp/zd-priv/plugins"));
        assert_eq!(
            paths.settings_dir(),
            PathBuf::from("/tmp/zd-unpriv/settings")
        );
        assert_eq!(paths.data_dir(), PathBuf::from("/tmp/zd-unpriv/data"));
        assert_eq!(paths.logs_dir(), PathBuf::from("/tmp/zd-unpriv/logs"));
        assert!(
            paths
                .filepicker_roots()
                .contains(&PathBuf::from("/home/tester"))
        );
        assert!(
            paths
                .filepicker_roots()
                .contains(&PathBuf::from("/run/media"))
        );
        assert_eq!(
            PrivilegeTarget::from_requires_root(true),
            PrivilegeTarget::Root
        );
        assert_eq!(
            PrivilegeTarget::from_requires_root(false),
            PrivilegeTarget::HostUser
        );
    }
}
