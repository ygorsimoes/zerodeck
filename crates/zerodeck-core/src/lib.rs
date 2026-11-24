//! Core domain services for `ZeroDeck`: atomic settings, plugin discovery, and archive validation.

mod archive;
mod discovery;
mod error;
mod settings;

pub use archive::{
    DEFAULT_MAX_ARCHIVE_BYTES, InstallSource, extract_zip_safely, extract_zip_safely_with_limit,
    verify_sha256_policy,
};
pub use discovery::{DiscoveredPlugin, discover_plugins};
pub use error::CoreError;
pub use settings::SettingsManager;

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Cursor, Write};
    use std::path::PathBuf;

    use sha2::{Digest, Sha256};
    use uuid::Uuid;
    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    use zerodeck_protocol::PluginLoadType;

    use crate::{
        CoreError, InstallSource, SettingsManager, discover_plugins, extract_zip_safely,
        verify_sha256_policy,
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
    fn settings_corrupt_json_returns_error_and_preserves_disk_file() {
        let dir = TestDir::new("settings-corrupt");
        let path = dir.0.join("loader.json");
        fs::write(&path, "{bad json").expect("write corrupt settings");

        let err = SettingsManager::open(path.clone()).expect_err("must reject corrupt JSON");
        assert!(matches!(err, CoreError::SettingsParse { .. }));

        let on_disk = fs::read_to_string(&path).expect("read settings after error");
        assert_eq!(on_disk, "{bad json");
    }

    #[test]
    fn settings_set_persists_atomically_across_reopen() {
        let dir = TestDir::new("settings-persist");
        let path = dir.0.join("loader.json");

        let mut manager = SettingsManager::open(path.clone()).expect("open new settings");
        manager
            .set("pluginOrder", vec!["Alpha", "Beta"])
            .expect("set pluginOrder");

        let reopened = SettingsManager::open(path).expect("reopen settings");
        let order: Vec<String> = reopened.get("pluginOrder").expect("pluginOrder must exist");
        assert_eq!(order, vec!["Alpha".to_owned(), "Beta".to_owned()]);
    }

    #[test]
    fn discover_plugins_detects_esmodule_legacy_and_passive() {
        let dir = TestDir::new("discover");

        let esm = dir.0.join("esm-plugin");
        fs::create_dir_all(&esm).expect("create esm dir");
        fs::write(
            esm.join("plugin.json"),
            r#"{"name": "ESM Plugin", "author": "Alice", "api_version": 1}"#,
        )
        .expect("write esm plugin.json");
        fs::write(
            esm.join("package.json"),
            r#"{"version": "2.0.0", "type": "module"}"#,
        )
        .expect("write esm package.json");
        fs::write(esm.join("main.py"), "class Plugin: pass").expect("write main.py");

        let legacy = dir.0.join("legacy-plugin");
        fs::create_dir_all(&legacy).expect("create legacy dir");
        fs::write(
            legacy.join("plugin.json"),
            r#"{"name": "Legacy Plugin", "author": "Bob", "flags": ["root"]}"#,
        )
        .expect("write legacy plugin.json");
        fs::write(legacy.join("main.py"), "class Plugin: pass").expect("write legacy main.py");

        let passive = dir.0.join("passive-plugin");
        fs::create_dir_all(&passive).expect("create passive dir");
        fs::write(
            passive.join("plugin.json"),
            r#"{"name": "Passive Plugin", "author": "Carol"}"#,
        )
        .expect("write passive plugin.json");

        let mut plugins = discover_plugins(&dir.0).expect("discover plugins");
        plugins.sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));

        assert_eq!(plugins.len(), 3);
        assert_eq!(plugins[0].manifest.name, "ESM Plugin");
        assert_eq!(plugins[0].load_type, PluginLoadType::EsModuleV1);
        assert!(!plugins[0].passive);
        assert!(plugins[0].has_entrypoint);

        assert_eq!(plugins[1].manifest.name, "Legacy Plugin");
        assert_eq!(plugins[1].load_type, PluginLoadType::LegacyEvalIife);
        assert!(!plugins[1].passive);
        assert!(plugins[1].manifest.requires_root());

        assert_eq!(plugins[2].manifest.name, "Passive Plugin");
        assert!(plugins[2].passive);
        assert!(!plugins[2].has_entrypoint);
    }

    #[test]
    fn extract_zip_safely_rejects_zip_slip_traversal() {
        let dir = TestDir::new("zip-slip");
        let dest = dir.0.join("out");
        fs::create_dir_all(&dest).expect("create out dir");

        let mut buf = Cursor::new(Vec::new());
        {
            let mut writer = ZipWriter::new(&mut buf);
            writer
                .start_file("../../evil.txt", SimpleFileOptions::default())
                .expect("start evil entry");
            writer.write_all(b"pwned").expect("write evil entry");
            writer.finish().expect("finish zip");
        }

        let err = extract_zip_safely(buf.get_ref(), &dest).expect_err("must reject Zip-Slip");
        assert!(matches!(err, CoreError::UnsafeArchiveEntry { .. }));
        assert_eq!(fs::read_dir(&dest).expect("read out dir").count(), 0);
    }

    #[test]
    fn verify_sha256_policy_enforces_remote_and_local_rules() {
        let payload = b"zerodeck-plugin-bytes";
        let expected_hex = hex::encode(Sha256::digest(payload));

        let err_missing_remote =
            verify_sha256_policy(payload, &InstallSource::Remote { sha256: None }, false)
                .expect_err("remote without hash must fail");
        assert!(matches!(err_missing_remote, CoreError::MissingSha256));

        let ok_remote = verify_sha256_policy(
            payload,
            &InstallSource::Remote {
                sha256: Some(expected_hex.clone()),
            },
            false,
        )
        .expect("matching remote hash must succeed");
        assert_eq!(ok_remote, expected_hex);

        let err_local_unconfirmed = verify_sha256_policy(
            payload,
            &InstallSource::LocalFile {
                sha256: None,
                user_confirmed: false,
            },
            false,
        )
        .expect_err("unconfirmed local sideload without hash must fail");
        assert!(matches!(
            err_local_unconfirmed,
            CoreError::ConfirmationRequired
        ));

        let ok_local_confirmed = verify_sha256_policy(
            payload,
            &InstallSource::LocalFile {
                sha256: None,
                user_confirmed: true,
            },
            false,
        )
        .expect("confirmed local sideload succeeds");
        assert_eq!(ok_local_confirmed, expected_hex);

        let whitespace_remote = verify_sha256_policy(
            payload,
            &InstallSource::Remote {
                sha256: Some("   ".to_owned()),
            },
            false,
        )
        .expect_err("whitespace-only hash must be treated as missing");
        assert!(matches!(whitespace_remote, CoreError::MissingSha256));

        let mismatch = verify_sha256_policy(
            payload,
            &InstallSource::Remote {
                sha256: Some("0".repeat(64)),
            },
            false,
        )
        .expect_err("wrong sha256 must fail");
        assert!(matches!(mismatch, CoreError::Sha256Mismatch { .. }));

        let allowed_override =
            verify_sha256_policy(payload, &InstallSource::Remote { sha256: None }, true)
                .expect("allow_unverified overrides missing hash");
        assert_eq!(allowed_override, expected_hex);
    }

    #[test]
    fn extract_zip_safely_rejects_symlinks_and_oversized_entries() {
        let dir = TestDir::new("zip-hardening");
        let dest = dir.0.join("out");
        fs::create_dir_all(&dest).expect("create out dir");

        let mut sym_buf = Cursor::new(Vec::new());
        {
            let mut writer = ZipWriter::new(&mut sym_buf);
            writer
                .add_symlink("link", "/etc/passwd", SimpleFileOptions::default())
                .expect("add symlink");
            writer.finish().expect("finish symlink zip");
        }
        let err_sym =
            extract_zip_safely(sym_buf.get_ref(), &dest).expect_err("must reject zip symlink");
        assert!(matches!(err_sym, CoreError::UnsafeArchiveEntry { .. }));

        let mut big_buf = Cursor::new(Vec::new());
        {
            let mut writer = ZipWriter::new(&mut big_buf);
            writer
                .start_file("huge.bin", SimpleFileOptions::default())
                .expect("start huge entry");
            writer
                .write_all(&vec![b'A'; 1024])
                .expect("write huge bytes");
            writer.finish().expect("finish huge zip");
        }
        let err_limit = crate::extract_zip_safely_with_limit(big_buf.get_ref(), &dest, 512)
            .expect_err("must reject entry exceeding byte limit");
        assert!(matches!(err_limit, CoreError::ArchiveSizeExceeded { .. }));
    }
}
