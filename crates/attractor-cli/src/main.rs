//! CLI binary for running and validating Attractor pipelines.

mod commands;

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use commands::{
    cmd_decompose, cmd_generate, cmd_generate_dir, cmd_info, cmd_init, cmd_launch, cmd_plan,
    cmd_run, cmd_run_dir, cmd_scaffold, cmd_validate, validate_decomposition, InitOpts,
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

        /// Maximum total spend across all nodes (USD). Pipeline aborts if exceeded.
        #[arg(long)]
        max_budget_usd: Option<f64>,

        /// Maximum number of node executions before aborting. Prevents runaway loops. Default: 200.
        #[arg(long, default_value = "200")]
        max_steps: u64,

        /// Ignore checkpoint and start fresh
        #[arg(long)]
        fresh: bool,
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

    /// Decompose a spec into beads epic and tasks
    Decompose {
        /// Path to the spec markdown file
        spec_path: PathBuf,

        /// Print the generated shell commands without executing them
        #[arg(long, conflicts_with = "validate")]
        dry_run: bool,

        /// Validate existing tickets against spec (skip LLM, just check coverage)
        #[arg(long, conflicts_with = "dry_run")]
        validate: Option<String>,
    },

    /// Scaffold a pipeline from a beads epic
    Scaffold {
        /// Beads epic ID (e.g., beads-xxx)
        epic_id: String,

        /// Output file path (default: pipelines/<epic-id>.dot)
        #[arg(short, long)]
        output: Option<PathBuf>,
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

        /// Output .dot file or directory (default: pipelines/<spec-stem>.dot)
        #[arg(short, long)]
        output: Option<PathBuf>,
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

        /// Maximum total spend across all nodes (USD)
        #[arg(long)]
        max_budget_usd: Option<f64>,

        /// Maximum number of node executions per pipeline. Default: 200.
        #[arg(long, default_value = "200")]
        max_steps: u64,

        /// Ignore checkpoints and start fresh
        #[arg(long)]
        fresh: bool,
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
    let cli = Cli::parse();

    // Setup tracing
    let filter = if cli.verbose { "debug" } else { "info" };
    tracing_subscriber::fmt().with_env_filter(filter).init();

    match cli.command {
        Commands::Run {
            pipeline,
            workdir,
            logs,
            dry_run,
            max_budget_usd,
            max_steps,
            fresh,
        } => {
            if pipeline.is_dir() {
                cmd_run_dir(
                    &pipeline,
                    workdir.as_deref(),
                    dry_run,
                    max_budget_usd,
                    max_steps,
                    fresh,
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
            dry_run,
            validate,
        } => {
            if let Some(epic_id) = validate {
                let spec_content = std::fs::read_to_string(&spec_path)?;
                validate_decomposition(&spec_content, Some(&epic_id)).await?;
            } else {
                cmd_decompose(&spec_path, dry_run).await?;
            }
        }
        Commands::Scaffold { epic_id, output } => {
            cmd_scaffold(&epic_id, output.as_deref()).await?;
        }
        Commands::Generate {
            files,
            prd,
            spec,
            output,
        } => {
            // Check if the single positional arg is a directory
            if files.len() == 1 && files[0].is_dir() && prd.is_none() && spec.is_none() {
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
                    resolved_prd.as_deref(),
                    &resolved_spec,
                    output.as_deref(),
                    cli.verbose,
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
        } => {
            cmd_launch(
                &docs_dir,
                output.as_deref(),
                workdir.as_deref(),
                dry_run,
                max_budget_usd,
                max_steps,
                fresh,
                cli.verbose,
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
