use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use serde_json::Value;
use tokio::sync::broadcast;
use zerodeck_platform::PlatformPaths;
use zerodeck_plugin_host::{
    PluginEvent, PluginSpawnSpec, PluginState, PluginSupervisor, SupervisorConfig,
};
use zerodeck_protocol::PluginLoadType;

use crate::{CoreError, DiscoveredPlugin, SettingsManager, discover_plugins};

/// Public snapshot of a discovered plugin and its runtime state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSummary {
    pub name: String,
    pub directory: String,
    pub version: Option<String>,
    pub load_type: PluginLoadType,
    pub disabled: bool,
    pub passive: bool,
    pub state: PluginState,
}

/// Coordinates discovered plugins, `loader.json` settings, and worker supervision.
#[derive(Debug)]
pub struct PluginManager {
    paths: PlatformPaths,
    settings: Mutex<SettingsManager>,
    plugins: Mutex<Vec<DiscoveredPlugin>>,
    supervisor: PluginSupervisor,
    loader_version: String,
}

impl PluginManager {
    /// Opens `settings/loader.json`, discovers plugins under `paths.plugins_dir()`, and initializes
    /// the [`PluginSupervisor`].
    pub fn open(
        paths: PlatformPaths,
        config: SupervisorConfig,
        loader_version: impl Into<String>,
    ) -> Result<Self, CoreError> {
        let settings = SettingsManager::open(paths.settings_dir().join("loader.json"))?;
        let plugins = discover_plugins(&paths.plugins_dir())?;
        Self::from_parts(paths, settings, plugins, config, loader_version)
    }

    /// Constructs a [`PluginManager`] from already-loaded settings and discovered plugins.
    pub fn from_parts(
        paths: PlatformPaths,
        settings: SettingsManager,
        plugins: Vec<DiscoveredPlugin>,
        config: SupervisorConfig,
        loader_version: impl Into<String>,
    ) -> Result<Self, CoreError> {
        let supervisor = PluginSupervisor::new(paths.clone(), config)?;
        Ok(Self {
            paths,
            settings: Mutex::new(settings),
            plugins: Mutex::new(plugins),
            supervisor,
            loader_version: loader_version.into(),
        })
    }

    /// Starts workers for all enabled, non-passive plugins in configured order.
    pub async fn start_all(&self) -> Result<(), CoreError> {
        let disabled = self.disabled_set();
        let ordered = self.ordered_plugins();
        for plugin in ordered {
            if plugin.passive || disabled.contains(&plugin.manifest.name) {
                continue;
            }
            let spec = self.spawn_spec_for(&plugin);
            self.supervisor.spawn_plugin(spec).await?;
        }
        Ok(())
    }

    /// Invokes a backend RPC method on an enabled, active plugin.
    pub async fn call_plugin_method(
        &self,
        plugin_name: &str,
        method: &str,
        args: Value,
        legacy: bool,
    ) -> Result<Value, CoreError> {
        let plugin = self.find_plugin(plugin_name).ok_or_else(|| {
            CoreError::Host(zerodeck_plugin_host::HostError::PluginNotFound(
                plugin_name.to_owned(),
            ))
        })?;

        if self.disabled_set().contains(&plugin.manifest.name) {
            return Err(CoreError::PluginDisabled(plugin.manifest.name));
        }
        if plugin.passive {
            return Err(CoreError::Host(
                zerodeck_plugin_host::HostError::PassivePlugin(plugin.manifest.name),
            ));
        }

        let res = self
            .supervisor
            .call_method(plugin_name, method, args, legacy)
            .await?;
        Ok(res)
    }

    /// Restarts an enabled, active plugin worker cleanly.
    pub async fn reload_plugin(&self, plugin_name: &str) -> Result<(), CoreError> {
        let plugin = self.find_plugin(plugin_name).ok_or_else(|| {
            CoreError::Host(zerodeck_plugin_host::HostError::PluginNotFound(
                plugin_name.to_owned(),
            ))
        })?;

        if self.disabled_set().contains(&plugin.manifest.name) {
            return Err(CoreError::PluginDisabled(plugin.manifest.name));
        }
        if plugin.passive {
            return Err(CoreError::Host(
                zerodeck_plugin_host::HostError::PassivePlugin(plugin.manifest.name),
            ));
        }

        if self.supervisor.plugin_state(plugin_name).is_some() {
            self.supervisor.reload_plugin(plugin_name).await?;
        } else {
            self.supervisor
                .spawn_plugin(self.spawn_spec_for(&plugin))
                .await?;
        }
        Ok(())
    }

