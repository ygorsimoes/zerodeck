use std::collections::{HashMap, VecDeque};
use std::path::Path;
#[cfg(any(not(unix), test))]
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinSet;
use tracing::{debug, error, info, warn};
use zerodeck_platform::{PlatformPaths, PrivilegeTarget, lock_unpoisoned};
use zerodeck_protocol::{PluginIpcFromWorker, PluginIpcToWorker, WorkerStartup};

#[cfg(unix)]
use std::os::unix::io::{AsRawFd, BorrowedFd};
#[cfg(unix)]
use zerodeck_platform::{
    HostUserIdentity, check_root_only_tree, check_secure_ancestors, current_process_identity,
    ensure_owned_dir,
};

use crate::{BoxHostFuture, HostError, PluginRuntime, PluginSpawnSpec, PythonHostConfig};

/// Maximum newline-delimited JSON line accepted from a worker (4 MiB, matching `zerodeck-py-host`).
const MAX_IPC_LINE_BYTES: usize = 4 * 1024 * 1024;
const MAX_LOG_LINE_BYTES: usize = 16 * 1024;
const STARTUP_OBSERVATION: Duration = Duration::from_secs(2);
const OBSERVATION_PROBE_TIMEOUT: Duration = Duration::from_secs(1);
const INITIAL_PROBE: u8 = 0;
const OBSERVATION_PROBE: u8 = 1;

/// Lifecycle state of a supervised plugin worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PluginState {
    Stopped,
    Starting,
    Running,
    BackingOff,
    Failed,
    Stopping,
}

#[derive(Debug, Clone)]
struct WorkerStatus {
    state: PluginState,
    confirmed: bool,
    ever_confirmed: bool,
    initial_failure: Option<String>,
    failure: Option<String>,
}

impl WorkerStatus {
    fn new(state: PluginState) -> Self {
        Self {
            state,
            confirmed: false,
            ever_confirmed: false,
            initial_failure: None,
            failure: None,
        }
    }
}

/// Asynchronous event emitted by a plugin worker via `decky.emit(...)`.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginEvent {
    pub plugin: String,
    pub event: String,
    pub args: Vec<Value>,
}

/// Tuning parameters for [`PluginSupervisor`] and worker lifecycle management.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorConfig {
    pub python_host: PythonHostConfig,
    pub startup_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub crash_window: Duration,
    pub max_crashes_in_window: usize,
    pub stability_window: Duration,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            python_host: PythonHostConfig::default(),
            startup_timeout: Duration::from_secs(15),
            shutdown_timeout: Duration::from_secs(5),
            crash_window: Duration::from_secs(60),
            max_crashes_in_window: 5,
            stability_window: Duration::from_secs(30),
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
        }
    }
}

/// Supervises out-of-process plugin workers, IPC channels, restarts, and shutdowns.
#[derive(Debug)]
pub struct PluginSupervisor {
    paths: PlatformPaths,
    config: SupervisorConfig,
    #[cfg(unix)]
    host_user: HostUserIdentity,
    #[cfg(not(unix))]
    fallback_home: PathBuf,
    events_tx: broadcast::Sender<PluginEvent>,
    plugins: Mutex<HashMap<String, Arc<ManagedPlugin>>>,
    lifecycle_lock: tokio::sync::Mutex<()>,
    shutting_down: Arc<AtomicBool>,
    allow_unsafe_root_plugins: Arc<AtomicBool>,
}

