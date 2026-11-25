use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use tokio::net::TcpListener;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use zerodeck_cef::CefConfig;
use zerodeck_core::{
    InstallerLimits, OutboundHttpPolicy, PluginManager, SettingsManager, discover_plugins,
    start_debug_watcher,
};
use zerodeck_platform::PlatformPaths;
use zerodeck_plugin_host::{PythonHostConfig, SupervisorConfig};
use zerodeck_protocol::WsMessageType;
use zerodeck_server::{AppState, ServerConfig, serve};

#[derive(Debug, Parser)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent command-line switches, not a state machine"
)]
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

    /// Allow binding to a non-loopback address. This exposes the auth token endpoint to that
    /// network; only enable it behind a trusted port mapping or firewall.
    #[arg(
        long,
        env = "ZERODECK_ALLOW_NON_LOOPBACK",
        value_parser = clap::builder::BoolishValueParser::new()
    )]
    allow_non_loopback: bool,

    /// Start `"root"` plugins even when a non-root user can modify their files. Development only:
    /// it hands root to whoever can write the plugin tree.
    #[arg(
        long,
        env = "ZERODECK_ALLOW_UNSAFE_ROOT_PLUGINS",
        value_parser = clap::builder::BoolishValueParser::new()
    )]
    allow_unsafe_root_plugins: bool,
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .init();
}

/// Directories searched for `zerodeck_py_host`, in order. Release builds only look at install
/// locations: the working directory (`~/homebrew/services` under systemd) and the build tree may be
/// writable by the unprivileged user, and root plugins import this module.
fn python_host_candidates(exe_dir: Option<&Path>) -> Vec<PathBuf> {
    install_candidates(exe_dir, "python", "hosts/python")
}

/// Directories searched for the `@zerodeck/frontend` bundle, in order (same policy as
/// [`python_host_candidates`]).
fn frontend_candidates(exe_dir: Option<&Path>) -> Vec<PathBuf> {
    install_candidates(exe_dir, "frontend", "frontend/dist")
}

fn install_candidates(exe_dir: Option<&Path>, installed: &str, repo: &str) -> Vec<PathBuf> {
    let mut candidates = vec![Path::new("/opt/zerodeck").join(installed)];
    if let Some(dir) = exe_dir {
        candidates.push(dir.join("..").join(installed));
    }
    #[cfg(debug_assertions)]
    {
        candidates.push(PathBuf::from(repo));
        candidates.push(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join(repo),
        );
    }
    #[cfg(not(debug_assertions))]
    let _ = repo;
    candidates
}

fn current_exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
}

