//! Core domain services for `ZeroDeck`: atomic settings, plugin discovery, and archive validation.

mod archive;
mod discovery;
mod error;
pub mod filepicker;
pub mod http_policy;
pub mod installer;
mod manager;
mod settings;

pub use archive::{
    DEFAULT_MAX_ARCHIVE_BYTES, DEFAULT_MAX_ARCHIVE_ENTRIES, ExtractLimits, InstallSource,
    extract_zip_safely, extract_zip_safely_with_limit, extract_zip_safely_with_limits,
    verify_sha256_policy,
};
pub use discovery::{DiscoveredPlugin, discover_plugins};
pub use error::CoreError;
pub use filepicker::{
    DEFAULT_FILEPICKER_MAX_ENTRIES, FilePicker, FilePickerEntry, FilePickerListing,
    FilePickerQuery, MAX_FILEPICKER_MAX_ENTRIES,
};
pub use http_policy::{
    CheckedHttpClient, DEFAULT_MAX_REDIRECTS, OutboundHttpPolicy, ValidatedResponse,
    send_checked_request,
};
pub use installer::{
    DEFAULT_MAX_REMOTE_BINARY_BYTES, DEFAULT_MAX_ZIP_DOWNLOAD_BYTES, InstallOutcome,
    InstallPluginRequest, InstallPluginSource, InstallerLimits, StagedPluginInstall,
    commit_staged_plugin, parse_install_source, prepare_staged_plugin, sanitize_directory_name,
};
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

    #[cfg(unix)]
    #[test]
    fn extract_zip_normalizes_permissions_without_unix_mode() {
        use std::os::unix::fs::PermissionsExt;

        if std::env::var_os("ZERODECK_TEST_UMASK_077").is_none() {
            let exe = std::env::current_exe().expect("current test binary");
            let status = std::process::Command::new("sh")
                .arg("-c")
                .arg("umask 077 && exec \"$1\" --exact \"$2\"")
                .arg("sh")
                .arg(&exe)
                .arg("tests::extract_zip_normalizes_permissions_without_unix_mode")
                .env("ZERODECK_TEST_UMASK_077", "1")
                .status()
                .expect("spawn umask 077 subtest");
            assert!(status.success(), "umask 077 subtest must succeed");
            return;
        }

        let dir = TestDir::new("zip-perms-umask077");
        let dest = dir.0.join("plugin");

        let mut buf = Cursor::new(Vec::new());
        {
            let mut writer = ZipWriter::new(&mut buf);
            let stripped = SimpleFileOptions::default().unix_permissions(0);
            let dos_no_mode = SimpleFileOptions::default().external_attributes(0);
            let executable = SimpleFileOptions::default().unix_permissions(0o755);

            writer
                .add_directory("explicit_dir", stripped)
                .expect("add explicit dir");
            writer
                .start_file("sub/file.txt", stripped)
                .expect("start stripped file");
            writer.write_all(b"hello").expect("write stripped file");
            writer
                .start_file("sub/dos.txt", dos_no_mode)
                .expect("start dos file");
            writer.write_all(b"dos").expect("write dos file");
            writer
                .start_file("sub/bin/run", executable)
                .expect("start exec file");
            writer.write_all(b"#!/bin/sh\n").expect("write exec file");
            writer.finish().expect("finish zip");
        }

        extract_zip_safely(buf.get_ref(), &dest).expect("extract zip under umask 077");

        let mode_of = |rel: &str| -> u32 {
            let p = if rel.is_empty() {
                dest.clone()
            } else {
                dest.join(rel)
            };
            fs::metadata(&p)
                .unwrap_or_else(|e| panic!("stat {}: {e}", p.display()))
                .permissions()
                .mode()
                & 0o777
        };

        assert_eq!(mode_of(""), 0o755, "destination root must be 0o755");
        assert_eq!(mode_of("explicit_dir"), 0o755, "explicit dir must be 0o755");
        assert_eq!(mode_of("sub"), 0o755, "implicit parent dir must be 0o755");
        assert_eq!(mode_of("sub/bin"), 0o755, "nested parent dir must be 0o755");
        assert_eq!(
            mode_of("sub/file.txt"),
            0o644,
            "stripped file must be normalized to 0o644"
        );
        assert_eq!(
            mode_of("sub/dos.txt"),
            0o644,
            "DOS entry without unix_mode must be normalized to 0o644"
        );
        assert_eq!(
            mode_of("sub/bin/run"),
            0o755,
            "executable file must remain 0o755"
        );
    }

    #[test]
    fn http_policy_blocks_loopback_metadata_and_allows_private_lan() {
        let policy = crate::http_policy::OutboundHttpPolicy::production();

        for blocked in [
            "",
            "   ",
            "not a url",
            "http://example.com:0/x",
            "ftp://example.com/file.zip",
            "http://127.0.0.1/x",
            "http://[::1]:8080/json",
            "http://localhost:1337/auth/token",
            "http://LOCALHOST./auth/token",
            "http://sub.localhost/a",
            "http://0.0.0.0/x",
            "http://0.42.0.1/x",
            "http://[::]/x",
            "http://169.254.169.254/latest",
            "http://[fe80::1]/",
            "http://[::ffff:127.0.0.1]:1337/auth/token",
            "http://[::ffff:169.254.169.254]/latest",
            "http://[::127.0.0.1]/x",
            "http://224.0.0.1/x",
            "http://[ff02::1]/x",
            "file:///etc/passwd",
        ] {
            let err = policy
                .validate_url(blocked)
                .expect_err(&format!("production policy must reject `{blocked}`"));
            assert!(
                matches!(err, CoreError::HttpPolicy(_)),
                "expected HttpPolicy error for `{blocked}`, got {err:?}"
            );
        }

        for allowed in [
            "https://plugins.deckbrew.xyz/a.zip",
            "http://192.168.1.50:8096",
            "http://10.0.0.5:8123",
            "http://172.16.4.2",
            "http://[fd00::1]/api",
            "http://[::ffff:192.168.1.50]/api",
        ] {
            policy
                .validate_url(allowed)
                .unwrap_or_else(|e| panic!("production policy must accept `{allowed}`: {e}"));
        }
    }

    async fn spawn_http_redirect_mock() -> (u16, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let port = listener.local_addr().expect("mock addr").port();

        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let req = String::from_utf8_lossy(&buf);
                let resp = if req.starts_with("GET /redirect ") {
                    "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1337/auth/token\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                } else if req.starts_with("GET /metadata ") {
                    "HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/latest/meta-data/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                } else if req.starts_with("GET /hop ") {
                    "HTTP/1.1 302 Found\r\nLocation: /ok\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                } else if req.starts_with("GET /loop ") {
                    "HTTP/1.1 302 Found\r\nLocation: /loop\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                } else if req.starts_with("GET /ok ") {
                    "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
                } else {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                };
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });

        (port, handle)
    }

    #[tokio::test]
    async fn http_policy_blocks_redirect_into_loopback() {
        let (port, server) = spawn_http_redirect_mock().await;
        let policy =
            crate::http_policy::OutboundHttpPolicy::for_loopback_ports_for_testing(vec![port]);
        let client = policy.build_checked_client().expect("build checked client");

        for path in ["redirect", "metadata", "loop"] {
            let err = crate::http_policy::send_checked_request(
                &client,
                reqwest::Method::GET,
                &format!("http://127.0.0.1:{port}/{path}"),
                reqwest::header::HeaderMap::new(),
                None,
                5,
            )
            .await
            .expect_err(&format!("`/{path}` must be blocked by policy"));
            assert!(
                matches!(err, CoreError::HttpPolicy(_)),
                "expected HttpPolicy for `/{path}`, got {err:?}"
            );
        }

        let ok = crate::http_policy::send_checked_request(
            &client,
            reqwest::Method::GET,
            &format!("http://127.0.0.1:{port}/hop"),
            reqwest::header::HeaderMap::new(),
            None,
            5,
        )
        .await
        .expect("allowed relative redirect on mock port must succeed");
        assert_eq!(ok.status(), reqwest::StatusCode::OK);
        assert_eq!(ok.text().await.expect("body"), "ok");

        let prod_policy = crate::http_policy::OutboundHttpPolicy::production();
        let prod_client = prod_policy
            .build_checked_client()
            .expect("build production checked client");
        let err_prod = crate::http_policy::send_checked_request(
            &prod_client,
            reqwest::Method::GET,
            &format!("http://127.0.0.1:{port}/ok"),
            reqwest::header::HeaderMap::new(),
            None,
            5,
        )
        .await
        .expect_err("production policy must block loopback port even if server is listening");
        assert!(matches!(err_prod, CoreError::HttpPolicy(_)));

        assert!(matches!(
            policy.validate_ip(std::net::IpAddr::from([127, 0, 0, 1])),
            Err(CoreError::HttpPolicy(_))
        ));

        let allow_all = crate::http_policy::OutboundHttpPolicy::production()
            .with_allow_loopback_for_tests(true);
        allow_all
            .validate_ip(std::net::IpAddr::from([127, 0, 0, 1]))
            .expect("with_allow_loopback_for_tests(true) permits loopback IP");

        server.abort();
    }

    async fn read_http_request_raw(sock: &mut tokio::net::TcpStream) -> String {
        use tokio::io::AsyncReadExt;

        let mut buf = Vec::with_capacity(2048);
        let mut chunk = [0u8; 512];
        while let Ok(n) = sock.read(&mut chunk).await {
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let header_end = pos + 4;
                let headers_str = String::from_utf8_lossy(&buf[..header_end]);
                let content_len = headers_str
                    .lines()
                    .find_map(|line| {
                        let (k, v) = line.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if buf.len() >= header_end + content_len {
                    break;
                }
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    #[tokio::test]
    async fn http_policy_strips_sensitive_headers_cross_origin_and_preserves_307_body() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let target_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind target listener");
        let target_port = target_listener.local_addr().expect("target addr").port();
        let origin_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind origin listener");
        let origin_port = origin_listener.local_addr().expect("origin addr").port();

        let origin_task = tokio::spawn(async move {
            for _ in 0..2 {
                let Ok((mut sock, _)) = origin_listener.accept().await else {
                    break;
                };
                let req = read_http_request_raw(&mut sock).await;
                let resp = if req.starts_with("POST /307 ") {
                    format!(
                        "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://127.0.0.1:{target_port}/echo\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                } else {
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{target_port}/echo\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                };
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });

        let target_task = tokio::spawn(async move {
            for _ in 0..2 {
                let Ok((mut sock, _)) = target_listener.accept().await else {
                    break;
                };
                let req = read_http_request_raw(&mut sock).await;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{req}",
                    req.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });

        let policy = crate::http_policy::OutboundHttpPolicy::for_loopback_ports_for_testing(vec![
            origin_port,
            target_port,
        ]);
        let client = policy.build_checked_client().expect("build checked client");

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            "Bearer secret".parse().expect("header"),
        );
        headers.insert(reqwest::header::COOKIE, "sid=xyz".parse().expect("header"));
        headers.insert(
            reqwest::header::HOST,
            "spoof.example".parse().expect("header"),
        );
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            "text/plain".parse().expect("header"),
        );

        let res_307 = crate::http_policy::send_checked_request(
            &client,
            reqwest::Method::POST,
            &format!("http://127.0.0.1:{origin_port}/307"),
            headers.clone(),
            Some(reqwest::Body::from("payload-307")),
            5,
        )
        .await
        .expect("307 cross-origin redirect must succeed");
        let echoed_307 = res_307.text().await.expect("307 body");
        assert!(echoed_307.starts_with("POST /echo "));
        assert!(echoed_307.ends_with("payload-307"));
        let lowered_307 = echoed_307.to_ascii_lowercase();
        assert!(!lowered_307.contains("authorization: bearer"));
        assert!(!lowered_307.contains("cookie: sid=xyz"));
        assert!(!lowered_307.contains("spoof.example"));

        let res_302 = crate::http_policy::send_checked_request(
            &client,
            reqwest::Method::POST,
            &format!("http://127.0.0.1:{origin_port}/302"),
            headers,
            Some(reqwest::Body::from("payload-302")),
            5,
        )
        .await
        .expect("302 cross-origin redirect must succeed");
        let echoed_302 = res_302.text().await.expect("302 body");
        assert!(echoed_302.starts_with("GET /echo "));
        assert!(!echoed_302.contains("payload-302"));
        assert!(!echoed_302.to_ascii_lowercase().contains("content-type:"));

        origin_task.abort();
        target_task.abort();
    }

    #[tokio::test]
    async fn checked_http_client_preserves_timeouts_and_user_agent() {
        use std::time::Duration;
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock listener");
        let port = listener.local_addr().expect("mock addr").port();

        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let req = read_http_request_raw(&mut sock).await;
                if req.starts_with("GET /slow ") {
                    tokio::time::sleep(Duration::from_millis(350)).await;
                }
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{req}",
                    req.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });

        let policy =
            crate::http_policy::OutboundHttpPolicy::for_loopback_ports_for_testing(vec![port])
                .with_timeouts(
                    Some(Duration::from_secs(2)),
                    Some(Duration::from_millis(100)),
                )
                .with_user_agent("ZeroDeck-Test/1.0");
        let client = crate::http_policy::CheckedHttpClient::new(policy).expect("checked client");

        let ok = crate::http_policy::send_checked_request(
            &client,
            reqwest::Method::GET,
            &format!("http://127.0.0.1:{port}/ua"),
            reqwest::header::HeaderMap::new(),
            None,
            5,
        )
        .await
        .expect("ua request must succeed");
        let body = ok.text().await.expect("ua body");
        assert!(
            body.to_ascii_lowercase()
                .contains("user-agent: zerodeck-test/1.0")
        );

        let err_timeout = crate::http_policy::send_checked_request(
            &client,
            reqwest::Method::GET,
            &format!("http://127.0.0.1:{port}/slow"),
            reqwest::header::HeaderMap::new(),
            None,
            5,
        )
        .await
        .expect_err("slow response must hit CheckedHttpClient response timeout");
        assert!(matches!(err_timeout, CoreError::HttpRequest(_)));

        server.abort();
    }

    #[test]
    fn settings_dotted_path_reads_writes_and_rejects_non_object_intermediate() {
        let dir = TestDir::new("settings-dotted");
        let path = dir.0.join("loader.json");

        let mut manager = SettingsManager::open(path.clone()).expect("open new settings");
        assert!(manager.values().is_empty());
        assert_eq!(
            manager.get_path("developer.allow_unverified_installs"),
            None
        );

        manager
            .set_path(
                "developer.allow_unverified_installs",
                serde_json::json!(true),
            )
            .expect("set nested bool");
        assert_eq!(
            manager.get_path("developer.allow_unverified_installs"),
            Some(serde_json::json!(true))
        );

        manager
            .set_path("developer.sub", serde_json::json!(1))
            .expect("set sibling nested key");

        let reopened = SettingsManager::open(path.clone()).expect("reopen settings");
        assert_eq!(
            reopened.get_path("developer.allow_unverified_installs"),
            Some(serde_json::json!(true))
        );
        assert_eq!(
            reopened.get_path("developer.sub"),
            Some(serde_json::json!(1))
        );

        manager
            .set_path("scalar", serde_json::json!("x"))
            .expect("set scalar");
        manager
            .set_path("arr", serde_json::json!([1, 2]))
            .expect("set array");
        manager
            .set_path("developer.nested_num", serde_json::json!(42))
            .expect("set nested number");
        let snapshot_before_err = fs::read_to_string(&path).expect("read loader.json before err");

        for bad_nested in ["scalar.child", "arr.0", "developer.nested_num.leaf"] {
            let err_scalar = manager
                .set_path(bad_nested, serde_json::json!(1))
                .expect_err("must reject traversing non-object intermediate");
            assert!(matches!(err_scalar, CoreError::InvalidSetting(_)));
        }

        for invalid in ["", "a..b", ".a", "a."] {
            assert_eq!(manager.get_path(invalid), None);
            let err_invalid = manager
                .set_path(invalid, serde_json::json!(1))
                .expect_err("must reject invalid dotted key");
            assert!(matches!(err_invalid, CoreError::InvalidSetting(_)));
        }

        let snapshot_after_err = fs::read_to_string(&path).expect("read loader.json after err");
        assert_eq!(snapshot_after_err, snapshot_before_err);

        let removed = manager.remove("scalar").expect("remove scalar");
        assert_eq!(removed, Some(serde_json::json!("x")));
        assert_eq!(manager.remove("missing").expect("remove missing"), None);

        let reopened_after_remove =
            SettingsManager::open(path).expect("reopen settings after remove");
        assert_eq!(reopened_after_remove.get_path("scalar"), None);
        assert_eq!(
            reopened_after_remove.get_path("developer.allow_unverified_installs"),
            Some(serde_json::json!(true))
        );
    }

    fn filepicker_test_paths(
        home: &std::path::Path,
        media: &std::path::Path,
    ) -> zerodeck_platform::PlatformPaths {
        let home_str = home.display().to_string();
        let media_str = media.display().to_string();
        let lookup = std::collections::HashMap::from([
            ("DECKY_USER_HOME", home_str.as_str()),
            ("HOME", home_str.as_str()),
            ("UNPRIVILEGED_PATH", media_str.as_str()),
            ("PRIVILEGED_PATH", media_str.as_str()),
        ]);
        zerodeck_platform::PlatformPaths::from_lookup(|k| lookup.get(k).map(|v| (*v).to_owned()))
    }

    fn entry_names(listing: &crate::FilePickerListing) -> Vec<&str> {
        listing.files.iter().map(|f| f.name.as_str()).collect()
    }

    #[test]
    fn filepicker_lists_allowed_roots_filters_extensions_and_omits_escaping_symlinks() {
        let dir = TestDir::new("filepicker");
        let home = dir.0.join("home");
        let media = dir.0.join("media");
        let outside = TestDir::new("filepicker-outside");

        fs::create_dir_all(home.join("sub")).expect("create home/sub");
        fs::create_dir_all(&media).expect("create media root");
        fs::write(home.join(".hidden"), b"secret-dot").expect("write .hidden");
        fs::write(home.join("a.zip"), b"zip-bytes").expect("write a.zip");
        fs::write(home.join("b.txt"), b"txt").expect("write b.txt");
        fs::write(media.join("pkg.zip"), b"media-pkg").expect("write media/pkg.zip");
        let outside_secret = outside.0.join("secret.txt");
        fs::write(&outside_secret, b"outside").expect("write outside secret");

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside_secret, home.join("leak"))
                .expect("create escaping symlink");
            std::os::unix::fs::symlink(dir.0.join("missing-target"), home.join("dangling"))
                .expect("create broken symlink");
            std::os::unix::fs::symlink(&media, home.join("to_media"))
                .expect("create allowed cross-root symlink");
        }

        let paths = filepicker_test_paths(&home, &media);
        let listing = crate::FilePicker::list(&paths, crate::FilePickerQuery::default())
            .expect("default home listing must succeed without /run/media");
        assert!(!listing.truncated);
        assert_eq!(
            listing.realpath,
            fs::canonicalize(&home)
                .expect("canonical home")
                .display()
                .to_string()
        );

        #[cfg(unix)]
        assert_eq!(
            entry_names(&listing),
            vec!["sub", "to_media", "a.zip", "b.txt"]
        );
        #[cfg(not(unix))]
        assert_eq!(entry_names(&listing), vec!["sub", "a.zip", "b.txt"]);

        let sub = listing.files.iter().find(|f| f.name == "sub").expect("sub");
        assert!(sub.isdir && sub.size == 0 && sub.modified > 0.0);
        let zip = listing
            .files
            .iter()
            .find(|f| f.name == "a.zip")
            .expect("a.zip");
        assert!(!zip.isdir && zip.size == 9 && zip.modified > 0.0);

        let hidden = crate::FilePicker::list(
            &paths,
            crate::FilePickerQuery {
                show_hidden: true,
                ..crate::FilePickerQuery::default()
            },
        )
        .expect("show_hidden listing");
        assert!(entry_names(&hidden).contains(&".hidden"));

        let ext = crate::FilePicker::list(
            &paths,
            crate::FilePickerQuery {
                extensions: vec!["ZIP".to_owned()],
                ..crate::FilePickerQuery::default()
            },
        )
        .expect("extension listing");
        assert!(entry_names(&ext).contains(&"a.zip") && entry_names(&ext).contains(&"sub"));
        assert!(!entry_names(&ext).contains(&"b.txt"));

        let media_listing = crate::FilePicker::list(
            &paths,
            crate::FilePickerQuery {
                path: Some(media.display().to_string()),
                ..crate::FilePickerQuery::default()
            },
        )
        .expect("media root listing");
        assert_eq!(entry_names(&media_listing), vec!["pkg.zip"]);

        let truncated = crate::FilePicker::list(
            &paths,
            crate::FilePickerQuery {
                max_entries: 1,
                ..crate::FilePickerQuery::default()
            },
        )
        .expect("truncated listing");
        assert!(truncated.truncated);
        assert_eq!(entry_names(&truncated), vec!["sub"]);

        let err_escape = crate::FilePicker::list(
            &paths,
            crate::FilePickerQuery {
                path: Some(outside.0.display().to_string()),
                ..crate::FilePickerQuery::default()
            },
        )
        .expect_err("outside path must fail with PathEscape");
        assert!(matches!(
            err_escape,
            CoreError::Platform(zerodeck_platform::PlatformError::PathEscape { .. })
        ));
    }

    #[test]
    fn filepicker_handles_empty_path_non_dir_filters_sorting_and_serde() {
        let dir = TestDir::new("filepicker-edge");
        let home = dir.0.join("home");
        fs::create_dir_all(home.join("Zebra")).expect("create Zebra dir");
        fs::create_dir_all(home.join("alpha")).expect("create alpha dir");
        fs::write(home.join("Beta.txt"), b"beta").expect("write Beta.txt");
        fs::write(home.join("alpha.tar.gz"), b"targz").expect("write alpha.tar.gz");
        fs::write(home.join("myzip"), b"not-zip").expect("write myzip");
        fs::write(home.join(".zip"), b"dotzip").expect("write .zip");

        let paths = filepicker_test_paths(&home, &home);

        // Empty string path defaults to home, and dirs sort before files case-insensitively.
        let listing = crate::FilePicker::list(
            &paths,
            crate::FilePickerQuery {
                path: Some(String::new()),
                ..crate::FilePickerQuery::default()
            },
        )
        .expect("empty path defaults to home");
        assert_eq!(
            entry_names(&listing),
            vec!["alpha", "Zebra", "alpha.tar.gz", "Beta.txt", "myzip"]
        );

        // Multi-part extension with leading dot matches `alpha.tar.gz` and ignores `myzip`/`.zip`.
        let ext_listing = crate::FilePicker::list(
            &paths,
            crate::FilePickerQuery {
                include_folders: false,
                show_hidden: true,
                extensions: vec![".TAR.GZ".to_owned()],
                ..crate::FilePickerQuery::default()
            },
        )
        .expect("multi-part extension filter");
        assert_eq!(entry_names(&ext_listing), vec!["alpha.tar.gz"]);

        // Case-insensitive substring and `?`/`*` glob filter.
        let sub_listing = crate::FilePicker::list(
            &paths,
            crate::FilePickerQuery {
                filter: Some("ETA.TX".to_owned()),
                ..crate::FilePickerQuery::default()
            },
        )
        .expect("substring filter");
        assert_eq!(entry_names(&sub_listing), vec!["Beta.txt"]);

        let glob_listing = crate::FilePicker::list(
            &paths,
            crate::FilePickerQuery {
                include_folders: false,
                filter: Some("?eta*".to_owned()),
                ..crate::FilePickerQuery::default()
            },
        )
        .expect("question-mark glob");
        assert_eq!(entry_names(&glob_listing), vec!["Beta.txt"]);

        // Overlong filter (> 256 chars) is rejected with InvalidSetting.
        let err_filter = crate::FilePicker::list(
            &paths,
            crate::FilePickerQuery {
                filter: Some("a".repeat(257)),
                ..crate::FilePickerQuery::default()
            },
        )
        .expect_err("overlong filter must fail");
        assert!(matches!(err_filter, CoreError::InvalidSetting(_)));

        // Querying a regular file instead of a directory fails cleanly.
        let err_file = crate::FilePicker::list(
            &paths,
            crate::FilePickerQuery {
                path: Some(home.join("Beta.txt").display().to_string()),
                ..crate::FilePickerQuery::default()
            },
        )
        .expect_err("non-directory target must fail");
        assert!(matches!(err_file, CoreError::Io { .. }));

        // Serde defaults and Decky wire fields roundtrip cleanly.
        let parsed_query: crate::FilePickerQuery =
            serde_json::from_str("{}").expect("default FilePickerQuery from JSON");
        assert_eq!(parsed_query, crate::FilePickerQuery::default());

        let json_listing = serde_json::to_value(&listing).expect("serialize FilePickerListing");
        assert!(json_listing.get("path").is_some());
        assert!(json_listing.get("realpath").is_some());
        assert!(json_listing.get("truncated").is_some());
        let first_file = &json_listing["files"][0];
        for key in ["name", "path", "realpath", "isdir", "size", "modified"] {
            assert!(first_file.get(key).is_some(), "missing key `{key}`");
        }
    }

    fn spawn_installer_mock_on(
        listener: tokio::net::TcpListener,
        routes: Vec<(String, Vec<u8>)>,
    ) -> tokio::task::JoinHandle<()> {
        use tokio::io::AsyncWriteExt;

        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let req = read_http_request_raw(&mut sock).await;
                let req_path = req
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("");

                if let Some((_, body)) = routes.iter().find(|(path, _)| path == req_path) {
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(header.as_bytes()).await;
                    let _ = sock.write_all(body).await;
                } else {
                    let _ = sock
                        .write_all(
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                }
            }
        })
    }

    async fn spawn_installer_mock(
        routes: Vec<(String, Vec<u8>)>,
    ) -> (u16, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind installer mock server");
        let port = listener.local_addr().expect("mock addr").port();
        let handle = spawn_installer_mock_on(listener, routes);
        (port, handle)
    }

    #[tokio::test]
    async fn installer_unwraps_top_level_wrapper_and_verifies_expected_version() {
        let dir = TestDir::new("inst-wrapper");
        let paths = manager_test_paths(&dir.0);
        let zip_bytes = zip_with_files(&[
            (
                "demo-v1.2.3/plugin.json",
                br#"{"name":"Demo","author":"Alice","api_version":1}"#,
            ),
            ("demo-v1.2.3/package.json", br#"{"version":"1.2.3"}"#),
            ("demo-v1.2.3/main.py", b"class Plugin: pass\n"),
            ("__MACOSX/demo-v1.2.3/._plugin.json", b"appledouble"),
        ]);
        let zip_sha = hex::encode(Sha256::digest(&zip_bytes));

        let (port, server) = spawn_installer_mock(vec![("/demo.zip".to_owned(), zip_bytes)]).await;
        let client = crate::OutboundHttpPolicy::for_loopback_ports_for_testing(vec![port])
            .build_checked_client()
            .expect("build checked client");
        let limits = crate::InstallerLimits::default();
        let url = format!("http://127.0.0.1:{port}/demo.zip");

        let wrong_version_req = crate::InstallPluginRequest {
            source: crate::InstallPluginSource::RemoteZip {
                url: url.clone(),
                sha256: Some(zip_sha.clone()),
            },
            directory_hint: None,
            expected_name: Some("Demo".to_owned()),
            expected_version: Some("9.9.9".to_owned()),
        };
        let err_ver =
            crate::prepare_staged_plugin(&paths, &client, &limits, &wrong_version_req, false)
                .await
                .expect_err("expected_version mismatch must fail");
        assert!(matches!(
            err_ver,
            CoreError::ManifestMismatch {
                field: "version",
                ..
            }
        ));
        assert_eq!(sorted_names(&paths.plugins_dir()), Vec::<String>::new());

        let wrong_name_req = crate::InstallPluginRequest {
            source: crate::InstallPluginSource::RemoteZip {
                url: url.clone(),
                sha256: Some(zip_sha.clone()),
            },
            directory_hint: None,
            expected_name: Some("Other".to_owned()),
            expected_version: Some("1.2.3".to_owned()),
        };
        let err_name =
            crate::prepare_staged_plugin(&paths, &client, &limits, &wrong_name_req, false)
                .await
                .expect_err("expected_name mismatch must fail");
        assert!(matches!(
            err_name,
            CoreError::ManifestMismatch { field: "name", .. }
        ));
        assert_eq!(sorted_names(&paths.plugins_dir()), Vec::<String>::new());

        let ok_req = crate::InstallPluginRequest {
            source: crate::InstallPluginSource::RemoteZip {
                url,
                sha256: Some(zip_sha.clone()),
            },
            directory_hint: None,
            expected_name: Some("Demo".to_owned()),
            expected_version: Some("1.2.3".to_owned()),
        };
        let staged = crate::prepare_staged_plugin(&paths, &client, &limits, &ok_req, false)
            .await
            .expect("matching wrapper archive must stage cleanly");
        assert_eq!(staged.suggested_directory(), "demo-v1.2.3");

        let target_dir = paths.plugins_dir().join(staged.suggested_directory());
        let outcome =
            crate::commit_staged_plugin(staged, &target_dir).expect("commit staged plugin");
        assert_eq!(outcome.name, "Demo");
        assert_eq!(outcome.directory, "demo-v1.2.3");
        assert_eq!(outcome.version.as_deref(), Some("1.2.3"));
        assert_eq!(outcome.sha256, zip_sha);
        assert!(!outcome.replaced_existing);

        assert!(target_dir.join("plugin.json").is_file());
        assert!(target_dir.join("package.json").is_file());
        assert!(target_dir.join("main.py").is_file());
        assert!(!target_dir.join("__MACOSX").exists());
        assert_eq!(
            sorted_names(&paths.plugins_dir()),
            vec!["demo-v1.2.3".to_owned()]
        );

        server.abort();
    }

    fn make_remote_binary_zip(port: u16, bin_name: &str, bin_sha: &str) -> (Vec<u8>, String) {
        let pkg = format!(
            r#"{{"version":"2.0.0","remote_binary":[{{"name":"{bin_name}","url":"http://127.0.0.1:{port}/bin/helper","sha256hash":"{bin_sha}"}}]}}"#
        );
        let bytes = zip_with_files(&[
            (
                "plugin.json",
                br#"{"name":"Demo","author":"Alice","api_version":1}"#,
            ),
            ("package.json", pkg.as_bytes()),
            ("main.py", b"VERSION = 'v2'\n"),
        ]);
        let sha = hex::encode(Sha256::digest(&bytes));
        (bytes, sha)
    }

    #[tokio::test]
    async fn installer_stages_remote_binary_and_preserves_existing_on_failure() {
        let dir = TestDir::new("inst-remote-bin");
        let paths = manager_test_paths(&dir.0);
        let target_dir = paths.plugins_dir().join("demo");
        fs::create_dir_all(&target_dir).expect("create v1 plugin dir");
        fs::write(
            target_dir.join("plugin.json"),
            r#"{"name":"Demo","author":"Alice","api_version":1}"#,
        )
        .expect("write v1 plugin.json");
        fs::write(target_dir.join("main.py"), "VERSION = 'v1'\n").expect("write v1 main.py");

        let bin_payload = b"#!/bin/sh\necho helper-v2\n";
        let bin_sha = hex::encode(Sha256::digest(bin_payload));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let port = listener.local_addr().expect("mock addr").port();

        let (bad_zip, bad_zip_sha) = make_remote_binary_zip(port, "helper", &"0".repeat(64));
        let (good_zip, good_zip_sha) = make_remote_binary_zip(port, "helper", &bin_sha);
        let server = spawn_installer_mock_on(
            listener,
            vec![
                ("/v2-bad.zip".to_owned(), bad_zip),
                ("/v2-good.zip".to_owned(), good_zip),
                ("/bin/helper".to_owned(), bin_payload.to_vec()),
            ],
        );

        let client = crate::OutboundHttpPolicy::for_loopback_ports_for_testing(vec![port])
            .build_checked_client()
            .expect("build checked client");
        let limits = crate::InstallerLimits::default();

        let bad_req = crate::InstallPluginRequest {
            source: crate::InstallPluginSource::RemoteZip {
                url: format!("http://127.0.0.1:{port}/v2-bad.zip"),
                sha256: Some(bad_zip_sha),
            },
            directory_hint: None,
            expected_name: None,
            expected_version: None,
        };
        let err = crate::prepare_staged_plugin(&paths, &client, &limits, &bad_req, false)
            .await
            .expect_err("bad remote_binary sha256 must fail");
        assert!(matches!(err, CoreError::Sha256Mismatch { .. }));
        assert_eq!(
            fs::read_to_string(target_dir.join("main.py")).expect("read v1 main.py"),
            "VERSION = 'v1'\n"
        );
        assert!(!target_dir.join("bin").exists());
        assert_eq!(sorted_names(&paths.plugins_dir()), vec!["demo".to_owned()]);

        let good_req = crate::InstallPluginRequest {
            source: crate::InstallPluginSource::RemoteZip {
                url: format!("http://127.0.0.1:{port}/v2-good.zip"),
                sha256: Some(good_zip_sha),
            },
            directory_hint: None,
            expected_name: None,
            expected_version: Some("2.0.0".to_owned()),
        };
        let staged = crate::prepare_staged_plugin(&paths, &client, &limits, &good_req, false)
            .await
            .expect("valid remote_binary upgrade must stage");
        let outcome =
            crate::commit_staged_plugin(staged, &target_dir).expect("commit remote_binary upgrade");
        assert!(outcome.replaced_existing);
        assert_eq!(
            fs::read_to_string(target_dir.join("main.py")).expect("read v2 main.py"),
            "VERSION = 'v2'\n"
        );

        let bin_path = target_dir.join("bin").join("helper");
        assert_eq!(
            fs::read(&bin_path).expect("read helper binary"),
            bin_payload
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&bin_path)
                .expect("stat helper binary")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o755, "remote_binary must be executable 0o755");
        }
        assert_eq!(sorted_names(&paths.plugins_dir()), vec!["demo".to_owned()]);

        server.abort();
    }

    #[tokio::test]
    async fn installer_enforces_sha256_policy_and_file_uri_sideload() {
        let dir = TestDir::new("inst-policy");
        let paths = manager_test_paths(&dir.0);
        let local_zip = paths.user_home().join("p.zip");
        let zip_bytes =
            zip_with_files(&[("plugin.json", br#"{"name":"LocalDemo","author":"Bob"}"#)]);
        fs::write(&local_zip, &zip_bytes).expect("write local zip");

        let (port, server) = spawn_installer_mock(vec![("/p.zip".to_owned(), zip_bytes)]).await;
        let client = crate::OutboundHttpPolicy::for_loopback_ports_for_testing(vec![port])
            .build_checked_client()
            .expect("build checked client");
        let limits = crate::InstallerLimits::default();

        // RemoteZip without SHA-256 fails closed unless allow_unverified == true.
        let remote_no_hash = crate::InstallPluginRequest {
            source: crate::parse_install_source(
                &format!("http://127.0.0.1:{port}/p.zip"),
                None,
                Some(&serde_json::json!(0)),
                false,
            )
            .expect("parse remote install source"),
            directory_hint: None,
            expected_name: None,
            expected_version: None,
        };
        let err_missing =
            crate::prepare_staged_plugin(&paths, &client, &limits, &remote_no_hash, false)
                .await
                .expect_err("unhashed remote install must fail closed");
        assert!(matches!(err_missing, CoreError::MissingSha256));

        let staged_unverified =
            crate::prepare_staged_plugin(&paths, &client, &limits, &remote_no_hash, true)
                .await
                .expect("allow_unverified permits unhashed remote install");
        drop(staged_unverified);
        assert_eq!(
            sorted_names(&paths.plugins_dir()),
            Vec::<String>::new(),
            "dropping uncommitted staged install must remove staging dir"
        );

        // file://<home>/p.zip parses to LocalZip and requires user_confirmed == true without hash.
        let file_uri = url::Url::from_file_path(&local_zip)
            .expect("file URI")
            .to_string();
        let unconfirmed_src =
            crate::parse_install_source(&file_uri, None, Some(&serde_json::json!("local")), false)
                .expect("parse file URI");
        assert_eq!(
            unconfirmed_src,
            crate::InstallPluginSource::LocalZip {
                path: local_zip.clone(),
                sha256: None,
                user_confirmed: false,
            }
        );

        let unconfirmed_req = crate::InstallPluginRequest {
            source: unconfirmed_src,
            directory_hint: None,
            expected_name: None,
            expected_version: None,
        };
        let err_confirm =
            crate::prepare_staged_plugin(&paths, &client, &limits, &unconfirmed_req, false)
                .await
                .expect_err("unconfirmed local zip without hash must require confirmation");
        assert!(matches!(err_confirm, CoreError::ConfirmationRequired));

        let confirmed_req = crate::InstallPluginRequest {
            source: crate::parse_install_source(&file_uri, None, None, true)
                .expect("parse confirmed file URI"),
            directory_hint: None,
            expected_name: None,
            expected_version: None,
        };
        let staged_local =
            crate::prepare_staged_plugin(&paths, &client, &limits, &confirmed_req, false)
                .await
                .expect("confirmed local sideload must stage");
        let target_dir = paths.plugins_dir().join(staged_local.suggested_directory());
        let outcome = crate::commit_staged_plugin(staged_local, &target_dir)
            .expect("commit confirmed local sideload");
        assert_eq!(outcome.name, "LocalDemo");
        assert!(target_dir.join("plugin.json").is_file());

        // Outside file:///etc/passwd must fail PathEscape even when user_confirmed == true.
        let escape_req = crate::InstallPluginRequest {
            source: crate::parse_install_source("file:///etc/passwd", None, None, true)
                .expect("parse /etc/passwd file URI"),
            directory_hint: None,
            expected_name: None,
            expected_version: None,
        };
        let err_escape = crate::prepare_staged_plugin(&paths, &client, &limits, &escape_req, false)
            .await
            .expect_err("file:///etc/passwd must fail PathEscape");
        assert!(matches!(
            err_escape,
            CoreError::Platform(zerodeck_platform::PlatformError::PathEscape { .. })
        ));

        server.abort();
    }

    #[test]
    fn installer_sanitizes_directory_names_and_validates_install_sources() {
        assert_eq!(
            crate::sanitize_directory_name("My Cool Plugin!"),
            "My-Cool-Plugin"
        );
        assert_eq!(crate::sanitize_directory_name("demo-v1.2.3"), "demo-v1.2.3");
        assert_eq!(crate::sanitize_directory_name(".hidden"), "hidden");
        assert_eq!(crate::sanitize_directory_name(".."), "plugin");
        assert_eq!(
            crate::sanitize_directory_name("../etc/passwd"),
            "etc-passwd"
        );
        assert_eq!(crate::sanitize_directory_name(""), "plugin");

        for (src, kind) in [
            ("file:///tmp/a.zip", Some(serde_json::json!("remote"))),
            ("/tmp/a.zip", Some(serde_json::json!("remote"))),
            (
                "https://example.com/a.zip",
                Some(serde_json::json!("local")),
            ),
            ("https://example.com/a.zip", Some(serde_json::json!(3))),
            ("ftp://example.com/a.zip", None),
            ("file://evil.example/etc/passwd", None),
            ("", None),
        ] {
            let err = crate::parse_install_source(src, None, kind.as_ref(), true)
                .expect_err(&format!("must reject `{src}` with {kind:?}"));
            assert!(matches!(err, CoreError::InvalidInstallSource(_)));
        }
    }

    fn write_local_zip_req(
        home: &std::path::Path,
        filename: &str,
        files: &[(&str, &[u8])],
    ) -> crate::InstallPluginRequest {
        let zip_path = home.join(filename);
        fs::write(&zip_path, zip_with_files(files)).expect("write local test zip");
        crate::InstallPluginRequest {
            source: crate::InstallPluginSource::LocalZip {
                path: zip_path,
                sha256: None,
                user_confirmed: true,
            },
            directory_hint: None,
            expected_name: None,
            expected_version: None,
        }
    }

    #[tokio::test]
    async fn installer_enforces_limits_unsafe_binary_names_and_manifest_edge_cases() {
        let dir = TestDir::new("inst-edge");
        let paths = manager_test_paths(&dir.0);
        let client = crate::OutboundHttpPolicy::production()
            .build_checked_client()
            .expect("prod client");
        let default_limits = crate::InstallerLimits::default();

        // expected_version succeeds when package.json omits `version`, and directory_hint sanitizes.
        let mut no_ver_req = write_local_zip_req(
            paths.user_home(),
            "no-ver.zip",
            &[
                ("plugin.json", br#"{"name":"NoVer","author":"Bob"}"#),
                ("package.json", br#"{"type":"module"}"#),
            ],
        );
        no_ver_req.directory_hint = Some("Custom Dir!".to_owned());
        no_ver_req.expected_version = Some("1.0.0".to_owned());

        let staged_no_ver =
            crate::prepare_staged_plugin(&paths, &client, &default_limits, &no_ver_req, false)
                .await
                .expect("omitted package.json version must not fail expected_version");
        assert_eq!(staged_no_ver.suggested_directory(), "Custom-Dir");

        // Invalid commit target dir cleans up staging on failure.
        let err_commit =
            crate::commit_staged_plugin(staged_no_ver, &paths.plugins_dir().join(".bad"))
                .expect_err("dot-prefixed commit target must fail");
        assert!(matches!(err_commit, CoreError::InvalidInstallSource(_)));
        assert_eq!(sorted_names(&paths.plugins_dir()), Vec::<String>::new());

        let bad_hint_req = crate::InstallPluginRequest {
            directory_hint: Some("../escape".to_owned()),
            ..no_ver_req.clone()
        };
        assert!(matches!(
            crate::prepare_staged_plugin(&paths, &client, &default_limits, &bad_hint_req, false)
                .await,
            Err(CoreError::InvalidInstallSource(_))
        ));

        let tight_limits = crate::InstallerLimits {
            max_zip_download_bytes: 10,
            max_remote_binary_bytes: 10,
            max_redirects: 5,
        };
        assert!(matches!(
            crate::prepare_staged_plugin(&paths, &client, &tight_limits, &no_ver_req, false).await,
            Err(CoreError::DownloadSizeExceeded { max_bytes: 10 })
        ));

        let bad_bin_req = write_local_zip_req(
            paths.user_home(),
            "bad-bin.zip",
            &[
                ("plugin.json", br#"{"name":"BadBin","author":"Bob"}"#),
                (
                    "package.json",
                    br#"{"remote_binary":[{"name":"../evil","url":"https://example.com/bin","sha256hash":"deadbeef"}]}"#,
                ),
            ],
        );
        assert!(matches!(
            crate::prepare_staged_plugin(&paths, &client, &default_limits, &bad_bin_req, false)
                .await,
            Err(CoreError::Platform(
                zerodeck_platform::PlatformError::PathEscape { .. }
            ))
        ));

        let deep_req = write_local_zip_req(
            paths.user_home(),
            "deep.zip",
            &[("a/b/plugin.json", br#"{"name":"Deep","author":"Bob"}"#)],
        );
        assert!(matches!(
            crate::prepare_staged_plugin(&paths, &client, &default_limits, &deep_req, false).await,
            Err(CoreError::MissingPluginManifest)
        ));
    }

    #[tokio::test]
    async fn installer_enforces_streaming_http_download_limits_and_http_errors() {
        use tokio::io::AsyncWriteExt;

        let dir = TestDir::new("inst-stream-limit");
        let paths = manager_test_paths(&dir.0);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stream mock");
        let port = listener.local_addr().expect("mock addr").port();

        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let req = read_http_request_raw(&mut sock).await;
                if req.starts_with("GET /chunked-huge ") {
                    let _ = sock
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n10\r\n0123456789abcdef\r\n10\r\n0123456789abcdef\r\n0\r\n\r\n",
                        )
                        .await;
                } else {
                    let _ = sock
                        .write_all(
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                }
            }
        });

        let client = crate::OutboundHttpPolicy::for_loopback_ports_for_testing(vec![port])
            .build_checked_client()
            .expect("client");
        let limits = crate::InstallerLimits {
            max_zip_download_bytes: 20,
            max_remote_binary_bytes: 20,
            max_redirects: 5,
        };

        let chunked_req = crate::InstallPluginRequest {
            source: crate::InstallPluginSource::RemoteZip {
                url: format!("http://127.0.0.1:{port}/chunked-huge"),
                sha256: None,
            },
            directory_hint: None,
            expected_name: None,
            expected_version: None,
        };
        let err_chunked =
            crate::prepare_staged_plugin(&paths, &client, &limits, &chunked_req, true)
                .await
                .expect_err("chunked body exceeding limit must fail");
        assert!(matches!(
            err_chunked,
            CoreError::DownloadSizeExceeded { max_bytes: 20 }
        ));

        let missing_req = crate::InstallPluginRequest {
            source: crate::InstallPluginSource::RemoteZip {
                url: format!("http://127.0.0.1:{port}/missing.zip"),
                sha256: None,
            },
            directory_hint: None,
            expected_name: None,
            expected_version: None,
        };
        let err_404 = crate::prepare_staged_plugin(&paths, &client, &limits, &missing_req, true)
            .await
            .expect_err("404 response must fail HttpRequest");
        assert!(matches!(err_404, CoreError::HttpRequest(_)));

        server.abort();
    }
}
