//! Privileged activation, offline rollback, and crash recovery.

use std::time::{Duration, Instant, SystemTime};

use uuid::Uuid;
use zerodeck_platform::MaintenanceLock;

use crate::{
    ACTIVATION_DEADLINE, ApplyKind, ApplyRequest, ApplyRequestStore, Catalog, GenerationId,
    GenerationLayout, GenerationStore, PendingTransaction, ReleaseIdentity, StateStore,
    SystemdService, TransactionPhase, UpdateError, UpdateState, verify_ready,
};

/// Managed daemon unit controlled by systemd.
pub const DAEMON_UNIT: &str = "zerodeck.service";
/// Independent oneshot unit that performs activation.
pub const APPLY_UNIT: &str = "zerodeck-apply.service";

const LOCK_WAIT: Duration = Duration::from_secs(30);
const LOCK_POLL: Duration = Duration::from_millis(50);

/// Durable boundaries a fault test can stop the process at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationCheckpoint {
    /// Immediately after the exclusive maintenance guard is acquired.
    AfterLock,
    /// Before the transaction intent is persisted.
    BeforeIntent,
    /// After the transaction intent is persisted as `Prepared`.
    AfterIntent,
    /// After the daemon was stopped and `Stopped` was persisted.
    AfterStop,
    /// Immediately before the active reference is switched.
    BeforeSwitch,
    /// After the active reference was switched and `Switched` was persisted.
    AfterSwitch,
    /// After the daemon was started and `Verifying` was persisted.
    AfterStart,
    /// After readiness passed, before the durable `Confirmed` marker.
    BeforeConfirm,
    /// After the durable `Confirmed` marker, before state confirmation.
    AfterConfirm,
}

/// Fault-injection seam for activation tests. Production callers use [`apply_pending`].
#[doc(hidden)]
pub trait ActivationFault: Send + Sync {
    /// Stops the simulated process at `checkpoint` when it returns an error.
    fn checkpoint(&self, _checkpoint: ActivationCheckpoint) -> Result<(), UpdateError> {
        Ok(())
    }
}

struct NoActivationFault;

impl ActivationFault for NoActivationFault {}

/// Recovery entry point, before the daemon starts on boot or after the apply unit exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryMode {
    /// Once-per-boot recovery: only disk state may change; services are never started or stopped.
    BeforeBootStart,
    /// `ExecStopPost` recovery: may restart a daemon that was running before the failed apply.
    AfterApply,
}

/// Result of staging an upgrade request for the privileged apply unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpgradeOutcome {
    /// A request is durable and the apply unit should be started.
    Requested {
        /// The durable request the unit will consume.
        request: Box<ApplyRequest>,
    },
    /// The daemon is stopped (or absent); the candidate is staged and `--start` is required.
    RequiresStart {
        /// The staged candidate that remains available.
        candidate: GenerationId,
    },
}

/// Acquires a non-blocking shared maintenance guard.
pub fn acquire_maintenance_shared(store: &GenerationStore) -> Result<MaintenanceLock, UpdateError> {
    try_lock(store.lock_path(), true)
}

/// Acquires a non-blocking exclusive maintenance guard.
pub fn acquire_maintenance_exclusive(
    store: &GenerationStore,
) -> Result<MaintenanceLock, UpdateError> {
    try_lock(store.lock_path(), false)
}

fn try_lock(path: std::path::PathBuf, shared: bool) -> Result<MaintenanceLock, UpdateError> {
    let result = if shared {
        MaintenanceLock::try_shared(&path)
    } else {
        MaintenanceLock::try_exclusive(&path)
    };
    match result {
        Ok(guard) => Ok(guard),
        Err(source) if source.kind() == std::io::ErrorKind::WouldBlock => Err(UpdateError::Busy),
        Err(source) => Err(UpdateError::Io { path, source }),
    }
}

