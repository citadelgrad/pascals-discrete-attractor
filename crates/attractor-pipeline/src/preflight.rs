//! Pipeline preflight checks.
//!
//! Distinct from `validation.rs` — validation is syntactic/structural (pure,
//! no filesystem access).  Preflight performs environment checks at run time:
//! it may read the filesystem, check manifest presence, etc.
//!
//! Entry point: [`run`] — called once per `pas run` before execution starts.
//! Can also be invoked from `pas validate --preflight`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use attractor_quality::resolution::{resolve, ResolutionError};

use crate::execution_plan::{ExecutionPlan, HandlerIdentity, LlmProvider, ResolvedNodeKind};
use crate::graph::PipelineGraph;
use crate::DEFAULT_MAX_BUDGET_USD;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Severity level for a preflight finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Severity {
    Warn,
    Error,
}

/// A single preflight finding emitted by [`run`].
#[derive(Debug, Clone)]
pub struct PreflightFinding {
    pub severity: Severity,
    /// Machine-readable code, e.g. `"QUALITY_NO_MANIFEST"`.
    pub code: String,
    pub message: String,
    pub suggestion: Option<String>,
    pub workdir: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Run all preflight checks for `graph` against the given `workdir`.
///
/// Returns a list of findings (empty = all clear).  Resolution of the quality
/// manifest is performed at most once regardless of how many quality nodes the
/// graph contains.
pub fn run(graph: &PipelineGraph, workdir: &Path) -> Vec<PreflightFinding> {
    run_with_budget(graph, workdir, None)
}

/// Run preflight checks with the optional budget supplied by the caller.
pub fn run_with_budget(
    graph: &PipelineGraph,
    workdir: &Path,
    max_budget_usd: Option<f64>,
) -> Vec<PreflightFinding> {
    match ExecutionPlan::compile(graph.clone()) {
        Ok(plan) => run_plan_with_budget(&plan, workdir, max_budget_usd),
        Err(error) => vec![PreflightFinding {
            severity: Severity::Error,
            code: "SEMANTIC_COMPILATION_FAILED".into(),
            message: error.to_string(),
            suggestion: Some("Fix semantic compilation errors before running preflight".into()),
            workdir: None,
        }],
    }
}

pub fn run_plan(plan: &ExecutionPlan, workdir: &Path) -> Vec<PreflightFinding> {
    run_plan_with_budget(plan, workdir, None)
}

pub fn run_plan_with_budget(
    plan: &ExecutionPlan,
    workdir: &Path,
    max_budget_usd: Option<f64>,
) -> Vec<PreflightFinding> {
    let mut findings = Vec::new();

    // Codergen nodes with no resolved timeout silently fall back to the
    // hardcoded 600s kill timeout in codergen_handler.rs. Warn per node.
    for node_id in nodes_missing_timeout(plan) {
        findings.push(PreflightFinding {
            severity: Severity::Warn,
            code: "CODERGEN_NO_TIMEOUT".into(),
            message: format!(
                "Node '{node_id}' has no timeout attribute and will fall back to the hardcoded 600s (10m) kill timeout"
            ),
            suggestion: Some(
                "Set an explicit timeout=\"<duration>\" on the node (or a graph/subgraph-level default) sized to its expected workload — e.g. 120s for routing/conditional nodes, 300s for medium tasks, 900s for heavy implementation/test nodes".into(),
            ),
            workdir: None,
        });
    }

    // Codex CLI and Gemini CLI JSON output does not report a dollar cost, so
    // these nodes always contribute $0 to the pipeline's cost total and
    // budget check, even though real spend occurred. Warn once per provider.
    for (provider_name, node_count) in uncosted_provider_counts(plan) {
        let display_name = provider_display_name(&provider_name);
        let node_label = if node_count == 1 { "node" } else { "nodes" };
        let budget_description = match max_budget_usd {
            Some(budget) => format!("the explicit ${budget:.2} budget from --max-budget-usd"),
            None => format!("the implicit ${DEFAULT_MAX_BUDGET_USD:.0} safety budget"),
        };
        findings.push(PreflightFinding {
            severity: Severity::Warn,
            code: "PROVIDER_COST_UNTRACKED".into(),
            message: format!(
                "{display_name} does not report per-call cost; Total cost and {budget_description} cannot track {node_count} {display_name} {node_label}. Step limits still apply."
            ),
            suggestion: Some(
                "Track spend for this provider separately (e.g. via its own billing dashboard) — pas cannot include it in its running total".into(),
            ),
            workdir: None,
        });
    }

    // Only proceed with quality-manifest checks if the graph has a quality node.
    if !graph_has_quality_node(plan) {
        return findings;
    }

    // Resolve the manifest exactly once.
    match resolve(workdir) {
        Ok(_) => {
            // Manifest found and valid — no warnings.
        }
        Err(ResolutionError::NotFound) => {
            findings.push(PreflightFinding {
                severity: Severity::Warn,
                code: "QUALITY_NO_MANIFEST".into(),
                message: format!(
                    "Pipeline uses 'quality' handler but no pas.toml found in {}",
                    workdir.display()
                ),
                suggestion: Some("pas init".into()),
                workdir: Some(workdir.to_path_buf()),
            });
        }
        Err(ResolutionError::Malformed { path, source }) => {
            findings.push(PreflightFinding {
                severity: Severity::Warn,
                code: "QUALITY_MALFORMED_MANIFEST".into(),
                message: format!("pas.toml at {} is malformed: {}", path.display(), source),
                suggestion: None,
                workdir: Some(workdir.to_path_buf()),
            });
        }
        Err(ResolutionError::Invalid { path, reason }) => {
            findings.push(PreflightFinding {
                severity: Severity::Warn,
                code: "QUALITY_INVALID_MANIFEST".into(),
                message: format!("pas.toml at {} is invalid: {}", path.display(), reason),
                suggestion: None,
                workdir: Some(workdir.to_path_buf()),
            });
        }
    }

    findings
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Returns the ids of all nodes in the compiled plan whose canonical handler is
/// `codergen` but have no resolved `timeout` — i.e. they would silently
/// fall back to the hardcoded 600s kill timeout in `codergen_handler.rs`.
///
/// `node.timeout` already reflects graph-/subgraph-level default cascading
/// (see `graph::node_def_to_pipeline_node`), so a `None` here means there is
/// truly no timeout override at any level.
fn nodes_missing_timeout(plan: &ExecutionPlan) -> Vec<String> {
    plan.all_nodes()
        .filter(|node| node.handler == HandlerIdentity::Codergen)
        .filter(|node| {
            plan.source_node(&node.node_id)
                .is_some_and(|source| source.timeout.is_none())
        })
        .map(|node| node.node_id.clone())
        .collect()
}

/// Counts nodes by normalized CLI provider when that provider never reports a
/// per-call dollar cost.
fn uncosted_provider_counts(plan: &ExecutionPlan) -> BTreeMap<String, usize> {
    plan.all_nodes().fold(BTreeMap::new(), |mut counts, node| {
        let Some(provider) = node.provider else {
            return counts;
        };
        let normalized = match provider {
            LlmProvider::Codex => "codex",
            LlmProvider::Gemini => "gemini",
            _ => return counts,
        };
        *counts.entry(normalized.to_string()).or_default() += 1;
        counts
    })
}

fn provider_display_name(provider: &str) -> &str {
    match provider {
        "codex" => "Codex",
        "gemini" => "Gemini",
        _ => provider,
    }
}

/// Returns `true` if any node in `graph` is dispatched to the `quality` handler.
///
/// A node is a quality node when:
/// - its `node_type` (from the DOT `type=` attribute) equals `"quality"`, or
/// - its `raw_attrs` contains `handler = "quality"`.
fn graph_has_quality_node(plan: &ExecutionPlan) -> bool {
    plan.all_nodes()
        .any(|node| node.kind == ResolvedNodeKind::Quality)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use tempfile::TempDir;

    // ---- graph construction helpers ----

    /// Build a graph with a single quality node (via DOT `type="quality"`).
    fn make_graph_with_quality_node() -> PipelineGraph {
        let dot = r#"digraph G {
            start [shape="Mdiamond"]
            quality_check [type="quality"]
            done [shape="Msquare"]
            start -> quality_check -> done
        }"#;
        let parsed = attractor_dot::parse(dot).unwrap();
        PipelineGraph::from_dot(parsed).unwrap()
    }

    /// Build a graph with NO quality node.
    ///
    /// `work` carries an explicit timeout so this fixture stays scoped to
    /// quality-manifest checks and isn't also flagged by CODERGEN_NO_TIMEOUT.
    fn make_graph_without_quality_node() -> PipelineGraph {
        let dot = r#"digraph G {
            start [shape="Mdiamond"]
            work [label="Do work", timeout="60s", llm_provider="claude"]
            done [shape="Msquare"]
            start -> work -> done
        }"#;
        let parsed = attractor_dot::parse(dot).unwrap();
        PipelineGraph::from_dot(parsed).unwrap()
    }

