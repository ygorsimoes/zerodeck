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
    DAEMON_UNIT, GenerationId, GenerationStore, RecoveryMode, SelectionPolicy, StateStore,
    SystemdService, UpdateError, UpgradeOutcome, acquire_maintenance_exclusive, apply_pending,
    bootstrap_generation, enqueue_rollback, enqueue_upgrade, recover, recover_before_boot,
    stage_release,
};

const DEFAULT_ROOT: &str = "/var/lib/zerodeck";
const DEFAULT_READY_URL: &str = "http://127.0.0.1:1337";
const APPLY_WAIT: Duration = Duration::from_secs(ACTIVATION_DEADLINE.as_secs() + 120);
/// Distinct exit status for a staged candidate that needs an explicit `--start`.
const EXIT_REQUIRES_START: i32 = 75;

#[derive(Debug, Parser)]
#[command(
    name = "zerodeckctl",
    version,
    about = "ZeroDeck managed loader controller",
    after_help = "Exit codes: 0 success; 75 candidate staged but the managed service is stopped \
                  (rerun `upgrade --start`); 1 error."
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
    /// Administrative first install or conversion to an already extracted verified generation.
    Bootstrap {
        /// Generation digest extracted under the update root's `generations/` directory.
        #[arg(long)]
        generation: String,
        /// Retain an existing active generation as the rollback target.
        #[arg(long)]
        convert_existing_zerodeck: bool,
        /// Validate refusal conditions and print the plan without changing state.
        #[arg(long)]
        check: bool,
    },
    /// Remove a digest from the failed-trial quarantine after it was reviewed.
    Unquarantine {
        /// Quarantined generation digest.
        digest: String,
    },
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
        std::process::exit(exit_code(&error));
    }
}

