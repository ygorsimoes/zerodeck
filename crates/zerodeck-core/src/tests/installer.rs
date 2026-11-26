//! Staged installs: remote and local archives, `remote_binary`, limits.

use super::*;

async fn spawn_installer_mock(
    routes: Vec<(String, Vec<u8>)>,
) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind installer mock server");
    let port = listener.local_addr().expect("mock addr").port();
    let handle = spawn_installer_mock_on(listener, routes);
    (port, handle)
}

fn make_remote_binary_zip(port: u16, bin_name: &str, bin_sha: &str) -> (Vec<u8>, String) {
    let pkg = format!(
        r#"{{"version":"2.0.0","remote_binary":[{{"name":"{bin_name}","url":"http://127.0.0.1:{port}/bin/helper","sha256hash":"{bin_sha}"}}]}}"#
    );
    let bytes = zip_with_files(&[
        (
            "plugin.json",
            br#"{"name":"Demo","author":"Alice","api_version":1}"#,
        ),
        ("package.json", pkg.as_bytes()),
        ("main.py", b"VERSION = 'v2'\n"),
    ]);
    let sha = hex::encode(Sha256::digest(&bytes));
    (bytes, sha)
}

fn write_local_zip_req(
    home: &std::path::Path,
    filename: &str,
    files: &[(&str, &[u8])],
) -> crate::InstallPluginRequest {
    let zip_path = home.join(filename);
    fs::write(&zip_path, zip_with_files(files)).expect("write local test zip");
    crate::InstallPluginRequest {
        source: crate::InstallPluginSource::LocalZip {
            path: zip_path,
            sha256: None,
            user_confirmed: true,
        },
        directory_hint: None,
        expected_name: None,
        expected_version: None,
    }
}

/// A passive plugin (no `main.py`) with one 256 MiB, highly compressible asset: extracting
/// it takes long enough that doing it on the async thread stalls the runtime.
fn big_asset_plugin_zip() -> Vec<u8> {
    let big = vec![0u8; 256 * 1024 * 1024];
    let mut buf = Cursor::new(Vec::new());
    {
        let mut writer = ZipWriter::new(&mut buf);
        let deflated =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        writer
            .start_file("plugin.json", deflated)
            .expect("start plugin.json");
        writer
            .write_all(br#"{"name":"BigAssets","author":"Alice","api_version":1}"#)
            .expect("write plugin.json");
        writer
            .start_file("assets/big.bin", deflated)
            .expect("start big.bin");
        writer.write_all(&big).expect("write big.bin");
        writer.finish().expect("finish zip");
    }
    buf.into_inner()
}

/// Runs `fut` while a 10 ms ticker on the same (current-thread) runtime records the longest
/// gap between its ticks; returns the output and that gap in milliseconds.
async fn with_max_stall_ms<F: std::future::Future>(fut: F) -> (F::Output, u64) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    let running = Arc::new(AtomicBool::new(true));
    let max_gap_ms = Arc::new(AtomicU64::new(0));
    let ticker = {
        let running = Arc::clone(&running);
        let max_gap_ms = Arc::clone(&max_gap_ms);
        tokio::spawn(async move {
            let mut last = Instant::now();
            while running.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(10)).await;
                let now = Instant::now();
                let gap = u64::try_from(now.duration_since(last).as_millis()).unwrap_or(u64::MAX);
                max_gap_ms.fetch_max(gap, Ordering::SeqCst);
                last = now;
            }
        })
    };
    tokio::task::yield_now().await;
    let output = fut.await;
    running.store(false, Ordering::SeqCst);
    ticker.await.expect("ticker task");
    (output, max_gap_ms.load(Ordering::SeqCst))
}

