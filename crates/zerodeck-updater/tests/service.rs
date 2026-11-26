//! Readiness verification against a local mock loader and a `systemctl` seam.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use semver::Version;
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use url::Url;
use zerodeck_updater::{
    ReleaseIdentity, Systemctl, SystemctlFuture, SystemdService, UpdateError, verify_ready,
};

const TOKEN: &str = "readiness-test-token";
const TEST_DEADLINE: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy)]
enum PingMode {
    Pong,
    ErrorReply,
}

#[derive(Debug)]
struct MockState {
    token: String,
    token_body: String,
    ws_token: String,
    ready_status: u16,
    ready: Value,
    frontend_status: u16,
    frontend_body: &'static str,
    ping: PingMode,
    ws_failures: AtomicUsize,
}

impl MockState {
    fn healthy() -> Self {
        Self {
            token: TOKEN.to_owned(),
            token_body: TOKEN.to_owned(),
            ws_token: TOKEN.to_owned(),
            ready_status: 200,
            ready: json!({
                "version": "1.0.0",
                "build_id": "build-new",
                "python_host_ready": true,
            }),
            frontend_status: 200,
            frontend_body: "console.log('managed frontend');\n",
            ping: PingMode::Pong,
            ws_failures: AtomicUsize::new(0),
        }
    }
}

struct MockServer {
    addr: SocketAddr,
    handle: JoinHandle<()>,
}

impl MockServer {
    async fn start(state: MockState) -> Self {
        let state = Arc::new(state);
        let app = Router::new()
            .route("/auth/token", get(token))
            .route("/readyz", get(readyz))
            .route("/frontend/index.js", get(frontend))
            .route("/ws", get(ws))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let addr = listener.local_addr().expect("mock server address");
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve mock server");
        });
        Self { addr, handle }
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn token(State(state): State<Arc<MockState>>) -> String {
    state.token_body.clone()
}

async fn readyz(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
) -> (StatusCode, Json<Value>) {
    let authorized = headers
        .get("x-decky-auth")
        .and_then(|value| value.to_str().ok())
        == Some(state.token.as_str());
    if !authorized {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "Forbidden"})));
    }
    (
        StatusCode::from_u16(state.ready_status).expect("valid mock status"),
        Json(state.ready.clone()),
    )
}

async fn frontend(State(state): State<Arc<MockState>>) -> (StatusCode, &'static str) {
    (
        StatusCode::from_u16(state.frontend_status).expect("valid mock status"),
        state.frontend_body,
    )
}

async fn ws(
    upgrade: WebSocketUpgrade,
    State(state): State<Arc<MockState>>,
    RawQuery(query): RawQuery,
) -> Response {
    let authorized = query.as_deref().is_some_and(|query| {
        query
            .split('&')
            .any(|pair| pair.strip_prefix("auth=") == Some(state.ws_token.as_str()))
    });
    if !authorized {
        return (StatusCode::FORBIDDEN, "Forbidden").into_response();
    }
    if state.ws_failures.load(Ordering::SeqCst) > 0 {
        state.ws_failures.fetch_sub(1, Ordering::SeqCst);
        return (StatusCode::SERVICE_UNAVAILABLE, "try later").into_response();
    }
    upgrade
        .on_upgrade(move |socket| handle_socket(socket, state))
        .into_response()
}