impl PluginSupervisor {
    /// Creates a new [`PluginSupervisor`] and resolves the unprivileged host identity.
    pub fn new(paths: PlatformPaths, config: SupervisorConfig) -> Result<Self, HostError> {
        #[cfg(unix)]
        let host_user = HostUserIdentity::resolve(&paths)?;
        #[cfg(not(unix))]
        let fallback_home = paths.user_home().to_path_buf();
        let (events_tx, _) = broadcast::channel(256);
        Ok(Self {
            paths,
            config,
            #[cfg(unix)]
            host_user,
            #[cfg(not(unix))]
            fallback_home,
            events_tx,
            plugins: Mutex::new(HashMap::new()),
            lifecycle_lock: tokio::sync::Mutex::new(()),
            shutting_down: Arc::new(AtomicBool::new(false)),
            allow_unsafe_root_plugins: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Subscribes to asynchronous [`PluginEvent`] broadcasts from all workers.
    #[must_use]
    pub fn subscribe_events(&self) -> broadcast::Receiver<PluginEvent> {
        self.events_tx.subscribe()
    }

    /// Sets the daemon's explicit development override for privileged worker launches.
    pub fn set_allow_unsafe_root_plugins(&self, allow: bool) {
        self.allow_unsafe_root_plugins
            .store(allow, Ordering::Relaxed);
    }

    /// Returns the resolved unprivileged host username (used when building plugin envs).
    #[must_use]
    pub fn host_username(&self) -> &str {
        #[cfg(unix)]
        {
            &self.host_user.username
        }
        #[cfg(not(unix))]
        {
            "deck"
        }
    }

    /// Returns the resolved unprivileged host home directory (`DECKY_USER_HOME`).
    #[must_use]
    pub fn host_home_dir(&self) -> &Path {
        #[cfg(unix)]
        {
            &self.host_user.home_dir
        }
        #[cfg(not(unix))]
        {
            &self.fallback_home
        }
    }

    /// Spawns and begins supervising an active plugin worker, replacing any prior worker with the
    /// same manifest name.
    pub async fn spawn_plugin(
        &self,
        spec: PluginSpawnSpec,
    ) -> Result<Arc<ManagedPlugin>, HostError> {
        let _guard = self.lifecycle_lock.lock().await;
        self.spawn_plugin_locked(spec).await
    }

    async fn spawn_plugin_locked(
        &self,
        spec: PluginSpawnSpec,
    ) -> Result<Arc<ManagedPlugin>, HostError> {
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(HostError::WorkerStopped);
        }

        let existing = {
            let mut guard = lock_unpoisoned(&self.plugins);
            guard.remove(&spec.manifest.name)
        };
        if let Some(old) = existing {
            let _ = old.shutdown(false).await;
        }

        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(HostError::WorkerStopped);
        }

        let managed = ManagedPlugin::start(
            spec.clone(),
            self.paths.clone(),
            self.config.clone(),
            #[cfg(unix)]
            self.host_user.clone(),
            self.events_tx.clone(),
            Arc::clone(&self.allow_unsafe_root_plugins),
            Arc::clone(&self.shutting_down),
        )
        .await?;

        let mut guard = lock_unpoisoned(&self.plugins);
        guard.insert(spec.manifest.name, Arc::clone(&managed));
        Ok(managed)
    }

    /// Invokes a method on a supervised plugin worker by `manifest.name`.
    pub async fn call_method(
        &self,
        plugin_name: &str,
        method: &str,
        args: Value,
        legacy: bool,
    ) -> Result<Value, HostError> {
        let managed = {
            let guard = lock_unpoisoned(&self.plugins);
            guard.get(plugin_name).cloned()
        }
        .ok_or_else(|| HostError::PluginNotFound(plugin_name.to_owned()))?;

        managed.call_method(method, args, legacy).await
    }

    /// Returns the current [`PluginState`] for a supervised plugin if registered.
    #[must_use]
    pub fn plugin_state(&self, plugin_name: &str) -> Option<PluginState> {
        self.plugin_status(plugin_name).map(|(state, _)| state)
    }

    /// Returns the current state and startup failure, if any, from one worker snapshot.
    #[must_use]
    pub fn plugin_status(&self, plugin_name: &str) -> Option<(PluginState, Option<String>)> {
        let managed = lock_unpoisoned(&self.plugins).get(plugin_name).cloned()?;
        let status = managed.state_rx.borrow();
        Some((status.state, status.failure.clone()))
    }

    /// Gracefully stops a single supervised plugin (running `_uninstall` when `uninstall` is true).
    pub async fn shutdown_plugin(
        &self,
        plugin_name: &str,
        uninstall: bool,
    ) -> Result<(), HostError> {
        let _guard = self.lifecycle_lock.lock().await;
        let managed = {
            let mut guard = lock_unpoisoned(&self.plugins);
            guard.remove(plugin_name)
        }
        .ok_or_else(|| HostError::PluginNotFound(plugin_name.to_owned()))?;
        managed.shutdown(uninstall).await
    }

    /// Restarts a supervised plugin cleanly.
    pub async fn reload_plugin(&self, plugin_name: &str) -> Result<Arc<ManagedPlugin>, HostError> {
        let _guard = self.lifecycle_lock.lock().await;
        let spec = {
            let guard = lock_unpoisoned(&self.plugins);
            guard
                .get(plugin_name)
                .map(|m| m.spec.clone())
                .ok_or_else(|| HostError::PluginNotFound(plugin_name.to_owned()))?
        };
        self.spawn_plugin_locked(spec).await
    }

    /// Gracefully shuts down all supervised plugin workers concurrently.
    pub async fn shutdown_all(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        let _guard = self.lifecycle_lock.lock().await;
        let all: Vec<Arc<ManagedPlugin>> = {
            let mut guard = lock_unpoisoned(&self.plugins);
            guard.drain().map(|(_, v)| v).collect()
        };

        let mut set = JoinSet::new();
        for plugin in all {
            set.spawn(async move {
                if let Err(err) = plugin.shutdown(false).await {
                    warn!(plugin = %plugin.spec.manifest.name, error = %err, "error shutting down plugin");
                }
            });
        }
        while set.join_next().await.is_some() {}
    }
}

#[derive(Debug, Default)]
struct PendingCalls {
    closed: bool,
    waiters: HashMap<String, oneshot::Sender<Result<Value, HostError>>>,
}

type SharedPendingCalls = Arc<Mutex<PendingCalls>>;

#[derive(Debug)]
struct ActiveWorker {
    write_tx: mpsc::Sender<String>,
    pending: SharedPendingCalls,
}

enum SupervisorCmd {
    Shutdown {
        uninstall: bool,
        done: oneshot::Sender<Result<(), HostError>>,
    },
}

/// Handle to one supervised plugin worker process.
#[derive(Debug)]
pub struct ManagedPlugin {
    spec: PluginSpawnSpec,
    state_rx: watch::Receiver<WorkerStatus>,
    active: Arc<Mutex<Option<ActiveWorker>>>,
    cmd_tx: mpsc::Sender<SupervisorCmd>,
    startup_timeout: Duration,
}

impl ManagedPlugin {
    async fn start(
        spec: PluginSpawnSpec,
        paths: PlatformPaths,
        config: SupervisorConfig,
        #[cfg(unix)] host_user: HostUserIdentity,
        events_tx: broadcast::Sender<PluginEvent>,
        allow_unsafe_root_plugins: Arc<AtomicBool>,
        shutting_down: Arc<AtomicBool>,
    ) -> Result<Arc<Self>, HostError> {
        let (state_tx, state_rx) = watch::channel(WorkerStatus::new(PluginState::Starting));
        let active = Arc::new(Mutex::new(None));
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        let (ready_tx, ready_rx) = oneshot::channel();

        let managed = Arc::new(Self {
            spec: spec.clone(),
            state_rx,
            active: Arc::clone(&active),
            cmd_tx,
            startup_timeout: config.startup_timeout,
        });

        let ctx = SupervisorTaskCtx {
            spec,
            paths,
            config,
            #[cfg(unix)]
            host_user,
            events_tx,
            state_tx,
            active,
            cmd_rx,
            allow_unsafe_root_plugins,
            shutting_down,
        };
        tokio::spawn(run_supervisor_loop(ctx, ready_tx));

        ready_rx.await.map_err(|_| HostError::WorkerStopped)??;
        Ok(managed)
    }

    /// Current lifecycle state of this plugin worker.
    #[must_use]
    pub fn state(&self) -> PluginState {
        self.state_rx.borrow().state
    }