async fn lock_for_recovery(store: &GenerationStore) -> Result<MaintenanceLock, UpdateError> {
    // `ExecStopPost` runs after the apply process exits, but a concurrently exiting process may
    // still hold the descriptor. Wait rather than steal or skip disk reconciliation.
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match MaintenanceLock::try_exclusive(&store.lock_path()) {
            Ok(guard) => return Ok(guard),
            Err(source) if source.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(UpdateError::Busy);
                }
                tokio::time::sleep(LOCK_POLL).await;
            }
            Err(source) => {
                return Err(UpdateError::Io {
                    path: store.lock_path(),
                    source,
                });
            }
        }
    }
}

/// Validates a staged release and writes the single apply request for an upgrade.
///
/// No catalog or artifact network access occurs: only the persisted trusted authorization is
/// consulted. A stopped managed service keeps the candidate staged and reports that `--start`
/// is required.
pub async fn enqueue_upgrade(
    store: &GenerationStore,
    state: &StateStore,
    service: &SystemdService,
    start: bool,
) -> Result<UpgradeOutcome, UpdateError> {
    let _guard = acquire_maintenance_shared(store)?;
    let persisted = state.read()?;
    if persisted.pending.is_some() {
        return Err(UpdateError::DuplicateTransaction);
    }
    let requests = ApplyRequestStore::for_state(state);
    if requests.read_optional()?.is_some() {
        return Err(UpdateError::RequestPending);
    }

    let catalog = Catalog::open_offline(store.root())?;
    let release = catalog
        .persisted_authorization()?
        .ok_or(UpdateError::NoStagedRelease)?;
    if persisted.quarantined.contains(&release.digest) {
        return Err(UpdateError::Quarantined(release.digest));
    }
    catalog.authorize_staged(&release, SystemTime::now())?;
    let layout = store.verify_generation(&release.digest)?;
    if layout.release().version() != &release.version
        || layout.release().build_id() != release.build_id
    {
        return Err(UpdateError::InvalidApplyRequest(
            "the staged generation does not match the authorized release identity".to_owned(),
        ));
    }

    let current = store.current_reference()?;
    if current.as_ref() == Some(&release.digest) {
        return Err(UpdateError::AlreadyActive(release.digest));
    }
    let was_running = service.is_active().await?;
    if !start && (!was_running || current.is_none()) {
        return Ok(UpgradeOutcome::RequiresStart {
            candidate: release.digest,
        });
    }

    let request = ApplyRequest::upgrade(release, current, start);
    requests.enqueue(&request)?;
    Ok(UpgradeOutcome::Requested {
        request: Box::new(request),
    })
}

/// Writes the single apply request that restores the retained rollback target offline.
pub fn enqueue_rollback(
    store: &GenerationStore,
    state: &StateStore,
) -> Result<ApplyRequest, UpdateError> {
    let _guard = acquire_maintenance_shared(store)?;
    let persisted = state.read()?;
    if persisted.pending.is_some() {
        return Err(UpdateError::DuplicateTransaction);
    }
    let requests = ApplyRequestStore::for_state(state);
    if requests.read_optional()?.is_some() {
        return Err(UpdateError::RequestPending);
    }
    let target = persisted
        .rollback_target
        .clone()
        .ok_or(UpdateError::NoRollbackTarget)?;
    if persisted.quarantined.contains(&target) {
        return Err(UpdateError::Quarantined(target));
    }
    store.verify_generation(&target)?;
    let current = store.current_reference()?;
    if current.as_ref() == Some(&target) {
        return Err(UpdateError::AlreadyActive(target));
    }
    let request = ApplyRequest::rollback(target, current);
    requests.enqueue(&request)?;
    Ok(request)
}

/// Consumes the durable apply request and activates it with rollback on failure.
pub async fn apply_pending(
    store: &GenerationStore,
    state: &StateStore,
    catalog: &Catalog,
    service: &SystemdService,
) -> Result<(), UpdateError> {
    apply_pending_with(store, state, catalog, service, &NoActivationFault).await
}

