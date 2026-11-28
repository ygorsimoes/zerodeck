//! Deterministic CEF supervision state machine and its wire-facing status.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{CdpTabDescriptor, CefError, is_gamepadui_tab, is_loopback_ws_url};

/// Public snapshot of the Steam integration, independent from daemon health.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CefStatus {
    /// Machine phase: `waiting_for_steam`, `attaching`, `bootstrapping`, `active`, `backoff`,
    /// `safe_mode`, or `disabled`.
    pub state: String,
    /// Most recent reason for the current phase, when one exists.
    pub reason: Option<String>,
    /// Identifier of the injection attempt the frontend must confirm, when one is outstanding.
    pub attempt: Option<String>,
    /// CEF target id of the current attempt.
    pub context: Option<String>,
    /// Whether the injected frontend confirmed that the Quick Access Menu is mounted.
    pub qam_ready: bool,
    /// Failures still inside the rolling failure window.
    pub consecutive_failures: usize,
    /// Milliseconds until the next automatic attempt while in `backoff`.
    pub retry_in_ms: Option<u64>,
}

/// Confirmation sent by the injected frontend for one specific injection attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CefConfirmation {
    /// Daemon instance that created the attempt.
    pub instance: String,
    /// CEF target id the bootstrap was injected into.
    pub context: String,
    /// Opaque attempt id carried by the injected bootstrap.
    pub attempt: String,
    /// True once the Quick Access Menu is mounted, not merely when the bundle connected.
    #[serde(default)]
    pub qam_ready: bool,
}

/// One injection attempt, bound to a daemon instance, CEF target and attempt id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InjectionAttempt {
    pub instance: String,
    pub context: String,
    pub attempt: String,
    pub websocket_url: String,
}

/// Timing and failure policy for the supervisor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CefTiming {
    pub poll_interval: Duration,
    pub confirm_timeout: Duration,
    pub retry_base: Duration,
    pub retry_max: Duration,
    pub failure_window: Duration,
    pub failures_before_safe_mode: usize,
}

impl Default for CefTiming {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(1),
            confirm_timeout: Duration::from_secs(10),
            retry_base: Duration::from_secs(1),
            retry_max: Duration::from_secs(30),
            failure_window: Duration::from_secs(60),
            failures_before_safe_mode: 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    WaitingForSteam,
    Attaching,
    Bootstrapping,
    Active,
    Backoff,
    SafeMode,
    Disabled,
}

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Self::WaitingForSteam => "waiting_for_steam",
            Self::Attaching => "attaching",
            Self::Bootstrapping => "bootstrapping",
            Self::Active => "active",
            Self::Backoff => "backoff",
            Self::SafeMode => "safe_mode",
            Self::Disabled => "disabled",
        }
    }
}

/// What the supervisor loop should do next.
#[derive(Debug)]
pub(crate) enum Decision {
    Discover,
    Inject(InjectionAttempt),
    Wait(Duration),
    Idle,
}

#[derive(Debug)]
pub(crate) struct CefMachine {
    instance: String,
    timing: CefTiming,
    phase: Phase,
    reason: Option<String>,
    attempt: Option<InjectionAttempt>,
    qam_ready: bool,
    failures: VecDeque<Instant>,
    next_retry_at: Option<Instant>,
    next_discovery_at: Instant,
    confirm_deadline: Option<Instant>,
    injection_in_flight: bool,
}

impl CefMachine {
    pub(crate) fn new(instance: String, timing: CefTiming, enabled: bool, now: Instant) -> Self {
        Self {
            instance,
            timing,
            phase: if enabled {
                Phase::WaitingForSteam
            } else {
                Phase::Disabled
            },
            reason: None,
            attempt: None,
            qam_ready: false,
            failures: VecDeque::new(),
            next_retry_at: None,
            next_discovery_at: now,
            confirm_deadline: None,
            injection_in_flight: false,
        }
    }

