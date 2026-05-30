//! End-to-end integration tests for the Attractor pipeline engine.
//!
//! Each test exercises the full pipeline: parse DOT -> build graph -> validate -> execute -> verify.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;

use attractor_dot::parse;
use attractor_pipeline::{
    apply_stylesheet, parse_stylesheet, validate, validate_or_raise, ConditionalHandler,
    ExitHandler, HandlerRegistry, NodeHandler, PipelineExecutor, PipelineGraph, PipelineNode,
    StartHandler,
};
use attractor_types::{Context, Outcome, StageStatus};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse DOT source into a PipelineGraph, panicking on failure.
fn build_graph(dot: &str) -> PipelineGraph {
    let parsed = parse(dot).expect("DOT parse failed");
    PipelineGraph::from_dot(parsed).expect("PipelineGraph::from_dot failed")
}

/// A mock codergen handler that returns Success without shelling out to Claude CLI.
/// This allows integration tests to run fast and without external dependencies.
struct MockCodergenHandler;

#[async_trait]
impl NodeHandler for MockCodergenHandler {
    fn handler_type(&self) -> &str {
        "codergen"
    }
    async fn execute(
        &self,
        node: &PipelineNode,
        _ctx: &Context,
        _graph: &PipelineGraph,
    ) -> attractor_types::Result<Outcome> {
        let mut updates = HashMap::new();
        updates.insert(
            format!("{}.completed", node.id),
            serde_json::Value::Bool(true),
        );
        updates.insert(
            format!("{}.result", node.id),
            serde_json::Value::String("mock result".into()),
        );
        if let Some(ref prompt) = node.prompt {
            updates.insert(
                format!("{}.prompt", node.id),
                serde_json::Value::String(prompt.clone()),
            );
        }
        Ok(Outcome {
            status: StageStatus::Success,
            preferred_label: None,
            suggested_next_ids: vec![],
            context_updates: updates,
            notes: "mock codergen".into(),
            failure_reason: None,
        })
    }
}

/// Build an executor with a mock codergen handler (no real CLI calls).
fn executor() -> PipelineExecutor {
    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(ExitHandler);
    registry.register(ConditionalHandler);
    registry.register(MockCodergenHandler);
    PipelineExecutor::new(registry)
}

// ---------------------------------------------------------------------------
// Test 1: Simple linear pipeline (start -> process -> done)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn simple_linear_pipeline_completes_in_order() {
    let graph = build_graph(
        r#"digraph Simple {
            start [shape="Mdiamond"]
            process [shape="box", prompt="Process data"]
            done [shape="Msquare"]
            start -> process -> done
        }"#,
    );

    // Validation should produce no errors
    let diags = validate_or_raise(&graph).expect("validation should pass");
    let errors: Vec<_> = diags
        .iter()
        .filter(|d| d.severity == attractor_pipeline::Severity::Error)
        .collect();
    assert!(errors.is_empty(), "Expected no validation errors: {errors:?}");

    // Execute
    let result = executor().run(&graph).await.expect("pipeline should succeed");

    // All 3 nodes should complete in order
    assert_eq!(
        result.completed_nodes,
        vec!["start", "process", "done"],
        "Nodes should complete in linear order"
    );

    // Each node should have a success outcome
    for node_id in &["start", "process", "done"] {
        let outcome = result
            .node_outcomes
            .get(*node_id)
            .unwrap_or_else(|| panic!("missing outcome for {node_id}"));
        assert_eq!(
            outcome.status,
            StageStatus::Success,
            "node '{node_id}' should be Success"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 2: Branching pipeline with conditions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn branching_pipeline_routes_via_condition() {
    let graph = build_graph(
        r#"digraph Branch {
            start [shape="Mdiamond"]
            check [shape="diamond"]
            path_a [shape="box", prompt="Path A"]
            path_b [shape="box", prompt="Path B"]
            done [shape="Msquare"]
            start -> check
            check -> path_a [condition="outcome=success"]
            check -> path_b
            path_a -> done
            path_b -> done
        }"#,
    );

    let result = executor().run(&graph).await.expect("pipeline should succeed");

    // The conditional handler returns Success, so outcome=success.
    // The edge to path_a has condition="outcome=success" which should match.
    assert!(
        result.completed_nodes.contains(&"path_a".to_string()),
        "path_a should be visited when condition matches; completed: {:?}",
        result.completed_nodes
    );
    assert!(
        !result.completed_nodes.contains(&"path_b".to_string()),
        "path_b should NOT be visited; completed: {:?}",
        result.completed_nodes
    );
    assert!(
        result.completed_nodes.contains(&"done".to_string()),
        "done should be reached"
    );
}

// ---------------------------------------------------------------------------
// Test 3: Pipeline with goal gates
// ---------------------------------------------------------------------------

#[tokio::test]
async fn goal_gate_satisfied_pipeline_completes() {
    let graph = build_graph(
        r#"digraph GoalGate {
            start [shape="Mdiamond"]
            review [shape="box", goal_gate=true, prompt="Review code"]
            done [shape="Msquare"]
            start -> review -> done
        }"#,
    );

    // The default codergen handler returns Success, satisfying the goal gate.
    let result = executor().run(&graph).await.expect("pipeline should succeed");

    assert!(
        result.completed_nodes.contains(&"review".to_string()),
        "review (goal gate) should be visited"
    );
    assert!(
        result.completed_nodes.contains(&"done".to_string()),
        "done should be reached after goal gate passes"
    );
    assert_eq!(
        result.node_outcomes["review"].status,
        StageStatus::Success,
        "review node should succeed"
    );
}

