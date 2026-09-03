//! Manager loop handler for "house" shaped nodes.
//!
//! Supervises a sub-pipeline or sub-section of the graph.

use async_trait::async_trait;
use attractor_types::{Context, Outcome, Result, StageStatus};

use crate::graph::{PipelineGraph, PipelineNode};
use crate::handler::NodeHandler;

/// Handler for "stack.manager_loop" type nodes (shape="house").
/// Supervises execution and can coordinate sub-tasks.
pub struct ManagerLoopHandler;

#[async_trait]
impl NodeHandler for ManagerLoopHandler {
    fn handler_type(&self) -> &str {
        "stack.manager_loop"
    }

    async fn execute(
        &self,
        node: &PipelineNode,
        _context: &Context,
        _graph: &PipelineGraph,
    ) -> Result<Outcome> {
        let prompt = node.prompt.as_deref().unwrap_or("Supervise sub-tasks");
        tracing::info!(node = %node.id, "Manager loop executing: {}", prompt);

        Ok(Outcome {
            status: StageStatus::Success,
            preferred_label: None,
            suggested_next_ids: vec![],
            context_updates: {
                let mut updates = std::collections::HashMap::new();
                updates.insert(
                    format!("{}.managed", node.id),
                    serde_json::Value::Bool(true),
                );
                updates
            },
            notes: format!("Manager completed: {}", prompt),
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

    fn make_node(id: &str, shape: &str, prompt: Option<&str>) -> PipelineNode {
        PipelineNode {
            id: id.to_string(),
            label: id.to_string(),
            shape: shape.to_string(),
            node_type: None,
            prompt: prompt.map(String::from),
            goal_gate: false,
            retry_target: None,
            fallback_retry_target: None,
            classes: Vec::new(),
            timeout: None,
            llm_model: None,
            llm_provider: None,
            raw_attrs: HashMap::new(),
        }
    }

    fn make_minimal_graph() -> PipelineGraph {
        let dot = r#"digraph G { A -> B }"#;
        let parsed = attractor_dot::parse(dot).unwrap();
        PipelineGraph::from_dot(parsed).unwrap()
    }

    #[tokio::test]
    async fn manager_handler_returns_success() {
        let handler = ManagerLoopHandler;
        let node = make_node("mgr", "house", Some("Coordinate workers"));
        let ctx = Context::default();
        let graph = make_minimal_graph();

        let outcome = handler.execute(&node, &ctx, &graph).await.unwrap();
        assert_eq!(outcome.status, StageStatus::Success);
        assert!(outcome.notes.contains("Coordinate workers"));
        assert_eq!(
            outcome.context_updates.get("mgr.managed"),
            Some(&serde_json::Value::Bool(true))
        );
    }

    #[tokio::test]
    async fn manager_handler_default_prompt() {
        let handler = ManagerLoopHandler;
        let node = make_node("mgr", "house", None);
        let ctx = Context::default();
        let graph = make_minimal_graph();

        let outcome = handler.execute(&node, &ctx, &graph).await.unwrap();
        assert_eq!(outcome.status, StageStatus::Success);
        assert!(outcome.notes.contains("Supervise sub-tasks"));
    }

    #[test]
    fn manager_handler_type() {
        let handler = ManagerLoopHandler;
        assert_eq!(handler.handler_type(), "stack.manager_loop");
    }
}
