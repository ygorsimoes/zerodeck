//! Out-of-process plugin runtime abstraction, `socketpair` IPC, and supervisor for `ZeroDeck`.

mod env;
mod error;
mod runtime;
mod supervisor;

pub use env::{PluginSpawnSpec, build_plugin_env};
pub use error::HostError;
pub use runtime::{BoxHostFuture, PluginRuntime, PythonHostConfig};
pub use supervisor::{ManagedPlugin, PluginEvent, PluginState, PluginSupervisor, SupervisorConfig};

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use serde_json::json;
    use uuid::Uuid;
    use zerodeck_platform::PlatformPaths;
    use zerodeck_protocol::PluginManifest;

    use crate::{
        HostError, PluginRuntime, PluginSpawnSpec, PluginState, PluginSupervisor, PythonHostConfig,
        SupervisorConfig, build_plugin_env,
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

    fn python_host_dir() -> String {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../hosts/python")
            .canonicalize()
            .expect("resolve hosts/python")
            .display()
            .to_string()
    }

    fn test_paths(root: &Path) -> PlatformPaths {
        let root_str = root.display().to_string();
        let home_str = root.join("home").display().to_string();
        fs::create_dir_all(&home_str).expect("create home dir");
        let lookup = HashMap::from([
            ("UNPRIVILEGED_PATH", root_str.as_str()),
            ("PRIVILEGED_PATH", root_str.as_str()),
            ("DECKY_USER_HOME", home_str.as_str()),
            ("HOME", home_str.as_str()),
        ]);
        PlatformPaths::from_lookup(|k| lookup.get(k).map(|v| (*v).to_owned()))
    }

    fn fast_supervisor_config() -> SupervisorConfig {
        SupervisorConfig {
            python_host: PythonHostConfig {
                python_bin: "python3".to_owned(),
                host_module_path: Some(python_host_dir()),
            },
            startup_timeout: Duration::from_secs(5),
            shutdown_timeout: Duration::from_millis(250),
            crash_window: Duration::from_secs(30),
            max_crashes_in_window: 3,
            stability_window: Duration::from_secs(30),
            initial_backoff: Duration::from_millis(30),
            max_backoff: Duration::from_millis(120),
        }
    }

    fn write_plugin(
        paths: &PlatformPaths,
        directory: &str,
        manifest: &PluginManifest,
        main_py: &str,
    ) -> PluginSpawnSpec {
        let plugin_dir = paths.plugins_dir().join(directory);
        fs::create_dir_all(&plugin_dir).expect("create plugin dir");
        let entrypoint = plugin_dir.join("main.py");
        fs::write(&entrypoint, main_py).expect("write main.py");
        PluginSpawnSpec {
            directory: directory.to_owned(),
            manifest: manifest.clone(),
            version: Some("1.0.0".to_owned()),
            host_user: "deck".to_owned(),
            loader_version: "0.1.0".to_owned(),
            entrypoint,
        }
    }

    #[test]
    fn build_plugin_env_sets_all_decky_vars_and_respects_root_flag() {
        let lookup = HashMap::from([
            ("UNPRIVILEGED_PATH", "/home/deck/homebrew"),
            ("PRIVILEGED_PATH", "/home/deck/homebrew"),
            ("DECKY_USER_HOME", "/home/deck"),
        ]);
        let paths = PlatformPaths::from_lookup(|k| lookup.get(k).map(|v| (*v).to_owned()));

        let spec = PluginSpawnSpec {
            directory: "powertools".to_owned(),
            manifest: PluginManifest {
                name: "PowerTools".to_owned(),
                author: "NGnius".to_owned(),
                flags: vec!["root".to_owned()],
                api_version: 1,
            },
            version: Some("1.5.0".to_owned()),
            host_user: "deck".to_owned(),
            loader_version: "0.1.0".to_owned(),
            entrypoint: PathBuf::from("/home/deck/homebrew/plugins/powertools/main.py"),
        };

        let env = build_plugin_env(&spec, &paths);
        assert_eq!(env.get("USER").map(String::as_str), Some("root"));
        assert_eq!(env.get("HOME").map(String::as_str), Some("/root"));
        assert_eq!(env.get("DECKY_USER").map(String::as_str), Some("deck"));
        assert_eq!(
            env.get("DECKY_HOME").map(String::as_str),
            Some("/home/deck/homebrew")
        );
        assert_eq!(
            env.get("DECKY_PLUGIN_SETTINGS_DIR").map(String::as_str),
            Some("/home/deck/homebrew/settings/powertools")
        );
        assert_eq!(
            env.get("DECKY_PLUGIN_RUNTIME_DIR").map(String::as_str),
            Some("/home/deck/homebrew/data/powertools")
        );
        assert_eq!(
            env.get("DECKY_PLUGIN_LOG_DIR").map(String::as_str),
            Some("/home/deck/homebrew/logs/powertools")
        );
        assert_eq!(
            env.get("DECKY_PLUGIN_DIR").map(String::as_str),
            Some("/home/deck/homebrew/plugins/powertools")
        );
        assert_eq!(
            env.get("DECKY_PLUGIN_NAME").map(String::as_str),
            Some("PowerTools")
        );
        assert_eq!(
            env.get("DECKY_PLUGIN_VERSION").map(String::as_str),
            Some("1.5.0")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn supervisor_spawns_python_worker_over_socketpair_and_calls_v0_and_v1_methods() {
        let dir = TestDir::new("sup-v0-v1");
        let paths = test_paths(&dir.0);
        let supervisor = PluginSupervisor::new(paths.clone(), fast_supervisor_config())
            .expect("init supervisor");
        let mut events = supervisor.subscribe_events();

        let v0_spec = write_plugin(
            &paths,
            "legacy_plug",
            &PluginManifest {
                name: "LegacyPlug".to_owned(),
                author: "Alice".to_owned(),
                flags: vec![],
                api_version: 0,
            },
            r#"import os, decky_plugin
class Plugin:
    @classmethod
    async def cls_echo(cls, msg="hi"):
        return f"cls:{msg}:{decky_plugin.DECKY_PLUGIN_NAME}:{os.path.basename(os.getcwd())}"
    @staticmethod
    async def static_echo(msg="hi"):
        return f"static:{msg}"
    async def plain_echo(self, msg="hi"):
        return f"plain:{msg}"
"#,
        );

        let v1_spec = write_plugin(
            &paths,
            "modern_plug",
            &PluginManifest {
                name: "ModernPlug".to_owned(),
                author: "Bob".to_owned(),
                flags: vec![],
                api_version: 1,
            },
            r#"import decky, helper_mod
from settings import SettingsManager
from helpers import get_ssl_context

class Plugin:
    async def _main(self):
        self.cfg = SettingsManager("settings", default_settings={"count": 1})

    async def bump_and_emit(self, delta):
        ctx = get_ssl_context()
        new_val = self.cfg.getSetting("count", 0) + delta
        self.cfg.setSetting("count", new_val)
        await decky.emit("bumped", new_val, helper_mod.TAG, hasattr(ctx, "wrap_socket"))
        return new_val

    async def explode(self):
        raise ValueError("bad value")
"#,
        );
        let py_mods = paths.plugins_dir().join("modern_plug").join("py_modules");
        fs::create_dir_all(&py_mods).expect("create py_modules");
        fs::write(py_mods.join("helper_mod.py"), "TAG = 'vendored'\n").expect("write helper_mod");

        let v0 = supervisor.spawn_plugin(v0_spec).await.expect("spawn v0");
        let v1 = supervisor.spawn_plugin(v1_spec).await.expect("spawn v1");

        assert_eq!(
            v0.call_method("cls_echo", json!({"msg": "a"}), true)
                .await
                .expect("cls_echo"),
            json!("cls:a:LegacyPlug:legacy_plug")
        );
        assert_eq!(
            v0.call_method("static_echo", json!({"msg": "b"}), true)
                .await
                .expect("static_echo"),
            json!("static:b")
        );
        assert_eq!(
            v0.call_method("plain_echo", json!({"msg": "c"}), true)
                .await
                .expect("plain_echo"),
            json!("plain:c")
        );

        let bumped = v1
            .call_method("bump_and_emit", json!([4]), false)
            .await
            .expect("bump_and_emit");
        assert_eq!(bumped, json!(5));

        let ev = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("receive event")
            .expect("broadcast ok");
        assert_eq!(ev.plugin, "ModernPlug");
        assert_eq!(ev.event, "bumped");
        assert_eq!(ev.args, vec![json!(5), json!("vendored"), json!(true)]);

        let err = v1
            .call_method("explode", json!([]), false)
            .await
            .expect_err("explode must fail");
        assert!(matches!(
            err,
            HostError::PluginCallFailed { ref name, ref message, traceback: Some(_) }
                if name == "ValueError" && message == "bad value"
        ));

        supervisor.shutdown_all().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn supervisor_cancels_in_flight_calls_with_worker_stopped_and_restarts_with_backoff() {
        let dir = TestDir::new("sup-crash-restart");
        let paths = test_paths(&dir.0);
        let supervisor = PluginSupervisor::new(paths.clone(), fast_supervisor_config())
            .expect("init supervisor");

        let spec = write_plugin(
            &paths,
            "crashy",
            &PluginManifest {
                name: "Crashy".to_owned(),
                author: "Alice".to_owned(),
                flags: vec![],
                api_version: 1,
            },
            r#"import os
class Plugin:
    async def crash_now(self):
        os._exit(1)

    async def ping(self):
        return "pong"
"#,
        );

        let handle = supervisor.spawn_plugin(spec).await.expect("spawn crashy");
        let err = handle
            .call_method("crash_now", json!([]), false)
            .await
            .expect_err("in-flight call must fail when worker dies");
        assert!(matches!(err, HostError::WorkerStopped));

        // Wait for backoff respawn and verify the plugin recovers cleanly.
        let mut recovered = false;
        for _ in 0..40 {
            if supervisor.plugin_state("Crashy") == Some(PluginState::Running)
                && handle.call_method("ping", json!([]), false).await.ok() == Some(json!("pong"))
            {
                recovered = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(recovered, "worker did not recover after crash backoff");
        supervisor.shutdown_all().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn supervisor_trips_crash_loop_to_failed_after_repeated_crashes() {
        let dir = TestDir::new("sup-crash-loop");
        let paths = test_paths(&dir.0);
        let supervisor = PluginSupervisor::new(paths.clone(), fast_supervisor_config())
            .expect("init supervisor");

        let spec = write_plugin(
            &paths,
            "broken_boot",
            &PluginManifest {
                name: "BrokenBoot".to_owned(),
                author: "Alice".to_owned(),
                flags: vec![],
                api_version: 1,
            },
            "raise RuntimeError('import failure')\n",
        );

        let _ = supervisor.spawn_plugin(spec).await;

        let mut tripped = false;
        for _ in 0..60 {
            if supervisor.plugin_state("BrokenBoot") == Some(PluginState::Failed) {
                tripped = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(tripped, "crash loop never tripped to PluginState::Failed");
        supervisor.shutdown_all().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn supervisor_shutdown_runs_uninstall_lifecycle_and_kills_hung_worker() {
        let dir = TestDir::new("sup-shutdown");
        let paths = test_paths(&dir.0);
        let supervisor = PluginSupervisor::new(paths.clone(), fast_supervisor_config())
            .expect("init supervisor");

        let flag_path = dir.0.join("uninstalled.flag");
        let uninstall_spec = write_plugin(
            &paths,
            "uninstall_plug",
            &PluginManifest {
                name: "UninstallPlug".to_owned(),
                author: "Alice".to_owned(),
                flags: vec![],
                api_version: 1,
            },
            &format!(
                r#"from pathlib import Path
class Plugin:
    async def _uninstall(self):
        Path({:?}).write_text("bye", encoding="utf-8")
    async def ping(self):
        return "ok"
"#,
                flag_path.display().to_string()
            ),
        );

        let hung_spec = write_plugin(
            &paths,
            "hung_plug",
            &PluginManifest {
                name: "HungPlug".to_owned(),
                author: "Bob".to_owned(),
                flags: vec![],
                api_version: 1,
            },
            r#"import signal, time
signal.signal(signal.SIGTERM, signal.SIG_IGN)
class Plugin:
    async def _unload(self):
        time.sleep(10)
    async def ping(self):
        return "ok"
"#,
        );

        let u = supervisor
            .spawn_plugin(uninstall_spec)
            .await
            .expect("spawn uninstall");
        assert_eq!(
            u.call_method("ping", json!([]), false).await.expect("ping"),
            json!("ok")
        );
        u.shutdown(true).await.expect("uninstall shutdown");
        assert_eq!(
            fs::read_to_string(&flag_path).expect("read uninstall flag"),
            "bye"
        );

        let h = supervisor
            .spawn_plugin(hung_spec)
            .await
            .expect("spawn hung");
        assert_eq!(
            h.call_method("ping", json!([]), false).await.expect("ping"),
            json!("ok")
        );
        let start = std::time::Instant::now();
        h.shutdown(false).await.expect("forced kill shutdown");
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "hung worker shutdown took too long: {:?}",
            start.elapsed()
        );

        supervisor.shutdown_all().await;
    }
}