/// Fault-injectable activation entry point used by tests.
#[doc(hidden)]
#[allow(clippy::too_many_lines)]
pub async fn apply_pending_with(
    store: &GenerationStore,
    state: &StateStore,
    catalog: &Catalog,
    service: &SystemdService,
    faults: &dyn ActivationFault,
) -> Result<(), UpdateError> {
    let _guard = acquire_maintenance_exclusive(store)?;
    faults.checkpoint(ActivationCheckpoint::AfterLock)?;

    let requests = ApplyRequestStore::for_state(state);
    let request = requests.read()?;
    request.validate()?;

    let mut persisted = state.read()?;
    if persisted.pending.is_some() {
        return Err(UpdateError::DuplicateTransaction);
    }
    if persisted.quarantined.contains(&request.generation) {
        return Err(UpdateError::Quarantined(request.generation));
    }
    let active = store.current_reference()?;
    if active != request.expected_active {
        return Err(UpdateError::StaleRequest {
            expected: request.expected_active,
            found: active,
        });
    }
    if active.as_ref() == Some(&request.generation) {
        return Err(UpdateError::AlreadyActive(request.generation));
    }

    let candidate = store.verify_generation(&request.generation)?;
    let candidate_identity = candidate.identity();
    match request.kind {
        ApplyKind::Upgrade => {
            let release = request.release.as_ref().ok_or_else(|| {
                UpdateError::InvalidApplyRequest(
                    "an upgrade request requires signed release metadata".to_owned(),
                )
            })?;
            if candidate.release().version() != &release.version
                || candidate.release().build_id() != release.build_id
            {
                return Err(UpdateError::InvalidApplyRequest(
                    "the staged generation does not match the requested release identity"
                        .to_owned(),
                ));
            }
            // Recheck the persisted, unexpired, not-superseded authorization under the lock and
            // immediately before persisting intent or stopping the daemon.
            catalog.authorize_staged(release, SystemTime::now())?;
        }
        ApplyKind::Rollback => {
            if persisted.rollback_target.as_ref() != Some(&request.generation) {
                return Err(UpdateError::RequestKindMismatch);
            }
            // Explicit rollback of an already confirmed generation is offline and must not touch
            // trusted metadata or require the catalog.
        }
    }

    let was_running = service.is_active().await?;
    if !was_running && !request.start_service && request.kind == ApplyKind::Upgrade {
        return Err(UpdateError::StartRequired);
    }

    let previous = active
        .filter(|generation| !persisted.quarantined.contains(generation))
        .and_then(|generation| store.verify_generation(&generation).ok());
    let previous_id = previous.as_ref().map(generation_from_layout).transpose()?;

    if !was_running && !request.start_service {
        return offline_rollback(
            store,
            state,
            &requests,
            &request,
            &mut persisted,
            previous_id,
            faults,
        );
    }

    faults.checkpoint(ActivationCheckpoint::BeforeIntent)?;
    let transaction = PendingTransaction {
        id: request.id,
        previous: previous_id,
        candidate: request.generation.clone(),
        phase: TransactionPhase::Prepared,
        was_running,
    };
    persisted.pending = Some(transaction.clone());
    persisted.last_result = None;
    persisted.last_error = None;
    state.write(&persisted)?;
    faults.checkpoint(ActivationCheckpoint::AfterIntent)?;

    let trial = run_trial(
        store,
        state,
        service,
        &transaction,
        &candidate_identity,
        was_running,
        faults,
    )
    .await;
    match trial {
        Ok(()) => finish_success(store, state, &requests, &transaction),
        Err(trial_error) => {
            if matches!(trial_error.error, UpdateError::SimulatedCrash(_)) {
                return Err(trial_error.error);
            }
            reconcile_failure(
                store,
                state,
                service,
                &requests,
                &transaction,
                previous.as_ref(),
                trial_error.candidate_attempted,
                trial_error.error,
            )
            .await
        }
    }
}

