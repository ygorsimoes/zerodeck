//! Administrative first-install and conversion bootstrap of one verified generation.
//!
//! This is deliberately separate from `update`/`upgrade`: it runs only after an operator has
//! verified an out-of-band package digest, installs an already extracted generation, and uses the
//! same durable transaction markers as a managed activation so an interrupted bootstrap is
//! reconciled by boot recovery instead of leaving an ambiguous `current` reference.

use std::path::Path;

use uuid::Uuid;

use crate::{
    ApplyRequestStore, GenerationId, GenerationStore, PendingTransaction, StateStore,
    TransactionPhase, UpdateError, acquire_maintenance_exclusive,
};

/// Result of a successful (or check-only) administrative bootstrap.
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
}

/// Installs or converts to `generation` under `root`.
///
/// The caller must already have verified the generation's archive against an out-of-band digest
/// and extracted it under `<root>/generations/<digest>`. With `check`, this validates all refusal
/// conditions without changing state and is used before replacing a controller binary. With
/// `convert`, an existing healthy active generation is retained as the rollback target; without it,
/// replacing an active generation is refused.
pub fn bootstrap_generation(
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
        return Ok(outcome(generation, None, updated.rollback_target, changed));
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
        return Ok(outcome(generation, previous, rollback_target, false));
    }

    let mut transaction = PendingTransaction {
        id: Uuid::new_v4(),
        previous: previous.clone(),
        candidate: generation.clone(),
        phase: TransactionPhase::Prepared,
        was_running: false,
        start_attempted: false,
    };
    persisted.pending = Some(transaction.clone());
    state.write(&persisted)?;

    store.switch_to(&generation)?;
    transaction.phase = TransactionPhase::Confirmed;
    persisted.pending = Some(transaction);
    state.write(&persisted)?;

    persisted.confirm();
    if rollback_target.is_some() {
        persisted.rollback_target.clone_from(&rollback_target);
    }
    persisted.last_result = Some(format!("bootstrapped generation {generation}"));
    persisted.last_error = None;
    state.write(&persisted)?;

    Ok(outcome(generation, previous, rollback_target, true))
}

fn outcome(
    generation: GenerationId,
    previous: Option<GenerationId>,
    rollback_target: Option<GenerationId>,
    changed: bool,
) -> BootstrapOutcome {
    BootstrapOutcome {
        generation,
        previous,
        rollback_target,
        changed,
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
        };
        assert!(format!("{outcome:?}").contains("BootstrapOutcome"));
    }
}