    /// Waits for this generation to finish startup and its observation window.
    pub async fn wait_ready(
        &self,
        expected_generation: &str,
        timeout: Duration,
    ) -> Result<(), HostError> {
        if expected_generation != self.spec.generation {
            return Err(HostError::GenerationMismatch);
        }
        let mut rx = self.state_rx.clone();
        tokio::time::timeout(timeout, async {
            loop {
                {
                    let current = rx.borrow_and_update();
                    if let Some(reason) = &current.initial_failure {
                        return Err(HostError::StartupFailed(reason.clone()));
                    }
                    if current.confirmed {
                        return Ok(());
                    }
                    if let Some(reason) = &current.failure {
                        return Err(HostError::StartupFailed(reason.clone()));
                    }
                    if !matches!(current.state, PluginState::Starting | PluginState::Running) {
                        return Err(HostError::WorkerStopped);
                    }
                }
                rx.changed().await.map_err(|_| HostError::WorkerStopped)?;
            }
        })
        .await
        .map_err(|_| HostError::StartupTimeout)?
    }

    async fn wait_for_active_worker(&self) -> Result<ActiveWorkerHandle, HostError> {
        let mut rx = self.state_rx.clone();
        let deadline = tokio::time::Instant::now() + self.startup_timeout;
        loop {
            let current = rx.borrow_and_update().state;
            match current {
                PluginState::Running => {
                    let guard = lock_unpoisoned(&self.active);
                    if let Some(worker) = guard.as_ref() {
                        return Ok(ActiveWorkerHandle {
                            write_tx: worker.write_tx.clone(),
                            pending: Arc::clone(&worker.pending),
                        });
                    }
                    return Err(HostError::WorkerStopped);
                }
                PluginState::Starting => {
                    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                    if remaining.is_zero() {
                        return Err(HostError::StartupTimeout);
                    }
                    match tokio::time::timeout(remaining, rx.changed()).await {
                        Ok(Ok(())) => {}
                        Ok(Err(_)) => return Err(HostError::WorkerStopped),
                        Err(_) => return Err(HostError::StartupTimeout),
                    }
                }
                PluginState::Failed => {
                    return Err(HostError::PluginFailed(self.spec.manifest.name.clone()));
                }
                PluginState::BackingOff | PluginState::Stopped | PluginState::Stopping => {
                    return Err(HostError::WorkerStopped);
                }
            }
        }
    }
}

struct ActiveWorkerHandle {
    write_tx: mpsc::Sender<String>,
    pending: SharedPendingCalls,
}

impl PluginRuntime for ManagedPlugin {
    fn call_method<'a>(
        &'a self,
        method: &'a str,
        args: Value,
        legacy: bool,
    ) -> BoxHostFuture<'a, Value> {
        Box::pin(async move {
            if method.is_empty() || method.starts_with('_') {
                return Err(HostError::InvalidMethodName(method.to_owned()));
            }

            let worker = self.wait_for_active_worker().await?;
            let call = PluginIpcToWorker::new_call(method, args, legacy);
            let call_id = match &call {
                PluginIpcToWorker::Call { id, .. } => id.clone(),
                PluginIpcToWorker::Control { .. } | PluginIpcToWorker::StartupProbe { .. } => {
                    unreachable!()
                }
            };
            let encoded =
                serde_json::to_string(&call).map_err(zerodeck_protocol::ProtocolError::from)?;

            let (tx, rx) = oneshot::channel();
            {
                let mut pending = lock_unpoisoned(&worker.pending);
                if pending.closed {
                    return Err(HostError::WorkerStopped);
                }
                pending.waiters.insert(call_id.clone(), tx);
            }

            if worker.write_tx.send(encoded).await.is_err() {
                let mut pending = lock_unpoisoned(&worker.pending);
                pending.waiters.remove(&call_id);
                return Err(HostError::WorkerStopped);
            }

            match rx.await {
                Ok(res) => res,
                Err(_) => Err(HostError::WorkerStopped),
            }
        })
    }

    fn shutdown(&self, uninstall: bool) -> BoxHostFuture<'_, ()> {
        Box::pin(async move {
            let (done_tx, done_rx) = oneshot::channel();
            if self
                .cmd_tx
                .send(SupervisorCmd::Shutdown {
                    uninstall,
                    done: done_tx,
                })
                .await
                .is_err()
            {
                return Ok(());
            }
            done_rx.await.unwrap_or(Ok(()))
        })
    }
}

struct SupervisorTaskCtx {
    spec: PluginSpawnSpec,
    paths: PlatformPaths,
    config: SupervisorConfig,
    #[cfg(unix)]
    host_user: HostUserIdentity,
    events_tx: broadcast::Sender<PluginEvent>,
    state_tx: watch::Sender<WorkerStatus>,
    active: Arc<Mutex<Option<ActiveWorker>>>,
    cmd_rx: mpsc::Receiver<SupervisorCmd>,
    allow_unsafe_root_plugins: Arc<AtomicBool>,
    shutting_down: Arc<AtomicBool>,
}

impl SupervisorTaskCtx {
    fn set_state(&self, next: PluginState) {
        self.state_tx.send_modify(|status| {
            status.state = next;
            status.confirmed = false;
            if next == PluginState::Starting {
                status.failure = None;
            }
        });
    }

    fn record_failure(&self, reason: String) {
        self.state_tx.send_modify(|status| {
            if !status.ever_confirmed && status.initial_failure.is_none() {
                status.initial_failure = Some(reason.clone());
            }
            status.failure = Some(reason);
        });
    }
}

