//! Privileged activation, offline rollback, and crash recovery.

use std::time::{Duration, Instant, SystemTime};

use semver::Version;
use uuid::Uuid;
use zerodeck_platform::MaintenanceLock;

use crate::{
    ACTIVATION_DEADLINE, ApplyKind, ApplyRequest, ApplyRequestStore, BootRecoveryMarker, Catalog,
    Channel, GenerationId, GenerationLayout, GenerationStore, PendingTransaction, ReleaseIdentity,
    StateStore, SystemdService, TransactionPhase, UpdateError, UpdateState, verify_ready,
};

/// Managed daemon unit controlled by systemd.
pub const DAEMON_UNIT: &str = "zerodeck.service";
/// Independent oneshot unit that performs activation.
pub const APPLY_UNIT: &str = "zerodeck-apply.service";

const LOCK_WAIT: Duration = Duration::from_secs(30);
/// Boot recovery must finish well inside `zerodeck-recovery.service`'s 60 s start timeout.
const BOOT_LOCK_WAIT: Duration = Duration::from_secs(45);
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
    /// After the previous reference was restored, before restarting the previous daemon.
    BeforePreviousRestart,
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

async fn lock_for_recovery(
    store: &GenerationStore,
    wait: Duration,
) -> Result<MaintenanceLock, UpdateError> {
    // `ExecStopPost` runs after the apply process exits, but a concurrently exiting process may
    // still hold the descriptor. Boot recovery can race a long `update` that holds a shared guard.
    // Wait within the unit's start timeout rather than steal or skip disk reconciliation.
    let deadline = Instant::now() + wait;
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

/// Validates a staged Stable release and writes the single apply request for an upgrade.
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
    enqueue_upgrade_with_channel(store, state, service, start, Channel::Stable).await
}

/// Validates a staged release for the explicitly requested channel.
///
/// Testing authorization is never accepted by the Stable entry point. The persisted selection and
/// the signed release must both match `channel`, and the candidate is rechecked against the live
/// installed generation and the running updater version under the exclusive lock.
pub async fn enqueue_upgrade_with_channel(
    store: &GenerationStore,
    state: &StateStore,
    service: &SystemdService,
    start: bool,
    channel: Channel,
) -> Result<UpgradeOutcome, UpdateError> {
    // Slot mutation is exclusive: two shared holders must never pass the check-then-write
    // sequence and replace each other's request.
    let _guard = acquire_maintenance_exclusive(store)?;
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
    let selection = catalog
        .persisted_selection()?
        .ok_or(UpdateError::NoStagedRelease)?;
    if selection.channel != channel {
        return Err(UpdateError::SelectionChannelMismatch {
            selected: selection.channel.as_str(),
            requested: channel.as_str(),
        });
    }
    if release.channel != selection.channel {
        return Err(UpdateError::ChannelMismatch {
            selected: selection.channel.as_str(),
            release: release.channel.as_str(),
        });
    }
    let updater_version = running_updater_version();
    if release.min_updater > updater_version {
        return Err(UpdateError::UpdaterTooOld {
            required: release.min_updater.clone(),
            current: updater_version,
        });
    }
    if selection.updater_version > updater_version {
        return Err(UpdateError::UpdaterTooOld {
            required: selection.updater_version,
            current: updater_version,
        });
    }
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
    if let Some(current) = &current {
        let installed = store.verify_generation(current)?;
        if release.version < *installed.release().version() {
            return Err(UpdateError::DowngradeRejected {
                installed: installed.release().version().clone(),
                candidate: release.version.clone(),
            });
        }
        if release.version == *installed.release().version() && current != &release.digest {
            return Err(UpdateError::VersionConflict {
                version: release.version.clone(),
                installed: current.clone(),
                candidate: release.digest.clone(),
            });
        }
    }
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

fn running_updater_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("workspace version is SemVer")
}

