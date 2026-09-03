//! Additional node handlers beyond the basic start/exit/conditional.

pub mod codergen_handler;
pub mod manager;
pub mod parallel;
mod process_group;
pub mod quality_handler;
pub mod tool_handler;
pub mod wait_human;

pub use codergen_handler::CodergenHandler;
pub use manager::ManagerLoopHandler;
pub use parallel::{FanInHandler, ParallelHandler};
pub use quality_handler::QualityHandler;
pub use tool_handler::ToolHandler;

// ---------------------------------------------------------------------------
// Shared test helpers used by child handler modules
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod quality_handler_tests;

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;

    use attractor_dot::AttributeValue;

    use crate::graph::{PipelineGraph, PipelineNode};

    pub(crate) fn make_node(
        id: &str,
        shape: &str,
        prompt: Option<&str>,
        attrs: HashMap<String, AttributeValue>,
    ) -> PipelineNode {
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
            raw_attrs: attrs,
        }
    }

    pub(crate) fn make_minimal_graph() -> PipelineGraph {
        let dot = r#"digraph G { A -> B }"#;
        let parsed = attractor_dot::parse(dot).unwrap();
        PipelineGraph::from_dot(parsed).unwrap()
    }
}
