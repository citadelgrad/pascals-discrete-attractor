use std::path::{Path, PathBuf};

use anyhow;
use attractor_pipeline::{BeadsError, Diagnostic, Severity};
use serde::Serialize;

use super::normalize_provider_defaults;

const TEMPLATE: &str = include_str!("../../../../templates/epic-runner.dot");

/// The template's goal line, replaced with the Epic's title and description.
const TEMPLATE_GOAL: &str =
    "goal=\"Implement all child tasks of epic EPIC_ID, closing each as completed.\"";

/// A `pas scaffold` failure. In `--json` mode it is printed as
/// `{"v":1,"ok":false,"error":{..}}` (C6).
#[derive(Debug)]
struct ScaffoldError {
    code: &'static str,
    message: String,
}

impl ScaffoldError {
    fn new(code: &'static str, message: impl std::fmt::Display) -> Self {
        Self {
            code,
            message: message.to_string(),
        }
    }
}

/// A Pipeline written to disk, with what the printers report about it.
struct Scaffolded {
    output_path: PathBuf,
    title: String,
    defaulted_providers: Vec<String>,
    errors: Vec<Diagnostic>,
    node_count: usize,
}

/// `pas scaffold --json` success payload (C6), fields in contract order.
#[derive(Serialize)]
struct ScaffoldJson {
    v: u32,
    ok: bool,
    pipeline_path: PathBuf,
}

/// Escape a value substituted into a quoted DOT string.
fn dot_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Fill the epic-runner template for an Epic: the goal names the typed
/// `epic_id`, and `beads.select` gets the `canonical_id` Beads returned.
/// Returns the normalized DOT and the nodes whose `llm_provider` was defaulted.
fn render_pipeline(
    template: &str,
    epic_id: &str,
    canonical_id: &str,
    title: &str,
    description: &str,
) -> anyhow::Result<(String, Vec<String>)> {
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

    let pipeline_content = template
        .replace(
            TEMPLATE_GOAL,
            &format!("goal=\"{}\"", dot_escape(&goal_text)),
        )
        .replace(
            "epic=\"EPIC_ID\"",
            &format!("epic=\"{}\"", dot_escape(canonical_id)),
        )
        .replace("EPIC_ID", &dot_escape(epic_id));

    // Fill in any missing llm_provider on runtime nodes before writing to disk,
    // so a scaffolded pipeline never silently depends on an implicit default.
    normalize_provider_defaults(&pipeline_content, "claude")
}

async fn scaffold(epic_id: &str, output: Option<&Path>) -> Result<Scaffolded, ScaffoldError> {
    // Get epic details via bd show --json
    let epic = attractor_pipeline::BeadsAdapter::new()
        .show(epic_id)
        .await
        .map_err(|e| {
            let code = match e {
                BeadsError::BdNotFound { .. } => "bd_not_found",
                BeadsError::CommandFailed { .. } => "epic_not_found",
                BeadsError::Io { .. } | BeadsError::InvalidOutput { .. } => "bd_failed",
            };
            ScaffoldError::new(code, format!("bd show failed: {}", e))
        })?;

    let (pipeline_content, defaulted_providers) = render_pipeline(
        TEMPLATE,
        epic_id,
        &epic.id,
        &epic.title,
        epic.description.as_deref().unwrap_or(""),
    )
    .map_err(|e| ScaffoldError::new("invalid_pipeline", e))?;

    // Determine output path
    let output_path = if let Some(path) = output {
        path.to_path_buf()
    } else {
        PathBuf::from(format!("pipelines/{}.dot", epic_id))
    };

    // Create parent directory if needed, then write the pipeline file
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ScaffoldError::new("write_failed", e))?;
    }
    std::fs::write(&output_path, &pipeline_content)
        .map_err(|e| ScaffoldError::new("write_failed", e))?;

    // Validate the generated pipeline exactly as `pas validate` does,
    // including the check that bd is on PATH for the Beads nodes.
    let plan = crate::load_execution_plan(&output_path)
        .map_err(|e| ScaffoldError::new("invalid_pipeline", e))?;
    let errors = attractor_pipeline::validate(plan.graph())
        .into_iter()
        .filter(|d| matches!(d.severity, Severity::Error))
        .collect();

    Ok(Scaffolded {
        output_path,
        title: epic.title,
        defaulted_providers,
        errors,
        node_count: plan.all_nodes().count(),
    })
}

