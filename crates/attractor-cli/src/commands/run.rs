use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow;
use attractor_journal::{AttemptEndReason, EventData, IndexEntry, PipelineDir, RunMeta};

use super::run_lock::{LockError, RunLock, WORKTREE_LOCK};

/// Print a human-facing line: to stdout normally, to stderr in `--json` mode,
/// where stdout carries only the machine-readable first line (C6).
macro_rules! say {
    ($json:expr, $($arg:tt)*) => {
        if $json {
            eprintln!($($arg)*)
        } else {
            println!($($arg)*)
        }
    };
}

/// How this `pas run` process was invoked, beyond the Pipeline options.
#[derive(Debug, Clone, Default)]
pub struct RunInvocation {
    /// The process argv, recorded in `run.json` and `AttemptStarted`.
    pub argv: Vec<String>,
    /// `--run-id`, unvalidated.
    pub run_id: Option<String>,
    /// `--json`: first stdout line is `{"v":1,"ok":true,"run_id","run_dir"}`.
    pub json: bool,
    /// Run Index file; `None` uses the machine-wide Index (C4).
    pub index_path: Option<PathBuf>,
    /// Time between Heartbeat Events; `None` uses [`HEARTBEAT_INTERVAL`].
    /// Only tests set it (`PAS_HEARTBEAT_INTERVAL_MS`).
    pub heartbeat_interval: Option<Duration>,
    /// `--allow-shared-workdir`: start even if another Run holds the
    /// Worktree lock, and record `shared_workdir: true` in `RunStarted`.
    pub allow_shared_workdir: bool,
}

/// Time between Heartbeat Events while an Attempt runs (C3).
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Env var that shortens the Heartbeat interval, for tests only.
const HEARTBEAT_INTERVAL_ENV: &str = "PAS_HEARTBEAT_INTERVAL_MS";

/// The Heartbeat interval set by `PAS_HEARTBEAT_INTERVAL_MS`, if any.
pub fn heartbeat_interval_from_env() -> Option<Duration> {
    let value = std::env::var(HEARTBEAT_INTERVAL_ENV).ok()?;
    let interval = parse_heartbeat_interval_ms(&value);
    if interval.is_none() {
        tracing::warn!(
            value,
            "ignoring {HEARTBEAT_INTERVAL_ENV}: expected a positive number of milliseconds"
        );
    }
    interval
}

/// A positive whole number of milliseconds; anything else is `None`.
fn parse_heartbeat_interval_ms(value: &str) -> Option<Duration> {
    match value.trim().parse::<u64>() {
        Ok(ms) if ms > 0 => Some(Duration::from_millis(ms)),
        _ => None,
    }
}

/// Writes `Heartbeat{pid}` to the Run Journal every `interval` while the
/// Attempt runs, the first one `interval` after it starts. Dropping it aborts
/// the task; [`Heartbeat::stop`] also waits for it, so no Heartbeat can land
/// after `AttemptEnded`.
struct Heartbeat(Option<tokio::task::JoinHandle<()>>);