const PYTHON_HOST_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Returns why `python3 -c "import zerodeck_py_host"` fails with the resolved `PYTHONPATH`, or
/// `None` when the worker host is importable.
async fn probe_python_host(cfg: &PythonHostConfig) -> Option<String> {
    let python = &cfg.python_bin;
    let python_path = cfg
        .host_module_path
        .clone()
        .or_else(|| std::env::var("PYTHONPATH").ok().filter(|v| !v.is_empty()));
    let mut cmd = tokio::process::Command::new(python);
    cmd.args(["-c", "import zerodeck_py_host"])
        .env_clear()
        // `-c` puts the working directory on `sys.path`: probe from `/` so a planted module in a
        // user-writable working directory is never imported by a root daemon.
        .current_dir("/")
        // A root daemon must not leave root-owned `__pycache__` in the worker host's directory.
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    if let Some(path) = &python_path {
        cmd.env("PYTHONPATH", path);
    }
    let shown_path = python_path.as_deref().unwrap_or("<unset>");

    let output = match tokio::time::timeout(PYTHON_HOST_PROBE_TIMEOUT, cmd.output()).await {
        Err(_) => {
            return Some(format!(
                "`{python} -c \"import zerodeck_py_host\"` did not finish within \
                 {PYTHON_HOST_PROBE_TIMEOUT:?} (PYTHONPATH={shown_path})"
            ));
        }
        Ok(Err(err)) => {
            return Some(format!(
                "cannot run `{python}` for the Python worker host: {err}"
            ));
        }
        Ok(Ok(output)) => output,
    };
    if output.status.success() {
        return None;
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no error output");
    Some(format!(
        "the Python worker host `zerodeck_py_host` is not importable by `{python}` \
         (PYTHONPATH={shown_path}): {detail}"
    ))
}

/// Resolves the `zerodeck_py_host` module search path from `PYTHONPATH` or standard install/repo
/// locations.
fn resolve_python_host_path() -> Option<String> {
    if let Ok(env_path) = std::env::var("PYTHONPATH")
        && !env_path.is_empty()
    {
        return Some(env_path);
    }

    python_host_candidates(current_exe_dir().as_deref())
        .into_iter()
        .find(|dir| dir.join("zerodeck_py_host").is_dir())
        .map(|dir| dir.canonicalize().unwrap_or(dir).display().to_string())
}

/// Resolves the packaged `@zerodeck/frontend` directory from `ZERODECK_FRONTEND_DIR` or standard
/// install/repo locations.
fn resolve_frontend_path() -> Option<PathBuf> {
    if let Ok(env_dir) = std::env::var("ZERODECK_FRONTEND_DIR")
        && !env_dir.is_empty()
    {
        let dir = PathBuf::from(&env_dir);
        if dir.is_dir() {
            return Some(dir.canonicalize().unwrap_or(dir));
        }
        warn!(path = %env_dir, "ZERODECK_FRONTEND_DIR does not point to a directory");
        return None;
    }

    frontend_candidates(current_exe_dir().as_deref())
        .into_iter()
        .find(|dir| dir.is_dir())
        .map(|dir| dir.canonicalize().unwrap_or(dir))
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

/// Refuses to bind anything but loopback addresses unless the operator opted in explicitly.
fn check_bind_policy(addrs: &[SocketAddr], allow_non_loopback: bool) -> Result<()> {
    let exposed: Vec<String> = addrs
        .iter()
        .filter(|addr| !addr.ip().is_loopback())
        .map(ToString::to_string)
        .collect();
    if exposed.is_empty() {
        return Ok(());
    }
    let exposed = exposed.join(", ");
    if allow_non_loopback {
        warn!(addresses = %exposed, "binding to a non-loopback address; the auth token is reachable from that network");
        return Ok(());
    }
    bail!(
        "refusing to bind non-loopback address(es) {exposed}: the daemon serves its auth token \
         over HTTP, so exposing it to a network allows remote code execution. Pass \
         --allow-non-loopback (or set ZERODECK_ALLOW_NON_LOOPBACK=1) to override"
    )
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

/// Warns once when a root daemon runs plugins from below directories a non-root user can modify:
/// that user can swap the tree between the spawn ownership check and the spawn itself.
#[cfg(unix)]
fn warn_about_writable_ancestors(paths: &PlatformPaths) {
    if zerodeck_platform::current_process_identity().euid != 0 {
        return;
    }
    let flagged = zerodeck_platform::non_root_writable_ancestors(paths.privileged_root());
    if flagged.is_empty() {
        return;
    }
    let flagged = flagged
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    warn!(
        paths = %flagged,
        "PRIVILEGED_PATH or its parents are writable by a non-root user, who could replace root \
         plugins after their ownership check"
    );
}

async fn run_daemon(
    cli: Cli,
    paths: PlatformPaths,
    settings: SettingsManager,
    plugins: Vec<zerodeck_core::DiscoveredPlugin>,
) -> Result<()> {
    let addrs = resolve(&cli.host, cli.port).await?;
    check_bind_policy(&addrs, cli.allow_non_loopback)?;
    #[cfg(unix)]
    warn_about_writable_ancestors(&paths);

    let python_host = PythonHostConfig {
        host_module_path: resolve_python_host_path(),
        ..PythonHostConfig::default()
    };
    let python_host_missing = probe_python_host(&python_host).await;
    let config = ServerConfig {
        host: cli.host,
        port: cli.port,
        cef: CefConfig::default(),
        python_host: python_host.clone(),
        frontend_dir: resolve_frontend_path(),
        ..ServerConfig::default()
    };
    let http_client = OutboundHttpPolicy::production()
        .with_timeouts(
            Some(config.fetch_connect_timeout),
            Some(config.fetch_response_timeout),
        )
        .build_checked_client()
        .context("initialize outbound HTTP client")?;
    let installer_limits = InstallerLimits {
        max_zip_download_bytes: config.max_zip_download_bytes,
        max_remote_binary_bytes: config.max_remote_binary_bytes,
        ..InstallerLimits::default()
    };
    let sup_config = SupervisorConfig {
        python_host,
        ..SupervisorConfig::default()
    };
    let manager = Arc::new(
        PluginManager::with_http_config(
            paths,
            settings,
            plugins,
            sup_config,
            env!("CARGO_PKG_VERSION"),
            http_client,
            installer_limits,
        )
        .context("initialize plugin manager")?,
    );
    if let Some(reason) = python_host_missing {
        error!(reason = %reason, "Python worker host unavailable; backend plugins will not start");
        manager.set_python_host_missing(Some(reason));
    }
    if cli.allow_unsafe_root_plugins {
        warn!("--allow-unsafe-root-plugins is set: root plugins skip the ownership check");
        manager.set_allow_unsafe_root_plugins(true);
    }
    if let Err(err) = manager.start_all().await {
        warn!(error = %err, "one or more plugins failed during startup");
    }

    let listener = match bind_first(&addrs).await {
        Ok(listener) => listener,
        Err(err) => {
            manager.shutdown_all().await;
            return Err(err);
        }
    };
    let addr = listener.local_addr().context("read bound address")?;
    let watcher = match start_debug_watcher(Arc::clone(&manager)) {
        Ok(handle) => Some(handle),
        Err(err) => {
            warn!(error = %err, "failed to start debug hot-reload watcher");
            None
        }
    };
    let frontend_dir = config
        .frontend_dir
        .as_ref()
        .map(|p| p.display().to_string());
    let state = AppState::with_manager_and_http(
        config,
        Arc::clone(&manager),
        manager.http_client().clone(),
    );

    info!(listen = %addr, frontend_dir = ?frontend_dir, "ZeroDeck daemon listening");
    let serve_result = serve(listener, state, shutdown_signal())
        .await
        .context("serve HTTP/WS");

    if let Some(watcher) = watcher {
        watcher.stop().await;
    }
    manager.shutdown_all().await;
    serve_result
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
        let addrs = resolve(&cli.host, cli.port).await?;
        check_bind_policy(&addrs, cli.allow_non_loopback)?;
        return Ok(());
    }

    run_daemon(cli, paths, settings, plugins).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addrs(list: &[&str]) -> Vec<SocketAddr> {
        list.iter()
            .map(|a| a.parse().expect("socket addr"))
            .collect()
    }

    const EXE_DIR: &str = "/usr/local/bin";

    #[cfg(not(debug_assertions))]
    #[test]
    fn release_candidates_exclude_cwd_and_manifest_dir() {
        let exe_dir = Path::new(EXE_DIR);
        assert_eq!(
            python_host_candidates(Some(exe_dir)),
            vec![
                PathBuf::from("/opt/zerodeck/python"),
                exe_dir.join("../python")
            ]
        );
        assert_eq!(
            frontend_candidates(Some(exe_dir)),
            vec![
                PathBuf::from("/opt/zerodeck/frontend"),
                exe_dir.join("../frontend")
            ]
        );
        assert_eq!(
            python_host_candidates(None),
            vec![PathBuf::from("/opt/zerodeck/python")]
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    fn debug_candidates_include_repo_paths() {
        let exe_dir = Path::new(EXE_DIR);
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        assert_eq!(
            python_host_candidates(Some(exe_dir)),
            vec![
                PathBuf::from("/opt/zerodeck/python"),
                exe_dir.join("../python"),
                PathBuf::from("hosts/python"),
                manifest_dir.join("../../hosts/python"),
            ]
        );
        assert_eq!(
            frontend_candidates(Some(exe_dir)),
            vec![
                PathBuf::from("/opt/zerodeck/frontend"),
                exe_dir.join("../frontend"),
                PathBuf::from("frontend/dist"),
                manifest_dir.join("../../frontend/dist"),
            ]
        );
    }

    #[tokio::test]
    async fn probe_python_host_accepts_importable_host() {
        let host = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../hosts/python");
        let cfg = PythonHostConfig {
            host_module_path: Some(host.display().to_string()),
            ..PythonHostConfig::default()
        };
        assert_eq!(probe_python_host(&cfg).await, None);
    }

    #[tokio::test]
    async fn probe_python_host_reports_why_it_fails() {
        let missing_module = PythonHostConfig {
            host_module_path: Some("/nonexistent/zerodeck/python".to_owned()),
            ..PythonHostConfig::default()
        };
        let reason = probe_python_host(&missing_module)
            .await
            .expect("module outside PYTHONPATH is missing");
        assert!(reason.contains("zerodeck_py_host"), "{reason}");
        assert!(reason.contains("/nonexistent/zerodeck/python"), "{reason}");

        let missing_python = PythonHostConfig {
            python_bin: "/nonexistent/python3".to_owned(),
            host_module_path: None,
        };
        let reason = probe_python_host(&missing_python)
            .await
            .expect("missing interpreter");
        assert!(reason.contains("/nonexistent/python3"), "{reason}");
    }

    #[test]
    fn bind_policy_accepts_loopback_addresses() {
        for list in [
            &["127.0.0.1:1337"][..],
            &["[::1]:1337"],
            &["127.0.0.1:1337", "[::1]:1337"],
        ] {
            check_bind_policy(&addrs(list), false).expect("loopback is allowed");
        }
    }

    #[tokio::test]
    async fn bind_policy_accepts_resolved_localhost() {
        let resolved = resolve("localhost", 1337).await.expect("resolve localhost");
        check_bind_policy(&resolved, false).expect("localhost resolves to loopback only");
    }

    #[test]
    fn bind_policy_rejects_unspecified_and_lan_without_opt_in() {
        for list in [
            &["0.0.0.0:1337"][..],
            &["[::]:1337"],
            &["192.168.1.10:1337"],
            &["127.0.0.1:1337", "192.168.1.10:1337"],
        ] {
            let err = check_bind_policy(&addrs(list), false)
                .expect_err("non-loopback must be refused")
                .to_string();
            assert!(err.contains("--allow-non-loopback"), "{err}");
            assert!(err.contains("ZERODECK_ALLOW_NON_LOOPBACK"), "{err}");
        }
    }

    #[test]
    fn bind_policy_allows_any_address_with_opt_in() {
        for list in [
            &["0.0.0.0:1337"][..],
            &["[::]:1337"],
            &["192.168.1.10:1337"],
            &["127.0.0.1:1337", "192.168.1.10:1337"],
        ] {
            check_bind_policy(&addrs(list), true).expect("opt-in allows any address");
        }
    }
}