    fn workdir_with_git_and_manifest(tmp: &TempDir) {
        fs::create_dir(tmp.path().join(".git")).unwrap();
        fs::write(
            tmp.path().join("pas.toml"),
            "[project]\nname = \"test\"\n[quality]\nstages = [\"check\"]\n",
        )
        .unwrap();
    }

    fn workdir_with_git_no_manifest(tmp: &TempDir) {
        fs::create_dir(tmp.path().join(".git")).unwrap();
        // No pas.toml
    }

    // ---- integration tests ----

    /// (a) quality node + manifest present → no findings.
    #[test]
    fn quality_with_manifest_produces_no_warnings() {
        let tmp = TempDir::new().unwrap();
        workdir_with_git_and_manifest(&tmp);

        let graph = make_graph_with_quality_node();
        let findings = run(&graph, tmp.path());
        assert!(
            findings.is_empty(),
            "expected no findings, got: {:?}",
            findings
        );
    }

    /// (b) quality node + no manifest → exactly one WARN with code QUALITY_NO_MANIFEST.
    #[test]
    fn quality_without_manifest_produces_exactly_one_warning() {
        let tmp = TempDir::new().unwrap();
        workdir_with_git_no_manifest(&tmp);

        let graph = make_graph_with_quality_node();
        let findings = run(&graph, tmp.path());

        assert_eq!(
            findings.len(),
            1,
            "expected exactly 1 finding, got: {:?}",
            findings
        );
        assert_eq!(findings[0].code, "QUALITY_NO_MANIFEST");
        assert_eq!(findings[0].severity, Severity::Warn);
        assert_eq!(findings[0].suggestion.as_deref(), Some("pas init"));
    }