    pub(crate) fn status(&self, now: Instant) -> CefStatus {
        CefStatus {
            state: self.phase.as_str().to_owned(),
            reason: self.reason.clone(),
            attempt: self.attempt.as_ref().map(|a| a.attempt.clone()),
            context: self.attempt.as_ref().map(|a| a.context.clone()),
            qam_ready: self.qam_ready,
            consecutive_failures: self.failures.len(),
            retry_in_ms: self.next_retry_at.map(|retry| {
                u64::try_from(retry.saturating_duration_since(now).as_millis()).unwrap_or(u64::MAX)
            }),
        }
    }

    pub(crate) fn confirm(
        &mut self,
        confirmation: &CefConfirmation,
        _now: Instant,
    ) -> Result<(), CefError> {
        let Some(attempt) = &self.attempt else {
            return Err(CefError::StaleConfirmation);
        };
        if attempt.instance != confirmation.instance
            || attempt.context != confirmation.context
            || attempt.attempt != confirmation.attempt
            || !matches!(self.phase, Phase::Bootstrapping | Phase::Active)
        {
            return Err(CefError::StaleConfirmation);
        }
        self.phase = Phase::Active;
        self.qam_ready = confirmation.qam_ready;
        self.reason = None;
        self.confirm_deadline = None;
        Ok(())
    }

    pub(crate) fn enter_safe_mode(&mut self, reason: impl Into<String>) {
        self.phase = Phase::SafeMode;
        self.reason = Some(reason.into());
        self.attempt = None;
        self.qam_ready = false;
        self.confirm_deadline = None;
        self.injection_in_flight = false;
        self.next_retry_at = None;
    }

    pub(crate) fn reset_safe_mode(&mut self, now: Instant) -> bool {
        let was_safe = self.phase == Phase::SafeMode;
        if was_safe {
            self.phase = Phase::WaitingForSteam;
            self.reason = None;
            self.failures.clear();
            self.attempt = None;
            self.qam_ready = false;
            self.next_retry_at = None;
            self.next_discovery_at = now;
        }
        was_safe
    }

    pub(crate) fn report_failure(&mut self, reason: impl Into<String>, now: Instant) {
        self.record_failure(reason.into(), now);
    }

    pub(crate) fn on_discovery_error(&mut self, error: &str, now: Instant) {
        self.reason = Some(format!("Steam CEF debugger unavailable: {error}"));
        match self.phase {
            Phase::WaitingForSteam | Phase::Active | Phase::Bootstrapping => {
                self.next_discovery_at = now + self.timing.poll_interval;
            }
            Phase::Attaching | Phase::Backoff | Phase::SafeMode | Phase::Disabled => {}
        }
    }

    pub(crate) fn on_tabs(&mut self, tabs: &[CdpTabDescriptor], now: Instant) {
        if matches!(self.phase, Phase::SafeMode | Phase::Disabled) {
            return;
        }
        let candidate = tabs.iter().find(|tab| {
            is_gamepadui_tab(tab)
                && tab
                    .websocket_debugger_url
                    .as_deref()
                    .is_some_and(is_loopback_ws_url)
        });

        let Some(tab) = candidate else {
            if matches!(self.phase, Phase::Active | Phase::Bootstrapping) {
                self.phase = Phase::WaitingForSteam;
                self.reason = Some("GamepadUI context is gone".to_owned());
                self.attempt = None;
                self.qam_ready = false;
                self.confirm_deadline = None;
            } else if self.phase == Phase::WaitingForSteam {
                self.reason = None;
            }
            self.next_discovery_at = now + self.timing.poll_interval;
            return;
        };

        let Some(websocket_url) = tab.websocket_debugger_url.clone() else {
            return;
        };

        if let Some(attempt) = &self.attempt
            && attempt.context == tab.id
            && attempt.websocket_url == websocket_url
            && matches!(self.phase, Phase::Active | Phase::Bootstrapping)
        {
            self.next_discovery_at = now + self.timing.poll_interval;
            return;
        }

        self.attempt = Some(InjectionAttempt {
            instance: self.instance.clone(),
            context: tab.id.clone(),
            attempt: Uuid::new_v4().to_string(),
            websocket_url,
        });
        self.phase = Phase::Attaching;
        self.reason = None;
        self.qam_ready = false;
        self.confirm_deadline = None;
        self.injection_in_flight = false;
        self.next_discovery_at = now;
    }

