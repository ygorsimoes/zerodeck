//! Steam Chromium Embedded Framework (CEF) CDP discovery and injection foundation.

// `deny`, not `forbid`: Phase 4 CDP work may need a reviewed `#[allow(unsafe_code)]`.
#![deny(unsafe_code)]

mod driver;
mod error;
mod state;
mod supervisor;
mod tab;

pub use driver::{CefDriver, DriverFuture, LiveCefDriver};
pub use error::CefError;
pub use state::{CefConfirmation, CefStatus, CefTiming};
pub use supervisor::{CefHandle, CefSupervisor, CefSupervisorConfig, spawn_supervisor};
pub use tab::{
    CdpTabDescriptor, CefConfig, connect_cdp, fetch_tabs, is_gamepadui_tab, is_loopback_http_url,
    is_loopback_ws_url,
};

#[cfg(test)]
mod tests {
    use crate::{CdpTabDescriptor, is_gamepadui_tab};

    #[test]
    fn cef_config_accepts_only_loopback_http() {
        for valid in [
            "http://127.0.0.1:8080",
            "http://localhost:8080",
            "http://[::1]:8080",
        ] {
            assert!(
                crate::CefConfig {
                    debugger_base_url: valid.to_owned()
                }
                .validate()
                .is_ok(),
                "{valid}"
            );
        }
        for invalid in [
            "http://10.0.0.5:8080",
            "https://127.0.0.1:8080",
            "http://user@127.0.0.1:8080",
            "http://evil.example:8080",
            "ws://127.0.0.1:8080",
            "not a url",
        ] {
            assert!(
                crate::CefConfig {
                    debugger_base_url: invalid.to_owned()
                }
                .validate()
                .is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn websocket_targets_must_be_loopback() {
        for valid in [
            "ws://127.0.0.1:8080/devtools/page/1",
            "ws://localhost:8080/devtools/page/1",
            "ws://[::1]:8080/devtools/page/1",
        ] {
            assert!(crate::is_loopback_ws_url(valid), "{valid}");
        }
        for invalid in [
            "ws://10.0.0.5:8080/devtools/page/1",
            "wss://127.0.0.1:8080/devtools/page/1",
            "http://127.0.0.1:8080/json",
            "ws://evil.example/devtools/page/1",
        ] {
            assert!(!crate::is_loopback_ws_url(invalid), "{invalid}");
        }
    }

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
