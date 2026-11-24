use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::CefError;

const SHARED_CONTEXT_TITLES: &[&str] = &[
    "SharedJSContext",
    "Steam Shared Context presented by Valve™",
    "Steam",
    "SP",
];

/// Configuration for connecting to Steam's local CEF remote debugging port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CefConfig {
    pub debugger_base_url: String,
}

impl Default for CefConfig {
    fn default() -> Self {
        Self {
            debugger_base_url: "http://127.0.0.1:8080".to_owned(),
        }
    }
}

/// Tab descriptor returned by CEF `/json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CdpTabDescriptor {
    pub id: String,
    pub title: String,
    pub url: String,
    #[serde(rename = "webSocketDebuggerUrl", default)]
    pub websocket_debugger_url: Option<String>,
}

/// Returns `true` when `tab` matches Steam's `GamepadUI` shared JS context.
#[must_use]
pub fn is_gamepadui_tab(tab: &CdpTabDescriptor) -> bool {
    let url_matches = tab.url.contains("https://steamloopback.host/routes/")
        || tab.url.contains("https://steamloopback.host/index.html");
    let title_matches = SHARED_CONTEXT_TITLES.contains(&tab.title.as_str());
    url_matches && title_matches
}

/// Queries CEF's `/json` target list endpoint.
pub async fn fetch_tabs(
    client: &reqwest::Client,
    cfg: &CefConfig,
) -> Result<Vec<CdpTabDescriptor>, CefError> {
    let url = format!("{}/json", cfg.debugger_base_url.trim_end_matches('/'));
    let tabs = client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(tabs)
}

/// Opens a CDP WebSocket connection to the specified tab debugger URL.
pub async fn connect_cdp(
    ws_url: &str,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, CefError> {
    let (stream, _) = connect_async(ws_url).await?;
    Ok(stream)
}