/// Writes the single apply request that restores the retained rollback target offline.
pub fn enqueue_rollback(
    store: &GenerationStore,
    state: &StateStore,
) -> Result<ApplyRequest, UpdateError> {
    // Slot mutation is exclusive: two shared holders must never pass the check-then-write
    // sequence and replace each other's request.
    let _guard = acquire_maintenance_exclusive(store)?;
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
    let request = match requests.read() {
        Ok(request) => request,
        Err(error) => {
            // A malformed request would otherwise wedge every later upgrade behind
            // `RequestPending`. The exclusive lock proves this helper owns the slot.
            if matches!(error, UpdateError::InvalidApplyRequest(_)) {
                requests.remove()?;
            }
            return Err(error);
        }
    };
    request.validate()?;

    let mut persisted = state.read()?;
    let prepared = match prepare_apply(store, catalog, service, &request, &persisted).await {
        Ok(prepared) => prepared,
        Err(error) => {
            if discard_terminal_pre_intent_request(&error) {
                remove_owned_request(&requests, request.id)?;
            }
            return Err(error);
        }
    };

    if prepared.offline_rollback {
        return offline_rollback(
            store,
            state,
            &requests,
            &request,
            &mut persisted,
            prepared.previous_id,
            faults,
        );
    }

    faults.checkpoint(ActivationCheckpoint::BeforeIntent)?;
    let transaction = PendingTransaction {
        id: request.id,
        previous: prepared.previous_id,
        candidate: request.generation.clone(),
        phase: TransactionPhase::Prepared,
        was_running: prepared.was_running,
        start_attempted: false,
        rollback_target: None,
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
        &prepared.candidate_identity,
        prepared.was_running,
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
                prepared.previous.as_ref(),
                trial_error.start_attempted,
                trial_error.error,
                faults,
            )
            .await
        }
    }
}

struct PreparedApply {
    previous: Option<GenerationLayout>,
    previous_id: Option<GenerationId>,
    candidate_identity: ReleaseIdentity,
    was_running: bool,
    offline_rollback: bool,
}

