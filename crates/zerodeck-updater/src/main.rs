//! `zerodeckctl` command-line entry point.

#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use semver::Version;
use url::Url;
use zerodeck_updater::{
    ACTIVATION_DEADLINE, APPLY_UNIT, ApplyRequestStore, AuthorizedRelease, Catalog, Channel,
    DAEMON_UNIT, GenerationStore, RecoveryMode, SelectionPolicy, StateStore, SystemdService,
    UpdateError, UpgradeOutcome, apply_pending, enqueue_rollback, enqueue_upgrade, recover,
    stage_release,
};

const DEFAULT_ROOT: &str = "/var/lib/zerodeck";
const DEFAULT_READY_URL: &str = "http://127.0.0.1:1337";
const APPLY_WAIT: Duration = Duration::from_secs(ACTIVATION_DEADLINE.as_secs() + 120);

#[derive(Debug, Parser)]
#[command(
    name = "zerodeckctl",
    version,
    about = "ZeroDeck managed loader controller"
)]
struct Cli {
    /// Root-owned update root (`current`, `generations/`, `state/`, `tuf/`).
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        env = "ZERODECK_UPDATE_ROOT",
        default_value = DEFAULT_ROOT
    )]
    root: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Verify the trusted catalog and stage the newest eligible release.
    Update {
        /// Signed loader channel.
        #[arg(
            long,
            default_value = "stable",
            value_parser = clap::builder::PossibleValuesParser::new(["stable", "testing"])
        )]
        channel: String,
        /// Report the selected release without downloading or staging it.
        #[arg(long)]
        check_only: bool,
    },
    /// Request privileged activation of the staged candidate.
    Upgrade {
        /// Explicitly start a stopped managed service (required for a first install).
        #[arg(long)]
        start: bool,
    },
    /// Print the redacted installed, staged, pending, and last-result summary.
    Status,
    /// Request offline restoration of the retained rollback target.
    Rollback,
    /// Internal: consume the single apply request (runs in `zerodeck-apply.service`).
    #[command(hide = true)]
    Apply,
    /// Internal: reconcile disk state after the apply unit exits.
    #[command(hide = true)]
    RecoverAfterApply,
    /// Internal: reconcile disk state once before the daemon starts on boot.
    #[command(hide = true)]
    RecoverBoot,
    /// Internal: exec the selected generation's daemon as systemd's main process.
    #[command(hide = true)]
    ExecCurrent,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(error) = run(cli).await {
        eprintln!("zerodeckctl: {error}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), UpdateError> {
    match cli.command {
        Command::Update {
            channel,
            check_only,
        } => run_update(&cli.root, &channel, check_only).await,
        Command::Upgrade { start } => run_upgrade(&cli.root, start).await,
        Command::Status => run_status(&cli.root),
        Command::Rollback => run_rollback(&cli.root).await,
        Command::Apply => run_apply(&cli.root).await,
        Command::RecoverAfterApply => {
            let (store, state, service) = open_runtime(&cli.root)?;
            recover(&store, &state, &service, RecoveryMode::AfterApply).await?;
            print!("{}", summary_line(&state.read()?));
            Ok(())
        }
        Command::RecoverBoot => {
            let (store, state, service) = open_runtime(&cli.root)?;
            recover(&store, &state, &service, RecoveryMode::BeforeBootStart).await?;
            print!("{}", summary_line(&state.read()?));
            Ok(())
        }
        Command::ExecCurrent => exec_current(&cli.root),
    }
}

async fn run_update(root: &Path, channel: &str, check_only: bool) -> Result<(), UpdateError> {
    let channel =
        Channel::parse(channel).ok_or_else(|| UpdateError::InvalidChannel(channel.to_owned()))?;
    fs::create_dir_all(root).map_err(|source| UpdateError::Io {
        path: root.to_path_buf(),
        source,
    })?;
    let store = GenerationStore::new(root)?;
    let state = state_store(root);
    let _guard = zerodeck_updater::acquire_maintenance_shared(&store)?;

    let metadata_url = environment_url("ZERODECK_UPDATE_METADATA_URL")?;
    let targets_url = environment_url("ZERODECK_UPDATE_TARGETS_URL")?;
    let trusted_root_path = std::env::var("ZERODECK_UPDATE_TRUSTED_ROOT")
        .map_err(|_| UpdateError::MissingEnvironment("ZERODECK_UPDATE_TRUSTED_ROOT"))?;
    let trusted_root = fs::read(&trusted_root_path).map_err(|source| UpdateError::Io {
        path: PathBuf::from(&trusted_root_path),
        source,
    })?;

    let catalog = Catalog::load(root, metadata_url, targets_url, &trusted_root).await?;
    let current = store.current_reference()?;
    let mut policy = SelectionPolicy::new(crate_version())
        .with_channel(channel)
        .with_prereleases(channel == Channel::Testing);
    if let Some(generation) = &current {
        let layout = store.verify_generation(generation)?;
        policy = policy.with_installed(layout.release().version().clone(), generation.clone());
    }

    let candidate = catalog.candidate(&policy)?;
    let Some(release) = candidate else {
        println!(
            "zerodeckctl: no newer {} release is available",
            channel_name(channel)
        );
        return Ok(());
    };
    if state.read()?.quarantined.contains(&release.digest) {
        return Err(UpdateError::Quarantined(release.digest));
    }
    if check_only {
        print_release("available", &release);
        return Ok(());
    }

    let digest = stage_release(&release, &store).await?;
    store.verify_generation(&digest)?;
    println!(
        "zerodeckctl: staged {digest} ({} {})",
        release.version,
        channel_name(release.channel)
    );
    Ok(())
}

async fn run_upgrade(root: &Path, start: bool) -> Result<(), UpdateError> {
    let (store, state, service) = open_runtime(root)?;
    match enqueue_upgrade(&store, &state, &service, start).await? {
        UpgradeOutcome::RequiresStart { candidate } => {
            println!(
                "zerodeckctl: candidate {candidate} is staged but the managed service is stopped; \
                 rerun with `--start` to activate it"
            );
            Ok(())
        }
        UpgradeOutcome::Requested { request } => {
            start_apply_and_report(&state, &request.generation).await
        }
    }
}

async fn run_rollback(root: &Path) -> Result<(), UpdateError> {
    let (store, state, _service) = open_runtime(root)?;
    let request = enqueue_rollback(&store, &state)?;
    start_apply_and_report(&state, &request.generation).await
}

fn run_status(root: &Path) -> Result<(), UpdateError> {
    let store = GenerationStore::new(root)?;
    let state = state_store(root);
    let persisted = state.read()?;
    let requests = ApplyRequestStore::for_state(&state);

    println!(
        "active: {}",
        store
            .current_reference()?
            .map_or_else(|| "none".to_owned(), |generation| generation.to_string())
    );
    println!(
        "rollback: {}",
        persisted
            .rollback_target
            .as_ref()
            .map_or_else(|| "none".to_owned(), ToString::to_string)
    );

    let catalog = Catalog::open_offline(root)?;
    match catalog.persisted_authorization()? {
        Some(release) => print_release("candidate", &release),
        None => println!("candidate: none"),
    }

    if persisted.quarantined.is_empty() {
        println!("quarantine: none");
    } else {
        println!("quarantine: {}", persisted.quarantined.len());
        for generation in &persisted.quarantined {
            println!("  {generation}");
        }
    }

    match &persisted.pending {
        Some(pending) => println!(
            "pending: {} candidate={} previous={} was_running={}",
            phase_name(pending.phase),
            pending.candidate,
            pending
                .previous
                .as_ref()
                .map_or_else(|| "none".to_owned(), ToString::to_string),
            pending.was_running
        ),
        None => println!("pending: none"),
    }
    match requests.read_optional() {
        Ok(Some(request)) => println!(
            "request: {} candidate={}",
            apply_kind_name(request.kind),
            request.generation
        ),
        Ok(None) => println!("request: none"),
        Err(_) => println!("request: invalid"),
    }
    println!(
        "last_result: {}",
        persisted.last_result.as_deref().unwrap_or("none")
    );
    println!(
        "last_error: {}",
        persisted.last_error.as_deref().unwrap_or("none")
    );
    Ok(())
}

async fn run_apply(root: &Path) -> Result<(), UpdateError> {
    let (store, state, service) = open_runtime(root)?;
    let catalog = Catalog::open_offline(root)?;
    apply_pending(&store, &state, &catalog, &service).await?;
    println!("{}", summary_line(&state.read()?));
    Ok(())
}

async fn start_apply_and_report(
    state: &StateStore,
    expected: &zerodeck_updater::GenerationId,
) -> Result<(), UpdateError> {
    let requests = ApplyRequestStore::for_state(state);
    let mut command = tokio::process::Command::new("systemctl");
    command
        .args(["start", "--wait", "--", APPLY_UNIT])
        .kill_on_drop(true);
    let systemctl_reason = match tokio::time::timeout(APPLY_WAIT, command.output()).await {
        Ok(Ok(output)) if output.status.success() => None,
        Ok(Ok(output)) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            Some(format!(
                "apply unit exited with {}: {stderr}",
                output.status
            ))
        }
        Ok(Err(error)) => Some(format!("cannot start {APPLY_UNIT}: {error}")),
        Err(_) => Some(format!("timed out waiting for {APPLY_UNIT}")),
    };

    let deadline = Instant::now() + APPLY_WAIT;
    loop {
        let persisted = state.read()?;
        let request_pending = requests.read_optional()?.is_some();
        if persisted.pending.is_none() && !request_pending {
            if persisted.last_confirmed.as_ref() == Some(expected) {
                println!("zerodeckctl: activated {expected}");
                return Ok(());
            }
            let reason = persisted
                .last_error
                .or(systemctl_reason)
                .unwrap_or_else(|| {
                    "apply completed without confirming the requested generation".to_owned()
                });
            return Err(UpdateError::ActivationFailed(reason));
        }
        if Instant::now() >= deadline {
            return Err(UpdateError::ActivationFailed(
                persisted.last_error.unwrap_or_else(|| {
                    format!("timed out waiting for {APPLY_UNIT} to reconcile {expected}")
                }),
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn exec_current(root: &Path) -> Result<(), UpdateError> {
    let store = GenerationStore::new(root)?;
    let generation = store.current()?.ok_or(UpdateError::NoActiveGeneration)?;
    let layout = store.verify_generation(&generation)?;
    let mut command = std::process::Command::new(layout.binary());
    command
        .arg("--managed-generation")
        .arg(layout.root())
        .env_remove("PYTHONPATH")
        .env_remove("ZERODECK_FRONTEND_DIR");

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = command.exec();
        Err(UpdateError::Io {
            path: layout.binary().to_path_buf(),
            source: error,
        })
    }
    #[cfg(not(unix))]
    {
        let status = command.status().map_err(|source| UpdateError::Io {
            path: layout.binary().to_path_buf(),
            source,
        })?;
        if status.success() {
            Ok(())
        } else {
            Err(UpdateError::ActivationFailed(format!(
                "managed daemon exited with {status}"
            )))
        }
    }
}

fn open_runtime(root: &Path) -> Result<(GenerationStore, StateStore, SystemdService), UpdateError> {
    let store = GenerationStore::new(root)?;
    let state = state_store(root);
    let service = managed_service()?;
    Ok((store, state, service))
}

fn state_store(root: &Path) -> StateStore {
    StateStore::new(root.join("state/update.json"))
}

fn managed_service() -> Result<SystemdService, UpdateError> {
    let endpoint =
        std::env::var("ZERODECK_READY_URL").unwrap_or_else(|_| DEFAULT_READY_URL.to_owned());
    let url = Url::parse(&endpoint).map_err(|error| UpdateError::InvalidReadyEndpoint {
        endpoint: endpoint.clone(),
        reason: error.to_string(),
    })?;
    Ok(SystemdService::new(DAEMON_UNIT, url))
}

fn environment_url(name: &'static str) -> Result<Url, UpdateError> {
    let value = std::env::var(name).map_err(|_| UpdateError::MissingEnvironment(name))?;
    Url::parse(&value).map_err(|error| UpdateError::InvalidEnvironmentUrl {
        name,
        reason: error.to_string(),
    })
}

fn crate_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("workspace version is SemVer")
}

fn print_release(label: &str, release: &AuthorizedRelease) {
    println!(
        "{label}: {} {} {} ({})",
        release.version,
        release.digest,
        release.build_id,
        channel_name(release.channel)
    );
}

fn summary_line(state: &zerodeck_updater::UpdateState) -> String {
    format!(
        "result: {} error: {}\n",
        state.last_result.as_deref().unwrap_or("none"),
        state.last_error.as_deref().unwrap_or("none")
    )
}

fn channel_name(channel: Channel) -> &'static str {
    match channel {
        Channel::Stable => "stable",
        Channel::Testing => "testing",
    }
}

fn phase_name(phase: zerodeck_updater::TransactionPhase) -> &'static str {
    match phase {
        zerodeck_updater::TransactionPhase::Prepared => "prepared",
        zerodeck_updater::TransactionPhase::Stopped => "stopped",
        zerodeck_updater::TransactionPhase::Switched => "switched",
        zerodeck_updater::TransactionPhase::Verifying => "verifying",
        zerodeck_updater::TransactionPhase::Confirmed => "confirmed",
        zerodeck_updater::TransactionPhase::Reverting => "reverting",
    }
}

fn apply_kind_name(kind: zerodeck_updater::ApplyKind) -> &'static str {
    match kind {
        zerodeck_updater::ApplyKind::Upgrade => "upgrade",
        zerodeck_updater::ApplyKind::Rollback => "rollback",
    }
}
