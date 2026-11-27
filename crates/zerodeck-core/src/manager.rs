use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use serde_json::{Map, Value};
use tokio::sync::broadcast;
use tracing::warn;
use zerodeck_platform::{PlatformError, PlatformPaths, SafePath, lock_unpoisoned};
use zerodeck_plugin_host::{
    HostError, PluginEvent, PluginSpawnSpec, PluginState, PluginSupervisor, SupervisorConfig,
};
use zerodeck_protocol::PluginLoadType;

use crate::blocking::run_blocking;
use crate::discovery::discover_single_plugin;
use crate::settings::insert_dotted;
use crate::{
    CheckedHttpClient, CoreError, DiscoveredPlugin, FilePicker, FilePickerListing, FilePickerQuery,
    InstallOutcome, InstallPluginRequest, InstallerLimits, OutboundHttpPolicy, SettingsManager,
    commit_staged_plugin, discover_plugins, prepare_staged_plugin,
};

const DISABLED_PLUGINS_KEY: &str = "disabledPlugins";
const LEGACY_DISABLED_PLUGINS_KEY: &str = "disabled_plugins";
const PLUGIN_ORDER_KEY: &str = "pluginOrder";

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
    /// Why the plugin's worker was refused at spawn; `state` is `Failed` while this is set.
    pub failure_reason: Option<String>,
}

/// Target subdirectory scope when resolving static files inside an installed plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PluginFileScope {
    Dist,
    Assets,
    Data,
    FrontendBundle,
}

/// Top-level lifecycle and hot-reload events broadcast by [`PluginManager`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoaderEvent {
    PluginsChanged(Vec<PluginSummary>),
    PluginReloaded { plugin: String, directory: String },
    FrontendReloaded { plugin: String, directory: String },
}

/// Coordinates discovered plugins, `loader.json` settings, plugin installation, and worker
/// supervision.
#[derive(Debug)]
pub struct PluginManager {
    paths: PlatformPaths,
    settings: Mutex<SettingsManager>,
    plugins: Mutex<Vec<DiscoveredPlugin>>,
    supervisor: PluginSupervisor,
    loader_version: String,
    http_client: CheckedHttpClient,
    installer_limits: InstallerLimits,
    lifecycle_lock: tokio::sync::Mutex<()>,
    /// Serializes `loader.json` writes so they reach disk in order; never held by readers.
    settings_writer: tokio::sync::Mutex<()>,
    loader_events_tx: broadcast::Sender<LoaderEvent>,
    python_host_missing: Mutex<Option<String>>,
    /// Spawn preflight refusals by `manifest.name`, cleared when a later preflight passes.
    failure_reasons: Mutex<HashMap<String, String>>,
}

/// Builds a [`PluginManager`]; everything except paths and settings has a default (no plugins,
/// default supervisor config, this crate's version, the production HTTP policy, default limits).
#[derive(Debug)]
pub struct PluginManagerBuilder {
    paths: PlatformPaths,
    settings: SettingsManager,
    plugins: Vec<DiscoveredPlugin>,
    supervisor_config: SupervisorConfig,
    loader_version: String,
    http_client: Option<CheckedHttpClient>,
    installer_limits: InstallerLimits,
}

impl PluginManagerBuilder {
    /// Plugins already discovered under `plugins_dir`.
    #[must_use]
    pub fn plugins(mut self, plugins: Vec<DiscoveredPlugin>) -> Self {
        self.plugins = plugins;
        self
    }

    /// Worker supervision settings (Python host, timeouts, restart policy).
    #[must_use]
    pub fn supervisor_config(mut self, config: SupervisorConfig) -> Self {
        self.supervisor_config = config;
        self
    }

    /// Loader version reported to plugins as `DECKY_VERSION`.
    #[must_use]
    pub fn loader_version(mut self, version: impl Into<String>) -> Self {
        self.loader_version = version.into();
        self
    }

    /// Outbound HTTP client used by the installer (defaults to the production SSRF policy).
    #[must_use]
    pub fn http_client(mut self, client: CheckedHttpClient) -> Self {
        self.http_client = Some(client);
        self
    }

    /// Installer size and redirect limits.
    #[must_use]
    pub fn installer_limits(mut self, limits: InstallerLimits) -> Self {
        self.installer_limits = limits;
        self
    }

