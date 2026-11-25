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

/// Kills a worker that outlived its daemon, so a failing test does not leak it.
#[cfg(target_os = "linux")]
struct Worker(u32);

#[cfg(target_os = "linux")]
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .args(["-KILL", &self.0.to_string()])
            .stderr(Stdio::null())
            .status();
    }
}

/// `kill -0` also succeeds on a zombie that is still waiting to be reaped, which already exited.
#[cfg(target_os = "linux")]
fn process_running(pid: u32) -> bool {
    let signalable = Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .expect("run kill -0")
        .success();
    signalable
        && std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(')')
                .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z'))
        })
}

#[cfg(target_os = "linux")]
fn wait_for_worker_pid(pid_file: &std::path::Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(pid) = std::fs::read_to_string(pid_file)
            .ok()
            .and_then(|text| text.trim().parse().ok())
        {
            return pid;
        }
        assert!(Instant::now() < deadline, "worker never wrote its pid file");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A plugin whose `_main` records the worker pid, then blocks the worker's event loop for good.
/// The worker then never sees EOF on its IPC socket when the daemon dies, so only the parent-death
/// signal (default SIGTERM action) can end it: exactly what the tests below observe.
#[cfg(target_os = "linux")]
fn write_blocking_plugin(home: &TempHome, pid_file: &std::path::Path) {
    let plug_dir = home.0.join("plugins").join("blocking_plug");
    std::fs::create_dir_all(&plug_dir).expect("create plugin dir");
    std::fs::write(
        plug_dir.join("plugin.json"),
        r#"{"name":"BlockingPlug","author":"Tester","api_version":1}"#,
    )
    .expect("write plugin.json");
    std::fs::write(
        plug_dir.join("main.py"),
        format!(
            r#"import os, signal, time
from pathlib import Path

class Plugin:
    async def _main(self):
        signal.signal(signal.SIGTERM, signal.SIG_DFL)
        Path({pid:?}).write_text(str(os.getpid()), encoding="utf-8")
        time.sleep(3600)
"#,
            pid = pid_file.display().to_string(),
        ),
    )
    .expect("write main.py");
}

/// Starts the daemon with the blocking plugin, SIGKILLs the daemon (no graceful shutdown) and
/// asserts the worker follows it within 5 s. `run_dir` must be writable by the worker's user.
#[cfg(target_os = "linux")]
fn assert_worker_exits_when_daemon_is_killed(
    home: &TempHome,
    run_dir: &std::path::Path,
    configure: impl FnOnce(&mut Command),
    inspect_worker: impl FnOnce(u32),
) {
    let pid_file = run_dir.join("worker.pid");
    write_blocking_plugin(home, &pid_file);

    let py_host = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../hosts/python")
        .canonicalize()
        .expect("resolve hosts/python");
    let port = free_port("127.0.0.1");
    let mut cmd = daemon_command(home, "127.0.0.1", port);
    cmd.env("PYTHONPATH", py_host);
    configure(&mut cmd);
    let mut daemon = Daemon(cmd.spawn().expect("spawn zerodeckd"));

    wait_until_healthy(home, "127.0.0.1", port);
    let worker = Worker(wait_for_worker_pid(&pid_file));
    assert!(process_running(worker.0), "worker is not running");
    inspect_worker(worker.0);

    let status = Command::new("kill")
        .args(["-KILL", &daemon.0.id().to_string()])
        .status()
        .expect("send SIGKILL");
    assert!(status.success());
    wait_for_exit(&mut daemon, Duration::from_secs(5));

    let deadline = Instant::now() + Duration::from_secs(5);
    while process_running(worker.0) {
        assert!(
            Instant::now() < deadline,
            "worker {} still alive 5 s after the daemon was killed",
            worker.0
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn worker_exits_when_daemon_is_killed() {
    let home = TempHome::new("pdeathsig");
    assert_worker_exits_when_daemon_is_killed(&home, &home.0, |_| {}, |_| {});
}

#[cfg(target_os = "linux")]
fn id_of(flag: &str, user: &str) -> u32 {
    let out = Command::new("id")
        .args([flag, user])
        .output()
        .expect("run id");
    assert!(out.status.success(), "unknown host user {user}");
    String::from_utf8(out.stdout)
        .expect("utf-8 id output")
        .trim()
        .parse()
        .expect("numeric id")
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires root; run with just test-privileged"]
fn privileged_worker_exits_when_root_daemon_is_killed() {
    assert_eq!(rustix::process::geteuid().as_raw(), 0);
    let host_user = std::env::var("ZERODECK_TEST_HOST_USER")
        .expect("ZERODECK_TEST_HOST_USER names the unprivileged host user");

    let home = TempHome::new("pdeathsig-root");
    // The worker drops to the host user, so it writes its pid where that user owns the directory.
    let run_dir = home.0.join("run");
    std::fs::create_dir_all(&run_dir).expect("create run dir");
    std::os::unix::fs::chown(
        &run_dir,
        Some(id_of("-u", &host_user)),
        Some(id_of("-g", &host_user)),
    )
    .expect("chown run dir to the host user");

    assert_worker_exits_when_daemon_is_killed(
        &home,
        &run_dir,
        |cmd| {
            cmd.env("DECKY_USER", &host_user);
        },
        |pid| {
            let status =
                std::fs::read_to_string(format!("/proc/{pid}/status")).expect("read worker status");
            let uids = status
                .lines()
                .find_map(|line| line.strip_prefix("Uid:"))
                .expect("Uid: line in /proc/<pid>/status");
            assert!(
                uids.split_whitespace().all(|uid| uid != "0"),
                "worker kept root ids after the drop: Uid:{uids}"
            );
            let gids = status
                .lines()
                .find_map(|line| line.strip_prefix("Gid:"))
                .expect("Gid: line in /proc/<pid>/status");
            assert!(
                gids.split_whitespace().all(|gid| gid != "0"),
                "worker kept root group ids after the drop: Gid:{gids}"
            );
        },
    );
}

fn build_sideload_zip(uninstall_marker: &std::path::Path) -> Vec<u8> {
    use std::io::{Cursor, Write};
    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    let mut buf = Cursor::new(Vec::new());
    {
        let mut writer = ZipWriter::new(&mut buf);
        let opts = SimpleFileOptions::default();

        writer
            .start_file("plugin.json", opts)
            .expect("start plugin.json");
        writer
            .write_all(br#"{"name":"SideloadPlug","author":"Tester","api_version":1}"#)
            .expect("write plugin.json");

        writer
            .start_file("package.json", opts)
            .expect("start package.json");
        writer
            .write_all(br#"{"version":"1.2.3","type":"module"}"#)
            .expect("write package.json");

        writer
            .start_file("assets/hello.txt", opts)
            .expect("start assets/hello.txt");
        writer
            .write_all(b"hello from plugin asset")
            .expect("write assets/hello.txt");

        writer.start_file("main.py", opts).expect("start main.py");
        let main_py = format!(
            r#"from pathlib import Path

class Plugin:
    async def _main(self):
        pass

    async def _unload(self):
        pass

    async def _uninstall(self):
        Path({marker:?}).write_text("uninstalled", encoding="utf-8")

    async def greet(self, who):
        return f"hi {{who}}"
"#,
            marker = uninstall_marker.display().to_string(),
        );
        writer.write_all(main_py.as_bytes()).expect("write main.py");

        writer.finish().expect("finish zip");
    }
    buf.into_inner()
}

async fn ws_next_json<S>(ws: &mut S, timeout: Duration) -> serde_json::Value
where
    S: futures_util::Stream<
            Item = Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin,
{
    use futures_util::StreamExt;
    let frame = tokio::time::timeout(timeout, ws.next())
        .await
        .expect("ws frame timeout")
        .expect("ws open")
        .expect("ws frame");
    serde_json::from_str(frame.to_text().expect("text")).expect("json")
}

async fn ws_call_reply<S>(
    ws: &mut S,
    id: u64,
    route: &str,
    args: serde_json::Value,
    timeout: Duration,
) -> serde_json::Value
where
    S: futures_util::Sink<
            tokio_tungstenite::tungstenite::Message,
            Error = tokio_tungstenite::tungstenite::Error,
        > + futures_util::Stream<
            Item = Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin,
{
    use futures_util::SinkExt;
    use serde_json::json;
    use tokio_tungstenite::tungstenite::Message;

    ws.send(Message::Text(
        json!({ "type": 0, "id": id, "route": route, "args": args })
            .to_string()
            .into(),
    ))
    .await
    .expect("send ws call");

    loop {
        let parsed = ws_next_json(ws, timeout).await;
        if parsed["type"] == json!(1) && parsed["id"] == json!(id) {
            return parsed;
        }
    }
}

async fn fetch_auth_token(client: &reqwest::Client, port: u16) -> String {
    client
        .get(format!("http://127.0.0.1:{port}/auth/token"))
        .send()
        .await
        .expect("GET /auth/token")
        .text()
        .await
        .expect("token text")
        .trim()
        .to_owned()
}

async fn ws_install_zip<S>(
    ws: &mut S,
    zip_path: &std::path::Path,
    timeout: Duration,
) -> serde_json::Value
where
    S: futures_util::Sink<
            tokio_tungstenite::tungstenite::Message,
            Error = tokio_tungstenite::tungstenite::Error,
        > + futures_util::Stream<
            Item = Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin,
{
    use futures_util::SinkExt;
    use serde_json::{Value, json};
    use tokio_tungstenite::tungstenite::Message;

    let file_uri = format!("file://{}", zip_path.display());
    ws.send(Message::Text(
        json!({
            "type": 0,
            "id": 1,
            "route": "loader/install_plugin",
            "args": [{ "url_or_path": file_uri, "user_confirmed": true }]
        })
        .to_string()
        .into(),
    ))
    .await
    .expect("send loader/install_plugin");

    let (mut install_reply, mut saw_plugins_changed): (Option<Value>, bool) = (None, false);
    while install_reply.is_none() || !saw_plugins_changed {
        let parsed = ws_next_json(ws, timeout).await;
        if parsed["type"] == json!(1) && parsed["id"] == json!(1) {
            install_reply = Some(parsed["result"].clone());
        } else if parsed["type"] == json!(5) && parsed["event"] == json!("loader/plugins_changed") {
            saw_plugins_changed = true;
        }
    }
    install_reply.expect("install reply")
}

#[tokio::test]
async fn daemon_serves_frontend_sideloads_plugin_assets_and_uninstalls() {
    use serde_json::json;

    let home = TempHome::new("full-e2e");
    let frontend_dir = home.0.join("frontend-dist");
    std::fs::create_dir_all(&frontend_dir).expect("create frontend dir");
    std::fs::write(
        frontend_dir.join("index.js"),
        "console.log('zerodeck-frontend');\n",
    )
    .expect("write frontend index.js");

    let uninstall_marker = home.0.join("plugin.uninstalled");
    let zip_path = home.0.join("sideload-plug.zip");
    std::fs::write(&zip_path, build_sideload_zip(&uninstall_marker)).expect("write sideload zip");

    let py_host = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../hosts/python")
        .canonicalize()
        .expect("resolve hosts/python");
    let port = free_port("127.0.0.1");
    let mut daemon = Daemon(
        daemon_command(&home, "127.0.0.1", port)
            .env("PYTHONPATH", py_host)
            .env("ZERODECK_FRONTEND_DIR", &frontend_dir)
            .spawn()
            .expect("spawn zerodeckd"),
    );

    wait_until_healthy(&home, "127.0.0.1", port);

    let client = reqwest::Client::new();
    let fe_resp = client
        .get(format!("http://127.0.0.1:{port}/frontend/index.js"))
        .send()
        .await
        .expect("GET /frontend/index.js");
    assert_eq!(fe_resp.status(), reqwest::StatusCode::OK);
    let ct = fe_resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(ct.contains("text/javascript"), "unexpected ct: {ct}");
    assert_eq!(
        fe_resp.text().await.expect("frontend body"),
        "console.log('zerodeck-frontend');\n"
    );

    let token = fetch_auth_token(&client, port).await;
    let (mut ws, _) =
        tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws?auth={token}"))
            .await
            .expect("connect ws");

    let outcome = ws_install_zip(&mut ws, &zip_path, Duration::from_secs(10)).await;
    assert_eq!(outcome["name"], json!("SideloadPlug"));
    assert_eq!(outcome["version"], json!("1.2.3"));
    assert_eq!(outcome["replaced_existing"], json!(false));

    let asset_url = format!("http://127.0.0.1:{port}/plugins/SideloadPlug/assets/hello.txt");
    let asset_resp = client.get(&asset_url).send().await.expect("GET asset");
    assert_eq!(asset_resp.status(), reqwest::StatusCode::OK);
    assert_eq!(
        asset_resp.text().await.expect("asset text"),
        "hello from plugin asset"
    );

    let call = ws_call_reply(
        &mut ws,
        2,
        "loader/call_plugin_method",
        json!(["SideloadPlug", "greet", "ZeroDeck"]),
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(call["result"], json!("hi ZeroDeck"));

    let uninstall = ws_call_reply(
        &mut ws,
        3,
        "loader/uninstall_plugin",
        json!(["SideloadPlug"]),
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(uninstall["result"], json!(true));
    assert_eq!(
        std::fs::read_to_string(&uninstall_marker).expect("marker"),
        "uninstalled"
    );
    assert_eq!(
        client.get(&asset_url).send().await.expect("404").status(),
        reqwest::StatusCode::NOT_FOUND
    );

    sigterm(&daemon);
    assert!(
        wait_for_exit(&mut daemon, Duration::from_secs(10)).success(),
        "daemon must exit cleanly"
    );
}

#[test]
fn refuses_non_loopback_bind_without_opt_in() {
    let home = TempHome::new("non-loopback");
    let port = free_port("127.0.0.1");
    let mut cmd = daemon_command(&home, "127.0.0.1", port);
    cmd.args(["--host", "0.0.0.0", "--port", &port.to_string()])
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn zerodeckd");
    let stderr = child.stderr.take().expect("piped stderr");
    let mut daemon = Daemon(child);

    let status = wait_for_exit(&mut daemon, Duration::from_secs(5));
    assert!(!status.success(), "daemon must refuse, got {status}");

    let mut output = String::new();
    std::io::Read::read_to_string(&mut { stderr }, &mut output).expect("read stderr");
    assert!(output.contains("--allow-non-loopback"), "stderr: {output}");
}

#[test]
fn check_config_accepts_env_opt_in_for_non_loopback_host() {
    let home = TempHome::new("opt-in-env");
    let status = daemon_command(&home, "0.0.0.0", 1)
        .env("ZERODECK_ALLOW_NON_LOOPBACK", "1")
        .arg("--check-config")
        .status()
        .expect("run --check-config");
    assert!(
        status.success(),
        "ZERODECK_ALLOW_NON_LOOPBACK=1 must be accepted"
    );

    let refused = daemon_command(&home, "0.0.0.0", 1)
        .arg("--check-config")
        .status()
        .expect("run --check-config");
    assert!(
        !refused.success(),
        "--check-config must report the bind policy"
    );
}

#[test]
fn check_config_refuses_non_loopback_host_when_env_opt_in_is_falsy() {
    let home = TempHome::new("opt-in-env-falsy");
    let output = daemon_command(&home, "127.0.0.1", 1)
        .env("ZERODECK_ALLOW_NON_LOOPBACK", "0")
        .args(["--check-config", "--host", "0.0.0.0"])
        .stderr(Stdio::piped())
        .output()
        .expect("run --check-config");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "ZERODECK_ALLOW_NON_LOOPBACK=0 must not opt in"
    );
    assert!(stderr.contains("refusing to bind"), "stderr: {stderr}");
}

/// Copies `zerodeck_py_host` (without `__pycache__`) under `home`: a root worker importing it from
/// the bind-mounted checkout would leave root-owned bytecode there.
#[cfg(target_os = "linux")]
fn copy_python_host(home: &TempHome) -> PathBuf {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../hosts/python/zerodeck_py_host");
    let root = home.0.join("python");
    let dest = root.join("zerodeck_py_host");
    std::fs::create_dir_all(&dest).expect("create python host copy");
    for entry in std::fs::read_dir(&src).expect("read hosts/python/zerodeck_py_host") {
        let path = entry.expect("dir entry").path();
        if path.extension().is_some_and(|ext| ext == "py") {
            std::fs::copy(&path, dest.join(path.file_name().expect("file name")))
                .expect("copy python host module");
        }
    }
    root
}

/// Reads the `loader/get_plugins` entry for `name` over a fresh WebSocket session.
#[cfg(target_os = "linux")]
async fn get_plugin_entry(port: u16, name: &str) -> serde_json::Value {
    let token = fetch_auth_token(&reqwest::Client::new(), port).await;
    let (mut ws, _) =
        tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws?auth={token}"))
            .await
            .expect("connect ws");
    let reply = ws_call_reply(
        &mut ws,
        1,
        "loader/get_plugins",
        serde_json::json!([]),
        Duration::from_secs(5),
    )
    .await;
    reply["result"]
        .as_array()
        .expect("get_plugins returns an array")
        .iter()
        .find(|entry| entry["name"] == name)
        .cloned()
        .unwrap_or_else(|| panic!("plugin {name} missing from {reply}"))
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires root; run with just test-privileged"]
async fn privileged_root_plugin_in_user_owned_dir_is_blocked() {
    assert_eq!(rustix::process::geteuid().as_raw(), 0);
    let host_user = std::env::var("ZERODECK_TEST_HOST_USER")
        .expect("ZERODECK_TEST_HOST_USER names the unprivileged host user");

    let home = TempHome::new("root-plugin-owner");
    let plug_dir = home.0.join("plugins").join("root_plug");
    std::fs::create_dir_all(&plug_dir).expect("create plugin dir");
    std::fs::write(
        plug_dir.join("plugin.json"),
        r#"{"name":"RootPlug","author":"Tester","flags":["root"],"api_version":1}"#,
    )
    .expect("write plugin.json");
    std::fs::write(
        plug_dir.join("main.py"),
        "class Plugin:\n    async def _main(self):\n        pass\n",
    )
    .expect("write main.py");
    let chown = Command::new("chown")
        .args(["-R", &host_user])
        .arg(&plug_dir)
        .status()
        .expect("run chown");
    assert!(chown.success(), "chown the plugin to the host user");

    let py_host = copy_python_host(&home);
    let start = |port: u16, opt_in: bool| {
        let mut cmd = daemon_command(&home, "127.0.0.1", port);
        cmd.env("PYTHONPATH", &py_host)
            .env("DECKY_USER", &host_user);
        if opt_in {
            cmd.env("ZERODECK_ALLOW_UNSAFE_ROOT_PLUGINS", "1");
        }
        Daemon(cmd.spawn().expect("spawn zerodeckd"))
    };
    let assert_blocked = |entry: &serde_json::Value| {
        assert_eq!(entry["state"], "Failed", "{entry}");
        let reason = entry["failure_reason"].as_str().unwrap_or_default();
        assert!(
            reason.contains(&plug_dir.display().to_string()) && reason.contains("not root"),
            "failure_reason must name the plugin path: {entry}"
        );
    };

    let port = free_port("127.0.0.1");
    let mut daemon = start(port, false);
    wait_until_healthy(&home, "127.0.0.1", port);
    assert_blocked(&get_plugin_entry(port, "RootPlug").await);
    sigterm(&daemon);
    assert!(wait_for_exit(&mut daemon, Duration::from_secs(10)).success());

    // settings/ belongs to the host user, so loader.json must not be able to grant root.
    std::fs::create_dir_all(home.0.join("settings")).expect("create settings dir");
    std::fs::write(
        home.0.join("settings").join("loader.json"),
        r#"{"developer":{"allow_unsafe_root_plugins":true}}"#,
    )
    .expect("write loader.json");
    let port = free_port("127.0.0.1");
    let mut daemon = start(port, false);
    wait_until_healthy(&home, "127.0.0.1", port);
    assert_blocked(&get_plugin_entry(port, "RootPlug").await);
    sigterm(&daemon);
    assert!(wait_for_exit(&mut daemon, Duration::from_secs(10)).success());

    let port = free_port("127.0.0.1");
    let mut daemon = start(port, true);
    wait_until_healthy(&home, "127.0.0.1", port);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let entry = get_plugin_entry(port, "RootPlug").await;
        if entry["state"] == "Running" {
            assert_eq!(entry["failure_reason"], serde_json::Value::Null, "{entry}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "opted-in root plugin never reached Running: {entry}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    sigterm(&daemon);
    assert!(wait_for_exit(&mut daemon, Duration::from_secs(10)).success());
}
