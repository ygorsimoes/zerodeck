//! Server configuration and shared application state.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;
use uuid::Uuid;
use zerodeck_cef::{CefConfig, CefSupervisor};
use zerodeck_core::{
    CheckedHttpClient, DEFAULT_MAX_REMOTE_BINARY_BYTES, DEFAULT_MAX_ZIP_DOWNLOAD_BYTES,
    DiscoveredPlugin, LoaderEvent, OutboundHttpPolicy, PluginManager,
};
use zerodeck_plugin_host::{PluginEvent, PythonHostConfig};

use crate::ServerError;
use crate::ws_sync::WsSyncCache;

/// Build identity compiled into this binary (`ZERODECK_BUILD_ID`, or `dev` for local builds).
#[must_use]
pub fn build_id() -> &'static str {
    match option_env!("ZERODECK_BUILD_ID") {
        Some(build_id) if !build_id.is_empty() => build_id,
        _ => "dev",
    }
}

/// Runtime configuration for the `ZeroDeck` HTTP/WebSocket server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub cef: CefConfig,
    pub python_host: PythonHostConfig,
    /// Interval between server keepalive pings; a client that misses one pong is disconnected.
    pub ws_ping_interval: Duration,
    /// Maximum size of a single inbound WebSocket message (and frame), in bytes.
    pub ws_max_message_bytes: usize,
    /// Optional path to the packaged `@zerodeck/frontend` dist directory.
    pub frontend_dir: Option<PathBuf>,
    /// Loader version reported by `/readyz`.
    pub version: String,
    /// Embedded loader build identity reported by `/readyz`.
    pub build_id: String,
    /// Whether the Python worker host was importable during startup.
    pub python_host_ready: bool,
    /// systemd invocation identity of this daemon process, reported by `/readyz`.
    ///
    /// The updater compares it with `systemctl show -p InvocationID` so an old process or an
    /// unrelated endpoint cannot satisfy activation readiness.
    pub invocation_id: String,
    /// Maximum compressed plugin ZIP archive size accepted by the installer.
    pub max_zip_download_bytes: usize,
    /// Maximum size of a single `remote_binary` download.
    pub max_remote_binary_bytes: usize,
    /// Connect timeout for outbound `/fetch` proxy requests.
    pub fetch_connect_timeout: Duration,
    /// Total response timeout for outbound `/fetch` proxy requests.
    pub fetch_response_timeout: Duration,
    /// Maximum number of completed unacknowledged replies kept in [`WsSyncCache`].
    pub ws_sync_cache_capacity: usize,
    /// Time-to-live for completed [`WsSyncCache`] entries.
    pub ws_sync_cache_ttl: Duration,
    /// Maximum number of calls one WebSocket connection runs at the same time.
    pub ws_max_in_flight_calls: usize,
    /// Maximum number of calls one connection may have waiting for a free slot; calls beyond
    /// in-flight + queued are rejected with `TooManyPendingCalls`.
    pub ws_max_queued_calls: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_owned(),
            port: 1337,
            cef: CefConfig::default(),
            python_host: PythonHostConfig::default(),
            ws_ping_interval: Duration::from_secs(30),
            ws_max_message_bytes: 16 * 1024 * 1024,
            frontend_dir: None,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            build_id: build_id().to_owned(),
            python_host_ready: false,
            invocation_id: String::new(),
            max_zip_download_bytes: DEFAULT_MAX_ZIP_DOWNLOAD_BYTES,
            max_remote_binary_bytes: DEFAULT_MAX_REMOTE_BINARY_BYTES,
            fetch_connect_timeout: Duration::from_secs(10),
            fetch_response_timeout: Duration::from_secs(60),
            ws_sync_cache_capacity: 256,
            ws_sync_cache_ttl: Duration::from_secs(300),
            ws_max_in_flight_calls: 64,
            ws_max_queued_calls: 1024,
        }
    }
}

/// Shared application state passed across Axum handlers and middleware.
#[derive(Debug, Clone)]
pub struct AppState {
    inner: Arc<AppStateInner>,
}

#[derive(Debug)]
pub(crate) enum PluginBackend {
    Static(Vec<DiscoveredPlugin>),
    Live(Arc<PluginManager>),
}

#[derive(Debug)]
struct AppStateInner {
    csrf_token: String,
    config: ServerConfig,
    backend: PluginBackend,
    http_client: CheckedHttpClient,
    ws_sync_cache: Arc<WsSyncCache>,
    cef_supervisor: Option<Arc<CefSupervisor>>,
}

