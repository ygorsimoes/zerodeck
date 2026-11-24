//! Steam Chromium Embedded Framework (CEF) CDP discovery and injection foundation.

mod error;
mod tab;

pub use error::CefError;
pub use tab::{CdpTabDescriptor, CefConfig, connect_cdp, fetch_tabs, is_gamepadui_tab};

#[cfg(test)]
mod tests {
    use crate::{CdpTabDescriptor, is_gamepadui_tab};

    #[test]
    fn gamepadui_tab_matches_shared_ctx_and_steamloopback_url() {
        let valid = CdpTabDescriptor {
            id: "1".to_owned(),
            title: "SharedJSContext".to_owned(),
            url: "https://steamloopback.host/index.html".to_owned(),
            websocket_debugger_url: Some("ws://127.0.0.1:8080/devtools/page/1".to_owned()),
        };
        assert!(is_gamepadui_tab(&valid));

        let blank = CdpTabDescriptor {
            id: "2".to_owned(),
            title: "SharedJSContext".to_owned(),
            url: "about:blank".to_owned(),
            websocket_debugger_url: None,
        };
        assert!(!is_gamepadui_tab(&blank));

        let external = CdpTabDescriptor {
            id: "3".to_owned(),
            title: "Other".to_owned(),
            url: "https://steamloopback.host/routes/store".to_owned(),
            websocket_debugger_url: None,
        };
        assert!(!is_gamepadui_tab(&external));

        let spoofed_query = CdpTabDescriptor {
            id: "4".to_owned(),
            title: "SharedJSContext".to_owned(),
            url: "https://evil.example/?next=https://steamloopback.host/index.html".to_owned(),
            websocket_debugger_url: None,
        };
        assert!(!is_gamepadui_tab(&spoofed_query));
    }
}