async fn run_supervisor_loop(
    mut ctx: SupervisorTaskCtx,
    initial_ready: oneshot::Sender<Result<(), HostError>>,
) {
    let mut initial_ready = Some(initial_ready);
    let mut recent_crashes: VecDeque<Instant> = VecDeque::new();
    let mut backoff = ctx.config.initial_backoff;
    let mut startup_failures = 0usize;

    loop {
        ctx.set_state(PluginState::Starting);
        let validation = tokio::select! {
            biased;
            command = ctx.cmd_rx.recv() => {
                acknowledge_stopped(&ctx, command);
                return;
            }
            result = validate_worker_launch(&ctx.paths, &ctx.spec, &ctx.allow_unsafe_root_plugins) => result,
        };
        if let Err(err) = validation {
            let reason = err.to_string();
            warn!(plugin = %ctx.spec.manifest.name, %reason, "plugin worker launch refused");
            if let Some(tx) = initial_ready.take() {
                let _ = tx.send(Err(err));
            } else {
                ctx.record_failure(reason);
                ctx.set_state(PluginState::Failed);
                wait_for_shutdown_only(&mut ctx).await;
            }
            return;
        }
        let spawned_at = Instant::now();
        let spawn_result = spawn_worker_process(&mut ctx);
        if let Some(tx) = initial_ready.take() {
            let _ = tx.send(Ok(()));
        }

        let mut running = match spawn_result {
            Ok(worker) => worker,
            Err(HostError::WorkerStopped) => {
                ctx.set_state(PluginState::Stopped);
                return;
            }
            Err(err) => {
                error!(plugin = %ctx.spec.manifest.name, error = %err, "failed to spawn plugin worker");
                ctx.record_failure(err.to_string());
                startup_failures = startup_failures.saturating_add(1);
                if handle_unexpected_exit(
                    &mut ctx,
                    spawned_at,
                    &mut recent_crashes,
                    &mut backoff,
                    startup_failures,
                )
                .await
                {
                    return;
                }
                backoff = next_backoff(backoff, ctx.config.max_backoff);
                continue;
            }
        };

        let exit_reason = wait_worker_or_command(&mut ctx, &mut running).await;
        let confirmed = ctx.state_tx.borrow().confirmed;
        ctx.set_state(PluginState::Stopping);
        fail_pending_calls(&running.pending);
        *lock_unpoisoned(&ctx.active) = None;

        let reason = match exit_reason {
            WorkerExit::Shutdown { uninstall, done } => {
                let res = terminate_worker(running, uninstall, ctx.config.shutdown_timeout).await;
                ctx.set_state(PluginState::Stopped);
                let _ = done.send(res);
                return;
            }
            WorkerExit::IpcClosed => {
                let _ = terminate_worker(running, false, ctx.config.shutdown_timeout).await;
                "plugin IPC reader terminated".to_owned()
            }
            WorkerExit::StartupFailed(reason) => {
                ctx.record_failure(reason.clone());
                let _ = terminate_worker(running, false, ctx.config.shutdown_timeout).await;
                reason
            }
            WorkerExit::Crashed(reason) => reason,
        };
        warn!(plugin = %ctx.spec.manifest.name, %reason, "plugin worker exited unexpectedly");
        if confirmed {
            startup_failures = 0;
        } else {
            ctx.record_failure(reason);
            startup_failures = startup_failures.saturating_add(1);
        }
        if handle_unexpected_exit(
            &mut ctx,
            spawned_at,
            &mut recent_crashes,
            &mut backoff,
            startup_failures,
        )
        .await
        {
            return;
        }
    }
}

/// Checks the actual executable tree for every launch, including supervisor-owned retries.
async fn validate_worker_launch(
    paths: &PlatformPaths,
    spec: &PluginSpawnSpec,
    allow_unsafe_root_plugins: &AtomicBool,
) -> Result<(), HostError> {
    #[cfg(unix)]
    if spec.privilege_target() == PrivilegeTarget::Root && current_process_identity().euid == 0 {
        if allow_unsafe_root_plugins.load(Ordering::Relaxed) {
            warn!(plugin = %spec.manifest.name, "--allow-unsafe-root-plugins skips the ownership check");
            return Ok(());
        }
        let directory = paths.plugins_dir().join(&spec.directory);
        let entrypoint = spec.entrypoint.clone();
        let plugin = spec.manifest.name.clone();
        tokio::task::spawn_blocking(move || {
            // Shared ancestors make both plugin import roots reachable through a trusted path.
            check_secure_ancestors(&entrypoint)?;
            // Python loads vendored modules from DECKY_PLUGIN_DIR and starts in the entrypoint's
            // parent. Both import roots must be trusted.
            check_root_only_tree(&directory)?;
            if let Some(parent) = entrypoint.parent().filter(|parent| *parent != directory) {
                check_root_only_tree(parent)?;
            }
            Ok(())
        })
        .await
        .map_err(|err| HostError::StartupFailed(format!("cannot validate `{plugin}`: {err}")))?
        .map_err(HostError::UnsafeRootPlugin)?;
    }
    #[cfg(not(unix))]
    let _ = (paths, spec, allow_unsafe_root_plugins);
    Ok(())
}

async fn handle_unexpected_exit(
    ctx: &mut SupervisorTaskCtx,
    spawned_at: Instant,
    recent_crashes: &mut VecDeque<Instant>,
    backoff: &mut Duration,
    startup_failures: usize,
) -> bool {
    if spawned_at.elapsed() >= ctx.config.stability_window {
        *backoff = ctx.config.initial_backoff;
        recent_crashes.clear();
    }
    if startup_failures >= ctx.config.max_crashes_in_window
        || record_crash_and_check_trip(recent_crashes, &ctx.config)
    {
        error!(
            plugin = %ctx.spec.manifest.name,
            window_secs = ctx.config.crash_window.as_secs(),
            max_crashes = ctx.config.max_crashes_in_window,
            "plugin tripped crash-loop circuit breaker"
        );
        ctx.set_state(PluginState::Failed);
        wait_for_shutdown_only(ctx).await;
        return true;
    }
    ctx.set_state(PluginState::BackingOff);
    if wait_backoff_or_shutdown(ctx, *backoff).await {
        ctx.set_state(PluginState::Stopped);
        return true;
    }
    *backoff = next_backoff(*backoff, ctx.config.max_backoff);
    false
}

fn acknowledge_stopped(ctx: &SupervisorTaskCtx, command: Option<SupervisorCmd>) {
    ctx.set_state(PluginState::Stopped);
    if let Some(SupervisorCmd::Shutdown { done, .. }) = command {
        let _ = done.send(Ok(()));
    }
}

