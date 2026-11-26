//! Outbound SSRF policy, redirects and the checked HTTP client.

use super::*;

async fn spawn_http_redirect_mock() -> (u16, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let port = listener.local_addr().expect("mock addr").port();

    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            let req = String::from_utf8_lossy(&buf);
            let resp = if req.starts_with("GET /redirect ") {
                "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1337/auth/token\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            } else if req.starts_with("GET /metadata ") {
                "HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/latest/meta-data/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            } else if req.starts_with("GET /hop ") {
                "HTTP/1.1 302 Found\r\nLocation: /ok\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            } else if req.starts_with("GET /loop ") {
                "HTTP/1.1 302 Found\r\nLocation: /loop\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            } else if req.starts_with("GET /ok ") {
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
            } else {
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            };
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });

    (port, handle)
}

#[test]
fn http_policy_blocks_loopback_metadata_and_allows_private_lan() {
    let policy = crate::http_policy::OutboundHttpPolicy::production();

    for blocked in [
        "",
        "   ",
        "not a url",
        "http://example.com:0/x",
        "ftp://example.com/file.zip",
        "http://127.0.0.1/x",
        "http://[::1]:8080/json",
        "http://localhost:1337/auth/token",
        "http://LOCALHOST./auth/token",
        "http://sub.localhost/a",
        "http://0.0.0.0/x",
        "http://0.42.0.1/x",
        "http://[::]/x",
        "http://169.254.169.254/latest",
        "http://[fe80::1]/",
        "http://[::ffff:127.0.0.1]:1337/auth/token",
        "http://[::ffff:169.254.169.254]/latest",
        "http://[::127.0.0.1]/x",
        "http://224.0.0.1/x",
        "http://[ff02::1]/x",
        "file:///etc/passwd",
    ] {
        let err = policy
            .validate_url(blocked)
            .expect_err(&format!("production policy must reject `{blocked}`"));
        assert!(
            matches!(err, CoreError::HttpPolicy(_)),
            "expected HttpPolicy error for `{blocked}`, got {err:?}"
        );
    }

    for allowed in [
        "https://plugins.deckbrew.xyz/a.zip",
        "http://192.168.1.50:8096",
        "http://10.0.0.5:8123",
        "http://172.16.4.2",
        "http://[fd00::1]/api",
        "http://[::ffff:192.168.1.50]/api",
    ] {
        policy
            .validate_url(allowed)
            .unwrap_or_else(|e| panic!("production policy must accept `{allowed}`: {e}"));
    }
}

#[tokio::test]
async fn http_policy_blocks_redirect_into_loopback() {
    let (port, server) = spawn_http_redirect_mock().await;
    let policy = crate::http_policy::OutboundHttpPolicy::for_loopback_ports_for_testing(vec![port]);
    let client = policy.build_checked_client().expect("build checked client");

    for path in ["redirect", "metadata", "loop"] {
        let err = crate::http_policy::send_checked_request(
            &client,
            reqwest::Method::GET,
            &format!("http://127.0.0.1:{port}/{path}"),
            reqwest::header::HeaderMap::new(),
            None,
            5,
        )
        .await
        .expect_err(&format!("`/{path}` must be blocked by policy"));
        assert!(
            matches!(err, CoreError::HttpPolicy(_)),
            "expected HttpPolicy for `/{path}`, got {err:?}"
        );
    }

    let ok = crate::http_policy::send_checked_request(
        &client,
        reqwest::Method::GET,
        &format!("http://127.0.0.1:{port}/hop"),
        reqwest::header::HeaderMap::new(),
        None,
        5,
    )
    .await
    .expect("allowed relative redirect on mock port must succeed");
    assert_eq!(ok.status(), reqwest::StatusCode::OK);
    assert_eq!(ok.text().await.expect("body"), "ok");

    let prod_policy = crate::http_policy::OutboundHttpPolicy::production();
    let prod_client = prod_policy
        .build_checked_client()
        .expect("build production checked client");
    let err_prod = crate::http_policy::send_checked_request(
        &prod_client,
        reqwest::Method::GET,
        &format!("http://127.0.0.1:{port}/ok"),
        reqwest::header::HeaderMap::new(),
        None,
        5,
    )
    .await
    .expect_err("production policy must block loopback port even if server is listening");
    assert!(matches!(err_prod, CoreError::HttpPolicy(_)));

    assert!(matches!(
        policy.validate_ip(std::net::IpAddr::from([127, 0, 0, 1])),
        Err(CoreError::HttpPolicy(_))
    ));

    let allow_all =
        crate::http_policy::OutboundHttpPolicy::production().with_allow_loopback_for_tests(true);
    allow_all
        .validate_ip(std::net::IpAddr::from([127, 0, 0, 1]))
        .expect("with_allow_loopback_for_tests(true) permits loopback IP");

    server.abort();
}

