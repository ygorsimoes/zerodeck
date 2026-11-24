//! Frames a WebSocket client can send to `/ws` must never panic the parser.
#![no_main]

use libfuzzer_sys::fuzz_target;
use zerodeck_protocol::WsClientMessage;

fuzz_target!(|data: &[u8]| {
    if let Ok(raw) = std::str::from_utf8(data) {
        let _ = WsClientMessage::from_json_str(raw);
    }
});
