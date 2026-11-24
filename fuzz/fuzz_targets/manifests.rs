//! Plugin-supplied `plugin.json` / `package.json` must parse or fail cleanly, and the derived
//! load type / privilege flags must be computable for anything that parses.
#![no_main]

use libfuzzer_sys::fuzz_target;
use zerodeck_protocol::{PackageManifest, PluginManifest};

fuzz_target!(|data: &[u8]| {
    if let Ok(plugin) = serde_json::from_slice::<PluginManifest>(data) {
        let _ = (plugin.requires_root(), plugin.is_debug());
    }
    if let Ok(package) = serde_json::from_slice::<PackageManifest>(data) {
        let _ = package.load_type();
    }
});
