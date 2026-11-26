//! `PluginManager` lifecycle, preflight, and settings guards.

use super::*;

#[cfg(unix)]
fn test_manager_with_http(
    paths: &zerodeck_platform::PlatformPaths,
    client: crate::CheckedHttpClient,
) -> std::sync::Arc<crate::PluginManager> {
    let settings =
        SettingsManager::open(paths.settings_dir().join("loader.json")).expect("open settings");
    std::sync::Arc::new(
        crate::PluginManager::with_http_config(
            paths.clone(),
            settings,
            Vec::new(),
            zerodeck_plugin_host::SupervisorConfig {
                python_host: zerodeck_plugin_host::PythonHostConfig {
                    python_bin: "python3".to_owned(),
                    host_module_path: Some(python_host_module_path()),
                },
                ..zerodeck_plugin_host::SupervisorConfig::default()
            },
            "0.1.0",
            client,
            crate::InstallerLimits::default(),
        )
        .expect("create PluginManager with_http_config"),
    )
}

#[cfg(unix)]
fn build_plugin_zip(
    prefix: &str,
    name: &str,
    version: &str,
    main_py: &[u8],
    extra: &[(&str, &[u8])],
) -> Vec<u8> {
    let plugin_json = format!(r#"{{"name":"{name}","author":"Alice","api_version":1}}"#);
    let package_json = format!(r#"{{"version":"{version}","type":"module"}}"#);
    let key = |rel: &str| {
        if prefix.is_empty() {
            rel.to_owned()
        } else {
            format!("{prefix}/{rel}")
        }
    };
    let mut owned: Vec<(String, Vec<u8>)> = vec![
        (key("plugin.json"), plugin_json.into_bytes()),
        (key("package.json"), package_json.into_bytes()),
        (key("main.py"), main_py.to_vec()),
    ];
    for (rel, bytes) in extra {
        owned.push((key(rel), bytes.to_vec()));
    }
    let refs: Vec<(&str, &[u8])> = owned
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_slice()))
        .collect();
    zip_with_files(&refs)
}

#[cfg(unix)]
async fn install_remote_zip(
    manager: &crate::PluginManager,
    url: String,
    sha256: String,
    expected_name: Option<&str>,
    expected_version: Option<&str>,
) -> crate::InstallOutcome {
    manager
        .install_plugin(crate::InstallPluginRequest {
            source: crate::InstallPluginSource::RemoteZip {
                url,
                sha256: Some(sha256),
            },
            directory_hint: None,
            expected_name: expected_name.map(str::to_owned),
            expected_version: expected_version.map(str::to_owned),
        })
        .await
        .expect("install remote zip")
}

#[cfg(unix)]
async fn call_plugin_ok(
    manager: &crate::PluginManager,
    plugin: &str,
    method: &str,
) -> serde_json::Value {
    manager
        .call_plugin_method(plugin, method, serde_json::json!([]), false)
        .await
        .expect("call plugin method")
}

#[cfg(unix)]
fn running_as_root() -> bool {
    zerodeck_platform::current_process_identity().euid == 0
}

