use super::*;
use crate::graph::PipelineGraph;
use crate::handler::{HandlerRegistry, NodeHandler, StartHandler, ExitHandler, ConditionalHandler};
use async_trait::async_trait;

fn parse_graph(dot: &str) -> PipelineGraph {
    let parsed = attractor_dot::parse(dot).unwrap();
    PipelineGraph::from_dot(parsed).unwrap()
}

/// A mock codergen handler that returns Success without shelling out to Claude CLI.
struct MockCodergenHandler;

#[async_trait]
impl NodeHandler for MockCodergenHandler {
    fn handler_type(&self) -> &str {
        "codergen"
    }
    async fn execute(
        &self,
        node: &crate::graph::PipelineNode,
        _ctx: &Context,
        _graph: &PipelineGraph,
    ) -> Result<Outcome> {
        let mut updates = HashMap::new();
        updates.insert(
            format!("{}.completed", node.id),
            serde_json::Value::Bool(true),
        );
        updates.insert(
            format!("{}.result", node.id),
            serde_json::Value::String("mock result".into()),
        );
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

/// Build a test registry with mock codergen handler (no real CLI calls).
fn test_registry() -> HandlerRegistry {
    let mut reg = HandlerRegistry::new();
    reg.register(StartHandler);
    reg.register(ExitHandler);
    reg.register(ConditionalHandler);
    reg.register(MockCodergenHandler);
    reg
}

fn test_executor() -> PipelineExecutor {
    PipelineExecutor::new(test_registry())
}

// Test 1: Linear pipeline (start -> A -> exit) completes successfully
#[tokio::test]
async fn linear_pipeline_completes() {
    let graph = parse_graph(
        r#"digraph G {
            start [shape="Mdiamond"]
            process [shape="box", label="Process", prompt="Do work"]
            done [shape="Msquare"]
            start -> process -> done
        }"#,
    );
    let executor = test_executor();
    let result = executor.run(&graph).await.unwrap();

    assert_eq!(result.completed_nodes, vec!["start", "process", "done"]);
    assert!(result.node_outcomes.contains_key("start"));
    assert!(result.node_outcomes.contains_key("process"));
    assert!(result.node_outcomes.contains_key("done"));
    assert_eq!(
        result.node_outcomes["start"].status,
        StageStatus::Success
    );
    assert_eq!(
        result.node_outcomes["process"].status,
        StageStatus::Success
    );
    assert_eq!(
        result.node_outcomes["done"].status,
        StageStatus::Success
    );
}

// Test 2: Branching pipeline routes based on conditions
#[tokio::test]
async fn branching_pipeline_routes_on_condition() {
    // The mock codergen handler returns Success, so outcome=success.
    // Edge to "yes_path" has condition="outcome=success", so it should be taken.
    let graph = parse_graph(
        r#"digraph G {
            start [shape="Mdiamond"]
            check [shape="box", label="Check", prompt="Check something"]
            yes_path [shape="box", label="Yes Path", prompt="Yes"]
            no_path [shape="box", label="No Path", prompt="No"]
            done [shape="Msquare"]
            start -> check
            check -> yes_path [condition="outcome=success"]
            check -> no_path [condition="outcome=fail"]
            yes_path -> done
            no_path -> done
        }"#,
    );
    let executor = test_executor();
    let result = executor.run(&graph).await.unwrap();

    assert!(result.completed_nodes.contains(&"yes_path".to_string()));
    assert!(!result.completed_nodes.contains(&"no_path".to_string()));
}

// Test 3: Pipeline with no start node returns error
#[tokio::test]
async fn no_start_node_returns_error() {
    let graph = parse_graph(
        r#"digraph G {
            process [shape="box", label="Do work"]
            done [shape="Msquare"]
            process -> done
        }"#,
    );
    let executor = test_executor();
    let result = executor.run(&graph).await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    match err {
        AttractorError::ValidationError(msg) => {
            assert!(
                msg.contains("start node"),
                "Expected error about start node, got: {msg}"
            );
        }
        other => panic!("Expected ValidationError, got: {other:?}"),
    }
}

// Test 4: Context updates from one node visible to next (verify via final_context)
#[tokio::test]
async fn context_updates_propagate() {
    // The mock codergen handler sets context_updates with
    // "<node_id>.completed", "<node_id>.result", etc.
    let graph = parse_graph(
        r#"digraph G {
            start [shape="Mdiamond"]
            step [shape="box", label="Step", prompt="Generate code"]
            done [shape="Msquare"]
            start -> step -> done
        }"#,
    );
    let executor = test_executor();
    let result = executor.run(&graph).await.unwrap();

    // The mock handler marks the node as completed
    assert_eq!(
        result.final_context.get("step.completed"),
        Some(&serde_json::Value::Bool(true)),
    );
    // The mock handler stores a result in "<node_id>.result"
    assert!(
        result.final_context.contains_key("step.result"),
        "Expected step.result in final context, keys: {:?}",
        result.final_context.keys().collect::<Vec<_>>()
    );
    // The engine also sets "outcome" in context
    assert_eq!(
        result.final_context.get("outcome"),
        Some(&serde_json::Value::String("success".into())),
    );
}

