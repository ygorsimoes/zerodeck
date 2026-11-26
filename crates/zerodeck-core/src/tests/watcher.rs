//! Debug hot-reload watcher.

use super::*;

#[cfg(unix)]
#[tokio::test]
async fn debug_watcher_reloads_debug_plugin_on_py_edit_and_emits_frontend_event_on_dist_edit() {
    use std::sync::Arc;
    use std::time::Duration;

    let dir = TestDir::new("watcher-hot-reload");
    let paths = manager_test_paths(&dir.0);
    let debug_dir = paths.plugins_dir().join("dbg_plug");
    let prod_dir = paths.plugins_dir().join("prod_plug");
    write_watcher_plugin(&debug_dir, "DebugPlug", &["debug"], "v1");
    write_watcher_plugin(&prod_dir, "ProdPlug", &[], "stable");

    let manager = test_manager_open(&paths);
    manager.start_all().await.expect("start_all");
    assert_eq!(
        manager
            .call_plugin_method("DebugPlug", "version_tag", serde_json::json!([]), false)
            .await
            .expect("call initial v1"),
        serde_json::json!("v1")
    );

    let mut events = manager.subscribe_loader_events();
    let watcher = crate::start_debug_watcher(Arc::clone(&manager)).expect("start watcher");

    fs::write(
        prod_dir.join("main.py"),
        "class Plugin:\n    async def version_tag(self):\n        return 'edited'\n",
    )
    .expect("edit prod main.py");
    fs::write(prod_dir.join("dist/index.js"), b"export const v = 2;").expect("edit prod dist");
    fs::create_dir_all(debug_dir.join("py_modules/__pycache__")).expect("create pycache");
    fs::write(
        debug_dir.join("py_modules/__pycache__/mod.cpython-312.pyc"),
        b"pyc",
    )
    .expect("write pyc");
    fs::write(debug_dir.join(".tmp-editor-swap"), b"swap").expect("write dotfile");

    let unexpected = tokio::time::timeout(Duration::from_millis(900), events.recv()).await;
    assert!(
        unexpected.is_err(),
        "unexpected reload event for non-debug or ignored files: {unexpected:?}"
    );

    fs::write(
            debug_dir.join("main.py"),
            "# reloaded debug plugin\nclass Plugin:\n    async def version_tag(self):\n        return 'v2'\n",
        )
        .expect("edit dbg main.py");
    wait_for_loader_event(&mut events, Duration::from_secs(5), |ev| {
        matches!(
            ev,
            crate::LoaderEvent::PluginReloaded { plugin, directory }
            if plugin == "DebugPlug" && directory == "dbg_plug"
        )
    })
    .await;

    assert_eq!(
        manager
            .call_plugin_method("DebugPlug", "version_tag", serde_json::json!([]), false)
            .await
            .expect("call v2 after watcher reload"),
        serde_json::json!("v2")
    );

    fs::write(debug_dir.join("dist/index.js"), b"export const v = 2;").expect("edit dbg dist");
    wait_for_loader_event(&mut events, Duration::from_secs(5), |ev| {
        matches!(
            ev,
            crate::LoaderEvent::FrontendReloaded { plugin, directory }
            if plugin == "DebugPlug" && directory == "dbg_plug"
        )
    })
    .await;

    watcher.stop().await;
    manager.shutdown_all().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn debug_watcher_inotify_replaces_moved_aside_dist_directory() {
    use std::sync::Arc;
    use std::time::Duration;

    let dir = TestDir::new("watcher-move-self");
    let paths = manager_test_paths(&dir.0);
    let plugin_dir = paths.plugins_dir().join("move_plug");
    write_watcher_plugin(&plugin_dir, "MovePlug", &["debug"], "v1");
    fs::create_dir_all(plugin_dir.join("dist/sub")).expect("create dist/sub");
    fs::write(plugin_dir.join("dist/sub/chunk.js"), b"export const c = 1;")
        .expect("write dist/sub/chunk.js");

    let manager = test_manager_open(&paths);
    manager.start_all().await.expect("start_all");

    let mut events = manager.subscribe_loader_events();
    let watcher = crate::start_debug_watcher(Arc::clone(&manager)).expect("start watcher");

    // Move `dist` aside (`MOVE_SELF`) and create a replacement `dist/sub` directory at the
    // original path.
    let dist_dir = plugin_dir.join("dist");
    let bak_dir = plugin_dir.join("dist.bak");
    fs::rename(&dist_dir, &bak_dir).expect("move dist to dist.bak");
    fs::create_dir_all(dist_dir.join("sub")).expect("recreate dist/sub");
    fs::write(dist_dir.join("index.js"), b"export const v = 2;").expect("write new dist");
    fs::write(dist_dir.join("sub/chunk.js"), b"export const c = 2;").expect("write new chunk");

    wait_for_loader_event(&mut events, Duration::from_secs(2), |ev| {
        matches!(
            ev,
            crate::LoaderEvent::FrontendReloaded { plugin, directory }
            if plugin == "MovePlug" && directory == "move_plug"
        )
    })
    .await;
    // Let any remaining bursts settle before testing moved-away vs replacement watches.
    tokio::time::sleep(Duration::from_millis(400)).await;
    while events.try_recv().is_ok() {}

    // Edits in the moved-away `dist.bak` (root or nested `sub/`) must NOT trigger reloads.
    fs::write(bak_dir.join("index.js"), b"export const v = 99;").expect("edit bak index");
    fs::write(bak_dir.join("sub/chunk.js"), b"export const c = 99;").expect("edit bak chunk");
    let stale_ev = tokio::time::timeout(Duration::from_millis(800), events.recv()).await;
    assert!(
        stale_ev.is_err(),
        "moved-away dist.bak edit must not emit FrontendReloaded: {stale_ev:?}"
    );

    // Edits in the new `dist` (and `dist/sub`) must fire promptly via fresh inotify watches
    // (< 1.1s, strictly faster than the 2s Linux fallback mtime sweep).
    fs::write(dist_dir.join("sub/chunk.js"), b"export const c = 3;")
        .expect("edit new dist/sub/chunk.js");
    wait_for_loader_event(&mut events, Duration::from_millis(1100), |ev| {
        matches!(
            ev,
            crate::LoaderEvent::FrontendReloaded { plugin, directory }
            if plugin == "MovePlug" && directory == "move_plug"
        )
    })
    .await;

    watcher.stop().await;
    manager.shutdown_all().await;
}