impl Heartbeat {
    fn start(journal: Arc<attractor_journal::JournalWriter>, pid: u32, interval: Duration) -> Self {
        Self(Some(tokio::spawn(async move {
            let first = tokio::time::Instant::now() + interval;
            let mut ticks = tokio::time::interval_at(first, interval);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticks.tick().await;
                if let Err(error) = journal.append(EventData::Heartbeat { pid }) {
                    tracing::warn!(
                        path = %journal.path().display(),
                        %error,
                        "cannot write Heartbeat to the Run Journal"
                    );
                }
            }
        })))
    }

    async fn stop(mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct CodergenClaudeCliOpts {
    pub settings_mode: Option<String>,
    pub setting_sources: Option<String>,
    pub settings: Option<String>,
    pub tools: Option<String>,
    pub agents: Option<String>,
    pub plugin_dirs: Vec<PathBuf>,
    pub mcp_config: Option<String>,
}

impl CodergenClaudeCliOpts {
    fn to_execution_options(&self) -> anyhow::Result<attractor_pipeline::ClaudeExecutionOptions> {
        let settings_mode = self
            .settings_mode
            .as_deref()
            .map(attractor_pipeline::ClaudeSettingsMode::from_str)
            .transpose()
            .map_err(anyhow::Error::msg)?;
        let setting_sources = self
            .setting_sources
            .as_deref()
            .map(|sources| {
                sources
                    .split(',')
                    .map(str::trim)
                    .filter(|source| !source.is_empty())
                    .map(attractor_pipeline::ClaudeSettingSource::from_str)
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()
            .map_err(anyhow::Error::msg)?;
        Ok(attractor_pipeline::ClaudeExecutionOptions {
            settings_mode,
            setting_sources,
            settings: self.settings.clone(),
            tools: self.tools.clone(),
            agents: self.agents.clone(),
            plugin_dirs: (!self.plugin_dirs.is_empty()).then(|| self.plugin_dirs.clone()),
            mcp_config: self.mcp_config.clone(),
        })
    }
}

/// Generate a deterministic logs directory name from the pipeline file path.
/// Format: `.pas/logs/<stem>-<8hex>` e.g. `.pas/logs/phase-01-spec-a3f1b2c9`
///
/// The hash is derived from the canonical file path so re-running the same
/// pipeline always finds the same logs dir (and its checkpoint).
/// FNV-1a 32-bit hash — deterministic across Rust versions and platforms.
fn fnv1a32(bytes: &[u8]) -> u32 {
    const OFFSET: u32 = 2166136261;
    const PRIME: u32 = 16777619;
    bytes
        .iter()
        .fold(OFFSET, |acc, &b| (acc ^ (b as u32)).wrapping_mul(PRIME))
}

fn stable_logs_dir(pipeline_path: &std::path::Path) -> PathBuf {
    let stem = pipeline_path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();

    // Hash the canonical path for deterministic directory across runs
    let canonical =
        std::fs::canonicalize(pipeline_path).unwrap_or_else(|_| pipeline_path.to_path_buf());
    let hash = fnv1a32(canonical.to_string_lossy().as_bytes());

    PathBuf::from(format!(".pas/logs/{}-{:08x}", stem, hash))
}

/// Print a highlighted resume banner built from real checkpoint data.
///
/// Deliberately omits a "completed N of M" figure: `completed_nodes` is
/// cleared on every `loop_restart` edge (see `checkpoint.rs`), so looping
/// pipelines -- e.g. the beads epic-runner template, which restarts its
/// task-picking loop after each closed task -- have no stable total to
/// report against. `step_count` and `total_cost` survive loop restarts, so
/// those are used instead as honest progress signals.
fn print_resume_banner(cp: &attractor_pipeline::PipelineCheckpoint, json: bool) {
    let age = chrono::DateTime::parse_from_rfc3339(&cp.timestamp)
        .ok()
        .map(|saved| {
            let secs = (chrono::Utc::now() - saved.with_timezone(&chrono::Utc))
                .num_seconds()
                .max(0);
            format_elapsed(secs)
        });

    let mut detail = format!("{} step(s) run so far", cp.step_count);
    if cp.total_cost > 0.0 {
        detail.push_str(&format!(", ${:.4} spent so far", cp.total_cost));
    }
    if let Some(age) = age {
        detail.push_str(&format!(", saved {age} ago"));
    }

    print_highlighted(
        &[
            format!(
                "Resuming from checkpoint -- next node: {}",
                cp.current_node_id
            ),
            detail,
        ],
        json,
    );
}

fn format_elapsed(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Print lines inside a border, bolded and colored when the output is a
/// terminal. Falls back to a plain ASCII box (no ANSI codes) when output is
/// redirected, so logs and CI output stay clean.
fn print_highlighted(lines: &[String], json: bool) {
    use std::io::IsTerminal;

    let color = if json {
        std::io::stderr().is_terminal()
    } else {
        std::io::stdout().is_terminal()
    };
    let (bold, cyan, reset) = if color {
        ("\x1b[1m", "\x1b[36m", "\x1b[0m")
    } else {
        ("", "", "")
    };

    let width = lines
        .iter()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(0)
        .max(20);
    let border = "-".repeat(width + 4);

    say!(json, "{cyan}+{border}+{reset}");
    for line in lines {
        let pad = " ".repeat(width - line.chars().count());
        say!(
            json,
            "{cyan}|{reset}  {bold}{line}{pad}{reset}  {cyan}|{reset}"
        );
    }
    say!(json, "{cyan}+{border}+{reset}");
}

fn prepare_run_configuration(
    path: &std::path::Path,
    workdir: Option<&std::path::Path>,
    dry_run: bool,
    max_budget_usd: Option<f64>,
    max_steps: Option<u64>,
    codergen_claude: &CodergenClaudeCliOpts,
    json: bool,
) -> anyhow::Result<attractor_pipeline::RunConfiguration> {
    let graph = crate::load_pipeline(path)?;
    let plan = match attractor_pipeline::ExecutionPlan::compile(graph.clone()) {
        Ok(plan) => plan,
        Err(error) => {
            let diagnostics = attractor_pipeline::validate(&graph);
            super::print_diagnostics_to(&diagnostics, json);
            anyhow::bail!("Pipeline validation failed: {error}");
        }
    };
    let mut diagnostics = attractor_pipeline::validate_plan(&plan);
    // A dry run never calls bd, so only a real Run needs it on PATH.
    if !dry_run {
        diagnostics.extend(attractor_pipeline::validate_beads_available(&plan));
    }
    if super::print_diagnostics_to(&diagnostics, json) {
        anyhow::bail!("Pipeline validation failed");
    }

    attractor_pipeline::RunConfiguration::prepare(
        plan,
        attractor_pipeline::ExecutionOptions {
            dry_run: dry_run.then_some(true),
            max_steps,
            max_budget_usd,
            workdir: workdir.map(std::path::Path::to_path_buf),
            claude: codergen_claude.to_execution_options()?,
            ..Default::default()
        },
    )
    .map_err(anyhow::Error::msg)
}

/// A `pas run` failure that happened before the Run could be reported. In
/// `--json` mode it is printed as `{"v":1,"ok":false,"error":{..}}` (C6).
#[derive(Debug)]
struct SetupError {
    code: &'static str,
    message: String,
    /// Process exit code; 1 unless the Run was refused by a lock (C5).
    exit_code: i32,
}

impl SetupError {
    fn new(code: &'static str, message: impl std::fmt::Display) -> Self {
        Self {
            code,
            message: message.to_string(),
            exit_code: 1,
        }
    }

    fn refused(code: &'static str, exit_code: i32, message: impl std::fmt::Display) -> Self {
        Self {
            exit_code,
            ..Self::new(code, message)
        }
    }
}

/// Exit code when another Run holds the Pipeline lock (C5).
pub const EXIT_PIPELINE_LOCKED: i32 = 5;
/// Exit code when another Run holds the Worktree lock (C5).
pub const EXIT_WORKTREE_LOCKED: i32 = 6;

/// `pas run` refused to start because another Run holds a lock. `main`
/// prints `error: <message>` and exits with `exit_code`.
#[derive(Debug)]
pub struct RunRefused {
    pub exit_code: i32,
    pub message: String,
}

impl std::fmt::Display for RunRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RunRefused {}

/// The locks one Attempt holds (C5). Dropping them releases both.
#[derive(Debug)]
struct RunLocks {
    pipeline: RunLock,
    worktree: Option<RunLock>,
}

impl RunLocks {
    /// Write this process's PID and `run_id` into every held lock file.
    fn record(&self, run_id: Option<&str>) {
        for lock in std::iter::once(&self.pipeline).chain(&self.worktree) {
            if let Err(error) = lock.record(run_id) {
                tracing::warn!(
                    path = %lock.path().display(),
                    %error,
                    "cannot write the lock holder"
                );
            }
        }
    }
}

/// Take the Pipeline lock on `<logs_dir>/run.lock`, then the Worktree lock on
/// `<git-dir>/pas-run.lock` when `workdir` is in a git worktree. Returns the
/// locks and whether the Run shares its worktree with another Run.
fn acquire_run_locks(
    logs_dir: &std::path::Path,
    workdir: &std::path::Path,
    allow_shared_workdir: bool,
) -> Result<(RunLocks, bool), SetupError> {
    let setup = |e: &dyn std::fmt::Display| SetupError::new("run_setup_failed", e);
    std::fs::create_dir_all(logs_dir).map_err(|e| {
        setup(&format!(
            "cannot create Pipeline folder {}: {e}",
            logs_dir.display()
        ))
    })?;
    let pipeline_path = PipelineDir::new(absolute(logs_dir)).run_lock();
    let pipeline = match RunLock::try_acquire(&pipeline_path) {
        Ok(lock) => lock,
        Err(LockError::Busy(holder)) => {
            return Err(SetupError::refused(
                "pipeline_locked",
                EXIT_PIPELINE_LOCKED,
                format!("pipeline already running ({holder})"),
            ))
        }
        Err(LockError::Io(e)) => {
            return Err(setup(&format!(
                "cannot lock {}: {e}",
                pipeline_path.display()
            )))
        }
    };
    let mut locks = RunLocks {
        pipeline,
        worktree: None,
    };
    locks.record(None);

    // Not a git worktree, or no `git`: only the Pipeline lock.
    let Some(git_dir) = git_rev_parse(workdir, "--absolute-git-dir") else {
        return Ok((locks, false));
    };
    let worktree_path = PathBuf::from(git_dir).join(WORKTREE_LOCK);
    let shared = match RunLock::try_acquire(&worktree_path) {
        Ok(lock) => {
            lock.record(None)
                .unwrap_or_else(|error| tracing::warn!(%error, "cannot write the lock holder"));
            locks.worktree = Some(lock);
            false
        }
        Err(LockError::Busy(holder)) if allow_shared_workdir => {
            eprintln!("warning: sharing this git worktree with another Run ({holder})");
            true
        }
        Err(LockError::Busy(holder)) => {
            return Err(SetupError::refused(
                "worktree_locked",
                EXIT_WORKTREE_LOCKED,
                format!(
                    "another Run is active in this git worktree ({holder}); pass --allow-shared-workdir to run anyway"
                ),
            ))
        }
        Err(LockError::Io(e)) => {
            return Err(setup(&format!(
                "cannot lock {}: {e}",
                worktree_path.display()
            )))
        }
    };
    Ok((locks, shared))
}

/// Which Run this `pas run` works on (spec C2).
#[derive(Debug, Clone, PartialEq, Eq)]
enum RunIdentity {
    /// A new Run: a new `run.json`, Index line, and `RunStarted`.
    New(String),
    /// Another Attempt of the checkpoint's Run.
    Resume(String),
}

impl RunIdentity {
    fn id(&self) -> &str {
        match self {
            Self::New(id) | Self::Resume(id) => id,
        }
    }
}

/// Decide the Run ID from the checkpoint's `run_id` (already validated) and
/// `--run-id` (already validated). A checkpoint resumes its own Run, so a
/// different `--run-id` is refused rather than silently renaming the Run.
fn decide_run_id(
    checkpoint_run_id: Option<&str>,
    requested: Option<&str>,
) -> Result<RunIdentity, SetupError> {
    match (checkpoint_run_id, requested) {
        (Some(cp), Some(req)) if cp != req => Err(SetupError::new(
            "run_id_mismatch",
            format!(
                "the checkpoint belongs to Run {cp}; pass --run-id {cp} to resume it or --fresh to start a new Run"
            ),
        )),
        (Some(cp), _) => Ok(RunIdentity::Resume(cp.to_string())),
        (None, Some(req)) => Ok(RunIdentity::New(req.to_string())),
        (None, None) => Ok(RunIdentity::New(attractor_journal::new_run_id())),
    }
}

/// How an Attempt ended, before it is mapped to an `AttemptEnded` reason.
enum AttemptOutcome {
    Finished(attractor_types::Result<attractor_pipeline::PipelineResult>),
    Terminated,
}

/// The `AttemptEnded` reason and message for an Attempt's outcome (C3).
fn attempt_end_reason(outcome: &AttemptOutcome) -> (AttemptEndReason, Option<String>) {
    use attractor_types::AttractorError;
    match outcome {
        AttemptOutcome::Finished(Ok(_)) => (AttemptEndReason::Completed, None),
        AttemptOutcome::Finished(Err(error)) => {
            let reason = match error {
                AttractorError::BudgetExhausted { .. } => AttemptEndReason::BudgetExhausted,
                AttractorError::MaxStepsExceeded { .. } => AttemptEndReason::MaxSteps,
                _ => AttemptEndReason::Failed,
            };
            (reason, Some(error.to_string()))
        }
        AttemptOutcome::Terminated => (AttemptEndReason::Stopped, Some("SIGTERM".to_string())),
    }
}

/// The highest `attempt` recorded in a journal, or 0 if there is none.
/// Unreadable lines are skipped: they must not make a Run unresumable.
fn last_attempt(events: &std::path::Path) -> u32 {
    let Ok(bytes) = std::fs::read(events) else {
        return 0;
    };
    bytes
        .split(|&b| b == b'\n')
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .filter_map(|event| event.get("attempt")?.as_u64())
        .max()
        .map_or(0, |attempt| u32::try_from(attempt).unwrap_or(u32::MAX))
}

/// `git -C <dir> rev-parse <args>`, or `None` outside a git worktree.
fn git_rev_parse(dir: &std::path::Path, arg: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", arg])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let value = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (output.status.success() && !value.is_empty()).then_some(value)
}

fn absolute(path: &std::path::Path) -> PathBuf {
    std::fs::canonicalize(path)
        .or_else(|_| std::path::absolute(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

/// Resolves once the process receives SIGTERM. The handler is installed when
/// this is called, so a signal that arrives before the future is polled is
/// not lost.
#[cfg(unix)]
fn sigterm() -> std::io::Result<impl std::future::Future<Output = ()>> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut signal = signal(SignalKind::terminate())?;
    Ok(async move {
        signal.recv().await;
    })
}

#[cfg(not(unix))]
fn sigterm() -> std::io::Result<impl std::future::Future<Output = ()>> {
    Ok(std::future::pending())
}

/// Report a failure that happened before the `--json` first line.
fn setup_failed(json: bool, error: SetupError) -> anyhow::Error {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "v": 1,
                "ok": false,
                "error": {"code": error.code, "message": error.message},
            })
        );
    }
    if error.exit_code != 1 {
        return anyhow::Error::new(RunRefused {
            exit_code: error.exit_code,
            message: error.message,
        });
    }
    anyhow::anyhow!(error.message)
}

/// Everything `cmd_run` sets up before the engine runs.
struct PreparedRun {
    configured: attractor_pipeline::RunConfiguration,
    logs_dir: PathBuf,
    checkpoint: Option<attractor_pipeline::PipelineCheckpoint>,
    run_id: String,
    run_dir: attractor_journal::RunDir,
    journal: Arc<attractor_journal::JournalWriter>,
    attempt: u32,
    locks: RunLocks,
}

#[allow(clippy::too_many_arguments)]
pub async fn cmd_run(
    path: &std::path::Path,
    workdir: Option<&std::path::Path>,
    logs: Option<&std::path::Path>,
    dry_run: bool,
    max_budget_usd: Option<f64>,
    max_steps: Option<u64>,
    fresh: bool,
    codergen_claude: &CodergenClaudeCliOpts,
    invocation: &RunInvocation,
) -> anyhow::Result<()> {
    let json = invocation.json;
    // Installed before `AttemptStarted` is written, so every Attempt the
    // journal records can end with `AttemptEnded{stopped}`.
    let terminated = sigterm().map_err(|e| {
        setup_failed(
            json,
            SetupError::new("run_setup_failed", format!("cannot handle SIGTERM: {e}")),
        )
    })?;
    let PreparedRun {
        configured,
        logs_dir,
        checkpoint,
        run_id,
        run_dir,
        journal,
        attempt,
        locks,
    } = prepare_run(
        path,
        workdir,
        logs,
        dry_run,
        max_budget_usd,
        max_steps,
        fresh,
        codergen_claude,
        invocation,
    )
    .await
    .map_err(|error| setup_failed(json, error))?;
    // Started right after `AttemptStarted`; stopped before `AttemptEnded`.
    let heartbeat = Heartbeat::start(
        journal.clone(),
        std::process::id(),
        invocation.heartbeat_interval.unwrap_or(HEARTBEAT_INTERVAL),
    );

    if json {
        println!(
            "{}",
            serde_json::json!({
                "v": 1,
                "ok": true,
                "run_id": run_id,
                "run_dir": run_dir.path(),
            })
        );
        use std::io::Write;
        std::io::stdout().flush()?;
    }

    let graph = configured.plan().graph();
    say!(json, "Running pipeline: {}", graph.name);
    if !graph.goal.is_empty() {
        say!(json, "Goal: {}", graph.goal);
    }
    say!(json, "Logs: {}", logs_dir.display());
    say!(json, "Run: {run_id}");
    if let Some(cp) = &checkpoint {
        print_resume_banner(cp, json);
    }
    if *configured.controls().dry_run().value() {
        say!(json, "(dry run mode -- no LLM calls)");
    }
    if *configured.controls().claude().settings_mode().value()
        == attractor_pipeline::ClaudeSettingsMode::Inherit
    {
        say!(
            json,
            "WARNING: codergen Claude settings inheritance enabled; personal Claude Code hooks/settings may run."
        );
    }

    say!(
        json,
        "Working directory: {}",
        configured.controls().workdir().value().display()
    );
    if configured.controls().max_budget_usd().source()
        == attractor_pipeline::ConfigurationSource::Caller
    {
        let budget = configured.controls().max_budget_usd().value();
        say!(json, "Budget limit: ${:.2}", budget);
    }
    say!(
        json,
        "Step limit: {}",
        configured.controls().max_steps().value()
    );

    let interviewer = std::sync::Arc::new(attractor_pipeline::ConsoleInterviewer);
    let registry = attractor_pipeline::default_registry_with_interviewer(interviewer);
    let executor =
        attractor_pipeline::PipelineExecutor::new(registry).with_journal(journal.clone());
    let outcome = {
        let run = executor.run_configuration_with_checkpoint(
            &configured,
            attractor_types::Context::new(),
            &logs_dir,
        );
        tokio::select! {
            result = run => AttemptOutcome::Finished(result),
            () = terminated => AttemptOutcome::Terminated,
        }
        // The engine future is dropped here; dropping it kills the current
        // stage's child process group.
    };

    // Wait for the task, not just abort it: a Heartbeat already being
    // written on another thread must land before `AttemptEnded`.
    heartbeat.stop().await;
    let (reason, message) = attempt_end_reason(&outcome);
    if let Err(error) = journal.append(EventData::AttemptEnded {
        attempt,
        reason,
        message,
    }) {
        tracing::error!(
            path = %journal.path().display(),
            %error,
            "cannot write AttemptEnded to the Run Journal"
        );
    }
    // Held until the Attempt has ended, so this process stays the only
    // journal writer (C3). On SIGTERM the OS releases them at exit.
    drop(locks);

    let result = match outcome {
        AttemptOutcome::Finished(result) => result?,
        AttemptOutcome::Terminated => {
            eprintln!("Stopped by SIGTERM (Run {run_id})");
            std::process::exit(143);
        }
    };

    say!(json, "\nPipeline completed");
    say!(json, "Completed nodes: {:?}", result.completed_nodes);

    // Print cost summary
    if result.total_cost > 0.0 {
        say!(json, "Total cost: ${:.4}", result.total_cost);
    }

    Ok(())
}

/// Validate the invocation and the plan, decide the Run, create its folder,
/// `run.json`, and Index entry, open the journal, and record the Attempt's
/// start. Nothing but the lock files is written to disk until every check
/// has passed.
#[allow(clippy::too_many_arguments)]
async fn prepare_run(
    path: &std::path::Path,
    workdir: Option<&std::path::Path>,
    logs: Option<&std::path::Path>,
    dry_run: bool,
    max_budget_usd: Option<f64>,
    max_steps: Option<u64>,
    fresh: bool,
    codergen_claude: &CodergenClaudeCliOpts,
    invocation: &RunInvocation,
) -> Result<PreparedRun, SetupError> {
    let json = invocation.json;
    let requested = match invocation.run_id.as_deref() {
        Some(raw) => Some(attractor_journal::parse_run_id(raw).ok_or_else(|| {
            SetupError::new(
                "invalid_run_id",
                format!("invalid --run-id {raw:?}: expected a UUID"),
            )
        })?),
        None => None,
    };

    let configured = prepare_run_configuration(
        path,
        workdir,
        dry_run,
        max_budget_usd,
        max_steps,
        codergen_claude,
        json,
    )
    .map_err(|e| SetupError::new("invalid_pipeline", e))?;

    // Preflight checks: environment-level warnings that don't fail validation
    // but can cause silent problems at runtime (e.g. a codergen node with no
    // timeout falling back to the hardcoded 600s kill).
    for finding in attractor_pipeline::preflight_run_configuration(&configured) {
        let severity = match finding.severity {
            attractor_pipeline::PreflightSeverity::Warn => "WARN",
            attractor_pipeline::PreflightSeverity::Error => "ERROR",
        };
        say!(json, "[{}] {}: {}", severity, finding.code, finding.message);
        if let Some(suggestion) = &finding.suggestion {
            say!(json, "  suggestion: {}", suggestion);
        }
    }

    let setup = |e: &dyn std::fmt::Display| SetupError::new("run_setup_failed", e);
    let index_path = match &invocation.index_path {
        Some(path) => path.clone(),
        None => attractor_journal::index_path().map_err(|e| setup(&e))?,
    };

    // Resolve logs directory: explicit flag or deterministic from path
    let logs_dir = match logs {
        Some(l) => l.to_path_buf(),
        None => stable_logs_dir(path),
    };

    // Locks come before the checkpoint is read or anything is written, so a
    // refused Run leaves no trace (C5).
    let workdir_abs = absolute(configured.controls().workdir().value());
    let (locks, shared_workdir) =
        acquire_run_locks(&logs_dir, &workdir_abs, invocation.allow_shared_workdir)?;

    // Check for existing checkpoint. Loaded (not just existence-checked) so
    // the resume banner can show real progress instead of a bare notice.
    // --fresh ignores it; it is cleared once the Run has been decided.
    let checkpoint = if fresh {
        None
    } else {
        attractor_pipeline::load_checkpoint(&logs_dir)
            .await
            .map_err(|e| setup(&e))?
    };
    let checkpoint_run_id = checkpoint
        .as_ref()
        .and_then(|cp| cp.run_id.as_deref())
        .and_then(|raw| {
            let id = attractor_journal::parse_run_id(raw);
            if id.is_none() {
                tracing::warn!(run_id = raw, "ignoring invalid run_id in checkpoint");
            }
            id
        });
    let identity = decide_run_id(checkpoint_run_id.as_deref(), requested.as_deref())?;
    let run_id = identity.id().to_string();
    locks.record(Some(&run_id));
    let run_dir = PipelineDir::new(absolute(&logs_dir))
        .run(&run_id)
        .map_err(|e| setup(&e))?;
    let is_new_run = !run_dir.run_json().exists();
    if matches!(identity, RunIdentity::New(_)) && !is_new_run {
        return Err(SetupError::new(
            "run_exists",
            format!(
                "Run {run_id} already exists at {}",
                run_dir.path().display()
            ),
        ));
    }

    // --fresh: clear any existing checkpoint before starting
    if fresh {
        attractor_pipeline::clear_checkpoint(&logs_dir)
            .await
            .map_err(|e| setup(&e))?;
    }

    run_dir.create_all().map_err(|e| {
        setup(&format!(
            "cannot create Run folder {}: {e}",
            run_dir.path().display()
        ))
    })?;
    let attempt = last_attempt(&run_dir.events()) + 1;
    let pipeline_path = absolute(path);
    let pas_version = env!("CARGO_PKG_VERSION").to_string();

    if is_new_run {
        let started_at = chrono::Utc::now();
        let meta = RunMeta {
            v: RunMeta::VERSION,
            run_id: run_id.clone(),
            pipeline_path: pipeline_path.clone(),
            pipeline_name: configured.plan().graph().name.clone(),
            workdir: workdir_abs.clone(),
            git_worktree: git_rev_parse(&workdir_abs, "--show-toplevel").map(PathBuf::from),
            logs_dir: absolute(&logs_dir),
            started_at,
            argv: invocation.argv.clone(),
            pas_version: pas_version.clone(),
            epic_id: None,
        };
        attractor_journal::write_run_meta(run_dir.path(), &meta).map_err(|e| {
            setup(&format!(
                "cannot write {}: {e}",
                run_dir.run_json().display()
            ))
        })?;
        let entry = IndexEntry::new(
            &run_id,
            started_at,
            &workdir_abs,
            &pipeline_path,
            run_dir.path(),
        );
        if let Err(e) = attractor_journal::append_entry_at(&index_path, &entry) {
            // Without an Index line the Run could never be found; undo
            // run.json so a retry starts the Run cleanly.
            let _ = std::fs::remove_file(run_dir.run_json());
            return Err(setup(&format!(
                "cannot append to Run Index {}: {e}",
                index_path.display()
            )));
        }
    }

    let journal = Arc::new(
        attractor_pipeline::open_journal(run_dir.path(), &run_id, attempt)
            .map_err(|e| setup(&e))?,
    );
    let journal_error = |e: std::io::Error| {
        setup(&format!(
            "cannot write Run Journal {}: {e}",
            journal.path().display()
        ))
    };
    if is_new_run {
        let controls = configured.controls();
        journal
            .append(EventData::RunStarted {
                pipeline_name: configured.plan().graph().name.clone(),
                pipeline_path: pipeline_path.display().to_string(),
                workdir: workdir_abs.display().to_string(),
                epic_id: None,
                max_budget_usd: Some(*controls.max_budget_usd().value()),
                max_steps: Some(*controls.max_steps().value()),
                shared_workdir,
            })
            .map_err(journal_error)?;
    }
    journal
        .append(EventData::AttemptStarted {
            attempt,
            pid: std::process::id(),
            pas_version,
            argv: invocation.argv.clone(),
            git_head: git_rev_parse(&workdir_abs, "HEAD"),
            resumed_from_node: checkpoint.as_ref().map(|cp| cp.current_node_id.clone()),
        })
        .map_err(journal_error)?;

    Ok(PreparedRun {
        configured,
        logs_dir,
        checkpoint,
        run_id,
        run_dir,
        journal,
        attempt,
        locks,
    })
}

/// Run a directory of .dot files sequentially with a cross-file manifest.
/// Files are sorted lexically — use zero-padded names (phase-01, phase-02).
#[allow(clippy::too_many_arguments)]
pub async fn cmd_run_dir(
    dir: &std::path::Path,
    workdir: Option<&std::path::Path>,
    dry_run: bool,
    max_budget_usd: Option<f64>,
    max_steps: Option<u64>,
    fresh: bool,
    codergen_claude: &CodergenClaudeCliOpts,
    invocation: &RunInvocation,
) -> anyhow::Result<()> {
    // One Run ID or one JSON first line cannot describe several Runs.
    if invocation.run_id.is_some() || invocation.json {
        return Err(setup_failed(
            invocation.json,
            SetupError::new(
                "unsupported_for_directory",
                "--run-id and --json need a single .dot file, not a directory",
            ),
        ));
    }

    // Collect and sort .dot files
    let mut dot_files: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "dot"))
        .collect();
    dot_files.sort();

    if dot_files.is_empty() {
        anyhow::bail!(
            "No .dot files found in {}\n\n\
             The directory must contain one or more *.dot pipeline files.\n\
             Files are sorted lexically and run in that order — use zero-padded\n\
             names to control execution order (e.g. phase-01.dot, phase-02.dot).\n\n\
             To generate .dot files from specs: pas generate <DOCS_DIR>",
            dir.display()
        );
    }

    // Prepare every plan before mutating the batch manifest or starting the
    // first pipeline. A later unsafe file must fail the whole batch closed.
    for dot_file in &dot_files {
        prepare_run_configuration(
            dot_file,
            workdir,
            dry_run,
            max_budget_usd,
            max_steps,
            codergen_claude,
            false,
        )?;
    }

    // Manifest tracks cross-file progress
    let manifest_dir = stable_manifest_dir(dir);
    let manifest_path = manifest_dir.join("manifest.json");

    if fresh {
        // Clear manifest and all per-pipeline checkpoints
        if manifest_path.exists() {
            std::fs::remove_file(&manifest_path)?;
        }
    }

    let mut manifest = load_manifest(&manifest_path)?;

    println!(
        "Running {} pipeline(s) from {} (lexical order)",
        dot_files.len(),
        dir.display()
    );
    for dot_file in &dot_files {
        println!(
            "  {}",
            dot_file.file_name().unwrap_or_default().to_string_lossy()
        );
    }
    for (i, dot_file) in dot_files.iter().enumerate() {
        let name = dot_file
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        // Skip already-completed pipelines
        if manifest.completed.contains(&name) {
            println!(
                "[{}/{}] {} — already completed, skipping",
                i + 1,
                dot_files.len(),
                name
            );
            continue;
        }

        println!("\n[{}/{}] {}", i + 1, dot_files.len(), name);
        manifest.current = Some(name.clone());
        save_manifest(&manifest, &manifest_path)?;

        cmd_run(
            dot_file,
            workdir,
            None, // each pipeline gets its own stable logs dir
            dry_run,
            max_budget_usd,
            max_steps,
            fresh, // propagate --fresh to clear per-pipeline checkpoints
            codergen_claude,
            invocation, // each pipeline gets its own Run
        )
        .await?;

        manifest.completed.push(name);
        manifest.current = None;
        save_manifest(&manifest, &manifest_path)?;
    }

    // All done — clean up manifest
    if manifest_path.exists() {
        std::fs::remove_file(&manifest_path)?;
    }

    println!("\nAll {} pipelines completed", dot_files.len());
    Ok(())
}