#[tokio::test]
async fn installer_unwraps_top_level_wrapper_and_verifies_expected_version() {
    let dir = TestDir::new("inst-wrapper");
    let paths = manager_test_paths(&dir.0);
    let zip_bytes = zip_with_files(&[
        (
            "demo-v1.2.3/plugin.json",
            br#"{"name":"Demo","author":"Alice","api_version":1}"#,
        ),
        ("demo-v1.2.3/package.json", br#"{"version":"1.2.3"}"#),
        ("demo-v1.2.3/main.py", b"class Plugin: pass\n"),
        ("__MACOSX/demo-v1.2.3/._plugin.json", b"appledouble"),
    ]);
    let zip_sha = hex::encode(Sha256::digest(&zip_bytes));

    let (port, server) = spawn_installer_mock(vec![("/demo.zip".to_owned(), zip_bytes)]).await;
    let client = crate::OutboundHttpPolicy::for_loopback_ports_for_testing(vec![port])
        .build_checked_client()
        .expect("build checked client");
    let limits = crate::InstallerLimits::default();
    let url = format!("http://127.0.0.1:{port}/demo.zip");

    let wrong_version_req = crate::InstallPluginRequest {
        source: crate::InstallPluginSource::RemoteZip {
            url: url.clone(),
            sha256: Some(zip_sha.clone()),
        },
        directory_hint: None,
        expected_name: Some("Demo".to_owned()),
        expected_version: Some("9.9.9".to_owned()),
    };
    let err_ver = crate::prepare_staged_plugin(&paths, &client, &limits, &wrong_version_req, false)
        .await
        .expect_err("expected_version mismatch must fail");
    assert!(matches!(
        err_ver,
        CoreError::ManifestMismatch {
            field: "version",
            ..
        }
    ));
    assert_eq!(sorted_names(&paths.plugins_dir()), Vec::<String>::new());

    let wrong_name_req = crate::InstallPluginRequest {
        source: crate::InstallPluginSource::RemoteZip {
            url: url.clone(),
            sha256: Some(zip_sha.clone()),
        },
        directory_hint: None,
        expected_name: Some("Other".to_owned()),
        expected_version: Some("1.2.3".to_owned()),
    };
    let err_name = crate::prepare_staged_plugin(&paths, &client, &limits, &wrong_name_req, false)
        .await
        .expect_err("expected_name mismatch must fail");
    assert!(matches!(
        err_name,
        CoreError::ManifestMismatch { field: "name", .. }
    ));
    assert_eq!(sorted_names(&paths.plugins_dir()), Vec::<String>::new());

    let ok_req = crate::InstallPluginRequest {
        source: crate::InstallPluginSource::RemoteZip {
            url,
            sha256: Some(zip_sha.clone()),
        },
        directory_hint: None,
        expected_name: Some("Demo".to_owned()),
        expected_version: Some("1.2.3".to_owned()),
    };
    let staged = crate::prepare_staged_plugin(&paths, &client, &limits, &ok_req, false)
        .await
        .expect("matching wrapper archive must stage cleanly");
    assert_eq!(staged.suggested_directory(), "demo-v1.2.3");

    let target_dir = paths.plugins_dir().join(staged.suggested_directory());
    let outcome = crate::commit_staged_plugin(staged, &target_dir).expect("commit staged plugin");
    assert_eq!(outcome.name, "Demo");
    assert_eq!(outcome.directory, "demo-v1.2.3");
    assert_eq!(outcome.version.as_deref(), Some("1.2.3"));
    assert_eq!(outcome.sha256, zip_sha);
    assert!(!outcome.replaced_existing);

    assert!(target_dir.join("plugin.json").is_file());
    assert!(target_dir.join("package.json").is_file());
    assert!(target_dir.join("main.py").is_file());
    assert!(!target_dir.join("__MACOSX").exists());
    assert_eq!(
        sorted_names(&paths.plugins_dir()),
        vec!["demo-v1.2.3".to_owned()]
    );

    server.abort();
}

#[tokio::test]
async fn installer_stages_remote_binary_and_preserves_existing_on_failure() {
    let dir = TestDir::new("inst-remote-bin");
    let paths = manager_test_paths(&dir.0);
    let target_dir = paths.plugins_dir().join("demo");
    fs::create_dir_all(&target_dir).expect("create v1 plugin dir");
    fs::write(
        target_dir.join("plugin.json"),
        r#"{"name":"Demo","author":"Alice","api_version":1}"#,
    )
    .expect("write v1 plugin.json");
    fs::write(target_dir.join("main.py"), "VERSION = 'v1'\n").expect("write v1 main.py");

    let bin_payload = b"#!/bin/sh\necho helper-v2\n";
    let bin_sha = hex::encode(Sha256::digest(bin_payload));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let port = listener.local_addr().expect("mock addr").port();

    let (bad_zip, bad_zip_sha) = make_remote_binary_zip(port, "helper", &"0".repeat(64));
    let (good_zip, good_zip_sha) = make_remote_binary_zip(port, "helper", &bin_sha);
    let server = spawn_installer_mock_on(
        listener,
        vec![
            ("/v2-bad.zip".to_owned(), bad_zip),
            ("/v2-good.zip".to_owned(), good_zip),
            ("/bin/helper".to_owned(), bin_payload.to_vec()),
        ],
    );

    let client = crate::OutboundHttpPolicy::for_loopback_ports_for_testing(vec![port])
        .build_checked_client()
        .expect("build checked client");
    let limits = crate::InstallerLimits::default();

    let bad_req = crate::InstallPluginRequest {
        source: crate::InstallPluginSource::RemoteZip {
            url: format!("http://127.0.0.1:{port}/v2-bad.zip"),
            sha256: Some(bad_zip_sha),
        },
        directory_hint: None,
        expected_name: None,
        expected_version: None,
    };
    let err = crate::prepare_staged_plugin(&paths, &client, &limits, &bad_req, false)
        .await
        .expect_err("bad remote_binary sha256 must fail");
    assert!(matches!(err, CoreError::Sha256Mismatch { .. }));
    assert_eq!(
        fs::read_to_string(target_dir.join("main.py")).expect("read v1 main.py"),
        "VERSION = 'v1'\n"
    );
    assert!(!target_dir.join("bin").exists());
    assert_eq!(sorted_names(&paths.plugins_dir()), vec!["demo".to_owned()]);

    let good_req = crate::InstallPluginRequest {
        source: crate::InstallPluginSource::RemoteZip {
            url: format!("http://127.0.0.1:{port}/v2-good.zip"),
            sha256: Some(good_zip_sha),
        },
        directory_hint: None,
        expected_name: None,
        expected_version: Some("2.0.0".to_owned()),
    };
    let staged = crate::prepare_staged_plugin(&paths, &client, &limits, &good_req, false)
        .await
        .expect("valid remote_binary upgrade must stage");
    let outcome =
        crate::commit_staged_plugin(staged, &target_dir).expect("commit remote_binary upgrade");
    assert!(outcome.replaced_existing);
    assert_eq!(
        fs::read_to_string(target_dir.join("main.py")).expect("read v2 main.py"),
        "VERSION = 'v2'\n"
    );

    let bin_path = target_dir.join("bin").join("helper");
    assert_eq!(
        fs::read(&bin_path).expect("read helper binary"),
        bin_payload
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&bin_path)
            .expect("stat helper binary")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755, "remote_binary must be executable 0o755");
    }
    assert_eq!(sorted_names(&paths.plugins_dir()), vec!["demo".to_owned()]);

    server.abort();
}