#[tokio::test]
async fn http_policy_strips_sensitive_headers_cross_origin_and_preserves_307_body() {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    let target_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind target listener");
    let target_port = target_listener.local_addr().expect("target addr").port();
    let origin_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind origin listener");
    let origin_port = origin_listener.local_addr().expect("origin addr").port();

    let origin_task = tokio::spawn(async move {
        for _ in 0..2 {
            let Ok((mut sock, _)) = origin_listener.accept().await else {
                break;
            };
            let req = read_http_request_raw(&mut sock).await;
            let resp = if req.starts_with("POST /307 ") {
                format!(
                    "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://127.0.0.1:{target_port}/echo\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
            } else {
                format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{target_port}/echo\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
            };
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });

    let target_task = tokio::spawn(async move {
        for _ in 0..2 {
            let Ok((mut sock, _)) = target_listener.accept().await else {
                break;
            };
            let req = read_http_request_raw(&mut sock).await;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{req}",
                req.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });

    let policy = crate::http_policy::OutboundHttpPolicy::for_loopback_ports_for_testing(vec![
        origin_port,
        target_port,
    ]);
    let client = policy.build_checked_client().expect("build checked client");

    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::AUTHORIZATION,
        "Bearer secret".parse().expect("header"),
    );
    headers.insert(reqwest::header::COOKIE, "sid=xyz".parse().expect("header"));
    headers.insert(
        reqwest::header::HOST,
        "spoof.example".parse().expect("header"),
    );
    headers.insert(
        reqwest::header::CONTENT_TYPE,
        "text/plain".parse().expect("header"),
    );

    let res_307 = crate::http_policy::send_checked_request(
        &client,
        reqwest::Method::POST,
        &format!("http://127.0.0.1:{origin_port}/307"),
        headers.clone(),
        Some(reqwest::Body::from("payload-307")),
        5,
    )
    .await
    .expect("307 cross-origin redirect must succeed");
    let echoed_307 = res_307.text().await.expect("307 body");
    assert!(echoed_307.starts_with("POST /echo "));
    assert!(echoed_307.ends_with("payload-307"));
    let lowered_307 = echoed_307.to_ascii_lowercase();
    assert!(!lowered_307.contains("authorization: bearer"));
    assert!(!lowered_307.contains("cookie: sid=xyz"));
    assert!(!lowered_307.contains("spoof.example"));

    let res_302 = crate::http_policy::send_checked_request(
        &client,
        reqwest::Method::POST,
        &format!("http://127.0.0.1:{origin_port}/302"),
        headers,
        Some(reqwest::Body::from("payload-302")),
        5,
    )
    .await
    .expect("302 cross-origin redirect must succeed");
    let echoed_302 = res_302.text().await.expect("302 body");
    assert!(echoed_302.starts_with("GET /echo "));
    assert!(!echoed_302.contains("payload-302"));
    assert!(!echoed_302.to_ascii_lowercase().contains("content-type:"));

    origin_task.abort();
    target_task.abort();
}

#[tokio::test]
async fn checked_http_client_preserves_timeouts_and_user_agent() {
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock listener");
    let port = listener.local_addr().expect("mock addr").port();

    let server = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let req = read_http_request_raw(&mut sock).await;
            if req.starts_with("GET /slow ") {
                tokio::time::sleep(Duration::from_millis(350)).await;
            }
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{req}",
                req.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });

    let policy = crate::http_policy::OutboundHttpPolicy::for_loopback_ports_for_testing(vec![port])
        .with_timeouts(
            Some(Duration::from_secs(2)),
            Some(Duration::from_millis(100)),
        )
        .with_user_agent("ZeroDeck-Test/1.0");
    let client = crate::http_policy::CheckedHttpClient::new(policy).expect("checked client");

    let ok = crate::http_policy::send_checked_request(
        &client,
        reqwest::Method::GET,
        &format!("http://127.0.0.1:{port}/ua"),
        reqwest::header::HeaderMap::new(),
        None,
        5,
    )
    .await
    .expect("ua request must succeed");
    let body = ok.text().await.expect("ua body");
    assert!(
        body.to_ascii_lowercase()
            .contains("user-agent: zerodeck-test/1.0")
    );

    let err_timeout = crate::http_policy::send_checked_request(
        &client,
        reqwest::Method::GET,
        &format!("http://127.0.0.1:{port}/slow"),
        reqwest::header::HeaderMap::new(),
        None,
        5,
    )
    .await
    .expect_err("slow response must hit CheckedHttpClient response timeout");
    assert!(
        matches!(err_timeout, CoreError::HttpTimeout(_)),
        "expected HttpTimeout, got {err_timeout:?}"
    );

    server.abort();
}