async fn handle_socket(mut socket: WebSocket, state: Arc<MockState>) {
    while let Some(Ok(message)) = socket.recv().await {
        let WsMessage::Text(text) = message else {
            continue;
        };
        let Ok(call) = serde_json::from_str::<Value>(text.as_str()) else {
            return;
        };
        let id = call.get("id").cloned().unwrap_or(Value::Null);
        let valid_call = call.get("type").and_then(Value::as_u64) == Some(0)
            && call.get("route").and_then(Value::as_str) == Some("utilities/ping")
            && call
                .get("args")
                .is_some_and(|args| args.as_array().is_some_and(Vec::is_empty));
        if !valid_call {
            let error = json!({
                "type": 2,
                "id": id,
                "error": {"name": "InvalidCall", "message": "expected utilities/ping CALL"},
            });
            let _ = socket.send(WsMessage::Text(error.to_string().into())).await;
            return;
        }
        let reply = match state.ping {
            PingMode::Pong => json!({"type": 1, "id": id, "result": "pong"}),
            PingMode::ErrorReply => json!({
                "type": 2,
                "id": id,
                "error": {"name": "Boom", "message": "ping failed"},
            }),
        };
        if socket
            .send(WsMessage::Text(reply.to_string().into()))
            .await
            .is_err()
        {
            return;
        }
    }
}

#[derive(Debug)]
struct MockSystemctl {
    invocation: Mutex<String>,
}

impl MockSystemctl {
    fn new(invocation: &str) -> Arc<Self> {
        Arc::new(Self {
            invocation: Mutex::new(invocation.to_owned()),
        })
    }

    fn set(&self, invocation: &str) {
        let mut current = self.invocation.lock().expect("mock invocation lock");
        invocation.clone_into(&mut current);
    }
}

impl Systemctl for MockSystemctl {
    fn invocation_id<'a>(&'a self, _unit: &'a str) -> SystemctlFuture<'a, String> {
        Box::pin(async move {
            Ok(self
                .invocation
                .lock()
                .expect("mock invocation lock")
                .clone())
        })
    }
}

struct Harness {
    _server: MockServer,
    service: SystemdService,
    systemctl: Arc<MockSystemctl>,
    expected: ReleaseIdentity,
}

impl Harness {
    async fn start(state: MockState) -> Self {
        let server = MockServer::start(state).await;
        let endpoint = Url::parse(&format!("http://{}", server.addr)).expect("mock endpoint URL");
        let systemctl = MockSystemctl::new("invocation-1");
        let service = SystemdService::with_systemctl(
            "zerodeck.service",
            endpoint,
            Arc::clone(&systemctl) as Arc<dyn Systemctl>,
        );
        service
            .begin_activation()
            .await
            .expect("record pre-activation invocation");
        Self {
            _server: server,
            service,
            systemctl,
            expected: ReleaseIdentity::new(
                Version::parse("1.0.0").expect("valid version"),
                "build-new",
            ),
        }
    }

    async fn verify(&self) -> Result<(), UpdateError> {
        self.verify_with(TEST_DEADLINE).await
    }

    async fn verify_with(&self, deadline: Duration) -> Result<(), UpdateError> {
        verify_ready(&self.service, &self.expected, deadline).await
    }
}

#[tokio::test]
async fn readiness_accepts_new_invocation_with_matching_identity() {
    let harness = Harness::start(MockState::healthy()).await;
    harness.systemctl.set("invocation-2");

    harness
        .verify()
        .await
        .expect("a new invocation with matching identity is ready");
}

#[tokio::test]
async fn readiness_rejects_old_invocation_and_wrong_build() {
    let old = Harness::start(MockState::healthy()).await;
    let err = old
        .verify()
        .await
        .expect_err("the same systemd invocation must not be accepted");
    assert!(
        matches!(err, UpdateError::ServiceNotRestarted(ref unit) if unit == "zerodeck.service"),
        "{err}"
    );

    let mut state = MockState::healthy();
    state.ready["build_id"] = json!("build-old");
    let wrong_build = Harness::start(state).await;
    wrong_build.systemctl.set("invocation-2");
    let err = wrong_build
        .verify()
        .await
        .expect_err("a 200 response with the wrong build must not be accepted");
    assert!(
        matches!(
            err,
            UpdateError::ReadinessIdentityMismatch {
                field: "build_id",
                ..
            }
        ),
        "{err}"
    );
}

