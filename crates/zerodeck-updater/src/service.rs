//! systemd service identity and post-restart readiness verification.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use semver::Version;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use url::Url;

use crate::UpdateError;

/// Activation readiness deadline for a freshly restarted loader.
pub const ACTIVATION_DEADLINE: Duration = Duration::from_secs(60);

const READINESS_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(5);
const READINESS_RETRY_INTERVAL: Duration = Duration::from_millis(100);
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(5);
const SYSTEMCTL_STOP_TIMEOUT: Duration = Duration::from_secs(30);
const SYSTEMCTL_START_TIMEOUT: Duration = Duration::from_secs(10);
const AUTH_TOKEN_PATH: &str = "/auth/token";
const READY_PATH: &str = "/readyz";
const FRONTEND_ASSET_PATH: &str = "/frontend/index.js";
const WS_PATH: &str = "/ws";
const READY_RPC_ID: u64 = 1;
const MAX_READY_BODY_BYTES: usize = 64 * 1024;
const MAX_FRONTEND_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Boxed future returned by [`Systemctl`] operations.
pub type SystemctlFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, UpdateError>> + Send + 'a>>;

/// Process seam for the `systemctl` queries and transitions used by activation.
///
/// The default implementations fail closed so older mocks that only provide
/// [`Systemctl::invocation_id`] keep compiling without gaining process powers.
pub trait Systemctl: std::fmt::Debug + Send + Sync {
    /// Returns the unit's current systemd `InvocationID`, or an empty string when inactive.
    fn invocation_id<'a>(&'a self, unit: &'a str) -> SystemctlFuture<'a, String>;

    /// Returns whether the unit is currently active.
    fn is_active<'a>(&'a self, unit: &'a str) -> SystemctlFuture<'a, bool> {
        Box::pin(async move {
            Err(UpdateError::SystemctlUnsupported {
                unit: unit.to_owned(),
                operation: "is-active",
            })
        })
    }

    /// Returns the unit's main PID, or `0` when the unit has no running main process.
    fn main_pid<'a>(&'a self, unit: &'a str) -> SystemctlFuture<'a, u32> {
        Box::pin(async move {
            Err(UpdateError::SystemctlUnsupported {
                unit: unit.to_owned(),
                operation: "main-pid",
            })
        })
    }

    /// Stops the unit and waits for it to become inactive.
    fn stop<'a>(&'a self, unit: &'a str) -> SystemctlFuture<'a, ()> {
        Box::pin(async move {
            Err(UpdateError::SystemctlUnsupported {
                unit: unit.to_owned(),
                operation: "stop",
            })
        })
    }

    /// Starts the unit without waiting for its readiness notification.
    fn start<'a>(&'a self, unit: &'a str) -> SystemctlFuture<'a, ()> {
        Box::pin(async move {
            Err(UpdateError::SystemctlUnsupported {
                unit: unit.to_owned(),
                operation: "start",
            })
        })
    }
}

/// Production [`Systemctl`] backed by the host's `systemctl` binary.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioSystemctl;

impl Systemctl for TokioSystemctl {
    fn invocation_id<'a>(&'a self, unit: &'a str) -> SystemctlFuture<'a, String> {
        Box::pin(async move {
            let output = run_systemctl(
                unit,
                "invocation-id",
                &["show", "--property=InvocationID", "--value"],
                SYSTEMCTL_TIMEOUT,
            )
            .await?;
            if !output.status.success() {
                return Err(systemctl_status_error(unit, "invocation-id", &output));
            }
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
        })
    }

    fn is_active<'a>(&'a self, unit: &'a str) -> SystemctlFuture<'a, bool> {
        Box::pin(async move {
            let output = run_systemctl(
                unit,
                "is-active",
                &["is-active", "--quiet"],
                SYSTEMCTL_TIMEOUT,
            )
            .await?;
            match output.status.code() {
                Some(0) => Ok(true),
                Some(3 | 4) => Ok(false),
                _ => Err(systemctl_status_error(unit, "is-active", &output)),
            }
        })
    }

    fn main_pid<'a>(&'a self, unit: &'a str) -> SystemctlFuture<'a, u32> {
        Box::pin(async move {
            let output = run_systemctl(
                unit,
                "main-pid",
                &["show", "--property=MainPID", "--value"],
                SYSTEMCTL_TIMEOUT,
            )
            .await?;
            if !output.status.success() {
                return Err(systemctl_status_error(unit, "main-pid", &output));
            }
            let value = String::from_utf8_lossy(&output.stdout);
            value
                .trim()
                .parse::<u32>()
                .map_err(|error| UpdateError::Systemctl {
                    unit: unit.to_owned(),
                    operation: "main-pid",
                    reason: format!("invalid MainPID `{}`: {error}", value.trim()),
                })
        })
    }

