use std::path::{Path, PathBuf};

use anyhow;

use super::plan_input::load_plan;

/// A Proposal: the Epic and Tasks a Plan decomposes into (spec C6).
///
/// `v` defaults to 1 so Claude's output without it still parses.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
struct Proposal {
    #[serde(default = "proposal_version")]
    v: u32,
    epic: EpicDef,
    tasks: Vec<TaskDef>,
    #[serde(default)]
    dependencies: Vec<DepDef>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
struct EpicDef {
    title: String,
    description: String,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
struct TaskDef {
    title: String,
    #[serde(default = "default_task_type")]
    r#type: String,
    #[serde(default = "default_priority")]
    priority: String,
    description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    acceptance: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    design: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    notes: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
struct DepDef {
    blocked: usize,
    blocker: usize,
}

fn proposal_version() -> u32 {
    1
}

fn default_task_type() -> String {
    "task".to_string()
}

fn default_priority() -> String {
    "P2".to_string()
}

impl Proposal {
    /// Pure checks that run before any `bd` call.
    fn validate(&self) -> Result<(), String> {
        if self.v != 1 {
            return Err(format!("unsupported Proposal version {}", self.v));
        }
        if self.tasks.is_empty() {
            return Err("Proposal has no Tasks".to_string());
        }
        for (i, task) in self.tasks.iter().enumerate() {
            if task.title.trim().is_empty() {
                return Err(format!("Task [{i}] has an empty title"));
            }
        }
        let count = self.tasks.len();
        for dep in &self.dependencies {
            if dep.blocked >= count || dep.blocker >= count {
                return Err(format!(
                    "dependency index out of range (blocked={}, blocker={}, tasks={count})",
                    dep.blocked, dep.blocker
                ));
            }
            if dep.blocked == dep.blocker {
                return Err(format!("Task [{}] cannot depend on itself", dep.blocked));
            }
        }
        Ok(())
    }
}

/// A failure with a machine-readable code, for the `--json` error payload (C6).
struct DecomposeError {
    code: &'static str,
    message: String,
}

impl DecomposeError {
    fn new(code: &'static str, message: impl std::fmt::Display) -> Self {
        Self {
            code,
            message: message.to_string(),
        }
    }
}

fn beads_error(e: attractor_pipeline::BeadsError) -> DecomposeError {
    let code = match e {
        attractor_pipeline::BeadsError::BdNotFound { .. } => "bd_not_found",
        _ => "beads_failed",
    };
    DecomposeError::new(code, e)
}

struct Created {
    epic_id: String,
    task_ids: Vec<String>,
    dep_count: usize,
}

/// Where the Plan text comes from.
pub enum DecomposeSource<'a> {
    Spec(&'a Path),
    Plan(&'a [PathBuf]),
    Proposal(&'a Path),
}

pub async fn cmd_decompose(
    source: DecomposeSource<'_>,
    dry_run: bool,
    json: bool,
) -> anyhow::Result<()> {
    match run(source, dry_run, json).await {
        Ok(()) => Ok(()),
        Err(error) => {
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
            anyhow::bail!(error.message)
        }
    }
}

async fn run(source: DecomposeSource<'_>, dry_run: bool, json: bool) -> Result<(), DecomposeError> {
    let (proposal, spec_content) = match source {
        DecomposeSource::Proposal(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| DecomposeError::new("io", format!("{}: {e}", path.display())))?;
            let proposal: Proposal = serde_json::from_str(&text).map_err(|e| {
                DecomposeError::new("invalid_proposal", format!("{}: {e}", path.display()))
            })?;
            (proposal, None)
        }
        DecomposeSource::Spec(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| DecomposeError::new("io", format!("{}: {e}", path.display())))?;
            let proposal = generate_proposal(&text).await?;
            (proposal, Some(text))
        }
        DecomposeSource::Plan(files) => {
            let text = load_plan(files).map_err(|e| DecomposeError::new("plan_input", e))?;
            let proposal = generate_proposal(&text).await?;
            (proposal, Some(text))
        }
    };

    if dry_run {
        proposal
            .validate()
            .map_err(|m| DecomposeError::new("invalid_proposal", m))?;
        if json {
            let payload = serde_json::json!({"v": 1, "ok": true, "proposal": proposal});
            println!("{payload}");
        } else {
            print_proposal(&proposal);
            if let Some(spec) = &spec_content {
                validate_decomposition(spec, None)
                    .await
                    .map_err(|e| DecomposeError::new("validation_failed", e))?;
            }
        }
        return Ok(());
    }

    let created = create_from_proposal(&attractor_pipeline::BeadsAdapter::new(), &proposal).await?;

    if json {
        let payload = serde_json::json!({
            "v": 1,
            "ok": true,
            "epic_id": created.epic_id,
            "task_ids": created.task_ids,
        });
        println!("{payload}");
        return Ok(());
    }

    println!("✓ Decomposition complete");
    println!("  Epic ID: {}", created.epic_id);
    println!("  Tasks created: {}", created.task_ids.len());
    println!("  Dependencies: {}", created.dep_count);

    // Post-decompose validation needs the source text; a Proposal file has none.
    if let Some(spec) = &spec_content {
        validate_decomposition(spec, Some(&created.epic_id))
            .await
            .map_err(|e| DecomposeError::new("validation_failed", e))?;
    }

    println!("\nNext steps:");
    println!("1. Review tasks: bd list");
    println!("2. Generate pipeline: pas scaffold {}", created.epic_id);

    Ok(())
}

fn build_prompt(spec_content: &str) -> String {
    format!(
        "Read this technical specification and output a JSON object describing an epic and its tasks.\n\n\
        SPEC:\n{}\n\n\
        INSTRUCTIONS:\n\
        \n\
        ## Output Format\n\
        Output ONLY a valid JSON object (no markdown fences, no commentary) with this structure:\n\
        {{\n\
          \"epic\": {{ \"title\": \"...\", \"description\": \"...\" }},\n\
          \"tasks\": [\n\
            {{\n\
              \"title\": \"...\",\n\
              \"type\": \"task\",\n\
              \"priority\": \"P2\",\n\
              \"description\": \"...\",\n\
              \"acceptance\": \"...\",\n\
              \"design\": \"...\",\n\
              \"notes\": \"...\"\n\
            }}\n\
          ],\n\
          \"dependencies\": [\n\
            {{ \"blocked\": 0, \"blocker\": 1 }}\n\
          ]\n\
        }}\n\
        \n\
        ## Structure\n\
        1. Extract the title from the spec (usually in the first heading) for the epic\n\
        2. Extract implementation phases/tasks from the spec sections\n\
        3. Priority should be P2 for most tasks unless critical (P1) or backlog (P3/P4)\n\
        4. Dependencies use task array indices (0-based). blocked depends on blocker.\n\
        \n\
        ## Task Content — PRESERVE ALL CONTEXT\n\
        Each task must contain ALL technical details needed to implement it without referring back to the spec.\n\
        An agent or developer picking up a ticket should have everything they need right there.\n\
        \n\
        Use these fields to carry the full context:\n\
        - description: High-level summary of what the task is and why (2-4 sentences)\n\
        - acceptance: Specific acceptance criteria — list the exact test names, assertions, expected behaviors, and edge cases.\n\
          Include function signatures, error types to check, and any numeric thresholds.\n\
        - design: Implementation details — code examples, file paths where code should go, architectural decisions,\n\
          design rationale, data structures, and any code snippets from the spec. This is where full code examples go.\n\
        - notes: Cross-references, warnings, gotchas, related tasks, CI considerations, and any \"IMPORTANT\" callouts from the spec.\n\
        \n\
        CRITICAL: Do NOT summarize or compress the spec content. If the spec has 60 lines of detail for a task, all 60 lines\n\
        of substance should be distributed across description, acceptance, design, and notes. The ticket IS the spec\n\
        for that unit of work. Lost context means wrong implementations.\n\
        \n\
        Output ONLY the JSON object. No other text.",
        spec_content
    )
}

/// Ask Claude to turn the Plan text into a Proposal.
async fn generate_proposal(spec_content: &str) -> Result<Proposal, DecomposeError> {
    let prompt = build_prompt(spec_content);

    // Call Claude CLI with JSON output format
    let mut cmd = tokio::process::Command::new("claude");
    cmd.arg("-p")
        .arg(&prompt)
        .arg("--output-format")
        .arg("json")
        .arg("--no-session-persistence");

    // Capture output
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let llm = |m: String| DecomposeError::new("llm_failed", m);
    let output_result = cmd.output().await.map_err(|e| llm(e.to_string()))?;

    if !output_result.status.success() {
        let stderr = String::from_utf8_lossy(&output_result.stderr);
        return Err(llm(format!("Claude CLI failed: {}", stderr)));
    }

    let output_json = String::from_utf8(output_result.stdout).map_err(|e| llm(e.to_string()))?;
    let parsed: serde_json::Value =
        serde_json::from_str(&output_json).map_err(|e| llm(e.to_string()))?;

    let result_str = parsed["result"]
        .as_str()
        .ok_or_else(|| llm("Claude output missing 'result' field".to_string()))?;

    // Strip markdown code fences if present (handles ```json, ```, etc.)
    let cleaned = strip_code_fences(result_str);

    serde_json::from_str(&cleaned).map_err(|e| {
        llm(format!(
            "Failed to parse Claude's JSON output: {}\n\nRaw output:\n{}",
            e, cleaned
        ))
    })
}

fn print_proposal(decompose: &Proposal) {
    println!("Decomposition (dry run):\n");
    println!("Epic: {}", decompose.epic.title);
    println!("  Description: {}", decompose.epic.description);
    println!("\nTasks ({}):", decompose.tasks.len());
    for (i, task) in decompose.tasks.iter().enumerate() {
        println!(
            "  [{}] {} (type={}, priority={})",
            i, task.title, task.r#type, task.priority
        );
        println!(
            "      Description: {}",
            truncate_for_display(&task.description, 120)
        );
        if let Some(ref a) = task.acceptance {
            println!("      Acceptance: {}", truncate_for_display(a, 120));
        }
        if let Some(ref d) = task.design {
            println!("      Design: {}", truncate_for_display(d, 120));
        }
        if let Some(ref n) = task.notes {
            println!("      Notes: {}", truncate_for_display(n, 120));
        }
    }
    println!("\nDependencies ({}):", decompose.dependencies.len());
    for dep in &decompose.dependencies {
        println!("  Task [{}] blocked by Task [{}]", dep.blocked, dep.blocker);
    }
}

/// Creates the Epic, its Tasks and dependencies exactly as `proposal` lists
/// them. Shared by the LLM path and `--from-proposal`; a Proposal that fails
/// `validate` never reaches `bd`.
async fn create_from_proposal(
    beads: &attractor_pipeline::BeadsAdapter,
    proposal: &Proposal,
) -> Result<Created, DecomposeError> {
    proposal
        .validate()
        .map_err(|m| DecomposeError::new("invalid_proposal", m))?;

    // Create the epic
    let epic_id = beads
        .create(&attractor_pipeline::NewIssue {
            title: &proposal.epic.title,
            issue_type: "epic",
            description: &proposal.epic.description,
            ..Default::default()
        })
        .await
        .map_err(|e| {
            DecomposeError::new("beads_failed", format!("Failed to create epic: {}", e))
        })?;

    // Create tasks and collect their IDs
    let mut task_ids: Vec<String> = Vec::with_capacity(proposal.tasks.len());

    for task in &proposal.tasks {
        let task_id = beads
            .create(&attractor_pipeline::NewIssue {
                title: &task.title,
                issue_type: &task.r#type,
                priority: Some(&task.priority),
                description: &task.description,
                acceptance: task.acceptance.as_deref(),
                design: task.design.as_deref(),
                notes: task.notes.as_deref(),
                parent: None,
            })
            .await
            .map_err(|e| {
                DecomposeError::new(
                    "beads_failed",
                    format!("Failed to create task '{}': {}", task.title, e),
                )
            })?;
        task_ids.push(task_id);
    }

    // Add all tasks as children of the epic
    for task_id in &task_ids {
        if let Err(e) = beads.add_dependency(&epic_id, task_id).await {
            if !matches!(e, attractor_pipeline::BeadsError::CommandFailed { .. }) {
                return Err(beads_error(e));
            }
            eprintln!(
                "Warning: failed to add epic dependency for {}: {}",
                task_id, e
            );
        }
    }

    // Add task-to-task dependencies
    let mut dep_count = 0;
    for dep in &proposal.dependencies {
        match beads
            .add_dependency(&task_ids[dep.blocked], &task_ids[dep.blocker])
            .await
        {
            Ok(()) => dep_count += 1,
            Err(e @ attractor_pipeline::BeadsError::CommandFailed { .. }) => eprintln!(
                "Warning: failed to add dependency [{} -> {}]: {}",
                task_ids[dep.blocked], task_ids[dep.blocker], e
            ),
            Err(e) => return Err(beads_error(e)),
        }
    }

    Ok(Created {
        epic_id,
        task_ids,
        dep_count,
    })
}

/// Strip markdown code fences from a string (e.g., ```json ... ```).
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

/// Truncate a string for display, replacing newlines with spaces.
fn truncate_for_display(s: &str, max_len: usize) -> String {
    let flat: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    if flat.len() > max_len {
        format!("{}...", &flat[..max_len])
    } else {
        flat
    }
}

#[path = "decompose_validate.rs"]
mod validate;
pub use validate::validate_decomposition;

#[cfg(test)]
mod tests {
    use super::*;

