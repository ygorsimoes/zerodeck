use std::collections::BTreeMap;
use std::path::PathBuf;

use zerodeck_platform::{PlatformPaths, PrivilegeTarget};
use zerodeck_protocol::PluginManifest;

/// Specification describing how to launch an out-of-process plugin worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSpawnSpec {
    /// Opaque identity of the plugin code being started.
    pub generation: String,
    pub directory: String,
    pub manifest: PluginManifest,
    pub version: Option<String>,
    pub host_user: String,
    pub host_user_home: PathBuf,
    pub loader_version: String,
    pub entrypoint: PathBuf,
}

impl PluginSpawnSpec {
    /// Privilege level requested by this plugin's manifest.
    #[must_use]
    pub fn privilege_target(&self) -> PrivilegeTarget {
        PrivilegeTarget::for_manifest(&self.manifest)
    }
}

/// Builds the full `DECKY_*` environment map injected into a plugin backend worker.
#[must_use]
pub fn build_plugin_env(spec: &PluginSpawnSpec, paths: &PlatformPaths) -> BTreeMap<String, String> {
    let is_root = spec.privilege_target() == PrivilegeTarget::Root;
    let effective_user = if is_root {
        "root".to_owned()
    } else {
        spec.host_user.clone()
    };
    let effective_home = if is_root {
        PathBuf::from("/root")
    } else {
        spec.host_user_home.clone()
    };

    let mut env = BTreeMap::new();
    env.insert("HOME".to_owned(), effective_home.display().to_string());
    env.insert("USER".to_owned(), effective_user);
    env.insert("DECKY_VERSION".to_owned(), spec.loader_version.clone());
    env.insert("DECKY_USER".to_owned(), spec.host_user.clone());
    env.insert(
        "DECKY_USER_HOME".to_owned(),
        spec.host_user_home.display().to_string(),
    );
    env.insert(
        "DECKY_HOME".to_owned(),
        paths.unprivileged_root().display().to_string(),
    );
    env.insert(
        "DECKY_PLUGIN_SETTINGS_DIR".to_owned(),
        paths
            .settings_dir()
            .join(&spec.directory)
            .display()
            .to_string(),
    );
    env.insert(
        "DECKY_PLUGIN_RUNTIME_DIR".to_owned(),
        paths.data_dir().join(&spec.directory).display().to_string(),
    );
    env.insert(
        "DECKY_PLUGIN_LOG_DIR".to_owned(),
        paths.logs_dir().join(&spec.directory).display().to_string(),
    );
    env.insert(
        "DECKY_PLUGIN_DIR".to_owned(),
        paths
            .plugins_dir()
            .join(&spec.directory)
            .display()
            .to_string(),
    );
    env.insert("DECKY_PLUGIN_NAME".to_owned(), spec.manifest.name.clone());
    if let Some(version) = &spec.version {
        env.insert("DECKY_PLUGIN_VERSION".to_owned(), version.clone());
    }
    env.insert(
        "DECKY_PLUGIN_AUTHOR".to_owned(),
        spec.manifest.author.clone(),
    );
    env
}