    pub(crate) fn on_injection_result(&mut self, result: Result<(), CefError>, now: Instant) {
        self.injection_in_flight = false;
        match result {
            Ok(()) => {
                self.phase = Phase::Bootstrapping;
                self.reason = None;
                self.confirm_deadline = Some(now + self.timing.confirm_timeout);
                self.next_discovery_at = now + self.timing.poll_interval;
            }
            Err(err) => self.record_failure(format!("bootstrap injection failed: {err}"), now),
        }
    }

    pub(crate) fn next_decision(&mut self, now: Instant) -> Decision {
        if self.phase == Phase::Bootstrapping
            && self
                .confirm_deadline
                .is_some_and(|deadline| now >= deadline)
        {
            self.record_failure("frontend did not confirm the injection".to_owned(), now);
        }

        match self.phase {
            Phase::WaitingForSteam => {
                if now < self.next_discovery_at {
                    Decision::Wait(self.next_discovery_at.saturating_duration_since(now))
                } else {
                    Decision::Discover
                }
            }
            Phase::Attaching => {
                if self.injection_in_flight {
                    return Decision::Wait(self.timing.poll_interval);
                }
                let Some(attempt) = self.attempt.clone() else {
                    self.phase = Phase::WaitingForSteam;
                    self.next_discovery_at = now;
                    return Decision::Discover;
                };
                self.injection_in_flight = true;
                Decision::Inject(attempt)
            }
            Phase::Bootstrapping => {
                let deadline = self.confirm_deadline.unwrap_or(now);
                let wait = deadline.saturating_duration_since(now);
                if now >= self.next_discovery_at {
                    self.next_discovery_at = now + self.timing.poll_interval;
                    Decision::Discover
                } else {
                    Decision::Wait(wait.min(self.next_discovery_at.saturating_duration_since(now)))
                }
            }
            Phase::Active => {
                if now >= self.next_discovery_at {
                    Decision::Discover
                } else {
                    Decision::Wait(self.next_discovery_at.saturating_duration_since(now))
                }
            }
            Phase::Backoff => {
                let retry = self.next_retry_at.unwrap_or(now);
                if now >= retry {
                    self.phase = Phase::WaitingForSteam;
                    self.next_retry_at = None;
                    self.next_discovery_at = now;
                    Decision::Discover
                } else {
                    Decision::Wait(retry.saturating_duration_since(now))
                }
            }
            Phase::SafeMode | Phase::Disabled => Decision::Idle,
        }
    }

    fn record_failure(&mut self, reason: String, now: Instant) {
        self.failures.push_back(now);
        while self
            .failures
            .front()
            .is_some_and(|first| now.saturating_duration_since(*first) > self.timing.failure_window)
        {
            let _ = self.failures.pop_front();
        }

        self.attempt = None;
        self.qam_ready = false;
        self.confirm_deadline = None;
        self.injection_in_flight = false;
        self.reason = Some(reason);

        if self.failures.len() >= self.timing.failures_before_safe_mode.max(1) {
            self.enter_safe_mode("repeated bootstrap failures");
            return;
        }

        self.phase = Phase::Backoff;
        self.next_retry_at = Some(now + self.backoff_delay(self.failures.len()));
    }

