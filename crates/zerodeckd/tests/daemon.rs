//! End-to-end tests that run the real `zerodeckd` binary.

#![cfg(unix)]

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_zerodeckd");

static NEXT_HOME: AtomicUsize = AtomicUsize::new(0);

struct TempHome(PathBuf);

impl TempHome {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "zerodeckd-{label}-{}-{}",
            std::process::id(),
            NEXT_HOME.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create temp homebrew");
        Self(path)
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Kills the daemon if a test fails before it shuts down on its own.
struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port(host: &str) -> u16 {
    TcpListener::bind((host, 0))
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
}

fn daemon_command(home: &TempHome, host: &str, port: u16) -> Command {
    let mut cmd = Command::new(BIN);
    cmd.env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home.0)
        .env("UNPRIVILEGED_PATH", &home.0)
        .env("PRIVILEGED_PATH", &home.0)
        .env("SERVER_HOST", host)
        .env("SERVER_PORT", port.to_string())
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

fn healthcheck(home: &TempHome, host: &str, port: u16) -> bool {
    daemon_command(home, host, port)
        .arg("--healthcheck")
        .status()
        .expect("run --healthcheck")
        .success()
}

fn wait_until_healthy(home: &TempHome, host: &str, port: u16) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if healthcheck(home, host, port) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("daemon never became healthy on {host}:{port}");
}

fn wait_for_exit(daemon: &mut Daemon, timeout: Duration) -> ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = daemon.0.try_wait().expect("poll daemon") {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "daemon did not exit within {timeout:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn sigterm(daemon: &Daemon) {
    let status = Command::new("kill")
        .args(["-TERM", &daemon.0.id().to_string()])
        .status()
        .expect("send SIGTERM");
    assert!(status.success());
}

fn serve_then_shutdown(host: &str) {
    let home = TempHome::new("serve");
    let port = free_port(host);
    let mut daemon = Daemon(
        daemon_command(&home, host, port)
            .spawn()
            .expect("spawn zerodeckd"),
    );

    wait_until_healthy(&home, host, port);
    sigterm(&daemon);
    let status = wait_for_exit(&mut daemon, Duration::from_secs(10));
    assert!(status.success(), "daemon exited with {status}");
}

#[test]
fn check_config_succeeds_on_empty_homebrew() {
    let home = TempHome::new("check-config");
    let status = daemon_command(&home, "127.0.0.1", 1)
        .arg("--check-config")
        .status()
        .expect("run --check-config");
    assert!(status.success());
}

#[test]
fn check_config_tolerates_a_broken_plugin() {
    let home = TempHome::new("check-config-broken");
    let plugin = home.0.join("plugins").join("broken");
    std::fs::create_dir_all(&plugin).expect("create plugin dir");
    std::fs::write(plugin.join("plugin.json"), "{not json").expect("write broken manifest");

    let status = daemon_command(&home, "127.0.0.1", 1)
        .arg("--check-config")
        .status()
        .expect("run --check-config");
    assert!(status.success());
}

#[test]
fn healthcheck_fails_when_nothing_is_listening() {
    let home = TempHome::new("healthcheck-down");
    assert!(!healthcheck(&home, "127.0.0.1", free_port("127.0.0.1")));
}

#[test]
fn serves_on_ipv4_loopback_and_exits_cleanly_on_sigterm() {
    serve_then_shutdown("127.0.0.1");
}

#[test]
fn serves_on_ipv6_loopback() {
    if TcpListener::bind(("::1", 0)).is_err() {
        eprintln!("skipping: IPv6 loopback is unavailable on this host");
        return;
    }
    serve_then_shutdown("::1");
}

#[test]
fn serves_on_localhost_hostname() {
    serve_then_shutdown("localhost");
}
