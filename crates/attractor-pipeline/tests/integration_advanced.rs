//! Advanced integration tests for the Attractor pipeline engine (split from integration.rs).
//!
//! Each test exercises the full pipeline: parse DOT -> build graph -> validate -> execute -> verify.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;

use attractor_dot::parse;
use attractor_pipeline::{
    apply_stylesheet, parse_stylesheet, validate, ConditionalHandler,
    ExitHandler, HandlerRegistry, NodeHandler, PipelineExecutor, PipelineGraph, PipelineNode,
    StartHandler,
};
use attractor_types::{Context, Outcome, StageStatus};

// ---------------------------------------------------------------------------
// Helpers (duplicated from integration.rs — each test binary is standalone)
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
// Test 8: Edge selection priority (weighted, labeled, condition edges)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn edge_selection_respects_condition_over_weight() {
    // When a condition matches, it takes priority over weight.
    // check -> low_weight has condition="outcome=success" (matches because conditional handler
    // returns success), but low weight.
    // check -> high_weight has higher weight but no condition.
    // Condition match should win.
    let graph = build_graph(
        r#"digraph EdgePriority {
            start [shape="Mdiamond"]
            check [shape="diamond"]
            low_weight [shape="box", prompt="Low weight path"]
            high_weight [shape="box", prompt="High weight path"]
            done [shape="Msquare"]
            start -> check
            check -> low_weight [condition="outcome=success", weight=1]
            check -> high_weight [weight=100]
            low_weight -> done
            high_weight -> done
        }"#,
    );

    let result = executor().run(&graph).await.expect("pipeline should succeed");

    assert!(
        result.completed_nodes.contains(&"low_weight".to_string()),
        "condition match should win over weight; completed: {:?}",
        result.completed_nodes
    );
    assert!(
        !result.completed_nodes.contains(&"high_weight".to_string()),
        "high_weight should not be taken; completed: {:?}",
        result.completed_nodes
    );
}

// ---------------------------------------------------------------------------
// Test 9: Goal gate failure without retry target returns error
// ---------------------------------------------------------------------------

#[tokio::test]
async fn goal_gate_unsatisfied_without_retry_returns_error() {
    // Use a custom handler that always returns Fail for the codergen type.
    struct AlwaysFailHandler;

    #[async_trait]
    impl NodeHandler for AlwaysFailHandler {
        fn handler_type(&self) -> &str {
            "codergen"
        }
        async fn execute(
            &self,
            _node: &PipelineNode,
            _ctx: &Context,
            _graph: &PipelineGraph,
        ) -> attractor_types::Result<Outcome> {
            Ok(Outcome::fail("intentional failure for test"))
        }
    }

    let graph = build_graph(
        r#"digraph GoalGateFail {
            start [shape="Mdiamond"]
            review [shape="box", goal_gate=true, prompt="Review code"]
            done [shape="Msquare"]
            start -> review -> done
        }"#,
    );

    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(ExitHandler);
    registry.register(ConditionalHandler);
    registry.register(AlwaysFailHandler);

    let exec = PipelineExecutor::new(registry);
    let result = exec.run(&graph).await;

    assert!(result.is_err(), "pipeline should fail with unsatisfied goal gate");
    let err = result.unwrap_err();
    let err_msg = err.to_string();
    assert!(
        err_msg.contains("Goal gate unsatisfied") || err_msg.contains("goal_gate"),
        "error should mention goal gate; got: {err_msg}"
    );
}

// ---------------------------------------------------------------------------
// Test 10: Goal gate with retry target loops back and eventually succeeds
// ---------------------------------------------------------------------------

#[tokio::test]
async fn goal_gate_with_retry_target_retries_then_succeeds() {
    // A handler that fails on the first call but succeeds on subsequent calls.
    struct FailOnceThenSucceedHandler {
        call_count: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl NodeHandler for FailOnceThenSucceedHandler {
        fn handler_type(&self) -> &str {
            "codergen"
        }
        async fn execute(
            &self,
            node: &PipelineNode,
            _ctx: &Context,
            _graph: &PipelineGraph,
        ) -> attractor_types::Result<Outcome> {
            let count = self.call_count.fetch_add(1, Ordering::SeqCst);
            if count == 0 {
                Ok(Outcome::fail("first attempt fails"))
            } else {
                let mut updates = HashMap::new();
                updates.insert(
                    format!("{}.completed", node.id),
                    serde_json::json!(true),
                );
                Ok(Outcome {
                    status: StageStatus::Success,
                    preferred_label: None,
                    suggested_next_ids: vec![],
                    context_updates: updates,
                    notes: "retry succeeded".into(),
                    failure_reason: None,
                })
            }
        }
    }

    let graph = build_graph(
        r#"digraph GoalGateRetry {
            start [shape="Mdiamond"]
            review [shape="box", goal_gate=true, retry_target="start", prompt="Review"]
            done [shape="Msquare"]
            start -> review -> done
        }"#,
    );

    let call_count = Arc::new(AtomicUsize::new(0));
    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(ExitHandler);
    registry.register(ConditionalHandler);
    registry.register(FailOnceThenSucceedHandler {
        call_count: call_count.clone(),
    });

    let exec = PipelineExecutor::new(registry);
    let result = exec.run(&graph).await.expect("pipeline should succeed after retry");

    // The handler was called at least twice (once fail, once success)
    assert!(
        call_count.load(Ordering::SeqCst) >= 2,
        "handler should be called at least twice (fail then succeed)"
    );
    assert!(
        result.completed_nodes.contains(&"done".to_string()),
        "pipeline should reach done after retry"
    );
}