// ---------------------------------------------------------------------------
// Manifest for cross-file resume
// ---------------------------------------------------------------------------

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct RunManifest {
    completed: Vec<String>,
    current: Option<String>,
}

fn stable_manifest_dir(dir: &std::path::Path) -> PathBuf {
    let canonical = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let hash = fnv1a32(canonical.to_string_lossy().as_bytes());

    let stem = dir.file_name().unwrap_or_default().to_string_lossy();

    PathBuf::from(format!(".pas/logs/{}-batch-{:08x}", stem, hash))
}

fn load_manifest(path: &std::path::Path) -> anyhow::Result<RunManifest> {
    if path.exists() {
        let json = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&json)?)
    } else {
        Ok(RunManifest::default())
    }
}

fn save_manifest(manifest: &RunManifest, path: &std::path::Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(manifest)?;
    std::fs::write(path, json)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An invocation whose Run Index lives in the test's temp folder.
    fn test_invocation(dir: &std::path::Path) -> RunInvocation {
        RunInvocation {
            argv: vec!["pas".into(), "run".into()],
            index_path: Some(dir.join("runs.jsonl")),
            ..Default::default()
        }
    }

    /// `pas run` must refuse to execute a pipeline that has a runtime
    /// (box/diamond) node with no explicit `llm_provider`: it must return
    /// an error, and it must not have started the engine at all -- no
    /// checkpoint.json should ever be written for a run that never began.
    #[tokio::test]
    async fn cmd_run_fails_fast_on_missing_llm_provider_no_checkpoint_written() {
        let pipeline_dir = tempfile::tempdir().unwrap();
        let pipeline_path = pipeline_dir.path().join("missing_provider.dot");
        std::fs::write(
            &pipeline_path,
            r#"digraph MissingProvider {
                start [shape="Mdiamond"]
                work [shape="box", prompt="Do work"]
                done [shape="Msquare"]
                start -> work -> done
            }"#,
        )
        .unwrap();

        let logs_dir = tempfile::tempdir().unwrap();

        let result = cmd_run(
            &pipeline_path,
            None,
            Some(logs_dir.path()),
            false,
            None,
            Some(100),
            false,
            &CodergenClaudeCliOpts::default(),
            &test_invocation(logs_dir.path()),
        )
        .await;

        assert!(
            result.is_err(),
            "cmd_run should fail fast on a node with no llm_provider"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("validation") || err_msg.to_lowercase().contains("validation"),
            "error should indicate a validation failure; got: {err_msg}"
        );

        assert!(
            !logs_dir.path().join("checkpoint.json").exists(),
            "no checkpoint should be written when validation blocks the run before execution starts"
        );
    }

    /// Sanity check that a pipeline with every runtime node carrying an
    /// explicit llm_provider is NOT blocked by the same gate (dry_run mode,
    /// so no real provider CLI is spawned even on success).
    #[tokio::test]
    async fn cmd_run_proceeds_when_llm_provider_is_explicit() {
        let pipeline_dir = tempfile::tempdir().unwrap();
        let pipeline_path = pipeline_dir.path().join("has_provider.dot");
        std::fs::write(
            &pipeline_path,
            r#"digraph HasProvider {
                start [shape="Mdiamond"]
                done [shape="Msquare"]
                start -> done
            }"#,
        )
        .unwrap();

        let logs_dir = tempfile::tempdir().unwrap();

        // A temp workdir: the crate dir would take this repository's
        // Worktree lock and clash with any `pas run` active here.
        let result = cmd_run(
            &pipeline_path,
            Some(pipeline_dir.path()),
            Some(logs_dir.path()),
            true,
            None,
            Some(100),
            false,
            &CodergenClaudeCliOpts::default(),
            &test_invocation(logs_dir.path()),
        )
        .await;

        assert!(
            result.is_ok(),
            "cmd_run should not be blocked by provider_required when there is nothing to flag: {result:?}"
        );
    }

    const CP_ID: &str = "0190a3b2-7c4d-7e5f-8a6b-1c2d3e4f5a6b";
    const OTHER_ID: &str = "0190a3b2-7c4d-7e5f-8a6b-000000000001";

    #[test]
    fn new_run_decision_uses_uuid_v7() {
        let RunIdentity::New(id) = decide_run_id(None, None).unwrap() else {
            panic!("no checkpoint must start a new Run");
        };
        let uuid = uuid::Uuid::parse_str(&id).unwrap();
        assert_eq!(uuid.get_version_num(), 7, "{id}");
        assert_eq!(id, uuid.hyphenated().to_string(), "lowercase hyphenated");
        assert_ne!(decide_run_id(None, None).unwrap(), RunIdentity::New(id));
    }

    #[test]
    fn decide_run_id_resumes_checkpoint_id() {
        assert_eq!(
            decide_run_id(Some(CP_ID), None).unwrap(),
            RunIdentity::Resume(CP_ID.into())
        );
        // Re-issuing run.json's argv with the same --run-id resumes.
        assert_eq!(
            decide_run_id(Some(CP_ID), Some(CP_ID)).unwrap(),
            RunIdentity::Resume(CP_ID.into())
        );
    }

    #[test]
    fn decide_run_id_uses_requested_id_for_a_new_run() {
        assert_eq!(
            decide_run_id(None, Some(OTHER_ID)).unwrap(),
            RunIdentity::New(OTHER_ID.into())
        );
    }

    #[test]
    fn decide_run_id_rejects_mismatch_with_checkpoint() {
        let error = decide_run_id(Some(CP_ID), Some(OTHER_ID)).unwrap_err();
        assert_eq!(error.code, "run_id_mismatch");
        assert!(error.message.contains(CP_ID), "{}", error.message);
    }

    #[test]
    fn attempt_end_reason_maps_outcomes() {
        use attractor_types::AttractorError;
        let ok = attractor_pipeline::PipelineResult {
            completed_nodes: vec![],
            node_outcomes: std::collections::HashMap::new(),
            final_context: std::collections::HashMap::new(),
            total_cost: 0.0,
        };
        let cases = [
            (
                AttemptOutcome::Finished(Ok(ok)),
                AttemptEndReason::Completed,
                None,
            ),
            (
                AttemptOutcome::Finished(Err(AttractorError::BudgetExhausted {
                    spent: 2.0,
                    limit: 1.0,
                })),
                AttemptEndReason::BudgetExhausted,
                Some("Pipeline exceeded budget ($2.00 > $1.00). Use --max-budget-usd to increase."),
            ),
            (
                AttemptOutcome::Finished(Err(AttractorError::MaxStepsExceeded { max_steps: 3 })),
                AttemptEndReason::MaxSteps,
                Some("Pipeline exceeded maximum step count (3). Use --max-steps to increase."),
            ),
            (
                AttemptOutcome::Finished(Err(AttractorError::Other("boom".into()))),
                AttemptEndReason::Failed,
                Some("boom"),
            ),
            (
                AttemptOutcome::Terminated,
                AttemptEndReason::Stopped,
                Some("SIGTERM"),
            ),
        ];
        for (outcome, reason, message) in cases {
            assert_eq!(
                attempt_end_reason(&outcome),
                (reason, message.map(str::to_string))
            );
        }
    }

    #[test]
    fn last_attempt_reads_highest_attempt_and_skips_bad_lines() {
        let dir = tempfile::tempdir().unwrap();
        let events = dir.path().join("events.jsonl");
        assert_eq!(last_attempt(&events), 0, "missing journal");
        std::fs::write(&events, "").unwrap();
        assert_eq!(last_attempt(&events), 0, "empty journal");
        std::fs::write(
            &events,
            "{\"attempt\":1}\n{\"attempt\":3}\nnot json\n{\"attempt\":2}\n{\"attem",
        )
        .unwrap();
        assert_eq!(last_attempt(&events), 3);
    }

    /// A `New` Run whose `run.json` already exists (e.g. `--run-id` of a
    /// finished Run) is refused before anything is written.
    #[tokio::test]
    async fn new_run_with_existing_run_json_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let pipeline = dir.path().join("p.dot");
        std::fs::write(
            &pipeline,
            r#"digraph P { start [shape="Mdiamond"] done [shape="Msquare"] start -> done }"#,
        )
        .unwrap();
        let logs = dir.path().join("logs");
        let run_dir = logs.join("runs").join(OTHER_ID);
        std::fs::create_dir_all(&run_dir).unwrap();
        std::fs::write(run_dir.join("run.json"), "{}").unwrap();

        let mut invocation = test_invocation(dir.path());
        invocation.run_id = Some(OTHER_ID.into());
        let error = cmd_run(
            &pipeline,
            None,
            Some(&logs),
            true,
            None,
            Some(10),
            false,
            &CodergenClaudeCliOpts::default(),
            &invocation,
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("already exists"), "{error}");
        assert_eq!(std::fs::read(run_dir.join("run.json")).unwrap(), b"{}");
        assert!(!run_dir.join("events.jsonl").exists());
        assert!(!dir.path().join("runs.jsonl").exists());
    }

    /// A journal for Heartbeat tests, in a temp folder.
    fn heartbeat_journal(dir: &std::path::Path) -> Arc<attractor_journal::JournalWriter> {
        Arc::new(attractor_journal::JournalWriter::open(dir, OTHER_ID, 1).unwrap())
    }

    fn journal_lines(dir: &std::path::Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(dir.join("events.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn heartbeats(lines: &[serde_json::Value]) -> Vec<&serde_json::Value> {
        lines.iter().filter(|e| e["type"] == "Heartbeat").collect()
    }

    #[test]
    fn heartbeat_interval_override_parses_positive_milliseconds() {
        assert_eq!(
            parse_heartbeat_interval_ms("250"),
            Some(Duration::from_millis(250))
        );
        assert_eq!(
            parse_heartbeat_interval_ms(" 30000 "),
            Some(HEARTBEAT_INTERVAL)
        );
        for bad in ["0", "", "x", "-5", "1.5"] {
            assert_eq!(parse_heartbeat_interval_ms(bad), None, "{bad:?}");
        }
    }

    // AC: a 95 s Attempt gets 3 Heartbeats, each with the process PID, 30 s
    // apart, through the shared writer (contiguous seq).
    #[tokio::test(start_paused = true)]
    async fn heartbeat_writes_three_in_95_seconds() {
        let dir = tempfile::tempdir().unwrap();
        let journal = heartbeat_journal(dir.path());
        let pid = std::process::id();
        let started = tokio::time::Instant::now();
        let heartbeat = Heartbeat::start(journal.clone(), pid, HEARTBEAT_INTERVAL);
        let mut at = Vec::new();
        let mut seen = 0;
        while started.elapsed() < Duration::from_secs(95) {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let count = heartbeats(&journal_lines(dir.path())).len();
            if count > seen {
                seen = count;
                at.push(started.elapsed());
            }
        }
        heartbeat.stop().await;

        let lines = journal_lines(dir.path());
        let beats = heartbeats(&lines);
        assert_eq!(beats.len(), 3, "{lines:?}");
        for beat in &beats {
            assert_eq!(beat["data"]["pid"], pid);
            assert_eq!(beat["run_id"], OTHER_ID);
            assert_eq!(beat["attempt"], 1);
        }
        let seqs: Vec<u64> = lines.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
        assert_eq!(seqs, vec![1, 2, 3]);
        // Observed (within the 0.5 s polling step) at 30 s, 60 s, 90 s.
        for (i, t) in at.iter().enumerate() {
            let expected = HEARTBEAT_INTERVAL * (i as u32 + 1);
            assert!(
                *t >= expected && *t <= expected + Duration::from_secs(1),
                "Heartbeat {i} at {t:?}"
            );
        }
    }

    // AC: an Attempt shorter than the interval gets no Heartbeat.
    #[tokio::test(start_paused = true)]
    async fn heartbeat_none_before_first_interval() {
        let dir = tempfile::tempdir().unwrap();
        let journal = heartbeat_journal(dir.path());
        let heartbeat = Heartbeat::start(journal, std::process::id(), HEARTBEAT_INTERVAL);
        tokio::time::sleep(HEARTBEAT_INTERVAL - Duration::from_millis(100)).await;
        heartbeat.stop().await;
        tokio::time::sleep(Duration::from_secs(300)).await;
        assert!(journal_lines(dir.path()).is_empty());
    }

    // AC: once stopped, nothing follows AttemptEnded.
    #[tokio::test(start_paused = true)]
    async fn heartbeat_stop_waits_and_nothing_follows() {
        let dir = tempfile::tempdir().unwrap();
        let journal = heartbeat_journal(dir.path());
        let heartbeat = Heartbeat::start(journal.clone(), std::process::id(), HEARTBEAT_INTERVAL);
        tokio::time::sleep(Duration::from_secs(61)).await;
        heartbeat.stop().await;
        journal
            .append(EventData::AttemptEnded {
                attempt: 1,
                reason: AttemptEndReason::Completed,
                message: None,
            })
            .unwrap();
        let next_seq = journal.next_seq();
        tokio::time::sleep(Duration::from_secs(300)).await;

        let lines = journal_lines(dir.path());
        assert_eq!(heartbeats(&lines).len(), 2, "{lines:?}");
        assert_eq!(lines.last().unwrap()["type"], "AttemptEnded");
        assert_eq!(journal.next_seq(), next_seq);
    }

    // An early return from `cmd_run` drops the guard: the task must not keep
    // writing into a later Pipeline's lifetime.
    #[tokio::test(start_paused = true)]
    async fn heartbeat_drop_aborts_task() {
        let dir = tempfile::tempdir().unwrap();
        let journal = heartbeat_journal(dir.path());
        let heartbeat = Heartbeat::start(journal, std::process::id(), HEARTBEAT_INTERVAL);
        tokio::time::sleep(Duration::from_secs(31)).await;
        drop(heartbeat);
        tokio::time::sleep(Duration::from_secs(300)).await;
        assert_eq!(heartbeats(&journal_lines(dir.path())).len(), 1);
    }

    // After a stall (e.g. laptop sleep) the task writes one Heartbeat and
    // resumes normal spacing, never a burst.
    #[tokio::test(start_paused = true)]
    async fn heartbeat_after_stall_does_not_burst() {
        let dir = tempfile::tempdir().unwrap();
        let journal = heartbeat_journal(dir.path());
        let heartbeat = Heartbeat::start(journal, std::process::id(), HEARTBEAT_INTERVAL);
        // Let the task register its first deadline, then jump far past it
        // without yielding, as a suspended machine would.
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(300)).await;
        tokio::time::sleep(Duration::from_secs(29)).await;
        heartbeat.stop().await;
        assert_eq!(heartbeats(&journal_lines(dir.path())).len(), 1);
    }
}
