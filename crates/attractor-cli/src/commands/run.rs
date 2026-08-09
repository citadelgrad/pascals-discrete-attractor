use std::path::PathBuf;

use anyhow;

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
    fn has_inherit_mode(&self) -> bool {
        self.settings_mode
            .as_deref()
            .map(|mode| mode.eq_ignore_ascii_case("inherit"))
            .unwrap_or(false)
    }

    async fn apply_to_context(&self, context: &attractor_types::Context) {
        if let Some(value) = &self.settings_mode {
            context
                .set(
                    "codergen.claude.settings_mode",
                    serde_json::Value::String(value.clone()),
                )
                .await;
        }
        if let Some(value) = &self.setting_sources {
            context
                .set(
                    "codergen.claude.setting_sources",
                    serde_json::Value::String(value.clone()),
                )
                .await;
        }
        if let Some(value) = &self.settings {
            context
                .set(
                    "codergen.claude.settings",
                    serde_json::Value::String(value.clone()),
                )
                .await;
        }
        if let Some(value) = &self.tools {
            context
                .set(
                    "codergen.claude.tools",
                    serde_json::Value::String(value.clone()),
                )
                .await;
        }
        if let Some(value) = &self.agents {
            context
                .set(
                    "codergen.claude.agents",
                    serde_json::Value::String(value.clone()),
                )
                .await;
        }
        if !self.plugin_dirs.is_empty() {
            context
                .set(
                    "codergen.claude.plugin_dirs",
                    serde_json::Value::Array(
                        self.plugin_dirs
                            .iter()
                            .map(|path| {
                                serde_json::Value::String(path.to_string_lossy().into_owned())
                            })
                            .collect(),
                    ),
                )
                .await;
        }
        if let Some(value) = &self.mcp_config {
            context
                .set(
                    "codergen.claude.mcp_config",
                    serde_json::Value::String(value.clone()),
                )
                .await;
        }
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
fn print_resume_banner(cp: &attractor_pipeline::PipelineCheckpoint) {
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

    print_highlighted(&[
        format!("Resuming from checkpoint -- next node: {}", cp.current_node_id),
        detail,
    ]);
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

/// Print lines inside a border, bolded and colored when stdout is a
/// terminal. Falls back to a plain ASCII box (no ANSI codes) when output is
/// redirected, so logs and CI output stay clean.
fn print_highlighted(lines: &[String]) {
    use std::io::IsTerminal;

    let color = std::io::stdout().is_terminal();
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

    println!("{cyan}+{border}+{reset}");
    for line in lines {
        let pad = " ".repeat(width - line.chars().count());
        println!("{cyan}|{reset}  {bold}{line}{pad}{reset}  {cyan}|{reset}");
    }
    println!("{cyan}+{border}+{reset}");
}

#[allow(clippy::too_many_arguments)]
pub async fn cmd_run(
    path: &std::path::Path,
    workdir: Option<&std::path::Path>,
    logs: Option<&std::path::Path>,
    dry_run: bool,
    max_budget_usd: Option<f64>,
    max_steps: u64,
    fresh: bool,
    codergen_claude: &CodergenClaudeCliOpts,
) -> anyhow::Result<()> {
    let graph = crate::load_pipeline(path)?;

    // Preflight checks: environment-level warnings that don't fail validation
    // but can cause silent problems at runtime (e.g. a codergen node with no
    // timeout falling back to the hardcoded 600s kill).
    let preflight_workdir = workdir
        .map(|d| d.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    for finding in attractor_pipeline::preflight_run(&graph, &preflight_workdir) {
        let severity = match finding.severity {
            attractor_pipeline::PreflightSeverity::Warn => "WARN",
            attractor_pipeline::PreflightSeverity::Error => "ERROR",
        };
        println!("[{}] {}: {}", severity, finding.code, finding.message);
        if let Some(suggestion) = &finding.suggestion {
            println!("  suggestion: {}", suggestion);
        }
    }

    // Resolve logs directory: explicit flag or deterministic from path
    let logs_dir = match logs {
        Some(l) => l.to_path_buf(),
        None => stable_logs_dir(path),
    };

    // --fresh: clear any existing checkpoint before starting
    if fresh {
        attractor_pipeline::clear_checkpoint(&logs_dir).await?;
    }

    // Check for existing checkpoint. Loaded (not just existence-checked) so
    // the resume banner can show real progress instead of a bare notice.
    let checkpoint = attractor_pipeline::load_checkpoint(&logs_dir).await?;

    println!("Running pipeline: {}", graph.name);
    if !graph.goal.is_empty() {
        println!("Goal: {}", graph.goal);
    }
    println!("Logs: {}", logs_dir.display());
    if let Some(cp) = &checkpoint {
        print_resume_banner(cp);
    }
    if dry_run {
        println!("(dry run mode -- no LLM calls)");
    }
    if codergen_claude.has_inherit_mode() {
        println!(
            "WARNING: codergen Claude settings inheritance enabled; personal Claude Code hooks/settings may run."
        );
    }

    // Set up the pipeline context with workdir
    let context = attractor_types::Context::new();
    if let Some(dir) = workdir {
        let abs = std::fs::canonicalize(dir)?;
        context
            .set(
                "workdir",
                serde_json::Value::String(abs.to_string_lossy().into_owned()),
            )
            .await;
        println!("Working directory: {}", abs.display());
    }
    if dry_run {
        context.set("dry_run", serde_json::Value::Bool(true)).await;
    }
    codergen_claude.apply_to_context(&context).await;

    // Safety limits
    if let Some(budget) = max_budget_usd {
        context
            .set("max_budget_usd", serde_json::json!(budget))
            .await;
        println!("Budget limit: ${:.2}", budget);
    }
    context.set("max_steps", serde_json::json!(max_steps)).await;
    println!("Step limit: {}", max_steps);

    let interviewer = std::sync::Arc::new(attractor_pipeline::ConsoleInterviewer);
    let registry = attractor_pipeline::default_registry_with_interviewer(interviewer);
    let executor = attractor_pipeline::PipelineExecutor::new(registry);
    let result = executor
        .run_with_checkpoint(&graph, context, &logs_dir)
        .await?;

    println!("\nPipeline completed");
    println!("Completed nodes: {:?}", result.completed_nodes);

    // Print cost summary
    if result.total_cost > 0.0 {
        println!("Total cost: ${:.4}", result.total_cost);
    }

    Ok(())
}

/// Run a directory of .dot files sequentially with a cross-file manifest.
/// Files are sorted lexically — use zero-padded names (phase-01, phase-02).
pub async fn cmd_run_dir(
    dir: &std::path::Path,
    workdir: Option<&std::path::Path>,
    dry_run: bool,
    max_budget_usd: Option<f64>,
    max_steps: u64,
    fresh: bool,
    codergen_claude: &CodergenClaudeCliOpts,
) -> anyhow::Result<()> {
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