// ---------------------------------------------------------------------------
// Test 11: Validation catches multiple structural errors
// ---------------------------------------------------------------------------

#[tokio::test]
async fn validation_catches_multiple_errors() {
    // Graph with no start node AND no terminal node
    let graph = build_graph(
        r#"digraph Bad {
            a [shape="box", prompt="A"]
            b [shape="box", prompt="B"]
            a -> b
        }"#,
    );

    let diags = validate(&graph);
    let error_rules: Vec<_> = diags
        .iter()
        .filter(|d| d.severity == attractor_pipeline::Severity::Error)
        .map(|d| d.rule.as_str())
        .collect();

    assert!(
        error_rules.contains(&"start_node"),
        "should flag missing start node; got rules: {error_rules:?}"
    );
    assert!(
        error_rules.contains(&"terminal_node"),
        "should flag missing terminal node; got rules: {error_rules:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 12: Validation detects unreachable nodes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn validation_detects_unreachable_nodes() {
    let graph = build_graph(
        r#"digraph Unreachable {
            start [shape="Mdiamond"]
            reachable [shape="box", prompt="Reachable"]
            orphan [shape="box", prompt="Orphan"]
            done [shape="Msquare"]
            start -> reachable -> done
        }"#,
    );

    let diags = validate(&graph);
    let unreachable_diags: Vec<_> = diags
        .iter()
        .filter(|d| d.rule == "reachability" && d.severity == attractor_pipeline::Severity::Error)
        .collect();

    assert!(
        !unreachable_diags.is_empty(),
        "should detect orphan node as unreachable"
    );
    assert!(
        unreachable_diags
            .iter()
            .any(|d| d.message.contains("orphan")),
        "unreachable diagnostic should mention orphan; got: {unreachable_diags:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 13: Edge weight tiebreaker selects highest-weight unconditional edge
// ---------------------------------------------------------------------------

#[tokio::test]
async fn edge_weight_tiebreaker_selects_highest_weight() {
    // Two unconditional edges from check: one with weight=1, one with weight=10.
    // The higher-weight edge should win.
    let graph = build_graph(
        r#"digraph WeightTest {
            start [shape="Mdiamond"]
            check [shape="box", prompt="Check"]
            low [shape="box", prompt="Low weight"]
            high [shape="box", prompt="High weight"]
            done [shape="Msquare"]
            start -> check
            check -> low [weight=1]
            check -> high [weight=10]
            low -> done
            high -> done
        }"#,
    );

    let result = executor().run(&graph).await.expect("pipeline should succeed");

    assert!(
        result.completed_nodes.contains(&"high".to_string()),
        "higher weight should be selected; completed: {:?}",
        result.completed_nodes
    );
    assert!(
        !result.completed_nodes.contains(&"low".to_string()),
        "lower weight should not be taken"
    );
}

// ---------------------------------------------------------------------------
// Test 14: Full round-trip with graph-level goal attribute
// ---------------------------------------------------------------------------

#[tokio::test]
async fn graph_goal_attribute_propagates_to_context() {
    let graph = build_graph(
        r#"digraph GoalTest {
            goal = "Build a working pipeline"
            start [shape="Mdiamond"]
            work [shape="box", prompt="Do the work"]
            done [shape="Msquare"]
            start -> work -> done
        }"#,
    );

    assert_eq!(graph.goal, "Build a working pipeline");

    let result = executor().run(&graph).await.expect("pipeline should succeed");

    // Graph attrs are loaded into context during initialization
    assert_eq!(
        result.final_context.get("goal"),
        Some(&serde_json::json!("Build a working pipeline")),
        "goal should be in final context"
    );
}

// ---------------------------------------------------------------------------
// Test 15: Condition-based routing with fail condition
// ---------------------------------------------------------------------------

#[tokio::test]
async fn condition_routes_to_fallback_on_no_match() {
    // When outcome=success but condition requires outcome=fail,
    // the unconditional fallback edge should be taken.
    let graph = build_graph(
        r#"digraph CondFallback {
            start [shape="Mdiamond"]
            check [shape="diamond"]
            fail_path [shape="box", prompt="Fail path"]
            default_path [shape="box", prompt="Default path"]
            done [shape="Msquare"]
            start -> check
            check -> fail_path [condition="outcome=fail"]
            check -> default_path
            fail_path -> done
            default_path -> done
        }"#,
    );

    let result = executor().run(&graph).await.expect("pipeline should succeed");

    // Conditional handler returns Success, so outcome=success, which does NOT match
    // the condition "outcome=fail". The unconditional edge to default_path should be taken.
    assert!(
        result
            .completed_nodes
            .contains(&"default_path".to_string()),
        "default_path should be taken when condition does not match; completed: {:?}",
        result.completed_nodes
    );
    assert!(
        !result.completed_nodes.contains(&"fail_path".to_string()),
        "fail_path should not be taken"
    );
}
