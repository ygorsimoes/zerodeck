use std::net::SocketAddr;

use anyhow::{Context, Result, bail};
use clap::Parser;
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;
use zerodeck_cef::CefConfig;
use zerodeck_core::{SettingsManager, discover_plugins};
use zerodeck_platform::PlatformPaths;
use zerodeck_plugin_host::{PluginRuntime, PythonHostConfig};
use zerodeck_protocol::WsMessageType;
use zerodeck_server::{AppState, ServerConfig, serve};

#[derive(Debug, Parser)]
#[command(name = "zerodeckd", version, about = "ZeroDeck plugin loader daemon")]
struct Cli {
    /// Validate environment paths, settings, and plugin manifests then exit.
    #[arg(long)]
    check_config: bool,

    /// Probe the running daemon's /healthz endpoint and exit with status 0/1.
    #[arg(long)]
    healthcheck: bool,

    /// Bind host address for HTTP/WebSocket server.
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

async fn run_healthcheck(port: u16) -> Result<()> {
    let url = format!("http://127.0.0.1:{port}/healthz");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .context("build healthcheck HTTP client")?;
    let response = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("probe {url}"))?;
    if !response.status().is_success() {
        bail!("healthcheck returned status {}", response.status());
    }
    Ok(())
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
        return run_healthcheck(cli.port).await;
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
        // Ensure trait symbol stays linked as the daemon foundation contract.
        let _: Option<&dyn PluginRuntime> = None;
        return Ok(());
    }

    let config = ServerConfig {
        host: cli.host,
        port: cli.port,
        cef: CefConfig::default(),
        python_host: PythonHostConfig::default(),
    };
    let addr: SocketAddr = format!("{}:{}", config.host, config.port)
        .parse()
        .context("parse server listen address")?;

    let state = AppState::with_plugins(config, plugins);
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;

    info!(listen = %addr, "ZeroDeck daemon listening");
    serve(listener, state, shutdown_signal())
        .await
        .context("serve HTTP/WS")?;

    Ok(())
}
