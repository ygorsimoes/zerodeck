//! Administrative first-install and conversion bootstrap of one verified generation.
//!
//! This is deliberately separate from `update`/`upgrade`: it runs only after an operator has
//! verified an out-of-band package digest, installs an already extracted generation, and uses the
//! same durable transaction markers as a managed activation so an interrupted bootstrap is
//! reconciled by boot recovery instead of leaving an ambiguous `current` reference.
//!
//! A starting bootstrap is two-phase: [`prepare_bootstrap`] switches `current` and records a
//! pending transaction, and [`confirm_bootstrap`] promotes it to `last_confirmed` only after the
//! caller has started the daemon and verified its authenticated `/readyz` identity.

use std::path::Path;

use uuid::Uuid;

use crate::{
    ApplyRequestStore, BootRecoveryMarker, GenerationId, GenerationStore, PendingTransaction,
    StateStore, TransactionPhase, UpdateError, acquire_maintenance_exclusive,
};

/// Result of a successful (or check-only) administrative bootstrap step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapOutcome {
    /// Generation selected as the active reference.
    pub generation: GenerationId,
    /// Generation that was active before the bootstrap, when converting.
    pub previous: Option<GenerationId>,
    /// Generation retained for an offline rollback after the bootstrap.
    pub rollback_target: Option<GenerationId>,
    /// Whether any state or active reference changed.
    pub changed: bool,
    /// Whether the generation is confirmed (`last_confirmed`) rather than merely prepared.
    pub confirmed: bool,
}

/// Installs or converts to `generation` under `root` without starting the daemon.
///
/// The caller must already have verified the generation's archive against an out-of-band digest
/// and extracted it under `<root>/generations/<digest>`. With `check`, this validates all refusal
/// conditions without changing state and is used before replacing a controller binary. With
/// `convert`, an existing healthy active generation is retained as the rollback target; without it,
/// replacing an active generation is refused.
pub fn prepare_bootstrap(
    root: impl AsRef<Path>,
    generation: &str,
    convert: bool,
    check: bool,
) -> Result<BootstrapOutcome, UpdateError> {
    let root = root.as_ref();
    let store = GenerationStore::new(root)?;
    let _guard = acquire_maintenance_exclusive(&store)?;
    let state = StateStore::new(root.join("state/update.json"));
    let requests = ApplyRequestStore::for_state(&state);

    if requests.read_optional()?.is_some() {
        return Err(UpdateError::BootstrapRefused(
            "an unconsumed apply request exists; resolve or remove it before bootstrapping"
                .to_owned(),
        ));
    }

    let generation = GenerationId::new(generation)?;
    store.verify_generation(&generation)?;

    let mut persisted = state.read()?;
    if let Some(pending) = &persisted.pending {
        return Err(UpdateError::BootstrapRefused(format!(
            "a managed activation is pending (phase {:?}); refusing to replace the controller or \
             active generation",
            pending.phase
        )));
    }

    let previous = store.current_reference()?;
    if previous.as_ref() == Some(&generation) {
        if check {
            return Ok(outcome(
                generation,
                previous,
                persisted.rollback_target,
                false,
                false,
            ));
        }
        let mut updated = persisted;
        let mut changed = false;
        if updated.last_confirmed.as_ref() != Some(&generation) {
            updated.last_confirmed = Some(generation.clone());
            changed = true;
        }
        if changed {
            updated.last_result = Some(format!("bootstrapped generation {generation}"));
            updated.last_error = None;
            state.write(&updated)?;
        }
        BootRecoveryMarker::record(&store)?;
        return Ok(outcome(
            generation,
            None,
            updated.rollback_target,
            changed,
            true,
        ));
    }

    if let Some(previous) = &previous {
        if !convert {
            return Err(UpdateError::BootstrapRefused(
                "an active generation already exists; pass --convert-existing-zerodeck to retain \
                 it as the rollback target"
                    .to_owned(),
            ));
        }
        store.verify_generation(previous)?;
    }

    let mut rollback_target = persisted.rollback_target.clone();
    if let Some(previous) = &previous {
        rollback_target = Some(previous.clone());
    } else if let Some(confirmed) = &persisted.last_confirmed
        && confirmed != &generation
        && store.verify_generation(confirmed).is_ok()
    {
        rollback_target = Some(confirmed.clone());
    }

    if check {
        return Ok(outcome(generation, previous, rollback_target, false, false));
    }

    let transaction = PendingTransaction {
        id: Uuid::new_v4(),
        previous: previous.clone(),
        candidate: generation.clone(),
        phase: TransactionPhase::Prepared,
        was_running: false,
        start_attempted: false,
        rollback_target: rollback_target.clone(),
    };
    persisted.pending = Some(transaction.clone());
    state.write(&persisted)?;

    store.switch_to(&generation)?;
    persisted
        .pending
        .as_mut()
        .expect("pending was written above")
        .phase = TransactionPhase::Switched;
    state.write(&persisted)?;
    BootRecoveryMarker::record(&store)?;

    Ok(outcome(generation, previous, rollback_target, true, false))
}