fn exit_code(error: &UpdateError) -> i32 {
    if matches!(error, UpdateError::StartRequired) {
        EXIT_REQUIRES_START
    } else {
        1
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
        Command::Bootstrap {
            generation,
            convert_existing_zerodeck,
            check,
        } => run_bootstrap(&cli.root, &generation, convert_existing_zerodeck, check),
        Command::Unquarantine { digest } => run_unquarantine(&cli.root, &digest),
        Command::Apply => run_apply(&cli.root).await,
        Command::RecoverAfterApply => {
            let (store, state, service) = open_runtime(&cli.root)?;
            recover(&store, &state, &service, RecoveryMode::AfterApply).await?;
            print!("{}", summary_line(&state.read()?));
            Ok(())
        }
        Command::RecoverBoot => {
            // Boot recovery is disk-only: do not parse a readiness URL or construct an HTTP
            // client that the ordered daemon start does not need.
            let store = GenerationStore::new(&cli.root)?;
            let state = state_store(&cli.root);
            recover_before_boot(&store, &state).await?;
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

    let installed = installed_release(&store);
    let metadata_url = environment_url("ZERODECK_UPDATE_METADATA_URL")?;
    let targets_url = environment_url("ZERODECK_UPDATE_TARGETS_URL")?;
    let trusted_root_path = std::env::var("ZERODECK_UPDATE_TRUSTED_ROOT")
        .map_err(|_| UpdateError::MissingEnvironment("ZERODECK_UPDATE_TRUSTED_ROOT"))?;
    let trusted_root = fs::read(&trusted_root_path).map_err(|source| UpdateError::Io {
        path: PathBuf::from(&trusted_root_path),
        source,
    })?;

    let catalog = Catalog::load(root, metadata_url, targets_url, &trusted_root).await?;
    let mut policy = SelectionPolicy::new(crate_version())
        .with_channel(channel)
        .with_prereleases(channel == Channel::Testing);
    if let Some((version, generation)) = installed {
        policy = policy.with_installed(version, generation);
    }

    // `--check-only` must not supersede the authorization of the release already staged.
    let candidate = if check_only {
        catalog.preview_candidate(&policy)?
    } else {
        catalog.candidate(&policy)?
    };
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
            Err(UpdateError::StartRequired)
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

fn run_bootstrap(
    root: &Path,
    generation: &str,
    convert: bool,
    check: bool,
) -> Result<(), UpdateError> {
    let outcome = bootstrap_generation(root, generation, convert, check)?;
    if check {
        println!(
            "zerodeckctl: bootstrap check passed for {} (previous: {}, rollback: {})",
            outcome.generation,
            outcome
                .previous
                .map_or_else(|| "none".to_owned(), |value| value.to_string()),
            outcome
                .rollback_target
                .map_or_else(|| "none".to_owned(), |value| value.to_string())
        );
    } else if outcome.changed {
        println!(
            "zerodeckctl: bootstrapped {} (rollback: {})",
            outcome.generation,
            outcome
                .rollback_target
                .map_or_else(|| "none".to_owned(), |value| value.to_string())
        );
    } else {
        println!(
            "zerodeckctl: generation {} is already active",
            outcome.generation
        );
    }
    Ok(())
}

fn run_unquarantine(root: &Path, digest: &str) -> Result<(), UpdateError> {
    let generation = GenerationId::new(digest)?;
    let store = GenerationStore::new(root)?;
    let _guard = acquire_maintenance_exclusive(&store)?;
    if !store.generation_dir(&generation).is_dir() {
        return Err(UpdateError::MissingGeneration(generation));
    }
    let state = state_store(root);
    let mut persisted = state.read()?;
    if persisted.quarantined.contains(&generation) {
        persisted.quarantined.retain(|entry| entry != &generation);
        state.write(&persisted)?;
        println!("zerodeckctl: unquarantined {generation}");
    } else {
        // Idempotent for a known installed generation; a digest with no generation directory was
        // rejected above as unknown.
        println!("zerodeckctl: {generation} is not quarantined");
    }
    Ok(())
}

fn run_status(root: &Path) -> Result<(), UpdateError> {
    let store = GenerationStore::open_existing(root).map_err(status_unavailable)?;
    let state = state_store(root);
    let persisted = state.read().map_err(status_unavailable)?;
    let requests = ApplyRequestStore::for_state(&state);

    let active = store.current_reference().map_err(status_unavailable)?;
    println!(
        "active: {}",
        active.map_or_else(|| "none".to_owned(), |generation| generation.to_string())
    );
    println!(
        "rollback: {}",
        persisted
            .rollback_target
            .as_ref()
            .map_or_else(|| "none".to_owned(), ToString::to_string)
    );

    match Catalog::open_offline(root).and_then(|catalog| catalog.persisted_authorization()) {
        Ok(Some(release)) => print_release("candidate", &release),
        Ok(None) => println!("candidate: none"),
        Err(_) => println!("candidate: unknown"),
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
        redact_status_text(persisted.last_result.as_deref().unwrap_or("none"), root)
    );
    println!(
        "last_error: {}",
        redact_status_text(persisted.last_error.as_deref().unwrap_or("none"), root)
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

fn installed_release(store: &GenerationStore) -> Option<(Version, zerodeck_updater::GenerationId)> {
    let generation = match store.current_reference() {
        Ok(Some(generation)) => generation,
        Ok(None) => return None,
        Err(_) => {
            eprintln!(
                "zerodeckctl: warning: the active generation reference is unreadable; treating \
                 the installed version as unknown"
            );
            return None;
        }
    };
    if let Ok(layout) = store.verify_generation(&generation) {
        Some((layout.release().version().clone(), generation))
    } else {
        eprintln!(
            "zerodeckctl: warning: active generation {generation} is corrupt or incomplete; \
             treating the installed version as unknown"
        );
        None
    }
}

fn status_unavailable(_error: UpdateError) -> UpdateError {
    UpdateError::ActivationFailed("status is unavailable".to_owned())
}

fn redact_status_text(value: &str, root: &Path) -> String {
    let root_text = root.to_string_lossy();
    let replaced = value.replace(&*root_text, "<root>");
    replaced
        .split_whitespace()
        .map(|token| {
            if looks_like_path(token) {
                "<path>".to_owned()
            } else {
                token.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn looks_like_path(token: &str) -> bool {
    let token = token.trim_matches(|character: char| {
        matches!(
            character,
            '`' | '\'' | '"' | ':' | ',' | ';' | '(' | ')' | '[' | ']'
        )
    });
    token.starts_with('/')
        || token.starts_with("./")
        || token.starts_with("../")
        || token.starts_with("~/")
        || token.starts_with("<root>")
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

#[cfg(test)]
mod tests {
    use super::{EXIT_REQUIRES_START, exit_code};
    use zerodeck_updater::UpdateError;

    #[test]
    fn requires_start_has_a_distinct_exit_code() {
        assert_eq!(exit_code(&UpdateError::StartRequired), EXIT_REQUIRES_START);
        assert_eq!(EXIT_REQUIRES_START, 75);
        assert_eq!(exit_code(&UpdateError::Busy), 1);
    }
}
