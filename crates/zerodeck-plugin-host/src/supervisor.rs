use std::collections::{HashMap, VecDeque};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::sync::{Notify, broadcast, mpsc, oneshot};
use tracing::{debug, error, info, warn};
use zerodeck_platform::{PlatformPaths, PrivilegeTarget};
use zerodeck_protocol::{PluginIpcFromWorker, PluginIpcToWorker};

#[cfg(unix)]
use std::os::unix::io::{AsRawFd, BorrowedFd};
#[cfg(unix)]
use zerodeck_platform::{HostUserIdentity, current_process_identity, ensure_owned_dir};

use crate::{BoxHostFuture, HostError, PluginRuntime, PluginSpawnSpec, PythonHostConfig};

/// Maximum newline-delimited JSON line accepted from a worker (4 MiB, matching `zerodeck-py-host`).
const MAX_IPC_LINE_BYTES: usize = 4 * 1024 * 1024;
const MAX_LOG_LINE_BYTES: usize = 16 * 1024;

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
    events_tx: broadcast::Sender<PluginEvent>,
    plugins: Mutex<HashMap<String, Arc<ManagedPlugin>>>,
}

impl PluginSupervisor {
    /// Creates a new [`PluginSupervisor`] and resolves the unprivileged host identity.
    pub fn new(paths: PlatformPaths, config: SupervisorConfig) -> Result<Self, HostError> {
        #[cfg(unix)]
        let host_user = HostUserIdentity::resolve(&paths)?;
        let (events_tx, _) = broadcast::channel(256);
        Ok(Self {
            paths,
            config,
            #[cfg(unix)]
            host_user,
            events_tx,
            plugins: Mutex::new(HashMap::new()),
        })
    }