    fn stop<'a>(&'a self, unit: &'a str) -> SystemctlFuture<'a, ()> {
        Box::pin(async move {
            let output = run_systemctl(unit, "stop", &["stop"], SYSTEMCTL_STOP_TIMEOUT).await?;
            if !output.status.success() {
                return Err(systemctl_status_error(unit, "stop", &output));
            }
            Ok(())
        })
    }

    fn start<'a>(&'a self, unit: &'a str) -> SystemctlFuture<'a, ()> {
        Box::pin(async move {
            // Readiness is owned by `verify_ready`; do not let systemd's default start timeout
            // outlive the activation deadline.
            let output = run_systemctl(
                unit,
                "start",
                &["start", "--no-block"],
                SYSTEMCTL_START_TIMEOUT,
            )
            .await?;
            if !output.status.success() {
                return Err(systemctl_status_error(unit, "start", &output));
            }
            Ok(())
        })
    }
}

async fn run_systemctl(
    unit: &str,
    operation: &'static str,
    args: &[&str],
    timeout: Duration,
) -> Result<std::process::Output, UpdateError> {
    let mut command = tokio::process::Command::new("systemctl");
    command.args(args).args(["--", unit]).kill_on_drop(true);
    tokio::time::timeout(timeout, command.output())
        .await
        .map_err(|_| UpdateError::Systemctl {
            unit: unit.to_owned(),
            operation,
            reason: format!("timed out after {timeout:?}"),
        })?
        .map_err(|error| UpdateError::Systemctl {
            unit: unit.to_owned(),
            operation,
            reason: error.to_string(),
        })
}

fn systemctl_status_error(
    unit: &str,
    operation: &'static str,
    output: &std::process::Output,
) -> UpdateError {
    let stderr = String::from_utf8_lossy(&output.stderr);
    UpdateError::Systemctl {
        unit: unit.to_owned(),
        operation,
        reason: format!("exited with {}: {}", output.status, stderr.trim()),
    }
}

/// Expected release identity for the generation that was just activated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseIdentity {
    /// Release version reported by the daemon.
    pub version: Version,
    /// Embedded build ID reported by the daemon.
    pub build_id: String,
}

impl ReleaseIdentity {
    /// Creates a release identity from a version and build ID.
    #[must_use]
    pub fn new(version: Version, build_id: impl Into<String>) -> Self {
        Self {
            version,
            build_id: build_id.into(),
        }
    }
}

/// One systemd service plus the local endpoint used to verify that it became ready.
#[derive(Debug, Clone)]
pub struct SystemdService {
    unit: String,
    endpoint: Url,
    client: reqwest::Client,
    systemctl: Arc<dyn Systemctl>,
    invocation_before_restart: Arc<Mutex<Option<String>>>,
}

impl SystemdService {
    /// Creates a service using the host's `systemctl` and a local HTTP endpoint.
    #[must_use]
    pub fn new(unit: impl Into<String>, endpoint: Url) -> Self {
        Self::with_systemctl(unit, endpoint, Arc::new(TokioSystemctl))
    }

