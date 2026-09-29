use anyhow;
use serde::Serialize;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::normalize_provider_defaults;
use super::plan_input::load_plan;

/// Spawn a braille spinner on stderr. Returns a guard that stops it on drop.
fn start_spinner(message: &str) -> SpinnerGuard {
    const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

    let done = Arc::new(AtomicBool::new(false));
    let done_clone = done.clone();
    let msg = message.to_string();

    let handle = std::thread::spawn(move || {
        let mut i = 0usize;
        let start = std::time::Instant::now();
        while !done_clone.load(Ordering::Relaxed) {
            let elapsed = start.elapsed().as_secs();
            let mins = elapsed / 60;
            let secs = elapsed % 60;
            eprint!(
                "\r\x1b[2K\x1b[36m{}\x1b[0m {} \x1b[2m{}:{:02}\x1b[0m",
                FRAMES[i % FRAMES.len()],
                msg,
                mins,
                secs
            );
            let _ = std::io::stderr().flush();
            std::thread::sleep(std::time::Duration::from_millis(80));
            i += 1;
        }
        // Clear the spinner line
        eprint!("\r\x1b[2K");
        let _ = std::io::stderr().flush();
    });

    SpinnerGuard {
        done,
        handle: Some(handle),
    }
}

struct SpinnerGuard {
    done: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Drop for SpinnerGuard {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// What `pas generate` reads: a spec (with optional PRD), or an ordered Plan.
pub enum GenerateInput<'a> {
    SpecPrd {
        spec: &'a Path,
        prd: Option<&'a Path>,
    },
    Plan(&'a [PathBuf]),
}

/// A `pas generate` failure. In `--json` mode it is printed as
/// `{"v":1,"ok":false,"error":{..}}` (C6).
#[derive(Debug)]
struct GenerateError {
    code: &'static str,
    message: String,
}

impl GenerateError {
    fn new(code: &'static str, message: impl std::fmt::Display) -> Self {
        Self {
            code,
            message: message.to_string(),
        }
    }
}

/// A Pipeline written to disk, with what the printers report about it.
struct Generated {
    output_path: PathBuf,
    defaulted_providers: Vec<String>,
    errors: Vec<attractor_pipeline::Diagnostic>,
    node_count: usize,
}

/// `pas generate --json` success payload (C6), fields in contract order.
#[derive(Serialize)]
struct GenerateJson {
    v: u32,
    ok: bool,
    pipeline_path: PathBuf,
}

pub async fn cmd_generate(
    input: GenerateInput<'_>,
    output: Option<&Path>,
    verbose: bool,
    json: bool,
) -> anyhow::Result<()> {
    let result = generate(&input, output, verbose, json).await;
    if json {
        return print_json(result);
    }
    let generated = result.map_err(|e| anyhow::anyhow!(e.message))?;
    print_human(&input, &generated);
    Ok(())
}

async fn generate(
    input: &GenerateInput<'_>,
    output: Option<&Path>,
    verbose: bool,
    quiet: bool,
) -> Result<Generated, GenerateError> {
    let prompt = match input {
        GenerateInput::SpecPrd { spec, prd } => {
            // Read spec (required)
            let spec_content = std::fs::read_to_string(spec).map_err(|e| {
                GenerateError::new(
                    "io",
                    format!("Failed to read spec file '{}': {}", spec.display(), e),
                )
            })?;

            // Read PRD (optional)
            let prd_content = match prd {
                Some(path) => Some(std::fs::read_to_string(path).map_err(|e| {
                    GenerateError::new(
                        "io",
                        format!("Failed to read PRD file '{}': {}", path.display(), e),
                    )
                })?),
                None => None,
            };

            if verbose {
                eprintln!(
                    "[debug] spec: {} ({} bytes)",
                    spec.display(),
                    spec_content.len()
                );
                if let Some(p) = prd {
                    eprintln!(
                        "[debug] prd: {} ({} bytes)",
                        p.display(),
                        prd_content.as_ref().map_or(0, |c| c.len())
                    );
                }
            }
            build_prompt(&spec_content, prd_content.as_deref())
        }
        GenerateInput::Plan(files) => {
            let plan_text =
                load_plan(files).map_err(|e| GenerateError::new("plan_input", format!("{e:#}")))?;
            if verbose {
                eprintln!(
                    "[debug] plan: {} file(s) ({} bytes)",
                    files.len(),
                    plan_text.len()
                );
            }
            build_plan_prompt(&plan_text)
        }
    };

    if verbose {
        eprintln!("[debug] prompt: {} bytes", prompt.len());
        eprintln!("[debug] cmd: claude -p - --model sonnet --settings '{{\"enabledPlugins\":{{}}}}' --strict-mcp-config '{{}}' --tools '' --output-format json --no-session-persistence");
    }

    let llm = |e: &dyn std::fmt::Display| GenerateError::new("llm_failed", e);

    // Call Claude CLI with spinner — disable plugins/MCP/skills for speed
    let mut cmd = tokio::process::Command::new("claude");
    cmd.arg("-p")
        .arg("-")
        .arg("--model")
        .arg("sonnet")
        .arg("--system-prompt")
        .arg("You are a Graphviz DOT generator. Output ONLY a raw digraph. No commentary, no markdown fences, no skill invocations, no function calls.")
        .arg("--settings")
        .arg(r#"{"enabledPlugins":{}}"#)
        .arg("--strict-mcp-config")
        .arg("{}")
        .arg("--tools")
        .arg("")
        .arg("--output-format")
        .arg("json")
        .arg("--no-session-persistence");

    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let gen_start = std::time::Instant::now();
    // --json keeps stderr free of spinner frames too
    let spinner = (!quiet).then(|| start_spinner("Generating pipeline from spec..."));
    let mut child = cmd.spawn().map_err(|e| llm(&e))?;

    // Write prompt to stdin, then close it
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        stdin
            .write_all(prompt.as_bytes())
            .await
            .map_err(|e| llm(&e))?;
        // stdin is dropped here, closing the pipe
    }

    let output_result = child.wait_with_output().await.map_err(|e| llm(&e))?;
    drop(spinner);
    let gen_elapsed = gen_start.elapsed();
    if !quiet {
        eprintln!("Claude responded in {:.1}s", gen_elapsed.as_secs_f64());
    }

    if !output_result.status.success() {
        let stderr = String::from_utf8_lossy(&output_result.stderr);
        if verbose {
            let stdout = String::from_utf8_lossy(&output_result.stdout);
            eprintln!("[debug] exit code: {:?}", output_result.status.code());
            eprintln!("[debug] stdout: {}", &stdout[..stdout.len().min(1000)]);
            eprintln!("[debug] stderr: {}", &stderr[..stderr.len().min(1000)]);
        }
        return Err(llm(&format!("Claude CLI failed: {}", stderr)));
    }

    let output_json = String::from_utf8(output_result.stdout).map_err(|e| llm(&e))?;

    if verbose {
        eprintln!("[debug] response json: {} bytes", output_json.len());
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&output_json) {
            if let Some(cost) = parsed.get("total_cost_usd") {
                eprintln!("[debug] cost: ${}", cost);
            }
            if let Some(usage) = parsed.get("usage") {
                eprintln!("[debug] usage: {}", usage);
            }
        }
    }

    let parsed: serde_json::Value = serde_json::from_str(&output_json).map_err(|e| llm(&e))?;

    let result_str = parsed["result"]
        .as_str()
        .ok_or_else(|| llm(&"Claude output missing 'result' field"))?;

    // Extract the digraph from Claude's response (handles preamble, fences, etc.)
    let dot_content = match extract_digraph(result_str) {
        Some(d) => d,
        None => {
            eprintln!("No digraph found in Claude's response. First 500 chars:");
            eprintln!("{}", &result_str[..result_str.len().min(500)]);
            return Err(GenerateError::new(
                "invalid_dot",
                "Claude did not produce a valid digraph",
            ));
        }
    };

    if verbose {
        eprintln!("[debug] extracted DOT: {} bytes", dot_content.len());
        eprintln!(
            "[debug] first 200 chars:\n{}",
            &dot_content[..dot_content.len().min(200)]
        );
    }

    // Fill in any missing llm_provider on runtime nodes before writing to
    // disk. build_prompt already asks Claude to set it explicitly, but this
    // normalization step is the actual guarantee: a generated pipeline must
    // never depend on an implicit provider default, no matter what the LLM
    // produced.
    let (dot_content, defaulted_providers) =
        match normalize_provider_defaults(&dot_content, "claude") {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("Generated DOT failed to parse for provider normalization:");
                eprintln!("{}", &dot_content[..dot_content.len().min(500)]);
                return Err(GenerateError::new(
                    "invalid_dot",
                    format!("Generated pipeline is not valid DOT: {}", e),
                ));
            }
        };