    /// Subscribes to asynchronous [`PluginEvent`] broadcasts from all workers.
    #[must_use]
    pub fn subscribe_events(&self) -> broadcast::Receiver<PluginEvent> {
        self.events_tx.subscribe()
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

    /// Spawns and begins supervising an active plugin worker, replacing any prior worker with the
    /// same manifest name.
    pub async fn spawn_plugin(
        &self,
        spec: PluginSpawnSpec,
    ) -> Result<Arc<ManagedPlugin>, HostError> {
        let existing = {
            let guard = lock_unpoisoned(&self.plugins);
            guard.get(&spec.manifest.name).cloned()
        };
        if let Some(old) = existing {
            let _ = old.shutdown(false).await;
        }

        let managed = ManagedPlugin::start(
            spec.clone(),
            self.paths.clone(),
            self.config.clone(),
            #[cfg(unix)]
            self.host_user.clone(),
            self.events_tx.clone(),
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
        let managed = {
            let guard = lock_unpoisoned(&self.plugins);
            guard.get(plugin_name).cloned()
        }?;
        Some(managed.state())
    }

    /// Gracefully stops a single supervised plugin (running `_uninstall` when `uninstall` is true).
    pub async fn shutdown_plugin(
        &self,
        plugin_name: &str,
        uninstall: bool,
    ) -> Result<(), HostError> {
        let managed = {
            let mut guard = lock_unpoisoned(&self.plugins);
            guard.remove(plugin_name)
        }
        .ok_or_else(|| HostError::PluginNotFound(plugin_name.to_owned()))?;
        managed.shutdown(uninstall).await
    }

    /// Restarts a supervised plugin cleanly.
    pub async fn reload_plugin(&self, plugin_name: &str) -> Result<Arc<ManagedPlugin>, HostError> {
        let spec = {
            let guard = lock_unpoisoned(&self.plugins);
            guard
                .get(plugin_name)
                .map(|m| m.spec.clone())
                .ok_or_else(|| HostError::PluginNotFound(plugin_name.to_owned()))?
        };
        self.spawn_plugin(spec).await
    }

    /// Gracefully shuts down all supervised plugin workers concurrently.
    pub async fn shutdown_all(&self) {
        let all: Vec<Arc<ManagedPlugin>> = {
            let mut guard = lock_unpoisoned(&self.plugins);
            guard.drain().map(|(_, v)| v).collect()
        };
        for plugin in all {
            if let Err(err) = plugin.shutdown(false).await {
                warn!(plugin = %plugin.spec.manifest.name, error = %err, "error shutting down plugin");
            }
        }
    }
}

type PendingCalls = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value, HostError>>>>>;

#[derive(Debug)]
struct ActiveWorker {
    write_tx: mpsc::Sender<String>,
    pending: PendingCalls,
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
    state: Arc<Mutex<PluginState>>,
    state_notify: Arc<Notify>,
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
    ) -> Result<Arc<Self>, HostError> {
        let state = Arc::new(Mutex::new(PluginState::Starting));
        let state_notify = Arc::new(Notify::new());
        let active = Arc::new(Mutex::new(None));
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        let (ready_tx, ready_rx) = oneshot::channel();

        let managed = Arc::new(Self {
            spec: spec.clone(),
            state: Arc::clone(&state),
            state_notify: Arc::clone(&state_notify),
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
            state,
            state_notify,
            active,
            cmd_rx,
        };
        tokio::spawn(run_supervisor_loop(ctx, ready_tx));

        let _ = ready_rx.await;
        Ok(managed)
    }

    /// Current lifecycle state of this plugin worker.
    #[must_use]
    pub fn state(&self) -> PluginState {
        *lock_unpoisoned(&self.state)
    }

    async fn wait_for_active_worker(&self) -> Result<ActiveWorkerHandle, HostError> {
        let deadline = tokio::time::Instant::now() + self.startup_timeout;
        loop {
            let current = self.state();
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
                    let notified = self.state_notify.notified();
                    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                    if remaining.is_zero() {
                        return Err(HostError::StartupTimeout);
                    }
                    if tokio::time::timeout(remaining, notified).await.is_err() {
                        return Err(HostError::StartupTimeout);
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
    pending: PendingCalls,
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
                PluginIpcToWorker::Control { .. } => unreachable!(),
            };
            let encoded =
                serde_json::to_string(&call).map_err(zerodeck_protocol::ProtocolError::from)?;

            let (tx, rx) = oneshot::channel();
            {
                let mut map = lock_unpoisoned(&worker.pending);
                map.insert(call_id.clone(), tx);
            }

            if worker.write_tx.send(encoded).await.is_err() {
                let mut map = lock_unpoisoned(&worker.pending);
                map.remove(&call_id);
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
    state: Arc<Mutex<PluginState>>,
    state_notify: Arc<Notify>,
    active: Arc<Mutex<Option<ActiveWorker>>>,
    cmd_rx: mpsc::Receiver<SupervisorCmd>,
}

impl SupervisorTaskCtx {
    fn set_state(&self, next: PluginState) {
        *lock_unpoisoned(&self.state) = next;
        self.state_notify.notify_waiters();
    }
}

async fn run_supervisor_loop(mut ctx: SupervisorTaskCtx, initial_ready: oneshot::Sender<()>) {
    let mut initial_ready = Some(initial_ready);
    let mut recent_crashes: VecDeque<Instant> = VecDeque::new();
    let mut backoff = ctx.config.initial_backoff;

    loop {
        ctx.set_state(PluginState::Starting);
        let spawned_at = Instant::now();
        let spawn_result = spawn_worker_process(&ctx);
        if let Some(tx) = initial_ready.take() {
            let _ = tx.send(());
        }

        let mut running = match spawn_result {
            Ok(worker) => {
                ctx.set_state(PluginState::Running);
                info!(plugin = %ctx.spec.manifest.name, "plugin worker started");
                worker
            }
            Err(err) => {
                error!(plugin = %ctx.spec.manifest.name, error = %err, "failed to spawn plugin worker");
                if record_crash_and_check_trip(&mut recent_crashes, &ctx.config) {
                    ctx.set_state(PluginState::Failed);
                    wait_for_shutdown_only(&mut ctx).await;
                    return;
                }
                ctx.set_state(PluginState::BackingOff);
                if wait_backoff_or_shutdown(&mut ctx, backoff).await {
                    ctx.set_state(PluginState::Stopped);
                    return;
                }
                backoff = next_backoff(backoff, ctx.config.max_backoff);
                continue;
            }
        };

        let exit_reason = wait_worker_or_command(&mut ctx, &mut running).await;
        fail_pending_calls(&running.pending);
        *lock_unpoisoned(&ctx.active) = None;

        match exit_reason {
            WorkerExit::Shutdown { uninstall, done } => {
                ctx.set_state(PluginState::Stopping);
                let res = terminate_worker(running, uninstall, ctx.config.shutdown_timeout).await;
                ctx.set_state(PluginState::Stopped);
                let _ = done.send(res);
                return;
            }
            WorkerExit::Crashed(status_msg) => {
                warn!(
                    plugin = %ctx.spec.manifest.name,
                    reason = %status_msg,
                    "plugin worker exited unexpectedly"
                );
                if spawned_at.elapsed() >= ctx.config.stability_window {
                    backoff = ctx.config.initial_backoff;
                    recent_crashes.clear();
                }
                if record_crash_and_check_trip(&mut recent_crashes, &ctx.config) {
                    error!(
                        plugin = %ctx.spec.manifest.name,
                        window_secs = ctx.config.crash_window.as_secs(),
                        max_crashes = ctx.config.max_crashes_in_window,
                        "plugin tripped crash-loop circuit breaker"
                    );
                    ctx.set_state(PluginState::Failed);
                    wait_for_shutdown_only(&mut ctx).await;
                    return;
                }
                ctx.set_state(PluginState::BackingOff);
                if wait_backoff_or_shutdown(&mut ctx, backoff).await {
                    ctx.set_state(PluginState::Stopped);
                    return;
                }
                backoff = next_backoff(backoff, ctx.config.max_backoff);
            }
        }
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
    Crashed(String),
}

struct RunningWorkerProcess {
    plugin_name: String,
    child: tokio::process::Child,
    write_tx: mpsc::Sender<String>,
    write_task: tokio::task::JoinHandle<()>,
    read_task: tokio::task::JoinHandle<()>,
    pending: PendingCalls,
}

async fn wait_worker_or_command(
    ctx: &mut SupervisorTaskCtx,
    running: &mut RunningWorkerProcess,
) -> WorkerExit {
    tokio::select! {
        status = running.child.wait() => {
            let msg = match status {
                Ok(exit) => format!("exit status {exit}"),
                Err(err) => format!("wait error: {err}"),
            };
            WorkerExit::Crashed(msg)
        }
        cmd = ctx.cmd_rx.recv() => match cmd {
            Some(SupervisorCmd::Shutdown { uninstall, done }) => WorkerExit::Shutdown { uninstall, done },
            None => {
                let (done, _) = oneshot::channel();
                WorkerExit::Shutdown { uninstall: false, done }
            }
        }
    }
}

async fn terminate_worker(
    mut running: RunningWorkerProcess,
    uninstall: bool,
    grace: Duration,
) -> Result<(), HostError> {
    if uninstall
        && let Ok(encoded) = serde_json::to_string(&PluginIpcToWorker::Control { uninstall: true })
    {
        let _ = running.write_tx.send(encoded).await;
    }

    // Dropping the sender signals the writer task to flush any queued Control frame and shut down
    // the socket write half, delivering clean EOF to `zerodeck-py-host`.
    drop(running.write_tx);
    let _ = tokio::time::timeout(Duration::from_millis(500), &mut running.write_task).await;

    if tokio::time::timeout(grace, running.child.wait())
        .await
        .is_err()
    {
        warn!(plugin = %running.plugin_name, "plugin worker ignored graceful shutdown; sending SIGKILL");
        send_sigkill(&mut running.child);
        let _ = running.child.wait().await;
    }

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
fn spawn_worker_process(ctx: &SupervisorTaskCtx) -> Result<RunningWorkerProcess, HostError> {
    let (parent_std, child_std) = prepare_worker_dirs_and_socket(ctx)?;
    let mut cmd = build_isolated_command(ctx, child_std.as_raw_fd());

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

    let drop_privs = spec.privilege_target() == PrivilegeTarget::HostUser
        && current_process_identity().euid == 0;
    let user_id = rustix::thread::Uid::from_raw(ctx.host_user.uid);
    let group_id = rustix::thread::Gid::from_raw(ctx.host_user.gid);
    #[cfg(target_os = "linux")]
    let target_groups: Vec<rustix::thread::Gid> = ctx
        .host_user
        .supplemental_gids
        .iter()
        .copied()
        .map(rustix::thread::Gid::from_raw)
        .collect();

    // SAFETY: `pre_exec` runs after `fork` and before `execve` in a single-threaded child process.
    // All values (`child_fd`, `user_id`, `group_id`, `target_groups`) are pre-computed before
    // `fork`, and the closure invokes only async-signal-safe syscalls without allocating or locking.
    #[allow(unsafe_code)]
    unsafe {
        cmd.pre_exec(move || {
            let borrowed = BorrowedFd::borrow_raw(child_fd);
            rustix::io::fcntl_setfd(borrowed, rustix::io::FdFlags::empty())
                .map_err(|err| std::io::Error::from_raw_os_error(err.raw_os_error()))?;

            let _ = rustix::process::setpgid(None, None);
            #[cfg(target_os = "linux")]
            {
                let _ = rustix::process::set_parent_process_death_signal(Some(
                    rustix::process::Signal::TERM,
                ));
            }

            if drop_privs {
                #[cfg(target_os = "linux")]
                rustix::thread::set_thread_groups(&target_groups)
                    .map_err(|err| std::io::Error::from_raw_os_error(err.raw_os_error()))?;
                rustix::thread::set_thread_res_gid(group_id, group_id, group_id)
                    .map_err(|err| std::io::Error::from_raw_os_error(err.raw_os_error()))?;
                rustix::thread::set_thread_res_uid(user_id, user_id, user_id)
                    .map_err(|err| std::io::Error::from_raw_os_error(err.raw_os_error()))?;
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
    let pending: PendingCalls = Arc::new(Mutex::new(HashMap::new()));

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
    let read_task = tokio::spawn(async move {
        run_ipc_reader(read_plugin, read_half, read_pending, read_events).await;
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
    })
}

#[cfg(not(unix))]
fn spawn_worker_process(ctx: &SupervisorTaskCtx) -> Result<RunningWorkerProcess, HostError> {
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
    pending: PendingCalls,
    events_tx: broadcast::Sender<PluginEvent>,
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
                            let mut map = lock_unpoisoned(&pending);
                            map.remove(&id)
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

fn spawn_log_reader<R: AsyncRead + Unpin + Send + 'static>(
    plugin: String,
    stream: &'static str,
    reader: R,
) {
    tokio::spawn(async move {
        let mut buf = BufReader::new(reader);
        while let Ok(Some(line)) = read_bounded_line(&mut buf, MAX_LOG_LINE_BYTES).await {
            let trimmed = line.trim_end_matches('\r');
            if !trimmed.is_empty() {
                if stream == "stderr" {
                    warn!(target: "zerodeck_plugin_host::worker", plugin = %plugin, stream, "{trimmed}");
                } else {
                    info!(target: "zerodeck_plugin_host::worker", plugin = %plugin, stream, "{trimmed}");
                }
            }
        }
    });
}

fn fail_pending_calls(pending: &PendingCalls) {
    let waiters: Vec<_> = {
        let mut map = lock_unpoisoned(pending);
        map.drain().map(|(_, tx)| tx).collect()
    };
    for tx in waiters {
        let _ = tx.send(Err(HostError::WorkerStopped));
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