// Test 5: Goal gate failure with retry target loops back
#[tokio::test]
async fn goal_gate_failure_with_retry_loops_back() {
    // The mock handler returns success, so goal gate is satisfied and no loop occurs.
    // Here we verify the goal gate path doesn't error when gates are satisfied.
    let graph = parse_graph(
        r#"digraph G {
            start [shape="Mdiamond"]
            review [shape="box", goal_gate=true, retry_target="start", label="Review", prompt="Review code"]
            done [shape="Msquare"]
            start -> review -> done
        }"#,
    );
    let executor = test_executor();
    let result = executor.run(&graph).await.unwrap();

    // Goal gate is satisfied (mock returns success), so pipeline completes
    assert!(result.completed_nodes.contains(&"done".to_string()));
}

// Test 6: Goal gate failure without retry target returns error
#[tokio::test]
async fn goal_gate_failure_without_retry_returns_error() {
    // To test this, we need a custom handler that returns Fail for the goal gate node.
    use async_trait::async_trait;
    use crate::handler::NodeHandler;
    use crate::graph::PipelineNode;

    struct FailHandler;

    #[async_trait]
    impl NodeHandler for FailHandler {
        fn handler_type(&self) -> &str {
            "codergen"
        }
        async fn execute(
            &self,
            _node: &PipelineNode,
            _ctx: &Context,
            _graph: &PipelineGraph,
        ) -> Result<Outcome> {
            Ok(Outcome::fail("intentional failure"))
        }
    }

    let graph = parse_graph(
        r#"digraph G {
            start [shape="Mdiamond"]
            review [shape="box", goal_gate=true, label="Review", prompt="Review"]
            done [shape="Msquare"]
            start -> review -> done
        }"#,
    );

    let mut registry = HandlerRegistry::new();
    registry.register(crate::handler::StartHandler);
    registry.register(crate::handler::ExitHandler);
    registry.register(crate::handler::ConditionalHandler);
    registry.register(FailHandler);

    let executor = PipelineExecutor::new(registry);
    let result = executor.run(&graph).await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    match err {
        AttractorError::GoalGateUnsatisfied { node } => {
            assert_eq!(node, "review");
        }
        other => panic!("Expected GoalGateUnsatisfied, got: {other:?}"),
    }
}

// Test 7: Goal gate failure with retry target retries correctly
#[tokio::test]
async fn goal_gate_failure_with_retry_target_retries() {
    use async_trait::async_trait;
    use crate::handler::NodeHandler;
    use crate::graph::PipelineNode;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    // Handler that fails on first call, succeeds on subsequent calls
    struct RetryableHandler {
        call_count: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl NodeHandler for RetryableHandler {
        fn handler_type(&self) -> &str {
            "codergen"
        }
        async fn execute(
            &self,
            _node: &PipelineNode,
            _ctx: &Context,
            _graph: &PipelineGraph,
        ) -> Result<Outcome> {
            let count = self.call_count.fetch_add(1, Ordering::SeqCst);
            if count == 0 {
                Ok(Outcome::fail("first attempt fails"))
            } else {
                Ok(Outcome::success("retry succeeded"))
            }
        }
    }

    let graph = parse_graph(
        r#"digraph G {
            start [shape="Mdiamond"]
            review [shape="box", goal_gate=true, retry_target="start", label="Review", prompt="Review"]
            done [shape="Msquare"]
            start -> review -> done
        }"#,
    );

    let call_count = Arc::new(AtomicUsize::new(0));
    let mut registry = HandlerRegistry::new();
    registry.register(crate::handler::StartHandler);
    registry.register(crate::handler::ExitHandler);
    registry.register(crate::handler::ConditionalHandler);
    registry.register(RetryableHandler {
        call_count: call_count.clone(),
    });

    let executor = PipelineExecutor::new(registry);
    let result = executor.run(&graph).await.unwrap();

    // Should have retried: start -> review(fail) -> exit(goal gate fails, retry to start)
    // -> start -> review(success) -> exit(done)
    assert!(result.completed_nodes.contains(&"done".to_string()));
    // The handler was called twice (once fail, once success)
    assert_eq!(call_count.load(Ordering::SeqCst), 2);
}