pub async fn cmd_scaffold(epic_id: &str, output: Option<&Path>, json: bool) -> anyhow::Result<()> {
    let result = scaffold(epic_id, output).await;
    if json {
        return print_json(result);
    }
    let scaffolded = result.map_err(|e| anyhow::anyhow!(e.message))?;
    print_human(epic_id, &scaffolded);
    Ok(())
}

/// Exactly one JSON object on stdout; notices and diagnostics go to stderr.
fn print_json(result: Result<Scaffolded, ScaffoldError>) -> anyhow::Result<()> {
    let result = result.and_then(|scaffolded| {
        if !scaffolded.defaulted_providers.is_empty() {
            eprintln!(
                "  Defaulted llm_provider=\"claude\" on: {}",
                scaffolded.defaulted_providers.join(", ")
            );
        }
        if scaffolded.errors.is_empty() {
            return Ok(scaffolded);
        }
        for diag in &scaffolded.errors {
            eprintln!("  [ERROR] {}: {}", diag.rule, diag.message);
        }
        Err(ScaffoldError::new(
            "invalid_pipeline",
            format!(
                "Pipeline {} was written but has {} validation error(s)",
                scaffolded.output_path.display(),
                scaffolded.errors.len()
            ),
        ))
    });
    let result = result.and_then(|scaffolded| {
        std::fs::canonicalize(&scaffolded.output_path)
            .map_err(|e| ScaffoldError::new("write_failed", e))
    });
    match result {
        Ok(pipeline_path) => {
            let payload = ScaffoldJson {
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

fn print_human(epic_id: &str, scaffolded: &Scaffolded) {
    let output_path = &scaffolded.output_path;
    if !scaffolded.defaulted_providers.is_empty() {
        println!(
            "  Defaulted llm_provider=\"claude\" on: {}",
            scaffolded.defaulted_providers.join(", ")
        );
    }

    let has_error = !scaffolded.errors.is_empty();
    if has_error {
        println!("⚠ Pipeline generated but has validation errors:");
        for diag in &scaffolded.errors {
            println!("  [ERROR] {}: {}", diag.rule, diag.message);
        }
    }

    println!("✓ Pipeline scaffolded");
    println!("  Output: {}", output_path.display());
    println!("  Epic: {} ({})", epic_id, scaffolded.title);
    println!("  Nodes: {}", scaffolded.node_count);
    println!(
        "  Validation: {}",
        if has_error { "FAILED" } else { "PASSED" }
    );

    if !has_error {
        println!("\nNext steps:");
        println!("1. Review pipeline: cat {}", output_path.display());
        println!("2. Run pipeline: pas run {} -w .", output_path.display());
    }
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

    fn scaffolded_plan(epic_id: &str) -> attractor_pipeline::ExecutionPlan {
        let (dot, _) = render_pipeline(TEMPLATE, epic_id, epic_id, "My Epic", "Body").unwrap();
        let graph =
            attractor_pipeline::PipelineGraph::from_dot(attractor_dot::parse(&dot).unwrap())
                .unwrap();
        attractor_pipeline::ExecutionPlan::compile(graph).unwrap()
    }

    fn nodes_with_handler<'a>(
        plan: &'a attractor_pipeline::ExecutionPlan,
        handler: &str,
    ) -> Vec<&'a str> {
        plan.all_nodes()
            .filter(|node| node.handler.as_str() == handler)
            .map(|node| node.node_id.as_str())
            .collect()
    }

    fn string_attr<'a>(
        plan: &'a attractor_pipeline::ExecutionPlan,
        node: &str,
        key: &str,
    ) -> Option<&'a str> {
        match plan.source_node(node)?.raw_attrs.get(key)? {
            attractor_dot::AttributeValue::String(value) => Some(value),
            _ => None,
        }
    }

    /// AC1: one beads.select with epic=<epic>, one beads.close, and no
    /// validation errors other than bd's own PATH lookup (the CLI test runs
    /// the real `pas validate` with bd on PATH).
    #[test]
    fn scaffolded_template_has_one_select_with_epic_and_one_close() {
        let plan = scaffolded_plan("e-1");

        let select = nodes_with_handler(&plan, "beads.select");
        assert_eq!(select.len(), 1, "{select:?}");
        assert_eq!(string_attr(&plan, select[0], "epic"), Some("e-1"));
        assert_eq!(nodes_with_handler(&plan, "beads.close").len(), 1);

        let errors: Vec<_> = attractor_pipeline::validate_plan(&plan)
            .into_iter()
            .filter(|d| matches!(d.severity, Severity::Error))
            .collect();
        assert!(errors.is_empty(), "{errors:?}");
    }

    /// AC2: MORE, DONE and BLOCKED each have an edge, to three different
    /// nodes. close_task routes success and failure explicitly, so an
    /// unpublished Task never falls back to picking the next one.
    #[test]
    fn scaffolded_template_routes_more_done_blocked_to_distinct_nodes() {
        let plan = scaffolded_plan("e-1");
        let select = nodes_with_handler(&plan, "beads.select")[0];
        let target = |node: &str, condition: &str| -> Vec<String> {
            plan.outgoing_edges(node)
                .iter()
                .filter(|edge| edge.condition.as_deref() == Some(condition))
                .map(|edge| edge.to.clone())
                .collect()
        };

        let more = target(select, "preferred_label=MORE");
        let done = target(select, "preferred_label=DONE");
        let blocked = target(select, "preferred_label=BLOCKED");
        assert_eq!((more.len(), done.len(), blocked.len()), (1, 1, 1));
        let mut targets = vec![&more[0], &done[0], &blocked[0]];
        targets.sort();
        targets.dedup();
        assert_eq!(targets.len(), 3, "{more:?} {done:?} {blocked:?}");
        assert!(plan.is_exit(&done[0]));
        assert!(!plan.is_exit(&blocked[0]));

        let close = nodes_with_handler(&plan, "beads.close")[0];
        assert_eq!(target(close, "outcome=success"), vec![select.to_string()]);
        assert_eq!(target(close, "outcome=fail").len(), 1);
        assert!(plan
            .outgoing_edges(close)
            .iter()
            .all(|edge| edge.condition.is_some()));
    }

    /// AC3: no prompt tells the model to run bd update, bd close or bd ready,
    /// and no node lets the model run bd at all.
    #[test]
    fn scaffolded_template_never_tells_a_model_to_run_bd() {
        let plan = scaffolded_plan("e-1");
        let mut prompts = 0;
        for node in plan.graph().all_nodes() {
            if let Some(prompt) = &node.prompt {
                prompts += 1;
                let words: Vec<&str> = prompt.split_whitespace().collect();
                for pair in words.windows(2) {
                    assert!(
                        !(pair[0].ends_with(concat!("b", "d"))
                            && ["update", "close", "ready"].contains(&pair[1])),
                        "node {} tells the model to run `{} {}`",
                        node.id,
                        pair[0],
                        pair[1]
                    );
                }
            }
            if let Some(attractor_dot::AttributeValue::String(tools)) =
                node.raw_attrs.get("allowed_tools")
            {
                assert!(!tools.contains("Bash(bd"), "node {}: {tools}", node.id);
            }
        }
        assert!(prompts > 0, "expected the codergen prompts to be checked");
    }

    /// beads.select gets the ID Beads returned; the goal keeps the typed one.
    #[test]
    fn epic_attribute_uses_the_id_beads_returned() {
        let (dot, _) = render_pipeline(TEMPLATE, "ino", "attractor-ino", "T", "").unwrap();
        assert!(dot.contains("epic=\"attractor-ino\""), "{dot}");
        assert!(
            dot.contains("Implement all child tasks of epic ino: T."),
            "{dot}"
        );
        assert!(!dot.contains("EPIC_ID"));
    }

    /// Quotes and backslashes in substituted values survive the round trip.
    #[test]
    fn substituted_values_are_escaped() {
        let id = r#"e-"1\n"#;
        let (dot, _) = render_pipeline(TEMPLATE, id, id, r#"Say "hi"\"#, r#"a\b"#).unwrap();
        let graph =
            attractor_pipeline::PipelineGraph::from_dot(attractor_dot::parse(&dot).unwrap())
                .unwrap();
        let plan = attractor_pipeline::ExecutionPlan::compile(graph).unwrap();
        let select = nodes_with_handler(&plan, "beads.select")[0];
        assert_eq!(string_attr(&plan, select, "epic"), Some(id));
        assert_eq!(
            plan.graph().goal,
            format!(r#"Implement all child tasks of epic {id}: Say "hi"\. a\b"#)
        );
    }
}
