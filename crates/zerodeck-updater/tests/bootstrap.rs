//! Administrative bootstrap state transitions and refusal paths.
//!
//! These tests use a temporary root and fake generation layouts; they never touch systemd, the
//! network, a real installation, or the caller's identity.

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use uuid::Uuid;
use zerodeck_updater::{
    ApplyRequest, ApplyRequestStore, BootstrapOutcome, GenerationId, GenerationStore,
    PendingTransaction, StateStore, TransactionPhase, UpdateError, UpdateState,
    bootstrap_generation,
};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "zerodeck-bootstrap-{label}-{}-{}",
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

fn generation_id(fill: char) -> GenerationId {
    GenerationId::new(fill.to_string().repeat(64)).expect("valid generation id")
}

fn generation(root: &Path, fill: char) -> GenerationId {
    let id = generation_id(fill);
    let directory = root.join("generations").join(id.as_str());
    fs::create_dir_all(directory.join("bin")).expect("create bin");
    fs::create_dir_all(directory.join("python")).expect("create python");
    fs::create_dir_all(directory.join("frontend")).expect("create frontend");
    fs::write(directory.join("bin/zerodeckd"), b"#!/bin/sh\n").expect("write binary");
    fs::set_permissions(
        directory.join("bin/zerodeckd"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .expect("chmod binary");
    fs::write(directory.join("python/host.py"), b"").expect("write python");
    fs::write(directory.join("frontend/index.js"), b"").expect("write frontend");
    fs::write(
        directory.join("release.json"),
        br#"{"schema":1,"build_id":"build-test","version":"0.1.0"}"#,
    )
    .expect("write release");
    id
}

fn state_store(root: &Path) -> StateStore {
    StateStore::new(root.join("state/update.json"))
}

#[test]
fn bootstrap_installs_the_first_generation_idempotently() {
    let dir = TestDir::new("first");
    let generation = generation(&dir.0, 'a');

    let first = bootstrap_generation(&dir.0, generation.as_str(), false, false)
        .expect("bootstrap first generation");
    assert_eq!(first.generation, generation);
    assert_eq!(first.previous, None);
    assert!(first.changed);

    let store = GenerationStore::new(&dir.0).expect("open store");
    assert_eq!(
        store.current().expect("read current"),
        Some(generation.clone())
    );
    let state = state_store(&dir.0).read().expect("read state");
    assert_eq!(state.last_confirmed, Some(generation.clone()));
    assert_eq!(state.rollback_target, None);
    assert!(state.pending.is_none());
    assert!(
        state
            .last_result
            .as_deref()
            .unwrap_or_default()
            .contains("bootstrapped")
    );

    let second = bootstrap_generation(&dir.0, generation.as_str(), false, false)
        .expect("bootstrap is idempotent");
    assert_eq!(second.generation, generation);
    assert!(!second.changed);
    assert_eq!(store.current().expect("read current"), Some(generation));
}

#[test]
fn bootstrap_refuses_a_pending_activation_before_touching_state() {
    let dir = TestDir::new("pending");
    let previous = generation(&dir.0, 'a');
    let candidate = generation(&dir.0, 'b');
    let store = GenerationStore::new(&dir.0).expect("open store");
    store.switch_to(&previous).expect("activate previous");
    let mut state = UpdateState::new();
    state.last_confirmed = Some(previous.clone());
    state.pending = Some(PendingTransaction {
        id: Uuid::new_v4(),
        previous: Some(previous.clone()),
        candidate: candidate.clone(),
        phase: TransactionPhase::Prepared,
        was_running: false,
        start_attempted: false,
    });
    state_store(&dir.0).write(&state).expect("seed pending");
    let before = fs::read(state_store(&dir.0).path()).expect("read state bytes");

    let error = bootstrap_generation(&dir.0, candidate.as_str(), true, false)
        .expect_err("pending activation must refuse bootstrap");
    assert!(matches!(error, UpdateError::BootstrapRefused(_)), "{error}");
    assert_eq!(
        store.current_reference().expect("read current"),
        Some(previous)
    );
    assert_eq!(
        fs::read(state_store(&dir.0).path()).expect("read state bytes"),
        before
    );
}

#[test]
fn bootstrap_replacement_requires_conversion_and_retains_the_previous_generation() {
    let dir = TestDir::new("convert");
    let previous = generation(&dir.0, 'a');
    let candidate = generation(&dir.0, 'b');
    bootstrap_generation(&dir.0, previous.as_str(), false, false).expect("first install");

    let error = bootstrap_generation(&dir.0, candidate.as_str(), false, false)
        .expect_err("replacement requires explicit conversion");
    assert!(matches!(error, UpdateError::BootstrapRefused(_)), "{error}");
    let store = GenerationStore::new(&dir.0).expect("open store");
    assert_eq!(
        store.current_reference().expect("read current"),
        Some(previous.clone())
    );

    let outcome =
        bootstrap_generation(&dir.0, candidate.as_str(), true, false).expect("explicit conversion");
    assert_eq!(outcome.generation, candidate.clone());
    assert_eq!(outcome.previous, Some(previous.clone()));
    assert_eq!(
        store.current_reference().expect("read current"),
        Some(candidate.clone())
    );
    let state = state_store(&dir.0).read().expect("read state");
    assert_eq!(state.last_confirmed, Some(candidate));
    assert_eq!(state.rollback_target, Some(previous.clone()));
    assert!(store.generation_dir(&previous).is_dir());
    assert!(state.pending.is_none());
}

#[test]
fn bootstrap_check_verifies_without_mutating_the_root() {
    let dir = TestDir::new("check");
    let generation = generation(&dir.0, 'a');
    let outcome = bootstrap_generation(&dir.0, generation.as_str(), false, true)
        .expect("check a valid generation");
    assert_eq!(outcome.generation, generation);
    assert!(!outcome.changed);
    assert!(!dir.0.join("current").exists());
    assert!(!dir.0.join("state/update.json").exists());
}

#[test]
fn bootstrap_refuses_an_unconsumed_apply_request() {
    let dir = TestDir::new("request");
    let generation = generation(&dir.0, 'a');
    let state = state_store(&dir.0);
    let requests = ApplyRequestStore::for_state(&state);
    let request = ApplyRequest {
        schema: zerodeck_updater::APPLY_REQUEST_SCHEMA,
        id: Uuid::new_v4(),
        kind: zerodeck_updater::ApplyKind::Rollback,
        generation: generation.clone(),
        release: None,
        expected_active: None,
        start_service: false,
    };
    requests.write(&request).expect("write request");

    let error = bootstrap_generation(&dir.0, generation.as_str(), false, false)
        .expect_err("an unconsumed request must refuse bootstrap");
    assert!(matches!(error, UpdateError::BootstrapRefused(_)), "{error}");
    assert!(!dir.0.join("current").exists());
}

#[test]
fn bootstrap_refuses_conversion_when_the_previous_generation_is_corrupt() {
    let dir = TestDir::new("corrupt-previous");
    let previous = generation(&dir.0, 'a');
    let candidate = generation(&dir.0, 'b');
    bootstrap_generation(&dir.0, previous.as_str(), false, false).expect("first install");
    fs::remove_dir_all(dir.0.join("generations").join(previous.as_str())).expect("remove previous");

    let error = bootstrap_generation(&dir.0, candidate.as_str(), true, false)
        .expect_err("a corrupt previous generation must refuse conversion");
    assert!(
        matches!(
            error,
            UpdateError::MissingGeneration(_) | UpdateError::BootstrapRefused(_)
        ),
        "{error}"
    );
    let store = GenerationStore::new(&dir.0).expect("open store");
    assert_eq!(
        store.current_reference().expect("read current"),
        Some(previous.clone())
    );
    let state = state_store(&dir.0).read().expect("read state");
    assert_eq!(state.last_confirmed, Some(previous));
    assert!(state.pending.is_none());
    assert!(
        !state
            .last_result
            .as_deref()
            .unwrap_or_default()
            .contains(&*candidate.to_string())
    );
}

#[test]
fn bootstrap_outcome_reports_the_retained_rollback_target() {
    let dir = TestDir::new("outcome");
    let previous = generation(&dir.0, 'a');
    let candidate = generation(&dir.0, 'b');
    bootstrap_generation(&dir.0, previous.as_str(), false, false).expect("first install");
    let outcome: BootstrapOutcome =
        bootstrap_generation(&dir.0, candidate.as_str(), true, false).expect("convert");
    assert_eq!(outcome.rollback_target, Some(previous));
}