/// Writes a backend plugin and a fake `python3` that leaves `marker` behind if it ever runs.
#[cfg(unix)]
fn write_spawn_marker_plugin(
    paths: &zerodeck_platform::PlatformPaths,
    root: &std::path::Path,
    marker: &std::path::Path,
) -> zerodeck_plugin_host::SupervisorConfig {
    use std::os::unix::fs::PermissionsExt;

    let plugin = paths.plugins_dir().join("blocked");
    fs::create_dir_all(&plugin).expect("create plugin dir");
    fs::write(
        plugin.join("plugin.json"),
        r#"{"name":"Blocked","author":"Tester","api_version":1}"#,
    )
    .expect("write plugin.json");
    fs::write(plugin.join("main.py"), "class Plugin:\n    pass\n").expect("write main.py");

    let fake_python = root.join("fake-python3");
    fs::write(
        &fake_python,
        format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
    )
    .expect("write fake python3");
    fs::set_permissions(&fake_python, fs::Permissions::from_mode(0o755))
        .expect("chmod fake python3");
    zerodeck_plugin_host::SupervisorConfig {
        python_host: zerodeck_plugin_host::PythonHostConfig {
            python_bin: fake_python.display().to_string(),
            host_module_path: None,
        },
        ..zerodeck_plugin_host::SupervisorConfig::default()
    }
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

    let manager = crate::PluginManager::open(paths.clone(), config, "0.1.0").expect("open manager");
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
#[tokio::test]
async fn plugin_manager_install_disable_enable_order_and_uninstall_preserve_data_by_default() {
    use std::time::Duration;

    let dir = TestDir::new("mgr-lifecycle");
    let paths = manager_test_paths(&dir.0);

    let alpha_py = br#"import os
from pathlib import Path
class Plugin:
    async def ping(self):
        return "alpha-v1"
    async def _uninstall(self):
        d = Path(os.environ["DECKY_PLUGIN_SETTINGS_DIR"])
        d.mkdir(parents=True, exist_ok=True)
        (d / "uninstalled.marker").write_text("yes", encoding="utf-8")
"#;
    let zip_alpha = build_plugin_zip(
        "",
        "AlphaPlug",
        "1.0.0",
        alpha_py,
        &[("dist/index.js", b"export default {};")],
    );
    let zip_beta = build_plugin_zip(
        "",
        "BetaPlug",
        "1.0.0",
        b"class Plugin:\n    async def ping(self):\n        return 'beta-v1'\n",
        &[],
    );

    let client = crate::CheckedHttpClient::new(crate::OutboundHttpPolicy::production())
        .expect("build checked client");
    let manager = test_manager_with_http(&paths, client);
    assert_eq!(manager.paths(), &paths);
    assert!(!manager.allow_unverified_installs());
    let _ = manager.http_client();

    let mut events = manager.subscribe_loader_events();
    let outcome_alpha =
        install_local_zip_with_hash(&manager, paths.user_home(), "alpha.zip", &zip_alpha).await;
    assert_eq!(outcome_alpha.name, "AlphaPlug");
    assert!(!outcome_alpha.replaced_existing);

    let ev = tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .expect("install event")
        .expect("broadcast ok");
    assert!(matches!(ev, crate::LoaderEvent::PluginsChanged(ref list) if list.len() == 1));
    assert_eq!(
        call_plugin_ok(&manager, "AlphaPlug", "ping").await,
        serde_json::json!("alpha-v1")
    );

    let alpha_settings_dir = paths.settings_dir().join(&outcome_alpha.directory);
    let alpha_data_dir = paths.data_dir().join(&outcome_alpha.directory);
    fs::create_dir_all(&alpha_settings_dir).expect("create alpha settings dir");
    fs::create_dir_all(&alpha_data_dir).expect("create alpha data dir");
    fs::write(alpha_data_dir.join("keep.txt"), b"keep-me").expect("write keep.txt");

    manager.disable_plugin("AlphaPlug").await.expect("disable");
    let disabled = manager
        .list_plugins()
        .into_iter()
        .find(|p| p.name == "AlphaPlug")
        .expect("AlphaPlug summary");
    assert!(disabled.disabled);
    assert_eq!(disabled.state, zerodeck_plugin_host::PluginState::Stopped);
    let err_disabled = manager
        .call_plugin_method("AlphaPlug", "ping", serde_json::json!([]), false)
        .await
        .expect_err("disabled plugin call must fail");
    assert!(matches!(err_disabled, CoreError::PluginDisabled(ref n) if n == "AlphaPlug"));

    manager.enable_plugin("AlphaPlug").await.expect("enable");
    assert_eq!(
        call_plugin_ok(&manager, "AlphaPlug", "ping").await,
        serde_json::json!("alpha-v1")
    );

    let _ = install_local_zip_with_hash(&manager, paths.user_home(), "beta.zip", &zip_beta).await;
    manager
        .set_plugin_order(vec!["BetaPlug".to_owned(), "AlphaPlug".to_owned()])
        .await
        .expect("set_plugin_order");
    let ordered_names: Vec<String> = manager.list_plugins().into_iter().map(|p| p.name).collect();
    assert_eq!(ordered_names, vec!["BetaPlug", "AlphaPlug"]);

    manager
        .uninstall_plugin("AlphaPlug", false)
        .await
        .expect("uninstall");
    assert!(!paths.plugins_dir().join(&outcome_alpha.directory).exists());
    assert_eq!(
        fs::read_to_string(alpha_settings_dir.join("uninstalled.marker")).expect("marker"),
        "yes"
    );
    assert_eq!(
        fs::read(alpha_data_dir.join("keep.txt")).expect("keep"),
        b"keep-me"
    );
    assert_eq!(
        manager.get_setting(Some("pluginOrder")),
        serde_json::json!(["BetaPlug"])
    );

    manager.shutdown_all().await;
}

#[cfg(unix)]
#[tokio::test]
async fn plugin_manager_uninstall_purge_data_removes_settings_and_data() {
    let dir = TestDir::new("mgr-purge");
    let paths = manager_test_paths(&dir.0);
    let zip_beta = build_plugin_zip(
        "",
        "BetaPlug",
        "1.0.0",
        b"class Plugin:\n    async def ping(self):\n        return 'beta-v1'\n",
        &[],
    );

    let client = crate::CheckedHttpClient::new(crate::OutboundHttpPolicy::production())
        .expect("build checked client");
    let manager = test_manager_with_http(&paths, client);
    let outcome =
        install_local_zip_with_hash(&manager, paths.user_home(), "beta.zip", &zip_beta).await;

    let settings_dir = paths.settings_dir().join(&outcome.directory);
    let data_dir = paths.data_dir().join(&outcome.directory);
    fs::create_dir_all(&settings_dir).expect("create beta settings");
    fs::create_dir_all(&data_dir).expect("create beta data");
    fs::write(settings_dir.join("cfg.json"), b"{}").expect("write cfg");
    fs::write(data_dir.join("state.bin"), b"1").expect("write state");

    manager
        .uninstall_plugin("BetaPlug", true)
        .await
        .expect("uninstall BetaPlug with purge_data = true");
    assert!(!paths.plugins_dir().join(&outcome.directory).exists());
    assert!(!settings_dir.exists());
    assert!(!data_dir.exists());
    assert_eq!(manager.list_plugins(), Vec::new());

    manager.shutdown_all().await;
}

#[cfg(unix)]
#[tokio::test]
async fn plugin_manager_set_setting_rejects_managed_keys_and_resolve_plugin_file_checks_jail() {
    let dir = TestDir::new("mgr-settings-files");
    let paths = manager_test_paths(&dir.0);

    let plugin_dir = paths.plugins_dir().join("demo-folder");
    fs::create_dir_all(plugin_dir.join("dist")).expect("create dist");
    fs::create_dir_all(plugin_dir.join("assets")).expect("create assets");
    fs::create_dir_all(paths.data_dir().join("demo-folder")).expect("create data");
    fs::write(
        plugin_dir.join("plugin.json"),
        r#"{"name":"Demo Name","author":"Alice","api_version":1}"#,
    )
    .expect("write plugin.json");
    fs::write(plugin_dir.join("dist/index.js"), b"console.log('hi');").expect("write index.js");
    fs::write(plugin_dir.join("assets/icon.svg"), b"<svg/>").expect("write icon.svg");
    fs::write(paths.data_dir().join("demo-folder/cache.json"), b"{}").expect("write cache.json");

    let outside = TestDir::new("mgr-outside");
    let outside_secret = outside.0.join("secret.txt");
    fs::write(&outside_secret, b"secret").expect("write outside secret");
    std::os::unix::fs::symlink(&outside_secret, plugin_dir.join("assets/escape.svg"))
        .expect("create escaping symlink");

    let manager = crate::PluginManager::open(
        paths.clone(),
        zerodeck_plugin_host::SupervisorConfig::default(),
        "0.1.0",
    )
    .expect("open manager");

    for protected in [
        "disabledPlugins",
        "pluginOrder",
        "disabled_plugins",
        "disabledPlugins.0",
        "pluginOrder.child",
    ] {
        let err = manager
            .set_setting(protected, serde_json::json!(["x"]))
            .await
            .expect_err(&format!("must reject managed key `{protected}`"));
        assert!(matches!(err, CoreError::InvalidSetting(_)));
    }

    manager
        .set_setting(
            "developer.allow_unverified_installs",
            serde_json::json!(true),
        )
        .await
        .expect("persist dotted developer setting");
    assert!(manager.allow_unverified_installs());
    assert_eq!(
        manager.get_setting(Some("developer.allow_unverified_installs")),
        serde_json::json!(true)
    );
    assert_eq!(
        manager.get_setting(None),
        serde_json::json!({"developer": {"allow_unverified_installs": true}})
    );

    let by_name = manager
        .resolve_plugin_file("Demo Name", crate::PluginFileScope::Assets, "icon.svg")
        .expect("resolve by manifest.name");
    assert_eq!(
        by_name,
        fs::canonicalize(plugin_dir.join("assets/icon.svg")).expect("canon icon")
    );

    let by_dir = manager
        .resolve_plugin_file("demo-folder", crate::PluginFileScope::Dist, "index.js")
        .expect("resolve by directory");
    let bundle = manager
        .resolve_plugin_file("Demo Name", crate::PluginFileScope::FrontendBundle, "")
        .expect("resolve frontend bundle");
    assert_eq!(by_dir, bundle);

    let data_file = manager
        .resolve_plugin_file("demo-folder", crate::PluginFileScope::Data, "cache.json")
        .expect("resolve data file");
    assert_eq!(
        data_file,
        fs::canonicalize(paths.data_dir().join("demo-folder/cache.json")).expect("canon data")
    );

    for bad in ["../secret", "/etc/passwd", "escape.svg", "..\\secret"] {
        let err = manager
            .resolve_plugin_file("Demo Name", crate::PluginFileScope::Assets, bad)
            .expect_err(&format!("must reject `{bad}`"));
        assert!(matches!(
            err,
            CoreError::Platform(zerodeck_platform::PlatformError::PathEscape { .. })
        ));
    }

    let static_root = dir.0.join("frontend");
    fs::create_dir_all(&static_root).expect("create frontend dir");
    fs::write(static_root.join("index.js"), b"boot();").expect("write frontend index.js");
    assert_eq!(
        crate::resolve_static_file(&static_root, "index.js").expect("crate root resolve"),
        crate::PluginManager::resolve_static_file(&static_root, "index.js")
            .expect("associated resolve")
    );
    assert!(matches!(
        crate::resolve_static_file(&static_root, "../secret"),
        Err(CoreError::Platform(
            zerodeck_platform::PlatformError::PathEscape { .. }
        ))
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn unsafe_root_plugins_opt_in_bypasses_tree_check() {
    if running_as_root() {
        eprintln!("skipping: the temp tree is root-owned when tests run as root");
        return;
    }
    let dir = TestDir::new("mgr-unsafe-root-opt-in");
    let paths = manager_test_paths(&dir.0);
    write_watcher_plugin(
        &paths.plugins_dir().join("rooted"),
        "Rooted",
        &["root"],
        "v1",
    );
    // loader.json must be ignored; only the daemon setter may opt in.
    fs::create_dir_all(paths.settings_dir()).expect("settings dir");
    fs::write(
        paths.settings_dir().join("loader.json"),
        r#"{"developer":{"allow_unsafe_root_plugins":true}}"#,
    )
    .expect("write loader.json");

    let mut manager = crate::PluginManager::open(
        paths.clone(),
        zerodeck_plugin_host::SupervisorConfig {
            python_host: zerodeck_plugin_host::PythonHostConfig {
                python_bin: "python3".to_owned(),
                host_module_path: Some(python_host_module_path()),
            },
            ..zerodeck_plugin_host::SupervisorConfig::default()
        },
        "0.1.0",
    )
    .expect("open manager");
    // The temp tree is owned by the test user, so the tree check fails as it would for a
    // deck-owned plugin under a root daemon.
    manager.assume_root_daemon_for_tests();
    manager.start_all().await.expect("start_all");

    let rooted = manager
        .list_plugins()
        .into_iter()
        .find(|p| p.name == "Rooted")
        .expect("plugin");
    assert_eq!(rooted.state, zerodeck_plugin_host::PluginState::Failed);
    let reason = rooted.failure_reason.expect("failure reason");
    assert!(
        reason.contains(&paths.plugins_dir().display().to_string()),
        "{reason}"
    );
    assert!(reason.contains("not root"), "{reason}");

    manager.set_allow_unsafe_root_plugins(true);
    manager
        .reload_plugin("Rooted")
        .await
        .expect("opt-in skips the tree check");
    assert_eq!(
        call_plugin_ok(&manager, "Rooted", "version_tag").await,
        serde_json::json!("v1")
    );
    let rooted = manager
        .list_plugins()
        .into_iter()
        .find(|p| p.name == "Rooted")
        .expect("plugin");
    assert_eq!(rooted.failure_reason, None);
    manager.shutdown_all().await;
}

#[cfg(unix)]
#[tokio::test]
async fn python_host_missing_marks_plugins_failed_with_reason() {
    let dir = TestDir::new("mgr-py-missing");
    let paths = manager_test_paths(&dir.0);
    let marker = dir.0.join("spawned");
    let config = write_spawn_marker_plugin(&paths, &dir.0, &marker);

    let manager = crate::PluginManager::open(paths, config, "0.1.0").expect("open manager");
    manager.set_python_host_missing(Some("zerodeck_py_host is not importable".to_owned()));
    manager
        .start_all()
        .await
        .expect("start_all continues past blocked plugins");

    let list = manager.list_plugins();
    let blocked = list.iter().find(|p| p.name == "Blocked").expect("plugin");
    assert_eq!(blocked.state, zerodeck_plugin_host::PluginState::Failed);
    assert_eq!(
        blocked.failure_reason.as_deref(),
        Some("zerodeck_py_host is not importable")
    );

    let err = manager
        .call_plugin_method("Blocked", "ping", serde_json::json!([]), false)
        .await
        .expect_err("a blocked plugin cannot be called");
    assert!(
        matches!(err, CoreError::PluginBlocked { ref name, .. } if name == "Blocked"),
        "{err}"
    );

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!marker.exists(), "no worker may be spawned");
    manager.shutdown_all().await;
}

#[cfg(unix)]
#[tokio::test]
async fn reload_rejects_rename_onto_another_directorys_name() {
    let dir = TestDir::new("mgr-reload-dup-name");
    let paths = manager_test_paths(&dir.0);
    write_watcher_plugin(&paths.plugins_dir().join("alpha"), "Alpha", &[], "alpha");
    let beta = paths.plugins_dir().join("beta");
    write_watcher_plugin(&beta, "Beta", &[], "beta");

    let manager = test_manager_open(&paths);
    manager.start_all().await.expect("start_all");

    // Beta's manifest now claims Alpha's name, which `alpha/` already owns.
    fs::write(
        beta.join("plugin.json"),
        r#"{"name":"Alpha","author":"Alice","flags":[],"api_version":1}"#,
    )
    .expect("rename beta's manifest");
    let err = manager
        .reload_plugin("Beta")
        .await
        .expect_err("a rename onto another directory's name must be refused");
    assert!(
        matches!(err, CoreError::DuplicatePluginName { ref name, ref directory }
                if name == "Alpha" && directory == "alpha"),
        "{err}"
    );

    let mut names: Vec<_> = manager.list_plugins().into_iter().map(|p| p.name).collect();
    names.sort();
    assert_eq!(names, ["Alpha", "Beta"]);
    assert_eq!(
        call_plugin_ok(&manager, "Alpha", "version_tag").await,
        serde_json::json!("alpha")
    );
    assert_eq!(
        call_plugin_ok(&manager, "Beta", "version_tag").await,
        serde_json::json!("beta")
    );
    manager.shutdown_all().await;
}

#[cfg(unix)]
#[tokio::test]
async fn debug_reload_runs_preflight() {
    let dir = TestDir::new("mgr-reload-preflight");
    let paths = manager_test_paths(&dir.0);
    let plugin = paths.plugins_dir().join("hot");
    write_watcher_plugin(&plugin, "Hot", &["debug"], "v1");

    let manager = test_manager_open(&paths);
    manager.start_all().await.expect("start_all");
    assert_eq!(
        call_plugin_ok(&manager, "Hot", "version_tag").await,
        serde_json::json!("v1")
    );

    let mut events = manager.subscribe_loader_events();
    manager.set_python_host_missing(Some("zerodeck_py_host is not importable".to_owned()));
    let err = manager
        .reload_plugin("Hot")
        .await
        .expect_err("reload must run the spawn preflight");
    assert!(
        matches!(err, CoreError::PluginBlocked { ref name, ref reason }
                if name == "Hot" && reason == "zerodeck_py_host is not importable"),
        "{err}"
    );
    wait_for_loader_event(&mut events, std::time::Duration::from_secs(5), |ev| {
        matches!(ev, crate::LoaderEvent::PluginsChanged(list)
                if list.iter().any(|p| p.name == "Hot" && p.failure_reason.is_some()))
    })
    .await;

    let hot = manager
        .list_plugins()
        .into_iter()
        .find(|p| p.name == "Hot")
        .expect("plugin");
    assert_eq!(hot.state, zerodeck_plugin_host::PluginState::Failed);

    // Clearing the condition lets the next spawn through and clears the recorded reason.
    manager.set_python_host_missing(None);
    manager
        .reload_plugin("Hot")
        .await
        .expect("reload after fix");
    let hot = manager
        .list_plugins()
        .into_iter()
        .find(|p| p.name == "Hot")
        .expect("plugin");
    assert_eq!(hot.failure_reason, None);
    assert_ne!(hot.state, zerodeck_plugin_host::PluginState::Failed);
    manager.shutdown_all().await;
}

#[cfg(unix)]
#[tokio::test]
async fn plugin_manager_upgrade_failure_preserves_running_worker_and_disambiguates_dirs() {
    let dir = TestDir::new("mgr-upgrade-rollback");
    let paths = manager_test_paths(&dir.0);
    let occupied = paths.plugins_dir().join("DemoPlug");
    fs::create_dir_all(&occupied).expect("create occupied dir");
    fs::write(occupied.join("unrelated.txt"), b"keep").expect("write unrelated file");

    let v1_zip = build_plugin_zip(
        "",
        "DemoPlug",
        "1.0.0",
        b"class Plugin:\n    async def ver(self):\n        return 'v1'\n",
        &[],
    );
    let v1_sha = hex::encode(Sha256::digest(&v1_zip));
    let v2_zip_bad = zip_with_files(&[
            ("plugin.json", br#"{"name":"DemoPlug","author":"Alice","api_version":1}"#),
            (
                "package.json",
                br#"{"version":"2.0.0","remote_binary":[{"name":"tool","url":"https://example.com/tool","sha256hash":"deadbeef"}]}"#,
            ),
            ("main.py", b"class Plugin:\n    async def ver(self):\n        return 'v2'\n"),
        ]);
    let v2_zip_good = build_plugin_zip(
        "wrapper-v2",
        "DemoPlug",
        "2.0.0",
        b"# upgraded to v2\nclass Plugin:\n    async def ver(self):\n        return 'v2'\n",
        &[],
    );
    let v2_good_sha = hex::encode(Sha256::digest(&v2_zip_good));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let port = listener.local_addr().expect("addr").port();
    let server = spawn_installer_mock_on(
        listener,
        vec![
            ("/v1.zip".to_owned(), v1_zip),
            ("/v2-good.zip".to_owned(), v2_zip_good),
        ],
    );
    let client = crate::OutboundHttpPolicy::for_loopback_ports_for_testing(vec![port])
        .build_checked_client()
        .expect("client");
    let manager = test_manager_with_http(&paths, client);

    let first = install_remote_zip(
        &manager,
        format!("http://127.0.0.1:{port}/v1.zip"),
        v1_sha,
        Some("DemoPlug"),
        Some("1.0.0"),
    )
    .await;
    assert_eq!(first.directory, "DemoPlug-2");
    assert!(!first.replaced_existing);
    assert_eq!(
        fs::read_to_string(occupied.join("unrelated.txt")).expect("keep"),
        "keep"
    );
    assert_eq!(
        call_plugin_ok(&manager, "DemoPlug", "ver").await,
        serde_json::json!("v1")
    );

    let bad_local = paths.user_home().join("v2-bad.zip");
    fs::write(&bad_local, &v2_zip_bad).expect("write v2-bad");
    let err_upgrade = manager
        .install_plugin(crate::InstallPluginRequest {
            source: crate::InstallPluginSource::LocalZip {
                path: bad_local,
                sha256: Some("0".repeat(64)),
                user_confirmed: false,
            },
            directory_hint: None,
            expected_name: None,
            expected_version: None,
        })
        .await
        .expect_err("bad upgrade hash must fail");
    assert!(matches!(err_upgrade, CoreError::Sha256Mismatch { .. }));
    assert_eq!(
        call_plugin_ok(&manager, "DemoPlug", "ver").await,
        serde_json::json!("v1")
    );

    let second = install_remote_zip(
        &manager,
        format!("http://127.0.0.1:{port}/v2-good.zip"),
        v2_good_sha,
        Some("DemoPlug"),
        Some("2.0.0"),
    )
    .await;
    assert_eq!(second.directory, "DemoPlug-2");
    assert!(second.replaced_existing);
    assert_eq!(
        call_plugin_ok(&manager, "DemoPlug", "ver").await,
        serde_json::json!("v2")
    );

    manager.shutdown_all().await;
    server.abort();
}

#[cfg(unix)]
#[tokio::test]
async fn plugin_manager_migrates_legacy_disabled_plugins_and_watcher_refreshes_manifest() {
    use std::sync::Arc;
    use std::time::Duration;

    let dir = TestDir::new("mgr-legacy-manifest");
    let paths = manager_test_paths(&dir.0);
    fs::create_dir_all(paths.settings_dir()).expect("create settings dir");
    fs::write(
        paths.settings_dir().join("loader.json"),
        r#"{"disabled_plugins":["LegacyTarget"]}"#,
    )
    .expect("seed legacy disabled_plugins");

    let p = paths.plugins_dir().join("legacy_target");
    write_watcher_plugin(&p, "LegacyTarget", &["debug"], "ok");

    let manager = test_manager_open(&paths);
    manager.start_all().await.expect("start_all");

    let fp = manager
        .list_files(crate::FilePickerQuery::default())
        .await
        .expect("list_files");
    assert_eq!(
        fp.realpath,
        fs::canonicalize(paths.user_home())
            .expect("canon home")
            .display()
            .to_string()
    );

    let watcher = crate::start_debug_watcher(Arc::clone(&manager)).expect("start watcher");
    let mut events = manager.subscribe_loader_events();

    manager
        .enable_plugin("LegacyTarget")
        .await
        .expect("enable LegacyTarget");
    assert_eq!(
        manager.get_setting(Some("disabled_plugins")),
        serde_json::Value::Null
    );
    assert_eq!(
        manager.get_setting(Some("disabledPlugins")),
        serde_json::json!([])
    );
    assert_eq!(
        manager
            .call_plugin_method("LegacyTarget", "version_tag", serde_json::json!([]), false)
            .await
            .expect("call enabled plugin"),
        serde_json::json!("ok")
    );

    fs::write(
        p.join("package.json"),
        r#"{"version":"1.0.1","type":"module"}"#,
    )
    .expect("bump package.json version");
    wait_for_loader_event(&mut events, Duration::from_secs(5), |ev| {
        matches!(
            ev,
            crate::LoaderEvent::PluginsChanged(list)
            if list.iter().any(|item| {
                item.name == "LegacyTarget" && item.version.as_deref() == Some("1.0.1")
            })
        )
    })
    .await;
    wait_for_loader_event(&mut events, Duration::from_secs(5), |ev| {
        matches!(
            ev,
            crate::LoaderEvent::PluginReloaded { plugin, directory }
            if plugin == "LegacyTarget" && directory == "legacy_target"
        )
    })
    .await;

    drop(watcher);
    manager.shutdown_all().await;
}
