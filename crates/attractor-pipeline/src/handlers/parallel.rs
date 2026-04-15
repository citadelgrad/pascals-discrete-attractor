use async_trait::async_trait;
use attractor_types::{Context, Outcome, Result, StageStatus};

use crate::graph::{PipelineGraph, PipelineNode};
use crate::handler::NodeHandler;

/// Handler for "parallel" type nodes (shape="component").
///
/// Represents a fan-out point where multiple branches could execute. This
/// handler populates `suggested_next_ids` with all outgoing branch targets so
/// the engine has visibility into the intended fan-out.
///
/// **Known limitation:** The engine's `select_edge` function follows exactly
/// one edge per step (sequential execution). When this handler returns multiple
/// `suggested_next_ids`, the engine will execute only the *first* matching
/// branch and skip the rest. True parallel fork/join semantics are **not yet
/// implemented** and would require significant engine changes (work-stealing
/// queue, join barriers, shared-context merging). Until that work is done,
/// pipelines using `shape="component"` nodes will silently execute only one
/// branch. A `tracing::warn!` is emitted at runtime to make this visible.
pub struct ParallelHandler;

#[async_trait]
impl NodeHandler for ParallelHandler {
    fn handler_type(&self) -> &str {
        "parallel"
    }

    async fn execute(
        &self,
        node: &PipelineNode,
        _context: &Context,
        graph: &PipelineGraph,
    ) -> Result<Outcome> {
        let outgoing = graph.outgoing_edges(&node.id);
        let branch_count = outgoing.len();
        let branch_targets: Vec<String> = outgoing.iter().map(|e| e.to.clone()).collect();

        tracing::info!(
            node = %node.id,
            branches = branch_count,
            targets = ?branch_targets,
            "Parallel fan-out"
        );

        if branch_count > 1 {
            tracing::warn!(
                node = %node.id,
                branches = branch_count,
                targets = ?branch_targets,
                "ParallelHandler suggested {} branches but engine executes sequentially. \
                 True parallel execution is not yet implemented.",
                branch_count
            );
        }

        // The parallel handler itself just passes through.
        // The execution engine is responsible for actually forking execution.
        // For now, suggest all branch targets; select_edge will follow only the
        // first matching one (sequential, not parallel — see struct-level doc).
        Ok(Outcome {
            status: StageStatus::Success,
            preferred_label: None,
            suggested_next_ids: branch_targets,
            context_updates: std::collections::HashMap::new(),
            notes: format!("Fan-out to {} branches", branch_count),
            failure_reason: None,
        })
    }
}

/// Handler for "parallel.fan_in" type nodes (shape="tripleoctagon").
/// Collects results from parallel branches.
pub struct FanInHandler;

#[async_trait]
impl NodeHandler for FanInHandler {
    fn handler_type(&self) -> &str {
        "parallel.fan_in"
    }

    async fn execute(
        &self,
        node: &PipelineNode,
        _context: &Context,
        _graph: &PipelineGraph,
    ) -> Result<Outcome> {
        tracing::info!(node = %node.id, "Fan-in merge point");

        Ok(Outcome {
            status: StageStatus::Success,
            preferred_label: None,
            suggested_next_ids: vec![],
            context_updates: std::collections::HashMap::new(),
            notes: "Fan-in merge completed".to_string(),
            failure_reason: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_node(id: &str, shape: &str) -> PipelineNode {
        PipelineNode {
            id: id.to_string(),
            label: id.to_string(),
            shape: shape.to_string(),
            node_type: None,
            prompt: None,
            max_retries: 0,
            goal_gate: false,
            retry_target: None,
            fallback_retry_target: None,
            fidelity: None,
            thread_id: None,
            classes: Vec::new(),
            timeout: None,
            llm_model: None,
            llm_provider: None,
            reasoning_effort: None,
            auto_status: true,
            allow_partial: false,
            raw_attrs: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn parallel_handler_returns_branch_targets() {
        let handler = ParallelHandler;
        let dot = r#"digraph G {
            fork [shape="component"]
            branch_a [shape="box"]
            branch_b [shape="box"]
            fork -> branch_a
            fork -> branch_b
        }"#;
        let parsed = attractor_dot::parse(dot).unwrap();
        let graph = PipelineGraph::from_dot(parsed).unwrap();
        let node = graph.node("fork").unwrap().clone();
        let ctx = Context::default();

        let outcome = handler.execute(&node, &ctx, &graph).await.unwrap();
        assert_eq!(outcome.status, StageStatus::Success);
        assert_eq!(outcome.suggested_next_ids.len(), 2);
        assert!(outcome.suggested_next_ids.contains(&"branch_a".to_string()));
        assert!(outcome.suggested_next_ids.contains(&"branch_b".to_string()));
        assert!(outcome.notes.contains("2 branches"));
    }

    #[tokio::test]
    async fn fan_in_handler_returns_success() {
        let handler = FanInHandler;
        let dot = r#"digraph G { A -> B }"#;
        let parsed = attractor_dot::parse(dot).unwrap();
        let graph = PipelineGraph::from_dot(parsed).unwrap();
        let node = make_node("merge", "tripleoctagon");
        let ctx = Context::default();

        let outcome = handler.execute(&node, &ctx, &graph).await.unwrap();
        assert_eq!(outcome.status, StageStatus::Success);
        assert!(outcome.suggested_next_ids.is_empty());
        assert_eq!(outcome.notes, "Fan-in merge completed");
    }
}