async fn prepare_apply(
    store: &GenerationStore,
    catalog: &Catalog,
    service: &SystemdService,
    request: &ApplyRequest,
    persisted: &UpdateState,
) -> Result<PreparedApply, UpdateError> {
    if persisted.pending.is_some() {
        return Err(UpdateError::DuplicateTransaction);
    }
    if persisted.quarantined.contains(&request.generation) {
        return Err(UpdateError::Quarantined(request.generation.clone()));
    }
    let active = store.current_reference()?;
    if active != request.expected_active {
        return Err(UpdateError::StaleRequest {
            expected: request.expected_active.clone(),
            found: active,
        });
    }
    if active.as_ref() == Some(&request.generation) {
        return Err(UpdateError::AlreadyActive(request.generation.clone()));
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
        .as_ref()
        .filter(|generation| !persisted.quarantined.contains(generation))
        .and_then(|generation| store.verify_generation(generation).ok());
    let previous_id = previous.as_ref().map(generation_from_layout).transpose()?;

    Ok(PreparedApply {
        previous,
        previous_id,
        candidate_identity,
        was_running,
        offline_rollback: !was_running && !request.start_service,
    })
}

/// Pre-intent failures that can never make progress with the same request must not hold the single
/// apply-request slot forever. Transient environment failures keep the request for a retry.
fn discard_terminal_pre_intent_request(error: &UpdateError) -> bool {
    matches!(
        error,
        UpdateError::Quarantined(_)
            | UpdateError::StaleRequest { .. }
            | UpdateError::AlreadyActive(_)
            | UpdateError::RequestKindMismatch
            | UpdateError::MissingGeneration(_)
            | UpdateError::CorruptGeneration(_)
            | UpdateError::InvalidApplyRequest(_)
            | UpdateError::ReleaseNotAuthorized { .. }
            | UpdateError::ReleaseSuperseded { .. }
            | UpdateError::AuthorizationExpired { .. }
            | UpdateError::ClockRollback { .. }
            | UpdateError::CatalogNotVerified
            | UpdateError::CatalogNotLoaded
            | UpdateError::NoStagedRelease
            | UpdateError::StartRequired
    )
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
        start_attempted: false,
        rollback_target: None,
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
    require_pending(&confirmed, transaction.id)?;
    // Remove only this transaction's own request while its pending marker still exists, then
    // clear the marker; a crash in between can safely replay confirmation.
    remove_owned_request(requests, transaction.id)?;
    confirmed.confirm();
    confirmed.last_result = Some(format!(
        "restored {}; managed service left stopped",
        request.generation
    ));
    confirmed.last_error = None;
    state.write(&confirmed)?;
    store.gc(&confirmed, &staged_generations(store))?;
    Ok(())
}

struct TrialError {
    error: UpdateError,
    start_attempted: bool,
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
    let mut start_attempted = false;
    let outcome = async {
        // Capture the pre-activation systemd invocation before stopping. Capturing it after the
        // switch compared the new invocation with an already-empty baseline, so an old process
        // answering `/readyz` could pass. `ready_once` now also requires an active unit, a main
        // PID, and a matching `INVOCATION_ID`.
        service.begin_activation().await?;
        if was_running {
            service.stop().await?;
        }
        write_phase(state, transaction.id, TransactionPhase::Stopped)?;
        faults.checkpoint(ActivationCheckpoint::AfterStop)?;
        faults.checkpoint(ActivationCheckpoint::BeforeSwitch)?;
        store.switch_to(&transaction.candidate)?;
        write_phase(state, transaction.id, TransactionPhase::Switched)?;
        faults.checkpoint(ActivationCheckpoint::AfterSwitch)?;

        write_start_attempted(state, transaction.id)?;
        start_attempted = true;
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
        start_attempted,
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
    require_pending(&confirmed, transaction.id)?;
    // The request belongs to this transaction: remove it before clearing the pending marker so a
    // crash cannot leave a stale request that a no-pending recovery must not delete.
    remove_owned_request(requests, transaction.id)?;
    confirmed.confirm();
    confirmed.last_result = Some(format!("confirmed {}", transaction.candidate));
    confirmed.last_error = None;
    state.write(&confirmed)?;
    // Housekeeping runs under the exclusive guard the caller already holds. `gc` itself keeps
    // `current`, the confirmed and rollback generations, and any authorized staged candidate.
    store.gc(&confirmed, &staged_generations(store))?;
    Ok(())
}

fn staged_generations(store: &GenerationStore) -> Vec<GenerationId> {
    Catalog::open_offline(store.root())
        .and_then(|catalog| catalog.persisted_authorization())
        .ok()
        .flatten()
        .map_or_else(Vec::new, |release| vec![release.digest])
}

#[allow(clippy::too_many_arguments)]
async fn reconcile_failure(
    store: &GenerationStore,
    state: &StateStore,
    service: &SystemdService,
    requests: &ApplyRequestStore,
    transaction: &PendingTransaction,
    previous: Option<&GenerationLayout>,
    start_attempted: bool,
    error: UpdateError,
    faults: &dyn ActivationFault,
) -> Result<(), UpdateError> {
    // Keep the pending transaction (including `was_running`) until recovery is durable. A process
    // loss anywhere below must leave a state that a second recovery can finish.
    let mut reverting = state.read()?;
    let Some(pending) = reverting.pending.as_mut() else {
        return Err(UpdateError::MissingApplyRequest);
    };
    pending.phase = TransactionPhase::Reverting;
    state.write(&reverting)?;

    let active = service.is_active().await?;
    if start_attempted && active {
        service.stop().await?;
    }

    if let Some(previous) = previous {
        let previous_id = generation_from_layout(previous)?;
        let current = store.current_reference()?;
        if current.as_ref() != Some(&previous_id) {
            store.switch_to(&previous_id)?;
        }

        if transaction.was_running {
            faults.checkpoint(ActivationCheckpoint::BeforePreviousRestart)?;
            if !service.is_active().await?
                && let Err(previous_error) = start_and_verify_previous(service, previous).await
            {
                abandon_without_generation(
                    store,
                    state,
                    Some(service),
                    requests,
                    &transaction.candidate,
                    &format!("previous generation `{previous_id}` is unhealthy: {previous_error}"),
                    start_attempted,
                )
                .await?;
                return Err(UpdateError::NoGoodGeneration);
            }
        }

        let mut recovered = state.read()?;
        if start_attempted {
            recovered.quarantine(transaction.candidate.clone());
        }
        recovered.last_result = Some(format!(
            "reverted {} -> {previous_id}",
            transaction.candidate
        ));
        recovered.last_error = Some(error.to_string());
        state.write(&recovered)?;
        remove_owned_request(requests, transaction.id)?;
        recovered.pending = None;
        state.write(&recovered)?;
        return Err(error);
    }

    abandon_without_generation(
        store,
        state,
        Some(service),
        requests,
        &transaction.candidate,
        &error.to_string(),
        start_attempted,
    )
    .await?;
    Err(UpdateError::NoGoodGeneration)
}

async fn abandon_without_generation(
    store: &GenerationStore,
    state: &StateStore,
    service: Option<&SystemdService>,
    requests: &ApplyRequestStore,
    candidate: &GenerationId,
    reason: &str,
    start_attempted: bool,
) -> Result<(), UpdateError> {
    if let Some(service) = service
        && service.is_active().await?
    {
        service.stop().await?;
    }
    store.clear_current()?;
    let mut stopped = state.read()?;
    let Some(pending) = stopped.pending.as_ref() else {
        return Err(UpdateError::MissingApplyRequest);
    };
    let transaction_id = pending.id;
    if start_attempted {
        stopped.quarantine(candidate.clone());
    }
    stopped.last_result = Some(format!("stopped {candidate}"));
    stopped.last_error = Some(reason.to_owned());
    state.write(&stopped)?;
    remove_owned_request(requests, transaction_id)?;
    stopped.pending = None;
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
///
/// `AfterApply` may restart a daemon that was previously running, so it can use the loopback
/// readiness endpoint. `BeforeBootStart` and the no-pending path never construct or use a
/// readiness client here. Neither mode opens the catalog or any external network resource.
#[allow(clippy::too_many_lines)]
pub async fn recover(
    store: &GenerationStore,
    state: &StateStore,
    service: &SystemdService,
    mode: RecoveryMode,
) -> Result<(), UpdateError> {
    recover_inner(store, state, Some(service), mode, &NoActivationFault).await
}

/// Disk-only boot recovery that never constructs or touches a readiness client.
pub async fn recover_before_boot(
    store: &GenerationStore,
    state: &StateStore,
) -> Result<(), UpdateError> {
    let result = recover_inner(
        store,
        state,
        None,
        RecoveryMode::BeforeBootStart,
        &NoActivationFault,
    )
    .await;
    // A fully reconciled disk state — including the deliberate "no good generation" stop — lets
    // the launcher start. Any other error leaves the marker absent so `exec-current` fails closed.
    if matches!(result, Ok(()) | Err(UpdateError::NoGoodGeneration)) {
        BootRecoveryMarker::record(store)?;
    }
    result
}

/// Fault-injectable recovery entry point used by tests.
#[doc(hidden)]
pub async fn recover_with(
    store: &GenerationStore,
    state: &StateStore,
    service: &SystemdService,
    mode: RecoveryMode,
    faults: &dyn ActivationFault,
) -> Result<(), UpdateError> {
    recover_inner(store, state, Some(service), mode, faults).await
}

#[allow(clippy::too_many_lines)]
async fn recover_inner(
    store: &GenerationStore,
    state: &StateStore,
    service: Option<&SystemdService>,
    mode: RecoveryMode,
    faults: &dyn ActivationFault,
) -> Result<(), UpdateError> {
    let _guard = match mode {
        RecoveryMode::BeforeBootStart => lock_for_recovery(store, BOOT_LOCK_WAIT).await?,
        RecoveryMode::AfterApply => lock_for_recovery(store, LOCK_WAIT).await?,
    };
    let requests = ApplyRequestStore::for_state(state);
    let mut persisted = state.read()?;
    let Some(pending) = persisted.pending.clone() else {
        // The slot may belong to a live or independently failed operation. Without a pending
        // transaction this helper has no ownership proof, so it must not delete anything.
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
        remove_owned_request(&requests, pending.id)?;
        persisted.confirm();
        persisted.last_result = Some(format!("confirmed {}", pending.candidate));
        persisted.last_error = None;
        state.write(&persisted)?;
        store.gc(&persisted, &staged_generations(store))?;
        return Ok(());
    }

    let previous = pending
        .previous
        .clone()
        .filter(|generation| !persisted.quarantined.contains(generation))
        .and_then(|generation| store.verify_generation(&generation).ok());
    persisted
        .pending
        .as_mut()
        .expect("pending was cloned above")
        .phase = TransactionPhase::Reverting;
    state.write(&persisted)?;

    match (mode, previous) {
        (RecoveryMode::BeforeBootStart, Some(previous)) => {
            let previous_id = generation_from_layout(&previous)?;
            if store.current_reference()?.as_ref() != Some(&previous_id) {
                store.switch_to(&previous_id)?;
            }
            persisted.last_result = Some(format!(
                "recovered {previous_id} before boot (service not started)"
            ));
            if pending.start_attempted {
                persisted.quarantine(pending.candidate.clone());
                persisted.last_error = Some(format!(
                    "interrupted activation; candidate {} was started and is quarantined",
                    pending.candidate
                ));
            } else {
                persisted.last_error = None;
            }
            state.write(&persisted)?;
            remove_owned_request(&requests, pending.id)?;
            persisted.pending = None;
            state.write(&persisted)?;
            store.gc(&persisted, &staged_generations(store))?;
            Ok(())
        }
        (RecoveryMode::BeforeBootStart, None) => {
            abandon_without_generation(
                store,
                state,
                None,
                &requests,
                &pending.candidate,
                "no healthy previous generation is available; managed service left stopped",
                pending.start_attempted,
            )
            .await?;
            Err(UpdateError::NoGoodGeneration)
        }
        (RecoveryMode::AfterApply, Some(previous)) => {
            let service = service.ok_or_else(|| UpdateError::SystemctlUnsupported {
                unit: DAEMON_UNIT.to_owned(),
                operation: "recover-after-apply",
            })?;
            let previous_id = generation_from_layout(&previous)?;
            let current = store.current_reference()?;
            if current.as_ref() != Some(&previous_id) && service.is_active().await? {
                service.stop().await?;
            }
            if store.current_reference()?.as_ref() != Some(&previous_id) {
                store.switch_to(&previous_id)?;
            }

            if pending.was_running {
                faults.checkpoint(ActivationCheckpoint::BeforePreviousRestart)?;
                if !service.is_active().await?
                    && let Err(previous_error) = start_and_verify_previous(service, &previous).await
                {
                    abandon_without_generation(
                        store,
                        state,
                        Some(service),
                        &requests,
                        &pending.candidate,
                        &format!(
                            "previous generation `{previous_id}` is unhealthy: {previous_error}"
                        ),
                        pending.start_attempted,
                    )
                    .await?;
                    return Err(UpdateError::NoGoodGeneration);
                }
            }

            persisted.last_result = Some(format!("recovered {previous_id}"));
            if pending.start_attempted {
                persisted.quarantine(pending.candidate.clone());
                persisted.last_error = Some(format!(
                    "interrupted activation; candidate {} was started and is quarantined",
                    pending.candidate
                ));
            } else {
                persisted.last_error = None;
            }
            state.write(&persisted)?;
            remove_owned_request(&requests, pending.id)?;
            persisted.pending = None;
            state.write(&persisted)?;
            store.gc(&persisted, &staged_generations(store))?;
            Ok(())
        }
        (RecoveryMode::AfterApply, None) => {
            let service = service.ok_or_else(|| UpdateError::SystemctlUnsupported {
                unit: DAEMON_UNIT.to_owned(),
                operation: "recover-after-apply",
            })?;
            abandon_without_generation(
                store,
                state,
                Some(service),
                &requests,
                &pending.candidate,
                "no healthy previous generation is available; managed service left stopped",
                pending.start_attempted,
            )
            .await?;
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

fn write_start_attempted(state: &StateStore, id: Uuid) -> Result<(), UpdateError> {
    let mut persisted = state.read()?;
    let Some(pending) = persisted.pending.as_mut() else {
        return Err(UpdateError::MissingApplyRequest);
    };
    if pending.id != id {
        return Err(UpdateError::DuplicateTransaction);
    }
    pending.start_attempted = true;
    state.write(&persisted)
}

fn require_pending(state: &UpdateState, id: Uuid) -> Result<(), UpdateError> {
    match state.pending.as_ref() {
        Some(pending) if pending.id == id => Ok(()),
        Some(_) => Err(UpdateError::DuplicateTransaction),
        None => Err(UpdateError::MissingApplyRequest),
    }
}

fn remove_owned_request(
    requests: &ApplyRequestStore,
    transaction: Uuid,
) -> Result<(), UpdateError> {
    match requests.read_optional()? {
        Some(request) if request.id == transaction => requests.remove(),
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
