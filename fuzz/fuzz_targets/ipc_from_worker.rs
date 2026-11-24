//! Lines a (possibly compromised) plugin worker writes over IPC must never panic the parser.
#![no_main]

use libfuzzer_sys::fuzz_target;
use zerodeck_protocol::PluginIpcFromWorker;

fuzz_target!(|data: &[u8]| {
    if let Ok(raw) = std::str::from_utf8(data) {
        let _ = PluginIpcFromWorker::from_json_str(raw);
    }
});