    // Determine output path
    let output_path = match output {
        Some(path) => path.to_path_buf(),
        None => {
            // A Plan is ordered PRD then spec, so the last file names the Pipeline
            let named = match input {
                GenerateInput::SpecPrd { spec, .. } => Some(*spec),
                GenerateInput::Plan(files) => files.last().map(|p| p.as_path()),
            };
            let stem = named
                .and_then(|p| p.file_stem())
                .and_then(|s| s.to_str())
                .unwrap_or("pipeline");
            PathBuf::from(format!("pipelines/{}.dot", stem))
        }
    };

    let io = |e: std::io::Error| GenerateError::new("io", e);

    // Create parent directory if needed
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent).map_err(io)?;
    }

    // Write the pipeline file
    std::fs::write(&output_path, &dot_content).map_err(io)?;

    // Validate the generated pipeline
    let plan = match crate::load_execution_plan(&output_path) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("Generated file written to: {}", output_path.display());
            eprintln!("DOT parse failed — first 500 chars of output:");
            eprintln!("{}", &dot_content[..dot_content.len().min(500)]);
            return Err(GenerateError::new(
                "invalid_dot",
                format!("Generated pipeline is not valid DOT: {}", e),
            ));
        }
    };
    let errors = attractor_pipeline::validate_plan(&plan)
        .into_iter()
        .filter(|d| matches!(d.severity, attractor_pipeline::Severity::Error))
        .collect();

    Ok(Generated {
        output_path,
        defaulted_providers,
        errors,
        node_count: plan.all_nodes().count(),
    })
}