// ---------------------------------------------------------------------------
// Test 4: Validation catches missing start node
// ---------------------------------------------------------------------------

#[tokio::test]
async fn validation_catches_missing_start_node() {
    let graph = build_graph(
        r#"digraph NoStart {
            process [shape="box", prompt="Work"]
            done [shape="Msquare"]
            process -> done
        }"#,
    );

    // validate_or_raise should return an error
    let result = validate_or_raise(&graph);
    assert!(result.is_err(), "validation should fail without a start node");

    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.to_lowercase().contains("start node"),
        "error should mention start node; got: {err_msg}"
    );

    // Also verify the advisory validate() produces an Error-level diagnostic
    let diags = validate(&graph);
    assert!(
        diags.iter().any(|d| d.rule == "start_node"
            && d.severity == attractor_pipeline::Severity::Error),
        "Expected start_node error diagnostic; got: {diags:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 5: Stylesheet application
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stylesheet_applies_model_to_nodes() {
    let mut graph = build_graph(
        r#"digraph Styled {
            start [shape="Mdiamond"]
            analyze [shape="box", prompt="Analyze", class="fast"]
            summarize [shape="box", prompt="Summarize", class="slow"]
            done [shape="Msquare"]
            start -> analyze -> summarize -> done
        }"#,
    );

    let css = r#"
        * { llm_model: default-model; llm_provider: anthropic; }
        .fast { llm_model: fast-model; }
        #summarize { llm_model: summarize-model; reasoning_effort: high; }
    "#;
    let stylesheet = parse_stylesheet(css).expect("stylesheet parse should succeed");
    apply_stylesheet(&mut graph, &stylesheet);

    // Universal rule sets defaults on all nodes
    let start_node = graph.node("start").unwrap();
    assert_eq!(
        start_node.llm_model.as_deref(),
        Some("default-model"),
        "start should get universal model"
    );
    assert_eq!(
        start_node.llm_provider.as_deref(),
        Some("anthropic"),
        "start should get universal provider"
    );

    // .fast class overrides universal for analyze
    let analyze_node = graph.node("analyze").unwrap();
    assert_eq!(
        analyze_node.llm_model.as_deref(),
        Some("fast-model"),
        "analyze should get .fast class model"
    );

    // #summarize ID selector overrides .slow class and universal
    let summarize_node = graph.node("summarize").unwrap();
    assert_eq!(
        summarize_node.llm_model.as_deref(),
        Some("summarize-model"),
        "summarize should get ID-specific model"
    );
    assert_eq!(
        summarize_node.reasoning_effort.as_deref(),
        Some("high"),
        "summarize should get reasoning_effort from ID selector"
    );

    // The graph should still be valid and executable after stylesheet application
    let result = executor()
        .run(&graph)
        .await
        .expect("styled pipeline should execute");
    assert_eq!(result.completed_nodes.len(), 4);
}

