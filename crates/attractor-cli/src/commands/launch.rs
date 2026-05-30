use std::path::PathBuf;

/// End-to-end: discover PRD+spec pairs → generate .dot files → validate → run.
#[allow(clippy::too_many_arguments)]
pub async fn cmd_launch(
    docs_dir: &std::path::Path,
    output_dir: Option<&std::path::Path>,
    workdir: Option<&std::path::Path>,
    dry_run: bool,
    max_budget_usd: Option<f64>,
    max_steps: u64,
    fresh: bool,
    verbose: bool,
) -> anyhow::Result<()> {
    let pipelines_dir = output_dir
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("pipelines"));

    // Step 1: Generate .dot files from spec+prd pairs
    println!("=== Step 1: Generate pipelines ===\n");
    let generated = super::cmd_generate_dir(docs_dir, Some(&pipelines_dir), verbose).await?;

    if generated.is_empty() {
        anyhow::bail!(
            "No pipelines were generated from {}\n\n\
             The directory must contain files ending in -spec.md. Each spec is\n\
             paired with a matching -prd.md if one exists (e.g. auth-spec.md\n\
             pairs with auth-prd.md). PRDs are optional but recommended.\n\n\
             Use zero-padded prefixes to control execution order:\n\
             \x20 phase-01-spec.md, phase-02-spec.md, ..., phase-11-spec.md",
            docs_dir.display()
        );
    }

    // Step 2: Validate all generated .dot files
    println!("\n=== Step 2: Validate pipelines ===\n");
    let mut validation_failed = false;
    for dot_path in &generated {
        let name = dot_path.file_name().unwrap_or_default().to_string_lossy();
        match crate::load_pipeline(dot_path) {
            Ok(graph) => {
                let diagnostics = attractor_pipeline::validate(&graph);
                let has_error = diagnostics
                    .iter()
                    .any(|d| matches!(d.severity, attractor_pipeline::Severity::Error));
                if has_error {
                    println!("[FAIL] {}", name);
                    for diag in &diagnostics {
                        if matches!(diag.severity, attractor_pipeline::Severity::Error) {
                            println!("  [ERROR] {}: {}", diag.rule, diag.message);
                        }
                    }
                    validation_failed = true;
                } else {
                    println!("[PASS] {}", name);
                }
            }
            Err(e) => {
                println!("[FAIL] {} — parse error: {}", name, e);
                validation_failed = true;
            }
        }
    }

    if validation_failed {
        anyhow::bail!(
            "Validation failed — fix the generated .dot files in {} before running.\n\
             You can re-run this command after fixing; completed generations are not re-done.",
            pipelines_dir.display()
        );
    }

    // Step 3: Run all pipelines sequentially
    println!("\n=== Step 3: Run pipelines ===\n");
    super::cmd_run_dir(
        &pipelines_dir,
        workdir,
        dry_run,
        max_budget_usd,
        max_steps,
        fresh,
    )
    .await?;

    println!("\nLaunch complete");
    Ok(())
}