#[tokio::test]
async fn installer_enforces_sha256_policy_and_file_uri_sideload() {
    let dir = TestDir::new("inst-policy");
    let paths = manager_test_paths(&dir.0);
    let local_zip = paths.user_home().join("p.zip");
    let zip_bytes = zip_with_files(&[("plugin.json", br#"{"name":"LocalDemo","author":"Bob"}"#)]);
    fs::write(&local_zip, &zip_bytes).expect("write local zip");

    let (port, server) = spawn_installer_mock(vec![("/p.zip".to_owned(), zip_bytes)]).await;
    let client = crate::OutboundHttpPolicy::for_loopback_ports_for_testing(vec![port])
        .build_checked_client()
        .expect("build checked client");
    let limits = crate::InstallerLimits::default();

    // RemoteZip without SHA-256 fails closed unless allow_unverified == true.
    let remote_no_hash = crate::InstallPluginRequest {
        source: crate::parse_install_source(
            &format!("http://127.0.0.1:{port}/p.zip"),
            None,
            Some(&serde_json::json!(0)),
            false,
        )
        .expect("parse remote install source"),
        directory_hint: None,
        expected_name: None,
        expected_version: None,
    };
    let err_missing =
        crate::prepare_staged_plugin(&paths, &client, &limits, &remote_no_hash, false)
            .await
            .expect_err("unhashed remote install must fail closed");
    assert!(matches!(err_missing, CoreError::MissingSha256));

    let staged_unverified =
        crate::prepare_staged_plugin(&paths, &client, &limits, &remote_no_hash, true)
            .await
            .expect("allow_unverified permits unhashed remote install");
    drop(staged_unverified);
    assert_eq!(
        sorted_names(&paths.plugins_dir()),
        Vec::<String>::new(),
        "dropping uncommitted staged install must remove staging dir"
    );

    // file://<home>/p.zip parses to LocalZip and requires user_confirmed == true without hash.
    let file_uri = url::Url::from_file_path(&local_zip)
        .expect("file URI")
        .to_string();
    let unconfirmed_src =
        crate::parse_install_source(&file_uri, None, Some(&serde_json::json!("local")), false)
            .expect("parse file URI");
    assert_eq!(
        unconfirmed_src,
        crate::InstallPluginSource::LocalZip {
            path: local_zip.clone(),
            sha256: None,
            user_confirmed: false,
        }
    );

    let unconfirmed_req = crate::InstallPluginRequest {
        source: unconfirmed_src,
        directory_hint: None,
        expected_name: None,
        expected_version: None,
    };
    let err_confirm =
        crate::prepare_staged_plugin(&paths, &client, &limits, &unconfirmed_req, false)
            .await
            .expect_err("unconfirmed local zip without hash must require confirmation");
    assert!(matches!(err_confirm, CoreError::ConfirmationRequired));

    let confirmed_req = crate::InstallPluginRequest {
        source: crate::parse_install_source(&file_uri, None, None, true)
            .expect("parse confirmed file URI"),
        directory_hint: None,
        expected_name: None,
        expected_version: None,
    };
    let staged_local =
        crate::prepare_staged_plugin(&paths, &client, &limits, &confirmed_req, false)
            .await
            .expect("confirmed local sideload must stage");
    let target_dir = paths.plugins_dir().join(staged_local.suggested_directory());
    let outcome = crate::commit_staged_plugin(staged_local, &target_dir)
        .expect("commit confirmed local sideload");
    assert_eq!(outcome.name, "LocalDemo");
    assert!(target_dir.join("plugin.json").is_file());

    // Outside file:///etc/passwd must fail PathOutsideRoots even when user_confirmed == true.
    let escape_req = crate::InstallPluginRequest {
        source: crate::parse_install_source("file:///etc/passwd", None, None, true)
            .expect("parse /etc/passwd file URI"),
        directory_hint: None,
        expected_name: None,
        expected_version: None,
    };
    let err_escape = crate::prepare_staged_plugin(&paths, &client, &limits, &escape_req, false)
        .await
        .expect_err("file:///etc/passwd must fail PathOutsideRoots");
    assert!(matches!(
        err_escape,
        CoreError::Platform(zerodeck_platform::PlatformError::PathOutsideRoots { .. })
    ));

    server.abort();
}

#[test]
fn installer_sanitizes_directory_names_and_validates_install_sources() {
    assert_eq!(
        crate::sanitize_directory_name("My Cool Plugin!"),
        "My-Cool-Plugin"
    );
    assert_eq!(crate::sanitize_directory_name("demo-v1.2.3"), "demo-v1.2.3");
    assert_eq!(crate::sanitize_directory_name(".hidden"), "hidden");
    assert_eq!(crate::sanitize_directory_name(".."), "plugin");
    assert_eq!(
        crate::sanitize_directory_name("../etc/passwd"),
        "etc-passwd"
    );
    assert_eq!(crate::sanitize_directory_name(""), "plugin");

    for (src, kind) in [
        ("file:///tmp/a.zip", Some(serde_json::json!("remote"))),
        ("/tmp/a.zip", Some(serde_json::json!("remote"))),
        (
            "https://example.com/a.zip",
            Some(serde_json::json!("local")),
        ),
        ("https://example.com/a.zip", Some(serde_json::json!(3))),
        ("ftp://example.com/a.zip", None),
        ("file://evil.example/etc/passwd", None),
        ("", None),
    ] {
        let err = crate::parse_install_source(src, None, kind.as_ref(), true)
            .expect_err(&format!("must reject `{src}` with {kind:?}"));
        assert!(matches!(err, CoreError::InvalidInstallSource(_)));
    }
}

#[tokio::test]
async fn installer_enforces_limits_unsafe_binary_names_and_manifest_edge_cases() {
    let dir = TestDir::new("inst-edge");
    let paths = manager_test_paths(&dir.0);
    let client = crate::OutboundHttpPolicy::production()
        .build_checked_client()
        .expect("prod client");
    let default_limits = crate::InstallerLimits::default();

    // expected_version succeeds when package.json omits `version`, and directory_hint sanitizes.
    let mut no_ver_req = write_local_zip_req(
        paths.user_home(),
        "no-ver.zip",
        &[
            ("plugin.json", br#"{"name":"NoVer","author":"Bob"}"#),
            ("package.json", br#"{"type":"module"}"#),
        ],
    );
    no_ver_req.directory_hint = Some("Custom Dir!".to_owned());
    no_ver_req.expected_version = Some("1.0.0".to_owned());

    let staged_no_ver =
        crate::prepare_staged_plugin(&paths, &client, &default_limits, &no_ver_req, false)
            .await
            .expect("omitted package.json version must not fail expected_version");
    assert_eq!(staged_no_ver.suggested_directory(), "Custom-Dir");

    // Invalid commit target dir cleans up staging on failure.
    let err_commit = crate::commit_staged_plugin(staged_no_ver, &paths.plugins_dir().join(".bad"))
        .expect_err("dot-prefixed commit target must fail");
    assert!(matches!(err_commit, CoreError::InvalidInstallSource(_)));
    assert_eq!(sorted_names(&paths.plugins_dir()), Vec::<String>::new());

    let bad_hint_req = crate::InstallPluginRequest {
        directory_hint: Some("../escape".to_owned()),
        ..no_ver_req.clone()
    };
    assert!(matches!(
        crate::prepare_staged_plugin(&paths, &client, &default_limits, &bad_hint_req, false).await,
        Err(CoreError::InvalidInstallSource(_))
    ));

    let tight_limits = crate::InstallerLimits {
        max_zip_download_bytes: 10,
        max_remote_binary_bytes: 10,
        max_redirects: 5,
    };
    assert!(matches!(
        crate::prepare_staged_plugin(&paths, &client, &tight_limits, &no_ver_req, false).await,
        Err(CoreError::DownloadSizeExceeded { max_bytes: 10 })
    ));

    let bad_bin_req = write_local_zip_req(
            paths.user_home(),
            "bad-bin.zip",
            &[
                ("plugin.json", br#"{"name":"BadBin","author":"Bob"}"#),
                (
                    "package.json",
                    br#"{"remote_binary":[{"name":"../evil","url":"https://example.com/bin","sha256hash":"deadbeef"}]}"#,
                ),
            ],
        );
    assert!(matches!(
        crate::prepare_staged_plugin(&paths, &client, &default_limits, &bad_bin_req, false).await,
        Err(CoreError::Platform(
            zerodeck_platform::PlatformError::PathEscape { .. }
        ))
    ));

    let deep_req = write_local_zip_req(
        paths.user_home(),
        "deep.zip",
        &[("a/b/plugin.json", br#"{"name":"Deep","author":"Bob"}"#)],
    );
    assert!(matches!(
        crate::prepare_staged_plugin(&paths, &client, &default_limits, &deep_req, false).await,
        Err(CoreError::MissingPluginManifest)
    ));
}

#[tokio::test]
async fn installer_enforces_streaming_http_download_limits_and_http_errors() {
    use tokio::io::AsyncWriteExt;

    let dir = TestDir::new("inst-stream-limit");
    let paths = manager_test_paths(&dir.0);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind stream mock");
    let port = listener.local_addr().expect("mock addr").port();

    let server = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let req = read_http_request_raw(&mut sock).await;
            if req.starts_with("GET /chunked-huge ") {
                let _ = sock
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n10\r\n0123456789abcdef\r\n10\r\n0123456789abcdef\r\n0\r\n\r\n",
                        )
                        .await;
            } else {
                let _ = sock
                    .write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await;
            }
        }
    });

    let client = crate::OutboundHttpPolicy::for_loopback_ports_for_testing(vec![port])
        .build_checked_client()
        .expect("client");
    let limits = crate::InstallerLimits {
        max_zip_download_bytes: 20,
        max_remote_binary_bytes: 20,
        max_redirects: 5,
    };

    let chunked_req = crate::InstallPluginRequest {
        source: crate::InstallPluginSource::RemoteZip {
            url: format!("http://127.0.0.1:{port}/chunked-huge"),
            sha256: None,
        },
        directory_hint: None,
        expected_name: None,
        expected_version: None,
    };
    let err_chunked = crate::prepare_staged_plugin(&paths, &client, &limits, &chunked_req, true)
        .await
        .expect_err("chunked body exceeding limit must fail");
    assert!(matches!(
        err_chunked,
        CoreError::DownloadSizeExceeded { max_bytes: 20 }
    ));

    let missing_req = crate::InstallPluginRequest {
        source: crate::InstallPluginSource::RemoteZip {
            url: format!("http://127.0.0.1:{port}/missing.zip"),
            sha256: None,
        },
        directory_hint: None,
        expected_name: None,
        expected_version: None,
    };
    let err_404 = crate::prepare_staged_plugin(&paths, &client, &limits, &missing_req, true)
        .await
        .expect_err("404 response must fail HttpRequest");
    assert!(matches!(err_404, CoreError::HttpRequest(_)));

    server.abort();
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn install_does_not_starve_current_thread_runtime() {
    let dir = TestDir::new("install-starve");
    let paths = manager_test_paths(&dir.0);
    let manager = test_manager_open(&paths);
    let zip_bytes = big_asset_plugin_zip();

    let (outcome, gap) = with_max_stall_ms(install_local_zip_with_hash(
        &manager,
        &dir.0.join("home"),
        "big.zip",
        &zip_bytes,
    ))
    .await;
    assert_eq!(outcome.name, "BigAssets");
    assert!(
        gap < 250,
        "async runtime stalled for {gap} ms during install"
    );
    manager.shutdown_all().await;
}
