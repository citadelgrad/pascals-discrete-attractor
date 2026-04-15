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

    let decompose: DecomposeOutput = serde_json::from_str(&cleaned).map_err(|e| {
        anyhow::anyhow!(
            "Failed to parse Claude's JSON output: {}\n\nRaw output:\n{}",
            e,
            cleaned
        )
    })?;

    if dry_run {
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
        validate_decomposition(&spec_content, None).await?;
        return Ok(());
    }

    // Create the epic
    let epic_output = tokio::process::Command::new("bd")
        .args([
            "create",
            "--title",
            &decompose.epic.title,
            "--type",
            "epic",
            "--description",
            &decompose.epic.description,
            "--silent",
        ])
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
            "--title".to_string(),
            task.title.clone(),
            "--type".to_string(),
            task.r#type.clone(),
            "--priority".to_string(),
            task.priority.clone(),
            "--description".to_string(),
            task.description.clone(),
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
            eprintln!(
                "Warning: failed to add epic dependency for {}: {}",
                task_id, stderr
            );
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
                eprintln!(
                    "Warning: failed to add dependency [{} -> {}]: {}",
                    task_ids[dep.blocked], task_ids[dep.blocker], stderr
                );
            } else {
                dep_count += 1;
            }
        } else {
            eprintln!(
                "Warning: dependency index out of range (blocked={}, blocker={}, tasks={})",
                dep.blocked,
                dep.blocker,
                task_ids.len()
            );
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
        && lines.last().map_or(false, |l| l.trim() == "```")
    {
        lines[1..lines.len() - 1].join("\n")
    } else {
        s.to_string()
    }
}

/// Truncate a string for display, replacing newlines with spaces.
fn truncate_for_display(s: &str, max_len: usize) -> String {
    let flat: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    if flat.chars().count() > max_len {
        let truncated: String = flat.chars().take(max_len).collect();
        format!("{}...", truncated)
    } else {
        flat
    }
}

