//! Axum HTTP and WebSocket server foundation for `ZeroDeck`.

#![forbid(unsafe_code)]

mod auth;
mod error;
mod fetch;
mod routes;
mod state;
mod ws;
mod ws_sync;

pub use error::ServerError;
pub use routes::{build_router, serve};
pub use state::{AppState, AppStateBuilder, ServerConfig, build_id};
pub use ws_sync::WsSyncCache;

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use zerodeck_protocol::{ProtocolError, WsClientMessage};

    use crate::{AppState, ServerConfig, build_router};

    #[tokio::test]
    async fn readyz_is_authenticated_and_reports_no_internal_paths() {
        use serde_json::{Value, json};

        let secret_frontend = std::env::temp_dir().join("zerodeck-readiness-secret-frontend");
        let state = AppState::builder(ServerConfig {
            version: "1.2.3".to_owned(),
            build_id: "build-abc".to_owned(),
            python_host_ready: true,
            invocation_id: "invocation-test".to_owned(),
            frontend_dir: Some(secret_frontend.clone()),
            ..ServerConfig::default()
        })
        .build()
        .expect("build app state");
        let token = state.csrf_token().to_owned();
        let app = build_router(state);

        let unauthenticated = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/readyz")
                    .body(Body::empty())
                    .expect("build unauthenticated /readyz req"),
            )
            .await
            .expect("call unauthenticated /readyz");
        assert_eq!(unauthenticated.status(), StatusCode::FORBIDDEN);

        let authenticated = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/readyz")
                    .header("X-Decky-Auth", &token)
                    .body(Body::empty())
                    .expect("build authenticated /readyz req"),
            )
            .await
            .expect("call authenticated /readyz");
        assert_eq!(authenticated.status(), StatusCode::OK);
        let body = to_bytes(authenticated.into_body(), 1024)
            .await
            .expect("read /readyz body");
        let text = std::str::from_utf8(&body).expect("readyz is utf-8");
        let ready: Value = serde_json::from_str(text).expect("readyz is json");
        assert_eq!(
            ready,
            json!({
                "version": "1.2.3",
                "build_id": "build-abc",
                "python_host_ready": true,
                "invocation_id": "invocation-test",
            })
        );
        assert!(!text.contains("frontend"), "{text}");
        assert!(!text.contains("zerodeck-readiness-secret"), "{text}");
        assert!(
            !text.contains(&secret_frontend.display().to_string()),
            "{text}"
        );

        let health = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .expect("build /healthz req"),
            )
            .await
            .expect("call /healthz");
        assert_eq!(health.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn server_auth_health_and_csrf_middleware_behave_as_specced() {
        let state = AppState::builder(ServerConfig::default())
            .build()
            .expect("build app state");
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
        let app = build_router(
            AppState::builder(ServerConfig::default())
                .build()
                .expect("build app state"),
        );
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
            crate::routes::redacted_uri(&uri),
            "/ws?foo=1&auth=[redacted]&bar=2"
        );

        let plain: axum::http::Uri = "/healthz".parse().expect("uri");
        assert_eq!(crate::routes::redacted_uri(&plain), "/healthz");
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
        let state = AppState::builder(ServerConfig::default())
            .build()
            .expect("build app state");
        let token = state.csrf_token().to_owned();
        let app = build_router(state);

        for uri in ["/fetch", "/fetch?auth=wrong", "/ws", "/ws?auth=wrong"] {
            assert_eq!(
                get_status(&app, uri, None).await,
                StatusCode::FORBIDDEN,
                "{uri}"
            );
        }
        // A valid token gets past the middleware (`/fetch` requires `fetch_url`).
        assert_eq!(
            get_status(&app, &format!("/fetch?url=x&auth={token}"), None).await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            get_status(&app, &format!("/fetch?auth={token}"), None).await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            get_status(&app, "/fetch", Some(&token)).await,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn percent_encoded_traversal_is_not_treated_as_public() {
        let app = build_router(
            AppState::builder(ServerConfig::default())
                .build()
                .expect("build app state"),
        );
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

    async fn call_get(
        app: &axum::Router,
        method: &str,
        uri: &str,
    ) -> (StatusCode, axum::http::HeaderMap, String) {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("call router");
        let status = res.status();
        let headers = res.headers().clone();
        let body = to_bytes(res.into_body(), 1024 * 1024)
            .await
            .expect("read body");
        (status, headers, String::from_utf8_lossy(&body).into_owned())
    }

    async fn call_get_with(
        app: &axum::Router,
        method: &str,
        uri: &str,
        extra: &[(&str, &str)],
    ) -> (StatusCode, axum::http::HeaderMap, String) {
        let mut builder = Request::builder().method(method).uri(uri);
        for (k, v) in extra {
            builder = builder.header(*k, *v);
        }
        let res = app
            .clone()
            .oneshot(builder.body(Body::empty()).expect("build request"))
            .await
            .expect("call router");
        let status = res.status();
        let headers = res.headers().clone();
        let body = to_bytes(res.into_body(), 1024 * 1024)
            .await
            .expect("read body");
        (status, headers, String::from_utf8_lossy(&body).into_owned())
    }

    #[test]
    fn mime_for_path_covers_common_asset_types() {
        for (ext, mime) in [
            ("gif", "image/gif"),
            ("ico", "image/x-icon"),
            ("avif", "image/avif"),
            ("ttf", "font/ttf"),
            ("otf", "font/otf"),
            ("woff", "font/woff"),
            ("map", "application/json"),
            ("txt", "text/plain; charset=utf-8"),
            ("mp3", "audio/mpeg"),
            ("ogg", "audio/ogg"),
            ("wav", "audio/wav"),
            ("mp4", "video/mp4"),
            ("webm", "video/webm"),
        ] {
            let path = std::path::PathBuf::from(format!("file.{ext}"));
            assert_eq!(
                crate::routes::static_files::mime_for_path(&path),
                mime,
                "{ext}"
            );
        }
    }

    #[test]
    fn duplicate_plugin_name_maps_to_its_own_ws_error() {
        let err = zerodeck_core::CoreError::DuplicatePluginName {
            name: "Alpha".to_owned(),
            directory: "alpha".to_owned(),
        };
        match crate::ws::errors::core_error_to_ws(7, err) {
            zerodeck_protocol::WsServerMessage::Error { id, error } => {
                assert_eq!(id, 7);
                assert_eq!(error.name, "DuplicatePluginNameError");
                assert!(error.error.contains("`alpha`"), "{}", error.error);
            }
            other => panic!("expected an error reply, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn static_responses_send_nosniff_and_last_modified() {
        let dir = std::env::temp_dir().join(format!("zd-srv-hdr-{}", uuid::Uuid::new_v4()));
        let (frontend, mgr) = seed_static_and_plugin_routes(&dir);
        let cfg = ServerConfig {
            frontend_dir: Some(frontend),
            ..ServerConfig::default()
        };
        let app = build_router(
            AppState::builder(cfg)
                .manager(mgr)
                .build()
                .expect("build app state"),
        );
        for method in ["GET", "HEAD"] {
            let (st, hdrs, _) = call_get(&app, method, "/plugins/Demo/assets/a.png").await;
            assert_eq!(st, StatusCode::OK);
            assert_eq!(hdrs.get("x-content-type-options").unwrap(), "nosniff");
            assert!(hdrs.get("last-modified").is_some(), "{method}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn static_if_modified_since_returns_304() {
        let dir = std::env::temp_dir().join(format!("zd-srv-ims-{}", uuid::Uuid::new_v4()));
        let (frontend, mgr) = seed_static_and_plugin_routes(&dir);
        let cfg = ServerConfig {
            frontend_dir: Some(frontend),
            ..ServerConfig::default()
        };
        let app = build_router(
            AppState::builder(cfg)
                .manager(mgr)
                .build()
                .expect("build app state"),
        );
        let uri = "/plugins/Demo/assets/a.png";
        let (_, hdrs, _) = call_get(&app, "GET", uri).await;
        let lm = hdrs
            .get("last-modified")
            .expect("last-modified")
            .to_str()
            .unwrap()
            .to_owned();
        let (st, hdrs, body) = call_get_with(&app, "GET", uri, &[("if-modified-since", &lm)]).await;
        assert_eq!(st, StatusCode::NOT_MODIFIED);
        assert_eq!(body, "");
        assert_eq!(hdrs.get("x-content-type-options").unwrap(), "nosniff");
        let (st, _, body) = call_get_with(&app, "HEAD", uri, &[("if-modified-since", &lm)]).await;
        assert_eq!(st, StatusCode::NOT_MODIFIED, "HEAD");
        assert_eq!(body, "");
        // An older validator yields the full body.
        let (st, _, body) = call_get_with(
            &app,
            "GET",
            uri,
            &[("if-modified-since", "Thu, 01 Jan 1970 00:00:00 GMT")],
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(body, "png");
        // Garbage validators are ignored.
        let (st, _, _) =
            call_get_with(&app, "GET", uri, &[("if-modified-since", "nonsense")]).await;
        assert_eq!(st, StatusCode::OK);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn static_if_modified_since_in_the_future_is_ignored() {
        let dir = std::env::temp_dir().join(format!("zd-srv-ims-future-{}", uuid::Uuid::new_v4()));
        let (frontend, mgr) = seed_static_and_plugin_routes(&dir);
        let cfg = ServerConfig {
            frontend_dir: Some(frontend),
            ..ServerConfig::default()
        };
        let app = build_router(
            AppState::builder(cfg)
                .manager(mgr)
                .build()
                .expect("build app state"),
        );
        // RFC 7232 section 3.3: a date later than the server's current time is invalid.
        let future = httpdate::fmt_http_date(
            std::time::SystemTime::now() + std::time::Duration::from_hours(24),
        );
        for method in ["GET", "HEAD"] {
            let (st, _, _) = call_get_with(
                &app,
                method,
                "/plugins/Demo/assets/a.png",
                &[("if-modified-since", &future)],
            )
            .await;
            assert_eq!(st, StatusCode::OK, "{method}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn frontend_bundle_never_returns_304() {
        let dir = std::env::temp_dir().join(format!("zd-srv-bundle-{}", uuid::Uuid::new_v4()));
        let (frontend, mgr) = seed_static_and_plugin_routes(&dir);
        let cfg = ServerConfig {
            frontend_dir: Some(frontend),
            ..ServerConfig::default()
        };
        let app = build_router(
            AppState::builder(cfg)
                .manager(mgr)
                .build()
                .expect("build app state"),
        );
        for uri in ["/plugins/Demo/frontend_bundle", "/plugins/load_main/Demo"] {
            let (_, hdrs, _) = call_get(&app, "GET", uri).await;
            let lm = hdrs
                .get("last-modified")
                .expect("last-modified")
                .to_str()
                .unwrap()
                .to_owned();
            let (st, hdrs, body) =
                call_get_with(&app, "GET", uri, &[("if-modified-since", &lm)]).await;
            assert_eq!(st, StatusCode::OK, "{uri}");
            assert_eq!(hdrs.get("cache-control").unwrap(), "no-store");
            assert_eq!(body, "export default 1;");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn test_paths_with_home(root: &std::path::Path) -> zerodeck_platform::PlatformPaths {
        let root_str = root.display().to_string();
        let home_str = root.join("home").display().to_string();
        std::fs::create_dir_all(&home_str).expect("create home");
        let lookup = std::collections::HashMap::from([
            ("UNPRIVILEGED_PATH", root_str.as_str()),
            ("PRIVILEGED_PATH", root_str.as_str()),
            ("DECKY_USER_HOME", home_str.as_str()),
            ("HOME", home_str.as_str()),
        ]);
        zerodeck_platform::PlatformPaths::from_lookup(|k| lookup.get(k).map(|v| (*v).to_owned()))
    }

    #[cfg(unix)]
    fn seed_static_and_plugin_routes(
        dir: &std::path::Path,
    ) -> (
        std::path::PathBuf,
        std::sync::Arc<zerodeck_core::PluginManager>,
    ) {
        let frontend = dir.join("frontend");
        std::fs::create_dir_all(frontend.join("static")).expect("mkdir static");
        std::fs::create_dir_all(frontend.join("locales")).expect("mkdir locales");
        std::fs::write(frontend.join("index.js"), "console.log('fe');").expect("index.js");
        std::fs::write(frontend.join("static/app.css"), "body{}").expect("app.css");
        std::fs::write(frontend.join("locales/en.json"), r#"{"ok":true}"#).expect("en.json");

        let paths = test_paths_with_home(dir);
        let plug = paths.plugins_dir().join("demo");
        std::fs::create_dir_all(plug.join("dist")).expect("dist");
        std::fs::create_dir_all(plug.join("assets")).expect("assets");
        std::fs::create_dir_all(paths.data_dir().join("demo")).expect("data");
        std::fs::write(
            plug.join("plugin.json"),
            r#"{"name":"Demo","author":"Alice"}"#,
        )
        .expect("plugin.json");
        std::fs::write(plug.join("dist/index.js"), "export default 1;").expect("dist/index.js");
        std::fs::write(plug.join("assets/icon.svg"), "<svg/>").expect("icon.svg");
        std::fs::write(plug.join("assets/a.png"), "png").expect("a.png");
        std::fs::write(paths.data_dir().join("demo/cache.json"), r#"{"n":1}"#).expect("cache.json");

        let outside = dir.join("outside.txt");
        std::fs::write(&outside, "secret").expect("outside");
        std::os::unix::fs::symlink(&outside, plug.join("assets/escape.svg")).expect("symlink");
        std::os::unix::fs::symlink(dir.join("missing.svg"), plug.join("assets/broken.svg"))
            .expect("broken symlink");

        let mgr = std::sync::Arc::new(
            zerodeck_core::PluginManager::open(
                paths,
                zerodeck_plugin_host::SupervisorConfig::default(),
                "0.1.0",
            )
            .expect("open mgr"),
        );
        (frontend, mgr)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn http_static_and_plugin_routes_serve_mime_types_and_block_traversal() {
        let dir = std::env::temp_dir().join(format!("zd-srv-static-{}", uuid::Uuid::new_v4()));
        let (frontend, mgr) = seed_static_and_plugin_routes(&dir);
        let cfg = ServerConfig {
            frontend_dir: Some(frontend),
            ..ServerConfig::default()
        };
        let app = build_router(
            AppState::builder(cfg)
                .manager(mgr)
                .build()
                .expect("build app state"),
        );

        for (uri, mime, expected_body) in [
            (
                "/frontend/index.js",
                "text/javascript; charset=utf-8",
                "console.log('fe');",
            ),
            ("/static/app.css", "text/css; charset=utf-8", "body{}"),
            ("/locales/en.json", "application/json", r#"{"ok":true}"#),
            (
                "/plugins/Demo/dist/index.js",
                "text/javascript; charset=utf-8",
                "export default 1;",
            ),
            ("/plugins/Demo/assets/icon.svg", "image/svg+xml", "<svg/>"),
            (
                "/plugins/Demo/data/cache.json",
                "application/json",
                r#"{"n":1}"#,
            ),
        ] {
            let (st, hdrs, body) = call_get(&app, "GET", uri).await;
            assert_eq!(st, StatusCode::OK, "{uri}");
            assert_eq!(hdrs.get("content-type").unwrap(), mime, "{uri}");
            assert_eq!(body, expected_body, "{uri}");
        }

        for bundle_uri in ["/plugins/load_main/Demo", "/plugins/Demo/frontend_bundle"] {
            let (st, hdrs, body) = call_get(&app, "GET", bundle_uri).await;
            assert_eq!(st, StatusCode::OK, "{bundle_uri}");
            assert_eq!(
                hdrs.get("content-type").unwrap(),
                "text/javascript; charset=utf-8"
            );
            assert_eq!(hdrs.get("cache-control").unwrap(), "no-store");
            assert_eq!(body, "export default 1;");
        }
        let (head_st, head_hdrs, head_body) =
            call_get(&app, "HEAD", "/plugins/Demo/frontend_bundle").await;
        assert_eq!(head_st, StatusCode::OK);
        assert_eq!(
            head_hdrs.get("content-type").unwrap(),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(head_body, "");

        for bad in [
            "/frontend/../outside.txt",
            "/plugins/Demo/assets/%2e%2e/plugin.json",
            "/plugins/Demo/assets/%00bad",
            "/plugins/Demo/assets/escape.svg",
        ] {
            let (st, _, _) = call_get(&app, "GET", bad).await;
            assert_eq!(st, StatusCode::FORBIDDEN, "{bad}");
        }

        for missing in [
            "/frontend/missing.js",
            "/plugins/Demo/assets/missing.svg",
            "/plugins/Demo/assets/broken.svg",
            "/plugins/Missing/dist/index.js",
        ] {
            let (st, _, _) = call_get(&app, "GET", missing).await;
            assert_eq!(st, StatusCode::NOT_FOUND, "{missing}");
        }

        let no_fe_app = build_router(
            AppState::builder(ServerConfig::default())
                .build()
                .expect("build app state"),
        );
        let (st, _, _) = call_get(&no_fe_app, "GET", "/static/app.css").await;
        assert_eq!(st, StatusCode::NOT_FOUND);

        let _ = std::fs::remove_dir_all(&dir);
    }

    async fn fetch_status_for(upstream_port: u16, path: &str) -> StatusCode {
        let policy =
            zerodeck_core::OutboundHttpPolicy::for_loopback_ports_for_testing(vec![upstream_port])
                .with_timeouts(None, Some(std::time::Duration::from_millis(200)));
        let client = zerodeck_core::CheckedHttpClient::new(policy).expect("checked client");
        let state = AppState::builder(ServerConfig::default())
            .http_client(client)
            .build()
            .expect("build app state");
        let token = state.csrf_token().to_owned();
        build_router(state)
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/fetch?fetch_url=http://127.0.0.1:{upstream_port}{path}"
                    ))
                    .header("X-Decky-Auth", &token)
                    .body(Body::empty())
                    .expect("fetch req"),
            )
            .await
            .expect("fetch resp")
            .status()
    }

    #[tokio::test]
    async fn fetch_proxy_maps_upstream_timeout_to_504() {
        let slow = axum::Router::new().route(
            "/slow",
            axum::routing::get(|| async {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                "late"
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind slow upstream");
        let port = listener.local_addr().expect("addr").port();
        let server = tokio::spawn(async move { axum::serve(listener, slow).await });

        assert_eq!(
            fetch_status_for(port, "/slow").await,
            StatusCode::GATEWAY_TIMEOUT
        );
        server.abort();
    }

    #[tokio::test]
    async fn fetch_proxy_maps_connection_refused_to_502() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);

        assert_eq!(
            fetch_status_for(port, "/gone").await,
            StatusCode::BAD_GATEWAY
        );
    }

    async fn spawn_mock_upstream() -> std::net::SocketAddr {
        use axum::routing::{get, post};
        let mock = axum::Router::new()
            .route(
                "/echo",
                post(
                    |headers: axum::http::HeaderMap, body: axum::body::Bytes| async move {
                        let hdr = |name: &str| {
                            headers
                                .get(name)
                                .and_then(|v| v.to_str().ok())
                                .map(str::to_owned)
                        };
                        let payload = serde_json::json!({
                            "x_custom": hdr("x-custom"),
                            "cookie": hdr("cookie"),
                            "origin": hdr("origin"),
                            "x_decky_auth": hdr("x-decky-auth"),
                            "body": String::from_utf8_lossy(&body).into_owned(),
                        });
                        (
                            StatusCode::OK,
                            [
                                ("access-control-allow-origin", "https://evil.example"),
                                ("access-control-expose-headers", "X-Evil"),
                                ("x-upstream", "ok"),
                                ("connection", "x-hop"),
                                ("x-hop", "secret"),
                            ],
                            axum::Json(payload),
                        )
                    },
                ),
            )
            .route(
                "/stream",
                get(|| async {
                    let chunks = vec![
                        Ok::<_, std::convert::Infallible>(axum::body::Bytes::from_static(
                            b"chunk1-",
                        )),
                        Ok(axum::body::Bytes::from_static(b"chunk2-")),
                        Ok(axum::body::Bytes::from_static(b"chunk3")),
                    ];
                    axum::response::Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "text/plain")
                        .body(Body::from_stream(futures_util::stream::iter(chunks)))
                        .expect("stream resp")
                }),
            )
            .route(
                "/echo-headers",
                get(|headers: axum::http::HeaderMap| async move {
                    let hdr = |name: &str| {
                        headers
                            .get(name)
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_owned)
                    };
                    axum::Json(serde_json::json!({
                        "upgrade": hdr("upgrade"),
                        "te": hdr("te"),
                        "keep_alive": hdr("keep-alive"),
                        "proxy_authorization": hdr("proxy-authorization"),
                        "x_decky_auth": hdr("x-decky-auth"),
                        "user_agent": hdr("user-agent"),
                    }))
                }),
            )
            .route(
                "/redirect-loopback",
                get(|| async {
                    axum::response::Response::builder()
                        .status(StatusCode::FOUND)
                        .header("location", "http://127.0.0.1:1337/auth/token")
                        .body(Body::empty())
                        .expect("redirect")
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock");
        let addr = listener.local_addr().expect("mock addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, mock).await;
        });
        addr
    }

    #[tokio::test]
    async fn fetch_proxy_translates_headers_streams_body_and_blocks_ssrf_redirects() {
        let mock_addr = spawn_mock_upstream().await;
        let policy = zerodeck_core::OutboundHttpPolicy::for_loopback_ports_for_testing(vec![
            mock_addr.port(),
        ]);
        let client = zerodeck_core::CheckedHttpClient::new(policy).expect("checked client");
        let state = AppState::builder(ServerConfig::default())
            .http_client(client)
            .build()
            .expect("build app state");
        let token = state.csrf_token().to_owned();
        let app = build_router(state);

        let echo_res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/fetch?fetch_url=http://127.0.0.1:{}/echo",
                        mock_addr.port()
                    ))
                    .header("X-Decky-Auth", &token)
                    .header("X-Decky-Header-X-Custom", "hello")
                    .header("X-Decky-Header-Cookie", "session=abc")
                    .header("Cookie", "leak=1")
                    .header("Origin", "https://steamloopback.host")
                    .header("X-Decky-Fetch-Excluded-Headers", "origin")
                    .body(Body::from("payload-bytes"))
                    .expect("echo req"),
            )
            .await
            .expect("echo resp");
        assert_eq!(echo_res.status(), StatusCode::OK);
        assert_eq!(echo_res.headers().get("x-upstream").unwrap(), "ok");
        assert_eq!(
            echo_res
                .headers()
                .get("access-control-allow-origin")
                .unwrap(),
            "https://steamloopback.host"
        );
        assert!(
            echo_res
                .headers()
                .get("access-control-expose-headers")
                .is_none()
        );
        assert!(echo_res.headers().get("x-hop").is_none());
        let echo_json: serde_json::Value =
            serde_json::from_slice(&to_bytes(echo_res.into_body(), 16 * 1024).await.unwrap())
                .expect("json");
        assert_eq!(
            echo_json,
            serde_json::json!({
                "x_custom": "hello",
                "cookie": "session=abc",
                "origin": null,
                "x_decky_auth": null,
                "body": "payload-bytes",
            })
        );

        let stream_res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/fetch?fetch_url=http://127.0.0.1:{}/stream&auth={token}",
                        mock_addr.port()
                    ))
                    .body(Body::empty())
                    .expect("stream req"),
            )
            .await
            .expect("stream resp");
        assert_eq!(stream_res.status(), StatusCode::OK);
        let stream_body = to_bytes(stream_res.into_body(), 16 * 1024).await.unwrap();
        assert_eq!(stream_body.as_ref(), b"chunk1-chunk2-chunk3");

        for blocked in [
            format!("/fetch?fetch_url=http://127.0.0.1:1337/auth/token&auth={token}"),
            format!("/fetch?fetch_url=http://169.254.169.254/latest/meta-data&auth={token}"),
            format!(
                "/fetch?fetch_url=http://127.0.0.1:{}/redirect-loopback&auth={token}",
                mock_addr.port()
            ),
        ] {
            let (st, _, _) = call_get(&app, "GET", &blocked).await;
            assert_eq!(st, StatusCode::FORBIDDEN, "{blocked}");
        }

        for invalid in [
            format!("/fetch?fetch_url=file:///etc/passwd&auth={token}"),
            format!("/fetch?fetch_url=not-a-url&auth={token}"),
        ] {
            let (st, _, _) = call_get(&app, "GET", &invalid).await;
            assert_eq!(st, StatusCode::BAD_REQUEST, "{invalid}");
        }
    }

    #[tokio::test]
    async fn fetch_proxy_blocks_hop_by_hop_and_auth_translated_headers() {
        let mock_addr = spawn_mock_upstream().await;
        let policy = zerodeck_core::OutboundHttpPolicy::for_loopback_ports_for_testing(vec![
            mock_addr.port(),
        ]);
        let client = zerodeck_core::CheckedHttpClient::new(policy).expect("checked client");
        let state = AppState::builder(ServerConfig::default())
            .http_client(client)
            .build()
            .expect("build app state");
        let token = state.csrf_token().to_owned();
        let app = build_router(state);

        let res = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!(
                        "/fetch?fetch_url=http://127.0.0.1:{}/echo-headers",
                        mock_addr.port()
                    ))
                    .header("X-Decky-Auth", &token)
                    .header("X-Decky-Header-Upgrade", "websocket")
                    .header("X-Decky-Header-TE", "trailers")
                    .header("X-Decky-Header-Keep-Alive", "timeout=5")
                    .header("X-Decky-Header-Proxy-Authorization", "Basic secret")
                    .header("X-Decky-Header-X-Decky-Auth", "forged-token")
                    .header("X-Decky-Header-User-Agent", "ZeroDeckFetch/1.0")
                    .body(Body::empty())
                    .expect("fetch req"),
            )
            .await
            .expect("fetch resp");
        assert_eq!(res.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(res.into_body(), 16 * 1024).await.unwrap())
                .expect("json");
        assert_eq!(
            body,
            serde_json::json!({
                "upgrade": null,
                "te": null,
                "keep_alive": null,
                "proxy_authorization": null,
                "x_decky_auth": null,
                "user_agent": "ZeroDeckFetch/1.0",
            })
        );
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
            let state = AppState::builder(config).build().expect("build app state");
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
        async fn plugin_relative_assets_follow_the_rollup_public_path_without_auth() {
            let dir = std::env::temp_dir().join(format!("zd-srv-assets-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("mkdir");
            let paths = server_test_paths(&dir);
            let plugin = paths.plugins_dir().join("assets");
            std::fs::create_dir_all(plugin.join("dist")).expect("dist");
            std::fs::create_dir_all(plugin.join("assets")).expect("assets");
            std::fs::write(
                plugin.join("plugin.json"),
                r#"{"name":"Assets","author":"Tester"}"#,
            )
            .expect("plugin.json");
            std::fs::write(plugin.join("dist/index.js"), "export default () => ({});")
                .expect("index");
            std::fs::write(plugin.join("assets/logo.txt"), "logo").expect("asset");
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
            let (addr, _token) =
                spawn_manager_server(ServerConfig::default(), manager.clone()).await;
            let client = reqwest::Client::new();

            let response = client
                .get(format!("http://{addr}/plugins/Assets/logo.txt"))
                .send()
                .await
                .expect("GET asset");
            assert_eq!(response.status(), reqwest::StatusCode::OK);
            assert_eq!(response.text().await.expect("asset body"), "logo");

            let traversal = client
                .get(format!(
                    "http://{addr}/plugins/Assets/%2e%2e/%2e%2e/etc/passwd"
                ))
                .send()
                .await
                .expect("GET traversal");
            assert!(
                matches!(
                    traversal.status(),
                    reqwest::StatusCode::FORBIDDEN | reqwest::StatusCode::NOT_FOUND
                ),
                "{}",
                traversal.status()
            );

            manager.shutdown_all().await;
            let _ = std::fs::remove_dir_all(&dir);
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
            let state = AppState::builder(config)
                .manager(manager)
                .build()
                .expect("build app state");
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

        /// Starts a manager with one async `SlowPlug` plugin (`wait_ms(ms)` sleeps then returns
        /// `"waited"`) behind a server built from `config`.
        #[cfg(unix)]
        async fn spawn_slow_plugin_server(
            config: ServerConfig,
        ) -> (
            std::sync::Arc<zerodeck_core::PluginManager>,
            SocketAddr,
            String,
            std::path::PathBuf,
        ) {
            let dir = std::env::temp_dir().join(format!("zd-srv-limits-{}", uuid::Uuid::new_v4()));
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
            let (addr, token) = spawn_manager_server(config, manager.clone()).await;
            (manager, addr, token, dir)
        }

        fn slow_call(id: u64, ms: u64) -> Value {
            json!({
                "type": 0, "id": id, "route": "loader/call_plugin_method",
                "args": ["SlowPlug", "wait_ms", ms]
            })
        }

        fn tiny_limits() -> ServerConfig {
            ServerConfig {
                ws_max_in_flight_calls: 1,
                ws_max_queued_calls: 1,
                ..ServerConfig::default()
            }
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn ws_rejects_calls_beyond_pending_capacity() {
            let (manager, addr, token, dir) = spawn_slow_plugin_server(tiny_limits()).await;
            let mut ws = connect(addr, &token).await;
            for (id, ms) in [(1, 400), (2, 401), (3, 402)] {
                ws.send(Message::Text(slow_call(id, ms).to_string().into()))
                    .await
                    .expect("send call");
            }

            let rejected = next_text_frame(&mut ws).await;
            assert_eq!(rejected["id"], json!(3));
            assert_eq!(rejected["error"]["name"], json!("TooManyPendingCalls"));
            let first = next_text_frame(&mut ws).await;
            let second = next_text_frame(&mut ws).await;
            assert_eq!(first["id"], json!(1));
            assert_eq!(first["result"], json!("waited"));
            assert_eq!(second["id"], json!(2));
            assert_eq!(second["result"], json!("waited"));

            manager.shutdown_all().await;
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn ws_full_sync_rejects_only_excess_messages() {
            let (manager, addr, token, dir) = spawn_slow_plugin_server(tiny_limits()).await;
            let mut ws = connect(addr, &token).await;
            let full_sync = json!({
                "type": 4,
                "messages": [slow_call(1, 300), slow_call(2, 301), slow_call(3, 302)]
            });
            ws.send(Message::Text(full_sync.to_string().into()))
                .await
                .expect("send FULL_SYNC");

            let mut rejected = Vec::new();
            let mut replied = Vec::new();
            while rejected.len() + replied.len() < 3 {
                let frame = next_text_frame(&mut ws).await;
                if frame["error"]["name"] == json!("TooManyPendingCalls") {
                    rejected.push(frame["id"].clone());
                } else {
                    assert_eq!(frame["result"], json!("waited"));
                    replied.push(frame["id"].clone());
                }
            }
            assert_eq!(rejected, vec![json!(3)]);
            assert_eq!(replied, vec![json!(1), json!(2)]);

            manager.shutdown_all().await;
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn ws_in_flight_cap_runs_calls_one_at_a_time() {
            let config = ServerConfig {
                ws_max_in_flight_calls: 1,
                ws_max_queued_calls: 4,
                ..ServerConfig::default()
            };
            let (manager, addr, token, dir) = spawn_slow_plugin_server(config).await;
            let mut ws = connect(addr, &token).await;
            // With one slot, the short call must wait for the long one; running them together
            // would answer the short call first.
            for (id, ms) in [(1, 600), (2, 50)] {
                ws.send(Message::Text(slow_call(id, ms).to_string().into()))
                    .await
                    .expect("send call");
            }

            let first = next_text_frame(&mut ws).await;
            let second = next_text_frame(&mut ws).await;
            assert_eq!(first["id"], json!(1));
            assert_eq!(second["id"], json!(2));

            manager.shutdown_all().await;
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn ws_keepalive_survives_flooded_queue() {
            let config = ServerConfig {
                ws_ping_interval: Duration::from_millis(80),
                ..tiny_limits()
            };
            let (manager, addr, token, dir) = spawn_slow_plugin_server(config).await;
            let mut ws = connect(addr, &token).await;
            // Two 400 ms calls fill the single slot and the single queue entry for ~800 ms
            // (10 ping intervals); the extra calls arrive while the queue is full.
            for (id, ms) in [(1, 400), (2, 401), (3, 402), (4, 403)] {
                ws.send(Message::Text(slow_call(id, ms).to_string().into()))
                    .await
                    .expect("send call");
            }

            let mut replies = 0usize;
            let mut rejections = 0usize;
            let mut pings_seen = 0usize;
            while replies < 2 {
                let frame = tokio::time::timeout(Duration::from_secs(3), ws.next())
                    .await
                    .expect("ws frame timeout")
                    .expect("ws stream open")
                    .expect("ws frame ok");
                match frame {
                    Message::Text(txt) => {
                        let frame: Value = serde_json::from_str(&txt).expect("json");
                        if frame["error"]["name"] == json!("TooManyPendingCalls") {
                            rejections += 1;
                        } else {
                            assert_eq!(frame["result"], json!("waited"));
                            replies += 1;
                        }
                    }
                    Message::Ping(_) => pings_seen += 1,
                    _ => {}
                }
            }
            assert_eq!(rejections, 2);
            assert!(
                pings_seen >= 3,
                "expected keepalive pings, saw {pings_seen}"
            );

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

            let state = AppState::builder(ServerConfig::default())
                .manager(manager.clone())
                .build()
                .expect("build app state");
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

        async fn ws_call<S>(
            ws: &mut S,
            events: &mut Vec<Value>,
            id: u64,
            route: &str,
            args: Value,
        ) -> Value
        where
            S: SinkExt<Message>
                + StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
                + Unpin,
            <S as futures_util::Sink<Message>>::Error: std::fmt::Debug,
        {
            let msg = json!({"type": 0, "id": id, "route": route, "args": args});
            ws.send(Message::Text(msg.to_string().into()))
                .await
                .expect("send ws call");
            loop {
                let frame = next_text_frame(ws).await;
                if frame["id"] == json!(id) {
                    return frame;
                }
                if frame["type"] == json!(5) {
                    events.push(frame);
                }
            }
        }

        async fn take_event<S>(ws: &mut S, events: &mut Vec<Value>, event_name: &str) -> Value
        where
            S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
        {
            if let Some(pos) = events.iter().position(|e| e["event"] == json!(event_name)) {
                return events.remove(pos);
            }
            loop {
                let frame = next_text_frame(ws).await;
                if frame["type"] == json!(5) && frame["event"] == json!(event_name) {
                    return frame;
                }
                if frame["type"] == json!(5) {
                    events.push(frame);
                }
            }
        }

        fn build_plugin_zip(
            zip_path: &std::path::Path,
            wrapper: &str,
            manifest_json: &str,
            main_py: &str,
        ) -> String {
            use sha2::Digest;
            use std::io::Write;
            let mut buf = std::io::Cursor::new(Vec::new());
            {
                let mut writer = zip::ZipWriter::new(&mut buf);
                let opts = zip::write::SimpleFileOptions::default();
                writer
                    .start_file(format!("{wrapper}/plugin.json"), opts)
                    .expect("start plugin.json");
                writer
                    .write_all(manifest_json.as_bytes())
                    .expect("write plugin.json");
                writer
                    .start_file(format!("{wrapper}/main.py"), opts)
                    .expect("start main.py");
                writer.write_all(main_py.as_bytes()).expect("write main.py");
                writer.finish().expect("finish zip");
            }
            let bytes = buf.into_inner();
            std::fs::write(zip_path, &bytes).expect("write zip");
            hex::encode(sha2::Sha256::digest(&bytes))
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn ws_full_sync_deduplicates_completed_call_and_evicts_on_received_response() {
            let dir = std::env::temp_dir().join(format!("zd-srv-sync-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("mkdir");
            let paths = server_test_paths(&dir);
            let counter_file = dir.join("count.txt");
            std::fs::write(&counter_file, "0").expect("seed counter");

            let plug = paths.plugins_dir().join("counter");
            std::fs::create_dir_all(&plug).expect("plug dir");
            std::fs::write(
                plug.join("plugin.json"),
                r#"{"name":"Counter","author":"Tester","api_version":1}"#,
            )
            .expect("plugin.json");
            let py = format!(
                "import asyncio\nfrom pathlib import Path\nP = Path({:?})\nclass Plugin:\n    async def inc(self):\n        n = int(P.read_text()) + 1\n        P.write_text(str(n))\n        return n\n    async def slow_inc(self):\n        await asyncio.sleep(0.15)\n        return await self.inc()\n",
                counter_file.display().to_string()
            );
            std::fs::write(plug.join("main.py"), py).expect("main.py");

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

            let (addr, token) =
                spawn_manager_server(ServerConfig::default(), manager.clone()).await;

            // 1) Completed call cached across reconnect FULL_SYNC until RECEIVED_RESPONSE.
            let mut ws1 = connect(addr, &token).await;
            let call_inc = json!({
                "type": 0, "id": 42, "route": "loader/call_plugin_method",
                "args": ["Counter", "inc"]
            });
            ws1.send(Message::Text(call_inc.to_string().into()))
                .await
                .expect("send call");
            let r1 = next_text_frame(&mut ws1).await;
            assert_eq!(r1, json!({"type": 1, "id": 42, "result": 1}));
            drop(ws1);

            let mut ws2 = connect(addr, &token).await;
            let sync = json!({"type": 4, "messages": [call_inc]});
            ws2.send(Message::Text(sync.to_string().into()))
                .await
                .expect("send sync");
            assert_eq!(
                next_text_frame(&mut ws2).await,
                json!({"type": 1, "id": 42, "result": 1})
            );
            assert_eq!(std::fs::read_to_string(&counter_file).unwrap(), "1");

            // 2) In-flight call deduplicated across reconnect FULL_SYNC.
            let mut ws_drop = connect(addr, &token).await;
            let call_slow = json!({
                "type": 0, "id": 43, "route": "loader/call_plugin_method",
                "args": ["Counter", "slow_inc"]
            });
            ws_drop
                .send(Message::Text(call_slow.to_string().into()))
                .await
                .expect("send slow");
            drop(ws_drop);
            ws2.send(Message::Text(
                json!({"type": 4, "messages": [call_slow]})
                    .to_string()
                    .into(),
            ))
            .await
            .expect("send in-flight sync");
            assert_eq!(
                next_text_frame(&mut ws2).await,
                json!({"type": 1, "id": 43, "result": 2})
            );
            assert_eq!(std::fs::read_to_string(&counter_file).unwrap(), "2");

            // 3) RECEIVED_RESPONSE evicts id=42 so calling again executes fresh.
            ws2.send(Message::Text(
                json!({"type": 3, "id": 42}).to_string().into(),
            ))
            .await
            .expect("send ack");
            tokio::time::sleep(Duration::from_millis(20)).await;
            ws2.send(Message::Text(call_inc.to_string().into()))
                .await
                .expect("send fresh call");
            assert_eq!(
                next_text_frame(&mut ws2).await,
                json!({"type": 1, "id": 42, "result": 3})
            );
            assert_eq!(std::fs::read_to_string(&counter_file).unwrap(), "3");

            manager.shutdown_all().await;
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn ws_new_session_reexecutes_unacknowledged_call_from_previous_session() {
            let dir = std::env::temp_dir().join(format!("zd-srv-session-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("mkdir");
            let paths = server_test_paths(&dir);
            let counter_file = dir.join("count.txt");
            std::fs::write(&counter_file, "0").expect("seed counter");

            let plug = paths.plugins_dir().join("counter");
            std::fs::create_dir_all(&plug).expect("plug dir");
            std::fs::write(
                plug.join("plugin.json"),
                r#"{"name":"Counter","author":"Tester","api_version":1}"#,
            )
            .expect("plugin.json");
            let py = format!(
                "from pathlib import Path\nP = Path({:?})\nclass Plugin:\n    async def inc(self):\n        n = int(P.read_text()) + 1\n        P.write_text(str(n))\n        return n\n",
                counter_file.display().to_string()
            );
            std::fs::write(plug.join("main.py"), py).expect("main.py");

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
            let (addr, token) =
                spawn_manager_server(ServerConfig::default(), manager.clone()).await;

            let call_inc = json!({
                "type": 0, "id": 1, "route": "loader/call_plugin_method",
                "args": ["Counter", "inc"]
            });
            for (session, expected) in [("s1", 1), ("s2", 2)] {
                let url = format!("ws://{addr}/ws?auth={token}&session={session}");
                let (mut ws, _) = tokio_tungstenite::connect_async(url)
                    .await
                    .expect("ws connect");
                ws.send(Message::Text(call_inc.to_string().into()))
                    .await
                    .expect("send call");
                assert_eq!(
                    next_text_frame(&mut ws).await,
                    json!({"type": 1, "id": 1, "result": expected})
                );
                // Close without acknowledging the response.
                drop(ws);
            }
            assert_eq!(std::fs::read_to_string(&counter_file).unwrap(), "2");

            manager.shutdown_all().await;
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[tokio::test]
        async fn ws_rejects_malformed_session_param() {
            let (addr, token) = spawn_server(ServerConfig::default()).await;
            let url = format!("ws://{addr}/ws?auth={token}&session=../x");
            let err = tokio_tungstenite::connect_async(url)
                .await
                .expect_err("malformed session must be rejected");
            match err {
                tokio_tungstenite::tungstenite::Error::Http(resp) => {
                    assert_eq!(resp.status().as_u16(), 400);
                }
                other => panic!("expected HTTP 400, got {other:?}"),
            }
        }

        #[tokio::test]
        async fn ws_rejects_bad_token_before_validating_session() {
            let (addr, _token) = spawn_server(ServerConfig::default()).await;
            let url = format!("ws://{addr}/ws?auth=wrong&session=../x");
            let err = tokio_tungstenite::connect_async(url)
                .await
                .expect_err("a bad token must be rejected");
            match err {
                tokio_tungstenite::tungstenite::Error::Http(resp) => {
                    assert_eq!(resp.status().as_u16(), 403);
                }
                other => panic!("expected HTTP 403, got {other:?}"),
            }
        }

        #[cfg(unix)]
        fn setup_lifecycle_fixtures(
            dir: &std::path::Path,
            paths: &zerodeck_platform::PlatformPaths,
        ) -> (
            std::path::PathBuf,
            std::path::PathBuf,
            std::path::PathBuf,
            std::path::PathBuf,
            String,
            std::path::PathBuf,
            String,
        ) {
            let sub = paths.user_home().join("sub");
            std::fs::create_dir_all(sub.join("nested")).expect("nested");
            std::fs::write(sub.join("sample.txt"), "hello").expect("sample.txt");
            let outside =
                std::env::temp_dir().join(format!("zd-srv-outside-{}.txt", uuid::Uuid::new_v4()));
            std::fs::write(&outside, "secret").expect("outside");
            std::os::unix::fs::symlink(&outside, sub.join("escape.txt")).expect("symlink");

            let marker = dir.join("uninstalled.marker");
            let zip1 = paths.user_home().join("demo1.zip");
            let sha1 = build_plugin_zip(
                &zip1,
                "Demo",
                r#"{"name":"Demo","author":"Alice","api_version":1}"#,
                &format!(
                    "from pathlib import Path\nclass Plugin:\n    async def _uninstall(self):\n        Path({:?}).write_text('done')\n    async def ping(self):\n        return 'ok'\n",
                    marker.display().to_string()
                ),
            );
            let zip2 = paths.user_home().join("demo2.zip");
            let sha2 = build_plugin_zip(
                &zip2,
                "Demo2",
                r#"{"name":"Demo2","author":"Bob"}"#,
                "class Plugin:\n    pass\n",
            );
            (sub, outside, marker, zip1, sha1, zip2, sha2)
        }

        async fn verify_lifecycle_and_settings<S>(ws: &mut S, events: &mut Vec<Value>)
        where
            S: SinkExt<Message>
                + StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
                + Unpin,
            <S as futures_util::Sink<Message>>::Error: std::fmt::Debug,
        {
            for (id, route, args) in [
                (4, "loader/disable_plugin", json!(["Demo"])),
                (5, "loader/enable_plugin", json!(["Demo"])),
                (6, "loader/set_plugin_order", json!([["Demo2", "Demo"]])),
                (
                    7,
                    "loader/set_setting",
                    json!(["developer.allow_unverified_installs", true]),
                ),
                (
                    8,
                    "loader/get_settings",
                    json!(["developer.allow_unverified_installs"]),
                ),
            ] {
                assert_eq!(
                    ws_call(ws, events, id, route, args).await["result"],
                    json!(true)
                );
            }
            let bad_set = ws_call(
                ws,
                events,
                9,
                "loader/set_setting",
                json!(["disabledPlugins", []]),
            )
            .await;
            assert_eq!(bad_set["type"], json!(-1));
            assert_eq!(bad_set["error"]["name"], json!("ValueError"));
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn ws_routes_cover_filepicker_install_lifecycle_and_settings() {
            let dir = std::env::temp_dir().join(format!("zd-srv-life-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("mkdir");
            let paths = server_test_paths(&dir);
            let (sub, outside, marker, zip1, sha1, zip2, sha2) =
                setup_lifecycle_fixtures(&dir, &paths);

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
            let (addr, token) =
                spawn_manager_server(ServerConfig::default(), manager.clone()).await;
            let mut ws = connect(addr, &token).await;
            let mut events = Vec::new();

            let fp = ws_call(
                &mut ws,
                &mut events,
                1,
                "utilities/filepicker",
                json!([sub]),
            )
            .await;
            let files = fp["result"]["files"].as_array().expect("files array");
            assert_eq!(files.len(), 2);
            assert_eq!(files[0]["name"], json!("nested"));
            assert_eq!(files[0]["isdir"], json!(true));
            assert_eq!(files[1]["name"], json!("sample.txt"));
            assert_eq!(files[1]["size"], json!(5));
            assert!(files[1]["modified"].as_f64().is_some());

            let install_uri = url::Url::from_file_path(&zip1).unwrap().to_string();
            let inst1 = ws_call(
                &mut ws,
                &mut events,
                2,
                "loader/install_plugin",
                json!([install_uri, "Demo", null, sha1, "local", true]),
            )
            .await;
            assert_eq!(inst1["result"]["name"], json!("Demo"));
            let _ = take_event(&mut ws, &mut events, "loader/plugins_changed").await;

            let inst2 = ws_call(
                &mut ws,
                &mut events,
                3,
                "utilities/install_plugin",
                json!([{"url": zip2.display().to_string(), "sha256": sha2, "user_confirmed": true}]),
            )
            .await;
            assert_eq!(inst2["result"]["name"], json!("Demo2"));
            let _ = take_event(&mut ws, &mut events, "loader/plugins_changed").await;

            verify_lifecycle_and_settings(&mut ws, &mut events).await;

            assert_eq!(
                ws_call(
                    &mut ws,
                    &mut events,
                    10,
                    "loader/uninstall_plugin",
                    json!(["Demo", false])
                )
                .await["result"],
                json!(true)
            );
            assert_eq!(std::fs::read_to_string(&marker).unwrap(), "done");

            manager.shutdown_all().await;
            let _ = std::fs::remove_dir_all(&dir);
            let _ = std::fs::remove_file(&outside);
        }
    }
}
