//! Single-flight supervisor that discovers Steam's `GamepadUI` context, injects the frontend
//! bootstrap and accepts only a confirmation bound to that exact daemon instance and attempt.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;

use crate::driver::CefDriver;
use crate::state::{CefConfirmation, CefMachine, CefStatus, CefTiming, Decision, InjectionAttempt};
use crate::{CefError, is_loopback_http_url};

/// Supervisor configuration owned by the daemon.
#[derive(Debug, Clone)]
pub struct CefSupervisorConfig {
    /// Ephemeral daemon instance id embedded in every injection attempt.
    pub daemon_instance: String,
    /// Loopback base URL that serves the frontend bundle.
    pub frontend_base_url: String,
    /// Poll, confirmation, backoff and safe-mode timings.
    pub timing: CefTiming,
    /// Operator switch; when false the supervisor stays `disabled` and touches nothing.
    pub enabled: bool,
}

/// Shared supervisor state plus a watch channel for authenticated status consumers.
#[derive(Debug)]
pub struct CefSupervisor {
    machine: Mutex<CefMachine>,
    status_tx: watch::Sender<CefStatus>,
    changed: Notify,
    frontend_base_url: String,
}

impl CefSupervisor {
    /// Creates a supervisor in `waiting_for_steam`.
    ///
    /// # Errors
    /// Returns [`CefError::InvalidConfig`] for an empty instance id or a non-loopback frontend URL.
    pub fn new(config: CefSupervisorConfig) -> Result<Self, CefError> {
        if config.daemon_instance.is_empty() {
            return Err(CefError::InvalidConfig(
                "daemon instance id must not be empty".to_owned(),
            ));
        }
        if !is_loopback_http_url(&config.frontend_base_url) {
            return Err(CefError::InvalidConfig(format!(
                "frontend base URL must be loopback http, got {:?}",
                config.frontend_base_url
            )));
        }
        let now = Instant::now();
        let machine = CefMachine::new(config.daemon_instance, config.timing, config.enabled, now);
        let (status_tx, _) = watch::channel(machine.status(now));
        Ok(Self {
            machine: Mutex::new(machine),
            status_tx,
            changed: Notify::new(),
            frontend_base_url: config.frontend_base_url.trim_end_matches('/').to_owned(),
        })
    }

    /// Returns the current status. This never blocks on Steam or the network.
    #[must_use]
    pub fn status(&self) -> CefStatus {
        self.machine().status(Instant::now())
    }

    /// Subscribes to status changes, including safe-mode transitions the daemon persists.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<CefStatus> {
        self.status_tx.subscribe()
    }

    /// Accepts a frontend confirmation only for the current daemon instance, context and attempt.
    ///
    /// # Errors
    /// Returns [`CefError::StaleConfirmation`] for any mismatched or expired attempt.
    pub fn confirm(&self, confirmation: &CefConfirmation) -> Result<(), CefError> {
        {
            let mut machine = self.machine();
            machine.confirm(confirmation, Instant::now())?;
        }
        self.publish();
        Ok(())
    }

    /// Clears safe mode and resumes discovery. Returns whether safe mode was active.
    pub fn reset_safe_mode(&self) -> bool {
        let changed = {
            let mut machine = self.machine();
            machine.reset_safe_mode(Instant::now())
        };
        if changed {
            self.publish();
        }
        changed
    }

    /// Enters safe mode with a reason; used for a persisted marker at startup.
    pub fn enter_safe_mode(&self, reason: impl Into<String>) {
        self.machine().enter_safe_mode(reason);
        self.publish();
    }

    /// Records a failure reported by the injected frontend and applies backoff or safe mode.
    pub fn report_failure(&self, reason: impl Into<String>) {
        self.machine().report_failure(reason, Instant::now());
        self.publish();
    }

    pub(crate) fn on_tabs(&self, tabs: &[crate::CdpTabDescriptor]) {
        {
            let mut machine = self.machine();
            machine.on_tabs(tabs, Instant::now());
        }
        self.publish();
    }

    pub(crate) fn on_discovery_error(&self, error: &str) {
        {
            let mut machine = self.machine();
            machine.on_discovery_error(error, Instant::now());
        }
        self.publish();
    }

    pub(crate) fn on_injection_result(&self, result: Result<(), CefError>) {
        {
            let mut machine = self.machine();
            machine.on_injection_result(result, Instant::now());
        }
        self.publish();
    }

    pub(crate) fn next_decision(&self) -> Decision {
        let now = Instant::now();
        let mut machine = self.machine();
        let before = machine.status(now);
        let decision = machine.next_decision(now);
        let after = machine.status(now);
        drop(machine);
        if before != after {
            self.publish();
        }
        decision
    }

    fn bootstrap_script(&self, attempt: &InjectionAttempt) -> String {
        let src = format!(
            "{}/frontend/index.js?cef_instance={}&cef_context={}&cef_attempt={}",
            self.frontend_base_url, attempt.instance, attempt.context, attempt.attempt
        );
        let src = serde_json::to_string(&src).unwrap_or_else(|_| "''".to_owned());
        let info = serde_json::json!({
            "instance": attempt.instance,
            "context": attempt.context,
            "attempt": attempt.attempt,
        })
        .to_string();
        format!(
            r"(() => {{
  const info = {info};
  window.__zerodeckCef = info;
  if (window.__zerodeckLoaderLoaded) {{
    window.dispatchEvent(new CustomEvent('zerodeck:cef-attempt', {{ detail: info }}));
    return;
  }}
  window.__zerodeckLoaderLoaded = true;
  const script = document.createElement('script');
  script.type = 'module';
  script.dataset.zerodeckAttempt = info.attempt;
  script.src = {src};
  document.documentElement.appendChild(script);
}})();"
        )
    }

    fn machine(&self) -> MutexGuard<'_, CefMachine> {
        self.machine.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn publish(&self) {
        let _ = self.status_tx.send(self.status());
        self.changed.notify_waiters();
    }
}