/// Validate decomposition results. When `epic_id` is Some, fetches tickets and
/// checks field completeness + spec coverage. When None (dry-run), just reports
/// the identifiers extracted from the spec that will be tracked.
pub async fn validate_decomposition(
    spec_content: &str,
    epic_id: Option<&str>,
) -> anyhow::Result<()> {
    use std::collections::HashSet;

    // Extract key identifiers from spec
    // Only test names (not general fn names — those are too noisy from code examples)
    let re_test = regex::Regex::new(r"(test_\w+)").unwrap();
    let re_path = regex::Regex::new(r"(?:src|tests)/[\w/]+\.(?:rs|ndjson)").unwrap();
    let re_qualified = regex::Regex::new(r"([A-Z]\w+::\w+)").unwrap();
    let re_header = regex::Regex::new(r"####\s+`([^`]+)`").unwrap();

    // Common stdlib/language types whose ::method calls are noise, not spec identifiers
    let stdlib_prefixes: HashSet<&str> = [
        "Arc",
        "Box",
        "Cell",
        "Cow",
        "HashMap",
        "HashSet",
        "Mutex",
        "Option",
        "PathBuf",
        "Rc",
        "RefCell",
        "Result",
        "RwLock",
        "String",
        "Vec",
        "AtomicBool",
        "AtomicU32",
        "AtomicU64",
        "AtomicUsize",
        "Ordering",
        "BufReader",
        "BufWriter",
        "Duration",
        "Instant",
        "SystemTime",
        "Command",
        "Path",
        "File",
        "Sender",
        "Receiver",
        "JoinHandle",
        "TcpStream",
        "TcpListener",
        "Bytes",
        "BytesMut",
        "Pin",
        "Waker",
        "Some",
        "None",
        "Ok",
        "Err",
        "Default",
        "Display",
        "Debug",
        "From",
        "Into",
        "TryFrom",
        "TryInto",
        "Iterator",
        "IntoIterator",
        "Read",
        "Write",
        "Seek",
        "BufRead",
        "AsRef",
        "Deref",
        "Serialize",
        "Deserialize",
        "Clone",
        "Copy",
        "Send",
        "Sync",
        "PhantomData",
        "ManuallyDrop",
        "MaybeUninit",
        // std::io and error types
        "Error",
        "ErrorKind",
        "IoError",
        // tokio types
        "Mutex",
        "RwLock",
        "Notify",
        "Semaphore",
        "Barrier",
        "OwnedSemaphorePermit",
        "JoinSet",
        "JoinError",
        // serde/common crate types
        "Value",
        "Map",
        "Number",
        "Formatter",
        // test assertion types
        "Assert",
        "Assertion",
    ]
    .into_iter()
    .collect();

    // Generic trait method suffixes that aren't meaningful on any type
    let noise_suffixes = [
        "::clone",
        "::into",
        "::unwrap",
        "::expect",
        "::is_ok",
        "::is_err",
        "::is_some",
        "::is_none",
        "::as_ref",
        "::as_str",
        "::as_bytes",
        "::as_slice",
        "::to_string",
        "::to_owned",
        "::to_vec",
    ];

    let mut identifiers = HashSet::new();

    for cap in re_test.captures_iter(spec_content) {
        identifiers.insert(cap[1].to_string());
    }
    for mat in re_path.find_iter(spec_content) {
        identifiers.insert(mat.as_str().to_string());
    }
    for cap in re_qualified.captures_iter(spec_content) {
        let full = &cap[1];
        // Extract the type prefix (before ::)
        let prefix = full.split("::").next().unwrap_or("");
        if stdlib_prefixes.contains(prefix) {
            continue;
        }
        if noise_suffixes.iter().any(|s| full.ends_with(s)) {
            continue;
        }
        identifiers.insert(full.to_string());
    }
    for cap in re_header.captures_iter(spec_content) {
        identifiers.insert(cap[1].to_string());
    }

    let total = identifiers.len();

    // Dry-run mode: just report what we extracted from the spec
    let epic_id = match epic_id {
        Some(id) => id,
        None => {
            println!("\nSpec validation (dry run):");
            if total > 0 {
                println!(
                    "  Extracted {} identifiers to track coverage against tickets",
                    total
                );
                let mut sorted: Vec<&str> = identifiers.iter().map(|s| s.as_str()).collect();
                sorted.sort();
                let display: Vec<&str> = sorted.iter().take(20).copied().collect();
                let suffix = if sorted.len() > 20 {
                    format!(", ... ({} more)", sorted.len() - 20)
                } else {
                    String::new()
                };
                println!("  Identifiers: {}{}", display.join(", "), suffix);
            } else {
                println!("  No identifiers extracted from spec (no function names, file paths, or types found)");
            }
            return Ok(());
        }
    };

    // Fetch all child tickets
    let list_output = tokio::process::Command::new("bd")
        .args(["list", "--parent", epic_id, "--json", "--limit", "0"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await?;

    if !list_output.status.success() {
        let stderr = String::from_utf8_lossy(&list_output.stderr);
        println!("\nValidation: skipped (bd list failed: {})", stderr.trim());
        return Ok(());
    }

    let json_str = String::from_utf8(list_output.stdout)?;
    let tickets: Vec<serde_json::Value> = match serde_json::from_str(&json_str) {
        Ok(v) => v,
        Err(_) => {
            println!("\nValidation: skipped (could not parse ticket JSON)");
            return Ok(());
        }
    };

    if tickets.is_empty() {
        println!("\nValidation: no child tickets found for {}", epic_id);
        return Ok(());
    }

    // Field completeness check
    let check_fields = ["description", "acceptance_criteria", "design"];
    let mut incomplete: Vec<(String, String, Vec<&str>)> = Vec::new();
    let mut complete_count = 0;

    for ticket in &tickets {
        let id = ticket["id"].as_str().unwrap_or("?");
        let title = ticket["title"].as_str().unwrap_or("?");
        let missing: Vec<&str> = check_fields
            .iter()
            .filter(|&&field| ticket[field].as_str().map_or(true, |v| v.trim().is_empty()))
            .copied()
            .collect();

        if missing.is_empty() {
            complete_count += 1;
        } else {
            incomplete.push((id.to_string(), title.to_string(), missing));
        }
    }

    // Check coverage against tickets
    let ticket_texts: Vec<String> = tickets
        .iter()
        .map(|t| {
            let mut combined = String::new();
            for field in &["description", "acceptance_criteria", "design", "notes"] {
                if let Some(v) = t[*field].as_str() {
                    combined.push_str(v);
                    combined.push('\n');
                }
            }
            combined
        })
        .collect();

    let mut missing_ids: Vec<&str> = Vec::new();
    let mut covered = 0usize;

    for ident in &identifiers {
        if ticket_texts
            .iter()
            .any(|text| text.contains(ident.as_str()))
        {
            covered += 1;
        } else {
            missing_ids.push(ident);
        }
    }

    missing_ids.sort();
    let pct = if total > 0 {
        (covered as f64 / total as f64 * 100.0) as u32
    } else {
        100
    };

    // Print report
    println!("\nValidation:");
    println!(
        "  Tickets: {} ({} tasks + 1 epic)",
        tickets.len() + 1,
        tickets.len()
    );
    println!(
        "  Field completeness: {}/{} tickets fully populated",
        complete_count,
        tickets.len()
    );
    for (id, title, missing) in &incomplete {
        println!(
            "    WARN: {} \"{}\" — missing: {}",
            id,
            title,
            missing.join(", ")
        );
    }

    if total > 0 {
        println!(
            "  Spec coverage: {}/{} identifiers ({}%)",
            covered, total, pct
        );
        if !missing_ids.is_empty() {
            let display: Vec<&str> = missing_ids.iter().take(10).copied().collect();
            let suffix = if missing_ids.len() > 10 {
                format!(", ... ({} more)", missing_ids.len() - 10)
            } else {
                String::new()
            };
            println!("    Missing: {}{}", display.join(", "), suffix);
        }
    } else {
        println!("  Spec coverage: no identifiers extracted from spec");
    }

    Ok(())
}
