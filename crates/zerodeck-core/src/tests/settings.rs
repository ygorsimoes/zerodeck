//! `SettingsManager` and `loader.json` persistence through the manager.

use super::*;

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

    let reopened_after_remove = SettingsManager::open(path).expect("reopen settings after remove");
    assert_eq!(reopened_after_remove.get_path("scalar"), None);
    assert_eq!(
        reopened_after_remove.get_path("developer.allow_unverified_installs"),
        Some(serde_json::json!(true))
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_setting_writes_leave_disk_and_memory_consistent() {
    let dir = TestDir::new("settings-concurrent");
    let paths = manager_test_paths(&dir.0);
    let manager = test_manager_open(&paths);

    let mut writes = tokio::task::JoinSet::new();
    for i in 0..32 {
        let manager = std::sync::Arc::clone(&manager);
        writes.spawn(async move {
            manager
                .set_setting("k", serde_json::json!(i))
                .await
                .expect("set_setting");
        });
    }
    while let Some(joined) = writes.join_next().await {
        joined.expect("write task");
    }

    let on_disk: serde_json::Value = serde_json::from_slice(
        &fs::read(paths.settings_dir().join("loader.json")).expect("read loader.json"),
    )
    .expect("parse loader.json");
    assert_eq!(on_disk["k"], manager.get_setting(Some("k")));
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reads_are_not_blocked_by_settings_writes() {
    use std::time::{Duration, Instant};

    let dir = TestDir::new("settings-reads");
    let paths = manager_test_paths(&dir.0);
    let manager = test_manager_open(&paths);

    let mut writes = tokio::task::JoinSet::new();
    for i in 0..20 {
        let manager = std::sync::Arc::clone(&manager);
        writes.spawn(async move {
            manager
                .set_setting(&format!("key{i}"), serde_json::json!(i))
                .await
                .expect("set_setting");
        });
    }
    let reader = {
        let manager = std::sync::Arc::clone(&manager);
        tokio::spawn(async move {
            let started = Instant::now();
            for _ in 0..100 {
                let _ = manager.list_plugins();
                tokio::task::yield_now().await;
            }
            started.elapsed()
        })
    };
    let elapsed = reader.await.expect("reader task");
    while let Some(joined) = writes.join_next().await {
        joined.expect("write task");
    }
    assert!(
        elapsed < Duration::from_secs(1),
        "100 reads took {elapsed:?} during writes"
    );
}