    /// Creates the manager and its supervisor.
    pub fn build(self) -> Result<PluginManager, CoreError> {
        PluginManager::from_builder(self)
    }
}

impl PluginManager {
    /// Opens `settings/loader.json`, discovers plugins under `paths.plugins_dir()`, and initializes
    /// the [`PluginSupervisor`] with the production HTTP policy and default installer limits.
    pub fn open(
        paths: PlatformPaths,
        config: SupervisorConfig,
        loader_version: impl Into<String>,
    ) -> Result<Self, CoreError> {
        let settings = SettingsManager::open(paths.settings_dir().join("loader.json"))?;
        let plugins = discover_plugins(&paths.plugins_dir())?;
        Self::builder(paths, settings)
            .plugins(plugins)
            .supervisor_config(config)
            .loader_version(loader_version)
            .build()
    }

    /// Starts a [`PluginManagerBuilder`] from already-loaded settings.
    #[must_use]
    pub fn builder(paths: PlatformPaths, settings: SettingsManager) -> PluginManagerBuilder {
        PluginManagerBuilder {
            paths,
            settings,
            plugins: Vec::new(),
            supervisor_config: SupervisorConfig::default(),
            loader_version: env!("CARGO_PKG_VERSION").to_owned(),
            http_client: None,
            installer_limits: InstallerLimits::default(),
        }
    }

    fn from_builder(builder: PluginManagerBuilder) -> Result<Self, CoreError> {
        let PluginManagerBuilder {
            paths,
            settings,
            plugins,
            supervisor_config: config,
            loader_version,
            http_client,
            installer_limits,
        } = builder;
        let http_client = match http_client {
            Some(client) => client,
            None => CheckedHttpClient::new(OutboundHttpPolicy::production())?,
        };
        let supervisor = PluginSupervisor::new(paths.clone(), config)?;
        let (loader_events_tx, _) = broadcast::channel(256);
        Ok(Self {
            paths,
            settings: Mutex::new(settings),
            plugins: Mutex::new(plugins),
            supervisor,
            loader_version,
            http_client,
            installer_limits,
            lifecycle_lock: tokio::sync::Mutex::new(()),
            settings_writer: tokio::sync::Mutex::new(()),
            loader_events_tx,
            python_host_missing: Mutex::new(None),
            failure_reasons: Mutex::new(HashMap::new()),
        })
    }

    /// Returns the resolved [`PlatformPaths`] hierarchy used by this manager.
    #[must_use]
    pub fn paths(&self) -> &PlatformPaths {
        &self.paths
    }

    /// Returns the shared SSRF-guarded [`CheckedHttpClient`] configured on this manager.
    #[must_use]
    pub fn http_client(&self) -> &CheckedHttpClient {
        &self.http_client
    }

    /// Returns the configured installer size and redirect limits.
    #[must_use]
    pub fn installer_limits(&self) -> &InstallerLimits {
        &self.installer_limits
    }

    /// Returns `true` only when `developer.allow_unverified_installs` is explicitly `true` in
    /// `loader.json`.
    #[must_use]
    pub fn allow_unverified_installs(&self) -> bool {
        let guard = lock_unpoisoned(&self.settings);
        matches!(
            guard.get_path("developer.allow_unverified_installs"),
            Some(Value::Bool(true))
        )
    }

    /// Records why the Python worker host cannot run; while set, every backend plugin spawn fails
    /// with that reason instead of retrying.
    pub fn set_python_host_missing(&self, reason: Option<String>) {
        *lock_unpoisoned(&self.python_host_missing) = reason;
    }

    /// Lets `"root"` plugins start from trees a non-root user can modify (logged on each spawn).
    /// Only the daemon's own command line or environment may turn this on: anything the
    /// unprivileged user can write, such as `loader.json`, must never grant root.
    pub fn set_allow_unsafe_root_plugins(&self, allow: bool) {
        self.supervisor.set_allow_unsafe_root_plugins(allow);
    }

