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

/// Scans `plugins_dir` for subdirectories containing a valid `plugin.json`.
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
        let dir_path = entry.path();
        if !dir_path.is_dir() {
            continue;
        }

        let plugin_json_path = dir_path.join("plugin.json");
        if !plugin_json_path.is_file() {
            continue;
        }

        let directory = entry.file_name().to_string_lossy().into_owned();
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
            let pkg_raw =
                fs::read_to_string(&package_json_path).map_err(|source| CoreError::Io {
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
        let passive = !has_entrypoint;

        if load_type == PluginLoadType::LegacyEvalIife {
            warn!(plugin = %manifest.name, "discovered legacy IIFE plugin");
        }

        plugins.push(DiscoveredPlugin {
            directory,
            manifest,
            package,
            load_type,
            passive,
            has_entrypoint,
        });
    }

    Ok(plugins)
}
