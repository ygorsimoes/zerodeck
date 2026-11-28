//! CDP transport used by the supervisor. Only the commands the supervisor itself needs are
//! allowed, so a compromised target cannot turn the loader into a generic debugger proxy.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

use crate::{CdpTabDescriptor, CefConfig, CefError, connect_cdp, fetch_tabs};

/// Boxed future returned by [`CefDriver`] methods so the trait stays object-safe.
pub type DriverFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, CefError>> + Send + 'a>>;

/// Discovery and injection operations the supervisor needs from Steam.
pub trait CefDriver: Send + Sync {
    /// Lists CEF targets through the loopback `/json` endpoint.
    fn list_targets(&self) -> DriverFuture<'_, Vec<CdpTabDescriptor>>;

    /// Evaluates the bootstrap script in the selected tab over CDP.
    fn inject(&self, websocket_url: &str, script: &str) -> DriverFuture<'_, ()>;
}

/// Production driver backed by `reqwest` and `tokio-tungstenite`.
#[derive(Debug, Clone)]
pub struct LiveCefDriver {
    client: reqwest::Client,
    config: CefConfig,
    command_timeout: Duration,
}

impl LiveCefDriver {
    /// Creates a driver for one validated loopback debugger endpoint.
    ///
    /// # Errors
    /// Returns [`CefError::InvalidConfig`] when the debugger URL is not loopback HTTP.
    pub fn new(
        client: reqwest::Client,
        config: CefConfig,
        command_timeout: Duration,
    ) -> Result<Self, CefError> {
        config.validate()?;
        Ok(Self {
            client,
            config,
            command_timeout,
        })
    }
}

impl CefDriver for LiveCefDriver {
    fn list_targets(&self) -> DriverFuture<'_, Vec<CdpTabDescriptor>> {
        let client = self.client.clone();
        let config = self.config.clone();
        Box::pin(async move { fetch_tabs(&client, &config).await })
    }

    fn inject(&self, websocket_url: &str, script: &str) -> DriverFuture<'_, ()> {
        let websocket_url = websocket_url.to_owned();
        let script = script.to_owned();
        let command_timeout = self.command_timeout;
        Box::pin(async move { evaluate(&websocket_url, &script, command_timeout).await })
    }
}

const BOOTSTRAP_EVALUATION: &str = "Runtime.evaluate";

/// Rejects CDP methods outside the supervisor's narrow allowlist.
fn ensure_allowed(method: &str) -> Result<(), CefError> {
    if method == BOOTSTRAP_EVALUATION {
        Ok(())
    } else {
        Err(CefError::CdpMethodNotAllowed(method.to_owned()))
    }
}

/// Evaluates `expression` with `Runtime.evaluate` and waits for the matching reply.
async fn evaluate(
    websocket_url: &str,
    expression: &str,
    command_timeout: Duration,
) -> Result<(), CefError> {
    ensure_allowed(BOOTSTRAP_EVALUATION)?;
    let mut socket = connect_cdp(websocket_url).await?;
    let command = json!({
        "id": 1,
        "method": BOOTSTRAP_EVALUATION,
        "params": {
            "expression": expression,
            "returnByValue": true,
            "awaitPromise": true,
        }
    });
    socket
        .send(Message::Text(command.to_string().into()))
        .await?;

    tokio::time::timeout(command_timeout, async {
        while let Some(message) = socket.next().await {
            let message = message?;
            let Message::Text(text) = message else {
                continue;
            };
            let value: Value = serde_json::from_str(&text)
                .map_err(|err| CefError::CdpResponse(err.to_string()))?;
            if value.get("id").and_then(Value::as_u64) != Some(1) {
                continue;
            }
            if let Some(error) = value.get("error") {
                return Err(CefError::CdpEvaluationFailed(error.to_string()));
            }
            if let Some(exception) = value.pointer("/result/exceptionDetails") {
                return Err(CefError::CdpEvaluationFailed(exception.to_string()));
            }
            return Ok(());
        }
        Err(CefError::CdpResponse("CDP socket closed".to_owned()))
    })
    .await
    .map_err(|_| CefError::CdpTimeout)?
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures_util::{SinkExt, StreamExt};
    use serde_json::json;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;
    use tokio_tungstenite::tungstenite::Message;

    use super::{ensure_allowed, evaluate};
    use crate::CefError;

    #[test]
    fn only_runtime_evaluate_is_an_allowed_cdp_command() {
        assert!(ensure_allowed("Runtime.evaluate").is_ok());
        assert!(matches!(
            ensure_allowed("Page.navigate"),
            Err(CefError::CdpMethodNotAllowed(method)) if method == "Page.navigate"
        ));
    }

    #[tokio::test]
    async fn evaluate_sends_runtime_evaluate_and_accepts_a_clean_result() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let mut socket = accept_async(stream).await.expect("ws handshake");
            let message = socket.next().await.expect("command").expect("frame");
            let Message::Text(text) = message else {
                panic!("expected text command");
            };
            let command: serde_json::Value = serde_json::from_str(&text).expect("command json");
            socket
                .send(Message::Text(
                    json!({"id": 1, "result": {"result": {"type": "undefined"}}})
                        .to_string()
                        .into(),
                ))
                .await
                .expect("reply");
            command
        });

        evaluate(
            &format!("ws://{addr}/devtools/page/1"),
            "window.__zerodeck = 1",
            Duration::from_secs(1),
        )
        .await
        .expect("clean evaluation");

        let command = server.await.expect("server task");
        assert_eq!(command["method"], json!("Runtime.evaluate"));
        assert_eq!(
            command["params"]["expression"],
            json!("window.__zerodeck = 1")
        );
    }

    #[tokio::test]
    async fn evaluate_reports_exception_details_from_the_target() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let mut socket = accept_async(stream).await.expect("ws handshake");
            let _ = socket.next().await.expect("command").expect("frame");
            socket
                .send(Message::Text(
                    json!({
                        "id": 1,
                        "result": { "exceptionDetails": { "text": "boom" } }
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .expect("reply");
        });

        let result = evaluate(
            &format!("ws://{addr}/devtools/page/1"),
            "throw new Error('boom')",
            Duration::from_secs(1),
        )
        .await;
        assert!(matches!(result, Err(CefError::CdpEvaluationFailed(_))));
        server.await.expect("server");
    }
}
