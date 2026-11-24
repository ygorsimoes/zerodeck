use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::Parser;
use tokio::net::TcpListener;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use zerodeck_cef::CefConfig;
use zerodeck_core::{PluginManager, SettingsManager, discover_plugins};
use zerodeck_platform::PlatformPaths;
use zerodeck_plugin_host::{PythonHostConfig, SupervisorConfig};
use zerodeck_protocol::WsMessageType;
use zerodeck_server::{AppState, ServerConfig, serve};

#[derive(Debug, Parser)]
#[command(name = "zerodeckd", version, about = "ZeroDeck plugin loader daemon")]
struct Cli {
    /// Validate environment paths, settings, and plugin manifests then exit.
    #[arg(long)]
    check_config: bool,

    /// Probe the running daemon's /healthz endpoint (on --host/--port) and exit with status 0/1.
    #[arg(long)]
    healthcheck: bool,

    /// Bind host for the HTTP/WebSocket server: an IPv4/IPv6 address or a hostname.
    #[arg(long, env = "SERVER_HOST", default_value = "127.0.0.1")]
    host: String,

    /// Bind port for HTTP/WebSocket server.
    #[arg(long, env = "SERVER_PORT", default_value_t = 1337)]
    port: u16,
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .init();
}

/// Resolves the `zerodeck_py_host` module search path from `PYTHONPATH` or standard install/repo
/// locations.
fn resolve_python_host_path() -> Option<String> {
    if let Ok(env_path) = std::env::var("PYTHONPATH")
        && !env_path.is_empty()
    {
        return Some(env_path);
    }

    let mut candidates = vec![PathBuf::from("/opt/zerodeck/python")];
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        candidates.push(parent.join("../python"));
    }
    candidates.push(PathBuf::from("hosts/python"));
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../hosts/python"));

    candidates
        .into_iter()
        .find(|dir| dir.join("zerodeck_py_host").is_dir())
        .map(|dir| dir.canonicalize().unwrap_or(dir).display().to_string())
}

/// Resolves `host:port` (IP literal or hostname) to socket addresses.
async fn resolve(host: &str, port: u16) -> Result<Vec<SocketAddr>> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .with_context(|| format!("resolve listen address {host}:{port}"))?
        .collect();
    if addrs.is_empty() {
        bail!("listen address {host}:{port} resolved to no addresses");
    }
    Ok(addrs)
}

/// Maps a wildcard bind address (`0.0.0.0` / `::`) to the matching loopback address to probe.
fn probe_target(addr: SocketAddr) -> SocketAddr {
    match addr.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), addr.port())
        }
        IpAddr::V6(ip) if ip.is_unspecified() => {
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), addr.port())
        }
        _ => addr,
    }
}

/// Binds the first address that accepts a listener (e.g. `localhost` may resolve to `::1` on a host
/// without IPv6, in which case `127.0.0.1` is used instead).
async fn bind_first(addrs: &[SocketAddr]) -> Result<TcpListener> {
    let mut last_error = None;
    for addr in addrs {
        match TcpListener::bind(addr).await {
            Ok(listener) => return Ok(listener),
            Err(err) => last_error = Some(anyhow::Error::new(err).context(format!("bind {addr}"))),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no address to bind")))
}

async fn run_healthcheck(host: &str, port: u16) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .context("build healthcheck HTTP client")?;

    let mut last_error = None;
    for addr in resolve(host, port).await? {
        let url = format!("http://{}/healthz", probe_target(addr));
        match client.get(&url).send().await {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(response) => {
                last_error = Some(anyhow::anyhow!(
                    "{url} returned status {}",
                    response.status()
                ));
            }
            Err(err) => last_error = Some(anyhow::Error::new(err).context(format!("probe {url}"))),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no address to probe")))
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    info!("received shutdown signal, draining ZeroDeck server");
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing();

    if cli.healthcheck {
        return run_healthcheck(&cli.host, cli.port).await;
    }

    let paths = PlatformPaths::from_env();
    let settings = SettingsManager::open(paths.settings_dir().join("loader.json"))
        .context("load loader settings")?;
    let plugins = discover_plugins(&paths.plugins_dir()).context("discover plugins")?;

    info!(
        version = env!("CARGO_PKG_VERSION"),
        protocol_call_code = WsMessageType::Call.as_i16(),
        plugins_dir = %paths.plugins_dir().display(),
        discovered_plugins = plugins.len(),
        settings_path = %settings.path().display(),
        "ZeroDeck configuration validated"
    );

    if cli.check_config {
        return Ok(());
    }

    let python_host = PythonHostConfig {
        host_module_path: resolve_python_host_path(),
        ..PythonHostConfig::default()
    };
    let sup_config = SupervisorConfig {
        python_host: python_host.clone(),
        ..SupervisorConfig::default()
    };
    let manager = Arc::new(
        PluginManager::from_parts(
            paths,
            settings,
            plugins,
            sup_config,
            env!("CARGO_PKG_VERSION"),
        )
        .context("initialize plugin manager")?,
    );
    if let Err(err) = manager.start_all().await {
        warn!(error = %err, "one or more plugins failed during startup");
    }

    let config = ServerConfig {
        host: cli.host,
        port: cli.port,
        cef: CefConfig::default(),
        python_host,
        ..ServerConfig::default()
    };
    let listener = bind_first(&resolve(&config.host, config.port).await?).await?;
    let addr = listener.local_addr().context("read bound address")?;
    let state = AppState::with_manager(config, Arc::clone(&manager));

    info!(listen = %addr, "ZeroDeck daemon listening");
    let serve_result = serve(listener, state, shutdown_signal())
        .await
        .context("serve HTTP/WS");

    manager.shutdown_all().await;
    serve_result
}
