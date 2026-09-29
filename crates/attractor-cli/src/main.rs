//! CLI binary for running and validating Attractor pipelines.

mod commands;

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use commands::{
    cmd_answer, cmd_decompose, cmd_generate, cmd_generate_dir, cmd_info, cmd_init, cmd_kill,
    cmd_launch, cmd_plan, cmd_run, cmd_run_dir, cmd_runs, cmd_scaffold, cmd_stop, cmd_validate,
    heartbeat_interval_from_env, validate_decomposition, AnswerSourceArg, CodergenClaudeCliOpts,
    DecomposeSource, GenerateInput, InitOpts, RunInvocation, RunRefused,
};

#[derive(Parser)]
#[command(
    name = "pas",
    version,
    about = "Pascal's Discrete Attractor — DOT-based pipeline runner for AI workflows"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Enable verbose logging
    #[arg(short, long, global = true)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a pipeline from a .dot file or a directory of .dot files.
    ///
    /// When given a directory, all *.dot files are collected and run
    /// sequentially in lexical order. Use zero-padded names to control
    /// execution order (e.g. phase-01.dot, phase-02.dot, phase-11.dot).
    ///
    /// Checkpoints are saved automatically after each node. If a run is
    /// interrupted, re-running the same command resumes from the last
    /// completed node. Use --fresh to discard checkpoints and start over.
    Run {
        /// Path to a .dot file, or a directory of .dot files (sorted lexically)
        pipeline: PathBuf,

        /// Working directory for tool execution
        #[arg(short, long)]
        workdir: Option<PathBuf>,

        /// Logs output directory (default: .pas/logs/<pipeline>-<hash>)
        #[arg(short, long)]
        logs: Option<PathBuf>,

        /// Don't actually call LLMs (dry run)
        #[arg(long)]
        dry_run: bool,

        /// Maximum tracked spend across all nodes (USD). Defaults to $200; Codex/Gemini costs are untracked.
        #[arg(long)]
        max_budget_usd: Option<f64>,

        /// Maximum number of node executions before aborting. Prevents runaway loops. Default: 200.
        #[arg(long)]
        max_steps: Option<u64>,

        /// Ignore checkpoint and start fresh
        #[arg(long)]
        fresh: bool,

        /// Claude settings mode for codergen nodes: subscription-bare, strict-bare, or inherit
        #[arg(long)]
        codergen_claude_settings_mode: Option<String>,

        /// Claude setting sources for inherit mode (comma-separated: user,project,local)
        #[arg(long)]
        codergen_claude_setting_sources: Option<String>,

        /// PAS-owned Claude settings JSON or file path for codergen nodes
        #[arg(long)]
        codergen_claude_settings: Option<String>,

        /// Claude built-in tools surface for codergen nodes (e.g. "Read,Edit" or "")
        #[arg(long)]
        codergen_claude_tools: Option<String>,

        /// Claude agents JSON for codergen nodes
        #[arg(long)]
        codergen_claude_agents: Option<String>,

        /// Claude plugin directory for codergen nodes (repeatable)
        #[arg(long)]
        codergen_claude_plugin_dir: Vec<PathBuf>,

        /// Claude MCP config JSON or file path for codergen nodes
        #[arg(long)]
        codergen_claude_mcp_config: Option<String>,

        /// Use this Run ID (a UUID) instead of generating one
        #[arg(long)]
        run_id: Option<String>,

        /// Print `{"v":1,"ok":true,"run_id","run_dir"}` as the first stdout
        /// line once the Run folder exists; other output goes to stderr
        #[arg(long)]
        json: bool,

        /// Start even if another Run is active in this git worktree
        /// (recorded as `shared_workdir` in `RunStarted`)
        #[arg(long)]
        allow_shared_workdir: bool,
    },

    /// Answer a waiting Human Gate by creating its answer file. Exits 7 when
    /// the question is already answered
    Answer {
        /// Run ID (a UUID from `pas runs`)
        run_id: String,

        /// Question ID from the Run's `HumanInputRequested` Event
        question_id: String,

        /// One of the question's choices (its edge label, exactly)
        choice: String,

        /// Who is answering, recorded in the journal
        #[arg(long, value_enum, default_value = "cli")]
        source: AnswerSourceArg,

        /// Print one JSON object `{"v":1,"ok":...,"run_id":...}`
        #[arg(long)]
        json: bool,
    },

    /// Ask an active Run to stop after its current stage. It ends with
    /// `stopped` and can be resumed by running the same command again
    Stop {
        /// Run ID (a UUID from `pas runs`)
        run_id: String,

        /// Who is stopping, recorded in the journal
        #[arg(long, value_enum, default_value = "cli")]
        source: AnswerSourceArg,

        /// Print one JSON object `{"v":1,"ok":...,"run_id":...}`
        #[arg(long)]
        json: bool,
    },

    /// End an active Run now: SIGTERM, then SIGKILL after the grace period.
    /// The last completed stage's checkpoint is kept, so the same `pas run`
    /// command resumes it
    Kill {
        /// Run ID (a UUID from `pas runs`)
        run_id: String,

        /// How long to wait after SIGTERM before SIGKILL (e.g. 500ms, 10s, 1m)
        #[arg(long, default_value = "10s")]
        grace: String,

        /// Print one JSON object `{"v":1,"ok":...,"run_id":...}`
        #[arg(long)]
        json: bool,
    },

    /// List Runs from the Run Index with a status derived from each Run
    /// Journal: running, completed, failed, stopped, crashed, or missing
    Runs {
        /// Only list Runs whose status is `running`
        #[arg(long)]
        active: bool,

        /// Print one JSON object `{"v":1,"ok":true,"runs":[...]}`
        #[arg(long)]
        json: bool,
    },

    /// Validate a pipeline .dot file
    Validate {
        /// Path to the pipeline .dot file
        pipeline: PathBuf,
    },

    /// Show information about a pipeline
    Info {
        /// Path to the pipeline .dot file
        pipeline: PathBuf,
    },

    /// Generate PRD or spec documents from templates
    Plan {
        /// Generate a PRD document
        #[arg(long, conflicts_with = "spec")]
        prd: bool,

        /// Generate a spec document
        #[arg(long, conflicts_with = "prd")]
        spec: bool,

        /// Generate from a prompt description (uses Claude CLI)
        #[arg(long)]
        from_prompt: Option<String>,

        /// Output file path (defaults: .pas/prd.md or .pas/spec.md)
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// Decompose a spec or Plan into beads epic and tasks
    #[command(group(clap::ArgGroup::new("source").required(true).args(["spec_path", "plan", "from_proposal"])))]
    Decompose {
        /// Path to the spec markdown file
        spec_path: Option<PathBuf>,

        /// A Plan document (.md or .txt); repeat for a multi-file Plan
        #[arg(long = "plan", value_name = "FILE", action = clap::ArgAction::Append, conflicts_with_all = ["spec_path", "from_proposal", "validate"])]
        plan: Vec<PathBuf>,

        /// Create exactly the Proposal in this JSON file, without an LLM call
        #[arg(long, value_name = "FILE", conflicts_with_all = ["spec_path", "plan", "dry_run", "validate"])]
        from_proposal: Option<PathBuf>,

        /// Print the generated shell commands without executing them
        #[arg(long, conflicts_with = "validate")]
        dry_run: bool,

        /// Validate existing tickets against spec (skip LLM, just check coverage)
        #[arg(long, conflicts_with = "dry_run", requires = "spec_path")]
        validate: Option<String>,

        /// Print one JSON object (C6): a Proposal with --dry-run, else
        /// `{"v":1,"ok":true,"epic_id":..,"task_ids":[..]}`; failures print
        /// `{"v":1,"ok":false,"error":{"code","message"}}` and exit 1
        #[arg(long, conflicts_with = "validate")]
        json: bool,
    },

    /// Scaffold a pipeline from a beads epic
    Scaffold {
        /// Beads epic ID (e.g., beads-xxx)
        epic_id: String,

        /// Output file path (default: pipelines/<epic-id>.dot)
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Print one JSON object: `{"v":1,"ok":true,"pipeline_path":..}`,
        /// or `{"v":1,"ok":false,"error":{"code","message"}}` and exit 1 (C6)
        #[arg(long)]
        json: bool,
    },

    /// Generate pipeline .dot files from spec (and optional PRD) files.
    ///
    /// Single-file mode:
    ///   pas generate my-spec.md
    ///   pas generate my-prd.md my-spec.md
    ///
    /// Directory mode:
    ///   pas generate docs/implementation/
    ///
    /// In directory mode, files ending in -spec.md are discovered and sorted
    /// lexically. Each spec is paired with a matching -prd.md file if one
    /// exists (e.g. auth-spec.md pairs with auth-prd.md). One .dot pipeline
    /// is generated per spec. Use zero-padded prefixes to control order
    /// (e.g. phase-01-spec.md, phase-02-spec.md).
    Generate {
        /// Spec file, prd then spec (positional), or a directory of *-spec.md files
        #[arg(value_name = "FILE")]
        files: Vec<PathBuf>,

        /// PRD file path (alternative to positional)
        #[arg(long)]
        prd: Option<PathBuf>,

        /// Spec file path (alternative to positional)
        #[arg(long)]
        spec: Option<PathBuf>,

        /// A Plan document (.md or .txt); repeat for a multi-file Plan, in order
        #[arg(long = "plan", value_name = "FILE", action = clap::ArgAction::Append, conflicts_with_all = ["files", "prd", "spec"])]
        plan: Vec<PathBuf>,

        /// Output .dot file or directory (default: pipelines/<spec-stem>.dot,
        /// or the last --plan file's stem)
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Print one JSON object (C6): `{"v":1,"ok":true,"pipeline_path":..}`;
        /// failures print `{"v":1,"ok":false,"error":{"code","message"}}` and exit 1
        #[arg(long)]
        json: bool,
    },

    /// Initialise a `pas.toml` in the current (or specified) project.
    ///
    /// Detects the project toolchain from well-known config files (Cargo.toml,
    /// pyproject.toml, package.json, …) and writes a starter `pas.toml` to the
    /// nearest `.git` root. If no `.git` is found the command exits with code 4
    /// in non-interactive mode unless `--force` is given.
    Init {
        /// Working directory to inspect (default: current directory)
        #[arg(long, default_value = ".")]
        workdir: PathBuf,

        /// Overwrite an existing `pas.toml` and proceed without a `.git` guard
        #[arg(long)]
        force: bool,

        /// Never prompt; fail instead of asking questions
        #[arg(long)]
        non_interactive: bool,

        /// Skip LLM enrichment (always true for now — enrichment is a future step)
        #[arg(long)]
        no_enrich: bool,

        /// Print what would be written without touching the filesystem
        #[arg(long)]
        dry_run: bool,
    },

    /// Manage the pas.toml trust store.
    Trust {
        #[command(subcommand)]
        action: TrustAction,
    },

    /// Generate, validate, and run pipelines end-to-end.
    ///
    /// Takes a directory containing spec files (and optional PRD files):
    ///   1. Discovers *-spec.md files, pairs each with its *-prd.md
    ///   2. Generates .dot pipelines (one per spec, sorted lexically)
    ///   3. Validates all generated .dot files — stops if any fail
    ///   4. Runs pipelines sequentially with checkpoint/resume
    ///
    /// Spec files must end in -spec.md. PRD files are paired by replacing
    /// -spec with -prd (e.g. auth-spec.md + auth-prd.md). PRDs are optional
    /// but recommended — they provide business context to the generator.
    ///
    /// Use zero-padded prefixes to control execution order:
    ///   phase-01-spec.md, phase-02-spec.md, ..., phase-11-spec.md
    Launch {
        /// Directory containing *-spec.md (required) and *-prd.md (optional) files
        docs_dir: PathBuf,

        /// Working directory for tool execution
        #[arg(short, long)]
        workdir: Option<PathBuf>,

        /// Output directory for generated .dot files (default: pipelines/)
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Don't actually call LLMs during run (dry run)
        #[arg(long)]
        dry_run: bool,

        /// Maximum tracked spend per pipeline (USD). Defaults to $200; Codex/Gemini costs are untracked.
        #[arg(long)]
        max_budget_usd: Option<f64>,

        /// Maximum number of node executions per pipeline. Default: 200.
        #[arg(long)]
        max_steps: Option<u64>,

        /// Ignore checkpoints and start fresh
        #[arg(long)]
        fresh: bool,

        /// Claude settings mode for codergen nodes: subscription-bare, strict-bare, or inherit
        #[arg(long)]
        codergen_claude_settings_mode: Option<String>,

        /// Claude setting sources for inherit mode (comma-separated: user,project,local)
        #[arg(long)]
        codergen_claude_setting_sources: Option<String>,

        /// PAS-owned Claude settings JSON or file path for codergen nodes
        #[arg(long)]
        codergen_claude_settings: Option<String>,

        /// Claude built-in tools surface for codergen nodes (e.g. "Read,Edit" or "")
        #[arg(long)]
        codergen_claude_tools: Option<String>,

        /// Claude agents JSON for codergen nodes
        #[arg(long)]
        codergen_claude_agents: Option<String>,

        /// Claude plugin directory for codergen nodes (repeatable)
        #[arg(long)]
        codergen_claude_plugin_dir: Vec<PathBuf>,

        /// Claude MCP config JSON or file path for codergen nodes
        #[arg(long)]
        codergen_claude_mcp_config: Option<String>,
    },
}