    fn proposal(tasks: usize, deps: &[(usize, usize)]) -> Proposal {
        Proposal {
            v: 1,
            epic: EpicDef {
                title: "E".into(),
                description: "d".into(),
            },
            tasks: (0..tasks)
                .map(|i| TaskDef {
                    title: format!("T{i}"),
                    r#type: "task".into(),
                    priority: "P2".into(),
                    description: "d".into(),
                    acceptance: None,
                    design: None,
                    notes: None,
                })
                .collect(),
            dependencies: deps
                .iter()
                .map(|&(blocked, blocker)| DepDef { blocked, blocker })
                .collect(),
        }
    }

    #[test]
    fn valid_proposal_passes() {
        assert!(proposal(3, &[(1, 0), (2, 1)]).validate().is_ok());
    }

    #[test]
    fn out_of_range_blocker_is_rejected() {
        let err = proposal(2, &[(0, 5)]).validate().unwrap_err();
        assert!(err.contains("blocker=5"), "{err}");
    }

    #[test]
    fn out_of_range_blocked_is_rejected() {
        assert!(proposal(2, &[(2, 0)]).validate().is_err());
    }

    #[test]
    fn self_dependency_is_rejected() {
        assert!(proposal(2, &[(1, 1)]).validate().is_err());
    }

    #[test]
    fn empty_task_list_is_rejected() {
        assert!(proposal(0, &[]).validate().is_err());
    }

    #[test]
    fn unsupported_version_is_rejected() {
        let mut p = proposal(1, &[]);
        p.v = 2;
        assert!(p.validate().is_err());
    }

    #[test]
    fn missing_version_defaults_to_one() {
        let p: Proposal = serde_json::from_str(
            r#"{"epic":{"title":"E","description":"d"},"tasks":[{"title":"T","description":"d"}]}"#,
        )
        .unwrap();
        assert_eq!(p.v, 1);
        assert_eq!(p.tasks[0].r#type, "task");
        assert_eq!(p.tasks[0].priority, "P2");
    }

    #[test]
    fn prompt_is_pinned() {
        let prompt = build_prompt("SPECTEXT");
        assert!(prompt.starts_with(
            "Read this technical specification and output a JSON object describing an epic and its tasks.\n\nSPEC:\nSPECTEXT\n\nINSTRUCTIONS:\n"
        ));
        assert!(prompt.ends_with("Output ONLY the JSON object. No other text."));
        assert!(prompt.contains("## Task Content — PRESERVE ALL CONTEXT"));
    }
}
