//! systemd service identity and post-restart readiness verification.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use semver::Version;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use url::Url;

use crate::UpdateError;

/// Activation readiness deadline for a freshly restarted loader.
pub const ACTIVATION_DEADLINE: Duration = Duration::from_secs(60);

const READINESS_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(5);
const READINESS_RETRY_INTERVAL: Duration = Duration::from_millis(100);
const AUTH_TOKEN_PATH: &str = "/auth/token";
const READY_PATH: &str = "/readyz";
const FRONTEND_ASSET_PATH: &str = "/frontend/index.js";
const WS_PATH: &str = "/ws";
const READY_RPC_ID: u64 = 1;
const MAX_READY_BODY_BYTES: usize = 64 * 1024;
const MAX_FRONTEND_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Boxed future returned by [`Systemctl`] operations.
pub type SystemctlFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, UpdateError>> + Send + 'a>>;

/// Process seam for the `systemctl` queries used by activation.
pub trait Systemctl: std::fmt::Debug + Send + Sync {
    /// Returns the unit's current systemd `InvocationID`, or an empty string when inactive.
    fn invocation_id<'a>(&'a self, unit: &'a str) -> SystemctlFuture<'a, String>;
}

/// Production [`Systemctl`] backed by the host's `systemctl` binary.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioSystemctl;

impl Systemctl for TokioSystemctl {
    fn invocation_id<'a>(&'a self, unit: &'a str) -> SystemctlFuture<'a, String> {
        Box::pin(async move {
            let output = tokio::process::Command::new("systemctl")
                .args(["show", "--property=InvocationID", "--value", "--", unit])
                .kill_on_drop(true)
                .output()
                .await
                .map_err(|error| UpdateError::Systemctl {
                    unit: unit.to_owned(),
                    reason: error.to_string(),
                })?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(UpdateError::Systemctl {
                    unit: unit.to_owned(),
                    reason: format!("exited with {}: {}", output.status, stderr.trim()),
                });
            }
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
        })
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
        let invocation = self.systemctl.invocation_id(&self.unit).await?;
        if invocation.is_empty() {
            return Err(UpdateError::ServiceNotRunning(self.unit.clone()));
        }
        if invocation == baseline {
            return Err(UpdateError::ServiceNotRestarted(self.unit.clone()));
        }

        let token = self.auth_token().await?;
        let ready = self.ready_report(&token).await?;

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
        let token = response
            .text()
            .await
            .map_err(|source| UpdateError::ReadyHttp {
                url: url.to_string(),
                source: Box::new(source),
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
            .map_err(|error| {
                UpdateError::ReadinessRpc(format!("cannot connect to `{url}`: {error}"))
            })?;
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
            .map_err(|error| UpdateError::ReadinessRpc(format!("send failed: {error}")))?;

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
                        UpdateError::ReadinessRpc(format!("pong failed: {error}"))
                    })?;
                }
                Some(Ok(Message::Close(_)) | Err(_)) | None => {
                    return Err(UpdateError::ReadinessRpc(
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
            Ok(Err(error)) => last_error = Some(error),
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