/// Handle for stopping the supervisor task.
#[derive(Debug)]
pub struct CefHandle {
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl CefHandle {
    /// Requests shutdown and waits for the loop to finish.
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(true);
        let _ = self.task.await;
    }
}

/// Starts the supervisor loop. Only one discovery/injection runs at a time.
pub fn spawn_supervisor(driver: Arc<dyn CefDriver>, supervisor: Arc<CefSupervisor>) -> CefHandle {
    let (shutdown, mut shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        loop {
            match supervisor.next_decision() {
                Decision::Discover => {
                    let result = tokio::select! {
                        () = wait_shutdown(&mut shutdown_rx) => break,
                        result = driver.list_targets() => result,
                    };
                    match result {
                        Ok(tabs) => supervisor.on_tabs(&tabs),
                        Err(err) => supervisor.on_discovery_error(&err.to_string()),
                    }
                }
                Decision::Inject(attempt) => {
                    let script = supervisor.bootstrap_script(&attempt);
                    let result = tokio::select! {
                        () = wait_shutdown(&mut shutdown_rx) => break,
                        result = driver.inject(&attempt.websocket_url, &script) => result,
                    };
                    supervisor.on_injection_result(result);
                }
                Decision::Wait(delay) => {
                    tokio::select! {
                        () = wait_shutdown(&mut shutdown_rx) => break,
                        () = tokio::time::sleep(delay) => {}
                        () = supervisor.changed.notified() => {}
                    }
                }
                Decision::Idle => {
                    tokio::select! {
                        () = wait_shutdown(&mut shutdown_rx) => break,
                        () = supervisor.changed.notified() => {}
                    }
                }
            }
        }
    });
    CefHandle { shutdown, task }
}