fn record_crash_and_check_trip(
    recent_crashes: &mut VecDeque<Instant>,
    config: &SupervisorConfig,
) -> bool {
    let now = Instant::now();
    recent_crashes.push_back(now);
    while let Some(&front) = recent_crashes.front() {
        if now.duration_since(front) > config.crash_window {
            recent_crashes.pop_front();
        } else {
            break;
        }
    }
    recent_crashes.len() >= config.max_crashes_in_window
}

fn next_backoff(current: Duration, max: Duration) -> Duration {
    current.saturating_mul(2).min(max)
}

async fn wait_backoff_or_shutdown(ctx: &mut SupervisorTaskCtx, delay: Duration) -> bool {
    tokio::select! {
        () = tokio::time::sleep(delay) => false,
        cmd = ctx.cmd_rx.recv() => {
            if let Some(SupervisorCmd::Shutdown { done, .. }) = cmd {
                let _ = done.send(Ok(()));
            }
            true
        }
    }
}

async fn wait_for_shutdown_only(ctx: &mut SupervisorTaskCtx) {
    if let Some(SupervisorCmd::Shutdown { done, .. }) = ctx.cmd_rx.recv().await {
        ctx.set_state(PluginState::Stopped);
        let _ = done.send(Ok(()));
    }
}

enum WorkerExit {
    Shutdown {
        uninstall: bool,
        done: oneshot::Sender<Result<(), HostError>>,
    },
    IpcClosed,
    StartupFailed(String),
    Crashed(String),
}

struct RunningWorkerProcess {
    plugin_name: String,
    child: tokio::process::Child,
    write_tx: mpsc::Sender<String>,
    write_task: tokio::task::JoinHandle<()>,
    read_task: tokio::task::JoinHandle<()>,
    pending: SharedPendingCalls,
    startup_rx: watch::Receiver<Option<WorkerStartup>>,
}

#[derive(PartialEq, Eq)]
enum StartupPhase {
    Starting,
    Observing,
    Verifying,
    Confirmed,
}

fn send_startup_probe(
    running: &RunningWorkerProcess,
    generation: &str,
    probe: u8,
) -> Result<(), HostError> {
    let frame = serde_json::to_string(&PluginIpcToWorker::StartupProbe {
        generation: generation.to_owned(),
        probe,
    })
    .map_err(zerodeck_protocol::ProtocolError::from)?;
    running
        .write_tx
        .try_send(frame)
        .map_err(|_| HostError::WorkerStopped)
}

async fn wait_worker_or_command(
    ctx: &mut SupervisorTaskCtx,
    running: &mut RunningWorkerProcess,
) -> WorkerExit {
    let deadline = tokio::time::sleep(ctx.config.startup_timeout);
    tokio::pin!(deadline);
    let mut phase = StartupPhase::Starting;
    let mut startup_open = true;
    if let Err(err) = send_startup_probe(running, &ctx.spec.generation, INITIAL_PROBE) {
        return WorkerExit::StartupFailed(err.to_string());
    }
    loop {
        tokio::select! {
            biased;
            cmd = ctx.cmd_rx.recv() => {
                return match cmd {
                    Some(SupervisorCmd::Shutdown { uninstall, done }) => WorkerExit::Shutdown { uninstall, done },
                    None => {
                        let (done, _) = oneshot::channel();
                        WorkerExit::Shutdown { uninstall: false, done }
                    }
                };
            }
            changed = running.startup_rx.changed(), if startup_open && phase != StartupPhase::Confirmed => {
                startup_open = changed.is_ok();
                match running.startup_rx.borrow_and_update().as_ref() {
                    Some(WorkerStartup::StartupFailed { error }) => {
                        return WorkerExit::StartupFailed(error.clone());
                    }
                    Some(WorkerStartup::Ready { probe: INITIAL_PROBE }) if phase == StartupPhase::Starting && startup_open => {
                        if deadline.is_elapsed() {
                            return WorkerExit::StartupFailed(HostError::StartupTimeout.to_string());
                        }
                        phase = StartupPhase::Observing;
                        ctx.set_state(PluginState::Running);
                        info!(plugin = %ctx.spec.manifest.name, "plugin worker ready for IPC");
                        deadline.as_mut().reset(tokio::time::Instant::now() + STARTUP_OBSERVATION);
                    }
                    Some(WorkerStartup::Ready { probe: OBSERVATION_PROBE }) if phase == StartupPhase::Verifying && startup_open => {
                        if deadline.is_elapsed() {
                            return WorkerExit::StartupFailed(HostError::StartupTimeout.to_string());
                        }
                        phase = StartupPhase::Confirmed;
                        ctx.state_tx.send_modify(|status| {
                            status.confirmed = true;
                            status.ever_confirmed = true;
                        });
                    }
                    _ => {}
                }
            }
            _ = &mut running.read_task => return WorkerExit::IpcClosed,
            status = running.child.wait() => {
                let msg = match status {
                    Ok(exit) => format!("exit status {exit}"),
                    Err(err) => format!("wait error: {err}"),
                };
                return WorkerExit::Crashed(msg);
            }
            () = &mut deadline, if phase != StartupPhase::Confirmed => {
                if phase != StartupPhase::Observing {
                    return WorkerExit::StartupFailed(HostError::StartupTimeout.to_string());
                }
                if let Err(err) = send_startup_probe(running, &ctx.spec.generation, OBSERVATION_PROBE) {
                    return WorkerExit::StartupFailed(err.to_string());
                }
                phase = StartupPhase::Verifying;
                deadline.as_mut().reset(tokio::time::Instant::now() + OBSERVATION_PROBE_TIMEOUT);
            }
        }
    }
}