    /// Returns plugins ordered by `loader.json`'s `pluginOrder` with unlisted plugins appended
    /// in deterministic directory order.
    #[must_use]
    pub fn list_plugins(&self) -> Vec<PluginSummary> {
        let disabled = self.disabled_set();
        self.ordered_plugins()
            .into_iter()
            .map(|plugin| {
                let is_disabled = disabled.contains(&plugin.manifest.name);
                let state = self
                    .supervisor
                    .plugin_state(&plugin.manifest.name)
                    .unwrap_or(PluginState::Stopped);
                PluginSummary {
                    name: plugin.manifest.name,
                    directory: plugin.directory,
                    version: plugin.package.and_then(|pkg| pkg.version),
                    load_type: plugin.load_type,
                    disabled: is_disabled,
                    passive: plugin.passive,
                    state,
                }
            })
            .collect()
    }

    /// Returns the declared `api_version` for a discovered plugin.
    #[must_use]
    pub fn plugin_api_version(&self, plugin_name: &str) -> Option<u32> {
        self.find_plugin(plugin_name)
            .map(|plugin| plugin.manifest.api_version)
    }

    /// Subscribes to asynchronous [`PluginEvent`] broadcasts emitted by all workers.
    #[must_use]
    pub fn subscribe_events(&self) -> broadcast::Receiver<PluginEvent> {
        self.supervisor.subscribe_events()
    }

    /// Shuts down all running plugin workers gracefully.
    pub async fn shutdown_all(&self) {
        self.supervisor.shutdown_all().await;
    }

    fn spawn_spec_for(&self, plugin: &DiscoveredPlugin) -> PluginSpawnSpec {
        PluginSpawnSpec {
            directory: plugin.directory.clone(),
            manifest: plugin.manifest.clone(),
            version: plugin.package.as_ref().and_then(|pkg| pkg.version.clone()),
            host_user: self.supervisor.host_username().to_owned(),
            host_user_home: self.supervisor.host_home_dir().to_path_buf(),
            loader_version: self.loader_version.clone(),
            entrypoint: self
                .paths
                .plugins_dir()
                .join(&plugin.directory)
                .join("main.py"),
        }
    }

    fn find_plugin(&self, plugin_name: &str) -> Option<DiscoveredPlugin> {
        let guard = lock_unpoisoned(&self.plugins);
        guard
            .iter()
            .find(|plugin| plugin.manifest.name == plugin_name)
            .cloned()
    }

    fn disabled_set(&self) -> HashSet<String> {
        let guard = lock_unpoisoned(&self.settings);
        let list = guard
            .get::<Vec<String>>("disabledPlugins")
            .or_else(|| guard.get::<Vec<String>>("disabled_plugins"))
            .unwrap_or_default();
        list.into_iter().collect()
    }

    fn ordered_plugins(&self) -> Vec<DiscoveredPlugin> {
        let order = {
            let guard = lock_unpoisoned(&self.settings);
            guard.get::<Vec<String>>("pluginOrder").unwrap_or_default()
        };
        let plugins = lock_unpoisoned(&self.plugins).clone();

        if order.is_empty() {
            return plugins;
        }

        let mut by_name: HashMap<String, DiscoveredPlugin> = plugins
            .into_iter()
            .map(|p| (p.manifest.name.clone(), p))
            .collect();
        let mut sorted = Vec::with_capacity(by_name.len());

        for name in order {
            if let Some(plugin) = by_name.remove(&name) {
                sorted.push(plugin);
            }
        }

        let mut remaining: Vec<DiscoveredPlugin> = by_name.into_values().collect();
        remaining.sort_by(|a, b| a.directory.cmp(&b.directory));
        sorted.extend(remaining);
        sorted
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