    /// Creates a service with an injected `systemctl` seam.
    #[doc(hidden)]
    #[must_use]
    pub fn with_systemctl(
        unit: impl Into<String>,
        endpoint: Url,
        systemctl: Arc<dyn Systemctl>,
    ) -> Self {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(2))
            .build()
            .expect("build local readiness HTTP client");
        Self {
            unit: unit.into(),
            endpoint,
            client,
            systemctl,
            invocation_before_restart: Arc::new(Mutex::new(None)),
        }
    }

    /// Returns the managed systemd unit name.
    #[must_use]
    pub fn unit(&self) -> &str {
        &self.unit
    }

    /// Returns the local daemon endpoint used by readiness checks.
    #[must_use]
    pub fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    /// Records the current invocation before stopping or restarting the unit.
    pub async fn begin_activation(&self) -> Result<(), UpdateError> {
        let invocation = self.systemctl.invocation_id(&self.unit).await?;
        let mut baseline = self
            .invocation_before_restart
            .lock()
            .map_err(|_| UpdateError::ReadinessStatePoisoned)?;
        *baseline = Some(invocation);
        Ok(())
    }

    /// Returns whether the managed daemon is currently active.
    pub async fn is_active(&self) -> Result<bool, UpdateError> {
        self.systemctl.is_active(&self.unit).await
    }

    /// Stops the managed daemon and waits for it to become inactive.
    pub async fn stop(&self) -> Result<(), UpdateError> {
        self.systemctl.stop(&self.unit).await
    }

    /// Starts the managed daemon without waiting for readiness.
    pub async fn start(&self) -> Result<(), UpdateError> {
        self.systemctl.start(&self.unit).await
    }

    fn baseline(&self) -> Result<Option<String>, UpdateError> {
        self.invocation_before_restart
            .lock()
            .map(|baseline| baseline.clone())
            .map_err(|_| UpdateError::ReadinessStatePoisoned)
    }

    async fn ready_once(&self, expected: &ReleaseIdentity) -> Result<(), UpdateError> {
        let baseline = self
            .baseline()?
            .ok_or_else(|| UpdateError::InvocationBaselineMissing(self.unit.clone()))?;
        if !self.systemctl.is_active(&self.unit).await? {
            return Err(UpdateError::ServiceNotRunning(self.unit.clone()));
        }
        if self.systemctl.main_pid(&self.unit).await? == 0 {
            return Err(UpdateError::ServiceNotRunning(self.unit.clone()));
        }
        let invocation = self.systemctl.invocation_id(&self.unit).await?;
        if invocation.is_empty() {
            return Err(UpdateError::ServiceNotRunning(self.unit.clone()));
        }
        if invocation == baseline {
            return Err(UpdateError::ServiceNotRestarted(self.unit.clone()));
        }

        let token = self.auth_token().await?;
        let ready = self.ready_report(&token).await?;
        if ready.invocation_id != invocation {
            return Err(UpdateError::ReadinessIdentityMismatch {
                field: "invocation_id",
                expected: invocation,
                found: ready.invocation_id,
            });
        }

        let version = Version::parse(&ready.version).map_err(|error| {
            UpdateError::InvalidReadyResponse(format!(
                "version `{}` is not SemVer: {error}",
                ready.version
            ))
        })?;
        if version != expected.version {
            return Err(UpdateError::ReadinessIdentityMismatch {
                field: "version",
                expected: expected.version.to_string(),
                found: version.to_string(),
            });
        }
        if ready.build_id != expected.build_id {
            return Err(UpdateError::ReadinessIdentityMismatch {
                field: "build_id",
                expected: expected.build_id.clone(),
                found: ready.build_id,
            });
        }
        if !ready.python_host_ready {
            return Err(UpdateError::PythonHostNotReady);
        }

        self.frontend_asset().await?;
        self.authenticated_ping(&token).await
    }

    fn endpoint_url(&self, path: &str) -> Result<Url, UpdateError> {
        self.endpoint
            .join(path)
            .map_err(|error| UpdateError::InvalidReadyEndpoint {
                endpoint: self.endpoint.to_string(),
                reason: error.to_string(),
            })
    }

    async fn auth_token(&self) -> Result<String, UpdateError> {
        let url = self.endpoint_url(AUTH_TOKEN_PATH)?;
        let response = self
            .client
            .get(url.clone())
            .send()
            .await
            .map_err(|source| UpdateError::ReadyHttp {
                url: url.to_string(),
                source: Box::new(source),
            })?;
        if !response.status().is_success() {
            return Err(UpdateError::ReadyStatus {
                path: AUTH_TOKEN_PATH,
                status: response.status().as_u16(),
            });
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_READY_BODY_BYTES as u64)
        {
            return Err(UpdateError::InvalidReadyResponse(format!(
                "`{AUTH_TOKEN_PATH}` exceeds {MAX_READY_BODY_BYTES} bytes"
            )));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|source| UpdateError::ReadyHttp {
                url: url.to_string(),
                source: Box::new(source),
            })?;
        if bytes.len() > MAX_READY_BODY_BYTES {
            return Err(UpdateError::InvalidReadyResponse(format!(
                "`{AUTH_TOKEN_PATH}` exceeds {MAX_READY_BODY_BYTES} bytes"
            )));
        }
        let token = std::str::from_utf8(&bytes)
            .map_err(|error| {
                UpdateError::InvalidReadyResponse(format!(
                    "auth token endpoint returned non-UTF-8 data: {error}"
                ))
            })?
            .trim()
            .to_owned();
        if token.is_empty() {
            return Err(UpdateError::InvalidReadyResponse(
                "auth token endpoint returned an empty body".to_owned(),
            ));
        }
        Ok(token)
    }

    async fn ready_report(&self, token: &str) -> Result<ReadyResponse, UpdateError> {
        let url = self.endpoint_url(READY_PATH)?;
        let response = self
            .client
            .get(url.clone())
            .header("X-Decky-Auth", token)
            .send()
            .await
            .map_err(|source| UpdateError::ReadyHttp {
                url: url.to_string(),
                source: Box::new(source),
            })?;
        if !response.status().is_success() {
            return Err(UpdateError::ReadyStatus {
                path: READY_PATH,
                status: response.status().as_u16(),
            });
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_READY_BODY_BYTES as u64)
        {
            return Err(UpdateError::InvalidReadyResponse(format!(
                "`{READY_PATH}` exceeds {MAX_READY_BODY_BYTES} bytes"
            )));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|source| UpdateError::ReadyHttp {
                url: url.to_string(),
                source: Box::new(source),
            })?;
        if bytes.len() > MAX_READY_BODY_BYTES {
            return Err(UpdateError::InvalidReadyResponse(format!(
                "`{READY_PATH}` exceeds {MAX_READY_BODY_BYTES} bytes"
            )));
        }
        serde_json::from_slice(&bytes)
            .map_err(|error| UpdateError::InvalidReadyResponse(error.to_string()))
    }

    async fn frontend_asset(&self) -> Result<(), UpdateError> {
        let url = self.endpoint_url(FRONTEND_ASSET_PATH)?;
        let response = self
            .client
            .get(url.clone())
            .send()
            .await
            .map_err(|source| UpdateError::ReadyHttp {
                url: url.to_string(),
                source: Box::new(source),
            })?;
        if !response.status().is_success() {
            return Err(UpdateError::ReadyStatus {
                path: FRONTEND_ASSET_PATH,
                status: response.status().as_u16(),
            });
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_FRONTEND_BODY_BYTES as u64)
        {
            return Err(UpdateError::FrontendAsset {
                path: FRONTEND_ASSET_PATH,
                reason: format!("response exceeds {MAX_FRONTEND_BODY_BYTES} bytes"),
            });
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|source| UpdateError::ReadyHttp {
                url: url.to_string(),
                source: Box::new(source),
            })?;
        if bytes.is_empty() {
            return Err(UpdateError::FrontendAsset {
                path: FRONTEND_ASSET_PATH,
                reason: "response body is empty".to_owned(),
            });
        }
        if bytes.len() > MAX_FRONTEND_BODY_BYTES {
            return Err(UpdateError::FrontendAsset {
                path: FRONTEND_ASSET_PATH,
                reason: format!("response exceeds {MAX_FRONTEND_BODY_BYTES} bytes"),
            });
        }
        Ok(())
    }

    async fn authenticated_ping(&self, token: &str) -> Result<(), UpdateError> {
        let mut url = self.endpoint.clone();
        let scheme = match url.scheme() {
            "http" => "ws",
            "https" => "wss",
            other => {
                return Err(UpdateError::InvalidReadyEndpoint {
                    endpoint: self.endpoint.to_string(),
                    reason: format!("unsupported scheme `{other}` for a WebSocket"),
                });
            }
        };
        url.set_scheme(scheme)
            .map_err(|()| UpdateError::InvalidReadyEndpoint {
                endpoint: self.endpoint.to_string(),
                reason: "cannot change the endpoint scheme to WebSocket".to_owned(),
            })?;
        url.set_path(WS_PATH);
        url.set_query(None);
        url.query_pairs_mut().append_pair("auth", token);

        let (mut socket, _) = tokio_tungstenite::connect_async(url.as_str())
            .await
            .map_err(|error| classify_ws_connect_error(&url, &error))?;
        socket
            .send(Message::Text(
                json!({
                    "type": 0,
                    "id": READY_RPC_ID,
                    "route": "utilities/ping",
                    "args": [],
                })
                .to_string()
                .into(),
            ))
            .await
            .map_err(|error| UpdateError::ReadinessRpcTransport(format!("send failed: {error}")))?;

        loop {
            match socket.next().await {
                Some(Ok(Message::Text(text))) => {
                    let reply: Value = serde_json::from_str(text.as_str()).map_err(|error| {
                        UpdateError::ReadinessRpc(format!("invalid reply: {error}"))
                    })?;
                    if reply.get("id").and_then(Value::as_u64) != Some(READY_RPC_ID) {
                        continue;
                    }
                    if reply.get("type").and_then(Value::as_u64) == Some(1) {
                        return if reply.get("result").and_then(Value::as_str) == Some("pong") {
                            Ok(())
                        } else {
                            Err(UpdateError::ReadinessRpc(format!(
                                "unexpected ping result: {}",
                                reply.get("result").cloned().unwrap_or(Value::Null)
                            )))
                        };
                    }
                    return Err(UpdateError::ReadinessRpc(format!(
                        "error reply: {}",
                        reply.get("error").cloned().unwrap_or(Value::Null)
                    )));
                }
                Some(Ok(Message::Ping(payload))) => {
                    socket.send(Message::Pong(payload)).await.map_err(|error| {
                        UpdateError::ReadinessRpcTransport(format!("pong failed: {error}"))
                    })?;
                }
                Some(Ok(Message::Close(_)) | Err(_)) | None => {
                    return Err(UpdateError::ReadinessRpcTransport(
                        "connection closed before the ping reply".to_owned(),
                    ));
                }
                Some(Ok(_)) => {}
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct ReadyResponse {
    version: String,
    build_id: String,
    python_host_ready: bool,
    #[serde(default)]
    invocation_id: String,
}

/// Verifies that `service` restarted into `expected` and is ready before `deadline`.
///
/// Notifications from unrelated plugins do not participate: only the loader's systemd
/// invocation, `/readyz` identity, frontend asset, Python host import and authenticated RPC
/// ping gate activation.
pub async fn verify_ready(
    service: &SystemdService,
    expected: &ReleaseIdentity,
    deadline: Duration,
) -> Result<(), UpdateError> {
    let started = Instant::now();
    let mut last_error = None;
    loop {
        let remaining = deadline.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(last_error.unwrap_or(UpdateError::ReadinessDeadline(deadline)));
        }

        let attempt = remaining.min(READINESS_ATTEMPT_TIMEOUT);
        match tokio::time::timeout(attempt, service.ready_once(expected)).await {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(error)) => {
                if !is_retryable_readiness_failure(&error) {
                    return Err(error);
                }
                last_error = Some(error);
            }
            Err(_) => last_error = Some(UpdateError::ReadinessDeadline(deadline)),
        }

        let remaining = deadline.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break;
        }
        tokio::time::sleep(remaining.min(READINESS_RETRY_INTERVAL)).await;
    }
    Err(last_error.unwrap_or(UpdateError::ReadinessDeadline(deadline)))
}

