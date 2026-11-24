//! Core domain services for `ZeroDeck`: atomic settings, plugin discovery, and archive validation.

mod archive;
mod discovery;
mod error;
mod manager;
mod settings;

pub use archive::{
    DEFAULT_MAX_ARCHIVE_BYTES, DEFAULT_MAX_ARCHIVE_ENTRIES, ExtractLimits, InstallSource,
    extract_zip_safely, extract_zip_safely_with_limit, extract_zip_safely_with_limits,
    verify_sha256_policy,
};
pub use discovery::{DiscoveredPlugin, discover_plugins};
pub use error::CoreError;
pub use manager::{PluginManager, PluginSummary};
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
    fn discover_plugins_skips_invalid_manifests_and_keeps_valid_plugins() {
        let dir = TestDir::new("discover-invalid");

        let good = dir.0.join("good-plugin");
        fs::create_dir_all(&good).expect("create good dir");
        fs::write(
            good.join("plugin.json"),
            r#"{"name": "Good Plugin", "author": "Alice"}"#,
        )
        .expect("write good plugin.json");

        let bad_plugin_json = dir.0.join("bad-plugin-json");
        fs::create_dir_all(&bad_plugin_json).expect("create bad plugin.json dir");
        fs::write(bad_plugin_json.join("plugin.json"), "{not json").expect("write bad plugin.json");

        let bad_package_json = dir.0.join("bad-package-json");
        fs::create_dir_all(&bad_package_json).expect("create bad package.json dir");
        fs::write(
            bad_package_json.join("plugin.json"),
            r#"{"name": "Bad Package", "author": "Bob"}"#,
        )
        .expect("write plugin.json");
        fs::write(bad_package_json.join("package.json"), "[1, 2").expect("write bad package.json");

        let plugins = discover_plugins(&dir.0).expect("discovery must tolerate invalid plugins");
        let names: Vec<&str> = plugins.iter().map(|p| p.manifest.name.as_str()).collect();
        assert_eq!(names, vec!["Good Plugin"]);
    }

    #[test]
    fn discover_plugins_returns_plugins_sorted_by_directory() {
        let dir = TestDir::new("discover-order");
        let names = [
            "zeta", "alpha", "Mid", "beta-2", "beta-10", "omega", "kappa", "delta",
        ];
        for name in names {
            let plugin = dir.0.join(name);
            fs::create_dir_all(&plugin).expect("create plugin dir");
            fs::write(
                plugin.join("plugin.json"),
                format!(r#"{{"name": "{name}", "author": "Alice"}}"#),
            )
            .expect("write plugin.json");
        }

        let plugins = discover_plugins(&dir.0).expect("discover plugins");
        let found: Vec<&str> = plugins.iter().map(|p| p.directory.as_str()).collect();
        let mut expected = names.to_vec();
        expected.sort_unstable();
        assert_eq!(found, expected);
    }

    #[test]
    fn discover_plugins_ignores_hidden_staging_directories() {
        let dir = TestDir::new("discover-hidden");
        for name in ["visible", ".visible.staging-1234"] {
            let plugin = dir.0.join(name);
            fs::create_dir_all(&plugin).expect("create plugin dir");
            fs::write(
                plugin.join("plugin.json"),
                format!(r#"{{"name": "{name}", "author": "Alice"}}"#),
            )
            .expect("write plugin.json");
        }

        let plugins = discover_plugins(&dir.0).expect("discover plugins");
        let dirs: Vec<&str> = plugins.iter().map(|p| p.directory.as_str()).collect();
        assert_eq!(dirs, vec!["visible"]);
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

    fn zip_with_files(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut writer = ZipWriter::new(&mut buf);
            let stored =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
            for (name, data) in files {
                writer.start_file(*name, stored).expect("start entry");
                writer.write_all(data).expect("write entry");
            }
            writer.finish().expect("finish zip");
        }
        buf.into_inner()
    }

    fn sorted_names(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .expect("read dir")
            .map(|e| {
                e.expect("dir entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn extract_zip_failure_mid_write_leaves_existing_install_untouched() {
        let dir = TestDir::new("zip-staging-fail");
        let dest = dir.0.join("plugin");
        fs::create_dir_all(&dest).expect("create dest");
        fs::write(dest.join("old.txt"), b"previous install").expect("seed old install");

        // `b.txt` is stored uncompressed, so flipping one payload byte breaks its CRC. Pre-validation
        // passes and the failure only surfaces while `b.txt` is being written, after `a.txt`.
        let mut bytes = zip_with_files(&[("a.txt", b"fine"), ("b.txt", b"CORRUPTME")]);
        let pos = bytes
            .windows(9)
            .position(|w| w == b"CORRUPTME")
            .expect("find stored payload");
        bytes[pos] ^= 0xFF;

        extract_zip_safely(&bytes, &dest).expect_err("corrupt entry must fail extraction");

        assert_eq!(sorted_names(&dest), vec!["old.txt".to_owned()]);
        assert_eq!(
            fs::read(dest.join("old.txt")).expect("read old"),
            b"previous install"
        );
        assert_eq!(sorted_names(&dir.0), vec!["plugin".to_owned()]);
    }

    #[test]
    fn extract_zip_success_replaces_destination_without_leftovers() {
        let dir = TestDir::new("zip-staging-ok");
        let dest = dir.0.join("plugin");
        fs::create_dir_all(&dest).expect("create dest");
        fs::write(dest.join("old.txt"), b"previous install").expect("seed old install");

        let bytes = zip_with_files(&[("main.py", b"class Plugin: pass"), ("dist/index.js", b"x")]);
        let extracted = extract_zip_safely(&bytes, &dest).expect("extract");

        assert_eq!(
            sorted_names(&dest),
            vec!["dist".to_owned(), "main.py".to_owned()]
        );
        assert!(
            extracted
                .iter()
                .all(|p| p.starts_with(fs::canonicalize(&dest).expect("canon")))
        );
        assert!(extracted.iter().all(|p| p.is_file()));
        assert_eq!(sorted_names(&dir.0), vec!["plugin".to_owned()]);
    }

    #[test]
    fn extract_zip_rejects_archives_with_too_many_entries() {
        let dir = TestDir::new("zip-entries");
        let dest = dir.0.join("plugin");
        let bytes = zip_with_files(&[("1", b""), ("2", b""), ("3", b""), ("4", b"")]);

        let limits = crate::ExtractLimits {
            max_entries: 3,
            ..crate::ExtractLimits::default()
        };
        let err = crate::extract_zip_safely_with_limits(&bytes, &dest, &limits)
            .expect_err("must reject entry count above limit");
        assert!(matches!(
            err,
            CoreError::ArchiveTooManyEntries {
                count: 4,
                max_entries: 3
            }
        ));
        assert!(!dest.exists());
    }

    fn manager_test_paths(root: &std::path::Path) -> zerodeck_platform::PlatformPaths {
        let root_str = root.display().to_string();
        let home_str = root.join("home").display().to_string();
        fs::create_dir_all(&home_str).expect("create home");
        let lookup = std::collections::HashMap::from([
            ("UNPRIVILEGED_PATH", root_str.as_str()),
            ("PRIVILEGED_PATH", root_str.as_str()),
            ("DECKY_USER_HOME", home_str.as_str()),
            ("HOME", home_str.as_str()),
        ]);
        zerodeck_platform::PlatformPaths::from_lookup(|k| lookup.get(k).map(|v| (*v).to_owned()))
    }

    fn python_host_module_path() -> String {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../hosts/python")
            .canonicalize()
            .expect("resolve hosts/python")
            .display()
            .to_string()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn plugin_manager_respects_plugin_order_and_disabled_plugins() {
        let dir = TestDir::new("mgr-order");
        let paths = manager_test_paths(&dir.0);
        for (folder, name, has_main) in [
            ("alpha", "Alpha", false),
            ("beta", "Beta", false),
            ("gamma", "Gamma", true),
            ("delta", "Delta", false),
        ] {
            let p = paths.plugins_dir().join(folder);
            fs::create_dir_all(&p).expect("create plugin dir");
            fs::write(
                p.join("plugin.json"),
                format!(r#"{{"name":"{name}","author":"Tester","api_version":1}}"#),
            )
            .expect("write plugin.json");
            if has_main {
                fs::write(p.join("main.py"), "raise RuntimeError('must not spawn')")
                    .expect("write main.py");
            }
        }

        let mut settings =
            SettingsManager::open(paths.settings_dir().join("loader.json")).expect("open settings");
        settings
            .set("pluginOrder", vec!["Beta", "Alpha"])
            .expect("set pluginOrder");
        settings
            .set("disabledPlugins", vec!["Gamma"])
            .expect("set disabledPlugins");

        let config = zerodeck_plugin_host::SupervisorConfig {
            python_host: zerodeck_plugin_host::PythonHostConfig {
                python_bin: "python3".to_owned(),
                host_module_path: Some(python_host_module_path()),
            },
            ..zerodeck_plugin_host::SupervisorConfig::default()
        };

        let manager =
            crate::PluginManager::open(paths.clone(), config, "0.1.0").expect("open manager");
        manager.start_all().await.expect("start_all");

        let list = manager.list_plugins();
        let names: Vec<&str> = list.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["Beta", "Alpha", "Delta", "Gamma"]);

        let gamma = list.iter().find(|p| p.name == "Gamma").expect("gamma");
        assert!(gamma.disabled);
        assert_eq!(gamma.state, zerodeck_plugin_host::PluginState::Stopped);

        let err = manager
            .call_plugin_method("Gamma", "ping", serde_json::json!([]), false)
            .await
            .expect_err("disabled plugin call must fail");
        assert!(matches!(err, CoreError::PluginDisabled(ref n) if n == "Gamma"));

        // Also verify snake_case `disabled_plugins` fallback when `disabledPlugins` is absent.
        let dir2 = TestDir::new("mgr-snake");
        let paths2 = manager_test_paths(&dir2.0);
        fs::create_dir_all(paths2.settings_dir()).expect("settings dir");
        fs::write(
            paths2.settings_dir().join("loader.json"),
            r#"{"disabled_plugins":["Alpha"]}"#,
        )
        .expect("write snake_case loader.json");
        let alpha = paths2.plugins_dir().join("alpha");
        fs::create_dir_all(&alpha).expect("alpha dir");
        fs::write(
            alpha.join("plugin.json"),
            r#"{"name":"Alpha","author":"Tester"}"#,
        )
        .expect("write plugin.json");
        let manager2 = crate::PluginManager::open(
            paths2,
            zerodeck_plugin_host::SupervisorConfig::default(),
            "0.1.0",
        )
        .expect("open manager2");
        assert!(manager2.list_plugins()[0].disabled);

        manager.shutdown_all().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn plugin_manager_calls_and_reloads_active_plugin() {
        let dir = TestDir::new("mgr-reload");
        let paths = manager_test_paths(&dir.0);
        let p = paths.plugins_dir().join("echo");
        fs::create_dir_all(&p).expect("create plugin dir");
        fs::write(
            p.join("plugin.json"),
            r#"{"name":"Echo","author":"Tester","api_version":1}"#,
        )
        .expect("write plugin.json");
        fs::write(
            p.join("main.py"),
            "class Plugin:\n    async def ping(self):\n        return 'pong'\n",
        )
        .expect("write main.py");

        let config = zerodeck_plugin_host::SupervisorConfig {
            python_host: zerodeck_plugin_host::PythonHostConfig {
                python_bin: "python3".to_owned(),
                host_module_path: Some(python_host_module_path()),
            },
            ..zerodeck_plugin_host::SupervisorConfig::default()
        };

        let manager = crate::PluginManager::open(paths, config, "0.1.0").expect("open manager");
        manager.start_all().await.expect("start_all");

        let res = manager
            .call_plugin_method("Echo", "ping", serde_json::json!([]), false)
            .await
            .expect("call ping");
        assert_eq!(res, serde_json::json!("pong"));

        manager.reload_plugin("Echo").await.expect("reload Echo");
        let res2 = manager
            .call_plugin_method("Echo", "ping", serde_json::json!([]), false)
            .await
            .expect("call ping after reload");
        assert_eq!(res2, serde_json::json!("pong"));

        manager.shutdown_all().await;
    }
}
