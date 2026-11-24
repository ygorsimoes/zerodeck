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
pub use privilege::{
    HostUserIdentity, ProcessIdentity, current_process_identity, ensure_owned_dir,
};
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

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_existing_file_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TestDir::new("atomic-perms");
        let target = dir.0.join("secrets.json");
        fs::write(&target, b"{}").expect("seed target");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).expect("chmod 0600");

        atomic_write(&target, br#"{"token":"x"}"#).expect("atomic write");

        let mode = fs::metadata(&target)
            .expect("stat target")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
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

    #[test]
    fn platform_paths_skips_empty_env_values_in_fallback_chain() {
        let env = HashMap::from([
            ("DECKY_USER_HOME", ""),
            ("HOME", "/home/fallback"),
            ("UNPRIVILEGED_PATH", ""),
            ("DECKY_HOME", "/opt/decky"),
        ]);
        let paths = PlatformPaths::from_lookup(|k| env.get(k).map(|v| (*v).to_owned()));
        assert_eq!(paths.user_home(), PathBuf::from("/home/fallback"));
        assert_eq!(paths.unprivileged_root(), PathBuf::from("/opt/decky"));
    }

    #[cfg(unix)]
    #[test]
    fn host_user_identity_bypasses_passwd_when_unprivileged() {
        let env = HashMap::from([("DECKY_USER", "alice"), ("DECKY_USER_HOME", "/Users/alice")]);
        let paths = PlatformPaths::from_lookup(|k| env.get(k).map(|v| (*v).to_owned()));
        let current = crate::current_process_identity();
        let resolved = crate::HostUserIdentity::resolve_from(
            &paths,
            current.uid.max(501),
            "",
            "",
            Some("alice"),
            None,
        )
        .expect("unprivileged host must bypass /etc/passwd");

        assert_eq!(resolved.username, "alice");
        assert_eq!(resolved.uid, current.uid);
        assert_eq!(resolved.gid, current.gid);
        assert_eq!(resolved.home_dir, PathBuf::from("/Users/alice"));
    }

    #[cfg(unix)]
    #[test]
    fn host_user_identity_skips_uid_zero_home_and_falls_back_to_deck_when_root() {
        let env = HashMap::from([("HOME", "/root")]);
        let paths = PlatformPaths::from_lookup(|k| env.get(k).map(|v| (*v).to_owned()));
        let passwd = "root:x:0:0:root:/root:/bin/bash\n\
                      deck:x:1000:1000:Steam Deck User:/home/deck:/bin/bash\n";
        let group = "root:x:0:\n\
                     wheel:x:998:deck\n\
                     video:x:985:someone,deck\n";

        let resolved =
            crate::HostUserIdentity::resolve_from(&paths, 0, passwd, group, None, Some(0))
                .expect("root daemon must skip uid 0 home and find deck");

        assert_eq!(resolved.username, "deck");
        assert_eq!(resolved.uid, 1000);
        assert_eq!(resolved.gid, 1000);
        assert_eq!(resolved.supplemental_gids, vec![985, 998]);
        assert_eq!(resolved.home_dir, PathBuf::from("/home/deck"));
    }

    #[cfg(unix)]
    #[test]
    fn host_user_identity_fails_closed_when_root_cannot_find_non_root_user() {
        let env = HashMap::from([("HOME", "/root")]);
        let paths = PlatformPaths::from_lookup(|k| env.get(k).map(|v| (*v).to_owned()));
        let passwd =
            "root:x:0:0:root:/root:/bin/bash\nnobody:x:65534:65534:nobody:/:/sbin/nologin\n";

        let err = crate::HostUserIdentity::resolve_from(&paths, 0, passwd, "", None, Some(0))
            .expect_err("must fail closed rather than falling back to root or nobody");
        assert!(matches!(err, PlatformError::HostUserNotFound(_)));
    }

    #[cfg(unix)]
    #[test]
    fn ensure_owned_dir_creates_directory_and_applies_mode() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TestDir::new("owned-dir");
        let target = dir.0.join("nested/ipc");
        crate::ensure_owned_dir(&target, None, Some(0o700)).expect("create 0700 dir");

        let mode = fs::metadata(&target)
            .expect("stat target")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
    }
}