    /// Starts workers for all enabled, non-passive plugins in configured order.
    pub async fn start_all(&self) -> Result<(), CoreError> {
        let _lifecycle = self.lifecycle_lock.lock().await;
        let disabled = self.disabled_set();
        let ordered = self.ordered_plugins();
        for plugin in ordered {
            if plugin.passive || disabled.contains(&plugin.manifest.name) {
                continue;
            }
            match self.spawn_checked(&plugin).await {
                Err(err @ CoreError::PluginBlocked { .. }) => {
                    warn!(error = %err, "plugin not started");
                }
                other => other?,
            }
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
        let plugin = self
            .find_plugin(plugin_name)
            .ok_or_else(|| CoreError::Host(HostError::PluginNotFound(plugin_name.to_owned())))?;

        if self.disabled_set().contains(&plugin.manifest.name) {
            return Err(CoreError::PluginDisabled(plugin.manifest.name));
        }
        if plugin.passive {
            return Err(CoreError::Host(HostError::PassivePlugin(
                plugin.manifest.name,
            )));
        }
        if let Some(reason) = self.failure_reason(&plugin.manifest.name) {
            return Err(CoreError::PluginBlocked {
                name: plugin.manifest.name,
                reason,
            });
        }

        let res = self
            .supervisor
            .call_method(plugin_name, method, args, legacy)
            .await?;
        Ok(res)
    }

    /// Restarts an enabled, active plugin worker cleanly after refreshing its on-disk manifests.
    pub async fn reload_plugin(&self, plugin_name: &str) -> Result<(), CoreError> {
        let _lifecycle = self.lifecycle_lock.lock().await;
        let existing = self
            .find_plugin(plugin_name)
            .ok_or_else(|| CoreError::Host(HostError::PluginNotFound(plugin_name.to_owned())))?;

        if self.disabled_set().contains(&existing.manifest.name) {
            return Err(CoreError::PluginDisabled(existing.manifest.name));
        }

        let plugin_dir = self.paths.plugins_dir().join(&existing.directory);
        if !plugin_dir.is_dir() {
            return Err(CoreError::Host(HostError::PluginNotFound(
                plugin_name.to_owned(),
            )));
        }
        let refreshed = discover_single_plugin(&plugin_dir, existing.directory.clone())?;
        // Workers are keyed by manifest name: refuse a rename onto a name another directory owns
        // (as discovery does), leaving both plugins as they were.
        if let Some(owner) = lock_unpoisoned(&self.plugins).iter().find(|p| {
            p.directory != existing.directory && p.manifest.name == refreshed.manifest.name
        }) {
            return Err(CoreError::DuplicatePluginName {
                name: refreshed.manifest.name,
                directory: owner.directory.clone(),
            });
        }
        let metadata_changed = refreshed != existing;

        if existing.manifest.name != refreshed.manifest.name
            && self
                .supervisor
                .plugin_state(&existing.manifest.name)
                .is_some()
        {
            shutdown_worker_if_present(&self.supervisor, &existing.manifest.name, false).await?;
        }

        if metadata_changed {
            self.replace_plugin_entry(&existing.directory, refreshed.clone());
        }

        let is_disabled = self.disabled_set().contains(&refreshed.manifest.name);
        if refreshed.passive || is_disabled {
            if self
                .supervisor
                .plugin_state(&refreshed.manifest.name)
                .is_some()
            {
                shutdown_worker_if_present(&self.supervisor, &refreshed.manifest.name, false)
                    .await?;
            }
            self.clear_failure_reason(&refreshed.manifest.name);
            if metadata_changed {
                self.emit_plugins_changed();
            }
            if is_disabled {
                return Err(CoreError::PluginDisabled(refreshed.manifest.name));
            }
            return Err(CoreError::Host(HostError::PassivePlugin(
                refreshed.manifest.name,
            )));
        }

        let cleared_failure = match self.preflight_plugin(&refreshed) {
            Ok(cleared) => cleared,
            Err(err) => {
                shutdown_worker_if_present(&self.supervisor, &refreshed.manifest.name, false)
                    .await?;
                self.emit_plugins_changed();
                return Err(err);
            }
        };

        let launch = if metadata_changed
            || self
                .supervisor
                .plugin_state(&refreshed.manifest.name)
                .is_none()
        {
            self.supervisor
                .spawn_plugin(self.spawn_spec_for(&refreshed))
                .await
        } else {
            self.supervisor
                .reload_plugin(&refreshed.manifest.name)
                .await
        };
        if let Err(err) = launch {
            let err = self.launch_error(&refreshed.manifest.name, err);
            self.emit_plugins_changed();
            return Err(err);
        }

        if metadata_changed || cleared_failure {
            self.emit_plugins_changed();
        }
        let _ = self.loader_events_tx.send(LoaderEvent::PluginReloaded {
            plugin: refreshed.manifest.name,
            directory: refreshed.directory,
        });
        Ok(())
    }

    /// Stages and atomically installs (or upgrades) a plugin from a remote or local ZIP archive.
    ///
    /// Download, SHA-256 verification, ZIP extraction, and `remote_binary` staging run before
    /// acquiring the lifecycle lock, leaving any running worker untouched if staging fails.
    #[allow(clippy::needless_pass_by_value)]
    pub async fn install_plugin(
        &self,
        req: InstallPluginRequest,
    ) -> Result<InstallOutcome, CoreError> {
        let allow_unverified = self.allow_unverified_installs();
        let staged = prepare_staged_plugin(
            &self.paths,
            &self.http_client,
            &self.installer_limits,
            &req,
            allow_unverified,
        )
        .await?;

        let _lifecycle = self.lifecycle_lock.lock().await;
        let existing = self.find_plugin(&staged.manifest().name);
        let plugins_dir = self.paths.plugins_dir();
        let target_dir_name = match &existing {
            Some(plugin) => plugin.directory.clone(),
            None => self.disambiguate_directory(&plugins_dir, &staged.suggested_directory()),
        };
        let target_path = plugins_dir.join(&target_dir_name);

        let had_running_worker = if let Some(prev) = &existing {
            let was_active = self.supervisor.plugin_state(&prev.manifest.name).is_some();
            if was_active {
                shutdown_worker_if_present(&self.supervisor, &prev.manifest.name, false).await?;
            }
            was_active
        } else {
            false
        };

        let commit_target = target_path.clone();
        let committed =
            run_blocking(move || Ok(commit_staged_plugin(staged, &commit_target))).await?;
        let outcome = match committed {
            Ok(outcome) => outcome,
            Err(err) => {
                if had_running_worker
                    && let Some(prev) = &existing
                    && !prev.passive
                    && !self.disabled_set().contains(&prev.manifest.name)
                {
                    let _ = self.spawn_checked(prev).await;
                }
                return Err(err);
            }
        };

        let discover_path = target_path.clone();
        let discover_dir = outcome.directory.clone();
        let discovered =
            run_blocking(move || discover_single_plugin(&discover_path, discover_dir)).await?;
        self.upsert_plugin_entry(discovered.clone());

        let is_disabled = self.disabled_set().contains(&discovered.manifest.name);
        let spawn_result = if !discovered.passive && !is_disabled {
            self.spawn_checked(&discovered).await
        } else {
            Ok(())
        };

        self.emit_plugins_changed();
        spawn_result?;
        Ok(outcome)
    }

    /// Stops and removes an installed plugin, optionally deleting its settings and runtime data.
    pub async fn uninstall_plugin(
        &self,
        plugin_name: &str,
        purge_data: bool,
    ) -> Result<(), CoreError> {
        let _lifecycle = self.lifecycle_lock.lock().await;
        let plugin = self
            .find_plugin(plugin_name)
            .ok_or_else(|| CoreError::Host(HostError::PluginNotFound(plugin_name.to_owned())))?;

        shutdown_worker_if_present(&self.supervisor, &plugin.manifest.name, true).await?;

        remove_path_if_exists(&self.paths.plugins_dir().join(&plugin.directory))?;
        if purge_data {
            remove_path_if_exists(&self.paths.settings_dir().join(&plugin.directory))?;
            remove_path_if_exists(&self.paths.data_dir().join(&plugin.directory))?;
        }

        self.remove_plugin_entry(&plugin.directory);
        self.clear_failure_reason(&plugin.manifest.name);
        self.strip_plugin_from_loader_settings(&plugin.manifest.name)
            .await?;
        self.emit_plugins_changed();
        Ok(())
    }

    /// Enables a plugin in `loader.json` and starts its worker when non-passive.
    pub async fn enable_plugin(&self, plugin_name: &str) -> Result<(), CoreError> {
        let _lifecycle = self.lifecycle_lock.lock().await;
        let plugin = self
            .find_plugin(plugin_name)
            .ok_or_else(|| CoreError::Host(HostError::PluginNotFound(plugin_name.to_owned())))?;

        self.mutate_disabled_plugins(|list| {
            let before = list.len();
            list.retain(|entry| entry != &plugin.manifest.name);
            list.len() != before
        })
        .await?;

        let needs_spawn = !plugin.passive
            && !matches!(
                self.supervisor.plugin_state(&plugin.manifest.name),
                Some(PluginState::Running | PluginState::Starting)
            );
        let spawn_result = if needs_spawn {
            self.spawn_checked(&plugin).await
        } else {
            Ok(())
        };

        self.emit_plugins_changed();
        spawn_result
    }

    /// Persists a plugin's disabled state in `loader.json` and stops any active worker.
    pub async fn disable_plugin(&self, plugin_name: &str) -> Result<(), CoreError> {
        let _lifecycle = self.lifecycle_lock.lock().await;
        let plugin = self
            .find_plugin(plugin_name)
            .ok_or_else(|| CoreError::Host(HostError::PluginNotFound(plugin_name.to_owned())))?;

        self.mutate_disabled_plugins(|list| {
            if list.iter().any(|entry| entry == &plugin.manifest.name) {
                false
            } else {
                list.push(plugin.manifest.name.clone());
                true
            }
        })
        .await?;

        shutdown_worker_if_present(&self.supervisor, &plugin.manifest.name, false).await?;
        self.clear_failure_reason(&plugin.manifest.name);
        self.emit_plugins_changed();
        Ok(())
    }

    /// Persists `pluginOrder` in `loader.json` and notifies subscribers.
    pub async fn set_plugin_order(&self, order: Vec<String>) -> Result<(), CoreError> {
        let _lifecycle = self.lifecycle_lock.lock().await;
        let encoded = serde_json::to_value(order)?;
        self.persist_settings(move |map| {
            map.insert(PLUGIN_ORDER_KEY.to_owned(), encoded);
            Ok(true)
        })
        .await?;
        self.emit_plugins_changed();
        Ok(())
    }

    /// Reads either the full `loader.json` map (`None`) or a dotted setting key (`Some(key)`).
    #[must_use]
    pub fn get_setting(&self, dotted_key: Option<&str>) -> Value {
        let guard = lock_unpoisoned(&self.settings);
        match dotted_key {
            None => Value::Object(guard.values().clone()),
            Some(key) => guard.get_path(key).unwrap_or(Value::Null),
        }
    }

    /// Atomically updates a dotted setting key in `loader.json`, rejecting lifecycle-managed keys.
    pub async fn set_setting(&self, dotted_key: &str, value: Value) -> Result<(), CoreError> {
        let root_key = dotted_key.split('.').next().unwrap_or("");
        if matches!(
            root_key,
            PLUGIN_ORDER_KEY | DISABLED_PLUGINS_KEY | LEGACY_DISABLED_PLUGINS_KEY
        ) {
            return Err(CoreError::InvalidSetting(format!(
                "setting `{dotted_key}` is managed by loader lifecycle methods; use set_plugin_order / enable_plugin / disable_plugin"
            )));
        }
        self.persist_settings(|map| {
            insert_dotted(map, dotted_key, value)?;
            Ok(true)
        })
        .await
    }

    /// Lists files inside the path-jailed filepicker roots, on the blocking pool.
    pub async fn list_files(&self, query: FilePickerQuery) -> Result<FilePickerListing, CoreError> {
        let paths = self.paths.clone();
        run_blocking(move || FilePicker::list(&paths, query)).await
    }

    /// Resolves a plugin-scoped static file (`dist/`, `assets/`, `data/`, or `dist/index.js`),
    /// matching `plugin_id` by `manifest.name` first and falling back to `directory`.
    pub fn resolve_plugin_file(
        &self,
        plugin_id: &str,
        scope: PluginFileScope,
        rel_path: &str,
    ) -> Result<PathBuf, CoreError> {
        let plugin = self
            .find_plugin_by_id(plugin_id)
            .ok_or_else(|| CoreError::Host(HostError::PluginNotFound(plugin_id.to_owned())))?;

        let (base_root, scope_root, effective_rel) = match scope {
            PluginFileScope::Dist => {
                let plugin_root = self.paths.plugins_dir().join(&plugin.directory);
                (plugin_root.clone(), plugin_root.join("dist"), rel_path)
            }
            PluginFileScope::Assets => {
                let plugin_root = self.paths.plugins_dir().join(&plugin.directory);
                (plugin_root.clone(), plugin_root.join("assets"), rel_path)
            }
            PluginFileScope::Data => (
                self.paths.data_dir(),
                self.paths.data_dir().join(&plugin.directory),
                rel_path,
            ),
            PluginFileScope::FrontendBundle => {
                let plugin_root = self.paths.plugins_dir().join(&plugin.directory);
                (plugin_root.clone(), plugin_root.join("dist"), "index.js")
            }
        };

        validate_rel_path_components(&scope_root, effective_rel)?;
        let base_jail = SafePath::new(&base_root)?;
        let scope_jail = SafePath::new(&scope_root)?;
        if !scope_jail.root().starts_with(base_jail.root()) {
            return Err(CoreError::Platform(PlatformError::PathEscape {
                candidate: scope_jail.root().to_path_buf(),
                root: base_jail.root().to_path_buf(),
            }));
        }

        resolve_static_file(&scope_root, effective_rel)
    }

    /// Resolves a relative file path inside `root` through [`SafePath`] and ensures it points to a
    /// regular file.
    pub fn resolve_static_file(root: &Path, rel_path: &str) -> Result<PathBuf, CoreError> {
        resolve_static_file(root, rel_path)
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
                let (state, failure_reason) = match self.failure_reason(&plugin.manifest.name) {
                    Some(reason) => (PluginState::Failed, Some(reason)),
                    None => self
                        .supervisor
                        .plugin_status(&plugin.manifest.name)
                        .unwrap_or((PluginState::Stopped, None)),
                };
                PluginSummary {
                    name: plugin.manifest.name,
                    directory: plugin.directory,
                    version: plugin.package.and_then(|pkg| pkg.version),
                    load_type: plugin.load_type,
                    disabled: is_disabled,
                    passive: plugin.passive,
                    state,
                    failure_reason,
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

    /// Subscribes to top-level [`LoaderEvent`] notifications (`PluginsChanged`, `PluginReloaded`,
    /// `FrontendReloaded`).
    #[must_use]
    pub fn subscribe_loader_events(&self) -> broadcast::Receiver<LoaderEvent> {
        self.loader_events_tx.subscribe()
    }

    /// Shuts down all running plugin workers gracefully.
    pub async fn shutdown_all(&self) {
        self.supervisor.shutdown_all().await;
    }

    pub(crate) fn debug_plugins_snapshot(&self) -> Vec<DiscoveredPlugin> {
        let disabled = self.disabled_set();
        let guard = lock_unpoisoned(&self.plugins);
        guard
            .iter()
            .filter(|plugin| {
                plugin.manifest.is_debug() && !disabled.contains(&plugin.manifest.name)
            })
            .cloned()
            .collect()
    }

    pub(crate) fn emit_frontend_reloaded(&self, plugin: &str, directory: &str) {
        let _ = self.loader_events_tx.send(LoaderEvent::FrontendReloaded {
            plugin: plugin.to_owned(),
            directory: directory.to_owned(),
        });
    }

    fn emit_plugins_changed(&self) {
        let _ = self
            .loader_events_tx
            .send(LoaderEvent::PluginsChanged(self.list_plugins()));
    }

    /// Runs the spawn preflight and, when it passes, starts the plugin's worker.
    async fn spawn_checked(&self, plugin: &DiscoveredPlugin) -> Result<(), CoreError> {
        self.preflight_plugin(plugin)?;
        self.supervisor
            .spawn_plugin(self.spawn_spec_for(plugin))
            .await
            .map_err(|err| self.launch_error(&plugin.manifest.name, err))?;
        Ok(())
    }

    /// Checks the Python host before handing off to the worker launch policy.
    fn preflight_plugin(&self, plugin: &DiscoveredPlugin) -> Result<bool, CoreError> {
        let name = &plugin.manifest.name;
        let python_host_missing = lock_unpoisoned(&self.python_host_missing).clone();
        match python_host_missing {
            None => Ok(lock_unpoisoned(&self.failure_reasons)
                .remove(name)
                .is_some()),
            Some(reason) => Err(self.record_blocked_plugin(name, reason)),
        }
    }

    fn launch_error(&self, name: &str, err: HostError) -> CoreError {
        match err {
            HostError::UnsafeRootPlugin(violation) => {
                self.record_blocked_plugin(name, violation.to_string())
            }
            other => other.into(),
        }
    }

    fn record_blocked_plugin(&self, name: &str, reason: String) -> CoreError {
        let name = name.to_owned();
        lock_unpoisoned(&self.failure_reasons).insert(name.clone(), reason.clone());
        CoreError::PluginBlocked { name, reason }
    }

    fn failure_reason(&self, plugin_name: &str) -> Option<String> {
        lock_unpoisoned(&self.failure_reasons)
            .get(plugin_name)
            .cloned()
    }

    fn clear_failure_reason(&self, plugin_name: &str) {
        lock_unpoisoned(&self.failure_reasons).remove(plugin_name);
    }

    fn spawn_spec_for(&self, plugin: &DiscoveredPlugin) -> PluginSpawnSpec {
        PluginSpawnSpec {
            generation: uuid::Uuid::new_v4().to_string(),
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

    fn find_plugin_by_id(&self, plugin_id: &str) -> Option<DiscoveredPlugin> {
        let guard = lock_unpoisoned(&self.plugins);
        guard
            .iter()
            .find(|plugin| plugin.manifest.name == plugin_id)
            .or_else(|| guard.iter().find(|plugin| plugin.directory == plugin_id))
            .cloned()
    }

    fn upsert_plugin_entry(&self, plugin: DiscoveredPlugin) {
        let mut guard = lock_unpoisoned(&self.plugins);
        if let Some(slot) = guard
            .iter_mut()
            .find(|p| p.directory == plugin.directory || p.manifest.name == plugin.manifest.name)
        {
            *slot = plugin;
        } else {
            guard.push(plugin);
        }
        guard.sort_by(|a, b| a.directory.cmp(&b.directory));
    }

    fn replace_plugin_entry(&self, directory: &str, plugin: DiscoveredPlugin) {
        let mut guard = lock_unpoisoned(&self.plugins);
        if let Some(slot) = guard.iter_mut().find(|p| p.directory == directory) {
            *slot = plugin;
        } else {
            guard.push(plugin);
        }
        guard.sort_by(|a, b| a.directory.cmp(&b.directory));
    }

    fn remove_plugin_entry(&self, directory: &str) {
        let mut guard = lock_unpoisoned(&self.plugins);
        guard.retain(|p| p.directory != directory);
    }

    fn disambiguate_directory(&self, plugins_dir: &Path, base: &str) -> String {
        let known_dirs: HashSet<String> = {
            let guard = lock_unpoisoned(&self.plugins);
            guard.iter().map(|p| p.directory.clone()).collect()
        };
        let is_occupied = |candidate: &str| -> bool {
            known_dirs.contains(candidate)
                || fs::symlink_metadata(plugins_dir.join(candidate)).is_ok()
        };

        if !is_occupied(base) {
            return base.to_owned();
        }
        let mut suffix = 2usize;
        loop {
            let candidate = format!("{base}-{suffix}");
            if !is_occupied(&candidate) {
                return candidate;
            }
            suffix += 1;
        }
    }

    /// Applies `mutate` to a copy of the settings, writes it on the blocking pool, then makes it
    /// current. Writes are ordered by `settings_writer`; readers keep seeing the previous values
    /// until the write succeeds and never wait on `fsync`.
    ///
    /// Not cancel-safe: if this future is dropped while the write runs, the file may hold the
    /// new values while memory keeps the old ones until the next write. Call it only from tasks
    /// that run to completion (every caller today runs in a detached WebSocket or lifecycle
    /// task).
    async fn persist_settings<F>(&self, mutate: F) -> Result<(), CoreError>
    where
        F: FnOnce(&mut Map<String, Value>) -> Result<bool, CoreError>,
    {
        let _writer = self.settings_writer.lock().await;
        let pending = lock_unpoisoned(&self.settings).prepare_update(mutate)?;
        let Some(pending) = pending else {
            return Ok(());
        };
        let pending = run_blocking(move || {
            pending.write()?;
            Ok(pending)
        })
        .await?;
        lock_unpoisoned(&self.settings).apply(pending);
        Ok(())
    }

    async fn mutate_disabled_plugins<F>(&self, update: F) -> Result<(), CoreError>
    where
        F: FnOnce(&mut Vec<String>) -> bool,
    {
        self.persist_settings(|map| {
            let mut list = read_disabled_list(map);
            let list_changed = update(&mut list);
            let had_legacy = map.remove(LEGACY_DISABLED_PLUGINS_KEY).is_some();
            if !list_changed && !had_legacy {
                return Ok(false);
            }
            let encoded = serde_json::to_value(list)?;
            map.insert(DISABLED_PLUGINS_KEY.to_owned(), encoded);
            Ok(true)
        })
        .await
    }

    async fn strip_plugin_from_loader_settings(
        &self,
        manifest_name: &str,
    ) -> Result<(), CoreError> {
        self.persist_settings(|map| {
            let mut changed = false;
            changed |= strip_name_from_array_key(map, PLUGIN_ORDER_KEY, manifest_name);
            changed |= strip_name_from_array_key(map, DISABLED_PLUGINS_KEY, manifest_name);
            changed |= strip_name_from_array_key(map, LEGACY_DISABLED_PLUGINS_KEY, manifest_name);
            Ok(changed)
        })
        .await
    }

    fn disabled_set(&self) -> HashSet<String> {
        let guard = lock_unpoisoned(&self.settings);
        let list = guard
            .get::<Vec<String>>(DISABLED_PLUGINS_KEY)
            .or_else(|| guard.get::<Vec<String>>(LEGACY_DISABLED_PLUGINS_KEY))
            .unwrap_or_default();
        list.into_iter().collect()
    }

    fn ordered_plugins(&self) -> Vec<DiscoveredPlugin> {
        let order = {
            let guard = lock_unpoisoned(&self.settings);
            guard
                .get::<Vec<String>>(PLUGIN_ORDER_KEY)
                .unwrap_or_default()
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

/// Resolves a relative file path inside `root` through [`SafePath`] and verifies it refers to a
/// regular file.
pub fn resolve_static_file(root: &Path, rel_path: &str) -> Result<PathBuf, CoreError> {
    validate_rel_path_components(root, rel_path)?;
    let jail = SafePath::new(root)?;
    if rel_path.is_empty() {
        return Err(CoreError::Io {
            path: jail.root().to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "requested static path is empty",
            ),
        });
    }

    let canonical = jail.resolve(rel_path)?;
    let meta = fs::metadata(&canonical).map_err(|source| CoreError::Io {
        path: canonical.clone(),
        source,
    })?;
    if !meta.is_file() {
        return Err(CoreError::Io {
            path: canonical,
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "requested static path is not a regular file",
            ),
        });
    }
    Ok(canonical)
}

fn validate_rel_path_components(root: &Path, rel_path: &str) -> Result<(), CoreError> {
    if rel_path.contains('\0') || rel_path.contains('\\') {
        return Err(CoreError::Platform(PlatformError::PathEscape {
            candidate: PathBuf::from(rel_path),
            root: root.to_path_buf(),
        }));
    }
    for component in Path::new(rel_path).components() {
        if matches!(
            component,
            Component::Prefix(_) | Component::RootDir | Component::ParentDir
        ) {
            return Err(CoreError::Platform(PlatformError::PathEscape {
                candidate: PathBuf::from(rel_path),
                root: root.to_path_buf(),
            }));
        }
    }
    Ok(())
}

async fn shutdown_worker_if_present(
    supervisor: &PluginSupervisor,
    plugin_name: &str,
    uninstall: bool,
) -> Result<(), CoreError> {
    match supervisor.shutdown_plugin(plugin_name, uninstall).await {
        Ok(()) | Err(HostError::PluginNotFound(_)) => Ok(()),
        Err(err) => Err(CoreError::Host(err)),
    }
}

fn remove_path_if_exists(path: &Path) -> Result<(), CoreError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => fs::remove_dir_all(path)
            .map_err(|source| CoreError::Io {
                path: path.to_path_buf(),
                source,
            }),
        Ok(_) => fs::remove_file(path).map_err(|source| CoreError::Io {
            path: path.to_path_buf(),
            source,
        }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(CoreError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn read_disabled_list(map: &Map<String, Value>) -> Vec<String> {
    let raw = map
        .get(DISABLED_PLUGINS_KEY)
        .or_else(|| map.get(LEGACY_DISABLED_PLUGINS_KEY));
    let Some(Value::Array(items)) = raw else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    let mut list = Vec::with_capacity(items.len());
    for item in items {
        if let Some(name) = item.as_str()
            && seen.insert(name.to_owned())
        {
            list.push(name.to_owned());
        }
    }
    list
}

fn strip_name_from_array_key(map: &mut Map<String, Value>, key: &str, name: &str) -> bool {
    let Some(Value::Array(items)) = map.get_mut(key) else {
        return false;
    };
    let before = items.len();
    items.retain(|entry| entry.as_str() != Some(name));
    items.len() != before
}
