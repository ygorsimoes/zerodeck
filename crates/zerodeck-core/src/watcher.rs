use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::{broadcast, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{debug, warn};
use zerodeck_plugin_host::HostError;

use crate::{CoreError, LoaderEvent, PluginManager};

const DEBOUNCE_DELAY: Duration = Duration::from_millis(300);

#[cfg(target_os = "linux")]
const SWEEP_INTERVAL: Duration = Duration::from_secs(2);
#[cfg(not(target_os = "linux"))]
const SWEEP_INTERVAL: Duration = Duration::from_millis(500);

/// Handle controlling the background hot-reload watcher for `"debug"` plugins.
///
/// Stopping or dropping this handle terminates the background watcher task.
#[derive(Debug)]
pub struct DebugWatcherHandle {
    stop_tx: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl DebugWatcherHandle {
    /// Signals the background watcher task to exit and awaits its completion.
    pub async fn stop(mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for DebugWatcherHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Starts a background watcher monitoring active `"debug"`-flagged plugins for backend and
/// frontend bundle changes.
///
/// Uses Linux `inotify` (`ISC`) combined with a lightweight `mtime` sweep (`2s` fallback on Linux
/// for bind-mounted filesystems, `500ms` on non-Linux hosts) and a `300ms` burst debounce.
pub fn start_debug_watcher(manager: Arc<PluginManager>) -> Result<DebugWatcherHandle, CoreError> {
    let loader_events = manager.subscribe_loader_events();
    let mut state = WatcherState::new(manager);
    state.sync_with_manager(false);

    #[cfg(target_os = "linux")]
    let (inotify, linux_watches) = linux::init_inotify(&state.plugins_dir, &state.tracked)?;

    let (stop_tx, stop_rx) = oneshot::channel();
    let task = tokio::spawn(run_watcher_loop(
        state,
        loader_events,
        stop_rx,
        #[cfg(target_os = "linux")]
        inotify,
        #[cfg(target_os = "linux")]
        linux_watches,
    ));

    Ok(DebugWatcherHandle {
        stop_tx: Some(stop_tx),
        task: Some(task),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
}

type Snapshot = BTreeMap<PathBuf, FileStamp>;

#[derive(Debug, Clone)]
struct TrackedPlugin {
    name: String,
    backend_snapshot: Snapshot,
    frontend_snapshot: Snapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum DirtyScope {
    Backend,
    Frontend,
}

struct WatcherState {
    manager: Arc<PluginManager>,
    plugins_dir: PathBuf,
    tracked: HashMap<String, TrackedPlugin>,
    pending: HashMap<(String, DirtyScope), Instant>,
}

impl WatcherState {
    fn new(manager: Arc<PluginManager>) -> Self {
        let plugins_dir = manager.paths().plugins_dir();
        Self {
            manager,
            plugins_dir,
            tracked: HashMap::new(),
            pending: HashMap::new(),
        }
    }

    fn sync_with_manager(&mut self, scan_existing: bool) -> bool {
        let debug_plugins = self.manager.debug_plugins_snapshot();
        let active_dirs: HashMap<String, String> = debug_plugins
            .into_iter()
            .map(|p| (p.directory, p.manifest.name))
            .collect();

        let mut structure_changed = false;
        let prev_len = self.tracked.len();
        self.tracked.retain(|dir, _| active_dirs.contains_key(dir));
        self.pending
            .retain(|(dir, _), _| active_dirs.contains_key(dir));
        if self.tracked.len() != prev_len {
            structure_changed = true;
        }

        for (directory, name) in active_dirs {
            let plugin_dir = self.plugins_dir.join(&directory);
            if let Some(existing) = self.tracked.get_mut(&directory) {
                existing.name = name;
                if scan_existing {
                    let next_backend = scan_backend_snapshot(&plugin_dir);
                    if next_backend != existing.backend_snapshot {
                        existing.backend_snapshot = next_backend;
                        Self::mark_pending_entry(
                            &mut self.pending,
                            &directory,
                            DirtyScope::Backend,
                        );
                    }
                    let next_frontend = scan_frontend_snapshot(&plugin_dir);
                    if next_frontend != existing.frontend_snapshot {
                        existing.frontend_snapshot = next_frontend;
                        Self::mark_pending_entry(
                            &mut self.pending,
                            &directory,
                            DirtyScope::Frontend,
                        );
                    }
                }
            } else {
                structure_changed = true;
                self.tracked.insert(
                    directory,
                    TrackedPlugin {
                        name,
                        backend_snapshot: scan_backend_snapshot(&plugin_dir),
                        frontend_snapshot: scan_frontend_snapshot(&plugin_dir),
                    },
                );
            }
        }

        structure_changed
    }

    #[cfg(target_os = "linux")]
    fn mark_dirty(&mut self, directory: &str, scope: DirtyScope) {
        if self.tracked.contains_key(directory) {
            Self::mark_pending_entry(&mut self.pending, directory, scope);
        }
    }

    fn mark_pending_entry(
        pending: &mut HashMap<(String, DirtyScope), Instant>,
        directory: &str,
        scope: DirtyScope,
    ) {
        pending.insert(
            (directory.to_owned(), scope),
            Instant::now() + DEBOUNCE_DELAY,
        );
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.pending.values().copied().min()
    }

    fn take_due(&mut self, now: Instant) -> Vec<(String, DirtyScope)> {
        let mut due: Vec<(String, DirtyScope)> = self
            .pending
            .iter()
            .filter_map(|((dir, scope), deadline)| {
                (*deadline <= now).then_some((dir.clone(), *scope))
            })
            .collect();
        for key in &due {
            self.pending.remove(key);
        }
        // Flush backend changes before frontend notifications for the same directory so manifest
        // updates and worker reloads settle before the frontend reloads.
        due.sort_by_key(|(dir, scope)| {
            let order = match scope {
                DirtyScope::Backend => 0u8,
                DirtyScope::Frontend => 1u8,
            };
            (dir.clone(), order)
        });
        due
    }

    fn refresh_snapshot(&mut self, directory: &str, scope: DirtyScope) {
        let plugin_dir = self.plugins_dir.join(directory);
        let Some(entry) = self.tracked.get_mut(directory) else {
            return;
        };
        match scope {
            DirtyScope::Backend => entry.backend_snapshot = scan_backend_snapshot(&plugin_dir),
            DirtyScope::Frontend => entry.frontend_snapshot = scan_frontend_snapshot(&plugin_dir),
        }
    }

    async fn flush_due(&mut self, due: Vec<(String, DirtyScope)>) {
        for (directory, scope) in due {
            self.refresh_snapshot(&directory, scope);
            let Some(active_debug) = self
                .manager
                .debug_plugins_snapshot()
                .into_iter()
                .find(|p| p.directory == directory)
            else {
                continue;
            };
            let plugin_name = active_debug.manifest.name;

            match scope {
                DirtyScope::Backend => match self.manager.reload_plugin(&plugin_name).await {
                    Ok(()) => {
                        debug!(plugin = %plugin_name, directory = %directory, "debug watcher reloaded backend");
                    }
                    Err(
                        CoreError::Host(HostError::PassivePlugin(_) | HostError::PluginNotFound(_))
                        | CoreError::PluginDisabled(_),
                    ) => {
                        debug!(plugin = %plugin_name, directory = %directory, "skipped backend reload for passive, disabled, or removed plugin");
                    }
                    Err(err) => {
                        warn!(plugin = %plugin_name, directory = %directory, error = %err, "debug watcher backend reload failed");
                    }
                },
                DirtyScope::Frontend => {
                    self.manager
                        .emit_frontend_reloaded(&plugin_name, &directory);
                    debug!(plugin = %plugin_name, directory = %directory, "debug watcher emitted frontend reload");
                }
            }
        }
    }
}

async fn run_watcher_loop(
    mut state: WatcherState,
    mut loader_events: broadcast::Receiver<LoaderEvent>,
    mut stop_rx: oneshot::Receiver<()>,
    #[cfg(target_os = "linux")] inotify_inst: inotify::Inotify,
    #[cfg(target_os = "linux")] mut linux_watches: linux::WatchIndex,
) {
    #[cfg(target_os = "linux")]
    let mut inotify_stream = {
        match inotify_inst.into_event_stream(vec![0u8; 8192]) {
            Ok(stream) => Some(stream),
            Err(err) => {
                warn!(error = %err, "failed to convert inotify into async stream; falling back to mtime sweep");
                None
            }
        }
    };

    let mut sweep_interval = tokio::time::interval(SWEEP_INTERVAL);
    sweep_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    sweep_interval.tick().await;

    loop {
        let debounce_sleep = async {
            match state.next_deadline() {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending::<()>().await,
            }
        };

        #[cfg(target_os = "linux")]
        let next_inotify = async {
            use inotify::StreamExt as _;
            match inotify_stream.as_mut() {
                Some(stream) => stream.next().await,
                None => std::future::pending().await,
            }
        };
        #[cfg(not(target_os = "linux"))]
        let next_inotify = std::future::pending::<Option<std::convert::Infallible>>();

        tokio::select! {
            _ = &mut stop_rx => break,
            () = debounce_sleep => {
                let due = state.take_due(Instant::now());
                state.flush_due(due).await;
                let changed = state.sync_with_manager(false);
                #[cfg(target_os = "linux")]
                if changed && let Some(stream) = inotify_stream.as_ref() {
                    let mut watches = stream.watches();
                    linux_watches.sync(&mut watches, &state.plugins_dir, &state.tracked);
                }
                #[cfg(not(target_os = "linux"))]
                let _ = changed;
            }
            _ = sweep_interval.tick() => {
                let changed = state.sync_with_manager(true);
                #[cfg(target_os = "linux")]
                if changed && let Some(stream) = inotify_stream.as_ref() {
                    let mut watches = stream.watches();
                    linux_watches.sync(&mut watches, &state.plugins_dir, &state.tracked);
                }
                #[cfg(not(target_os = "linux"))]
                let _ = changed;
            }
            ev = loader_events.recv() => {
                match ev {
                    Ok(LoaderEvent::PluginsChanged(_)) | Err(broadcast::error::RecvError::Lagged(_)) => {
                        let changed = state.sync_with_manager(false);
                        #[cfg(target_os = "linux")]
                        if changed && let Some(stream) = inotify_stream.as_ref() {
                            let mut watches = stream.watches();
                            linux_watches.sync(&mut watches, &state.plugins_dir, &state.tracked);
                        }
                        #[cfg(not(target_os = "linux"))]
                        let _ = changed;
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            inotify_ev = next_inotify => {
                #[cfg(target_os = "linux")]
                match inotify_ev {
                    Some(Ok(event)) => {
                        let resync = linux::handle_event(
                            &event,
                            &mut linux_watches,
                            &mut state,
                        );
                        if resync && let Some(stream) = inotify_stream.as_ref() {
                            let mut watches = stream.watches();
                            linux_watches.sync(&mut watches, &state.plugins_dir, &state.tracked);
                        }
                    }
                    Some(Err(err)) => {
                        warn!(error = %err, "inotify stream read error");
                    }
                    None => {
                        inotify_stream = None;
                    }
                }
                #[cfg(not(target_os = "linux"))]
                if let Some(impossible) = inotify_ev {
                    match impossible {}
                }
            }
        }
    }
}

fn is_ignored_name(name: &str) -> bool {
    name.starts_with('.')
        || name == "__pycache__"
        || Path::new(name)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("pyc"))
}

fn scan_backend_snapshot(plugin_dir: &Path) -> Snapshot {
    let mut snap = Snapshot::new();
    for rel in ["main.py", "plugin.json", "package.json"] {
        let path = plugin_dir.join(rel);
        if let Some(stamp) = file_stamp_if_regular(&path) {
            snap.insert(PathBuf::from(rel), stamp);
        }
    }
    let py_modules = plugin_dir.join("py_modules");
    collect_dir_snapshot(&py_modules, Path::new("py_modules"), &mut snap);
    snap
}

fn scan_frontend_snapshot(plugin_dir: &Path) -> Snapshot {
    let mut snap = Snapshot::new();
    let dist_dir = plugin_dir.join("dist");
    collect_dir_snapshot(&dist_dir, Path::new("dist"), &mut snap);
    snap
}

fn collect_dir_snapshot(dir: &Path, prefix: &Path, snap: &mut Snapshot) {
    let Ok(meta) = fs::symlink_metadata(dir) else {
        return;
    };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name_os = entry.file_name();
        let name = name_os.to_string_lossy();
        if is_ignored_name(&name) {
            continue;
        }
        let path = entry.path();
        let rel = prefix.join(&name_os);
        let Ok(entry_meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if entry_meta.file_type().is_symlink() {
            continue;
        }
        if entry_meta.is_dir() {
            collect_dir_snapshot(&path, &rel, snap);
        } else if entry_meta.is_file() {
            snap.insert(
                rel,
                FileStamp {
                    len: entry_meta.len(),
                    modified: entry_meta.modified().ok(),
                },
            );
        }
    }
}

fn file_stamp_if_regular(path: &Path) -> Option<FileStamp> {
    let meta = fs::symlink_metadata(path).ok()?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return None;
    }
    Some(FileStamp {
        len: meta.len(),
        modified: meta.modified().ok(),
    })
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::HashMap;
    use std::fs;
    use std::path::{Path, PathBuf};

    use inotify::{EventMask, EventOwned, Inotify, WatchDescriptor, WatchMask, Watches};
    use tracing::warn;

    use super::{DirtyScope, TrackedPlugin, WatcherState, is_ignored_name};
    use crate::CoreError;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum WatchScope {
        PluginRoot,
        PyModules,
        Dist,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) struct WatchTarget {
        pub directory: String,
        pub scope: WatchScope,
    }

    #[derive(Debug, Default)]
    pub(super) struct WatchIndex {
        by_wd: HashMap<WatchDescriptor, WatchTarget>,
        by_path: HashMap<PathBuf, WatchDescriptor>,
    }

    impl WatchIndex {
        pub(super) fn sync(
            &mut self,
            watches: &mut Watches,
            plugins_dir: &Path,
            tracked: &HashMap<String, TrackedPlugin>,
        ) {
            let mut desired: HashMap<PathBuf, WatchTarget> = HashMap::new();
            for directory in tracked.keys() {
                let plugin_root = plugins_dir.join(directory);
                if !is_real_dir(&plugin_root) {
                    continue;
                }
                desired.insert(
                    plugin_root.clone(),
                    WatchTarget {
                        directory: directory.clone(),
                        scope: WatchScope::PluginRoot,
                    },
                );
                collect_watch_dirs(
                    &plugin_root.join("py_modules"),
                    directory,
                    WatchScope::PyModules,
                    &mut desired,
                );
                collect_watch_dirs(
                    &plugin_root.join("dist"),
                    directory,
                    WatchScope::Dist,
                    &mut desired,
                );
            }

            let stale_paths: Vec<PathBuf> = self
                .by_path
                .keys()
                .filter(|path| !desired.contains_key(*path))
                .cloned()
                .collect();
            for path in stale_paths {
                if let Some(wd) = self.by_path.remove(&path) {
                    self.by_wd.remove(&wd);
                    let _ = watches.remove(wd);
                }
            }

            let mask = WatchMask::MODIFY
                | WatchMask::CLOSE_WRITE
                | WatchMask::CREATE
                | WatchMask::DELETE
                | WatchMask::MOVED_FROM
                | WatchMask::MOVED_TO
                | WatchMask::DELETE_SELF
                | WatchMask::MOVE_SELF
                | WatchMask::ONLYDIR;

            for (path, target) in desired {
                if let Some(wd) = self.by_path.get(&path) {
                    self.by_wd.insert(wd.clone(), target);
                    continue;
                }
                match watches.add(&path, mask) {
                    Ok(wd) => {
                        self.by_path.insert(path, wd.clone());
                        self.by_wd.insert(wd, target);
                    }
                    Err(err) => {
                        warn!(path = %path.display(), error = %err, "failed to add inotify watch");
                    }
                }
            }
        }

        fn remove_wd(&mut self, wd: &WatchDescriptor) {
            self.by_wd.remove(wd);
            self.by_path.retain(|_, existing| existing != wd);
        }
    }

    pub(super) fn init_inotify(
        plugins_dir: &Path,
        tracked: &HashMap<String, TrackedPlugin>,
    ) -> Result<(Inotify, WatchIndex), CoreError> {
        let inotify = Inotify::init()
            .map_err(|err| CoreError::Watcher(format!("failed to initialize inotify: {err}")))?;
        let mut index = WatchIndex::default();
        let mut watches = inotify.watches();
        index.sync(&mut watches, plugins_dir, tracked);
        Ok((inotify, index))
    }

    pub(super) fn handle_event(
        event: &EventOwned,
        index: &mut WatchIndex,
        state: &mut WatcherState,
    ) -> bool {
        if event.mask.contains(EventMask::Q_OVERFLOW) {
            state.sync_with_manager(true);
            return true;
        }

        let mut resync = false;
        if event.mask.contains(EventMask::IGNORED) {
            index.remove_wd(&event.wd);
            return true;
        }

        let Some(target) = index.by_wd.get(&event.wd).cloned() else {
            return false;
        };

        let name_str = event
            .name
            .as_ref()
            .map(|os| os.to_string_lossy().into_owned());
        if let Some(name) = name_str.as_deref()
            && is_ignored_name(name)
        {
            return false;
        }

        match target.scope {
            WatchScope::PluginRoot => {
                if event
                    .mask
                    .intersects(EventMask::DELETE_SELF | EventMask::MOVE_SELF)
                {
                    resync = true;
                }
                match name_str.as_deref() {
                    Some("main.py" | "plugin.json" | "package.json") => {
                        state.mark_dirty(&target.directory, DirtyScope::Backend);
                    }
                    Some("py_modules") => {
                        state.mark_dirty(&target.directory, DirtyScope::Backend);
                        resync = true;
                    }
                    Some("dist") => {
                        state.mark_dirty(&target.directory, DirtyScope::Frontend);
                        resync = true;
                    }
                    _ => {}
                }
            }
            WatchScope::PyModules => {
                state.mark_dirty(&target.directory, DirtyScope::Backend);
                if event.mask.contains(EventMask::ISDIR)
                    || event
                        .mask
                        .intersects(EventMask::DELETE_SELF | EventMask::MOVE_SELF)
                {
                    resync = true;
                }
            }
            WatchScope::Dist => {
                state.mark_dirty(&target.directory, DirtyScope::Frontend);
                if event.mask.contains(EventMask::ISDIR)
                    || event
                        .mask
                        .intersects(EventMask::DELETE_SELF | EventMask::MOVE_SELF)
                {
                    resync = true;
                }
            }
        }

        resync
    }

    fn is_real_dir(path: &Path) -> bool {
        fs::symlink_metadata(path).is_ok_and(|meta| !meta.file_type().is_symlink() && meta.is_dir())
    }

    fn collect_watch_dirs(
        dir: &Path,
        directory: &str,
        scope: WatchScope,
        out: &mut HashMap<PathBuf, WatchTarget>,
    ) {
        if !is_real_dir(dir) {
            return;
        }
        out.insert(
            dir.to_path_buf(),
            WatchTarget {
                directory: directory.to_owned(),
                scope,
            },
        );
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name_os = entry.file_name();
            let name = name_os.to_string_lossy();
            if is_ignored_name(&name) {
                continue;
            }
            let child = entry.path();
            if is_real_dir(&child) {
                collect_watch_dirs(&child, directory, scope, out);
            }
        }
    }
}