async fn terminate_worker(
    mut running: RunningWorkerProcess,
    uninstall: bool,
    grace: Duration,
) -> Result<(), HostError> {
    let deadline = tokio::time::Instant::now() + grace;
    if let Ok(encoded) = serde_json::to_string(&PluginIpcToWorker::Control { uninstall }) {
        let _ = tokio::time::timeout_at(deadline, running.write_tx.send(encoded)).await;
    }

    // Dropping the sender signals the writer task to flush the Control frame and shut down
    // the socket write half, delivering clean EOF to `zerodeck-py-host`.
    drop(running.write_tx);
    if tokio::time::timeout_at(deadline, running.child.wait())
        .await
        .is_err()
    {
        warn!(plugin = %running.plugin_name, "plugin worker ignored graceful shutdown; sending SIGKILL");
        send_sigkill(&mut running.child);
        let _ = running.child.wait().await;
    }

    running.write_task.abort();
    running.read_task.abort();
    Ok(())
}

#[cfg(unix)]
fn send_sigkill(child: &mut tokio::process::Child) {
    if let Some(pid_raw) = child.id()
        && let Ok(pid_i32) = i32::try_from(pid_raw)
        && let Some(pid) = rustix::process::Pid::from_raw(pid_i32)
    {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    let _ = child.start_kill();
}

#[cfg(not(unix))]
fn send_sigkill(child: &mut tokio::process::Child) {
    let _ = child.start_kill();
}

#[cfg(unix)]
fn spawn_worker_process(ctx: &mut SupervisorTaskCtx) -> Result<RunningWorkerProcess, HostError> {
    let (parent_std, child_std) = prepare_worker_dirs_and_socket(ctx)?;
    let mut cmd = build_isolated_command(ctx, child_std.as_raw_fd());

    match ctx.cmd_rx.try_recv() {
        Ok(command) => {
            acknowledge_stopped(ctx, Some(command));
            return Err(HostError::WorkerStopped);
        }
        Err(mpsc::error::TryRecvError::Disconnected) => return Err(HostError::WorkerStopped),
        Err(mpsc::error::TryRecvError::Empty) => {}
    }
    if ctx.shutting_down.load(Ordering::SeqCst) {
        return Err(HostError::WorkerStopped);
    }
    let child = cmd.spawn().map_err(|source| HostError::ProcessIo {
        plugin: ctx.spec.manifest.name.clone(),
        source,
    })?;
    // Close parent's copy of the child FD immediately so EOF triggers when the worker exits.
    drop(child_std);

    wire_worker_io(ctx, child, parent_std)
}

#[cfg(unix)]
fn prepare_worker_dirs_and_socket(
    ctx: &SupervisorTaskCtx,
) -> Result<
    (
        std::os::unix::net::UnixStream,
        std::os::unix::net::UnixStream,
    ),
    HostError,
> {
    let spec = &ctx.spec;
    let owner = match spec.privilege_target() {
        PrivilegeTarget::Root => None,
        PrivilegeTarget::HostUser => Some((ctx.host_user.uid, ctx.host_user.gid)),
    };

    ensure_owned_dir(&ctx.paths.settings_dir().join(&spec.directory), owner, None)?;
    ensure_owned_dir(&ctx.paths.data_dir().join(&spec.directory), owner, None)?;
    ensure_owned_dir(&ctx.paths.logs_dir().join(&spec.directory), owner, None)?;

    let (parent_std, child_std) =
        std::os::unix::net::UnixStream::pair().map_err(|source| HostError::ProcessIo {
            plugin: spec.manifest.name.clone(),
            source,
        })?;
    parent_std
        .set_nonblocking(true)
        .map_err(|source| HostError::ProcessIo {
            plugin: spec.manifest.name.clone(),
            source,
        })?;
    Ok((parent_std, child_std))
}

#[cfg(unix)]
fn build_isolated_command(
    ctx: &SupervisorTaskCtx,
    child_fd: std::os::unix::io::RawFd,
) -> tokio::process::Command {
    let spec = &ctx.spec;
    let mut cmd = ctx.config.python_host.build_command(spec, &ctx.paths);
    cmd.arg("--fd").arg(child_fd.to_string());
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);

    let drop_privs = spec.privilege_target() == PrivilegeTarget::HostUser
        && current_process_identity().euid == 0;

    #[cfg(target_os = "linux")]
    let (user_id, group_id, target_groups) = {
        let uid = rustix::thread::Uid::from_raw(ctx.host_user.uid);
        let gid = rustix::thread::Gid::from_raw(ctx.host_user.gid);
        let groups: Vec<rustix::thread::Gid> = ctx
            .host_user
            .supplemental_gids
            .iter()
            .copied()
            .map(rustix::thread::Gid::from_raw)
            .collect();
        (uid, gid, groups)
    };

    #[cfg(all(unix, not(target_os = "linux")))]
    if drop_privs {
        cmd.uid(ctx.host_user.uid);
        cmd.gid(ctx.host_user.gid);
    }

    #[cfg(target_os = "linux")]
    let daemon_pid = rustix::process::getpid();

    // SAFETY: `pre_exec` runs after `fork` and before `execve` in a single-threaded child process.
    // All captured descriptors and IDs are pre-computed before `fork`, and the closure invokes
    // only async-signal-safe syscalls without allocating or locking. The parent-death signal is
    // set after the uid/gid change because Linux clears it on credential changes, and failing to
    // arm it fails the spawn (fail closed); the `getppid` check then catches a daemon that died
    // before the signal was armed. `PR_SET_PDEATHSIG` fires when the forking *thread* exits, not
    // the process, so workers must only be spawned from long-lived async runtime worker threads,
    // never from `spawn_blocking` or other blocking-pool threads that may be reaped while idle.
    #[allow(unsafe_code)]
    unsafe {
        cmd.pre_exec(move || {
            let borrowed = BorrowedFd::borrow_raw(child_fd);
            rustix::io::fcntl_setfd(borrowed, rustix::io::FdFlags::empty())
                .map_err(|err| std::io::Error::from_raw_os_error(err.raw_os_error()))?;

            let _ = rustix::process::setpgid(None, None);
            #[cfg(target_os = "linux")]
            {
                if drop_privs {
                    rustix::thread::set_thread_groups(&target_groups)
                        .map_err(|err| std::io::Error::from_raw_os_error(err.raw_os_error()))?;
                    rustix::thread::set_thread_res_gid(group_id, group_id, group_id)
                        .map_err(|err| std::io::Error::from_raw_os_error(err.raw_os_error()))?;
                    rustix::thread::set_thread_res_uid(user_id, user_id, user_id)
                        .map_err(|err| std::io::Error::from_raw_os_error(err.raw_os_error()))?;
                }
                rustix::process::set_parent_process_death_signal(Some(
                    rustix::process::Signal::TERM,
                ))
                .map_err(|err| std::io::Error::from_raw_os_error(err.raw_os_error()))?;
                if rustix::process::getppid() != Some(daemon_pid) {
                    return Err(std::io::Error::from_raw_os_error(
                        rustix::io::Errno::SRCH.raw_os_error(),
                    ));
                }
            }
            Ok(())
        });
    }

    cmd
}