/// Exactly one JSON object on stdout; notices and diagnostics go to stderr.
fn print_json(result: Result<Generated, GenerateError>) -> anyhow::Result<()> {
    let result = result.and_then(|generated| {
        if !generated.defaulted_providers.is_empty() {
            eprintln!(
                "  Defaulted llm_provider=\"claude\" on: {}",
                generated.defaulted_providers.join(", ")
            );
        }
        if generated.errors.is_empty() {
            return Ok(generated);
        }
        for diag in &generated.errors {
            eprintln!("  [ERROR] {}: {}", diag.rule, diag.message);
        }
        Err(GenerateError::new(
            "invalid_pipeline",
            format!(
                "Pipeline {} was written but has {} validation error(s)",
                generated.output_path.display(),
                generated.errors.len()
            ),
        ))
    });
    let result = result.and_then(|generated| {
        std::fs::canonicalize(&generated.output_path).map_err(|e| GenerateError::new("io", e))
    });
    match result {
        Ok(pipeline_path) => {
            let payload = GenerateJson {
                v: 1,
                ok: true,
                pipeline_path,
            };
            println!("{}", serde_json::to_string(&payload)?);
            Ok(())
        }
        Err(error) => {
            println!(
                "{}",
                serde_json::json!({
                    "v": 1,
                    "ok": false,
                    "error": {"code": error.code, "message": error.message},
                })
            );
            anyhow::bail!(error.message)
        }
    }
}

fn print_human(input: &GenerateInput<'_>, generated: &Generated) {
    if !generated.defaulted_providers.is_empty() {
        println!(
            "  Defaulted llm_provider=\"claude\" on: {}",
            generated.defaulted_providers.join(", ")
        );
    }

    let has_error = !generated.errors.is_empty();
    if has_error {
        println!("Warning: pipeline has validation errors:");
        for diag in &generated.errors {
            println!("  [ERROR] {}: {}", diag.rule, diag.message);
        }
    }

    println!("Pipeline generated");
    println!("  Output: {}", generated.output_path.display());
    match input {
        GenerateInput::SpecPrd { spec, prd } => {
            println!("  Spec: {}", spec.display());
            if let Some(prd) = prd {
                println!("  PRD: {}", prd.display());
            }
        }
        GenerateInput::Plan(files) => {
            let names: Vec<String> = files.iter().map(|f| f.display().to_string()).collect();
            println!("  Plan: {}", names.join(", "));
        }
    }
    println!("  Nodes: {}", generated.node_count);
    println!(
        "  Validation: {}",
        if has_error { "FAILED" } else { "PASSED" }
    );

    if !has_error {
        println!("\nNext steps:");
        println!(
            "1. Review pipeline: cat {}",
            generated.output_path.display()
        );
        println!(
            "2. Run pipeline: pas run {} -w .",
            generated.output_path.display()
        );
    }
}

