//! Arbitrary archives must never write outside the destination, and extraction must be
//! all-or-nothing: afterwards the parent directory holds either nothing or exactly the destination,
//! never a leftover staging or backup directory.
#![no_main]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use libfuzzer_sys::fuzz_target;
use zerodeck_core::{ExtractLimits, extract_zip_safely_with_limits};

static RUN: AtomicU64 = AtomicU64::new(0);

fn scratch_root() -> PathBuf {
    std::env::temp_dir().join(format!(
        "zerodeck-fuzz-zip-{}-{}",
        std::process::id(),
        RUN.fetch_add(1, Ordering::Relaxed)
    ))
}

fuzz_target!(|data: &[u8]| {
    let root = scratch_root();
    fs::create_dir_all(&root).expect("create scratch root");
    let dest = root.join("plugin");
    let limits = ExtractLimits {
        max_total_bytes: 1024 * 1024,
        max_entries: 256,
    };

    let result = extract_zip_safely_with_limits(data, &dest, &limits);

    let mut names: Vec<String> = fs::read_dir(&root)
        .expect("read scratch root")
        .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    match &result {
        Ok(files) => {
            assert_eq!(names, ["plugin"], "unexpected entries next to destination");
            let canonical = fs::canonicalize(&dest).expect("canonical dest");
            for file in files {
                assert!(file.starts_with(&canonical), "{file:?} escaped {canonical:?}");
            }
        }
        Err(_) => assert!(names.is_empty(), "failed extraction left {names:?} behind"),
    }

    let _ = fs::remove_dir_all(&root);
});
