//! Server configuration and shared application state.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;
use uuid::Uuid;
use zerodeck_cef::CefConfig;
use zerodeck_core::{
    CheckedHttpClient, DEFAULT_MAX_REMOTE_BINARY_BYTES, DEFAULT_MAX_ZIP_DOWNLOAD_BYTES,
    DiscoveredPlugin, LoaderEvent, OutboundHttpPolicy, PluginManager,
};
use zerodeck_plugin_host::{PluginEvent, PythonHostConfig};

use crate::ws_sync::WsSyncCache;

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
}

impl AppState {
    /// Creates a new [`AppState`] with a fresh ephemeral CSRF token and no plugins.
    #[must_use]
    pub fn new(config: ServerConfig) -> Self {
        Self::with_plugins(config, Vec::new())
    }

    /// Creates a new [`AppState`] preloaded with static discovered plugin metadata.
    #[must_use]
    pub fn with_plugins(config: ServerConfig, plugins: Vec<DiscoveredPlugin>) -> Self {
        let http_client = build_default_http_client(&config);
        Self::from_parts(config, PluginBackend::Static(plugins), http_client)
    }

    /// Creates a new [`AppState`] backed by a live [`PluginManager`].
    #[must_use]
    pub fn with_manager(config: ServerConfig, manager: Arc<PluginManager>) -> Self {
        let http_client = build_default_http_client(&config);
        Self::with_manager_and_http(config, manager, http_client)
    }

    /// Creates a new [`AppState`] backed by a live [`PluginManager`] and explicit [`CheckedHttpClient`].
    #[must_use]
    pub fn with_manager_and_http(
        config: ServerConfig,
        manager: Arc<PluginManager>,
        http_client: CheckedHttpClient,
    ) -> Self {
        Self::from_parts(config, PluginBackend::Live(manager), http_client)
    }

    /// Test helper constructing an [`AppState`] with a custom [`CheckedHttpClient`] and no manager.
    #[doc(hidden)]
    #[must_use]
    pub fn with_http_for_tests(config: ServerConfig, http_client: CheckedHttpClient) -> Self {
        Self::from_parts(config, PluginBackend::Static(Vec::new()), http_client)
    }

    fn from_parts(
        config: ServerConfig,
        backend: PluginBackend,
        http_client: CheckedHttpClient,
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

fn build_default_http_client(config: &ServerConfig) -> CheckedHttpClient {
    OutboundHttpPolicy::production()
        .with_timeouts(
            Some(config.fetch_connect_timeout),
            Some(config.fetch_response_timeout),
        )
        .build_checked_client()
        .expect("build production HTTP client")
}