    fn backoff_delay(&self, failures: usize) -> Duration {
        let exponent = u32::try_from(failures.saturating_sub(1).min(5)).unwrap_or(5);
        let scaled = self
            .timing
            .retry_base
            .saturating_mul(1_u32 << exponent)
            .min(self.timing.retry_max);
        // Deterministic jitter of up to 1/8 of the delay keeps retries from synchronizing without
        // adding an RNG dependency.
        let base_millis = u64::try_from(scaled.as_millis()).unwrap_or(u64::MAX);
        let span = (base_millis / 8).max(1);
        let offset = u64::from(exponent).wrapping_mul(2_654_435_761) % span;
        let millis = base_millis.saturating_sub(span / 2).saturating_add(offset);
        Duration::from_millis(millis.max(1)).min(self.timing.retry_max)
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{CefConfirmation, CefMachine, CefTiming, Decision, InjectionAttempt, Phase};
    use crate::{CdpTabDescriptor, CefError};

    fn tab(id: &str) -> CdpTabDescriptor {
        CdpTabDescriptor {
            id: id.to_owned(),
            title: "SharedJSContext".to_owned(),
            url: "https://steamloopback.host/index.html".to_owned(),
            websocket_debugger_url: Some(format!("ws://127.0.0.1:8080/devtools/page/{id}")),
        }
    }

    fn machine() -> CefMachine {
        CefMachine::new(
            "instance-a".to_owned(),
            CefTiming::default(),
            true,
            Instant::now(),
        )
    }

    fn drive_to_bootstrapping(machine: &mut CefMachine, now: Instant) -> InjectionAttempt {
        machine.on_tabs(&[tab("ctx-1")], now);
        match machine.next_decision(now) {
            Decision::Inject(attempt) => {
                machine.on_injection_result(Ok(()), now);
                attempt
            }
            other => panic!("expected injection, got {other:?}"),
        }
    }

    fn confirmation(attempt: &InjectionAttempt) -> CefConfirmation {
        CefConfirmation {
            instance: attempt.instance.clone(),
            context: attempt.context.clone(),
            attempt: attempt.attempt.clone(),
            qam_ready: false,
        }
    }

    #[test]
    fn confirmation_binds_instance_context_and_attempt() {
        let now = Instant::now();
        let mut machine = machine();
        let attempt = drive_to_bootstrapping(&mut machine, now);
        assert_eq!(machine.status(now).state, Phase::Bootstrapping.as_str());

        for mutated in [
            CefConfirmation {
                instance: "other".to_owned(),
                ..confirmation(&attempt)
            },
            CefConfirmation {
                context: "ctx-2".to_owned(),
                ..confirmation(&attempt)
            },
            CefConfirmation {
                attempt: "other".to_owned(),
                ..confirmation(&attempt)
            },
        ] {
            assert!(matches!(
                machine.confirm(&mutated, now),
                Err(CefError::StaleConfirmation)
            ));
        }

        assert!(machine.confirm(&confirmation(&attempt), now).is_ok());
        assert_eq!(machine.status(now).state, Phase::Active.as_str());
        assert!(!machine.status(now).qam_ready);

        let mut qam = confirmation(&attempt);
        qam.qam_ready = true;
        assert!(machine.confirm(&qam, now).is_ok());
        assert!(machine.status(now).qam_ready);
    }

    #[test]
    fn context_change_starts_a_new_attempt_and_rejects_the_old_confirmation() {
        let now = Instant::now();
        let mut machine = machine();
        let first = drive_to_bootstrapping(&mut machine, now);
        machine
            .confirm(&confirmation(&first), now)
            .expect("confirm");

        machine.on_tabs(&[tab("ctx-2")], now + Duration::from_millis(1));
        let second = match machine.next_decision(now + Duration::from_millis(1)) {
            Decision::Inject(attempt) => attempt,
            other => panic!("expected a new injection, got {other:?}"),
        };
        assert_ne!(first.attempt, second.attempt);
        assert_eq!(second.context, "ctx-2");
        assert!(matches!(
            machine.confirm(&confirmation(&first), now + Duration::from_millis(1)),
            Err(CefError::StaleConfirmation)
        ));
    }

    #[test]
    fn repeated_failures_enter_safe_mode_within_the_window() {
        let now = Instant::now();
        let mut machine = machine();
        for step in 0..3 {
            machine.report_failure("inject failed", now + Duration::from_secs(step));
            if step < 2 {
                assert_eq!(machine.status(now).state, Phase::Backoff.as_str());
            }
        }
        assert_eq!(machine.status(now).state, Phase::SafeMode.as_str());
        assert!(machine.reset_safe_mode(now + Duration::from_secs(4)));
        assert_eq!(machine.status(now).state, Phase::WaitingForSteam.as_str());
        assert!(!machine.reset_safe_mode(now));
    }

    #[test]
    fn failures_outside_the_window_do_not_trip_safe_mode() {
        let now = Instant::now();
        let mut machine = machine();
        machine.report_failure("first", now);
        machine.report_failure("second", now + Duration::from_secs(61));
        machine.report_failure("third", now + Duration::from_secs(62));
        assert_eq!(
            machine.status(now + Duration::from_secs(62)).state,
            Phase::Backoff.as_str()
        );
        assert_eq!(
            machine
                .status(now + Duration::from_secs(62))
                .consecutive_failures,
            2
        );
    }

    #[test]
    fn backoff_is_bounded_and_eventually_retries() {
        let now = Instant::now();
        let timing = CefTiming {
            failures_before_safe_mode: 100,
            failure_window: Duration::from_secs(3600),
            ..CefTiming::default()
        };
        let mut machine = CefMachine::new("instance-a".to_owned(), timing.clone(), true, now);
        machine.report_failure("inject failed", now);
        let mut delay = match machine.next_decision(now) {
            Decision::Wait(delay) => delay,
            other => panic!("expected backoff, got {other:?}"),
        };
        assert!(delay >= Duration::from_millis(800));
        assert!(delay <= Duration::from_secs(2));

        for _ in 0..8 {
            machine.report_failure("inject failed", now + Duration::from_secs(1));
        }
        delay = match machine.next_decision(now + Duration::from_secs(1)) {
            Decision::Wait(delay) => delay,
            other => panic!("expected backoff, got {other:?}"),
        };
        assert!(delay <= timing.retry_max);
    }

    #[test]
    fn disabled_supervisor_stays_idle() {
        let now = Instant::now();
        let mut machine =
            CefMachine::new("instance-a".to_owned(), CefTiming::default(), false, now);
        machine.on_tabs(&[tab("ctx-1")], now);
        assert!(matches!(machine.next_decision(now), Decision::Idle));
        assert_eq!(machine.status(now).state, Phase::Disabled.as_str());
    }

    #[test]
    fn only_one_injection_is_in_flight_at_a_time() {
        let now = Instant::now();
        let mut machine = machine();
        machine.on_tabs(&[tab("ctx-1")], now);
        assert!(matches!(machine.next_decision(now), Decision::Inject(_)));
        assert!(matches!(machine.next_decision(now), Decision::Wait(_)));
    }

    #[test]
    fn transient_discovery_error_does_not_tear_down_an_active_context() {
        let now = Instant::now();
        let mut machine = machine();
        let attempt = drive_to_bootstrapping(&mut machine, now);
        machine
            .confirm(&confirmation(&attempt), now)
            .expect("confirm");
        machine.on_discovery_error("connection refused", now);
        assert_eq!(machine.status(now).state, Phase::Active.as_str());
        assert!(machine.status(now).reason.is_some());
    }

    #[test]
    fn target_loss_returns_to_waiting_and_clears_the_attempt() {
        let now = Instant::now();
        let mut machine = machine();
        let attempt = drive_to_bootstrapping(&mut machine, now);
        machine
            .confirm(&confirmation(&attempt), now)
            .expect("confirm");
        machine.on_tabs(&[], now + Duration::from_millis(1));
        let status = machine.status(now + Duration::from_millis(1));
        assert_eq!(status.state, Phase::WaitingForSteam.as_str());
        assert!(status.attempt.is_none());
        assert!(status.context.is_none());
    }

    #[test]
    fn unconfirmed_injection_times_out_into_backoff() {
        let now = Instant::now();
        let mut machine = machine();
        let attempt = drive_to_bootstrapping(&mut machine, now);
        let after_timeout = now + CefTiming::default().confirm_timeout + Duration::from_millis(1);
        assert!(matches!(
            machine.next_decision(after_timeout),
            Decision::Wait(_)
        ));
        assert_eq!(machine.status(after_timeout).state, Phase::Backoff.as_str());
        assert!(matches!(
            machine.confirm(&confirmation(&attempt), after_timeout),
            Err(CefError::StaleConfirmation)
        ));
    }

    #[test]
    fn non_loopback_websocket_targets_are_not_injected() {
        let now = Instant::now();
        let mut machine = machine();
        let mut remote = tab("ctx-remote");
        remote.websocket_debugger_url = Some("ws://10.0.0.5:8080/devtools/page/1".to_owned());
        machine.on_tabs(&[remote], now);
        assert_eq!(machine.status(now).state, Phase::WaitingForSteam.as_str());
    }
}