/// Input section for a spec (and optional PRD).
fn build_prompt(spec: &str, prd: Option<&str>) -> String {
    let prd_section = match prd {
        Some(content) => format!("## PRD (Product Requirements Document)\n\n{}\n\n", content),
        None => String::new(),
    };

    render_prompt(&format!(
        "{prd_section}## Technical Specification\n\n{spec}\n\n"
    ))
}

/// Prompt for an ordered multi-file Plan; `load_plan` supplies the headings.
fn build_plan_prompt(plan_text: &str) -> String {
    render_prompt(&format!("## Plan\n\n{plan_text}\n\n"))
}

fn render_prompt(input_section: &str) -> String {
    format!(
        r#"Generate a Graphviz DOT pipeline for an AI workflow engine. Each provider-backed `codergen` node runs its explicitly selected local provider CLI with the `prompt` attribute as its task.

{input_section}## Pipeline conventions

IMPORTANT: ALL attribute values MUST be double-quoted. Use `shape="Mdiamond"` not `shape=Mdiamond`. Use multi-line node declarations with one attribute per line.

Shapes: `"Mdiamond"` = start, `"Msquare"` = done, `"box"` = work, `"diamond"` (node_type="conditional") = decision, `"hexagon"` (node_type="wait.human") = human gate.
Graph attrs: `label`, `goal`, `model="sonnet"`.
Node attrs: `label`, `shape`, `prompt` (self-contained instructions with ALL context from the spec — no references to external tickets).
Optional: `allowed_tools` (e.g. "Read,Grep,Glob"), `goal_gate="true"`, `llm_model`.
Edge attrs: `label` (e.g. "PASS","FAIL"), `condition` (e.g. preferred_label=PASS), `loop_restart="true"` on back-edges.

## Provider (REQUIRED on every provider-backed node)

Every node whose resolved handler is `codergen` MUST have an explicit `llm_provider` attribute (e.g. `llm_provider="claude"`). This includes conventional `box` tasks, prompted `diamond` conditionals, and any node that explicitly sets `type="codergen"`. An unprompted `diamond` is pass-through and needs no provider unless `type="codergen"` explicitly selects provider-backed execution. Start, exit, quality, tool, human-gate, parallel, and pass-through conditional handlers do not consume providers. Do not rely on an implicit default: a pipeline author must be able to read the DOT file and see which provider each provider-backed node runs on. Use `llm_provider="claude"` unless the spec says otherwise.

## Timeouts

Every node MUST have a `timeout` attribute. Set it based on complexity:
- Lightweight (conditionals, haiku routing, simple file reads): `timeout="120s"`
- Medium (investigation, verification, fixups, linting): `timeout="300s"`
- Heavy (implementation, full test suites, multi-step builds): `timeout="900s"`

Example node format:
    my_node [
        label="Short Label"
        shape="box"
        llm_provider="claude"
        timeout="300s"
        prompt="Detailed instructions here."
    ]

## Structure

start -> [task1] -> [verify1] -PASS-> [task2] -> ... -> commit_changes -> done
                        \-FAIL-> [fixup1] -> [verify1] (loop_restart=true)

Each spec task becomes a work node + verify diamond. FAIL edges loop through a fixup node. PASS edges advance. Node prompts must be fully self-contained.

## Commit step (REQUIRED)

The LAST work node before `done` MUST be a commit node that stages and commits all changes:
    commit_changes [
        label="Commit Changes"
        shape="box"
        llm_provider="claude"
        timeout="120s"
        allowed_tools="Bash(git:*)"
        prompt="Stage and commit all changes made by this pipeline.
1. Run git diff --stat to review what changed
2. Stage the changed files: git add -A
3. Commit with a descriptive message summarizing the work done"
    ]

Output ONLY the raw digraph. No markdown fences, no commentary."#,
        input_section = input_section,
    )
}

