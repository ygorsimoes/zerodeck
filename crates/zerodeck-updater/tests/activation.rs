//! Fault-driven activation, recovery, request, and maintenance-lock tests.
//!
//! Every test uses the `Systemctl` seam and a local mock readiness endpoint. No test talks to
//! Steam, systemd, a catalog server, or a real daemon.

#![cfg(unix)]

use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
use uuid::Uuid;

use zerodeck_platform::MaintenanceLock;
use zerodeck_updater::{
    APPLY_REQUEST_SCHEMA, ActivationCheckpoint, ActivationFault, ApplyKind, ApplyRequest,
    ApplyRequestStore, AuthorizedRelease, CATALOG_STATE_SCHEMA, Catalog, CatalogState,
    CatalogStateStore, Channel, DAEMON_UNIT, GenerationId, GenerationStore, PendingTransaction,
    RecoveryMode, StateStore, Systemctl, SystemctlFuture, SystemdService, TransactionPhase,
    UpdateError, UpdateState, UpgradeOutcome, apply_pending, apply_pending_with, enqueue_rollback,
    enqueue_upgrade, recover,
};

const TOKEN: &str = "activation-test-token";
const TEST_DEADLINE: Duration = Duration::from_secs(5);

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "zerodeck-activation-{label}-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("create temp test dir");
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn generation_id(fill: char) -> GenerationId {
    GenerationId::new(fill.to_string().repeat(64)).expect("valid generation id")
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs()
}

