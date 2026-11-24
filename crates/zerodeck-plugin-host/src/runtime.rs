use std::future::Future;
use std::pin::Pin;

use serde_json::Value;
use tokio::process::Command;
use zerodeck_platform::PlatformPaths;

use crate::{HostError, PluginSpawnSpec, build_plugin_env};

/// Boxed async future used by [`PluginRuntime`] to remain object-safe across backends.
pub type BoxHostFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, HostError>> + Send + 'a>>;

/// Abstraction over out-of-process plugin runtimes (Python host, future native/WASM hosts).
pub trait PluginRuntime: Send + Sync {
    /// Invokes a named RPC method on the plugin worker.
    fn call_method<'a>(
        &'a self,
        method: &'a str,
        args: Value,
        legacy: bool,
    ) -> BoxHostFuture<'a, Value>;

    /// Requests graceful shutdown of the plugin worker.
    fn shutdown(&self, uninstall: bool) -> BoxHostFuture<'_, ()>;
}

/// Configuration for launching the external `zerodeck-py-host` worker via system `python3`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PythonHostConfig {
    pub python_bin: String,
    pub host_module_path: Option<String>,
}

impl Default for PythonHostConfig {
    fn default() -> Self {
        Self {
            python_bin: "python3".to_owned(),
            host_module_path: None,
        }
    }
}

const PRESERVED_BASE_ENV_VARS: &[&str] = &[
    "PATH",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TERM",
    "TZ",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

impl PythonHostConfig {
    /// Builds a sanitized `tokio::process::Command` with `env_clear()`, `cwd`, and `DECKY_*` env.
    #[must_use]
    pub fn build_command(&self, spec: &PluginSpawnSpec, paths: &PlatformPaths) -> Command {
        let mut cmd = Command::new(&self.python_bin);
        cmd.arg("-m").arg("zerodeck_py_host");
        cmd.arg("--entrypoint").arg(&spec.entrypoint);
        cmd.arg("--plugin-name").arg(&spec.manifest.name);
        cmd.arg("--api-version")
            .arg(spec.manifest.api_version.to_string());

        cmd.env_clear();
        for key in PRESERVED_BASE_ENV_VARS {
            if let Ok(value) = std::env::var(key) {
                cmd.env(key, value);
            }
        }
        cmd.envs(build_plugin_env(spec, paths));

        let python_path = self
            .host_module_path
            .clone()
            .or_else(|| std::env::var("PYTHONPATH").ok().filter(|v| !v.is_empty()));
        if let Some(module_path) = python_path {
            cmd.env("PYTHONPATH", module_path);
        }

        if let Some(plugin_dir) = spec.entrypoint.parent()
            && plugin_dir.is_dir()
        {
            cmd.current_dir(plugin_dir);
        }

        cmd
    }
}