fn offline_rollback(
    store: &GenerationStore,
    state: &StateStore,
    requests: &ApplyRequestStore,
    request: &ApplyRequest,
    persisted: &mut UpdateState,
    previous: Option<GenerationId>,
    faults: &dyn ActivationFault,
) -> Result<(), UpdateError> {
    faults.checkpoint(ActivationCheckpoint::BeforeIntent)?;
    let transaction = PendingTransaction {
        id: request.id,
        previous,
        candidate: request.generation.clone(),
        phase: TransactionPhase::Prepared,
        was_running: false,
    };
    persisted.pending = Some(transaction.clone());
    persisted.last_result = None;
    persisted.last_error = None;
    state.write(persisted)?;
    faults.checkpoint(ActivationCheckpoint::AfterIntent)?;

    write_phase(state, transaction.id, TransactionPhase::Stopped)?;
    faults.checkpoint(ActivationCheckpoint::AfterStop)?;
    faults.checkpoint(ActivationCheckpoint::BeforeSwitch)?;
    store.switch_to(&request.generation)?;
    write_phase(state, transaction.id, TransactionPhase::Switched)?;
    faults.checkpoint(ActivationCheckpoint::AfterSwitch)?;
    write_phase(state, transaction.id, TransactionPhase::Confirmed)?;
    faults.checkpoint(ActivationCheckpoint::AfterConfirm)?;

    let mut confirmed = state.read()?;
    confirmed.confirm();
    confirmed.last_result = Some(format!(
        "restored {}; managed service left stopped",
        request.generation
    ));
    confirmed.last_error = None;
    state.write(&confirmed)?;
    remove_owned_request(requests, Some(transaction.id))?;
    Ok(())
}

struct TrialError {
    error: UpdateError,
    candidate_attempted: bool,
}

async fn run_trial(
    store: &GenerationStore,
    state: &StateStore,
    service: &SystemdService,
    transaction: &PendingTransaction,
    identity: &ReleaseIdentity,
    was_running: bool,
    faults: &dyn ActivationFault,
) -> Result<(), TrialError> {
    let mut candidate_attempted = false;
    let outcome = async {
        if was_running {
            service.stop().await?;
        }
        write_phase(state, transaction.id, TransactionPhase::Stopped)?;
        faults.checkpoint(ActivationCheckpoint::AfterStop)?;
        faults.checkpoint(ActivationCheckpoint::BeforeSwitch)?;
        store.switch_to(&transaction.candidate)?;
        write_phase(state, transaction.id, TransactionPhase::Switched)?;
        faults.checkpoint(ActivationCheckpoint::AfterSwitch)?;

        service.begin_activation().await?;
        candidate_attempted = true;
        service.start().await?;
        write_phase(state, transaction.id, TransactionPhase::Verifying)?;
        faults.checkpoint(ActivationCheckpoint::AfterStart)?;
        verify_ready(service, identity, ACTIVATION_DEADLINE).await?;
        faults.checkpoint(ActivationCheckpoint::BeforeConfirm)?;
        write_phase(state, transaction.id, TransactionPhase::Confirmed)?;
        faults.checkpoint(ActivationCheckpoint::AfterConfirm)?;
        Ok(())
    }
    .await;
    outcome.map_err(|error| TrialError {
        error,
        candidate_attempted,
    })
}