fn is_retryable_readiness_failure(error: &UpdateError) -> bool {
    match error {
        UpdateError::Systemctl { .. }
        | UpdateError::ServiceNotRunning(_)
        | UpdateError::ServiceNotRestarted(_)
        | UpdateError::ReadyHttp { .. }
        | UpdateError::ReadinessRpcTransport(_) => true,
        UpdateError::ReadyStatus { status, .. } => is_retryable_http_status(*status),
        _ => false,
    }
}

fn is_retryable_http_status(status: u16) -> bool {
    status == 408 || status == 429 || (500..=599).contains(&status)
}

fn classify_ws_connect_error(url: &Url, error: &WsError) -> UpdateError {
    let message = format!("cannot connect to `{}`: {error}", redacted_url(url));
    match error {
        WsError::Http(response) if !is_retryable_http_status(response.status().as_u16()) => {
            UpdateError::ReadinessRpc(message)
        }
        _ => UpdateError::ReadinessRpcTransport(message),
    }
}

fn redacted_url(url: &Url) -> String {
    let mut redacted = url.clone();
    redacted.set_query(None);
    redacted.to_string()
}

#[cfg(test)]
mod tests {
    use url::Url;

    use super::{is_retryable_readiness_failure, redacted_url};
    use crate::UpdateError;