#[tokio::test]
async fn readiness_rejects_wrong_version() {
    let mut state = MockState::healthy();
    state.ready["version"] = json!("0.9.0");
    let harness = Harness::start(state).await;
    harness.systemctl.set("invocation-2");

    let err = harness
        .verify()
        .await
        .expect_err("a 200 response with the wrong version must not be accepted");
    assert!(
        matches!(
            err,
            UpdateError::ReadinessIdentityMismatch {
                field: "version",
                ..
            }
        ),
        "{err}"
    );
}

#[tokio::test]
async fn readiness_rejects_missing_frontend_asset() {
    let mut state = MockState::healthy();
    state.frontend_status = 404;
    let harness = Harness::start(state).await;
    harness.systemctl.set("invocation-2");

    let err = harness
        .verify()
        .await
        .expect_err("the expected frontend asset must be served");
    assert!(
        matches!(
            err,
            UpdateError::ReadyStatus {
                path: "/frontend/index.js",
                status: 404
            }
        ),
        "{err}"
    );
}

#[tokio::test]
async fn readiness_rejects_python_host_not_ready() {
    let mut state = MockState::healthy();
    state.ready["python_host_ready"] = json!(false);
    let harness = Harness::start(state).await;
    harness.systemctl.set("invocation-2");

    let err = harness
        .verify()
        .await
        .expect_err("Python import readiness must gate activation");
    assert!(matches!(err, UpdateError::PythonHostNotReady), "{err}");
}

#[tokio::test]
async fn readiness_rejects_authenticated_ping_failure() {
    let mut state = MockState::healthy();
    state.ping = PingMode::ErrorReply;
    let harness = Harness::start(state).await;
    harness.systemctl.set("invocation-2");

    let err = harness
        .verify()
        .await
        .expect_err("a failed authenticated RPC ping must not be accepted");
    assert!(matches!(err, UpdateError::ReadinessRpc(_)), "{err}");
}

#[tokio::test]
async fn readiness_rejects_unauthorized_ws() {
    let mut state = MockState::healthy();
    state.ws_token = "different-ws-token".to_owned();
    let harness = Harness::start(state).await;
    harness.systemctl.set("invocation-2");

    let started = Instant::now();
    let err = harness
        .verify()
        .await
        .expect_err("the readiness WebSocket must require the auth token");
    let message = err.to_string();
    assert!(matches!(err, UpdateError::ReadinessRpc(_)), "{message}");
    assert!(!message.contains(TOKEN), "the auth token leaked: {message}");
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "an unauthorized WS is a permanent failure, took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn readiness_rejects_oversized_auth_token_body() {
    let mut state = MockState::healthy();
    state.token_body = "x".repeat(70 * 1024);
    let harness = Harness::start(state).await;
    harness.systemctl.set("invocation-2");

    let err = harness
        .verify()
        .await
        .expect_err("an oversized auth-token body must be rejected");
    assert!(matches!(err, UpdateError::InvalidReadyResponse(_)), "{err}");
}

#[tokio::test]
async fn readiness_retries_transient_ws_failure() {
    let mut state = MockState::healthy();
    state.ws_failures = AtomicUsize::new(1);
    let harness = Harness::start(state).await;
    harness.systemctl.set("invocation-2");

    harness
        .verify_with(Duration::from_secs(2))
        .await
        .expect("a transient WebSocket failure must be retried");
}

#[tokio::test]
async fn readiness_does_not_retry_permanent_failures() {
    let mut state = MockState::healthy();
    state.ready["build_id"] = json!("build-old");
    let harness = Harness::start(state).await;
    harness.systemctl.set("invocation-2");

    let started = Instant::now();
    let err = harness
        .verify_with(Duration::from_secs(2))
        .await
        .expect_err("the wrong build is a permanent readiness failure");
    assert!(
        matches!(
            err,
            UpdateError::ReadinessIdentityMismatch {
                field: "build_id",
                ..
            }
        ),
        "{err}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "a permanent identity mismatch must not be retried, took {:?}",
        started.elapsed()
    );
}