fn release_for(generation: &GenerationId, version: &str, build_id: &str) -> AuthorizedRelease {
    AuthorizedRelease {
        channel: Channel::Stable,
        version: Version::parse(version).expect("valid version"),
        digest: generation.clone(),
        size: 1,
        build_id: build_id.to_owned(),
        min_updater: Version::parse("0.1.0").expect("valid updater version"),
        target: format!("linux-x86_64/zerodeck-{version}.tar.gz"),
        url: None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServiceCall {
    Stop,
    Start,
}

#[derive(Debug)]
struct Runtime {
    active: AtomicBool,
    invocation: Mutex<String>,
    invocation_counter: AtomicU64,
    calls: Mutex<Vec<ServiceCall>>,
    current_file: PathBuf,
    fail_build: Mutex<Option<String>>,
    readiness_hits: AtomicUsize,
}

impl Runtime {
    fn new(current_file: PathBuf) -> Self {
        Self {
            active: AtomicBool::new(false),
            invocation: Mutex::new(String::new()),
            invocation_counter: AtomicU64::new(0),
            calls: Mutex::new(Vec::new()),
            current_file,
            fail_build: Mutex::new(None),
            readiness_hits: AtomicUsize::new(0),
        }
    }

    fn set_running(&self, running: bool) {
        self.active.store(running, Ordering::SeqCst);
        let mut invocation = self.invocation.lock().expect("invocation lock");
        if running {
            "inv-initial".clone_into(&mut invocation);
        } else {
            invocation.clear();
        }
    }

    fn calls(&self) -> Vec<ServiceCall> {
        self.calls.lock().expect("call log lock").clone()
    }

    fn fail_build(&self, build_id: Option<&str>) {
        let mut fail = self.fail_build.lock().expect("fail build lock");
        *fail = build_id.map(str::to_owned);
    }
}

#[derive(Debug)]
struct MockSystemctl {
    runtime: Arc<Runtime>,
}

impl Systemctl for MockSystemctl {
    fn invocation_id<'a>(&'a self, _unit: &'a str) -> SystemctlFuture<'a, String> {
        Box::pin(async move {
            Ok(self
                .runtime
                .invocation
                .lock()
                .expect("invocation lock")
                .clone())
        })
    }

    fn is_active<'a>(&'a self, _unit: &'a str) -> SystemctlFuture<'a, bool> {
        Box::pin(async move { Ok(self.runtime.active.load(Ordering::SeqCst)) })
    }

    fn stop<'a>(&'a self, _unit: &'a str) -> SystemctlFuture<'a, ()> {
        Box::pin(async move {
            self.runtime
                .calls
                .lock()
                .expect("call log lock")
                .push(ServiceCall::Stop);
            self.runtime.active.store(false, Ordering::SeqCst);
            self.runtime
                .invocation
                .lock()
                .expect("invocation lock")
                .clear();
            Ok(())
        })
    }

    fn start<'a>(&'a self, _unit: &'a str) -> SystemctlFuture<'a, ()> {
        Box::pin(async move {
            self.runtime
                .calls
                .lock()
                .expect("call log lock")
                .push(ServiceCall::Start);
            self.runtime.active.store(true, Ordering::SeqCst);
            let next = self
                .runtime
                .invocation_counter
                .fetch_add(1, Ordering::SeqCst)
                + 1;
            *self.runtime.invocation.lock().expect("invocation lock") = format!("inv-{next}");
            Ok(())
        })
    }
}

#[derive(Clone)]
struct ServerState {
    runtime: Arc<Runtime>,
}

struct MockServer {
    endpoint: Url,
    handle: JoinHandle<()>,
}

impl MockServer {
    async fn start(runtime: Arc<Runtime>) -> Self {
        let state = ServerState { runtime };
        let app = Router::new()
            .route("/auth/token", get(token))
            .route("/readyz", get(readyz))
            .route("/frontend/index.js", get(frontend))
            .route("/ws", get(ws))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let addr: SocketAddr = listener.local_addr().expect("mock server address");
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve mock server");
        });
        let endpoint = Url::parse(&format!("http://{addr}")).expect("mock endpoint URL");
        Self { endpoint, handle }
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn token(State(server): State<ServerState>) -> String {
    let _ = server;
    TOKEN.to_owned()
}

async fn readyz(State(server): State<ServerState>, headers: HeaderMap) -> Response {
    let authorized = headers
        .get("x-decky-auth")
        .and_then(|value| value.to_str().ok())
        == Some(TOKEN);
    if !authorized {
        return (StatusCode::FORBIDDEN, "Forbidden").into_response();
    }
    server.runtime.readiness_hits.fetch_add(1, Ordering::SeqCst);
    let Some((version, build_id)) = served_identity(&server.runtime) else {
        return (StatusCode::SERVICE_UNAVAILABLE, "not ready").into_response();
    };
    let failed = server
        .runtime
        .fail_build
        .lock()
        .expect("fail build lock")
        .as_deref()
        == Some(build_id.as_str());
    let reported_build = if failed {
        "failed-readiness".to_owned()
    } else {
        build_id
    };
    (
        StatusCode::OK,
        Json(json!({
            "version": version,
            "build_id": reported_build,
            "python_host_ready": true,
        })),
    )
        .into_response()
}

async fn frontend() -> &'static str {
    "console.log('activation test frontend');\n"
}

async fn ws(
    upgrade: WebSocketUpgrade,
    State(server): State<ServerState>,
    RawQuery(query): RawQuery,
) -> Response {
    let authorized = query.as_deref().is_some_and(|query| {
        query
            .split('&')
            .any(|pair| pair.strip_prefix("auth=") == Some(TOKEN))
    });
    if !authorized {
        return (StatusCode::FORBIDDEN, "Forbidden").into_response();
    }
    upgrade
        .on_upgrade(move |socket| handle_socket(socket, server))
        .into_response()
}

async fn handle_socket(mut socket: WebSocket, _server: ServerState) {
    while let Some(Ok(message)) = socket.recv().await {
        let WsMessage::Text(text) = message else {
            continue;
        };
        let Ok(call) = serde_json::from_str::<Value>(text.as_str()) else {
            return;
        };
        let id = call.get("id").cloned().unwrap_or(Value::Null);
        let reply = json!({"type": 1, "id": id, "result": "pong"});
        if socket
            .send(WsMessage::Text(reply.to_string().into()))
            .await
            .is_err()
        {
            return;
        }
    }
}

fn served_identity(runtime: &Runtime) -> Option<(String, String)> {
    let reference = fs::read_to_string(&runtime.current_file).ok()?;
    let generation = reference.trim();
    let root = runtime.current_file.parent()?;
    let manifest = fs::read(
        root.join("generations")
            .join(generation)
            .join("release.json"),
    )
    .ok()?;
    let value: Value = serde_json::from_slice(&manifest).ok()?;
    let version = value.get("version")?.as_str()?.to_owned();
    let build_id = value.get("build_id")?.as_str()?.to_owned();
    Some((version, build_id))
}

struct Harness {
    _dir: TestDir,
    root: PathBuf,
    store: GenerationStore,
    state: StateStore,
    requests: ApplyRequestStore,
    service: SystemdService,
    runtime: Arc<Runtime>,
    _server: MockServer,
}

impl Harness {
    async fn new(label: &str) -> Self {
        let dir = TestDir::new(label);
        let root = dir.0.clone();
        let store = GenerationStore::new(&root).expect("open generation store");
        let state = StateStore::new(root.join("state/update.json"));
        let requests = ApplyRequestStore::for_state(&state);
        let runtime = Arc::new(Runtime::new(root.join("current")));
        let server = MockServer::start(Arc::clone(&runtime)).await;
        let systemctl = Arc::new(MockSystemctl {
            runtime: Arc::clone(&runtime),
        });
        let service = SystemdService::with_systemctl(
            DAEMON_UNIT,
            server.endpoint.clone(),
            Arc::clone(&systemctl) as Arc<dyn Systemctl>,
        );
        Self {
            _dir: dir,
            root,
            store,
            state,
            requests,
            service,
            runtime,
            _server: server,
        }
    }

    fn add_generation(
        &self,
        fill: char,
        version: &str,
        build_id: &str,
    ) -> (GenerationId, AuthorizedRelease) {
        let id = generation_id(fill);
        let generation = self.store.generation_dir(&id);
        fs::create_dir_all(generation.join("bin")).expect("create bin");
        fs::create_dir_all(generation.join("python")).expect("create python");
        fs::create_dir_all(generation.join("frontend")).expect("create frontend");
        let binary = generation.join("bin/zerodeckd");
        fs::write(&binary, b"#!/bin/sh\nexit 0\n").expect("write daemon");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))
                .expect("make daemon executable");
        }
        fs::write(
            generation.join("release.json"),
            format!(r#"{{"schema":1,"build_id":"{build_id}","version":"{version}"}}"#),
        )
        .expect("write release manifest");
        (id.clone(), release_for(&id, version, build_id))
    }

    fn set_catalog(&self, release: AuthorizedRelease, verified_at: u64, expires_at: u64) {
        CatalogStateStore::new(self.root.join("state/catalog.json"))
            .write(&CatalogState {
                schema: CATALOG_STATE_SCHEMA,
                last_verified_at: Some(verified_at),
                expires_at: Some(expires_at),
                authorization: Some(release),
            })
            .expect("write catalog state");
    }

    fn write_state(&self, state: &UpdateState) {
        self.state.write(state).expect("write update state");
    }

    fn read_state(&self) -> UpdateState {
        self.state.read().expect("read update state")
    }

    fn current(&self) -> Option<GenerationId> {
        self.store.current_reference().expect("read current")
    }

    fn catalog_bytes(&self) -> Option<Vec<u8>> {
        fs::read(self.root.join("state/catalog.json")).ok()
    }

    fn enqueue(&self, request: &ApplyRequest) {
        self.requests.enqueue(request).expect("enqueue request");
    }

    fn set_running(&self, running: bool) {
        self.runtime.set_running(running);
    }

    fn calls(&self) -> Vec<ServiceCall> {
        self.runtime.calls()
    }

    fn fail_build(&self, build_id: Option<&str>) {
        self.runtime.fail_build(build_id);
    }

    fn catalog(&self) -> Catalog {
        Catalog::open_offline(&self.root).expect("open offline catalog")
    }

    async fn apply(&self) -> Result<(), UpdateError> {
        let catalog = self.catalog();
        apply_pending(&self.store, &self.state, &catalog, &self.service).await
    }
}

#[derive(Debug)]
struct CrashAt {
    checkpoint: ActivationCheckpoint,
}

impl ActivationFault for CrashAt {
    fn checkpoint(&self, checkpoint: ActivationCheckpoint) -> Result<(), UpdateError> {
        if checkpoint == self.checkpoint {
            return Err(UpdateError::SimulatedCrash(checkpoint));
        }
        Ok(())
    }
}