    #[test]
    fn readiness_errors_redact_the_auth_query() {
        let url = Url::parse("ws://127.0.0.1:1337/ws?auth=secret-token").expect("valid URL");
        let redacted = redacted_url(&url);
        assert_eq!(redacted, "ws://127.0.0.1:1337/ws");
        assert!(!redacted.contains("secret-token"), "{redacted}");
    }

    #[test]
    fn readiness_classifies_permanent_and_transient_failures() {
        assert!(is_retryable_readiness_failure(
            &UpdateError::ServiceNotRestarted("zerodeck.service".to_owned())
        ));
        assert!(is_retryable_readiness_failure(&UpdateError::ReadyStatus {
            path: "/readyz",
            status: 503,
        }));
        assert!(!is_retryable_readiness_failure(&UpdateError::ReadyStatus {
            path: "/readyz",
            status: 404,
        }));
        assert!(!is_retryable_readiness_failure(
            &UpdateError::ReadinessIdentityMismatch {
                field: "build_id",
                expected: "new".to_owned(),
                found: "old".to_owned(),
            }
        ));
        assert!(!is_retryable_readiness_failure(
            &UpdateError::PythonHostNotReady
        ));
        assert!(is_retryable_readiness_failure(
            &UpdateError::ReadinessRpcTransport("temporary".to_owned())
        ));
        assert!(!is_retryable_readiness_failure(&UpdateError::ReadinessRpc(
            "forbidden".to_owned()
        )));
    }
}