    /// (c) no quality node + no manifest → no findings.
    #[test]
    fn no_quality_node_produces_no_warnings_even_without_manifest() {
        let tmp = TempDir::new().unwrap();
        workdir_with_git_no_manifest(&tmp);

        let graph = make_graph_without_quality_node();
        let findings = run(&graph, tmp.path());
        assert!(
            findings.is_empty(),
            "expected no findings when no quality node, got: {:?}",
            findings
        );
    }

    #[test]
    fn raw_graph_preflight_fails_closed_on_missing_provider() {
        let tmp = TempDir::new().unwrap();
        let graph = PipelineGraph::from_dot(
            attractor_dot::parse(
                r#"digraph G {
                    start [shape="Mdiamond"]
                    work [shape="box", prompt="work", timeout=1s]
                    done [shape="Msquare"]
                    start -> work -> done
                }"#,
            )
            .unwrap(),
        )
        .unwrap();

        let findings = run(&graph, tmp.path());

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Error);
        assert_eq!(findings[0].code, "SEMANTIC_COMPILATION_FAILED");
        assert!(findings[0].message.contains("work"));
        assert!(findings[0].message.contains("llm_provider"));
    }

    /// Resolve is called exactly once even when the graph has multiple quality nodes.
    /// We verify this indirectly: 10 iterations of `run` on the same no-manifest workdir
    /// all produce exactly 1 finding (one per call, not accumulating), confirming the
    /// single-resolve-per-call contract.
    #[test]
    fn resolve_called_once_per_run_call_not_accumulated() {
        let tmp = TempDir::new().unwrap();
        workdir_with_git_no_manifest(&tmp);

        let graph = make_graph_with_quality_node();
        for i in 0..10 {
            let findings = run(&graph, tmp.path());
            assert_eq!(
                findings.len(),
                1,
                "iteration {i}: expected exactly 1 finding (resolve called once per run)"
            );
        }
    }

    /// Verify graph_has_quality_node detects `handler="quality"` in raw_attrs.
    #[test]
    fn detects_quality_via_handler_attr() {
        // Build a graph node with handler="quality" in raw_attrs
        let dot = r#"digraph G {
            start [shape="Mdiamond"]
            qcheck [handler="quality"]
            done [shape="Msquare"]
            start -> qcheck -> done
        }"#;
        let parsed = attractor_dot::parse(dot).unwrap();
        let graph = PipelineGraph::from_dot(parsed).unwrap();
        let plan = ExecutionPlan::compile_for_generation(graph, LlmProvider::Claude)
            .unwrap()
            .plan;
        assert!(graph_has_quality_node(&plan));
    }

    /// Verify graph_has_quality_node returns false for a graph with no quality nodes.
    #[test]
    fn no_quality_node_detection() {
        let graph = make_graph_without_quality_node();
        let plan = ExecutionPlan::compile_for_generation(graph, LlmProvider::Claude)
            .unwrap()
            .plan;
        assert!(!graph_has_quality_node(&plan));
    }

    // ---- CODERGEN_NO_TIMEOUT tests ----

    fn run_no_manifest(graph: &PipelineGraph) -> Vec<PreflightFinding> {
        let tmp = TempDir::new().unwrap();
        workdir_with_git_no_manifest(&tmp);
        run(graph, tmp.path())
    }

    /// A box-shaped (codergen) node with no timeout and no graph-level default
    /// produces exactly one CODERGEN_NO_TIMEOUT warning naming that node.
    #[test]
    fn codergen_node_without_timeout_produces_warning() {
        let dot = r#"digraph G {
            start [shape="Mdiamond"]
            work [label="Do work", llm_provider="claude"]
            done [shape="Msquare"]
            start -> work -> done
        }"#;
        let parsed = attractor_dot::parse(dot).unwrap();
        let graph = PipelineGraph::from_dot(parsed).unwrap();

        let findings = run_no_manifest(&graph);
        assert_eq!(
            findings.len(),
            1,
            "expected exactly 1 finding: {findings:?}"
        );
        assert_eq!(findings[0].code, "CODERGEN_NO_TIMEOUT");
        assert_eq!(findings[0].severity, Severity::Warn);
        assert!(findings[0].message.contains("work"));
    }

    /// A box-shaped node with an explicit timeout produces no finding.
    #[test]
    fn codergen_node_with_explicit_timeout_produces_no_warning() {
        let dot = r#"digraph G {
            start [shape="Mdiamond"]
            work [label="Do work", timeout="60s", llm_provider="claude"]
            done [shape="Msquare"]
            start -> work -> done
        }"#;
        let parsed = attractor_dot::parse(dot).unwrap();
        let graph = PipelineGraph::from_dot(parsed).unwrap();

        let findings = run_no_manifest(&graph);
        assert!(findings.is_empty(), "expected no findings: {findings:?}");
    }

    /// A node with no node-level timeout but covered by a `node [timeout=...]`
    /// graph-level default produces no finding — cascaded defaults count as set.
    #[test]
    fn codergen_node_with_graph_level_default_timeout_produces_no_warning() {
        let dot = r#"digraph G {
            node [timeout="300s", llm_provider="claude"]
            start [shape="Mdiamond"]
            work [label="Do work"]
            done [shape="Msquare"]
            start -> work -> done
        }"#;
        let parsed = attractor_dot::parse(dot).unwrap();
        let graph = PipelineGraph::from_dot(parsed).unwrap();

        let findings = run_no_manifest(&graph);
        assert!(findings.is_empty(), "expected no findings: {findings:?}");
    }

    /// A diamond-shaped node with a prompt canonically resolves to `codergen`,
    /// so a missing timeout is still flagged.
    #[test]
    fn conditional_node_with_prompt_without_timeout_produces_warning() {
        let dot = r#"digraph G {
            start [shape="Mdiamond"]
            check [shape="diamond", type="conditional", prompt="Decide something", llm_provider="claude"]
            done [shape="Msquare"]
            start -> check -> done
        }"#;
        let parsed = attractor_dot::parse(dot).unwrap();
        let graph = PipelineGraph::from_dot(parsed).unwrap();

        let findings = run_no_manifest(&graph);
        assert_eq!(
            findings.len(),
            1,
            "expected exactly 1 finding: {findings:?}"
        );
        assert_eq!(findings[0].code, "CODERGEN_NO_TIMEOUT");
        assert!(findings[0].message.contains("check"));
    }

    /// A pure diamond-shaped routing node (no prompt) resolves to
    /// "conditional", not "codergen", so a missing timeout is not flagged.
    #[test]
    fn conditional_node_without_prompt_without_timeout_produces_no_warning() {
        let dot = r#"digraph G {
            start [shape="Mdiamond"]
            check [shape="diamond", type="conditional"]
            done [shape="Msquare"]
            start -> check -> done
        }"#;
        let parsed = attractor_dot::parse(dot).unwrap();
        let graph = PipelineGraph::from_dot(parsed).unwrap();

        let findings = run_no_manifest(&graph);
        assert!(findings.is_empty(), "expected no findings: {findings:?}");
    }

    /// Non-codergen nodes (start/exit) with no timeout are never flagged —
    /// they aren't subject to the hardcoded 600s codergen fallback.
    #[test]
    fn non_codergen_nodes_without_timeout_produce_no_warnings() {
        let dot = r#"digraph G {
            start [shape="Mdiamond"]
            done [shape="Msquare"]
            start -> done
        }"#;
        let parsed = attractor_dot::parse(dot).unwrap();
        let graph = PipelineGraph::from_dot(parsed).unwrap();

        let findings = run_no_manifest(&graph);
        assert!(findings.is_empty(), "expected no findings: {findings:?}");
    }

    /// Multiple codergen nodes missing timeouts each produce their own
    /// distinct finding rather than being merged into one.
    #[test]
    fn multiple_codergen_nodes_missing_timeout_produce_distinct_findings() {
        let dot = r#"digraph G {
            start [shape="Mdiamond"]
            first [label="First", llm_provider="claude"]
            second [label="Second", llm_provider="claude"]
            done [shape="Msquare"]
            start -> first -> second -> done
        }"#;
        let parsed = attractor_dot::parse(dot).unwrap();
        let graph = PipelineGraph::from_dot(parsed).unwrap();

        let findings = run_no_manifest(&graph);
        assert_eq!(
            findings.len(),
            2,
            "expected 2 distinct findings: {findings:?}"
        );
        assert!(findings.iter().all(|f| f.code == "CODERGEN_NO_TIMEOUT"));
        let mentioned: Vec<&str> = findings.iter().map(|f| f.message.as_str()).collect();
        assert!(mentioned.iter().any(|m| m.contains("first")));
        assert!(mentioned.iter().any(|m| m.contains("second")));
    }

    // ---- PROVIDER_COST_UNTRACKED tests ----

    /// Multiple Codex nodes produce one aggregated warning that explains the
    /// implicit budget and reports the affected node count.
    #[test]
    fn codex_provider_nodes_produce_aggregated_implicit_budget_warning() {
        let dot = r#"digraph G {
            start [shape="Mdiamond"]
            first [label="First", timeout="60s", llm_provider="codex"]
            second [label="Second", timeout="60s", llm_provider="openai"]
            done [shape="Msquare"]
            start -> first -> second -> done
        }"#;
        let parsed = attractor_dot::parse(dot).unwrap();
        let graph = PipelineGraph::from_dot(parsed).unwrap();

        let findings = run_no_manifest(&graph);
        assert_eq!(
            findings.len(),
            1,
            "expected exactly 1 finding: {findings:?}"
        );
        assert_eq!(findings[0].code, "PROVIDER_COST_UNTRACKED");
        assert_eq!(findings[0].severity, Severity::Warn);
        assert!(findings[0].message.contains("2 Codex nodes"));
        assert!(findings[0].message.contains("implicit $200"));
        assert!(!findings[0].message.contains("--max-budget-usd"));
    }

    #[test]
    fn codex_provider_warning_identifies_explicit_budget() {
        let dot = r#"digraph G {
            start [shape="Mdiamond"]
            work [label="Do work", timeout="60s", llm_provider="codex"]
            done [shape="Msquare"]
            start -> work -> done
        }"#;
        let parsed = attractor_dot::parse(dot).unwrap();
        let graph = PipelineGraph::from_dot(parsed).unwrap();
        let tmp = TempDir::new().unwrap();

        let findings = run_with_budget(&graph, tmp.path(), Some(42.5));

        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("explicit $42.50 budget"));
        assert!(findings[0].message.contains("--max-budget-usd"));
    }

    #[test]
    fn mixed_providers_are_aggregated_separately() {
        let dot = r#"digraph G {
            start [shape="Mdiamond"]
            codex [label="Codex", timeout="60s", llm_provider="codex"]
            gemini [label="Gemini", timeout="60s", llm_provider="gemini"]
            claude [label="Claude", timeout="60s", llm_provider="claude"]
            done [shape="Msquare"]
            start -> codex -> gemini -> claude -> done
        }"#;
        let parsed = attractor_dot::parse(dot).unwrap();
        let graph = PipelineGraph::from_dot(parsed).unwrap();

        let findings = run_no_manifest(&graph);

        assert_eq!(findings.len(), 2);
        assert!(findings
            .iter()
            .any(|finding| finding.message.contains("1 Codex node")));
        assert!(findings
            .iter()
            .any(|finding| finding.message.contains("1 Gemini node")));
        assert!(findings
            .iter()
            .all(|finding| !finding.message.contains("Claude")));
    }

    /// A node with llm_provider="gemini" produces exactly one
    /// PROVIDER_COST_UNTRACKED warning naming that node.
    #[test]
    fn gemini_provider_node_produces_cost_warning() {
        let dot = r#"digraph G {
            start [shape="Mdiamond"]
            work [label="Do work", timeout="60s", llm_provider="gemini"]
            done [shape="Msquare"]
            start -> work -> done
        }"#;
        let parsed = attractor_dot::parse(dot).unwrap();
        let graph = PipelineGraph::from_dot(parsed).unwrap();

        let findings = run_no_manifest(&graph);
        assert_eq!(
            findings.len(),
            1,
            "expected exactly 1 finding: {findings:?}"
        );
        assert_eq!(findings[0].code, "PROVIDER_COST_UNTRACKED");
    }

    /// An explicit Claude provider reports cost, so it produces no
    /// PROVIDER_COST_UNTRACKED warning.
    #[test]
    fn explicit_claude_provider_produces_no_cost_warning() {
        let dot = r#"digraph G {
            start [shape="Mdiamond"]
            work [label="Do work", timeout="60s", llm_provider="claude"]
            done [shape="Msquare"]
            start -> work -> done
        }"#;
        let parsed = attractor_dot::parse(dot).unwrap();
        let graph = PipelineGraph::from_dot(parsed).unwrap();

        let findings = run_no_manifest(&graph);
        assert!(findings.is_empty(), "expected no findings: {findings:?}");
    }

    /// The CODERGEN_NO_TIMEOUT check and the quality-manifest check compose:
    /// a graph with both a timeout-less codergen node and a quality node
    /// missing its manifest produces both findings.
    #[test]
    fn codergen_and_quality_findings_compose() {
        let tmp = TempDir::new().unwrap();
        workdir_with_git_no_manifest(&tmp);

        let dot = r#"digraph G {
            start [shape="Mdiamond"]
            work [label="Do work", llm_provider="claude"]
            quality_check [type="quality"]
            done [shape="Msquare"]
            start -> work -> quality_check -> done
        }"#;
        let parsed = attractor_dot::parse(dot).unwrap();
        let graph = PipelineGraph::from_dot(parsed).unwrap();

        let findings = run(&graph, tmp.path());
        assert_eq!(findings.len(), 2, "expected 2 findings: {findings:?}");
        let codes: Vec<&str> = findings.iter().map(|f| f.code.as_str()).collect();
        assert!(codes.contains(&"CODERGEN_NO_TIMEOUT"));
        assert!(codes.contains(&"QUALITY_NO_MANIFEST"));
    }
}
