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

        fn server_test_paths(root: &std::path::Path) -> zerodeck_platform::PlatformPaths {
            let root_str = root.display().to_string();
            let home_str = root.join("home").display().to_string();
            std::fs::create_dir_all(&home_str).expect("create home");
            let lookup = std::collections::HashMap::from([
                ("UNPRIVILEGED_PATH", root_str.as_str()),
                ("PRIVILEGED_PATH", root_str.as_str()),
                ("DECKY_USER_HOME", home_str.as_str()),
                ("HOME", home_str.as_str()),
            ]);
            zerodeck_platform::PlatformPaths::from_lookup(|k| {
                lookup.get(k).map(|v| (*v).to_owned())
            })
        }

        fn python_host_dir() -> String {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../hosts/python")
                .canonicalize()
                .expect("resolve hosts/python")
                .display()
                .to_string()
        }

        async fn spawn_manager_server(
            config: ServerConfig,
            manager: std::sync::Arc<zerodeck_core::PluginManager>,
        ) -> (SocketAddr, String) {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("local addr");
            let state = AppState::with_manager(config, manager);
            let token = state.csrf_token().to_owned();
            tokio::spawn(serve(listener, state, std::future::pending()));
            (addr, token)
        }

        async fn next_text_frame<S>(ws: &mut S) -> Value
        where
            S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
        {
            loop {
                let frame = tokio::time::timeout(Duration::from_secs(3), ws.next())
                    .await
                    .expect("ws frame timeout")
                    .expect("ws stream open")
                    .expect("ws frame ok");
                if let Message::Text(txt) = frame {
                    return serde_json::from_str(&txt).expect("valid json frame");
                }
            }
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn ws_slow_plugin_call_does_not_starve_keepalive_or_concurrent_calls() {
            let dir = std::env::temp_dir().join(format!("zd-srv-slow-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("mkdir");
            let paths = server_test_paths(&dir);
            let plug = paths.plugins_dir().join("slow");
            std::fs::create_dir_all(&plug).expect("plug dir");
            std::fs::write(
                plug.join("plugin.json"),
                r#"{"name":"SlowPlug","author":"Tester","api_version":1}"#,
            )
            .expect("plugin.json");
            std::fs::write(
                plug.join("main.py"),
                "import asyncio\nclass Plugin:\n    async def wait_ms(self, ms):\n        await asyncio.sleep(ms / 1000.0)\n        return 'waited'\n",
            )
            .expect("main.py");

            let sup_cfg = zerodeck_plugin_host::SupervisorConfig {
                python_host: zerodeck_plugin_host::PythonHostConfig {
                    python_bin: "python3".to_owned(),
                    host_module_path: Some(python_host_dir()),
                },
                ..zerodeck_plugin_host::SupervisorConfig::default()
            };
            let manager = std::sync::Arc::new(
                zerodeck_core::PluginManager::open(paths, sup_cfg, "0.1.0").expect("open mgr"),
            );
            manager.start_all().await.expect("start_all");

            let srv_cfg = ServerConfig {
                ws_ping_interval: Duration::from_millis(80),
                ..ServerConfig::default()
            };
            let (addr, token) = spawn_manager_server(srv_cfg, manager.clone()).await;
            let mut ws = connect(addr, &token).await;

            // Slow call runs 350ms (> 4 keepalive intervals of 80ms); ping/pong must keep flowing
            // and a concurrent ping must reply before the slow call finishes.
            let slow_call = json!({
                "type": 0, "id": 1, "route": "loader/call_plugin_method",
                "args": ["SlowPlug", "wait_ms", 350]
            });
            let fast_call = json!({
                "type": 0, "id": 2, "route": "utilities/ping", "args": []
            });
            ws.send(Message::Text(slow_call.to_string().into()))
                .await
                .expect("send slow");
            ws.send(Message::Text(fast_call.to_string().into()))
                .await
                .expect("send fast");

            let mut replies = Vec::new();
            let mut pings_seen = 0usize;
            while replies.len() < 2 {
                let frame = tokio::time::timeout(Duration::from_secs(3), ws.next())
                    .await
                    .expect("ws frame timeout")
                    .expect("ws stream open")
                    .expect("ws frame ok");
                match frame {
                    Message::Text(txt) => {
                        replies.push(serde_json::from_str::<Value>(&txt).expect("json"));
                    }
                    Message::Ping(_) => {
                        pings_seen += 1;
                    }
                    _ => {}
                }
            }
            assert!(
                pings_seen >= 2,
                "expected keepalive pings during slow call, saw {pings_seen}"
            );
            assert_eq!(replies[0]["id"], json!(2));
            assert_eq!(replies[0]["result"], json!("pong"));
            assert_eq!(replies[1]["id"], json!(1));
            assert_eq!(replies[1]["result"], json!("waited"));

            manager.shutdown_all().await;
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[cfg(unix)]
        fn seed_v0_and_v1_plugins(paths: &zerodeck_platform::PlatformPaths) {
            let v0 = paths.plugins_dir().join("v0");
            std::fs::create_dir_all(&v0).expect("v0 dir");
            std::fs::write(
                v0.join("plugin.json"),
                r#"{"name":"V0","author":"Alice","api_version":0}"#,
            )
            .expect("v0 manifest");
            std::fs::write(
                v0.join("main.py"),
                "class Plugin:\n    async def greet(self, name='world'):\n        return f'hi {name}'\n",
            )
            .expect("v0 main");

            let v1 = paths.plugins_dir().join("v1");
            std::fs::create_dir_all(&v1).expect("v1 dir");
            std::fs::write(
                v1.join("plugin.json"),
                r#"{"name":"V1","author":"Bob","api_version":1}"#,
            )
            .expect("v1 manifest");
            std::fs::write(
                v1.join("main.py"),
                r#"import os, decky
class Plugin:
    async def emit_and_sum(self, a, b):
        await decky.emit("tick", a + b)
        return a + b
    async def boom(self):
        raise ValueError("nope")
    async def die(self):
        os._exit(1)
"#,
            )
            .expect("v1 main");
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn ws_dispatches_v0_legacy_v1_positional_private_guard_and_plugin_events() {
            let dir = std::env::temp_dir().join(format!("zd-srv-rpc-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("mkdir");
            let paths = server_test_paths(&dir);
            seed_v0_and_v1_plugins(&paths);

            let sup_cfg = zerodeck_plugin_host::SupervisorConfig {
                python_host: zerodeck_plugin_host::PythonHostConfig {
                    python_bin: "python3".to_owned(),
                    host_module_path: Some(python_host_dir()),
                },
                initial_backoff: Duration::from_secs(5),
                ..zerodeck_plugin_host::SupervisorConfig::default()
            };
            let manager = std::sync::Arc::new(
                zerodeck_core::PluginManager::open(paths, sup_cfg, "0.1.0").expect("open mgr"),
            );
            manager.start_all().await.expect("start_all");

            let (addr, token) =
                spawn_manager_server(ServerConfig::default(), manager.clone()).await;
            let mut ws = connect(addr, &token).await;

            // Legacy v0 call
            ws.send(Message::Text(
                json!({
                    "type": 0, "id": 10, "route": "loader/call_legacy_plugin_method",
                    "args": ["V0", "greet", {"name": "deck"}]
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("send v0");
            assert_eq!(
                next_text_frame(&mut ws).await,
                json!({"type": 1, "id": 10, "result": "hi deck"})
            );

            // Private method guard
            ws.send(Message::Text(
                json!({
                    "type": 0, "id": 11, "route": "loader/call_plugin_method",
                    "args": ["V1", "_main"]
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("send private");
            let priv_err = next_text_frame(&mut ws).await;
            assert_eq!(priv_err["type"], json!(-1));
            assert_eq!(priv_err["id"], json!(11));

            // v1 call + loader/plugin_event broadcast
            ws.send(Message::Text(
                json!({
                    "type": 0, "id": 12, "route": "loader/call_plugin_method",
                    "args": ["V1", "emit_and_sum", 20, 22]
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("send v1");
            let f1 = next_text_frame(&mut ws).await;
            let f2 = next_text_frame(&mut ws).await;
            let frames = [f1, f2];
            assert!(frames.iter().any(|f| {
                f["type"] == json!(1) && f["id"] == json!(12) && f["result"] == json!(42)
            }));
            assert!(frames.iter().any(|f| {
                f["type"] == json!(5)
                    && f["event"] == json!("loader/plugin_event")
                    && f["args"] == json!([{"plugin": "V1", "event": "tick", "args": [42]}])
            }));

            // Python exception -> ERROR (-1) with traceback
            ws.send(Message::Text(
                json!({
                    "type": 0, "id": 13, "route": "loader/call_plugin_method",
                    "args": ["V1", "boom"]
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("send boom");
            let boom = next_text_frame(&mut ws).await;
            assert_eq!(boom["type"], json!(-1));
            assert_eq!(boom["error"]["name"], json!("ValueError"));
            assert!(boom["error"]["traceback"].as_str().is_some());

            // Worker crash mid-call -> DISCARD (2)
            ws.send(Message::Text(
                json!({
                    "type": 0, "id": 14, "route": "loader/call_plugin_method",
                    "args": ["V1", "die"]
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("send die");
            let disc = next_text_frame(&mut ws).await;
            assert_eq!(disc, json!({"type": 2, "id": 14}));

            manager.shutdown_all().await;
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn http_methods_call_plugin_method_respects_contract() {
            use axum::body::{Body, to_bytes};
            use axum::http::{Request, StatusCode};
            use tower::ServiceExt;

            let dir = std::env::temp_dir().join(format!("zd-srv-http-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("mkdir");
            let paths = server_test_paths(&dir);

            let p = paths.plugins_dir().join("demo");
            std::fs::create_dir_all(&p).expect("demo dir");
            std::fs::write(
                p.join("plugin.json"),
                r#"{"name":"Demo","author":"Alice","api_version":1}"#,
            )
            .expect("plugin.json");
            std::fs::write(
                p.join("main.py"),
                "class Plugin:\n    async def add(self, a, b):\n        return a + b\n    async def fail(self):\n        raise RuntimeError('bad')\n",
            )
            .expect("main.py");

            let passive = paths.plugins_dir().join("passive");
            std::fs::create_dir_all(&passive).expect("passive dir");
            std::fs::write(
                passive.join("plugin.json"),
                r#"{"name":"Passive","author":"Bob"}"#,
            )
            .expect("passive plugin.json");

            let sup_cfg = zerodeck_plugin_host::SupervisorConfig {
                python_host: zerodeck_plugin_host::PythonHostConfig {
                    python_bin: "python3".to_owned(),
                    host_module_path: Some(python_host_dir()),
                },
                ..zerodeck_plugin_host::SupervisorConfig::default()
            };
            let manager = std::sync::Arc::new(
                zerodeck_core::PluginManager::open(paths, sup_cfg, "0.1.0").expect("open mgr"),
            );
            manager.start_all().await.expect("start_all");

            let state = AppState::with_manager(ServerConfig::default(), manager.clone());
            let token = state.csrf_token().to_owned();
            let app = crate::build_router(state);

            let call = |body: Value| {
                let app = app.clone();
                let token = token.clone();
                async move {
                    let res = app
                        .oneshot(
                            Request::builder()
                                .method("POST")
                                .uri("/methods/call_plugin_method")
                                .header("X-Decky-Auth", token)
                                .header("Content-Type", "application/json")
                                .body(Body::from(body.to_string()))
                                .expect("req"),
                        )
                        .await
                        .expect("resp");
                    let status = res.status();
                    let bytes = to_bytes(res.into_body(), 16 * 1024).await.expect("body");
                    let json: Value = serde_json::from_slice(&bytes).expect("json");
                    (status, json)
                }
            };

            let (st, ok_body) =
                call(json!({"plugin_name":"Demo","method_name":"add","args":[2,3]})).await;
            assert_eq!(st, StatusCode::OK);
            assert_eq!(ok_body, json!({"success": true, "result": 5}));

            let (st, exc_body) = call(json!({"plugin_name":"Demo","method_name":"fail"})).await;
            assert_eq!(st, StatusCode::OK);
            assert_eq!(exc_body["success"], json!(false));
            assert_eq!(exc_body["result"]["name"], json!("RuntimeError"));

            let (st, _) = call(json!({"plugin_name":"Demo","method_name":"_main"})).await;
            assert_eq!(st, StatusCode::BAD_REQUEST);

            let (st, _) = call(json!({"plugin_name":"Missing","method_name":"add"})).await;
            assert_eq!(st, StatusCode::NOT_FOUND);

            let (st, _) = call(json!({"plugin_name":"Passive","method_name":"x"})).await;
            assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);

            manager.shutdown_all().await;
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
