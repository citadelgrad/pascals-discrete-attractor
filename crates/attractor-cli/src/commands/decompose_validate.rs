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
        "Arc", "Box", "Cell", "Cow", "HashMap", "HashSet", "Mutex", "Option",
        "PathBuf", "Rc", "RefCell", "Result", "RwLock", "String", "Vec",
        "AtomicBool", "AtomicU32", "AtomicU64", "AtomicUsize", "Ordering",
        "BufReader", "BufWriter", "Duration", "Instant", "SystemTime",
        "Command", "Path", "File", "Sender", "Receiver",
        "JoinHandle", "TcpStream", "TcpListener",
        "Bytes", "BytesMut", "Pin", "Waker",
        "Some", "None", "Ok", "Err", "Default", "Display", "Debug",
        "From", "Into", "TryFrom", "TryInto", "Iterator", "IntoIterator",
        "Read", "Write", "Seek", "BufRead", "AsRef", "Deref",
        "Serialize", "Deserialize", "Clone", "Copy", "Send", "Sync",
        "PhantomData", "ManuallyDrop", "MaybeUninit",
        // std::io and error types
        "Error", "ErrorKind", "IoError",
        // tokio types
        "Mutex", "RwLock", "Notify", "Semaphore", "Barrier",
        "OwnedSemaphorePermit", "JoinSet", "JoinError",
        // serde/common crate types
        "Value", "Map", "Number", "Formatter",
        // test assertion types
        "Assert", "Assertion",
    ].into_iter().collect();

    // Generic trait method suffixes that aren't meaningful on any type
    let noise_suffixes = ["::clone", "::into",
        "::unwrap", "::expect", "::is_ok", "::is_err", "::is_some", "::is_none",
        "::as_ref", "::as_str", "::as_bytes", "::as_slice",
        "::to_string", "::to_owned", "::to_vec",
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
                println!("  Extracted {} identifiers to track coverage against tickets", total);
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
            .filter(|&&field| {
                ticket[field]
                    .as_str()
                    .map_or(true, |v| v.trim().is_empty())
            })
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
        if ticket_texts.iter().any(|text| text.contains(ident.as_str())) {
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