#[cfg(unix)]
fn wire_worker_io(
    ctx: &SupervisorTaskCtx,
    mut child: tokio::process::Child,
    parent_std: std::os::unix::net::UnixStream,
) -> Result<RunningWorkerProcess, HostError> {
    let plugin_name = ctx.spec.manifest.name.clone();
    if let Some(stdout) = child.stdout.take() {
        spawn_log_reader(plugin_name.clone(), "stdout", stdout);
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_log_reader(plugin_name.clone(), "stderr", stderr);
    }

    let tokio_stream =
        tokio::net::UnixStream::from_std(parent_std).map_err(|source| HostError::ProcessIo {
            plugin: plugin_name.clone(),
            source,
        })?;
    let (read_half, mut write_half) = tokio_stream.into_split();

    let (write_tx, mut write_rx) = mpsc::channel::<String>(128);
    let pending: SharedPendingCalls = Arc::new(Mutex::new(PendingCalls::default()));
    let (startup_tx, startup_rx) = watch::channel(None);

    let write_plugin = plugin_name.clone();
    let write_task = tokio::spawn(async move {
        while let Some(mut line) = write_rx.recv().await {
            line.push('\n');
            if let Err(err) = write_half.write_all(line.as_bytes()).await {
                debug!(plugin = %write_plugin, error = %err, "worker IPC write failed");
                break;
            }
        }
        let _ = write_half.shutdown().await;
    });

    let read_plugin = plugin_name.clone();
    let read_pending = Arc::clone(&pending);
    let read_events = ctx.events_tx.clone();
    let generation = ctx.spec.generation.clone();
    let read_task = tokio::spawn(async move {
        run_ipc_reader(
            read_plugin,
            read_half,
            read_pending,
            read_events,
            generation,
            startup_tx,
        )
        .await;
    });

    *lock_unpoisoned(&ctx.active) = Some(ActiveWorker {
        write_tx: write_tx.clone(),
        pending: Arc::clone(&pending),
    });

    Ok(RunningWorkerProcess {
        plugin_name,
        child,
        write_tx,
        write_task,
        read_task,
        pending,
        startup_rx,
    })
}

#[cfg(not(unix))]
fn spawn_worker_process(ctx: &mut SupervisorTaskCtx) -> Result<RunningWorkerProcess, HostError> {
    Err(HostError::ProcessIo {
        plugin: ctx.spec.manifest.name.clone(),
        source: std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Unix socketpair plugin workers are only supported on Unix hosts",
        ),
    })
}

async fn run_ipc_reader<R: AsyncRead + Unpin>(
    plugin_name: String,
    reader: R,
    pending: SharedPendingCalls,
    events_tx: broadcast::Sender<PluginEvent>,
    expected_generation: String,
    startup_tx: watch::Sender<Option<WorkerStartup>>,
) {
    let mut buf = BufReader::new(reader);
    loop {
        match read_bounded_line(&mut buf, MAX_IPC_LINE_BYTES).await {
            Ok(None) => break,
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                match PluginIpcFromWorker::from_json_str(&line) {
                    Ok(PluginIpcFromWorker::Response {
                        id,
                        success,
                        res,
                        error,
                    }) => {
                        let waiter = {
                            let mut guard = lock_unpoisoned(&pending);
                            guard.waiters.remove(&id)
                        };
                        if let Some(tx) = waiter {
                            let outcome = if success {
                                Ok(res)
                            } else {
                                let message =
                                    res.as_str().map_or_else(|| res.to_string(), str::to_owned);
                                let (name, traceback) = match error {
                                    Some(detail) => (detail.name, detail.traceback),
                                    None => ("PluginError".to_owned(), None),
                                };
                                Err(HostError::PluginCallFailed {
                                    name,
                                    message,
                                    traceback,
                                })
                            };
                            let _ = tx.send(outcome);
                        }
                    }
                    Ok(PluginIpcFromWorker::Event { event, args }) => {
                        let _ = events_tx.send(PluginEvent {
                            plugin: plugin_name.clone(),
                            event,
                            args,
                        });
                    }
                    Ok(PluginIpcFromWorker::Startup { generation, status }) => {
                        if generation == expected_generation {
                            startup_tx.send_if_modified(|current| {
                                if matches!(current, Some(WorkerStartup::StartupFailed { .. }))
                                    || current.as_ref() == Some(&status)
                                {
                                    return false;
                                }
                                *current = Some(status);
                                true
                            });
                        }
                    }
                    Err(err) => {
                        warn!(plugin = %plugin_name, error = %err, "ignored malformed IPC line");
                    }
                }
            }
            Err(err) => {
                warn!(plugin = %plugin_name, error = %err, "worker IPC reader failed");
                break;
            }
        }
    }
    fail_pending_calls(&pending);
}

async fn read_bounded_line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    max_bytes: usize,
) -> Result<Option<String>, HostError> {
    let mut bytes = Vec::new();
    loop {
        let available = reader
            .fill_buf()
            .await
            .map_err(|source| HostError::ProcessIo {
                plugin: "ipc".to_owned(),
                source,
            })?;
        if available.is_empty() {
            if bytes.is_empty() {
                return Ok(None);
            }
            return Ok(Some(String::from_utf8_lossy(&bytes).into_owned()));
        }

        if let Some(pos) = available.iter().position(|b| *b == b'\n') {
            if bytes.len().saturating_add(pos) > max_bytes {
                return Err(HostError::FrameTooLarge);
            }
            bytes.extend_from_slice(&available[..pos]);
            reader.consume(pos + 1);
            return Ok(Some(String::from_utf8_lossy(&bytes).into_owned()));
        }

        if bytes.len().saturating_add(available.len()) > max_bytes {
            return Err(HostError::FrameTooLarge);
        }
        bytes.extend_from_slice(available);
        let len = available.len();
        reader.consume(len);
    }
}

