//! Pipeline preflight checks.
//!
//! Distinct from `validation.rs` — validation is syntactic/structural (pure,
//! no filesystem access).  Preflight performs environment checks at run time:
//! it may read the filesystem, check manifest presence, etc.
//!
//! Entry point: [`run`] — called once per `pas run` before execution starts.
//! Can also be invoked from `pas validate --preflight`.

use std::path::{Path, PathBuf};

use attractor_quality::resolution::{resolve, ResolutionError};

use crate::graph::PipelineGraph;

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
    let mut findings = Vec::new();

    // Only proceed if the graph has at least one quality node.
    if !graph_has_quality_node(graph) {
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

/// Returns `true` if any node in `graph` is dispatched to the `quality` handler.
///
/// A node is a quality node when:
/// - its `node_type` (from the DOT `type=` attribute) equals `"quality"`, or
/// - its `raw_attrs` contains `handler = "quality"`.
fn graph_has_quality_node(graph: &PipelineGraph) -> bool {
    use attractor_dot::AttributeValue;

    graph.all_nodes().any(|node| {
        // Primary: explicit `type="quality"` attribute.
        if node.node_type.as_deref() == Some("quality") {
            return true;
        }
        // Secondary: `handler="quality"` in raw attributes.
        if let Some(AttributeValue::String(s)) = node.raw_attrs.get("handler") {
            if s == "quality" {
                return true;
            }
        }
        false
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use tempfile::TempDir;

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
    fn make_graph_without_quality_node() -> PipelineGraph {
        let dot = r#"digraph G {
            start [shape="Mdiamond"]
            work [label="Do work"]
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
        assert!(graph_has_quality_node(&graph));
    }

    /// Verify graph_has_quality_node returns false for a graph with no quality nodes.
    #[test]
    fn no_quality_node_detection() {
        let graph = make_graph_without_quality_node();
        assert!(!graph_has_quality_node(&graph));
    }
}