// Test 8a: Context-based edge conditions are resolved from pipeline context
#[tokio::test]
async fn context_based_conditions_resolve_from_context() {
    // A handler that sets a context key and succeeds
    struct ContextSettingHandler;

    #[async_trait]
    impl NodeHandler for ContextSettingHandler {
        fn handler_type(&self) -> &str {
            "codergen"
        }
        async fn execute(
            &self,
            node: &crate::graph::PipelineNode,
            _ctx: &Context,
            _graph: &PipelineGraph,
        ) -> Result<Outcome> {
            let mut updates = HashMap::new();
            updates.insert(
                format!("{}.completed", node.id),
                serde_json::Value::Bool(true),
            );
            updates.insert(
                "deploy_env".to_string(),
                serde_json::Value::String("prod".to_string()),
            );
            Ok(Outcome {
                status: StageStatus::Success,
                preferred_label: None,
                suggested_next_ids: vec![],
                context_updates: updates,
                notes: "set context".into(),
                failure_reason: None,
            })
        }
    }

    let graph = parse_graph(
        r#"digraph G {
            start [shape="Mdiamond"]
            setup [shape="box", label="Setup", prompt="setup"]
            prod_path [shape="box", label="Prod", prompt="prod"]
            dev_path [shape="box", label="Dev", prompt="dev"]
            done [shape="Msquare"]
            start -> setup
            setup -> prod_path [condition="deploy_env=prod"]
            setup -> dev_path [condition="deploy_env=dev"]
            prod_path -> done
            dev_path -> done
        }"#,
    );

    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(ExitHandler);
    registry.register(ConditionalHandler);
    registry.register(ContextSettingHandler);

    let executor = PipelineExecutor::new(registry);
    let result = executor.run(&graph).await.unwrap();

    // The condition "deploy_env=prod" should route to prod_path
    assert!(
        result.completed_nodes.contains(&"prod_path".to_string()),
        "Expected prod_path in completed nodes, got: {:?}",
        result.completed_nodes
    );
    assert!(
        !result.completed_nodes.contains(&"dev_path".to_string()),
        "dev_path should not be in completed nodes"
    );
}

// Test 8: PipelineExecutor::new and with_default_registry
#[test]
fn executor_constructors() {
    let executor = PipelineExecutor::with_default_registry();
    assert!(executor.registry.has("start"));
    assert!(executor.registry.has("exit"));
    assert!(executor.registry.has("codergen"));

    let custom = PipelineExecutor::new(HandlerRegistry::new());
    assert!(!custom.registry.has("start"));
}

// Test 9: Step limit aborts runaway pipelines
#[tokio::test]
async fn step_limit_aborts_pipeline() {
    // A pipeline with a loop that never exits will hit the step limit.
    let graph = parse_graph(
        r#"digraph G {
            start [shape="Mdiamond"]
            loop_node [shape="box", label="Loop", prompt="loop"]
            done [shape="Msquare"]
            start -> loop_node
            loop_node -> loop_node [condition="outcome=success"]
            loop_node -> done [condition="outcome=fail"]
        }"#,
    );
    let executor = test_executor();
    let context = Context::new();
    context.set("max_steps", serde_json::json!(5)).await;

    let result = executor.run_with_context(&graph, context).await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("maximum step count"),
        "Expected step limit error, got: {err}"
    );
}

// Test 10: Budget limit aborts pipeline when cost exceeds cap
#[tokio::test]
async fn budget_limit_aborts_pipeline() {
    use crate::graph::PipelineNode;

    /// Handler that reports a cost in its context_updates.
    struct CostlyHandler;

    #[async_trait::async_trait]
    impl NodeHandler for CostlyHandler {
        fn handler_type(&self) -> &str {
            "codergen"
        }
        async fn execute(
            &self,
            node: &PipelineNode,
            _ctx: &Context,
            _graph: &PipelineGraph,
        ) -> Result<Outcome> {
            let mut updates = HashMap::new();
            updates.insert(
                format!("{}.completed", node.id),
                serde_json::Value::Bool(true),
            );
            updates.insert(
                format!("{}.cost_usd", node.id),
                serde_json::json!(1.50),
            );
            Ok(Outcome {
                status: StageStatus::Success,
                preferred_label: None,
                suggested_next_ids: vec![],
                context_updates: updates,
                notes: "costly operation".into(),
                failure_reason: None,
            })
        }
    }

    let graph = parse_graph(
        r#"digraph G {
            start [shape="Mdiamond"]
            step1 [shape="box", label="Step1", prompt="work"]
            step2 [shape="box", label="Step2", prompt="work"]
            done [shape="Msquare"]
            start -> step1 -> step2 -> done
        }"#,
    );

    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(ExitHandler);
    registry.register(ConditionalHandler);
    registry.register(CostlyHandler);

    let executor = PipelineExecutor::new(registry);
    let context = Context::new();
    // Budget of $2.00, but two nodes cost $1.50 each = $3.00 total
    context.set("max_budget_usd", serde_json::json!(2.0)).await;

    let result = executor.run_with_context(&graph, context).await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("exceeded budget"),
        "Expected budget error, got: {err}"
    );
}
