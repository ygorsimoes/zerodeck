//! Focused fault tests for generation durability and the shared maintenance lock.

#![cfg(unix)]

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use zerodeck_platform::MaintenanceLock;
use zerodeck_updater::{GenerationId, GenerationStore, UpdateError, UpdateState};

const WORKER_LOCK_PATH: &str = "ZERODECK_TEST_MAINTENANCE_LOCK";
const WORKER_READY_PATH: &str = "ZERODECK_TEST_MAINTENANCE_READY";
const WORKER_GO_PATH: &str = "ZERODECK_TEST_MAINTENANCE_GO";
const WORKER_TEST: &str = "maintenance_worker_acquires_after_parent_release";

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "zerodeck-updater-{label}-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("create temp test dir");
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Kills a spawned lock worker if the parent test fails before releasing it.
struct Worker(Child);

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn generation_id(fill: char) -> GenerationId {
    GenerationId::new(fill.to_string().repeat(64)).expect("valid generation id")
}

#[test]
fn generation_recovers_from_rename_sync_error() {
    let dir = TestDir::new("rename-sync");
    let store = GenerationStore::new(&dir.0).expect("open generation store");
    let previous = generation_id('a');
    let candidate = generation_id('b');
    for id in [&previous, &candidate] {
        fs::create_dir(store.generation_dir(id)).expect("create generation directory");
    }

    store.switch_to(&previous).expect("activate previous");
    let persisted_previous = store
        .current()
        .expect("read active reference")
        .expect("previous generation is active");

    let err = store
        .switch_to_with(&candidate, |_| {
            Err(io::Error::other("injected directory sync failure"))
        })
        .expect_err("a failed directory sync must not report success");
    assert!(
        matches!(err, UpdateError::Io { .. }),
        "unexpected switch error: {err:?}"
    );
    assert_eq!(
        store.current().expect("read active reference"),
        Some(candidate.clone()),
        "the rename is visible even though its directory sync failed"
    );

    store
        .switch_to(&persisted_previous)
        .expect("replay the persisted previous reference");
    assert_eq!(
        store.current().expect("read active reference"),
        Some(persisted_previous.clone())
    );

    store
        .switch_to(&persisted_previous)
        .expect("repeat the same reconciliation");
    assert_eq!(
        store.current().expect("read active reference"),
        Some(persisted_previous),
        "repeated reconciliation must be idempotent"
    );
}

#[test]
fn maintenance_guard_blocks_exclusive_during_shared() {
    let dir = TestDir::new("maintenance");
    let lock_path = dir.0.join("maintenance.lock");
    let ready = dir.0.join("worker-ready");
    let go = dir.0.join("worker-go");

    let first = MaintenanceLock::try_shared(&lock_path).expect("first shared guard");
    let second =
        MaintenanceLock::try_shared(&lock_path).expect("second shared guard on a separate handle");

    let err = MaintenanceLock::try_exclusive(&lock_path)
        .expect_err("exclusive must conflict with shared guards");
    assert_eq!(err.kind(), io::ErrorKind::WouldBlock);

    let mut worker = Worker(
        Command::new(std::env::current_exe().expect("test executable path"))
            .args(["--exact", WORKER_TEST, "--ignored"])
            .env(WORKER_LOCK_PATH, &lock_path)
            .env(WORKER_READY_PATH, &ready)
            .env(WORKER_GO_PATH, &go)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn lock worker"),
    );

    // The worker starts while both guards are alive, so a leaked file descriptor would keep a
    // shared lock after the parent closes its handles.
    wait_for(&ready, "lock worker ready");

    drop(first);
    let err = MaintenanceLock::try_exclusive(&lock_path)
        .expect_err("exclusive must conflict until every shared handle closes");
    assert_eq!(err.kind(), io::ErrorKind::WouldBlock);

    drop(second);
    drop(MaintenanceLock::try_exclusive(&lock_path).expect("exclusive after all guards close"));

    fs::write(&go, b"go").expect("release lock worker");
    let status = worker.0.wait().expect("wait for lock worker");
    assert!(
        status.success(),
        "the worker did not acquire the lock; the guard was inherited across exec"
    );
}

#[test]
#[ignore = "spawned by maintenance_guard_blocks_exclusive_during_shared"]
fn maintenance_worker_acquires_after_parent_release() {
    let lock_path = std::env::var_os(WORKER_LOCK_PATH).expect("worker lock path");
    let ready = std::env::var_os(WORKER_READY_PATH).expect("worker ready path");
    let go = std::env::var_os(WORKER_GO_PATH).expect("worker go path");

    fs::write(&ready, b"ready").expect("signal worker readiness");
    wait_for(Path::new(&go), "parent release");

    drop(
        MaintenanceLock::try_exclusive(Path::new(&lock_path))
            .expect("worker must not inherit the parent's lock descriptor"),
    );
}

fn wait_for(path: &Path, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !path.exists() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn make_generation(store: &GenerationStore, id: &GenerationId) {
    let dir = store.generation_dir(id);
    fs::create_dir_all(dir.join("bin")).expect("create generation tree");
    fs::write(dir.join("release.json"), b"{}").expect("write manifest");
}

fn entries(store: &GenerationStore) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(
        store
            .generation_dir(&generation_id('0'))
            .parent()
            .expect("parent"),
    )
    .expect("list generations")
    .map(|entry| {
        entry
            .expect("entry")
            .file_name()
            .to_string_lossy()
            .into_owned()
    })
    .collect();
    names.sort();
    names
}

#[test]
fn gc_keeps_quarantined_generations_until_they_are_released() {
    let dir = TestDir::new("gc-quarantine");
    let store = GenerationStore::new(&dir.0).expect("open store");
    let quarantined = generation_id('c');
    let unreferenced = generation_id('d');
    make_generation(&store, &quarantined);
    make_generation(&store, &unreferenced);
    let mut state = UpdateState::new();
    state.quarantine(quarantined.clone());

    store.gc(&state, &[]).expect("gc");

    assert_eq!(entries(&store), vec![quarantined.to_string()]);
}

#[test]
fn gc_sweeps_leftover_trash_and_staging_directories() {
    let dir = TestDir::new("gc-sweep");
    let store = GenerationStore::new(&dir.0).expect("open store");
    let generations = store
        .generation_dir(&generation_id('0'))
        .parent()
        .expect("parent")
        .to_path_buf();
    // A crash between gc's rename and delete, and an interrupted extraction.
    fs::create_dir_all(generations.join(format!(".trash-{}", "f".repeat(8)))).expect("trash");
    fs::create_dir_all(generations.join(".staging-interrupted/tree")).expect("staging");

    store.gc(&UpdateState::new(), &[]).expect("gc");

    assert!(entries(&store).is_empty(), "{:?}", entries(&store));
}