// ---------------------------------------------------------------------------
// Test 6: Context propagation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn context_propagation_across_nodes() {
    let graph = build_graph(
        r#"digraph ContextTest {
            start [shape="Mdiamond"]
            step_one [shape="box", prompt="First step"]
            step_two [shape="box", prompt="Second step"]
            done [shape="Msquare"]
            start -> step_one -> step_two -> done
        }"#,
    );

    let result = executor().run(&graph).await.expect("pipeline should succeed");

    // The codergen handler sets "<node_id>.prompt" and "<node_id>.completed" in context_updates.
    // These should propagate through the engine into final_context.
    assert_eq!(
        result.final_context.get("step_one.prompt"),
        Some(&serde_json::json!("First step")),
        "step_one.prompt should be in final context"
    );
    assert_eq!(
        result.final_context.get("step_one.completed"),
        Some(&serde_json::json!(true)),
        "step_one.completed should be in final context"
    );
    assert_eq!(
        result.final_context.get("step_two.prompt"),
        Some(&serde_json::json!("Second step")),
        "step_two.prompt should be in final context"
    );
    assert_eq!(
        result.final_context.get("step_two.completed"),
        Some(&serde_json::json!(true)),
        "step_two.completed should be in final context"
    );

    // Engine sets "outcome" to the status string of the last non-exit node
    assert_eq!(
        result.final_context.get("outcome"),
        Some(&serde_json::json!("success")),
        "outcome should be set in final context"
    );
}

// ---------------------------------------------------------------------------
// Test 7: Pipeline with many nodes (10-node linear chain)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ten_node_linear_pipeline_completes() {
    // Programmatically build a 10-node linear DOT graph
    let mut dot = String::from("digraph ManyNodes {\n");
    dot.push_str("    start [shape=\"Mdiamond\"]\n");
    for i in 1..=8 {
        dot.push_str(&format!(
            "    step_{i} [shape=\"box\", prompt=\"Step {i}\"]\n"
        ));
    }
    dot.push_str("    done [shape=\"Msquare\"]\n");

    // Edges: start -> step_1 -> step_2 -> ... -> step_8 -> done
    dot.push_str("    start -> step_1\n");
    for i in 1..8 {
        dot.push_str(&format!("    step_{i} -> step_{}\n", i + 1));
    }
    dot.push_str("    step_8 -> done\n");
    dot.push_str("}\n");

    let graph = build_graph(&dot);

    // Validate
    let diags = validate_or_raise(&graph).expect("10-node graph should validate");
    let errors: Vec<_> = diags
        .iter()
        .filter(|d| d.severity == attractor_pipeline::Severity::Error)
        .collect();
    assert!(errors.is_empty(), "No validation errors expected: {errors:?}");

    // Execute
    let result = executor().run(&graph).await.expect("pipeline should succeed");

    // Total: start + 8 steps + done = 10 nodes
    assert_eq!(
        result.completed_nodes.len(),
        10,
        "All 10 nodes should complete; got: {:?}",
        result.completed_nodes
    );

    // Verify ordering: start first, done last
    assert_eq!(result.completed_nodes[0], "start");
    assert_eq!(result.completed_nodes[9], "done");

    // Verify all step nodes are present
    for i in 1..=8 {
        let node_id = format!("step_{i}");
        assert!(
            result.completed_nodes.contains(&node_id),
            "missing {node_id}"
        );
    }

    // All outcomes should be Success
    for (id, outcome) in &result.node_outcomes {
        assert_eq!(
            outcome.status,
            StageStatus::Success,
            "node '{id}' should succeed"
        );
    }
}