async fn wait_shutdown(shutdown: &mut watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow() {
            return;
        }
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::{CefSupervisor, CefSupervisorConfig, spawn_supervisor};
    use crate::driver::{CefDriver, DriverFuture};
    use crate::state::{CefConfirmation, CefStatus, CefTiming};
    use crate::{CdpTabDescriptor, CefError};

    fn tab(id: &str) -> CdpTabDescriptor {
        CdpTabDescriptor {
            id: id.to_owned(),
            title: "SharedJSContext".to_owned(),
            url: "https://steamloopback.host/index.html".to_owned(),
            websocket_debugger_url: Some(format!("ws://127.0.0.1:8080/devtools/page/{id}")),
        }
    }

    #[derive(Default)]
    struct FakeDriver {
        targets: Mutex<Vec<CdpTabDescriptor>>,
        injections: Mutex<Vec<String>>,
        fail: AtomicBool,
    }

    impl FakeDriver {
        fn set_targets(&self, targets: Vec<CdpTabDescriptor>) {
            *self.targets.lock().expect("targets") = targets;
        }

        fn injection_count(&self) -> usize {
            self.injections.lock().expect("injections").len()
        }
    }

    impl CefDriver for FakeDriver {
        fn list_targets(&self) -> DriverFuture<'_, Vec<CdpTabDescriptor>> {
            let targets = self.targets.lock().expect("targets").clone();
            Box::pin(async move { Ok(targets) })
        }

        fn inject(&self, websocket_url: &str, script: &str) -> DriverFuture<'_, ()> {
            self.injections
                .lock()
                .expect("injections")
                .push(websocket_url.to_owned());
            assert!(script.contains("__zerodeckCef"));
            let fail = self.fail.load(Ordering::SeqCst);
            Box::pin(async move {
                if fail {
                    Err(CefError::CdpEvaluationFailed("injected failure".to_owned()))
                } else {
                    Ok(())
                }
            })
        }
    }

    fn supervisor(timing: CefTiming) -> Arc<CefSupervisor> {
        Arc::new(
            CefSupervisor::new(CefSupervisorConfig {
                daemon_instance: "daemon-a".to_owned(),
                frontend_base_url: "http://127.0.0.1:1337".to_owned(),
                timing,
                enabled: true,
            })
            .expect("supervisor"),
        )
    }

    fn fast_timing() -> CefTiming {
        CefTiming {
            poll_interval: Duration::from_millis(5),
            confirm_timeout: Duration::from_secs(1),
            retry_base: Duration::from_millis(5),
            retry_max: Duration::from_millis(20),
            failure_window: Duration::from_secs(1),
            failures_before_safe_mode: 3,
        }
    }

    async fn wait_for(
        supervisor: &CefSupervisor,
        predicate: impl Fn(&CefStatus) -> bool,
    ) -> CefStatus {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let status = supervisor.status();
                if predicate(&status) {
                    return status;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("status within timeout")
    }

    fn confirmation(status: &CefStatus, qam_ready: bool) -> CefConfirmation {
        CefConfirmation {
            instance: "daemon-a".to_owned(),
            context: status.context.clone().expect("context"),
            attempt: status.attempt.clone().expect("attempt"),
            qam_ready,
        }
    }

    #[tokio::test]
    async fn runner_injects_once_confirms_and_reinjects_on_context_change() {
        let driver = Arc::new(FakeDriver::default());
        driver.set_targets(vec![tab("1")]);
        let supervisor = supervisor(fast_timing());
        let handle = spawn_supervisor(driver.clone(), Arc::clone(&supervisor));

        let first = wait_for(&supervisor, |s| s.state == "bootstrapping").await;
        assert_eq!(driver.injection_count(), 1);
        assert_eq!(first.context.as_deref(), Some("1"));
        supervisor
            .confirm(&confirmation(&first, true))
            .expect("confirm");
        assert_eq!(supervisor.status().state, "active");
        assert!(supervisor.status().qam_ready);

        driver.set_targets(vec![tab("2")]);
        let second = wait_for(&supervisor, |s| {
            s.state == "bootstrapping" && s.context.as_deref() == Some("2")
        })
        .await;
        assert_ne!(first.attempt, second.attempt);
        assert_eq!(driver.injection_count(), 2);
        assert!(matches!(
            supervisor.confirm(&confirmation(&first, false)),
            Err(CefError::StaleConfirmation)
        ));

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn failed_injections_reach_safe_mode_and_reset_resumes() {
        let driver = Arc::new(FakeDriver::default());
        driver.set_targets(vec![tab("1")]);
        driver.fail.store(true, Ordering::SeqCst);
        let mut timing = fast_timing();
        timing.failures_before_safe_mode = 2;
        let supervisor = supervisor(timing);
        let handle = spawn_supervisor(driver.clone(), Arc::clone(&supervisor));

        wait_for(&supervisor, |s| s.state == "safe_mode").await;
        assert!(supervisor.status().reason.is_some());
        assert!(supervisor.reset_safe_mode());
        driver.fail.store(false, Ordering::SeqCst);
        wait_for(&supervisor, |s| s.state == "bootstrapping").await;

        handle.shutdown().await;
    }

    #[test]
    fn bootstrap_script_reuses_an_existing_loader_and_carries_the_attempt() {
        let supervisor = supervisor(CefTiming::default());
        let attempt = super::InjectionAttempt {
            instance: "daemon-a".to_owned(),
            context: "42".to_owned(),
            attempt: "attempt-1".to_owned(),
            websocket_url: "ws://127.0.0.1:8080/devtools/page/42".to_owned(),
        };
        let script = supervisor.bootstrap_script(&attempt);
        assert!(script.contains("__zerodeckLoaderLoaded"));
        assert!(script.contains("zerodeck:cef-attempt"));
        assert!(script.contains("attempt-1"));
        assert!(script.contains("http://127.0.0.1:1337/frontend/index.js"));
    }

    #[test]
    fn rejects_non_loopback_frontend_or_empty_instance() {
        let base = CefSupervisorConfig {
            daemon_instance: "daemon-a".to_owned(),
            frontend_base_url: "http://127.0.0.1:1337".to_owned(),
            timing: CefTiming::default(),
            enabled: true,
        };
        assert!(CefSupervisor::new(base.clone()).is_ok());
        assert!(
            CefSupervisor::new(CefSupervisorConfig {
                frontend_base_url: "http://10.0.0.5:1337".to_owned(),
                ..base.clone()
            })
            .is_err()
        );
        assert!(
            CefSupervisor::new(CefSupervisorConfig {
                daemon_instance: String::new(),
                ..base
            })
            .is_err()
        );
    }
}
