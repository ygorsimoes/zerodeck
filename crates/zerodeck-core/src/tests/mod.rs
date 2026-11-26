//! Unit tests for `zerodeck-core`, one submodule per area. Shared fixtures live here.

use crate::{
    CoreError, InstallSource, SettingsManager, discover_plugins, extract_zip_safely,
    verify_sha256_policy,
};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Cursor, Write};
use std::path::PathBuf;
use uuid::Uuid;
use zerodeck_protocol::PluginLoadType;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

mod archive;
mod discovery;
mod filepicker;
mod http_policy;
mod installer;
mod manager;
mod settings;
mod watcher;

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!("zerodeck-{label}-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp test dir");
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn zip_with_files(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buf = Cursor::new(Vec::new());
    {
        let mut writer = ZipWriter::new(&mut buf);
        let stored =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, data) in files {
            writer.start_file(*name, stored).expect("start entry");
            writer.write_all(data).expect("write entry");
        }
        writer.finish().expect("finish zip");
    }
    buf.into_inner()
}

fn sorted_names(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("read dir")
        .map(|e| {
            e.expect("dir entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

fn manager_test_paths(root: &std::path::Path) -> zerodeck_platform::PlatformPaths {
    let root_str = root.display().to_string();
    let home_str = root.join("home").display().to_string();
    fs::create_dir_all(&home_str).expect("create home");
    let lookup = std::collections::HashMap::from([
        ("UNPRIVILEGED_PATH", root_str.as_str()),
        ("PRIVILEGED_PATH", root_str.as_str()),
        ("DECKY_USER_HOME", home_str.as_str()),
        ("HOME", home_str.as_str()),
    ]);
    zerodeck_platform::PlatformPaths::from_lookup(|k| lookup.get(k).map(|v| (*v).to_owned()))
}

fn python_host_module_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../hosts/python")
        .canonicalize()
        .expect("resolve hosts/python")
        .display()
        .to_string()
}

async fn read_http_request_raw(sock: &mut tokio::net::TcpStream) -> String {
    use tokio::io::AsyncReadExt;

    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 512];
    while let Ok(n) = sock.read(&mut chunk).await {
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let header_end = pos + 4;
            let headers_str = String::from_utf8_lossy(&buf[..header_end]);
            let content_len = headers_str
                .lines()
                .find_map(|line| {
                    let (k, v) = line.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            if buf.len() >= header_end + content_len {
                break;
            }
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn spawn_installer_mock_on(
    listener: tokio::net::TcpListener,
    routes: Vec<(String, Vec<u8>)>,
) -> tokio::task::JoinHandle<()> {
    use tokio::io::AsyncWriteExt;

    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let req = read_http_request_raw(&mut sock).await;
            let req_path = req
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("");

            if let Some((_, body)) = routes.iter().find(|(path, _)| path == req_path) {
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(header.as_bytes()).await;
                let _ = sock.write_all(body).await;
            } else {
                let _ = sock
                    .write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await;
            }
        }
    })
}

#[cfg(unix)]
fn test_manager_open(
    paths: &zerodeck_platform::PlatformPaths,
) -> std::sync::Arc<crate::PluginManager> {
    std::sync::Arc::new(
        crate::PluginManager::open(
            paths.clone(),
            zerodeck_plugin_host::SupervisorConfig {
                python_host: zerodeck_plugin_host::PythonHostConfig {
                    python_bin: "python3".to_owned(),
                    host_module_path: Some(python_host_module_path()),
                },
                ..zerodeck_plugin_host::SupervisorConfig::default()
            },
            "0.1.0",
        )
        .expect("open manager"),
    )
}

#[cfg(unix)]
async fn install_local_zip_with_hash(
    manager: &crate::PluginManager,
    home: &std::path::Path,
    filename: &str,
    zip_bytes: &[u8],
) -> crate::InstallOutcome {
    let zip_path = home.join(filename);
    fs::write(&zip_path, zip_bytes).expect("write local zip");
    manager
        .install_plugin(crate::InstallPluginRequest {
            source: crate::InstallPluginSource::LocalZip {
                path: zip_path,
                sha256: Some(hex::encode(Sha256::digest(zip_bytes))),
                user_confirmed: false,
            },
            directory_hint: None,
            expected_name: None,
            expected_version: None,
        })
        .await
        .expect("install local zip")
}

#[cfg(unix)]
async fn wait_for_loader_event<F>(
    events: &mut tokio::sync::broadcast::Receiver<crate::LoaderEvent>,
    timeout: std::time::Duration,
    mut pred: F,
) where
    F: FnMut(&crate::LoaderEvent) -> bool,
{
    let matched = tokio::time::timeout(timeout, async {
        loop {
            match events.recv().await {
                Ok(ev) if pred(&ev) => return,
                Ok(_) => {}
                Err(err) => panic!("loader event channel error: {err}"),
            }
        }
    })
    .await;
    assert!(
        matched.is_ok(),
        "timed out waiting for expected LoaderEvent"
    );
}

#[cfg(unix)]
fn write_watcher_plugin(dir: &std::path::Path, name: &str, flags: &[&str], tag: &str) {
    fs::create_dir_all(dir.join("dist")).expect("create dist");
    fs::create_dir_all(dir.join("py_modules")).expect("create py_modules");
    let flags_json = serde_json::json!(flags);
    fs::write(
        dir.join("plugin.json"),
        format!(r#"{{"name":"{name}","author":"Alice","flags":{flags_json},"api_version":1}}"#),
    )
    .expect("write plugin.json");
    fs::write(
        dir.join("package.json"),
        r#"{"version":"1.0.0","type":"module"}"#,
    )
    .expect("write package.json");
    fs::write(
        dir.join("main.py"),
        format!("class Plugin:\n    async def version_tag(self):\n        return '{tag}'\n"),
    )
    .expect("write main.py");
    fs::write(dir.join("dist/index.js"), b"export const v = 1;").expect("write dist");
}