async fn read_truncated_log_line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    max_bytes: usize,
) -> std::io::Result<Option<(String, bool)>> {
    let mut bytes = Vec::new();
    let mut truncated = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if bytes.is_empty() && !truncated {
                return Ok(None);
            }
            return Ok(Some((
                String::from_utf8_lossy(&bytes).into_owned(),
                truncated,
            )));
        }

        if let Some(pos) = available.iter().position(|b| *b == b'\n') {
            let room = max_bytes.saturating_sub(bytes.len());
            let take = pos.min(room);
            bytes.extend_from_slice(&available[..take]);
            if pos > room {
                truncated = true;
            }
            reader.consume(pos + 1);
            return Ok(Some((
                String::from_utf8_lossy(&bytes).into_owned(),
                truncated,
            )));
        }

        let room = max_bytes.saturating_sub(bytes.len());
        let take = available.len().min(room);
        bytes.extend_from_slice(&available[..take]);
        if available.len() > room {
            truncated = true;
        }
        let len = available.len();
        reader.consume(len);
    }
}

fn spawn_log_reader<R: AsyncRead + Unpin + Send + 'static>(
    plugin: String,
    stream: &'static str,
    reader: R,
) {
    tokio::spawn(async move {
        let mut buf = BufReader::new(reader);
        while let Ok(Some((line, truncated))) =
            read_truncated_log_line(&mut buf, MAX_LOG_LINE_BYTES).await
        {
            let trimmed = line.trim_end_matches('\r');
            if !trimmed.is_empty() || truncated {
                let suffix = if truncated { " ... [truncated]" } else { "" };
                if stream == "stderr" {
                    warn!(target: "zerodeck_plugin_host::worker", plugin = %plugin, stream, "{trimmed}{suffix}");
                } else {
                    info!(target: "zerodeck_plugin_host::worker", plugin = %plugin, stream, "{trimmed}{suffix}");
                }
            }
        }
    });
}

fn fail_pending_calls(pending: &SharedPendingCalls) {
    let waiters: Vec<_> = {
        let mut guard = lock_unpoisoned(pending);
        guard.closed = true;
        guard.waiters.drain().map(|(_, tx)| tx).collect()
    };
    for tx in waiters {
        let _ = tx.send(Err(HostError::WorkerStopped));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_calls_rejects_new_waiters_after_fail_pending_calls_closes() {
        let pending: SharedPendingCalls = Arc::new(Mutex::new(PendingCalls::default()));
        fail_pending_calls(&pending);

        let guard = lock_unpoisoned(&pending);
        assert!(guard.closed);
        assert!(guard.waiters.is_empty());
    }

    #[tokio::test]
    async fn ipc_discards_startup_reports_for_another_generation() {
        let (mut writer, reader) = tokio::io::duplex(1024);
        let (events, mut event_rx) = broadcast::channel(1);
        let (startup_tx, mut startup_rx) = watch::channel(None);
        let task = tokio::spawn(run_ipc_reader(
            "demo".to_owned(),
            reader,
            Arc::new(Mutex::new(PendingCalls::default())),
            events,
            "current".to_owned(),
            startup_tx,
        ));
        writer
            .write_all(
                b"{\"type\":3,\"generation\":\"older\",\"status\":\"ready\",\"probe\":0}\n\
                  {\"type\":2,\"event\":\"processed\",\"args\":[]}\n",
            )
            .await
            .expect("write startup and marker frames");
        assert_eq!(
            event_rx.recv().await.expect("marker event").event,
            "processed"
        );
        assert_eq!(*startup_rx.borrow(), None);
        writer
            .write_all(
                b"{\"type\":3,\"generation\":\"current\",\"status\":\"ready\",\"probe\":0}\n",
            )
            .await
            .expect("write current startup report");
        startup_rx.changed().await.expect("current report");
        assert_eq!(
            *startup_rx.borrow(),
            Some(WorkerStartup::Ready {
                probe: INITIAL_PROBE
            })
        );
        drop(writer);
        task.await.expect("IPC reader exits");
    }

    #[tokio::test]
    async fn wait_for_active_worker_observes_transition_without_lost_wakeup() {
        let (state_tx, state_rx) = watch::channel(WorkerStatus::new(PluginState::Starting));
        let active = Arc::new(Mutex::new(None));
        let (cmd_tx, _cmd_rx) = mpsc::channel(1);
        let (write_tx, _write_rx) = mpsc::channel(1);

        let managed = ManagedPlugin {
            spec: PluginSpawnSpec {
                generation: "test".to_owned(),
                directory: "demo".to_owned(),
                manifest: zerodeck_protocol::PluginManifest {
                    name: "Demo".to_owned(),
                    author: "Alice".to_owned(),
                    flags: vec![],
                    api_version: 1,
                },
                version: None,
                host_user: "deck".to_owned(),
                host_user_home: PathBuf::from("/home/deck"),
                loader_version: "0.1.0".to_owned(),
                entrypoint: PathBuf::from("/tmp/main.py"),
            },
            state_rx,
            active: Arc::clone(&active),
            cmd_tx,
            startup_timeout: Duration::from_millis(500),
        };

        // Transition happens before `wait_for_active_worker()` is polled.
        *lock_unpoisoned(&active) = Some(ActiveWorker {
            write_tx,
            pending: Arc::new(Mutex::new(PendingCalls::default())),
        });
        let _ = state_tx.send(WorkerStatus::new(PluginState::Running));

        let handle = managed
            .wait_for_active_worker()
            .await
            .expect("must observe Running without lost wakeup");
        assert!(!lock_unpoisoned(&handle.pending).closed);
    }
}
