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
    }
}
