use std::path::PathBuf;
use std::str::FromStr;

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
        format!(
            "Resuming from checkpoint -- next node: {}",
            cp.current_node_id
        ),
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

fn prepare_run_configuration(
    path: &std::path::Path,
    workdir: Option<&std::path::Path>,
    dry_run: bool,
    max_budget_usd: Option<f64>,
    max_steps: Option<u64>,
    codergen_claude: &CodergenClaudeCliOpts,
) -> anyhow::Result<attractor_pipeline::RunConfiguration> {
    let graph = crate::load_pipeline(path)?;
    let plan = match attractor_pipeline::ExecutionPlan::compile(graph.clone()) {
        Ok(plan) => plan,
        Err(error) => {
            let diagnostics = attractor_pipeline::validate(&graph);
            super::print_diagnostics(&diagnostics);
            anyhow::bail!("Pipeline validation failed: {error}");
        }
    };
    let diagnostics = attractor_pipeline::validate_plan(&plan);
    if super::print_diagnostics(&diagnostics) {
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
) -> anyhow::Result<()> {
    let configured = prepare_run_configuration(
        path,
        workdir,
        dry_run,
        max_budget_usd,
        max_steps,
        codergen_claude,
    )?;
    let graph = configured.plan().graph();

    // Preflight checks: environment-level warnings that don't fail validation
    // but can cause silent problems at runtime (e.g. a codergen node with no
    // timeout falling back to the hardcoded 600s kill).
    for finding in attractor_pipeline::preflight_run_configuration(&configured) {
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
    if *configured.controls().dry_run().value() {
        println!("(dry run mode -- no LLM calls)");
    }
    if *configured.controls().claude().settings_mode().value()
        == attractor_pipeline::ClaudeSettingsMode::Inherit
    {
        println!(
            "WARNING: codergen Claude settings inheritance enabled; personal Claude Code hooks/settings may run."
        );
    }

    println!(
        "Working directory: {}",
        configured.controls().workdir().value().display()
    );
    if configured.controls().max_budget_usd().source()
        == attractor_pipeline::ConfigurationSource::Caller
    {
        let budget = configured.controls().max_budget_usd().value();
        println!("Budget limit: ${:.2}", budget);
    }
    println!("Step limit: {}", configured.controls().max_steps().value());

    let interviewer = std::sync::Arc::new(attractor_pipeline::ConsoleInterviewer);
    let registry = attractor_pipeline::default_registry_with_interviewer(interviewer);
    let executor = attractor_pipeline::PipelineExecutor::new(registry);
    let result = executor
        .run_configuration_with_checkpoint(&configured, attractor_types::Context::new(), &logs_dir)
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
    max_steps: Option<u64>,
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

        let result = cmd_run(
            &pipeline_path,
            None,
            Some(logs_dir.path()),
            true,
            None,
            Some(100),
            false,
            &CodergenClaudeCliOpts::default(),
        )
        .await;

        assert!(
            result.is_ok(),
            "cmd_run should not be blocked by provider_required when there is nothing to flag: {result:?}"
        );
    }
}
