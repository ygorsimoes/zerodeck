//! Axum HTTP and WebSocket server foundation for `ZeroDeck`.

mod auth;
mod error;
mod router;

pub use error::ServerError;
pub use router::{AppState, ServerConfig, build_router, serve};

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use zerodeck_protocol::{ProtocolError, WsClientMessage};

    use crate::{AppState, ServerConfig, build_router};

    #[tokio::test]
    async fn server_auth_health_and_csrf_middleware_behave_as_specced() {
        let state = AppState::new(ServerConfig::default());
        let expected_token = state.csrf_token().to_owned();
        let app = build_router(state);

        let token_res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/auth/token")
                    .body(Body::empty())
                    .expect("build /auth/token req"),
            )
            .await
            .expect("call /auth/token");
        assert_eq!(token_res.status(), StatusCode::OK);
        let token_bytes = to_bytes(token_res.into_body(), 1024)
            .await
            .expect("read token body");
        assert_eq!(
            std::str::from_utf8(&token_bytes).expect("utf8"),
            expected_token
        );

        let health_res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .expect("build /healthz req"),
            )
            .await
            .expect("call /healthz");
        assert_eq!(health_res.status(), StatusCode::OK);

        let unauth_res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/methods/ping")
                    .body(Body::empty())
                    .expect("build unauth POST"),
            )
            .await
            .expect("call unauth POST");
        assert_eq!(unauth_res.status(), StatusCode::FORBIDDEN);

        let auth_res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/methods/ping")
                    .header("X-Decky-Auth", expected_token)
                    .body(Body::empty())
                    .expect("build auth POST"),
            )
            .await
            .expect("call auth POST");
        assert_eq!(auth_res.status(), StatusCode::OK);

        let ws_err = WsClientMessage::from_json_str(r#"{"type": -99}"#)
            .expect_err("unknown WS type must error");
        assert_eq!(ws_err, ProtocolError::UnknownMessageType(-99));

        let post_plugin_assets = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/plugins/demo/assets/file")
                    .body(Body::empty())
                    .expect("build post plugin assets"),
            )
            .await
            .expect("call post plugin assets");
        assert_eq!(post_plugin_assets.status(), StatusCode::FORBIDDEN);

        let traversal_assets = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/plugins/demo/../assets/secret")
                    .body(Body::empty())
                    .expect("build traversal plugin assets"),
            )
            .await
            .expect("call traversal plugin assets");
        assert_eq!(traversal_assets.status(), StatusCode::FORBIDDEN);
    }

    async fn status_for_host(host: &str) -> StatusCode {
        let app = build_router(AppState::new(ServerConfig::default()));
        app.oneshot(
            Request::builder()
                .uri("/auth/token")
                .header("Host", host)
                .body(Body::empty())
                .expect("build request with Host"),
        )
        .await
        .expect("call /auth/token")
        .status()
    }

    #[tokio::test]
    async fn host_guard_allows_loopback_hosts() {
        for host in [
            "127.0.0.1:1337",
            "127.0.0.1",
            "localhost:1337",
            "LOCALHOST:8080",
            "[::1]:1337",
            "[::1]",
        ] {
            assert_eq!(status_for_host(host).await, StatusCode::OK, "host {host}");
        }
    }

    #[tokio::test]
    async fn host_guard_rejects_non_loopback_hosts_to_block_dns_rebinding() {
        for host in [
            "attacker.example:1337",
            "attacker.example",
            "192.168.1.20:1337",
            "localhost.attacker.example",
            "127.0.0.1.nip.io:1337",
            "",
        ] {
            assert_eq!(
                status_for_host(host).await,
                StatusCode::FORBIDDEN,
                "host {host:?}"
            );
        }
    }

    #[test]
    fn redacted_uri_hides_auth_token_but_keeps_other_query_params() {
        let uri: axum::http::Uri = "/ws?foo=1&auth=secret-token&bar=2".parse().expect("uri");
        assert_eq!(
            crate::router::redacted_uri(&uri),
            "/ws?foo=1&auth=[redacted]&bar=2"
        );

        let plain: axum::http::Uri = "/healthz".parse().expect("uri");
        assert_eq!(crate::router::redacted_uri(&plain), "/healthz");
    }

    async fn get_status(app: &axum::Router, uri: &str, header: Option<&str>) -> StatusCode {
        let mut req = Request::builder().uri(uri);
        if let Some(token) = header {
            req = req.header("X-Decky-Auth", token);
        }
        app.clone()
            .oneshot(req.body(Body::empty()).expect("build request"))
            .await
            .expect("call router")
            .status()
    }

    #[tokio::test]
    async fn query_auth_paths_require_a_valid_token() {
        let state = AppState::new(ServerConfig::default());
        let token = state.csrf_token().to_owned();
        let app = build_router(state);

        for uri in ["/fetch", "/fetch?auth=wrong", "/ws", "/ws?auth=wrong"] {
            assert_eq!(
                get_status(&app, uri, None).await,
                StatusCode::FORBIDDEN,
                "{uri}"
            );
        }
        // A valid token gets past the middleware (`/fetch` itself is not routed yet).
        assert_eq!(
            get_status(&app, &format!("/fetch?url=x&auth={token}"), None).await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            get_status(&app, "/fetch", Some(&token)).await,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn percent_encoded_traversal_is_not_treated_as_public() {
        let app = build_router(AppState::new(ServerConfig::default()));
        for uri in [
            "/plugins/demo/assets/%2e%2e/%2e%2e/secret",
            "/plugins/demo/assets/%2E%2e/secret",
            "/static/%2e%2e/settings/loader.json",
        ] {
            assert_eq!(
                get_status(&app, uri, None).await,
                StatusCode::FORBIDDEN,
                "{uri}"
            );
        }
    }

    #[test]
    fn token_comparison_matches_only_identical_tokens() {
        assert!(crate::auth::tokens_match("abc-123", "abc-123"));
        assert!(!crate::auth::tokens_match("abc-123", "abc-124"));
        assert!(!crate::auth::tokens_match("abc-123", "abc-1234"));
        assert!(!crate::auth::tokens_match("", "abc"));
    }

    mod ws {
        use std::net::SocketAddr;
        use std::time::Duration;

        use futures_util::{SinkExt, StreamExt};
        use serde_json::{Value, json};
        use tokio::net::TcpListener;
        use tokio_tungstenite::tungstenite::Message;

        use crate::{AppState, ServerConfig, serve};

        async fn spawn_server(config: ServerConfig) -> (SocketAddr, String) {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("local addr");
            let state = AppState::new(config);
            let token = state.csrf_token().to_owned();
            tokio::spawn(serve(listener, state, std::future::pending()));
            (addr, token)
        }

        async fn connect(
            addr: SocketAddr,
            token: &str,
        ) -> tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        > {
            let url = format!("ws://{addr}/ws?auth={token}");
            tokio_tungstenite::connect_async(url)
                .await
                .expect("ws connect")
                .0
        }

        /// Reads frames until the server closes the connection; panics if it stays open for 2s.
        async fn assert_server_closes<S>(ws: &mut S)
        where
            S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
        {
            let closed = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    match ws.next().await {
                        None | Some(Err(_) | Ok(Message::Close(_))) => break,
                        Some(Ok(_)) => {}
                    }
                }
            })
            .await;
            assert!(closed.is_ok(), "server kept the connection open");
        }

        #[tokio::test]
        async fn full_sync_replays_pending_calls() {
            let (addr, token) = spawn_server(ServerConfig::default()).await;
            let mut ws = connect(addr, &token).await;

            let sync = json!({
                "type": 4,
                "messages": [{"type": 0, "id": 7, "route": "utilities/ping", "args": []}],
            });
            ws.send(Message::Text(sync.to_string().into()))
                .await
                .expect("send FULL_SYNC");

            let frame = tokio::time::timeout(Duration::from_secs(2), ws.next())
                .await
                .expect("reply to replayed call")
                .expect("stream open")
                .expect("frame");
            let reply: Value = serde_json::from_str(frame.to_text().expect("text")).expect("json");
            assert_eq!(reply, json!({"type": 1, "id": 7, "result": "pong"}));
        }

        #[tokio::test]
        async fn oversized_message_closes_connection() {
            let config = ServerConfig {
                ws_max_message_bytes: 1024,
                ..ServerConfig::default()
            };
            let (addr, token) = spawn_server(config).await;
            let mut ws = connect(addr, &token).await;

            let _ = ws.send(Message::Text("x".repeat(4096).into())).await;
            assert_server_closes(&mut ws).await;
        }

        #[tokio::test]
        async fn responsive_client_survives_many_keepalive_rounds() {
            let config = ServerConfig {
                ws_ping_interval: Duration::from_millis(30),
                ..ServerConfig::default()
            };
            let (addr, token) = spawn_server(config).await;
            let mut ws = connect(addr, &token).await;

            // Reading lets tungstenite answer each ping with a pong, as a browser would.
            let mut pings = 0;
            let _ = tokio::time::timeout(Duration::from_millis(400), async {
                while let Some(Ok(frame)) = ws.next().await {
                    if frame.is_ping() {
                        pings += 1;
                    }
                }
            })
            .await;
            assert!(pings >= 5, "expected several keepalive rounds, saw {pings}");

            let call = json!({"type": 0, "id": 1, "route": "utilities/ping", "args": []});
            ws.send(Message::Text(call.to_string().into()))
                .await
                .expect("still connected");
            let reply = loop {
                let frame = tokio::time::timeout(Duration::from_secs(2), ws.next())
                    .await
                    .expect("reply")
                    .expect("stream open")
                    .expect("frame");
                if frame.is_text() {
                    break frame;
                }
            };
            let reply: Value = serde_json::from_str(reply.to_text().expect("text")).expect("json");
            assert_eq!(reply["result"], json!("pong"));
        }

        #[tokio::test]
        async fn unresponsive_client_is_disconnected_by_keepalive() {
            let config = ServerConfig {
                ws_ping_interval: Duration::from_millis(50),
                ..ServerConfig::default()
            };
            let (addr, token) = spawn_server(config).await;
            let mut ws = connect(addr, &token).await;

            // Not reading means no pong is sent back, so the server must give up on us.
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_server_closes(&mut ws).await;
        }
    }
}