/// Extract a DOT digraph from Claude's response.
/// Handles: raw digraph, markdown-fenced digraph, digraph buried in preamble text.
fn extract_digraph(s: &str) -> Option<String> {
    // First strip markdown code fences if present
    let stripped = strip_code_fences(s);

    // Find "digraph" keyword
    let start = stripped.find("digraph")?;

    // Find the opening brace
    let brace_start = stripped[start..].find('{')? + start;

    // Walk forward matching braces to find the closing one
    let mut depth = 0;
    let mut end = None;
    for (i, ch) in stripped[brace_start..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(brace_start + i + 1);
                    break;
                }
            }
            _ => {}
        }
    }

    end.map(|e| stripped[start..e].to_string())
}

/// Strip markdown code fences from a string (e.g., ```dot ... ```).
fn strip_code_fences(s: &str) -> String {
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() > 2
        && lines[0].starts_with("```")
        && lines.last().is_some_and(|l| l.trim() == "```")
    {
        lines[1..lines.len() - 1].join("\n")
    } else {
        s.to_string()
    }
}

/// Generate pipelines from a directory of PRD+spec pairs.
///
/// Scans `dir` for files matching `*-spec.md`, pairs each with a
/// corresponding `*-prd.md` (if present), and generates one .dot file per
/// pair.  Output names are zero-padded to sort correctly.
pub async fn cmd_generate_dir(
    dir: &std::path::Path,
    output_dir: Option<&std::path::Path>,
    verbose: bool,
    // ponytail: force=false skips existing .dot files so resume doesn't clobber valid checkpoints
    force: bool,
) -> anyhow::Result<Vec<std::path::PathBuf>> {
    // Discover *-spec.md files, sorted
    let mut specs: Vec<std::path::PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|ext| ext == "md")
                && p.file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s.ends_with("-spec"))
        })
        .collect();
    specs.sort();

    if specs.is_empty() {
        anyhow::bail!(
            "No *-spec.md files found in {}\n\n\
             Spec files must have names ending in -spec.md (e.g. auth-spec.md,\n\
             phase-01-spec.md). Each spec is paired with a matching -prd.md if\n\
             one exists (auth-spec.md pairs with auth-prd.md). PRDs are optional.\n\n\
             Use zero-padded prefixes to control generation order:\n\
             \x20 phase-01-spec.md, phase-02-spec.md, ..., phase-11-spec.md",
            dir.display()
        );
    }

    let out_dir = output_dir
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("pipelines"));
    std::fs::create_dir_all(&out_dir)?;

    println!(
        "Found {} spec(s) in {} (lexical order):",
        specs.len(),
        dir.display()
    );
    for spec in &specs {
        let stem = spec.file_stem().and_then(|s| s.to_str()).unwrap_or("?");
        let prd_stem = stem.replace("-spec", "-prd");
        let prd_path = dir.join(format!("{}.md", prd_stem));
        let prd_status = if prd_path.exists() { "+ PRD" } else { "no PRD" };
        println!(
            "  {} ({})",
            spec.file_name().unwrap_or_default().to_string_lossy(),
            prd_status
        );
    }
    println!();

    let mut generated: Vec<std::path::PathBuf> = Vec::new();

    for (i, spec_path) in specs.iter().enumerate() {
        let stem = spec_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("pipeline");

        // Pair with PRD: replace "-spec" suffix with "-prd"
        let prd_stem = stem.replace("-spec", "-prd");
        let prd_path = dir.join(format!("{}.md", prd_stem));
        let prd = if prd_path.exists() {
            Some(prd_path.as_path())
        } else {
            None
        };

        let output_path = out_dir.join(format!("{}.dot", stem));

        if !force && output_path.exists() {
            println!(
                "[{}/{}] {} (skipped — {} already exists; use --fresh to regenerate)",
                i + 1,
                specs.len(),
                spec_path.file_name().unwrap_or_default().to_string_lossy(),
                output_path.display()
            );
            generated.push(output_path);
            continue;
        }

        println!(
            "[{}/{}] {} {}→ {}",
            i + 1,
            specs.len(),
            spec_path.file_name().unwrap_or_default().to_string_lossy(),
            if prd.is_some() { "(with PRD) " } else { "" },
            output_path.display()
        );

        cmd_generate(
            GenerateInput::SpecPrd {
                spec: spec_path,
                prd,
            },
            Some(&output_path),
            verbose,
            false,
        )
        .await?;
        generated.push(output_path);
    }

    println!("\nGenerated {} pipeline(s)", generated.len());
    Ok(generated)
}

#[cfg(test)]
#[path = "generate_tests.rs"]
mod tests;