fn finish_success(
    store: &GenerationStore,
    state: &StateStore,
    requests: &ApplyRequestStore,
    transaction: &PendingTransaction,
) -> Result<(), UpdateError> {
    let current = store.current_reference()?;
    if current.as_ref() != Some(&transaction.candidate) {
        return Err(UpdateError::StaleRequest {
            expected: Some(transaction.candidate.clone()),
            found: current,
        });
    }
    let mut confirmed = state.read()?;
    confirmed.confirm();
    confirmed.last_result = Some(format!("confirmed {}", transaction.candidate));
    confirmed.last_error = None;
    state.write(&confirmed)?;
    remove_owned_request(requests, Some(transaction.id))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn reconcile_failure(
    store: &GenerationStore,
    state: &StateStore,
    service: &SystemdService,
    requests: &ApplyRequestStore,
    transaction: &PendingTransaction,
    previous: Option<&GenerationLayout>,
    candidate_attempted: bool,
    error: UpdateError,
) -> Result<(), UpdateError> {
    if let Ok(mut reverting) = state.read()
        && let Some(pending) = reverting.pending.as_mut()
    {
        pending.phase = TransactionPhase::Reverting;
        let _ = state.write(&reverting);
    }

    let active = service.is_active().await?;
    if candidate_attempted && active {
        service.stop().await?;
    }

    if let Some(previous) = previous {
        let previous_id = generation_from_layout(previous)?;
        let current = store.current_reference()?;
        if current.as_ref() != Some(&previous_id) {
            store.switch_to(&previous_id)?;
        }

        let mut recovered = state.read()?;
        recovered.pending = None;
        if candidate_attempted {
            recovered.quarantine(transaction.candidate.clone());
        }
        recovered.last_result = Some(format!(
            "reverted {} -> {previous_id}",
            transaction.candidate
        ));
        recovered.last_error = Some(error.to_string());
        state.write(&recovered)?;
        remove_owned_request(requests, Some(transaction.id))?;

        if transaction.was_running
            && !service.is_active().await?
            && let Err(previous_error) = start_and_verify_previous(service, previous).await
        {
            abandon_without_generation(
                store,
                state,
                service,
                &transaction.candidate,
                &format!("previous generation `{previous_id}` is unhealthy: {previous_error}"),
            )
            .await?;
            return Err(UpdateError::NoGoodGeneration);
        }
        return Err(error);
    }

    abandon_without_generation(
        store,
        state,
        service,
        &transaction.candidate,
        &error.to_string(),
    )
    .await?;
    Err(UpdateError::NoGoodGeneration)
}

async fn abandon_without_generation(
    store: &GenerationStore,
    state: &StateStore,
    service: &SystemdService,
    candidate: &GenerationId,
    reason: &str,
) -> Result<(), UpdateError> {
    if service.is_active().await? {
        service.stop().await?;
    }
    store.clear_current()?;
    let mut stopped = state.read()?;
    stopped.pending = None;
    stopped.quarantine(candidate.clone());
    stopped.last_result = Some(format!("stopped {candidate}"));
    stopped.last_error = Some(reason.to_owned());
    state.write(&stopped)?;
    Ok(())
}

async fn start_and_verify_previous(
    service: &SystemdService,
    previous: &GenerationLayout,
) -> Result<(), UpdateError> {
    service.begin_activation().await?;
    service.start().await?;
    verify_ready(service, &previous.identity(), ACTIVATION_DEADLINE).await
}

/// Restores disk state after an interrupted activation.
#[allow(clippy::too_many_lines)]
pub async fn recover(
    store: &GenerationStore,
    state: &StateStore,
    service: &SystemdService,
    mode: RecoveryMode,
) -> Result<(), UpdateError> {
    let _guard = match mode {
        RecoveryMode::BeforeBootStart => acquire_maintenance_exclusive(store)?,
        RecoveryMode::AfterApply => lock_for_recovery(store).await?,
    };
    let requests = ApplyRequestStore::for_state(state);
    let mut persisted = state.read()?;
    let Some(pending) = persisted.pending.clone() else {
        // A process may have been lost after enqueueing but before writing intent. The request is
        // stale and must not be consumed by a later unrelated activation.
        remove_owned_request(&requests, None)?;
        return Ok(());
    };

    if pending.phase == TransactionPhase::Confirmed {
        let current = store.current_reference()?;
        if current.as_ref() != Some(&pending.candidate) {
            return Err(UpdateError::StaleRequest {
                expected: Some(pending.candidate),
                found: current,
            });
        }
        persisted.confirm();
        persisted.last_result = Some(format!("confirmed {}", pending.candidate));
        persisted.last_error = None;
        state.write(&persisted)?;
        remove_owned_request(&requests, Some(pending.id))?;
        return Ok(());
    }

    let previous = pending
        .previous
        .clone()
        .filter(|generation| !persisted.quarantined.contains(generation))
        .and_then(|generation| store.verify_generation(&generation).ok());

    match (mode, previous) {
        (RecoveryMode::BeforeBootStart, Some(previous)) => {
            let previous_id = generation_from_layout(&previous)?;
            if store.current_reference()?.as_ref() != Some(&previous_id) {
                store.switch_to(&previous_id)?;
            }
            persisted.pending = None;
            persisted.last_result = Some(format!(
                "recovered {previous_id} before boot (service not started)"
            ));
            persisted.last_error = Some("interrupted activation reverted".to_owned());
            state.write(&persisted)?;
            remove_owned_request(&requests, Some(pending.id))?;
            Ok(())
        }
        (RecoveryMode::BeforeBootStart, None) => {
            store.clear_current()?;
            persisted.pending = None;
            persisted.quarantine(pending.candidate.clone());
            persisted.last_result = Some(format!("stopped {} before boot", pending.candidate));
            persisted.last_error = Some(
                "no healthy previous generation is available; managed service left stopped"
                    .to_owned(),
            );
            state.write(&persisted)?;
            remove_owned_request(&requests, Some(pending.id))?;
            Err(UpdateError::NoGoodGeneration)
        }
        (RecoveryMode::AfterApply, Some(previous)) => {
            let previous_id = generation_from_layout(&previous)?;
            let current = store.current_reference()?;
            if current.as_ref() != Some(&previous_id) && service.is_active().await? {
                service.stop().await?;
            }
            if store.current_reference()?.as_ref() != Some(&previous_id) {
                store.switch_to(&previous_id)?;
            }
            persisted.pending = None;
            persisted.last_result = Some(format!("recovered {previous_id}"));
            persisted.last_error = Some("interrupted activation reverted".to_owned());
            state.write(&persisted)?;
            remove_owned_request(&requests, Some(pending.id))?;

            if pending.was_running
                && !service.is_active().await?
                && let Err(previous_error) = start_and_verify_previous(service, &previous).await
            {
                abandon_without_generation(
                    store,
                    state,
                    service,
                    &pending.candidate,
                    &format!("previous generation `{previous_id}` is unhealthy: {previous_error}"),
                )
                .await?;
                return Err(UpdateError::NoGoodGeneration);
            }
            Ok(())
        }
        (RecoveryMode::AfterApply, None) => {
            abandon_without_generation(
                store,
                state,
                service,
                &pending.candidate,
                "no healthy previous generation is available; managed service left stopped",
            )
            .await?;
            remove_owned_request(&requests, Some(pending.id))?;
            Err(UpdateError::NoGoodGeneration)
        }
    }
}

fn write_phase(state: &StateStore, id: Uuid, phase: TransactionPhase) -> Result<(), UpdateError> {
    let mut persisted = state.read()?;
    let Some(pending) = persisted.pending.as_mut() else {
        return Err(UpdateError::MissingApplyRequest);
    };
    if pending.id != id {
        return Err(UpdateError::DuplicateTransaction);
    }
    pending.phase = phase;
    state.write(&persisted)
}

fn remove_owned_request(
    requests: &ApplyRequestStore,
    owner: Option<Uuid>,
) -> Result<(), UpdateError> {
    match requests.read_optional()? {
        Some(request) if owner.is_none() || owner == Some(request.id) => requests.remove(),
        _ => Ok(()),
    }
}

fn generation_from_layout(layout: &GenerationLayout) -> Result<GenerationId, UpdateError> {
    let name = layout
        .root()
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    GenerationId::new(name)
}