/// Builds an [`AppState`]. See [`AppState::builder`].
#[derive(Debug)]
pub struct AppStateBuilder {
    config: ServerConfig,
    backend: PluginBackend,
    http_client: Option<CheckedHttpClient>,
    cef_supervisor: Option<Arc<CefSupervisor>>,
}

impl AppStateBuilder {
    /// Serves plugins and RPC from a live [`PluginManager`].
    #[must_use]
    pub fn manager(mut self, manager: Arc<PluginManager>) -> Self {
        self.backend = PluginBackend::Live(manager);
        self
    }

    /// Outbound client for `/fetch` (defaults to the production SSRF policy with the configured
    /// timeouts).
    #[must_use]
    pub fn http_client(mut self, client: CheckedHttpClient) -> Self {
        self.http_client = Some(client);
        self
    }

    /// Exposes the Steam CEF supervisor status and confirmation routes.
    #[must_use]
    pub fn cef_supervisor(mut self, supervisor: Arc<CefSupervisor>) -> Self {
        self.cef_supervisor = Some(supervisor);
        self
    }

    /// Creates the state with a fresh ephemeral auth token.
    pub fn build(self) -> Result<AppState, ServerError> {
        let http_client = match self.http_client {
            Some(client) => client,
            None => OutboundHttpPolicy::production()
                .with_timeouts(
                    Some(self.config.fetch_connect_timeout),
                    Some(self.config.fetch_response_timeout),
                )
                .build_checked_client()?,
        };
        Ok(AppState::from_parts(
            self.config,
            self.backend,
            http_client,
            self.cef_supervisor,
        ))
    }
}

impl AppState {
    /// Starts an [`AppStateBuilder`]; without a manager the state serves a static, empty plugin
    /// list, and without an HTTP client it builds one with the production SSRF policy.
    #[must_use]
    pub fn builder(config: ServerConfig) -> AppStateBuilder {
        AppStateBuilder {
            config,
            backend: PluginBackend::Static(Vec::new()),
            http_client: None,
            cef_supervisor: None,
        }
    }

    fn from_parts(
        config: ServerConfig,
        backend: PluginBackend,
        http_client: CheckedHttpClient,
        cef_supervisor: Option<Arc<CefSupervisor>>,
    ) -> Self {
        let ws_sync_cache = Arc::new(WsSyncCache::new(
            config.ws_sync_cache_capacity,
            config.ws_sync_cache_ttl,
        ));
        Self {
            inner: Arc::new(AppStateInner {
                csrf_token: Uuid::new_v4().to_string(),
                config,
                backend,
                http_client,
                ws_sync_cache,
                cef_supervisor,
            }),
        }
    }

    /// Returns the daemon's active CSRF token.
    #[must_use]
    pub fn csrf_token(&self) -> &str {
        &self.inner.csrf_token
    }

    /// Plugin metadata source: a live manager or a static discovered list.
    pub(crate) fn backend(&self) -> &PluginBackend {
        &self.inner.backend
    }

    /// Returns the active server configuration.
    #[must_use]
    pub fn config(&self) -> &ServerConfig {
        &self.inner.config
    }

    pub(crate) fn http_client(&self) -> &CheckedHttpClient {
        &self.inner.http_client
    }

    pub(crate) fn ws_sync_cache(&self) -> &Arc<WsSyncCache> {
        &self.inner.ws_sync_cache
    }

    /// Steam CEF supervisor when the daemon started one.
    pub(crate) fn cef_supervisor(&self) -> Option<&Arc<CefSupervisor>> {
        self.inner.cef_supervisor.as_ref()
    }

    pub(crate) fn subscribe_plugin_events(&self) -> Option<broadcast::Receiver<PluginEvent>> {
        match &self.inner.backend {
            PluginBackend::Static(_) => None,
            PluginBackend::Live(manager) => Some(manager.subscribe_events()),
        }
    }

    pub(crate) fn subscribe_loader_events(&self) -> Option<broadcast::Receiver<LoaderEvent>> {
        match &self.inner.backend {
            PluginBackend::Static(_) => None,
            PluginBackend::Live(manager) => Some(manager.subscribe_loader_events()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AppState, PluginBackend, ServerConfig};

    #[test]
    fn app_state_builder_defaults_to_static_empty_backend() {
        let state = AppState::builder(ServerConfig::default())
            .build()
            .expect("build app state");
        assert!(matches!(state.backend(), PluginBackend::Static(plugins) if plugins.is_empty()));
        assert!(
            state
                .http_client()
                .policy()
                .validate_url("http://127.0.0.1:1337/")
                .is_err(),
            "the default client must use the production SSRF policy"
        );
    }
}
