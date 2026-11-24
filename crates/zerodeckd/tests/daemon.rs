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

fn write_e2e_plugin(home: &TempHome, pid_file: &std::path::Path, stop_file: &std::path::Path) {
    let plug_dir = home.0.join("plugins").join("e2e_plug");
    std::fs::create_dir_all(&plug_dir).expect("create plugin dir");
    std::fs::write(
        plug_dir.join("plugin.json"),
        r#"{"name":"E2ePlug","author":"Tester","api_version":1}"#,
    )
    .expect("write plugin.json");
    std::fs::write(
        plug_dir.join("main.py"),
        format!(
            r#"import os, decky
from pathlib import Path

class Plugin:
    async def _main(self):
        Path({pid:?}).write_text(str(os.getpid()), encoding="utf-8")

    async def _unload(self):
        Path({stop:?}).write_text("clean", encoding="utf-8")

    async def echo_and_emit(self, msg):
        await decky.emit("e2e_event", msg)
        return f"echo:{{msg}}"
"#,
            pid = pid_file.display().to_string(),
            stop = stop_file.display().to_string(),
        ),
    )
    .expect("write main.py");
}

#[tokio::test]
async fn daemon_runs_python_plugin_over_ws_and_shuts_down_worker_on_sigterm() {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    use futures_util::{SinkExt, StreamExt};
    use serde_json::{Value, json};
    use tokio_tungstenite::tungstenite::Message;

    let home = TempHome::new("plugin-e2e");
    let pid_file = home.0.join("worker.pid");
    let stop_file = home.0.join("worker.stopped");
    write_e2e_plugin(&home, &pid_file, &stop_file);

    let py_host = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../hosts/python")
        .canonicalize()
        .expect("resolve hosts/python");
    let port = free_port("127.0.0.1");
    let mut daemon = Daemon(
        daemon_command(&home, "127.0.0.1", port)
            .env("PYTHONPATH", py_host)
            .spawn()
            .expect("spawn zerodeckd"),
    );

    wait_until_healthy(&home, "127.0.0.1", port);

    let mut http = TcpStream::connect(("127.0.0.1", port)).expect("connect http");
    http.write_all(b"GET /auth/token HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .expect("send /auth/token");
    let mut resp = String::new();
    http.read_to_string(&mut resp).expect("read token");
    let token = resp.rsplit("\r\n\r\n").next().expect("body").trim();
    assert_ne!(token, "");

    let (mut ws, _) =
        tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws?auth={token}"))
            .await
            .expect("connect ws");
    ws.send(Message::Text(
        json!({
            "type": 0,
            "id": 1,
            "route": "loader/call_plugin_method",
            "args": ["E2ePlug", "echo_and_emit", "hello"]
        })
        .to_string()
        .into(),
    ))
    .await
    .expect("send ws call");

    let mut got_reply = false;
    let mut got_event = false;
    for _ in 0..2 {
        let frame = tokio::time::timeout(Duration::from_secs(3), ws.next())
            .await
            .expect("ws frame timeout")
            .expect("ws open")
            .expect("ws frame");
        let parsed: Value = serde_json::from_str(frame.to_text().expect("text")).expect("json");
        if parsed["type"] == json!(1) && parsed["id"] == json!(1) {
            assert_eq!(parsed["result"], json!("echo:hello"));
            got_reply = true;
        } else if parsed["type"] == json!(5) && parsed["event"] == json!("loader/plugin_event") {
            assert_eq!(
                parsed["args"],
                json!([{"plugin": "E2ePlug", "event": "e2e_event", "args": ["hello"]}])
            );
            got_event = true;
        }
    }
    assert!(
        got_reply && got_event,
        "missing reply={got_reply} or event={got_event}"
    );
    assert!(pid_file.is_file(), "worker did not write pid file");

    sigterm(&daemon);
    let status = wait_for_exit(&mut daemon, Duration::from_secs(10));
    assert!(status.success(), "daemon exited with {status}");
    assert_eq!(
        std::fs::read_to_string(&stop_file).expect("read stop marker"),
        "clean"
    );
}
