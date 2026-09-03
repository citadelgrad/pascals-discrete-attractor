use anyhow;

use super::normalize_provider_defaults;

pub async fn cmd_scaffold(epic_id: &str, output: Option<&std::path::Path>) -> anyhow::Result<()> {
    // Load epic-runner template
    let template = include_str!("../../../../templates/epic-runner.dot");

    // Get epic details via bd show --json
    let mut cmd = tokio::process::Command::new("bd");
    cmd.arg("show").arg(epic_id).arg("--json");

    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let output_result = cmd.output().await?;

    if !output_result.status.success() {
        let stderr = String::from_utf8_lossy(&output_result.stderr);
        anyhow::bail!("bd show failed: {}", stderr);
    }

    let json_output = String::from_utf8(output_result.stdout)?;
    let epic_array: serde_json::Value = serde_json::from_str(&json_output)?;

    // bd show --json returns an array with one element
    let epic_data = epic_array
        .as_array()
        .and_then(|arr| arr.first())
        .ok_or_else(|| anyhow::anyhow!("bd show returned empty array"))?;

    let title = epic_data["title"].as_str().unwrap_or("Unknown Epic");
    let description = epic_data["description"].as_str().unwrap_or("");

    // First, update the goal attribute BEFORE replacing EPIC_ID
    let goal_text = format!(
        "Implement all child tasks of epic {}: {}.{}",
        epic_id,
        title,
        if description.is_empty() {
            String::new()
        } else {
            format!(" {}", description)
        }
    );

    let mut pipeline_content = template.replace(
        "goal=\"Implement all child tasks of epic EPIC_ID, closing each as completed.\"",
        &format!("goal=\"{}\"", goal_text.replace('"', "\\\"")),
    );

    // Then replace all remaining EPIC_ID placeholders
    pipeline_content = pipeline_content.replace("EPIC_ID", epic_id);

    // Determine output path
    let output_path = if let Some(path) = output {
        path.to_path_buf()
    } else {
        std::path::PathBuf::from(format!("pipelines/{}.dot", epic_id))
    };

    // Create parent directory if needed
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Fill in any missing llm_provider on runtime nodes before writing to disk,
    // so a scaffolded pipeline never silently depends on an implicit default.
    let (normalized_content, defaulted_providers) =
        normalize_provider_defaults(&pipeline_content, "claude")?;
    pipeline_content = normalized_content;

    // Write pipeline file
    std::fs::write(&output_path, &pipeline_content)?;

    if !defaulted_providers.is_empty() {
        println!(
            "  Defaulted llm_provider=\"claude\" on: {}",
            defaulted_providers.join(", ")
        );
    }

    // Validate the generated pipeline
    let plan = crate::load_execution_plan(&output_path)?;
    let diagnostics = attractor_pipeline::validate_plan(&plan);

    let has_error = diagnostics
        .iter()
        .any(|d| matches!(d.severity, attractor_pipeline::Severity::Error));

    if has_error {
        println!("⚠ Pipeline generated but has validation errors:");
        for diag in &diagnostics {
            if matches!(diag.severity, attractor_pipeline::Severity::Error) {
                println!("  [ERROR] {}: {}", diag.rule, diag.message);
            }
        }
    }

    // Count nodes
    let node_count = plan.all_nodes().count();

    println!("✓ Pipeline scaffolded");
    println!("  Output: {}", output_path.display());
    println!("  Epic: {} ({})", epic_id, title);
    println!("  Nodes: {}", node_count);
    println!(
        "  Validation: {}",
        if has_error { "FAILED" } else { "PASSED" }
    );

    if !has_error {
        println!("\nNext steps:");
        println!("1. Review pipeline: cat {}", output_path.display());
        println!("2. Run pipeline: pas run {} -w .", output_path.display());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scaffolded pipeline's runtime nodes must never rely on the implicit
    /// "silently defaults to Claude" behavior -- normalize_provider_defaults
    /// must make the provider explicit in the DOT source itself.
    #[test]
    fn normalize_provider_defaults_fills_missing_runtime_provider() {
        let dot = r#"digraph G {
            start [shape="Mdiamond"]
            work [shape="box", prompt="Do work"]
            done [shape="Msquare"]
            start -> work -> done
        }"#;

        let (normalized, defaulted) = normalize_provider_defaults(dot, "claude").unwrap();

        assert_eq!(defaulted, vec!["work".to_string()]);
        let parsed = attractor_dot::parse(&normalized).unwrap();
        let attrs = &parsed.nodes.get("work").unwrap().attrs;
        assert_eq!(
            attrs.get("llm_provider"),
            Some(&attractor_dot::AttributeValue::String("claude".to_string()))
        );
    }

    /// Nodes exempt from the requirement (start/exit/quality) must not be
    /// touched, and nodes that already name a provider (even a non-default
    /// one, like "codex") must be left exactly as authored.
    #[test]
    fn normalize_provider_defaults_leaves_exempt_and_explicit_nodes_alone() {
        let dot = r#"digraph G {
            start [shape="Mdiamond"]
            verify [shape="box", type="quality"]
            other [shape="box", llm_provider="codex"]
            done [shape="Msquare"]
            start -> verify -> other -> done
        }"#;

        let (normalized, defaulted) = normalize_provider_defaults(dot, "claude").unwrap();

        assert!(defaulted.is_empty());
        let parsed = attractor_dot::parse(&normalized).unwrap();
        assert!(!parsed
            .nodes
            .get("verify")
            .unwrap()
            .attrs
            .contains_key("llm_provider"));
        assert_eq!(
            parsed.nodes.get("other").unwrap().attrs.get("llm_provider"),
            Some(&attractor_dot::AttributeValue::String("codex".to_string()))
        );
    }

    /// The real epic-runner.dot template already names an explicit
    /// llm_provider on every runtime node (hand-authored, not relying on
    /// normalization to fill it in) -- and, run through the same
    /// normalization step cmd_scaffold uses, it must validate cleanly with
    /// no provider_required errors. This proves a freshly scaffolded
    /// pipeline has every runtime node's llm_provider set explicitly.
    #[test]
    fn scaffolded_epic_runner_template_has_no_missing_providers_after_normalization() {
        let template = include_str!("../../../../templates/epic-runner.dot");
        let filled = template.replace("EPIC_ID", "test-epic-123");

        let (normalized, defaulted) = normalize_provider_defaults(&filled, "claude").unwrap();
        assert!(
            defaulted.is_empty(),
            "template should already name llm_provider explicitly on every runtime node, but had to default: {defaulted:?}"
        );

        let dot_graph = attractor_dot::parse(&normalized).unwrap();
        let pipeline_graph = attractor_pipeline::PipelineGraph::from_dot(dot_graph).unwrap();
        let diagnostics = attractor_pipeline::validate(&pipeline_graph);
        let provider_errors: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.rule == "provider_required")
            .collect();
        assert!(
            provider_errors.is_empty(),
            "expected no provider_required diagnostics after normalization, got: {provider_errors:?}"
        );
    }
}