fn assert_phase(harness: &Harness, phase: TransactionPhase) {
    let state = harness.read_state();
    let pending = state.pending.expect("pending transaction");
    assert_eq!(pending.phase, phase);
}

async fn crashed_at(checkpoint: ActivationCheckpoint) -> Harness {
    let harness = Harness::new(&format!("crash-{checkpoint:?}")).await;
    let (previous, _previous_release) = harness.add_generation('a', "1.0.0", "build-old");
    let (_, candidate_release) = harness.add_generation('b', "2.0.0", "build-new");
    harness
        .store
        .switch_to(&previous)
        .expect("activate previous");
    harness.write_state(&UpdateState {
        last_confirmed: Some(previous.clone()),
        ..UpdateState::new()
    });
    harness.set_catalog(candidate_release.clone(), now() - 10, now() + 3_600);
    harness.set_running(true);
    harness.enqueue(&ApplyRequest::upgrade(
        candidate_release,
        Some(previous),
        false,
    ));
    let error = apply_pending_with(
        &harness.store,
        &harness.state,
        &harness.catalog(),
        &harness.service,
        &CrashAt { checkpoint },
    )
    .await
    .expect_err("the injected process loss must abort activation");
    assert!(
        matches!(error, UpdateError::SimulatedCrash(point) if point == checkpoint),
        "unexpected fault result: {error:?}"
    );
    harness
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn activation_recovers_after_every_interrupted_transition() {
    let previous = generation_id('a');
    let candidate = generation_id('b');
    for checkpoint in [
        ActivationCheckpoint::BeforeIntent,
        ActivationCheckpoint::AfterIntent,
        ActivationCheckpoint::AfterStop,
        ActivationCheckpoint::BeforeSwitch,
        ActivationCheckpoint::AfterSwitch,
        ActivationCheckpoint::AfterStart,
        ActivationCheckpoint::BeforeConfirm,
        ActivationCheckpoint::AfterConfirm,
    ] {
        let harness = crashed_at(checkpoint).await;
        let expected_current = match checkpoint {
            ActivationCheckpoint::BeforeIntent => {
                assert!(harness.read_state().pending.is_none());
                assert!(harness.runtime.active.load(Ordering::SeqCst));
                previous.clone()
            }
            ActivationCheckpoint::AfterIntent => {
                assert_phase(&harness, TransactionPhase::Prepared);
                assert!(harness.runtime.active.load(Ordering::SeqCst));
                previous.clone()
            }
            ActivationCheckpoint::AfterStop | ActivationCheckpoint::BeforeSwitch => {
                assert_phase(&harness, TransactionPhase::Stopped);
                assert!(!harness.runtime.active.load(Ordering::SeqCst));
                previous.clone()
            }
            ActivationCheckpoint::AfterSwitch => {
                assert_phase(&harness, TransactionPhase::Switched);
                assert!(!harness.runtime.active.load(Ordering::SeqCst));
                candidate.clone()
            }
            ActivationCheckpoint::AfterStart | ActivationCheckpoint::BeforeConfirm => {
                assert_phase(&harness, TransactionPhase::Verifying);
                assert!(harness.runtime.active.load(Ordering::SeqCst));
                candidate.clone()
            }
            ActivationCheckpoint::AfterConfirm => {
                assert_phase(&harness, TransactionPhase::Confirmed);
                assert!(harness.runtime.active.load(Ordering::SeqCst));
                candidate.clone()
            }
            ActivationCheckpoint::AfterLock => unreachable!(),
        };
        assert_eq!(harness.current(), Some(expected_current), "{checkpoint:?}");

        let catalog_before = harness.catalog_bytes();
        let calls_before = harness.calls();
        let result = recover(
            &harness.store,
            &harness.state,
            &harness.service,
            RecoveryMode::AfterApply,
        )
        .await;

        if checkpoint == ActivationCheckpoint::AfterConfirm {
            result.expect("durable confirmation must be completed");
            assert_eq!(harness.current(), Some(candidate.clone()));
            let state = harness.read_state();
            assert_eq!(state.last_confirmed, Some(candidate.clone()));
            assert_eq!(state.rollback_target, Some(previous.clone()));
            assert!(state.pending.is_none());
            assert_eq!(harness.calls(), calls_before, "no restart after commit");
            assert_eq!(
                served_identity(&harness.runtime),
                Some(("2.0.0".to_owned(), "build-new".to_owned()))
            );
        } else {
            result.expect("interrupted activation must recover the previous identity");
            assert_eq!(harness.current(), Some(previous.clone()), "{checkpoint:?}");
            let state = harness.read_state();
            assert_eq!(state.last_confirmed, Some(previous.clone()));
            assert!(state.pending.is_none());
            assert!(
                harness.runtime.active.load(Ordering::SeqCst),
                "{checkpoint:?} must restore the daemon"
            );
            assert_eq!(
                served_identity(&harness.runtime),
                Some(("1.0.0".to_owned(), "build-old".to_owned())),
                "{checkpoint:?} must leave the old identity serving"
            );
            if matches!(
                checkpoint,
                ActivationCheckpoint::AfterStop
                    | ActivationCheckpoint::BeforeSwitch
                    | ActivationCheckpoint::AfterSwitch
                    | ActivationCheckpoint::AfterStart
                    | ActivationCheckpoint::BeforeConfirm
            ) {
                assert!(
                    harness.calls().contains(&ServiceCall::Start),
                    "{checkpoint:?} must start the previous generation"
                );
            }
        }
        assert_eq!(
            harness.catalog_bytes(),
            catalog_before,
            "recovery must not change trusted metadata"
        );
        assert!(
            harness
                .requests
                .read_optional()
                .expect("read request")
                .is_none(),
            "recovery must consume the owned request"
        );

        let state_before = fs::read(harness.state.path()).expect("state bytes");
        let calls_before_second = harness.calls();
        recover(
            &harness.store,
            &harness.state,
            &harness.service,
            RecoveryMode::AfterApply,
        )
        .await
        .expect("second recovery pass is idempotent");
        assert_eq!(
            fs::read(harness.state.path()).expect("state bytes"),
            state_before,
            "second recovery pass must not change state"
        );
        assert_eq!(harness.calls(), calls_before_second);
    }
}

#[tokio::test]
async fn activation_confirms_a_durable_successful_commit() {
    let harness = Harness::new("success").await;
    let (previous, _) = harness.add_generation('a', "1.0.0", "build-old");
    let (candidate, candidate_release) = harness.add_generation('b', "2.0.0", "build-new");
    harness
        .store
        .switch_to(&previous)
        .expect("activate previous");
    harness.write_state(&UpdateState {
        last_confirmed: Some(previous.clone()),
        ..UpdateState::new()
    });
    harness.set_catalog(candidate_release.clone(), now() - 10, now() + 3_600);
    harness.set_running(true);
    harness.enqueue(&ApplyRequest::upgrade(
        candidate_release,
        Some(previous.clone()),
        false,
    ));

    harness.apply().await.expect("activation succeeds");

    assert_eq!(harness.current(), Some(candidate.clone()));
    let state = harness.read_state();
    assert_eq!(state.last_confirmed, Some(candidate.clone()));
    assert_eq!(state.rollback_target, Some(previous));
    assert!(state.pending.is_none());
    assert_eq!(state.quarantined, Vec::new());
    assert!(
        state
            .last_result
            .as_deref()
            .is_some_and(|result| result.contains("confirmed"))
    );
    assert!(harness.runtime.active.load(Ordering::SeqCst));
    assert_eq!(harness.calls(), vec![ServiceCall::Stop, ServiceCall::Start]);
    assert_eq!(
        served_identity(&harness.runtime),
        Some(("2.0.0".to_owned(), "build-new".to_owned()))
    );
    assert!(
        harness
            .requests
            .read_optional()
            .expect("read request")
            .is_none()
    );
}

#[tokio::test]
async fn activation_leaves_a_recoverable_stopped_state_without_a_good_generation() {
    let harness = Harness::new("no-good-generation").await;
    let (candidate, candidate_release) = harness.add_generation('b', "2.0.0", "build-new");
    harness.set_catalog(candidate_release.clone(), now() - 10, now() + 3_600);
    harness.enqueue(&ApplyRequest::upgrade(candidate_release, None, true));

    let error = apply_pending_with(
        &harness.store,
        &harness.state,
        &harness.catalog(),
        &harness.service,
        &CrashAt {
            checkpoint: ActivationCheckpoint::AfterStart,
        },
    )
    .await
    .expect_err("the first install must be interrupted");
    assert!(matches!(error, UpdateError::SimulatedCrash(_)));

    let result = recover(
        &harness.store,
        &harness.state,
        &harness.service,
        RecoveryMode::AfterApply,
    )
    .await;
    assert!(
        matches!(result, Err(UpdateError::NoGoodGeneration)),
        "{result:?}"
    );
    assert_eq!(harness.current(), None);
    assert!(!harness.runtime.active.load(Ordering::SeqCst));
    let state = harness.read_state();
    assert!(state.pending.is_none());
    assert!(state.quarantined.contains(&candidate));
    assert!(
        state
            .last_result
            .as_deref()
            .is_some_and(|result| result.contains("stopped"))
    );
    assert!(state.last_error.is_some());
    assert!(
        harness
            .requests
            .read_optional()
            .expect("read request")
            .is_none()
    );

    let state_bytes = fs::read(harness.state.path()).expect("state bytes");
    let calls = harness.calls();
    recover(
        &harness.store,
        &harness.state,
        &harness.service,
        RecoveryMode::AfterApply,
    )
    .await
    .expect("the stopped state is idempotent");
    assert_eq!(
        fs::read(harness.state.path()).expect("state bytes"),
        state_bytes
    );
    assert_eq!(harness.calls(), calls);
    assert_eq!(harness.current(), None);
}

#[derive(Debug)]
struct ProbeFault {
    lock_path: PathBuf,
    observed: AtomicBool,
}

impl ActivationFault for ProbeFault {
    fn checkpoint(&self, checkpoint: ActivationCheckpoint) -> Result<(), UpdateError> {
        if checkpoint == ActivationCheckpoint::AfterLock {
            let error = MaintenanceLock::try_shared(&self.lock_path)
                .expect_err("apply must own the exclusive guard");
            assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
            self.observed.store(true, Ordering::SeqCst);
        }
        if checkpoint == ActivationCheckpoint::AfterIntent {
            return Err(UpdateError::SimulatedCrash(checkpoint));
        }
        Ok(())
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn activation_does_not_block_boot_recovery_or_interleave_with_plugins() {
    let harness = Harness::new("interleave").await;
    let (previous, _) = harness.add_generation('a', "1.0.0", "build-old");
    let (_, candidate_release) = harness.add_generation('b', "2.0.0", "build-new");
    harness
        .store
        .switch_to(&previous)
        .expect("activate previous");
    harness.write_state(&UpdateState {
        last_confirmed: Some(previous.clone()),
        ..UpdateState::new()
    });
    harness.set_catalog(candidate_release.clone(), now() - 10, now() + 3_600);
    harness.set_running(true);
    harness.enqueue(&ApplyRequest::upgrade(
        candidate_release.clone(),
        Some(previous.clone()),
        false,
    ));

    let shared = MaintenanceLock::try_shared(&harness.store.lock_path()).expect("shared guard");
    let error = harness
        .apply()
        .await
        .expect_err("a shared guard must block apply");
    assert!(matches!(error, UpdateError::Busy), "{error:?}");
    assert_eq!(harness.current(), Some(previous.clone()));
    assert!(harness.read_state().pending.is_none());
    assert!(
        harness.calls().is_empty(),
        "apply must fail before stopping"
    );
    assert!(
        harness
            .requests
            .read_optional()
            .expect("read request")
            .is_some()
    );
    drop(shared);

    let probe = ProbeFault {
        lock_path: harness.store.lock_path(),
        observed: AtomicBool::new(false),
    };
    let error = apply_pending_with(
        &harness.store,
        &harness.state,
        &harness.catalog(),
        &harness.service,
        &probe,
    )
    .await
    .expect_err("the probe stops after intent");
    assert!(matches!(error, UpdateError::SimulatedCrash(_)));
    assert!(
        probe.observed.load(Ordering::SeqCst),
        "apply did not hold the exclusive guard at the checkpoint"
    );

    let calls_before_boot = harness.calls();
    recover(
        &harness.store,
        &harness.state,
        &harness.service,
        RecoveryMode::BeforeBootStart,
    )
    .await
    .expect("boot recovery restores the reference");
    assert_eq!(harness.current(), Some(previous.clone()));
    assert!(harness.read_state().pending.is_none());
    assert_eq!(
        harness.calls(),
        calls_before_boot,
        "boot recovery must never start or stop the daemon"
    );
    assert!(harness.runtime.active.load(Ordering::SeqCst));

    let state_bytes = fs::read(harness.state.path()).expect("state bytes");
    recover(
        &harness.store,
        &harness.state,
        &harness.service,
        RecoveryMode::BeforeBootStart,
    )
    .await
    .expect("second boot recovery pass is idempotent");
    assert_eq!(
        fs::read(harness.state.path()).expect("state bytes"),
        state_bytes
    );
    assert_eq!(harness.calls(), calls_before_boot);

    // Stop the daemon and leave a pending transaction at `Stopped`. Recovery must wait for the
    // failed process's exclusive guard instead of stealing it, then restart the previous daemon.
    let stopped = crashed_at(ActivationCheckpoint::AfterStop).await;
    assert!(!stopped.runtime.active.load(Ordering::SeqCst));
    let guard = MaintenanceLock::try_exclusive(&stopped.store.lock_path())
        .expect("test owns the exclusive guard");
    let store = stopped.store.clone();
    let state = stopped.state.clone();
    let service = stopped.service.clone();
    let recovery =
        tokio::spawn(
            async move { recover(&store, &state, &service, RecoveryMode::AfterApply).await },
        );
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        !recovery.is_finished(),
        "post-stop recovery must wait for the process guard"
    );
    drop(guard);
    tokio::time::timeout(TEST_DEADLINE, recovery)
        .await
        .expect("recovery completes after the lock is released")
        .expect("recovery task")
        .expect("recovery restores and starts the previous daemon");
    assert_eq!(stopped.current(), Some(generation_id('a')));
    assert!(stopped.runtime.active.load(Ordering::SeqCst));
    assert!(stopped.calls().contains(&ServiceCall::Start));
}

#[tokio::test]
async fn ordinary_upgrade_leaves_an_intentionally_stopped_service_stopped() {
    let harness = Harness::new("stopped-upgrade").await;
    let (previous, _) = harness.add_generation('a', "1.0.0", "build-old");
    let (candidate, candidate_release) = harness.add_generation('b', "2.0.0", "build-new");
    harness
        .store
        .switch_to(&previous)
        .expect("activate previous");
    harness.write_state(&UpdateState {
        last_confirmed: Some(previous.clone()),
        ..UpdateState::new()
    });
    harness.set_catalog(candidate_release.clone(), now() - 10, now() + 3_600);
    harness.set_running(false);
    let catalog_before = harness.catalog_bytes();

    let outcome = enqueue_upgrade(&harness.store, &harness.state, &harness.service, false)
        .await
        .expect("stopped upgrade is reported, not activated");
    assert_eq!(
        outcome,
        UpgradeOutcome::RequiresStart {
            candidate: candidate.clone()
        }
    );
    assert_eq!(harness.current(), Some(previous.clone()));
    assert!(!harness.runtime.active.load(Ordering::SeqCst));
    assert!(
        harness.calls().is_empty(),
        "ordinary upgrade must not start/stop"
    );
    assert!(harness.read_state().pending.is_none());
    assert!(
        harness
            .requests
            .read_optional()
            .expect("read request")
            .is_none()
    );
    assert_eq!(harness.catalog_bytes(), catalog_before);
    assert!(harness.store.generation_dir(&candidate).is_dir());

    let outcome = enqueue_upgrade(&harness.store, &harness.state, &harness.service, true)
        .await
        .expect("--start enqueues the prepared candidate");
    let UpgradeOutcome::Requested { request } = outcome else {
        panic!("expected a durable request");
    };
    assert!(request.start_service);
    assert_eq!(request.expected_active, Some(previous));
    assert_eq!(request.generation, candidate);
    assert!(!harness.runtime.active.load(Ordering::SeqCst));
}

#[tokio::test]
async fn duplicate_and_foreign_apply_requests_are_rejected() {
    let interrupted = crashed_at(ActivationCheckpoint::AfterIntent).await;
    let calls = interrupted.calls();
    let error = interrupted
        .apply()
        .await
        .expect_err("a pending transaction rejects duplicates");
    assert!(
        matches!(error, UpdateError::DuplicateTransaction),
        "{error:?}"
    );
    assert_eq!(interrupted.calls(), calls);
    assert_eq!(interrupted.current(), Some(generation_id('a')));
    assert!(
        interrupted
            .requests
            .read_optional()
            .expect("read request")
            .is_some()
    );

    let stale = Harness::new("stale-request").await;
    let (previous, _) = stale.add_generation('a', "1.0.0", "build-old");
    let (_, candidate_release) = stale.add_generation('b', "2.0.0", "build-new");
    stale.store.switch_to(&previous).expect("activate previous");
    stale.set_catalog(candidate_release.clone(), now() - 10, now() + 3_600);
    stale.set_running(true);
    let mut request =
        ApplyRequest::upgrade(candidate_release.clone(), Some(generation_id('c')), false);
    request.id = Uuid::new_v4();
    stale.enqueue(&request);
    let error = stale
        .apply()
        .await
        .expect_err("a stale request is rejected");
    assert!(
        matches!(error, UpdateError::StaleRequest { .. }),
        "{error:?}"
    );
    assert_eq!(stale.calls(), Vec::new());
    assert_eq!(stale.current(), Some(previous.clone()));

    let mismatch = Harness::new("kind-mismatch").await;
    let (current, current_release) = mismatch.add_generation('a', "1.0.0", "build-old");
    let (other, _) = mismatch.add_generation('b', "2.0.0", "build-new");
    mismatch
        .store
        .switch_to(&current)
        .expect("activate current");
    mismatch.write_state(&UpdateState {
        last_confirmed: Some(current.clone()),
        rollback_target: Some(generation_id('c')),
        ..UpdateState::new()
    });
    mismatch.set_catalog(current_release.clone(), now() - 10, now() + 3_600);
    mismatch.set_running(true);
    mismatch.enqueue(&ApplyRequest::rollback(other, Some(current.clone())));
    let error = mismatch
        .apply()
        .await
        .expect_err("wrong rollback target rejected");
    assert!(
        matches!(error, UpdateError::RequestKindMismatch),
        "{error:?}"
    );
    assert_eq!(mismatch.calls(), Vec::new());

    let duplicate = Harness::new("duplicate-slot").await;
    let (_, release) = duplicate.add_generation('b', "2.0.0", "build-new");
    let request = ApplyRequest::upgrade(release, None, true);
    duplicate
        .requests
        .enqueue(&request)
        .expect("first slot write");
    let error = duplicate
        .requests
        .enqueue(&request)
        .expect_err("the single slot refuses overwrite");
    assert!(matches!(error, UpdateError::RequestPending), "{error:?}");
}

#[tokio::test]
async fn expiry_and_supersession_are_checked_before_stopping() {
    let expired = Harness::new("expired").await;
    let (previous, _) = expired.add_generation('a', "1.0.0", "build-old");
    let (_, candidate_release) = expired.add_generation('b', "2.0.0", "build-new");
    expired
        .store
        .switch_to(&previous)
        .expect("activate previous");
    expired.write_state(&UpdateState {
        last_confirmed: Some(previous.clone()),
        ..UpdateState::new()
    });
    expired.set_catalog(candidate_release.clone(), now() - 100, now() - 1);
    expired.set_running(true);
    expired.enqueue(&ApplyRequest::upgrade(
        candidate_release,
        Some(previous.clone()),
        false,
    ));
    let error = expired
        .apply()
        .await
        .expect_err("expired authorization must not stop");
    assert!(
        matches!(error, UpdateError::AuthorizationExpired { .. }),
        "{error:?}"
    );
    assert_eq!(expired.calls(), Vec::new());
    assert_eq!(expired.current(), Some(previous.clone()));
    assert!(expired.read_state().pending.is_none());

    let superseded = Harness::new("superseded").await;
    let (previous, _) = superseded.add_generation('a', "1.0.0", "build-old");
    let (_, old_release) = superseded.add_generation('b', "2.0.0", "build-new");
    let (_, newer_release) = superseded.add_generation('c', "3.0.0", "build-newer");
    superseded
        .store
        .switch_to(&previous)
        .expect("activate previous");
    superseded.write_state(&UpdateState {
        last_confirmed: Some(previous.clone()),
        ..UpdateState::new()
    });
    superseded.set_catalog(newer_release, now() - 10, now() + 3_600);
    superseded.set_running(true);
    superseded.enqueue(&ApplyRequest::upgrade(
        old_release,
        Some(previous.clone()),
        false,
    ));
    let error = superseded
        .apply()
        .await
        .expect_err("superseded release must not stop");
    assert!(
        matches!(error, UpdateError::ReleaseSuperseded { .. }),
        "{error:?}"
    );
    assert_eq!(superseded.calls(), Vec::new());
    assert_eq!(superseded.current(), Some(previous.clone()));

    let rolled_back_clock = Harness::new("clock-rollback").await;
    let (previous, _) = rolled_back_clock.add_generation('a', "1.0.0", "build-old");
    let (_, candidate_release) = rolled_back_clock.add_generation('b', "2.0.0", "build-new");
    rolled_back_clock
        .store
        .switch_to(&previous)
        .expect("activate previous");
    rolled_back_clock.write_state(&UpdateState {
        last_confirmed: Some(previous.clone()),
        ..UpdateState::new()
    });
    rolled_back_clock.set_catalog(candidate_release.clone(), now() + 3_600, now() + 7_200);
    rolled_back_clock.set_running(true);
    rolled_back_clock.enqueue(&ApplyRequest::upgrade(
        candidate_release,
        Some(previous.clone()),
        false,
    ));
    let error = rolled_back_clock
        .apply()
        .await
        .expect_err("clock rollback must not stop");
    assert!(
        matches!(error, UpdateError::ClockRollback { .. }),
        "{error:?}"
    );
    assert_eq!(rolled_back_clock.calls(), Vec::new());
    assert_eq!(rolled_back_clock.current(), Some(previous));
}

#[tokio::test]
async fn rollback_is_offline_and_rejects_a_quarantined_target() {
    let harness = Harness::new("rollback").await;
    let (target, _) = harness.add_generation('a', "1.0.0", "build-old");
    let (current, current_release) = harness.add_generation('b', "2.0.0", "build-new");
    harness.store.switch_to(&current).expect("activate current");
    harness.write_state(&UpdateState {
        last_confirmed: Some(current.clone()),
        rollback_target: Some(target.clone()),
        ..UpdateState::new()
    });
    harness.set_catalog(current_release, now() - 100, now() - 1);
    harness.set_running(true);
    let catalog_before = harness.catalog_bytes();

    let request = enqueue_rollback(&harness.store, &harness.state).expect("enqueue rollback");
    assert_eq!(request.kind, ApplyKind::Rollback);
    assert_eq!(request.expected_active, Some(current.clone()));
    assert_eq!(request.generation, target);
    harness
        .apply()
        .await
        .expect("explicit rollback ignores expired catalog metadata");
    assert_eq!(harness.current(), Some(target.clone()));
    let state = harness.read_state();
    assert_eq!(state.last_confirmed, Some(target.clone()));
    assert_eq!(state.rollback_target, Some(current.clone()));
    assert!(state.pending.is_none());
    assert_eq!(harness.catalog_bytes(), catalog_before);
    assert!(harness.runtime.active.load(Ordering::SeqCst));

    let quarantined = Harness::new("rollback-quarantined").await;
    let (target, _) = quarantined.add_generation('a', "1.0.0", "build-old");
    let (current, _) = quarantined.add_generation('b', "2.0.0", "build-new");
    quarantined
        .store
        .switch_to(&current)
        .expect("activate current");
    quarantined.write_state(&UpdateState {
        last_confirmed: Some(current.clone()),
        rollback_target: Some(target.clone()),
        quarantined: vec![target.clone()],
        ..UpdateState::new()
    });
    quarantined.set_running(true);
    let error = enqueue_rollback(&quarantined.store, &quarantined.state)
        .expect_err("a quarantined target cannot be requested");
    assert!(
        matches!(error, UpdateError::Quarantined(ref id) if id == &target),
        "{error:?}"
    );
    assert_eq!(quarantined.calls(), Vec::new());
    assert!(
        quarantined
            .requests
            .read_optional()
            .expect("read request")
            .is_none()
    );

    quarantined.enqueue(&ApplyRequest::rollback(
        target.clone(),
        Some(current.clone()),
    ));
    let error = quarantined
        .apply()
        .await
        .expect_err("apply also rejects quarantine");
    assert!(
        matches!(error, UpdateError::Quarantined(ref id) if id == &target),
        "{error:?}"
    );
    assert_eq!(quarantined.calls(), Vec::new());
    assert_eq!(quarantined.current(), Some(current));
    assert!(quarantined.read_state().pending.is_none());

    let stopped = Harness::new("rollback-stopped").await;
    let (target, _) = stopped.add_generation('a', "1.0.0", "build-old");
    let (current, current_release) = stopped.add_generation('b', "2.0.0", "build-new");
    stopped.store.switch_to(&current).expect("activate current");
    stopped.write_state(&UpdateState {
        last_confirmed: Some(current.clone()),
        rollback_target: Some(target.clone()),
        ..UpdateState::new()
    });
    stopped.set_catalog(current_release, now() - 100, now() - 1);
    stopped.set_running(false);
    let catalog_before = stopped.catalog_bytes();
    enqueue_rollback(&stopped.store, &stopped.state).expect("enqueue stopped rollback");
    stopped
        .apply()
        .await
        .expect("offline rollback restores the reference without starting");
    assert_eq!(stopped.current(), Some(target.clone()));
    assert!(!stopped.runtime.active.load(Ordering::SeqCst));
    assert!(
        stopped.calls().is_empty(),
        "stopped rollback needs no service calls"
    );
    assert_eq!(stopped.catalog_bytes(), catalog_before);
    assert_eq!(stopped.read_state().last_confirmed, Some(target));
}

#[tokio::test]
async fn upgrade_requires_start_for_a_first_install() {
    let harness = Harness::new("first-install").await;
    let (candidate, candidate_release) = harness.add_generation('b', "2.0.0", "build-new");
    harness.set_catalog(candidate_release, now() - 10, now() + 3_600);
    harness.set_running(false);

    let outcome = enqueue_upgrade(&harness.store, &harness.state, &harness.service, false)
        .await
        .expect("first install without --start is reported");
    assert_eq!(
        outcome,
        UpgradeOutcome::RequiresStart {
            candidate: candidate.clone()
        }
    );
    assert_eq!(harness.current(), None);
    assert!(
        harness
            .requests
            .read_optional()
            .expect("read request")
            .is_none()
    );
    assert!(harness.store.generation_dir(&candidate).is_dir());
}

#[tokio::test]
async fn boot_recovery_never_changes_services_or_trusted_metadata() {
    let harness = crashed_at(ActivationCheckpoint::AfterSwitch).await;
    let catalog_before = harness.catalog_bytes();
    let calls_before = harness.calls();
    let readiness_before = harness.runtime.readiness_hits.load(Ordering::SeqCst);

    recover(
        &harness.store,
        &harness.state,
        &harness.service,
        RecoveryMode::BeforeBootStart,
    )
    .await
    .expect("boot recovery restores the previous reference");

    assert_eq!(harness.current(), Some(generation_id('a')));
    assert!(harness.read_state().pending.is_none());
    assert_eq!(harness.calls(), calls_before);
    assert_eq!(
        harness.runtime.readiness_hits.load(Ordering::SeqCst),
        readiness_before,
        "boot recovery must not perform readiness requests"
    );
    assert_eq!(harness.catalog_bytes(), catalog_before);
    assert!(!harness.runtime.active.load(Ordering::SeqCst));

    let state_bytes = fs::read(harness.state.path()).expect("state bytes");
    recover(
        &harness.store,
        &harness.state,
        &harness.service,
        RecoveryMode::AfterApply,
    )
    .await
    .expect("after-apply recovery after boot recovery is a no-op");
    assert_eq!(
        fs::read(harness.state.path()).expect("state bytes"),
        state_bytes
    );
}

#[tokio::test]
async fn recovery_starts_the_previous_generation_with_the_same_readiness_gate() {
    let harness = crashed_at(ActivationCheckpoint::AfterStart).await;
    let previous = generation_id('a');
    harness.fail_build(Some("build-old"));
    let result = recover(
        &harness.store,
        &harness.state,
        &harness.service,
        RecoveryMode::AfterApply,
    )
    .await;
    assert!(
        matches!(result, Err(UpdateError::NoGoodGeneration)),
        "{result:?}"
    );
    assert_eq!(harness.current(), None);
    assert!(!harness.runtime.active.load(Ordering::SeqCst));
    let state = harness.read_state();
    assert!(state.quarantined.contains(&generation_id('b')));
    assert!(state.pending.is_none());
    assert!(
        state
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains(&previous.to_string()))
    );
}

#[tokio::test]
async fn request_store_is_private_and_roundtrips_the_authorized_release() {
    use std::os::unix::fs::PermissionsExt;

    let harness = Harness::new("request-store").await;
    let (_, release) = harness.add_generation('b', "2.0.0", "build-new");
    let request = ApplyRequest::upgrade(release.clone(), Some(generation_id('a')), true);
    harness.requests.write(&request).expect("write request");

    let mode = fs::metadata(harness.requests.path())
        .expect("request metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode, 0o600,
        "the root-owned request must not be world writable"
    );
    let read = harness.requests.read().expect("read request");
    assert_eq!(read, request);
    assert_eq!(read.schema, APPLY_REQUEST_SCHEMA);
    assert_eq!(
        read.release.as_ref().expect("release").digest,
        release.digest
    );

    fs::set_permissions(harness.requests.path(), fs::Permissions::from_mode(0o666))
        .expect("make request world writable");
    let error = harness
        .requests
        .read()
        .expect_err("a request writable by another user must be rejected");
    assert!(
        matches!(error, UpdateError::InvalidApplyRequest(_)),
        "{error:?}"
    );
}

#[tokio::test]
async fn malformed_apply_requests_are_rejected_without_stopping() {
    let harness = Harness::new("malformed-request").await;
    let (previous, _) = harness.add_generation('a', "1.0.0", "build-old");
    harness
        .store
        .switch_to(&previous)
        .expect("activate previous");
    harness.write_state(&UpdateState::new());
    harness.set_running(true);

    fs::write(harness.requests.path(), b"not json").expect("seed malformed request");
    let error = harness
        .apply()
        .await
        .expect_err("malformed JSON is rejected");
    assert!(
        matches!(error, UpdateError::InvalidApplyRequest(_)),
        "{error:?}"
    );
    assert_eq!(harness.calls(), Vec::new());

    let request = ApplyRequest {
        schema: 2,
        id: Uuid::new_v4(),
        kind: ApplyKind::Upgrade,
        generation: previous.clone(),
        expected_active: Some(previous.clone()),
        release: None,
        start_service: true,
    };
    fs::write(
        harness.requests.path(),
        serde_json::to_vec(&request).expect("serialize request"),
    )
    .expect("seed unsupported schema");
    let error = harness
        .apply()
        .await
        .expect_err("unsupported schema is rejected");
    assert!(
        matches!(error, UpdateError::InvalidApplyRequest(_)),
        "{error:?}"
    );
    assert_eq!(harness.calls(), Vec::new());
}

#[tokio::test]
async fn failed_candidate_is_quarantined_and_previous_is_kept() {
    let harness = Harness::new("candidate-failure").await;
    let (previous, _) = harness.add_generation('a', "1.0.0", "build-old");
    let (candidate, candidate_release) = harness.add_generation('b', "2.0.0", "build-new");
    harness
        .store
        .switch_to(&previous)
        .expect("activate previous");
    harness.write_state(&UpdateState {
        last_confirmed: Some(previous.clone()),
        ..UpdateState::new()
    });
    harness.set_catalog(candidate_release.clone(), now() - 10, now() + 3_600);
    harness.set_running(true);
    harness.fail_build(Some("build-new"));
    harness.enqueue(&ApplyRequest::upgrade(
        candidate_release,
        Some(previous.clone()),
        false,
    ));

    let error = harness
        .apply()
        .await
        .expect_err("a candidate that cannot serve must fail");
    assert!(
        matches!(error, UpdateError::ReadinessIdentityMismatch { .. }),
        "{error:?}"
    );
    assert_eq!(harness.current(), Some(previous.clone()));
    assert!(harness.runtime.active.load(Ordering::SeqCst));
    let state = harness.read_state();
    assert_eq!(state.last_confirmed, Some(previous));
    assert!(state.pending.is_none());
    assert!(state.quarantined.contains(&candidate));
    assert!(state.last_error.is_some());
}

#[test]
fn exec_current_validates_the_layout_and_clears_standalone_overrides() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    let dir = TestDir::new("exec-current");
    let root = dir.0.clone();
    let store = GenerationStore::new(&root).expect("open generation store");
    let generation = generation_id('d');
    let generation_root = store.generation_dir(&generation);
    fs::create_dir_all(generation_root.join("bin")).expect("create bin");
    fs::create_dir(generation_root.join("python")).expect("create python");
    fs::create_dir(generation_root.join("frontend")).expect("create frontend");
    fs::write(
        generation_root.join("release.json"),
        br#"{"schema":1,"build_id":"build-a","version":"1.2.3"}"#,
    )
    .expect("write manifest");
    let output_path = root.join("exec-observed.txt");
    let binary = generation_root.join("bin/zerodeckd");
    fs::write(
        &binary,
        format!(
            "#!/bin/sh\n{{\n  printf 'arg=%s\\n' \"$@\"\n  printf 'PYTHONPATH=%s\\n' \"${{PYTHONPATH-__unset__}}\"\n  printf 'ZERODECK_FRONTEND_DIR=%s\\n' \"${{ZERODECK_FRONTEND_DIR-__unset__}}\"\n  printf 'pid=%s\\n' \"$$\"\n}} > \"{}\"\n",
            output_path.display()
        ),
    )
    .expect("write fake daemon");
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))
        .expect("make fake daemon executable");
    store.switch_to(&generation).expect("activate generation");

    let output = Command::new(env!("CARGO_BIN_EXE_zerodeckctl"))
        .args(["exec-current", "--root"])
        .arg(&root)
        .env("PYTHONPATH", "/legacy/python")
        .env("ZERODECK_FRONTEND_DIR", "/legacy/frontend")
        .env("ZERODECK_EXEC_OUT", &output_path)
        .output()
        .expect("run exec-current");
    assert!(
        output.status.success(),
        "exec-current failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let observed = fs::read_to_string(&output_path).expect("read observed exec");
    assert!(observed.contains("arg=--managed-generation"), "{observed}");
    let canonical_generation = fs::canonicalize(&generation_root).expect("canonical generation");
    assert!(
        observed.contains(&format!("arg={}", canonical_generation.display())),
        "{observed}"
    );
    assert!(observed.contains("PYTHONPATH=__unset__"), "{observed}");
    assert!(
        observed.contains("ZERODECK_FRONTEND_DIR=__unset__"),
        "{observed}"
    );
    assert!(observed.contains("pid="), "{observed}");

    fs::remove_dir_all(generation_root.join("frontend")).expect("break layout");
    let output = Command::new(env!("CARGO_BIN_EXE_zerodeckctl"))
        .args(["exec-current", "--root"])
        .arg(&root)
        .output()
        .expect("run exec-current with a broken layout");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("incomplete or corrupt"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn duplicate_pending_transaction_is_rejected_without_touching_the_service() {
    let harness = Harness::new("pending-duplicate").await;
    let (previous, _) = harness.add_generation('a', "1.0.0", "build-old");
    harness
        .store
        .switch_to(&previous)
        .expect("activate previous");
    harness.write_state(&UpdateState {
        last_confirmed: Some(previous.clone()),
        pending: Some(PendingTransaction {
            id: Uuid::new_v4(),
            previous: Some(previous.clone()),
            candidate: generation_id('b'),
            phase: TransactionPhase::Switched,
            was_running: true,
        }),
        ..UpdateState::new()
    });
    harness.set_running(true);
    let error = harness
        .apply()
        .await
        .expect_err("existing transaction blocks apply");
    assert!(
        matches!(
            error,
            UpdateError::MissingApplyRequest | UpdateError::DuplicateTransaction
        ),
        "{error:?}"
    );
    assert_eq!(harness.calls(), Vec::new());
}
