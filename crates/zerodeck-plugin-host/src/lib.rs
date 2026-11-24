//! Out-of-process plugin runtime abstraction and Python host launcher for `ZeroDeck`.

mod env;
mod error;
mod runtime;

pub use env::{PluginSpawnSpec, build_plugin_env};
pub use error::HostError;
pub use runtime::{BoxHostFuture, PluginRuntime, PythonHostConfig};

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use zerodeck_platform::PlatformPaths;
    use zerodeck_protocol::PluginManifest;

    use crate::{PluginSpawnSpec, build_plugin_env};

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
}