/// Promotes a prepared bootstrap to `last_confirmed` after the caller verified readiness.
///
/// For a bootstrap without `--start`, call this immediately: there is no running process to probe,
/// and the operator explicitly chose not to start the service.
pub fn confirm_bootstrap(root: impl AsRef<Path>) -> Result<BootstrapOutcome, UpdateError> {
    let root = root.as_ref();
    let store = GenerationStore::new(root)?;
    let _guard = acquire_maintenance_exclusive(&store)?;
    let state = StateStore::new(root.join("state/update.json"));
    let mut persisted = state.read()?;

    let Some(pending) = persisted.pending.clone() else {
        let current = store.current_reference()?;
        let confirmed = current
            .as_ref()
            .is_some_and(|current| persisted.last_confirmed.as_ref() == Some(current));
        if !confirmed {
            return Err(UpdateError::BootstrapRefused(
                "no prepared bootstrap transaction exists and the active generation is not \
                 confirmed"
                    .to_owned(),
            ));
        }
        BootRecoveryMarker::record(&store)?;
        return Ok(outcome(
            current.expect("confirmed current"),
            None,
            persisted.rollback_target,
            false,
            true,
        ));
    };

    let current = store.current_reference()?;
    if current.as_ref() != Some(&pending.candidate) {
        return Err(UpdateError::StaleRequest {
            expected: Some(pending.candidate),
            found: current,
        });
    }
    if pending.phase != TransactionPhase::Switched {
        return Err(UpdateError::BootstrapRefused(format!(
            "the prepared bootstrap is in phase {:?}, not a confirmable phase",
            pending.phase
        )));
    }
    let previous = pending.previous.clone();
    persisted.confirm();
    persisted.last_result = Some(format!("bootstrapped generation {}", pending.candidate));
    persisted.last_error = None;
    state.write(&persisted)?;
    BootRecoveryMarker::record(&store)?;
    store.gc(&persisted, std::slice::from_ref(&pending.candidate))?;

    Ok(outcome(
        pending.candidate,
        previous,
        persisted.rollback_target,
        true,
        true,
    ))
}

/// Prepares and immediately confirms `generation` without starting the daemon.
///
/// This preserves the historical single-call API for callers that do not start a service. A
/// caller that starts the daemon must use [`prepare_bootstrap`], verify readiness, then call
/// [`confirm_bootstrap`]; otherwise the generation is recorded as healthy before it ever ran.
pub fn bootstrap_generation(
    root: impl AsRef<Path>,
    generation: &str,
    convert: bool,
    check: bool,
) -> Result<BootstrapOutcome, UpdateError> {
    let prepared = prepare_bootstrap(root.as_ref(), generation, convert, check)?;
    if check {
        return Ok(prepared);
    }
    let mut confirmed = confirm_bootstrap(root.as_ref())?;
    confirmed.previous = prepared.previous;
    confirmed.changed |= prepared.changed;
    Ok(confirmed)
}

fn outcome(
    generation: GenerationId,
    previous: Option<GenerationId>,
    rollback_target: Option<GenerationId>,
    changed: bool,
    confirmed: bool,
) -> BootstrapOutcome {
    BootstrapOutcome {
        generation,
        previous,
        rollback_target,
        changed,
        confirmed,
    }
}

#[cfg(test)]
mod tests {
    use super::BootstrapOutcome;
    use crate::GenerationId;

    #[test]
    fn bootstrap_outcome_is_debug() {
        let outcome = BootstrapOutcome {
            generation: GenerationId::new("a".repeat(64)).expect("valid"),
            previous: None,
            rollback_target: None,
            changed: false,
            confirmed: false,
        };
        assert!(format!("{outcome:?}").contains("BootstrapOutcome"));
    }
}
