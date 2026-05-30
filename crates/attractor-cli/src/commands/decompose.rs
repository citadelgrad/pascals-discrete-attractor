use anyhow;

/// JSON schema for Claude's decompose output.
#[derive(serde::Deserialize, Debug)]
struct DecomposeOutput {
    epic: EpicDef,
    tasks: Vec<TaskDef>,
    #[serde(default)]
    dependencies: Vec<DepDef>,
}

#[derive(serde::Deserialize, Debug)]
struct EpicDef {
    title: String,
    description: String,
}

#[derive(serde::Deserialize, Debug)]
struct TaskDef {
    title: String,
    #[serde(default = "default_task_type")]
    r#type: String,
    #[serde(default = "default_priority")]
    priority: String,
    description: String,
    #[serde(default)]
    acceptance: Option<String>,
    #[serde(default)]
    design: Option<String>,
    #[serde(default)]
    notes: Option<String>,
}

#[derive(serde::Deserialize, Debug)]
struct DepDef {
    blocked: usize,
    blocker: usize,
}

fn default_task_type() -> String {
    "task".to_string()
}

fn default_priority() -> String {
    "P2".to_string()
}

pub async fn cmd_decompose(spec_path: &std::path::Path, dry_run: bool) -> anyhow::Result<()> {
    // Read the spec file
    let spec_content = std::fs::read_to_string(spec_path)?;

    // Build prompt for Claude to generate structured JSON
    let prompt = format!(
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
    );

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

    let output_result = cmd.output().await?;

    if !output_result.status.success() {
        let stderr = String::from_utf8_lossy(&output_result.stderr);
        anyhow::bail!("Claude CLI failed: {}", stderr);
    }

    let output_json = String::from_utf8(output_result.stdout)?;
    let parsed: serde_json::Value = serde_json::from_str(&output_json)?;

    let result_str = parsed["result"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Claude output missing 'result' field"))?;

    // Strip markdown code fences if present (handles ```json, ```, etc.)
    let cleaned = strip_code_fences(result_str);

    let decompose: DecomposeOutput = serde_json::from_str(&cleaned)
        .map_err(|e| anyhow::anyhow!("Failed to parse Claude's JSON output: {}\n\nRaw output:\n{}", e, cleaned))?;

    if dry_run {
        println!("Decomposition (dry run):\n");
        println!("Epic: {}", decompose.epic.title);
        println!("  Description: {}", decompose.epic.description);
        println!("\nTasks ({}):", decompose.tasks.len());
        for (i, task) in decompose.tasks.iter().enumerate() {
            println!("  [{}] {} (type={}, priority={})", i, task.title, task.r#type, task.priority);
            println!("      Description: {}", truncate_for_display(&task.description, 120));
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
        validate_decomposition(&spec_content, None).await?;
        return Ok(());
    }

    // Create the epic
    let epic_output = tokio::process::Command::new("bd")
        .args(["create", "--title", &decompose.epic.title, "--type", "epic",
               "--description", &decompose.epic.description, "--silent"])
        .output()
        .await?;

    if !epic_output.status.success() {
        let stderr = String::from_utf8_lossy(&epic_output.stderr);
        anyhow::bail!("Failed to create epic: {}", stderr);
    }

    let epic_id = String::from_utf8(epic_output.stdout)?.trim().to_string();

    // Create tasks and collect their IDs
    let mut task_ids: Vec<String> = Vec::with_capacity(decompose.tasks.len());

    for task in &decompose.tasks {
        let mut args = vec![
            "create".to_string(),
            "--title".to_string(), task.title.clone(),
            "--type".to_string(), task.r#type.clone(),
            "--priority".to_string(), task.priority.clone(),
            "--description".to_string(), task.description.clone(),
        ];

        if let Some(ref acceptance) = task.acceptance {
            args.push("--acceptance".to_string());
            args.push(acceptance.clone());
        }
        if let Some(ref design) = task.design {
            args.push("--design".to_string());
            args.push(design.clone());
        }
        if let Some(ref notes) = task.notes {
            args.push("--notes".to_string());
            args.push(notes.clone());
        }
        args.push("--silent".to_string());

        let task_output = tokio::process::Command::new("bd")
            .args(&args)
            .output()
            .await?;

        if !task_output.status.success() {
            let stderr = String::from_utf8_lossy(&task_output.stderr);
            anyhow::bail!("Failed to create task '{}': {}", task.title, stderr);
        }

        let task_id = String::from_utf8(task_output.stdout)?.trim().to_string();
        task_ids.push(task_id);
    }

    // Add all tasks as children of the epic
    for task_id in &task_ids {
        let dep_output = tokio::process::Command::new("bd")
            .args(["dep", "add", &epic_id, task_id])
            .output()
            .await?;

        if !dep_output.status.success() {
            let stderr = String::from_utf8_lossy(&dep_output.stderr);
            eprintln!("Warning: failed to add epic dependency for {}: {}", task_id, stderr);
        }
    }

    // Add task-to-task dependencies
    let mut dep_count = 0;
    for dep in &decompose.dependencies {
        if dep.blocked < task_ids.len() && dep.blocker < task_ids.len() {
            let dep_output = tokio::process::Command::new("bd")
                .args(["dep", "add", &task_ids[dep.blocked], &task_ids[dep.blocker]])
                .output()
                .await?;

            if !dep_output.status.success() {
                let stderr = String::from_utf8_lossy(&dep_output.stderr);
                eprintln!("Warning: failed to add dependency [{} -> {}]: {}",
                    task_ids[dep.blocked], task_ids[dep.blocker], stderr);
            } else {
                dep_count += 1;
            }
        } else {
            eprintln!("Warning: dependency index out of range (blocked={}, blocker={}, tasks={})",
                dep.blocked, dep.blocker, task_ids.len());
        }
    }

    println!("✓ Decomposition complete");
    println!("  Epic ID: {}", epic_id);
    println!("  Tasks created: {}", task_ids.len());
    println!("  Dependencies: {}", dep_count);

    // Run post-decompose validation
    validate_decomposition(&spec_content, Some(&epic_id)).await?;

    println!("\nNext steps:");
    println!("1. Review tasks: bd list");
    println!("2. Generate pipeline: pas scaffold {}", epic_id);

    Ok(())
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