#[derive(Subcommand)]
enum TrustAction {
    /// Add a manifest to the trust store
    Add {
        /// Path to the pas.toml file
        path: PathBuf,
        /// blake3 hash of the manifest
        hash: String,
    },
    /// Remove a manifest from the trust store
    Remove {
        /// Path to the pas.toml file
        path: PathBuf,
        /// blake3 hash of the manifest
        hash: String,
    },
    /// List all trusted manifests
    List,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let result = run_cli().await;
    // A Run refused by a lock has its own exit code (C5).
    if let Some(refused) = result
        .as_ref()
        .err()
        .and_then(|error| error.downcast_ref::<RunRefused>())
    {
        eprintln!("error: {refused}");
        std::process::exit(refused.exit_code);
    }
    result
}

async fn run_cli() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Setup tracing
    let filter = if cli.verbose { "debug" } else { "info" };
    // `--json` modes keep stdout for their JSON.
    if matches!(
        cli.command,
        Commands::Run { json: true, .. }
            | Commands::Runs { json: true, .. }
            | Commands::Scaffold { json: true, .. }
    ) {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }

    match cli.command {
        Commands::Run {
            pipeline,
            workdir,
            logs,
            dry_run,
            max_budget_usd,
            max_steps,
            fresh,
            codergen_claude_settings_mode,
            codergen_claude_setting_sources,
            codergen_claude_settings,
            codergen_claude_tools,
            codergen_claude_agents,
            codergen_claude_plugin_dir,
            codergen_claude_mcp_config,
            run_id,
            json,
            allow_shared_workdir,
        } => {
            let codergen_claude = CodergenClaudeCliOpts {
                settings_mode: codergen_claude_settings_mode,
                setting_sources: codergen_claude_setting_sources,
                settings: codergen_claude_settings,
                tools: codergen_claude_tools,
                agents: codergen_claude_agents,
                plugin_dirs: codergen_claude_plugin_dir,
                mcp_config: codergen_claude_mcp_config,
            };
            let invocation = RunInvocation {
                argv: std::env::args().collect(),
                run_id,
                json,
                index_path: None,
                heartbeat_interval: heartbeat_interval_from_env(),
                allow_shared_workdir,
            };
            if pipeline.is_dir() {
                cmd_run_dir(
                    &pipeline,
                    workdir.as_deref(),
                    dry_run,
                    max_budget_usd,
                    max_steps,
                    fresh,
                    &codergen_claude,
                    &invocation,
                )
                .await?;
            } else {
                cmd_run(
                    &pipeline,
                    workdir.as_deref(),
                    logs.as_deref(),
                    dry_run,
                    max_budget_usd,
                    max_steps,
                    fresh,
                    &codergen_claude,
                    &invocation,
                )
                .await?;
            }
        }
        Commands::Init {
            workdir,
            force,
            non_interactive,
            no_enrich,
            dry_run,
        } => {
            let opts = InitOpts {
                force,
                non_interactive,
                no_enrich,
                dry_run,
            };
            cmd_init(&workdir, &opts)?;
        }
        Commands::Runs { active, json } => cmd_runs(active, json)?,
        Commands::Answer {
            run_id,
            question_id,
            choice,
            source,
            json,
        } => cmd_answer(&run_id, &question_id, &choice, source, json)?,
        Commands::Stop {
            run_id,
            source,
            json,
        } => cmd_stop(&run_id, source, json)?,
        Commands::Kill {
            run_id,
            grace,
            json,
        } => cmd_kill(&run_id, &grace, json)?,
        Commands::Validate { pipeline } => {
            cmd_validate(&pipeline)?;
        }
        Commands::Info { pipeline } => {
            cmd_info(&pipeline)?;
        }
        Commands::Plan {
            prd,
            spec,
            from_prompt,
            output,
        } => {
            cmd_plan(prd, spec, from_prompt.as_deref(), output.as_deref()).await?;
        }
        Commands::Decompose {
            spec_path,
            plan,
            from_proposal,
            dry_run,
            validate,
            json,
        } => {
            if let (Some(epic_id), Some(spec_path)) = (&validate, &spec_path) {
                let spec_content = std::fs::read_to_string(spec_path)?;
                validate_decomposition(&spec_content, Some(epic_id)).await?;
            } else {
                let source = match (&spec_path, &from_proposal) {
                    (_, Some(file)) => DecomposeSource::Proposal(file),
                    (Some(path), _) => DecomposeSource::Spec(path),
                    _ => DecomposeSource::Plan(&plan),
                };
                cmd_decompose(source, dry_run, json).await?;
            }
        }
        Commands::Scaffold {
            epic_id,
            output,
            json,
        } => {
            cmd_scaffold(&epic_id, output.as_deref(), json).await?;
        }
        Commands::Generate {
            files,
            prd,
            spec,
            plan,
            output,
            json,
        } => {
            if !plan.is_empty() {
                cmd_generate(
                    GenerateInput::Plan(&plan),
                    output.as_deref(),
                    cli.verbose,
                    json,
                )
                .await?;
            } else if files.len() == 1
                && files[0].is_dir()
                && prd.is_none()
                && spec.is_none()
                && json
            {
                anyhow::bail!("--json is not supported in directory mode");
            } else if files.len() == 1 && files[0].is_dir() && prd.is_none() && spec.is_none() {
                cmd_generate_dir(&files[0], output.as_deref(), cli.verbose, true).await?;
            } else {
                // Resolve spec and prd from positional args and/or named flags.
                let (resolved_prd, resolved_spec) = match (prd, spec, files.len()) {
                    (Some(p), Some(s), _) => (Some(p), s),
                    (None, Some(s), _) => (None, s),
                    (Some(p), None, 1) => (Some(p), files[0].clone()),
                    (None, None, 1) => (None, files[0].clone()),
                    (None, None, 2) => (Some(files[0].clone()), files[1].clone()),
                    (Some(_), None, 0) => {
                        anyhow::bail!(
                            "Spec file is required. Usage: pas generate [--prd PRD] <SPEC>"
                        );
                    }
                    (None, None, 0) => {
                        anyhow::bail!("Spec file is required. Usage: pas generate [PRD] <SPEC>");
                    }
                    _ => {
                        anyhow::bail!(
                            "Too many arguments. Usage:\n  \
                             pas generate <SPEC>\n  \
                             pas generate <PRD> <SPEC>\n  \
                             pas generate --prd <PRD> --spec <SPEC>\n  \
                             pas generate <DIRECTORY>"
                        );
                    }
                };
                cmd_generate(
                    GenerateInput::SpecPrd {
                        spec: &resolved_spec,
                        prd: resolved_prd.as_deref(),
                    },
                    output.as_deref(),
                    cli.verbose,
                    json,
                )
                .await?;
            }
        }
        Commands::Launch {
            docs_dir,
            workdir,
            output,
            dry_run,
            max_budget_usd,
            max_steps,
            fresh,
            codergen_claude_settings_mode,
            codergen_claude_setting_sources,
            codergen_claude_settings,
            codergen_claude_tools,
            codergen_claude_agents,
            codergen_claude_plugin_dir,
            codergen_claude_mcp_config,
        } => {
            let codergen_claude = CodergenClaudeCliOpts {
                settings_mode: codergen_claude_settings_mode,
                setting_sources: codergen_claude_setting_sources,
                settings: codergen_claude_settings,
                tools: codergen_claude_tools,
                agents: codergen_claude_agents,
                plugin_dirs: codergen_claude_plugin_dir,
                mcp_config: codergen_claude_mcp_config,
            };
            cmd_launch(
                &docs_dir,
                output.as_deref(),
                workdir.as_deref(),
                dry_run,
                max_budget_usd,
                max_steps,
                fresh,
                cli.verbose,
                &codergen_claude,
            )
            .await?;
        }
        Commands::Trust { action } => match action {
            TrustAction::Add { path, hash } => {
                attractor_quality::add_trust(&path, &hash, "flag")
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                println!("Trusted: {}", path.display());
            }
            TrustAction::Remove { path, hash } => {
                attractor_quality::remove_trust(&path, &hash)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                println!("Removed trust for: {}", path.display());
            }
            TrustAction::List => {
                let entries =
                    attractor_quality::list_trusted().map_err(|e| anyhow::anyhow!("{e}"))?;
                for e in &entries {
                    println!(
                        "{} ({})",
                        e.path,
                        &e.blake3_hash[..16.min(e.blake3_hash.len())]
                    );
                }
                if entries.is_empty() {
                    println!("(no trusted manifests)");
                }
            }
        },
    }

    Ok(())
}

pub(crate) fn load_pipeline(
    path: &std::path::Path,
) -> anyhow::Result<attractor_pipeline::PipelineGraph> {
    let source = std::fs::read_to_string(path)?;
    let dot = attractor_dot::parse(&source)?;
    let graph = attractor_pipeline::PipelineGraph::from_dot(dot)?;
    Ok(graph)
}

pub(crate) fn load_execution_plan(
    path: &std::path::Path,
) -> anyhow::Result<attractor_pipeline::ExecutionPlan> {
    let graph = load_pipeline(path)?;
    attractor_pipeline::ExecutionPlan::compile(graph)
        .map_err(|error| anyhow::anyhow!("Pipeline validation failed: {error}"))
}
