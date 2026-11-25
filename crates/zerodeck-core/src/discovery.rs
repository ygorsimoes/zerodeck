use std::collections::HashMap;
use std::fs;
use std::path::Path;

use tracing::warn;
use zerodeck_protocol::{PackageManifest, PluginLoadType, PluginManifest};

use crate::CoreError;

/// Metadata discovered for an installed plugin directory under `plugins/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredPlugin {
    pub directory: String,
    pub manifest: PluginManifest,
    pub package: Option<PackageManifest>,
    pub load_type: PluginLoadType,
    pub passive: bool,
    pub has_entrypoint: bool,
}

/// Scans `plugins_dir` for non-hidden subdirectories containing a valid `plugin.json`, sorted by
/// directory name. At most one plugin is returned per manifest name: later directories declaring a
/// name already claimed by an earlier directory are logged and skipped.
///
/// A plugin whose `plugin.json` or `package.json` cannot be read or parsed is logged and skipped,
/// so one broken plugin never prevents the remaining plugins (or the daemon) from loading.
pub fn discover_plugins(plugins_dir: &Path) -> Result<Vec<DiscoveredPlugin>, CoreError> {
    if !plugins_dir.exists() {
        return Ok(Vec::new());
    }

    let entries = fs::read_dir(plugins_dir).map_err(|source| CoreError::Io {
        path: plugins_dir.to_path_buf(),
        source,
    })?;

    let mut plugins = Vec::new();
    for entry_res in entries {
        let entry = entry_res.map_err(|source| CoreError::Io {
            path: plugins_dir.to_path_buf(),
            source,
        })?;
        let directory = entry.file_name().to_string_lossy().into_owned();
        // Hidden entries include in-progress install staging and parked previous installs.
        if directory.starts_with('.') {
            continue;
        }
        let dir_path = entry.path();
        if !dir_path.is_dir() || !dir_path.join("plugin.json").is_file() {
            continue;
        }

        match discover_single_plugin(&dir_path, directory) {
            Ok(plugin) => plugins.push(plugin),
            Err(err) => {
                warn!(plugin_dir = %dir_path.display(), error = %err, "skipping invalid plugin");
            }
        }
    }

    // `read_dir` order is filesystem-dependent; sort so load order and listings are stable.
    plugins.sort_by(|a, b| a.directory.cmp(&b.directory));

    // Workers are keyed by manifest name, so two directories declaring the same name would
    // collide. Keep the first in sorted order and skip the rest.
    // Maps each kept manifest name to the directory that owns it, for the duplicate warning.
    let mut kept_dirs: HashMap<String, String> = HashMap::new();
    let mut unique = Vec::with_capacity(plugins.len());
    for plugin in plugins {
        if let Some(first_dir) = kept_dirs.get(&plugin.manifest.name) {
            warn!(
                plugin = %plugin.manifest.name,
                kept = %first_dir,
                skipped = %plugin.directory,
                "duplicate plugin name"
            );
            continue;
        }
        kept_dirs.insert(plugin.manifest.name.clone(), plugin.directory.clone());
        unique.push(plugin);
    }
    Ok(unique)
}

pub(crate) fn discover_single_plugin(
    dir_path: &Path,
    directory: String,
) -> Result<DiscoveredPlugin, CoreError> {
    let plugin_json_path = dir_path.join("plugin.json");
    let plugin_raw = fs::read_to_string(&plugin_json_path).map_err(|source| CoreError::Io {
        path: plugin_json_path.clone(),
        source,
    })?;
    let manifest: PluginManifest =
        serde_json::from_str(&plugin_raw).map_err(|source| CoreError::ManifestParse {
            path: plugin_json_path,
            source,
        })?;

    let package_json_path = dir_path.join("package.json");
    let package = if package_json_path.is_file() {
        let pkg_raw = fs::read_to_string(&package_json_path).map_err(|source| CoreError::Io {
            path: package_json_path.clone(),
            source,
        })?;
        Some(
            serde_json::from_str::<PackageManifest>(&pkg_raw).map_err(|source| {
                CoreError::ManifestParse {
                    path: package_json_path,
                    source,
                }
            })?,
        )
    } else {
        None
    };

    let load_type = package
        .as_ref()
        .map_or(PluginLoadType::LegacyEvalIife, PackageManifest::load_type);
    let has_entrypoint = dir_path.join("main.py").is_file();

    if load_type == PluginLoadType::LegacyEvalIife {
        warn!(plugin = %manifest.name, "discovered legacy IIFE plugin");
    }

    Ok(DiscoveredPlugin {
        directory,
        manifest,
        package,
        load_type,
        passive: !has_entrypoint,
        has_entrypoint,
    })
}
