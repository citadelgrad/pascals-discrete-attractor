use super::*;
use crate::graph::PipelineGraph;
use crate::handler::{ConditionalHandler, ExitHandler, HandlerRegistry, NodeHandler, StartHandler};
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
            node [llm_provider="claude"]
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
    assert_eq!(result.node_outcomes["start"].status, StageStatus::Success);
    assert_eq!(result.node_outcomes["process"].status, StageStatus::Success);
    assert_eq!(result.node_outcomes["done"].status, StageStatus::Success);
}

// Test 2: Branching pipeline routes based on conditions
#[tokio::test]
async fn branching_pipeline_routes_on_condition() {
    // The mock codergen handler returns Success, so outcome=success.
    // Edge to "yes_path" has condition="outcome=success", so it should be taken.
    let graph = parse_graph(
        r#"digraph G {
            node [llm_provider="claude"]
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

#[tokio::test]
async fn unsupported_capability_invokes_no_handler_event_or_checkpoint() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct CountingHandler(Arc<AtomicUsize>);

    #[async_trait]
    impl NodeHandler for CountingHandler {
        fn handler_type(&self) -> &str {
            "codergen"
        }

        async fn execute(
            &self,
            _node: &crate::PipelineNode,
            _ctx: &Context,
            _graph: &PipelineGraph,
        ) -> Result<Outcome> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Outcome::success("unexpected"))
        }
    }

    let graph = parse_graph(
        r#"digraph G {
            start [shape="Mdiamond"]
            work [shape="box", prompt="work", llm_provider="claude", fidelity="compact"]
            done [shape="Msquare"]
            start -> work -> done
        }"#,
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(ExitHandler);
    registry.register(CountingHandler(Arc::clone(&calls)));
    let logs = tempfile::tempdir().unwrap();
    let emitter = EventEmitter::new(16);
    let mut receiver = emitter.subscribe();

    let result = PipelineExecutor::new(registry)
        .with_event_emitter(emitter)
        .run_with_checkpoint(&graph, Context::new(), logs.path())
        .await;

    assert!(result.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(!logs.path().join("checkpoint.json").exists());
    assert!(receiver.try_recv().is_err());
}

#[tokio::test]
async fn unsupported_topology_invokes_no_handler_and_writes_no_checkpoint() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct CountingHandler {
        name: &'static str,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl NodeHandler for CountingHandler {
        fn handler_type(&self) -> &str {
            self.name
        }

        async fn execute(
            &self,
            _node: &crate::PipelineNode,
            _ctx: &Context,
            _graph: &PipelineGraph,
        ) -> Result<Outcome> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Outcome::success("unexpected"))
        }
    }

    let cases = [
        (
            r#"digraph G {
            start [shape="Mdiamond"]
            fork [shape="component"]
            left [shape="diamond"]
            right [shape="diamond"]
            done [shape="Msquare"]
            start -> fork
            fork -> left
            fork -> right
            left -> done
            right -> done
        }"#,
            "one successor per step",
        ),
        (
            r#"digraph G {
            start [shape="Mdiamond"]
            merge [shape="tripleoctagon"]
            done [shape="Msquare"]
            start -> merge -> done
        }"#,
            "cannot merge branch results",
        ),
    ];

    for (source, expected) in cases {
        let graph = parse_graph(source);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = HandlerRegistry::new();
        for name in [
            "start",
            "exit",
            "conditional",
            "parallel",
            "parallel.fan_in",
        ] {
            registry.register(CountingHandler {
                name,
                calls: Arc::clone(&calls),
            });
        }
        let logs = tempfile::tempdir().unwrap();

        let error = PipelineExecutor::new(registry)
            .run_with_checkpoint(&graph, Context::new(), logs.path())
            .await
            .unwrap_err();

        assert!(
            matches!(error, AttractorError::ValidationError(ref message) if message.contains(expected)),
            "{error:?}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(!logs.path().join("checkpoint.json").exists());
    }
}

#[tokio::test]
async fn custom_handler_known_shape_conflict_invokes_no_handler_and_writes_no_checkpoint() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct CountingHandler {
        name: &'static str,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl NodeHandler for CountingHandler {
        fn handler_type(&self) -> &str {
            self.name
        }

        async fn execute(
            &self,
            _node: &crate::PipelineNode,
            _ctx: &Context,
            _graph: &PipelineGraph,
        ) -> Result<Outcome> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Outcome::success("unexpected"))
        }
    }

    let graph = parse_graph(
        r#"digraph G {
            start [shape="Mdiamond"]
            disguised [shape="Msquare", type="custom.review"]
            done [shape="Msquare"]
            start -> disguised -> done
        }"#,
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = HandlerRegistry::new();
    for name in ["start", "exit", "custom.review"] {
        registry.register(CountingHandler {
            name,
            calls: Arc::clone(&calls),
        });
    }
    let logs = tempfile::tempdir().unwrap();

    let result = PipelineExecutor::new(registry)
        .run_with_checkpoint(&graph, Context::new(), logs.path())
        .await;

    let error = result.expect_err("known shape must conflict with a custom handler");
    assert!(
        matches!(error, AttractorError::ValidationError(ref message) if message.contains("conflicting role signals")),
        "{error:?}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(!logs.path().join("checkpoint.json").exists());
}

// Test 4: Context updates from one node visible to next (verify via final_context)
#[tokio::test]
async fn context_updates_propagate() {
    // The mock codergen handler sets context_updates with
    // "<node_id>.completed", "<node_id>.result", etc.
    let graph = parse_graph(
        r#"digraph G {
            node [llm_provider="claude"]
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
    // Framework routing state is typed and is not persisted as workflow data.
    assert!(!result.final_context.contains_key("outcome"));
    assert!(!result.final_context.contains_key("preferred_label"));
}

// Test 5: Goal gate failure with retry target loops back
#[tokio::test]
async fn goal_gate_failure_with_retry_loops_back() {
    // The mock handler returns success, so goal gate is satisfied and no loop occurs.
    // Here we verify the goal gate path doesn't error when gates are satisfied.
    let graph = parse_graph(
        r#"digraph G {
            node [llm_provider="claude"]
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
    use crate::graph::PipelineNode;
    use crate::handler::NodeHandler;
    use async_trait::async_trait;

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
            node [llm_provider="claude"]
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
    use crate::graph::PipelineNode;
    use crate::handler::NodeHandler;
    use async_trait::async_trait;
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
            node [llm_provider="claude"]
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

#[tokio::test]
async fn node_max_retries_reinvokes_handler_until_success() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct RetryThenSuccess {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl NodeHandler for RetryThenSuccess {
        fn handler_type(&self) -> &str {
            "codergen"
        }

        async fn execute(
            &self,
            _node: &crate::PipelineNode,
            _ctx: &Context,
            _graph: &PipelineGraph,
        ) -> Result<Outcome> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(Outcome {
                    status: StageStatus::Retry,
                    preferred_label: None,
                    suggested_next_ids: vec![],
                    context_updates: HashMap::from([
                        ("work.cost_usd".into(), serde_json::json!(1.0)),
                        (
                            "intermediate.must_not_commit".into(),
                            serde_json::json!(true),
                        ),
                    ]),
                    notes: "retry".into(),
                    failure_reason: None,
                })
            } else {
                Ok(Outcome {
                    status: StageStatus::Success,
                    preferred_label: None,
                    suggested_next_ids: vec![],
                    context_updates: HashMap::from([
                        ("work.cost_usd".into(), serde_json::json!(2.0)),
                        ("final.committed".into(), serde_json::json!(true)),
                    ]),
                    notes: "recovered".into(),
                    failure_reason: None,
                })
            }
        }
    }

    let graph = parse_graph(
        r#"digraph G {
            node [llm_provider="claude"]
            start [shape="Mdiamond"]
            work [shape="box", prompt="work", max_retries=1]
            done [shape="Msquare"]
            start -> work -> done
        }"#,
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(ExitHandler);
    registry.register(RetryThenSuccess {
        calls: Arc::clone(&calls),
    });
    let emitter = EventEmitter::new(64);
    let mut receiver = emitter.subscribe();

    let result = PipelineExecutor::new(registry)
        .with_event_emitter(emitter)
        .run(&graph)
        .await
        .unwrap();

    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(result.node_outcomes["work"].status, StageStatus::Success);
    assert_eq!(result.total_cost, 3.0);
    assert!(!result
        .final_context
        .contains_key("intermediate.must_not_commit"));
    assert_eq!(result.final_context["final.committed"], true);
    let mut retry_attempts = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        if let PipelineEvent::StageRetrying { node_id, attempt } = event {
            if node_id == "work" {
                retry_attempts.push(attempt);
            }
        }
    }
    assert_eq!(retry_attempts, vec![2]);
}

#[tokio::test]
async fn fail_outcomes_are_not_retried() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct FailOnce(Arc<AtomicUsize>);

    #[async_trait]
    impl NodeHandler for FailOnce {
        fn handler_type(&self) -> &str {
            "codergen"
        }

        async fn execute(
            &self,
            _node: &crate::PipelineNode,
            _ctx: &Context,
            _graph: &PipelineGraph,
        ) -> Result<Outcome> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Outcome::fail("terminal stage result"))
        }
    }

    let graph = parse_graph(
        r#"digraph G {
            node [llm_provider="claude"]
            start [shape="Mdiamond"]
            work [shape="box", prompt="work", max_retries=3]
            done [shape="Msquare"]
            start -> work -> done
        }"#,
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(ExitHandler);
    registry.register(FailOnce(Arc::clone(&calls)));

    let result = PipelineExecutor::new(registry).run(&graph).await.unwrap();

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(result.node_outcomes["work"].status, StageStatus::Fail);
}

#[tokio::test]
async fn exit_handlers_use_the_same_retry_boundary() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct RetryThenExit(Arc<AtomicUsize>);

    #[async_trait]
    impl NodeHandler for RetryThenExit {
        fn handler_type(&self) -> &str {
            "exit"
        }

        async fn execute(
            &self,
            _node: &crate::PipelineNode,
            _ctx: &Context,
            _graph: &PipelineGraph,
        ) -> Result<Outcome> {
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(Outcome::with_label(StageStatus::Retry, "retry"))
            } else {
                Ok(Outcome::success("exited"))
            }
        }
    }

    let graph = parse_graph(
        r#"digraph G {
            start [shape="Mdiamond"]
            done [shape="Msquare", max_retries=1]
            start -> done
        }"#,
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(RetryThenExit(Arc::clone(&calls)));

    let result = PipelineExecutor::new(registry).run(&graph).await.unwrap();

    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(result.node_outcomes["done"].status, StageStatus::Success);
}

#[tokio::test]
async fn retry_attempts_consume_the_global_step_limit() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct AlwaysRetry(Arc<AtomicUsize>);

    #[async_trait]
    impl NodeHandler for AlwaysRetry {
        fn handler_type(&self) -> &str {
            "codergen"
        }

        async fn execute(
            &self,
            _node: &crate::PipelineNode,
            _ctx: &Context,
            _graph: &PipelineGraph,
        ) -> Result<Outcome> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Outcome::with_label(StageStatus::Retry, "retry"))
        }
    }

    let graph = parse_graph(
        r#"digraph G {
            node [llm_provider="claude"]
            start [shape="Mdiamond"]
            work [shape="box", prompt="work", max_retries=3]
            done [shape="Msquare"]
            start -> work -> done
        }"#,
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(ExitHandler);
    registry.register(AlwaysRetry(Arc::clone(&calls)));
    let context = Context::new();
    context.set("max_steps", serde_json::json!(2)).await;

    let error = PipelineExecutor::new(registry)
        .run_with_context(&graph, context)
        .await
        .unwrap_err();

    assert!(error.to_string().contains("maximum step count (2)"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn retry_attempts_are_checkpointed_before_handler_invocation() {
    struct AlwaysRateLimited;

    #[async_trait]
    impl NodeHandler for AlwaysRateLimited {
        fn handler_type(&self) -> &str {
            "codergen"
        }

        async fn execute(
            &self,
            _node: &crate::PipelineNode,
            _ctx: &Context,
            _graph: &PipelineGraph,
        ) -> Result<Outcome> {
            Err(AttractorError::RateLimited {
                provider: "test".into(),
                retry_after_ms: 0,
            })
        }
    }

    let graph = parse_graph(
        r#"digraph G {
            node [llm_provider="claude"]
            start [shape="Mdiamond"]
            work [shape="box", prompt="work", max_retries=1]
            done [shape="Msquare"]
            start -> work -> done
        }"#,
    );
    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(ExitHandler);
    registry.register(AlwaysRateLimited);
    let logs = tempfile::tempdir().unwrap();

    let error = PipelineExecutor::new(registry)
        .run_with_checkpoint(&graph, Context::new(), logs.path())
        .await
        .unwrap_err();
    assert!(matches!(error, AttractorError::RateLimited { .. }));

    let checkpoint: serde_json::Value = serde_json::from_str(
        &tokio::fs::read_to_string(logs.path().join("checkpoint.json"))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(checkpoint["current_node_id"], "work");
    assert_eq!(checkpoint["step_count"], 3);
    assert_eq!(checkpoint["total_handler_attempts"], 3);
    assert_eq!(checkpoint["active_node_id"], "work");
    assert_eq!(checkpoint["active_node_attempts"], 2);
}

#[tokio::test]
async fn resumed_node_does_not_regain_consumed_retry_attempts() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct AlwaysRateLimited(Arc<AtomicUsize>);

    #[async_trait]
    impl NodeHandler for AlwaysRateLimited {
        fn handler_type(&self) -> &str {
            "codergen"
        }

        async fn execute(
            &self,
            _node: &crate::PipelineNode,
            _ctx: &Context,
            _graph: &PipelineGraph,
        ) -> Result<Outcome> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(AttractorError::RateLimited {
                provider: "test".into(),
                retry_after_ms: 0,
            })
        }
    }

    let graph = parse_graph(
        r#"digraph G {
            node [llm_provider="claude"]
            start [shape="Mdiamond"]
            work [shape="box", prompt="work", max_retries=1]
            done [shape="Msquare"]
            start -> work -> done
        }"#,
    );
    let logs = tempfile::tempdir().unwrap();
    let mut checkpoint = PipelineCheckpoint::new(
        "work".into(),
        vec!["start".into()],
        HashMap::new(),
        HashMap::new(),
    );
    checkpoint.step_count = 2;
    checkpoint.total_handler_attempts = 2;
    checkpoint.active_node_id = Some("work".into());
    checkpoint.active_node_attempts = 1;
    save_checkpoint(&checkpoint, logs.path()).await.unwrap();

    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(ExitHandler);
    registry.register(AlwaysRateLimited(Arc::clone(&calls)));

    let error = PipelineExecutor::new(registry)
        .run_with_checkpoint(&graph, Context::new(), logs.path())
        .await
        .unwrap_err();

    assert!(matches!(error, AttractorError::RateLimited { .. }));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let restored = load_checkpoint(logs.path()).await.unwrap().unwrap();
    assert_eq!(restored.active_node_attempts, 2);
    assert_eq!(restored.total_handler_attempts, 3);
    assert_eq!(restored.step_count, 3);
}

#[tokio::test]
async fn node_timeout_applies_to_custom_handlers_and_is_retryable() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct SlowHandler(Arc<AtomicUsize>);

    #[async_trait]
    impl NodeHandler for SlowHandler {
        fn handler_type(&self) -> &str {
            "codergen"
        }

        async fn execute(
            &self,
            _node: &crate::PipelineNode,
            _ctx: &Context,
            _graph: &PipelineGraph,
        ) -> Result<Outcome> {
            self.0.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            Ok(Outcome::success("too late"))
        }
    }

    let graph = parse_graph(
        r#"digraph G {
            node [llm_provider="claude"]
            start [shape="Mdiamond"]
            work [shape="box", prompt="work", max_retries=1, timeout=1ms]
            done [shape="Msquare"]
            start -> work -> done
        }"#,
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(ExitHandler);
    registry.register(SlowHandler(Arc::clone(&calls)));

    let error = PipelineExecutor::new(registry)
        .run(&graph)
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        AttractorError::CommandTimeout { timeout_ms: 1 }
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[cfg(unix)]
#[tokio::test]
async fn canonical_tool_timeout_terminates_descendant_processes() {
    use std::time::Duration;

    let fixture = tempfile::tempdir().unwrap();
    let pid_file = fixture.path().join("descendant.pid");
    let command = format!(
        "sleep 30 & child=$!; printf %s $child > {}; wait",
        pid_file.display()
    );
    let graph = parse_graph(&format!(
        r#"digraph G {{
            start [shape="Mdiamond"]
            work [shape="parallelogram", tool_command="{command}", timeout=3000ms]
            done [shape="Msquare"]
            start -> work -> done
        }}"#
    ));

    let error = PipelineExecutor::with_default_registry()
        .run(&graph)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        AttractorError::CommandTimeout { timeout_ms: 3000 }
    ));

    let pid = std::fs::read_to_string(&pid_file)
        .unwrap()
        .parse::<libc::pid_t>()
        .unwrap();
    let mut alive = true;
    for _ in 0..100 {
        alive = unsafe { libc::kill(pid, 0) == 0 };
        if !alive {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    if alive {
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    assert!(!alive, "descendant process {pid} survived the node timeout");
}

#[tokio::test]
async fn executor_emits_pipeline_stage_context_and_edge_lifecycle() {
    use crate::PipelineEvent;

    let graph = parse_graph(
        r#"digraph G {
            node [llm_provider="claude"]
            start [shape="Mdiamond"]
            work [shape="box", prompt="work"]
            done [shape="Msquare"]
            start -> work -> done
        }"#,
    );
    let emitter = EventEmitter::new(64);
    let mut receiver = emitter.subscribe();

    test_executor()
        .with_event_emitter(emitter)
        .run(&graph)
        .await
        .unwrap();

    let mut event_kinds = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        event_kinds.push(match event {
            PipelineEvent::PipelineStarted { .. } => "pipeline_started",
            PipelineEvent::PipelineCompleted { .. } => "pipeline_completed",
            PipelineEvent::PipelineFailed { .. } => "pipeline_failed",
            PipelineEvent::StageStarted { .. } => "stage_started",
            PipelineEvent::StageCompleted { .. } => "stage_completed",
            PipelineEvent::StageFailed { .. } => "stage_failed",
            PipelineEvent::StageRetrying { .. } => "stage_retrying",
            PipelineEvent::EdgeSelected { .. } => "edge_selected",
            PipelineEvent::GoalGateChecked { .. } => "goal_gate_checked",
            PipelineEvent::CheckpointSaved { .. } => "checkpoint_saved",
            PipelineEvent::ContextUpdated { .. } => "context_updated",
            PipelineEvent::CommitsCreated { .. } => "commits_created",
            PipelineEvent::LlmInvoked { .. } => "llm_invoked",
            PipelineEvent::EpicSnapshot { .. } => "epic_snapshot",
            PipelineEvent::TaskClaimed { .. } => "task_claimed",
            PipelineEvent::TaskSelectionBlocked { .. } => "task_selection_blocked",
            PipelineEvent::TaskClosed { .. } => "task_closed",
        });
    }

    assert_eq!(event_kinds.first(), Some(&"pipeline_started"));
    assert_eq!(event_kinds.last(), Some(&"pipeline_completed"));
    assert_eq!(
        event_kinds
            .iter()
            .filter(|kind| **kind == "stage_started")
            .count(),
        3
    );
    assert_eq!(
        event_kinds
            .iter()
            .filter(|kind| **kind == "stage_completed")
            .count(),
        3
    );
    assert!(event_kinds.contains(&"context_updated"));
    assert_eq!(
        event_kinds
            .iter()
            .filter(|kind| **kind == "edge_selected")
            .count(),
        2
    );
    assert!(!event_kinds.contains(&"pipeline_failed"));
}

#[tokio::test]
async fn absent_or_lagging_event_subscribers_cannot_change_execution() {
    let graph = parse_graph(
        r#"digraph G {
            node [llm_provider="claude"]
            start [shape="Mdiamond"]
            work [shape="box", prompt="work"]
            done [shape="Msquare"]
            start -> work -> done
        }"#,
    );

    let result = test_executor()
        .with_event_emitter(EventEmitter::new(1))
        .run(&graph)
        .await
        .unwrap();

    assert_eq!(result.completed_nodes, vec!["start", "work", "done"]);

    let lagging_emitter = EventEmitter::new(1);
    let _lagging_receiver = lagging_emitter.subscribe();
    let lagged_result = test_executor()
        .with_event_emitter(lagging_emitter)
        .run(&graph)
        .await
        .unwrap();
    assert_eq!(lagged_result.completed_nodes, result.completed_nodes);
}

#[tokio::test]
async fn executor_emits_exactly_one_pipeline_failure_for_runtime_errors() {
    use crate::PipelineEvent;

    struct BrokenHandler;

    #[async_trait]
    impl NodeHandler for BrokenHandler {
        fn handler_type(&self) -> &str {
            "codergen"
        }

        async fn execute(
            &self,
            _node: &crate::PipelineNode,
            _ctx: &Context,
            _graph: &PipelineGraph,
        ) -> Result<Outcome> {
            Err(AttractorError::AuthError {
                provider: "test".into(),
            })
        }
    }

    let graph = parse_graph(
        r#"digraph G {
            node [llm_provider="claude"]
            start [shape="Mdiamond"]
            work [shape="box", prompt="work"]
            done [shape="Msquare"]
            start -> work -> done
        }"#,
    );
    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(ExitHandler);
    registry.register(BrokenHandler);
    let emitter = EventEmitter::new(64);
    let mut receiver = emitter.subscribe();

    PipelineExecutor::new(registry)
        .with_event_emitter(emitter)
        .run(&graph)
        .await
        .unwrap_err();

    let mut failures = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        if let PipelineEvent::PipelineFailed { error, .. } = event {
            failures.push(error);
        }
    }
    assert_eq!(failures.len(), 1);
    assert!(failures[0].contains("Authentication failed"));
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
            node [llm_provider="claude"]
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
            node [llm_provider="claude"]
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
            updates.insert(format!("{}.cost_usd", node.id), serde_json::json!(1.50));
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
            node [llm_provider="claude"]
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
    let err = result.unwrap_err();
    assert!(
        matches!(err, AttractorError::BudgetExhausted { .. }),
        "Expected a typed budget error, got: {err:?}"
    );
    let err = err.to_string();
    assert!(
        err.contains("exceeded budget"),
        "Expected budget error, got: {err}"
    );
}

// The step limit is a typed error so `pas run` can end the Attempt with
// reason `max_steps`; the message is unchanged.
#[tokio::test]
async fn step_limit_returns_max_steps_exceeded_error() {
    let graph = parse_graph(
        r#"digraph G {
            node [llm_provider="claude"]
            start [shape="Mdiamond"]
            a [shape="box", prompt="a"]
            b [shape="box", prompt="b"]
            done  [shape="Msquare"]
            start -> a -> b -> done
        }"#,
    );
    let context = Context::new();
    context.set("dry_run", serde_json::json!(true)).await;
    context.set("max_steps", serde_json::json!(1u64)).await;
    let err = PipelineExecutor::with_default_registry()
        .run_with_context(&graph, context)
        .await
        .unwrap_err();
    assert!(
        matches!(err, AttractorError::MaxStepsExceeded { max_steps: 1 }),
        "Expected a typed step-limit error, got: {err:?}"
    );
    assert_eq!(
        err.to_string(),
        "Pipeline exceeded maximum step count (1). Use --max-steps to increase."
    );
}

// Test 11: Step limit does not abort at the exact boundary
#[tokio::test]
async fn step_limit_exact_boundary_does_not_abort() {
    // start → done is exactly 2 steps; step_count reaches 2 and is checked as 2 > max_steps.
    // max_steps=2: 2 > 2 = false → pipeline succeeds.
    // Mutation (>= max_steps): 2 >= 2 = true → wrongly aborts.
    let graph = parse_graph(
        r#"digraph G {
            start [shape="Mdiamond"]
            done  [shape="Msquare"]
            start -> done
        }"#,
    );
    let context = Context::new();
    context.set("max_steps", serde_json::json!(2u64)).await;
    let result = test_executor().run_with_context(&graph, context).await;
    assert!(
        result.is_ok(),
        "2 steps with max_steps=2 should succeed, got: {:?}",
        result.unwrap_err()
    );
}

// Test 12: Budget limit does not abort when cost exactly equals cap
#[tokio::test]
async fn budget_limit_exact_equality_does_not_abort() {
    // A node reporting cost_usd equal to max_budget_usd should not abort.
    // total_cost > max_budget: 2.0 > 2.0 = false → succeeds.
    // Mutation (>= max_budget): 2.0 >= 2.0 = true → wrongly aborts.
    use crate::graph::PipelineNode;

    struct ExactCostHandler;

    #[async_trait]
    impl NodeHandler for ExactCostHandler {
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
            updates.insert(format!("{}.cost_usd", node.id), serde_json::json!(2.0f64));
            Ok(Outcome {
                status: StageStatus::Success,
                preferred_label: None,
                suggested_next_ids: vec![],
                context_updates: updates,
                notes: "exact cost".into(),
                failure_reason: None,
            })
        }
    }

    let graph = parse_graph(
        r#"digraph G {
            node [llm_provider="claude"]
            start [shape="Mdiamond"]
            step  [shape="box", label="Step", prompt="work"]
            done  [shape="Msquare"]
            start -> step -> done
        }"#,
    );

    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(ExitHandler);
    registry.register(ConditionalHandler);
    registry.register(ExactCostHandler);

    let context = Context::new();
    context
        .set("max_budget_usd", serde_json::json!(2.0f64))
        .await;

    let result = PipelineExecutor::new(registry)
        .run_with_context(&graph, context)
        .await;
    assert!(
        result.is_ok(),
        "cost equal to budget should not abort, got: {:?}",
        result.unwrap_err()
    );
}

// Test 13: Quality loop aborts after max_fix_iterations, not before
#[tokio::test]
async fn quality_loop_fires_at_iteration_beyond_max_fix_iterations() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct AlwaysFailQualityHandler {
        call_count: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl NodeHandler for AlwaysFailQualityHandler {
        fn handler_type(&self) -> &str {
            "quality"
        }
        async fn execute(
            &self,
            _node: &crate::graph::PipelineNode,
            _ctx: &Context,
            _graph: &PipelineGraph,
        ) -> Result<Outcome> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok(Outcome::fail("always fails"))
        }
    }

    // fix → verify(fail) → fix(loop_restart) repeats; each re-entry of verify from fix
    // increments the same loop key "verify::fix".
    let graph = parse_graph(
        r#"digraph G {
            node   [llm_provider="claude"]
            start  [shape="Mdiamond"]
            fix    [shape="box", label="Fix", prompt="fix"]
            verify [shape="box", type="quality", label="Verify", prompt="verify"]
            done   [shape="Msquare"]
            start -> fix -> verify
            verify -> done [condition="outcome=success"]
            verify -> fix  [condition="outcome=fail", loop_restart=true]
        }"#,
    );

    let call_count = Arc::new(AtomicUsize::new(0));
    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(ExitHandler);
    registry.register(ConditionalHandler);
    registry.register(MockCodergenHandler); // handles "fix" node
    registry.register(AlwaysFailQualityHandler {
        call_count: call_count.clone(),
    });

    let context = Context::new();
    // max_fix_iterations=1: iteration 1 runs handler; iteration 2 aborts before running it.
    // Mutation (>= 1): iteration 1 aborts immediately → handler never called.
    context
        .set("quality_max_fix_iterations", serde_json::json!(1u64))
        .await;

    let result = PipelineExecutor::new(registry)
        .run_with_context(&graph, context)
        .await;

    assert!(
        result.is_err(),
        "should abort after exceeding max_fix_iterations"
    );
    assert_eq!(
        call_count.load(Ordering::SeqCst),
        1,
        "handler should execute exactly once (iter=1 runs, iter=2 aborts before executing)"
    );
}

// Test 14: Retry warning injected when quality node re-enters on iteration 2
// Note: the engine sleeps 1 second at iteration >= 2; this test takes ~1s.
#[tokio::test]
async fn quality_retry_warning_remains_engine_owned_on_second_iteration() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct FailOnceThenSucceedQualityHandler {
        call_count: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl NodeHandler for FailOnceThenSucceedQualityHandler {
        fn handler_type(&self) -> &str {
            "quality"
        }
        async fn execute(
            &self,
            _node: &crate::graph::PipelineNode,
            _ctx: &Context,
            _graph: &PipelineGraph,
        ) -> Result<Outcome> {
            let prev = self.call_count.fetch_add(1, Ordering::SeqCst);
            if prev == 0 {
                Ok(Outcome::fail("first attempt fails"))
            } else {
                Ok(Outcome::success("retry succeeded"))
            }
        }
    }

    let graph = parse_graph(
        r#"digraph G {
            node   [llm_provider="claude"]
            start  [shape="Mdiamond"]
            fix    [shape="box", label="Fix", prompt="fix"]
            verify [shape="box", type="quality", label="Verify", prompt="verify"]
            done   [shape="Msquare"]
            start -> fix -> verify
            verify -> done [condition="outcome=success"]
            verify -> fix  [condition="outcome=fail", loop_restart=true]
        }"#,
    );

    let call_count = Arc::new(AtomicUsize::new(0));
    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(ExitHandler);
    registry.register(ConditionalHandler);
    registry.register(MockCodergenHandler);
    registry.register(FailOnceThenSucceedQualityHandler {
        call_count: call_count.clone(),
    });

    let context = Context::new();
    // Allow 2 iterations so iter=2 passes the abort check and injects the warning.
    context
        .set("quality_max_fix_iterations", serde_json::json!(2u64))
        .await;

    let result = PipelineExecutor::new(registry)
        .run_with_context(&graph, context)
        .await
        .expect("pipeline should succeed on second quality attempt");

    assert!(
        !result
            .final_context
            .contains_key("__quality_retry_warning::verify"),
        "framework retry state must not leak into workflow Context"
    );
}

// Test 15: PipelineResult.total_cost sums every node execution, including
// ones re-run after a loop_restart clears completed_nodes/node_outcomes.
// Regression for the CLI's old approach of re-summing `<node>.cost_usd`
// keys out of final_context, which is last-write-wins per node id and so
// only ever counted the final loop iteration's cost.
#[tokio::test]
async fn total_cost_accumulates_across_loop_restart_iterations() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct CostingFixHandler {
        call_count: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl NodeHandler for CostingFixHandler {
        fn handler_type(&self) -> &str {
            "codergen"
        }
        async fn execute(
            &self,
            node: &crate::graph::PipelineNode,
            _ctx: &Context,
            _graph: &PipelineGraph,
        ) -> Result<Outcome> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            let mut updates = HashMap::new();
            updates.insert(format!("{}.cost_usd", node.id), serde_json::json!(1.0));
            Ok(Outcome {
                context_updates: updates,
                ..Outcome::success("fixed")
            })
        }
    }

    struct FailTwiceThenSucceedQualityHandler {
        call_count: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl NodeHandler for FailTwiceThenSucceedQualityHandler {
        fn handler_type(&self) -> &str {
            "quality"
        }
        async fn execute(
            &self,
            _node: &crate::graph::PipelineNode,
            _ctx: &Context,
            _graph: &PipelineGraph,
        ) -> Result<Outcome> {
            let prev = self.call_count.fetch_add(1, Ordering::SeqCst);
            if prev < 2 {
                Ok(Outcome::fail("not fixed yet"))
            } else {
                Ok(Outcome::success("fixed on third attempt"))
            }
        }
    }

    let graph = parse_graph(
        r#"digraph G {
            node   [llm_provider="claude"]
            start  [shape="Mdiamond"]
            fix    [shape="box", label="Fix", prompt="fix"]
            verify [shape="box", type="quality", label="Verify", prompt="verify"]
            done   [shape="Msquare"]
            start -> fix -> verify
            verify -> done [condition="outcome=success"]
            verify -> fix  [condition="outcome=fail", loop_restart=true]
        }"#,
    );

    let fix_calls = Arc::new(AtomicUsize::new(0));
    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(ExitHandler);
    registry.register(ConditionalHandler);
    registry.register(CostingFixHandler {
        call_count: fix_calls.clone(),
    });
    registry.register(FailTwiceThenSucceedQualityHandler {
        call_count: Arc::new(AtomicUsize::new(0)),
    });

    let context = Context::new();
    // Allow 3 iterations so the fix->verify loop runs three times before succeeding.
    context
        .set("quality_max_fix_iterations", serde_json::json!(3u64))
        .await;

    let result = PipelineExecutor::new(registry)
        .run_with_context(&graph, context)
        .await
        .expect("pipeline should succeed on third quality attempt");

    assert_eq!(
        fix_calls.load(Ordering::SeqCst),
        3,
        "fix handler should run once per loop iteration"
    );
    assert_eq!(
        result.total_cost, 3.0,
        "total_cost must sum every loop iteration's cost, not just the last"
    );

    // The bug this guards against: summing `.cost_usd` keys out of
    // final_context (last-write-wins per node id) only ever sees the last
    // iteration's cost for the "fix" node.
    let final_context_sum: f64 = result
        .final_context
        .iter()
        .filter(|(k, _)| k.ends_with(".cost_usd"))
        .filter_map(|(_, v)| v.as_f64())
        .sum();
    assert_eq!(
        final_context_sum, 1.0,
        "final_context re-summing undercounts to a single iteration's cost — \
         confirms total_cost is the field that must be used, not final_context"
    );
}

// Test 15: Handler returning Fail with no outgoing edge returns HandlerError
#[tokio::test]
async fn fail_handler_with_no_outgoing_edge_returns_handler_error() {
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
            Ok(Outcome::fail("dead end failure"))
        }
    }

    // dead_end has zero outgoing edges (None branch fires when outcome=Fail).
    // done is reachable via an impossible condition so validation passes.
    let graph = parse_graph(
        r#"digraph G {
            node     [llm_provider="claude"]
            start    [shape="Mdiamond"]
            dead_end [shape="box", label="Dead End", prompt="will fail"]
            done     [shape="Msquare"]
            start -> dead_end
            start -> done [condition="__never__=true"]
        }"#,
    );

    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(ExitHandler);
    registry.register(ConditionalHandler);
    registry.register(FailHandler);

    let result = PipelineExecutor::new(registry).run(&graph).await;
    assert!(result.is_err(), "fail with no outgoing edge should error");
    match result.unwrap_err() {
        AttractorError::HandlerError { message, .. } => {
            assert!(
                message.contains("no outgoing edge"),
                "expected 'no outgoing edge' in error, got: {message}"
            );
        }
        other => panic!("expected HandlerError, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Quality work-cycle boundary + checkpoint hardening tests (attractor-1cc.7)
// ---------------------------------------------------------------------------

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Quality handler with scripted per-call outcomes, consumed in order.
struct ScriptedQuality {
    outcomes: Vec<StageStatus>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl NodeHandler for ScriptedQuality {
    fn handler_type(&self) -> &str {
        "quality"
    }

    async fn execute(
        &self,
        _node: &crate::graph::PipelineNode,
        _ctx: &Context,
        _graph: &PipelineGraph,
    ) -> Result<Outcome> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        match self.outcomes.get(call) {
            Some(StageStatus::Fail) => Ok(Outcome::fail("objective gate failed")),
            _ => Ok(Outcome::success("objective gates passed")),
        }
    }
}

/// Quality handler that always fails (drives abort-at-ceiling tests).
struct AlwaysFailQuality {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl NodeHandler for AlwaysFailQuality {
    fn handler_type(&self) -> &str {
        "quality"
    }

    async fn execute(
        &self,
        _node: &crate::graph::PipelineNode,
        _ctx: &Context,
        _graph: &PipelineGraph,
    ) -> Result<Outcome> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Outcome::fail("never heals"))
    }
}

/// Workflow handler with scripted per-node outcomes. Each node's scripted
/// results are consumed in order; when exhausted the node succeeds.
struct ScriptedWorkflow {
    scripts: HashMap<String, Vec<StageStatus>>,
    calls: HashMap<String, Arc<AtomicUsize>>,
}

impl ScriptedWorkflow {
    fn new() -> Self {
        Self {
            scripts: HashMap::new(),
            calls: HashMap::new(),
        }
    }

    fn script(&mut self, node: &str, outcomes: Vec<StageStatus>) -> Arc<AtomicUsize> {
        let counter = Arc::new(AtomicUsize::new(0));
        self.scripts.insert(node.to_string(), outcomes);
        self.calls.insert(node.to_string(), Arc::clone(&counter));
        counter
    }
}

#[async_trait]
impl NodeHandler for ScriptedWorkflow {
    fn handler_type(&self) -> &str {
        "codergen"
    }

    async fn execute(
        &self,
        node: &crate::graph::PipelineNode,
        _ctx: &Context,
        _graph: &PipelineGraph,
    ) -> Result<Outcome> {
        let call = self
            .calls
            .get(&node.id)
            .expect("scripted node registered")
            .fetch_add(1, Ordering::SeqCst);
        match self.scripts.get(&node.id).and_then(|s| s.get(call)) {
            Some(StageStatus::Fail) => Ok(Outcome::fail("scripted fail")),
            _ => Ok(Outcome::success("scripted")),
        }
    }
}

/// Two-task epic-runner shape with `max_fix_iterations=1` on the quality
/// node. `boundary_attr` is appended to the outer `next -> implement` edge
/// ("" or ", reset_quality_loop_state=true").
fn two_cycle_graph(boundary_attr: &str) -> PipelineGraph {
    parse_graph(&format!(
        r#"digraph G {{
            node      [llm_provider="claude"]
            start     [shape="Mdiamond"]
            implement [shape="box", prompt="implement"]
            quality   [shape="box", type="quality", max_fix_iterations=1]
            review    [shape="diamond", prompt="review"]
            fixup     [shape="box", prompt="fixup"]
            next      [shape="diamond", prompt="next"]
            done      [shape="Msquare"]

            start -> implement -> quality
            quality -> review [condition="outcome=success"]
            quality -> fixup  [condition="outcome=fail"]
            review -> next  [condition="outcome=success"]
            review -> fixup [condition="outcome=fail"]
            fixup -> quality [loop_restart=true]
            next -> implement [loop_restart=true{boundary_attr}]
            next -> done [condition="outcome=fail"]
        }}"#
    ))
}

fn two_cycle_registry(workflow: ScriptedWorkflow, quality: ScriptedQuality) -> HandlerRegistry {
    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(ExitHandler);
    registry.register(ConditionalHandler);
    registry.register(workflow);
    registry.register(quality);
    registry
}

/// Scripted outcomes for one full two-task run where each task's quality
/// gate fails once, is fixed, then passes review.
fn two_cycle_workflow() -> ScriptedWorkflow {
    let mut workflow = ScriptedWorkflow::new();
    workflow.script(
        "implement",
        vec![StageStatus::Success, StageStatus::Success],
    );
    workflow.script("review", vec![StageStatus::Success, StageStatus::Success]);
    workflow.script("fixup", vec![StageStatus::Success, StageStatus::Success]);
    workflow.script("next", vec![StageStatus::Success, StageStatus::Fail]);
    workflow
}

// An exhausted cycle's consumed retry budget must not leak into the next
// outer work cycle. With the explicit boundary, the second task's quality
// node starts at iteration 1 again even though the first task consumed its
// whole budget.
#[tokio::test]
async fn work_cycle_boundary_gives_each_task_a_fresh_quality_budget() {
    let graph = two_cycle_graph(", reset_quality_loop_state=true");
    let workflow = two_cycle_workflow();
    let quality = ScriptedQuality {
        outcomes: vec![
            StageStatus::Fail,    // cycle 1: quality fails -> fixup
            StageStatus::Success, // cycle 1: fixed -> review
            StageStatus::Fail,    // cycle 2: quality fails -> fixup
            StageStatus::Success, // cycle 2: fixed -> review
        ],
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let registry = two_cycle_registry(workflow, quality);

    let result = PipelineExecutor::new(registry).run(&graph).await;
    assert!(
        result.is_ok(),
        "second work cycle must receive a fresh retry budget: {:?}",
        result.err()
    );
}

// Without the boundary attribute the old bug reproduces: the second task
// re-enters quality from the same upstream node with the first task's
// consumed counter and aborts at the ceiling.
#[tokio::test]
async fn without_work_cycle_boundary_second_task_inherits_consumed_budget() {
    let graph = two_cycle_graph("");
    let workflow = two_cycle_workflow();
    let quality = ScriptedQuality {
        outcomes: vec![
            StageStatus::Fail,
            StageStatus::Success,
            StageStatus::Fail, // cycle 2's first entry exceeds the budget
        ],
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let registry = two_cycle_registry(workflow, quality);

    let error = PipelineExecutor::new(registry)
        .run(&graph)
        .await
        .expect_err("stale counter from cycle 1 must abort cycle 2 without a boundary");
    assert!(
        error.to_string().contains("exceeded max_fix_iterations"),
        "expected quality ceiling abort, got: {error}"
    );
}

// The boundary reset must be persisted in the checkpoint saved *before* the
// next cycle's nodes run. The run is interrupted by the step limit right at
// cycle 2's first node; the persisted checkpoint must already carry the
// cleared quality-loop state and the plan fingerprint.
#[tokio::test]
async fn boundary_reset_is_checkpointed_and_survives_resume() {
    let graph = two_cycle_graph(", reset_quality_loop_state=true");
    let logs = tempfile::tempdir().unwrap();

    let workflow = two_cycle_workflow();
    let quality = ScriptedQuality {
        outcomes: vec![
            StageStatus::Fail,
            StageStatus::Success,
            StageStatus::Fail,
            StageStatus::Success,
        ],
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let registry = two_cycle_registry(workflow, quality);
    let context = Context::new();
    // Execution order: start(1) implement(2) quality(3) fixup(4) quality(5)
    // review(6) next(7) -> boundary edge checkpoint saved -> implement(8)
    // -> step-limit check fails on attempt 9. The last persisted checkpoint
    // is the boundary-edge save with cleared quality-loop state.
    context.set("max_steps", serde_json::json!(8u64)).await;

    let first = PipelineExecutor::new(registry)
        .run_with_checkpoint(&graph, context, logs.path())
        .await;
    assert!(
        first.is_err(),
        "step limit should interrupt cycle 2, got: {first:?}"
    );

    let cp = load_checkpoint(logs.path()).await.unwrap().unwrap();
    assert!(
        cp.quality_loop_counters.is_empty(),
        "boundary reset must be persisted, found: {:?}",
        cp.quality_loop_counters
    );
    assert!(
        cp.quality_last_footprint.is_empty(),
        "boundary reset must clear footprints too"
    );
    assert!(
        cp.execution_fingerprint.is_some(),
        "checkpoints must record the plan fingerprint"
    );
    // The interruption lands on cycle 2's quality invocation; the last
    // persisted checkpoint is the edge save into `quality`, which must
    // still carry the boundary-cleared loop state.
    assert_eq!(cp.current_node_id, "quality");
}

// Resume inside an *active* retry episode must preserve the consumed
// budget: the counter restored from the checkpoint still counts.
#[tokio::test]
async fn resume_inside_active_fixup_preserves_consumed_budget() {
    let graph = parse_graph(
        r#"digraph G {
            node   [llm_provider="claude"]
            start  [shape="Mdiamond"]
            fix    [shape="box", prompt="fix"]
            verify [shape="box", type="quality", max_fix_iterations=2]
            done   [shape="Msquare"]
            start -> fix -> verify
            verify -> done [condition="outcome=success"]
            verify -> fix  [condition="outcome=fail", loop_restart=true]
        }"#,
    );
    let logs = tempfile::tempdir().unwrap();

    let quality_calls = Arc::new(AtomicUsize::new(0));
    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(ExitHandler);
    registry.register(ConditionalHandler);
    registry.register(MockCodergenHandler); // handles `fix`
    registry.register(AlwaysFailQuality {
        calls: Arc::clone(&quality_calls),
    });

    // Simulate an interruption mid-retry: quality already consumed one
    // attempt from the fix->verify edge and is about to run again. The
    // fingerprint must come from the same compile path the engine uses
    // (compile_with_registry with this registry).
    let mut checkpoint = PipelineCheckpoint::new(
        "verify".into(),
        vec!["start".into(), "fix".into()],
        HashMap::new(),
        HashMap::new(),
    );
    checkpoint.previous_node_id = Some("fix".into());
    checkpoint
        .quality_loop_counters
        .insert("verify::fix".into(), 1);
    checkpoint.schema_version = crate::checkpoint::CHECKPOINT_SCHEMA_VERSION;
    let plan =
        crate::execution_plan::ExecutionPlan::compile_with_registry(graph.clone(), &registry)
            .unwrap();
    checkpoint.execution_fingerprint = Some(plan.fingerprint());
    save_checkpoint(&checkpoint, logs.path()).await.unwrap();

    let error = PipelineExecutor::new(registry)
        .run_with_checkpoint(&graph, Context::new(), logs.path())
        .await
        .expect_err("restored budget must still enforce the ceiling");
    assert!(
        error.to_string().contains("exceeded max_fix_iterations"),
        "expected ceiling abort, got: {error}"
    );
    assert_eq!(
        quality_calls.load(Ordering::SeqCst),
        1,
        "resumed counter=1 -> iteration 2 runs the handler once, iteration 3 aborts"
    );
}

// A checkpoint whose recorded fingerprint does not match the current plan
// must be rejected before any state is restored.
#[tokio::test]
async fn fingerprint_mismatch_rejects_resume() {
    let graph = parse_graph(
        r#"digraph G {
            node [llm_provider="claude"]
            start [shape="Mdiamond"]
            work [shape="box", prompt="work"]
            done [shape="Msquare"]
            start -> work -> done
        }"#,
    );
    let logs = tempfile::tempdir().unwrap();
    let mut checkpoint = PipelineCheckpoint::new(
        "work".into(),
        vec!["start".into()],
        HashMap::new(),
        HashMap::new(),
    );
    checkpoint.execution_fingerprint = Some("deadbeef".into());
    save_checkpoint(&checkpoint, logs.path()).await.unwrap();

    let error = PipelineExecutor::new(test_registry())
        .run_with_checkpoint(&graph, Context::new(), logs.path())
        .await
        .expect_err("mismatched fingerprint must fail closed");
    match error {
        AttractorError::CheckpointIncompatible { reason, .. } => {
            assert!(reason.contains("fingerprint"), "{reason}");
        }
        other => panic!("expected CheckpointIncompatible, got: {other:?}"),
    }
}

// Legacy checkpoints without a fingerprint resume (with a warning) rather
// than blocking every pre-fingerprint run.
#[tokio::test]
async fn legacy_checkpoint_without_fingerprint_still_resumes() {
    let graph = parse_graph(
        r#"digraph G {
            node [llm_provider="claude"]
            start [shape="Mdiamond"]
            work [shape="box", prompt="work"]
            done [shape="Msquare"]
            start -> work -> done
        }"#,
    );
    let logs = tempfile::tempdir().unwrap();
    // Schema version 1, no fingerprint field at all.
    let json = r#"{
        "current_node_id": "work",
        "completed_nodes": ["start"],
        "node_outcomes": {},
        "context_snapshot": {},
        "timestamp": "2026-09-03T00:00:00Z",
        "schema_version": 1
    }"#;
    tokio::fs::write(logs.path().join("checkpoint.json"), json)
        .await
        .unwrap();

    let result = PipelineExecutor::new(test_registry())
        .run_with_checkpoint(&graph, Context::new(), logs.path())
        .await;
    assert!(result.is_ok(), "legacy resume should proceed: {result:?}");
}

// A checkpoint written by a newer schema must be rejected, not guessed at.
#[tokio::test]
async fn future_schema_version_rejects_resume() {
    let graph = parse_graph(
        r#"digraph G {
            node [llm_provider="claude"]
            start [shape="Mdiamond"]
            work [shape="box", prompt="work"]
            done [shape="Msquare"]
            start -> work -> done
        }"#,
    );
    let logs = tempfile::tempdir().unwrap();
    let json = r#"{
        "current_node_id": "work",
        "completed_nodes": ["start"],
        "node_outcomes": {},
        "context_snapshot": {},
        "timestamp": "2026-09-03T00:00:00Z",
        "schema_version": 99
    }"#;
    tokio::fs::write(logs.path().join("checkpoint.json"), json)
        .await
        .unwrap();

    let error = PipelineExecutor::new(test_registry())
        .run_with_checkpoint(&graph, Context::new(), logs.path())
        .await
        .expect_err("future schema must fail closed");
    match error {
        AttractorError::CheckpointIncompatible { reason, .. } => {
            assert!(reason.contains("newer PAS"), "{reason}");
        }
        other => panic!("expected CheckpointIncompatible, got: {other:?}"),
    }
}

// Resuming must carry the checkpoint's run_id through the engine's own
// re-saves (it rewrites checkpoint.json before every attempt), so the
// resumed Run keeps its identity.
#[tokio::test]
async fn resume_preserves_checkpoint_run_id() {
    use std::sync::{Arc, Mutex};

    struct RunIdProbe {
        logs: std::path::PathBuf,
        seen: Arc<Mutex<Vec<Option<String>>>>,
    }

    #[async_trait]
    impl NodeHandler for RunIdProbe {
        fn handler_type(&self) -> &str {
            "run_id_probe"
        }
        async fn execute(
            &self,
            _node: &crate::graph::PipelineNode,
            _ctx: &Context,
            _graph: &PipelineGraph,
        ) -> Result<Outcome> {
            let cp = load_checkpoint(&self.logs)
                .await?
                .expect("checkpoint saved before attempt");
            self.seen.lock().unwrap().push(cp.run_id);
            Ok(Outcome::success("probed"))
        }
    }

    const RUN_ID: &str = "0192f3c4-5a6b-7c8d-9e0f-1a2b3c4d5e6f";
    let graph = parse_graph(
        r#"digraph G {
            start [shape="Mdiamond"]
            probe [type="run_id_probe"]
            done  [shape="Msquare"]
            start -> probe -> done
        }"#,
    );
    let logs = tempfile::tempdir().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut registry = test_registry();
    registry.register(RunIdProbe {
        logs: logs.path().to_path_buf(),
        seen: Arc::clone(&seen),
    });

    let plan =
        crate::execution_plan::ExecutionPlan::compile_with_registry(graph.clone(), &registry)
            .unwrap();
    let mut checkpoint = PipelineCheckpoint::new(
        "probe".into(),
        vec!["start".into()],
        HashMap::new(),
        HashMap::new(),
    );
    checkpoint.previous_node_id = Some("start".into());
    checkpoint.execution_fingerprint = Some(plan.fingerprint());
    checkpoint.run_id = Some(RUN_ID.into());
    save_checkpoint(&checkpoint, logs.path()).await.unwrap();

    PipelineExecutor::new(registry)
        .run_with_checkpoint(&graph, Context::new(), logs.path())
        .await
        .expect("resume should complete");

    assert_eq!(*seen.lock().unwrap(), vec![Some(RUN_ID.to_string())]);
    assert!(
        load_checkpoint(logs.path()).await.unwrap().is_none(),
        "checkpoint is still cleared after a successful Run"
    );
}

// Atomic persistence: a saved checkpoint leaves no stray temp file and
// round-trips; a second save cleanly replaces the first.
#[tokio::test]
async fn checkpoint_save_is_atomic_and_leaves_no_temp_file() {
    let dir = tempfile::tempdir().unwrap();
    let cp = PipelineCheckpoint::new("b".into(), vec!["a".into()], HashMap::new(), HashMap::new());
    save_checkpoint(&cp, dir.path()).await.unwrap();
    save_checkpoint(&cp, dir.path()).await.unwrap();

    assert!(!dir.path().join("checkpoint.json.tmp").exists());
    let loaded = load_checkpoint(dir.path()).await.unwrap().unwrap();
    assert_eq!(loaded.current_node_id, "b");
    assert_eq!(
        loaded.schema_version,
        crate::checkpoint::CHECKPOINT_SCHEMA_VERSION
    );
}

// The retry warning must not claim an objective quality failure when the
// re-entry was driven by a downstream review/fixup cycle (empty footprint).
#[test]
fn retry_warning_reason_distinguishes_quality_failure_from_review_fixup() {
    let after_failure = super::retry_warning_reason("a3f9c2b14e8d7012");
    assert!(after_failure.contains("Quality stage failed"));

    let after_review_fixup = super::retry_warning_reason("");
    assert!(
        after_review_fixup.contains("re-entered"),
        "{after_review_fixup}"
    );
    assert!(!after_review_fixup.contains("Quality stage failed"));
}

// ---------------------------------------------------------------------------
// Run Journal tests (attractor-ino.3 [T1-3])
// ---------------------------------------------------------------------------

use attractor_journal::{EventData, JournalWriter, EVENTS_FILE};

const JOURNAL_RUN_ID: &str = "0192f3c4-5a6b-7c8d-9e0f-1a2b3c4d5e6f";

/// A dry-run Pipeline with 3 stages between start and exit.
fn three_stage_graph() -> PipelineGraph {
    parse_graph(
        r#"digraph G {
            node [llm_provider="claude"]
            start     [shape="Mdiamond"]
            plan      [shape="box", prompt="plan"]
            implement [shape="box", prompt="implement"]
            review    [shape="box", prompt="review"]
            done      [shape="Msquare"]
            start -> plan -> implement -> review -> done
        }"#,
    )
}

async fn dry_run_context(workdir: &Path) -> Context {
    let context = Context::new();
    context.set("dry_run", serde_json::Value::Bool(true)).await;
    context
        .set(
            "workdir",
            serde_json::Value::String(workdir.display().to_string()),
        )
        .await;
    context
}

fn drain_types(receiver: &mut tokio::sync::broadcast::Receiver<PipelineEvent>) -> Vec<String> {
    let mut types = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        types.push(event.to_journal_data().type_name().to_string());
    }
    types
}

fn journal_types(path: &Path) -> Vec<String> {
    attractor_journal::read_all(path)
        .unwrap()
        .into_iter()
        .map(|event| event.data.type_name().to_string())
        .collect()
}

/// A journal whose appends fail on demand, and which can record how many
/// broadcast Events were already queued when each append ran.
struct ScriptedSink {
    path: PathBuf,
    fails: Box<dyn Fn(usize) -> bool + Send + Sync>,
    calls: AtomicUsize,
    written: std::sync::Mutex<Vec<EventData>>,
    probe: Option<std::sync::Mutex<tokio::sync::broadcast::Receiver<PipelineEvent>>>,
    queued_at_append: std::sync::Mutex<Vec<usize>>,
}

impl ScriptedSink {
    /// `fails(n)` decides whether the n-th append (1-based) fails.
    fn new(fails: impl Fn(usize) -> bool + Send + Sync + 'static) -> Self {
        Self {
            path: PathBuf::from("/scripted/runs/r/events.jsonl"),
            fails: Box::new(fails),
            calls: AtomicUsize::new(0),
            written: std::sync::Mutex::new(Vec::new()),
            probe: None,
            queued_at_append: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn written_types(&self) -> Vec<String> {
        self.written
            .lock()
            .unwrap()
            .iter()
            .map(|data| data.type_name().to_string())
            .collect()
    }
}

impl JournalSink for ScriptedSink {
    fn append(&self, data: EventData) -> std::io::Result<()> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some(probe) = &self.probe {
            self.queued_at_append
                .lock()
                .unwrap()
                .push(probe.lock().unwrap().len());
        }
        if (self.fails)(n) {
            return Err(std::io::Error::other("disk full"));
        }
        self.written.lock().unwrap().push(data);
        Ok(())
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

/// Captures `tracing` output for the current thread.
#[derive(Clone, Default)]
struct CapturedLog(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for CapturedLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl CapturedLog {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

thread_local! {
    static LOG_CAPTURE: std::cell::RefCell<Option<CapturedLog>> =
        const { std::cell::RefCell::new(None) };
}

/// Writes to the calling thread's capture buffer, if any.
struct ThreadLog;

impl std::io::Write for ThreadLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        LOG_CAPTURE.with(|capture| {
            if let Some(log) = capture.borrow_mut().as_mut() {
                log.write_all(buf)?;
            }
            Ok(buf.len())
        })
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Capture `tracing` output emitted on the current thread.
///
/// A global subscriber is used instead of `set_default`: with a single scoped
/// dispatcher, tracing-core caches a callsite's interest from whichever
/// thread first hits it, so a parallel test hitting the same callsite with no
/// subscriber would silence it here.
fn capture_log() -> CapturedLog {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(|| ThreadLog)
            .with_ansi(false)
            .finish();
        tracing::subscriber::set_global_default(subscriber)
            .expect("no other global subscriber in this test binary");
    });
    let log = CapturedLog::default();
    LOG_CAPTURE.with(|capture| *capture.borrow_mut() = Some(log.clone()));
    log
}

// AC1: every broadcast Event is in events.jsonl with the same type, in the
// same order (including CheckpointSaved, which is saved outside `emit`).
#[tokio::test]
async fn journal_records_every_broadcast_event_in_order() {
    let tmp = tempfile::tempdir().unwrap();
    let run_dir = tmp.path().join("runs").join(JOURNAL_RUN_ID);
    let journal = JournalWriter::open(&run_dir, JOURNAL_RUN_ID, 1).unwrap();
    let emitter = EventEmitter::new(256);
    let mut receiver = emitter.subscribe();

    let result = PipelineExecutor::with_default_registry()
        .with_event_emitter(emitter)
        .with_journal(journal)
        .run_with_checkpoint(
            &three_stage_graph(),
            dry_run_context(tmp.path()).await,
            &tmp.path().join("logs"),
        )
        .await
        .unwrap();
    assert_eq!(
        result.completed_nodes,
        vec!["start", "plan", "implement", "review", "done"]
    );

    let broadcast = drain_types(&mut receiver);
    let journal_path = run_dir.join(EVENTS_FILE);
    assert_eq!(journal_types(&journal_path), broadcast);
    assert_eq!(
        broadcast.first().map(String::as_str),
        Some("PipelineStarted")
    );
    assert_eq!(
        broadcast.last().map(String::as_str),
        Some("PipelineCompleted")
    );
    assert_eq!(broadcast.iter().filter(|t| *t == "StageStarted").count(), 5);
    assert!(broadcast.iter().any(|t| t == "CheckpointSaved"));

    let journal = attractor_journal::read_all(&journal_path).unwrap();
    for (index, event) in journal.iter().enumerate() {
        assert_eq!(event.seq, index as u64 + 1);
        assert_eq!(event.run_id, JOURNAL_RUN_ID);
        assert_eq!(event.attempt, 1);
    }
}

// AC2: a subscriber that reads the journal as soon as it receives an Event
// always finds that Event already on disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscriber_finds_event_in_journal_on_receipt() {
    let tmp = tempfile::tempdir().unwrap();
    let run_dir = tmp.path().join("runs").join(JOURNAL_RUN_ID);
    let journal = JournalWriter::open(&run_dir, JOURNAL_RUN_ID, 1).unwrap();
    let journal_path = run_dir.join(EVENTS_FILE);
    let emitter = EventEmitter::new(256);
    let mut receiver = emitter.subscribe();

    let subscriber = tokio::spawn({
        let journal_path = journal_path.clone();
        async move {
            let mut received = 0usize;
            loop {
                let event = receiver.recv().await.expect("no lag, no close");
                received += 1;
                let on_disk = attractor_journal::read_all(&journal_path).unwrap();
                assert!(
                    on_disk.len() >= received,
                    "event {received} broadcast before it was journaled"
                );
                assert_eq!(
                    on_disk[received - 1].data.type_name(),
                    event.to_journal_data().type_name()
                );
                if matches!(event, PipelineEvent::PipelineCompleted { .. }) {
                    return received;
                }
            }
        }
    });

    PipelineExecutor::with_default_registry()
        .with_event_emitter(emitter)
        .with_journal(journal)
        .run_with_checkpoint(
            &three_stage_graph(),
            dry_run_context(tmp.path()).await,
            &tmp.path().join("logs"),
        )
        .await
        .unwrap();

    let received = subscriber.await.unwrap();
    assert_eq!(received, journal_types(&journal_path).len());
}

// AC2 (deterministic): when the k-th Event is appended, only k-1 Events have
// been broadcast. Swapping the two steps in `emit` fails this every time.
#[tokio::test]
async fn journal_append_happens_before_broadcast() {
    let tmp = tempfile::tempdir().unwrap();
    let emitter = EventEmitter::new(256);
    let mut sink = ScriptedSink::new(|_| false);
    sink.probe = Some(std::sync::Mutex::new(emitter.subscribe()));
    let sink = Arc::new(sink);

    PipelineExecutor::with_default_registry()
        .with_event_emitter(emitter)
        .with_journal_sink(sink.clone())
        .run_with_checkpoint(
            &three_stage_graph(),
            dry_run_context(tmp.path()).await,
            &tmp.path().join("logs"),
        )
        .await
        .unwrap();

    let queued = sink.queued_at_append.lock().unwrap().clone();
    assert!(queued.len() > 10, "expected a full Run, got {queued:?}");
    let expected: Vec<usize> = (0..queued.len()).collect();
    assert_eq!(queued, expected);
}

fn find_named(dir: &Path, name: &str, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.file_name().is_some_and(|n| n == name) {
            found.push(path.clone());
        }
        if path.is_dir() {
            find_named(&path, name, found);
        }
    }
}

// AC3: without `with_journal` the Run behaves as before and writes no journal.
#[tokio::test]
async fn executor_without_journal_writes_no_journal_file() {
    let tmp = tempfile::tempdir().unwrap();
    let emitter = EventEmitter::new(256);
    let mut receiver = emitter.subscribe();

    let result = PipelineExecutor::with_default_registry()
        .with_event_emitter(emitter)
        .run_with_checkpoint(
            &three_stage_graph(),
            dry_run_context(tmp.path()).await,
            &tmp.path().join("logs"),
        )
        .await
        .unwrap();

    assert_eq!(
        result.completed_nodes,
        vec!["start", "plan", "implement", "review", "done"]
    );
    let broadcast = drain_types(&mut receiver);
    assert_eq!(
        broadcast.last().map(String::as_str),
        Some("PipelineCompleted")
    );

    let mut found = Vec::new();
    find_named(tmp.path(), EVENTS_FILE, &mut found);
    find_named(tmp.path(), attractor_journal::RUNS_DIR, &mut found);
    assert!(found.is_empty(), "unexpected journal files: {found:?}");
}

// AC4: a journal that becomes unwritable mid-Run is logged, and the Run still
// reaches its exit node with the full broadcast stream.
#[tokio::test]
async fn journal_write_failure_mid_run_is_logged_and_run_completes() {
    let tmp = tempfile::tempdir().unwrap();
    let log = capture_log();

    let emitter = EventEmitter::new(256);
    let mut receiver = emitter.subscribe();
    let sink = Arc::new(ScriptedSink::new(|n| n > 3));

    let result = PipelineExecutor::with_default_registry()
        .with_event_emitter(emitter)
        .with_journal_sink(sink.clone())
        .run_with_checkpoint(
            &three_stage_graph(),
            dry_run_context(tmp.path()).await,
            &tmp.path().join("logs"),
        )
        .await
        .expect("a mid-Run journal failure must not stop the Run");

    assert_eq!(
        result.completed_nodes.last().map(String::as_str),
        Some("done")
    );
    let broadcast = drain_types(&mut receiver);
    assert_eq!(
        broadcast.last().map(String::as_str),
        Some("PipelineCompleted")
    );
    assert_eq!(sink.written_types(), broadcast[..3].to_vec());
    assert_eq!(sink.calls.load(Ordering::SeqCst), broadcast.len());

    let text = log.text();
    assert!(text.contains("ERROR"), "{text}");
    assert!(text.contains("Run Journal"), "{text}");
    assert!(text.contains(&sink.path.display().to_string()), "{text}");
    assert!(text.contains("disk full"), "{text}");
}

// AC4: after a single failed write the next Event is journaled again.
#[tokio::test]
async fn journal_recovers_after_transient_write_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let emitter = EventEmitter::new(256);
    let mut receiver = emitter.subscribe();
    let sink = Arc::new(ScriptedSink::new(|n| n == 4));

    PipelineExecutor::with_default_registry()
        .with_event_emitter(emitter)
        .with_journal_sink(sink.clone())
        .run_with_checkpoint(
            &three_stage_graph(),
            dry_run_context(tmp.path()).await,
            &tmp.path().join("logs"),
        )
        .await
        .unwrap();

    let mut expected = drain_types(&mut receiver);
    expected.remove(3);
    assert_eq!(sink.written_types(), expected);
}

// AC5 (open): a journal that cannot be opened is an error naming its path.
#[test]
fn open_journal_error_names_the_journal_path() {
    let tmp = tempfile::tempdir().unwrap();

    // The Run folder is an existing regular file.
    let file_run_dir = tmp.path().join("not-a-dir");
    std::fs::write(&file_run_dir, b"x").unwrap();
    let error = open_journal(&file_run_dir, JOURNAL_RUN_ID, 1).unwrap_err();
    let expected = file_run_dir.join(EVENTS_FILE).display().to_string();
    assert!(error.to_string().contains(&expected), "{error}");

    // events.jsonl is a directory.
    let run_dir = tmp.path().join("run");
    std::fs::create_dir_all(run_dir.join(EVENTS_FILE)).unwrap();
    let error = open_journal(&run_dir, JOURNAL_RUN_ID, 1).unwrap_err();
    let expected = run_dir.join(EVENTS_FILE).display().to_string();
    assert!(error.to_string().contains(&expected), "{error}");

    // A usable Run folder opens.
    let ok_dir = tmp.path().join("ok");
    let journal = open_journal(&ok_dir, JOURNAL_RUN_ID, 1).unwrap();
    assert_eq!(journal.path(), ok_dir.join(EVENTS_FILE));
}

// AC5 (engine): a journal that cannot be written at start fails the Run
// before any stage runs, with an error that names the journal path.
#[tokio::test]
async fn unwritable_journal_at_start_fails_run_before_first_stage() {
    struct Counting(Arc<AtomicUsize>);

    #[async_trait]
    impl NodeHandler for Counting {
        fn handler_type(&self) -> &str {
            "counting"
        }
        async fn execute(
            &self,
            _node: &crate::graph::PipelineNode,
            _ctx: &Context,
            _graph: &PipelineGraph,
        ) -> Result<Outcome> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Outcome::success("counted"))
        }
    }

    let graph = parse_graph(
        r#"digraph G {
            start [shape="Mdiamond"]
            one   [type="counting"]
            two   [type="counting"]
            done  [shape="Msquare"]
            start -> one -> two -> done
        }"#,
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = test_registry();
    registry.register(Counting(Arc::clone(&calls)));
    let emitter = EventEmitter::new(64);
    let mut receiver = emitter.subscribe();
    let sink = Arc::new(ScriptedSink::new(|_| true));
    let logs = tempfile::tempdir().unwrap();

    let error = PipelineExecutor::new(registry)
        .with_event_emitter(emitter)
        .with_journal_sink(sink.clone())
        .run_with_checkpoint(&graph, Context::new(), logs.path())
        .await
        .expect_err("an unwritable journal at start must fail the Run");

    assert!(
        error.to_string().contains(&sink.path.display().to_string()),
        "{error}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(sink.calls.load(Ordering::SeqCst), 1);
    assert_eq!(drain_types(&mut receiver), vec!["PipelineFailed"]);
    assert!(!logs.path().join("checkpoint.json").exists());
}

// T1-4: a fresh Run with a journal records the journal's Run ID in its
// checkpoint, so a later `pas run` resumes the same Run.
#[tokio::test]
async fn fresh_run_with_journal_records_run_id_in_checkpoint() {
    let tmp = tempfile::tempdir().unwrap();
    let run_dir = tmp.path().join("runs").join(JOURNAL_RUN_ID);
    let journal = JournalWriter::open(&run_dir, JOURNAL_RUN_ID, 1).unwrap();
    let logs = tmp.path().join("logs");
    // Stop after two steps so the checkpoint is kept (it is cleared on completion).
    let context = dry_run_context(tmp.path()).await;
    context.set("max_steps", serde_json::json!(2u64)).await;

    let err = PipelineExecutor::with_default_registry()
        .with_journal(journal)
        .run_with_checkpoint(&three_stage_graph(), context, &logs)
        .await
        .unwrap_err();

    assert!(
        matches!(err, AttractorError::MaxStepsExceeded { .. }),
        "{err:?}"
    );
    let checkpoint = load_checkpoint(&logs)
        .await
        .unwrap()
        .expect("checkpoint kept");
    assert_eq!(checkpoint.run_id.as_deref(), Some(JOURNAL_RUN_ID));
}

// ---------------------------------------------------------------------------
// T2-1: Run Commits around each stage (spec File Change 5)
// ---------------------------------------------------------------------------

/// `git` in `dir`, independent of the caller's `GIT_DIR` and git config.
fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.com",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap()
}

fn git_ok(dir: &Path, args: &[&str]) -> String {
    let output = git(dir, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

/// A fresh repository in `dir`, with one commit when `with_commit`.
fn init_repo(dir: &Path, with_commit: bool) {
    git_ok(dir, &["init", "-q"]);
    if with_commit {
        git_ok(dir, &["commit", "--allow-empty", "-qm", "initial"]);
    }
}

/// A shell command that makes one empty commit per subject.
fn commit_command(subjects: &[&str]) -> String {
    let commits = subjects
        .iter()
        .map(|subject| {
            format!(
                "git -c user.name=T -c user.email=t@example.com -c commit.gpgsign=false \
                 -c core.hooksPath=/dev/null commit --allow-empty -qm {subject}"
            )
        })
        .collect::<Vec<_>>()
        .join(" && ");
    format!("unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE; {commits}")
}

/// A Run in `workdir` whose journal lives in a separate temp dir, so the Run
/// folder is never inside the repository.
struct CommitRun {
    journal_dir: tempfile::TempDir,
    result: Result<PipelineResult>,
    broadcast: Vec<PipelineEvent>,
}

impl CommitRun {
    async fn start(registry: HandlerRegistry, graph: &PipelineGraph, workdir: &Path) -> Self {
        let journal_dir = tempfile::tempdir().unwrap();
        let journal = JournalWriter::open(journal_dir.path(), JOURNAL_RUN_ID, 1).unwrap();
        let emitter = EventEmitter::new(256);
        let mut receiver = emitter.subscribe();
        let context = Context::new();
        context
            .set(
                "workdir",
                serde_json::Value::String(workdir.display().to_string()),
            )
            .await;
        let result = PipelineExecutor::new(registry)
            .with_event_emitter(emitter)
            .with_journal(journal)
            .run_with_context(graph, context)
            .await;
        let mut broadcast = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            broadcast.push(event);
        }
        Self {
            journal_dir,
            result,
            broadcast,
        }
    }

    fn journal(&self) -> Vec<attractor_journal::JournalEvent> {
        attractor_journal::read_all(self.journal_dir.path().join(EVENTS_FILE)).unwrap()
    }

    /// `(node_id, task_id, commits)` of each journaled `CommitsCreated`.
    fn commits_created(&self) -> Vec<(String, Option<String>, Vec<attractor_journal::CommitRef>)> {
        self.journal()
            .into_iter()
            .filter_map(|event| match event.data {
                EventData::CommitsCreated {
                    node_id,
                    task_id,
                    commits,
                } => Some((node_id, task_id, commits)),
                _ => None,
            })
            .collect()
    }

    /// Raw JSON of each journaled `CommitsCreated` line.
    fn raw_commits_created(&self) -> Vec<serde_json::Value> {
        std::fs::read_to_string(self.journal_dir.path().join(EVENTS_FILE))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|line| line["type"] == "CommitsCreated")
            .collect()
    }
}

fn tool_registry() -> HandlerRegistry {
    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(ExitHandler);
    registry.register(crate::handlers::ToolHandler);
    registry
}

fn tool_graph(stages: &[(&str, &str)]) -> PipelineGraph {
    let nodes = stages
        .iter()
        .map(|(id, command)| format!(r#"{id} [shape="parallelogram", tool_command="{command}"]"#))
        .collect::<Vec<_>>()
        .join("\n");
    let chain = stages
        .iter()
        .map(|(id, _)| *id)
        .collect::<Vec<_>>()
        .join(" -> ");
    parse_graph(&format!(
        r#"digraph G {{
            start [shape="Mdiamond"]
            {nodes}
            done [shape="Msquare"]
            start -> {chain} -> done
        }}"#
    ))
}

/// A `codergen` stand-in that sets or clears `task.id` and can commit.
struct TaskStage {
    task_updates: HashMap<String, serde_json::Value>,
    commit_in: HashMap<String, PathBuf>,
}

#[async_trait]
impl NodeHandler for TaskStage {
    fn handler_type(&self) -> &str {
        "codergen"
    }

    async fn execute(
        &self,
        node: &crate::graph::PipelineNode,
        _ctx: &Context,
        _graph: &PipelineGraph,
    ) -> Result<Outcome> {
        if let Some(dir) = self.commit_in.get(&node.id) {
            git_ok(dir, &["commit", "--allow-empty", "-qm", &node.id]);
        }
        let mut outcome = Outcome::success("task stage");
        if let Some(value) = self.task_updates.get(&node.id) {
            outcome
                .context_updates
                .insert("task.id".into(), value.clone());
        }
        Ok(outcome)
    }
}

// AC1: 2 commits in one stage → exactly one CommitsCreated whose SHAs equal
// `git log --format=%H old..new`, in the same order.
#[tokio::test]
async fn stage_with_two_commits_emits_one_commits_created() {
    let repo = tempfile::tempdir().unwrap();
    init_repo(repo.path(), true);
    let old = git_ok(repo.path(), &["rev-parse", "HEAD"]);
    let command = commit_command(&["one", "two"]);

    let run = CommitRun::start(
        tool_registry(),
        &tool_graph(&[("work", &command)]),
        repo.path(),
    )
    .await;
    run.result.as_ref().unwrap();

    let new = git_ok(repo.path(), &["rev-parse", "HEAD"]);
    let expected: Vec<String> = git_ok(
        repo.path(),
        &["log", "--format=%H", &format!("{old}..{new}")],
    )
    .lines()
    .map(str::to_string)
    .collect();
    assert_eq!(expected.len(), 2);

    let created = run.commits_created();
    assert_eq!(created.len(), 1, "{created:?}");
    let (node_id, task_id, commits) = &created[0];
    assert_eq!(node_id, "work");
    assert_eq!(task_id, &None);
    let shas: Vec<String> = commits.iter().map(|commit| commit.sha.clone()).collect();
    assert_eq!(shas, expected);
    let subjects: Vec<&str> = commits.iter().map(|c| c.subject.as_str()).collect();
    assert_eq!(subjects, ["two", "one"]);
    for commit in commits {
        assert_eq!(commit.author, "T");
        chrono::DateTime::parse_from_rfc3339(&commit.ts).unwrap();
    }

    // Between the stage's StageStarted and StageCompleted.
    let types: Vec<(String, Option<String>)> = run
        .journal()
        .into_iter()
        .map(|event| {
            let node = match &event.data {
                EventData::StageStarted { node_id, .. }
                | EventData::StageCompleted { node_id, .. }
                | EventData::CommitsCreated { node_id, .. } => Some(node_id.clone()),
                _ => None,
            };
            (event.data.type_name().to_string(), node)
        })
        .filter(|(_, node)| node.as_deref() == Some("work"))
        .collect();
    let names: Vec<&str> = types.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["StageStarted", "CommitsCreated", "StageCompleted"]);

    // Journaled before broadcast: the broadcast stream carries the same Event.
    let broadcast: Vec<EventData> = run
        .broadcast
        .iter()
        .map(PipelineEvent::to_journal_data)
        .collect();
    let journaled: Vec<EventData> = run.journal().into_iter().map(|e| e.data).collect();
    assert_eq!(broadcast, journaled);
}

// AC2: no commit → no CommitsCreated.
#[tokio::test]
async fn stage_without_commit_emits_no_commits_created() {
    let repo = tempfile::tempdir().unwrap();
    init_repo(repo.path(), true);

    let run = CommitRun::start(
        tool_registry(),
        &tool_graph(&[("look", "git status --short"), ("idle", "true")]),
        repo.path(),
    )
    .await;

    run.result.as_ref().unwrap();
    assert!(run.commits_created().is_empty());
    assert!(run
        .journal()
        .iter()
        .any(|e| e.data.type_name() == "StageCompleted"));
}

// AC3: the claimed Task's ID is recorded; a stage before any claim has none,
// and the key is absent from the journal line.
#[tokio::test]
async fn commits_created_carries_claimed_task_id() {
    let repo = tempfile::tempdir().unwrap();
    init_repo(repo.path(), true);
    let mut registry = tool_registry();
    registry.register(TaskStage {
        task_updates: HashMap::from([("claim".into(), serde_json::json!("epic.1"))]),
        commit_in: HashMap::new(),
    });
    let before_claim = commit_command(&["early"]);
    let after_claim = commit_command(&["late"]);
    let graph = parse_graph(&format!(
        r#"digraph G {{
            start [shape="Mdiamond"]
            early [shape="parallelogram", tool_command="{before_claim}"]
            claim [shape="box", prompt="claim", llm_provider="claude"]
            late  [shape="parallelogram", tool_command="{after_claim}"]
            done  [shape="Msquare"]
            start -> early -> claim -> late -> done
        }}"#
    ));

    let run = CommitRun::start(registry, &graph, repo.path()).await;
    run.result.as_ref().unwrap();

    let created = run.commits_created();
    let tasks: Vec<(&str, Option<&str>)> = created
        .iter()
        .map(|(node, task, _)| (node.as_str(), task.as_deref()))
        .collect();
    assert_eq!(tasks, [("early", None), ("late", Some("epic.1"))]);

    let raw = run.raw_commits_created();
    assert_eq!(raw.len(), 2);
    let early = raw[0]["data"].as_object().unwrap();
    assert!(!early.contains_key("task_id"), "{early:?}");
    assert_eq!(raw[1]["data"]["task_id"], "epic.1");
}

// AC3: a stage that claims a Task and commits attributes to the new Task; a
// stage that clears `task.id` (beads.close) attributes to the Task it closed;
// after that, commits have no Task.
#[tokio::test]
async fn commits_are_attributed_to_the_task_active_during_the_stage() {
    let repo = tempfile::tempdir().unwrap();
    init_repo(repo.path(), true);
    let mut registry = tool_registry();
    registry.register(TaskStage {
        task_updates: HashMap::from([
            ("claim".into(), serde_json::json!("epic.2")),
            ("close".into(), serde_json::Value::Null),
        ]),
        commit_in: HashMap::from([
            ("claim".into(), repo.path().to_path_buf()),
            ("close".into(), repo.path().to_path_buf()),
        ]),
    });
    let after = commit_command(&["after"]);
    let graph = parse_graph(&format!(
        r#"digraph G {{
            start [shape="Mdiamond"]
            claim [shape="box", prompt="claim", llm_provider="claude"]
            close [shape="box", prompt="close", llm_provider="claude"]
            after [shape="parallelogram", tool_command="{after}"]
            done  [shape="Msquare"]
            start -> claim -> close -> after -> done
        }}"#
    ));

    let run = CommitRun::start(registry, &graph, repo.path()).await;
    run.result.as_ref().unwrap();

    let tasks: Vec<(String, Option<String>)> = run
        .commits_created()
        .into_iter()
        .map(|(node, task, _)| (node, task))
        .collect();
    assert_eq!(
        tasks,
        [
            ("claim".to_string(), Some("epic.2".to_string())),
            ("close".to_string(), Some("epic.2".to_string())),
            ("after".to_string(), None),
        ]
    );
}

#[test]
fn only_non_empty_string_task_ids_are_tasks() {
    assert_eq!(task_id_value(None), None);
    assert_eq!(task_id_value(Some(&serde_json::Value::Null)), None);
    assert_eq!(task_id_value(Some(&serde_json::json!(""))), None);
    assert_eq!(task_id_value(Some(&serde_json::json!("  "))), None);
    assert_eq!(task_id_value(Some(&serde_json::json!(7))), None);
    assert_eq!(
        task_id_value(Some(&serde_json::json!("e.1"))),
        Some("e.1".to_string())
    );
}

// AC4: outside a git repository the Run completes with no CommitsCreated.
#[tokio::test]
async fn run_outside_git_repo_emits_no_commits_created() {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        !git(dir.path(), &["rev-parse", "--git-dir"])
            .status
            .success(),
        "temp dir {} is inside a git repository; the test would be vacuous",
        dir.path().display()
    );
    // A git command in a stage fails there, but the Run does not.
    let run = CommitRun::start(
        tool_registry(),
        &tool_graph(&[("idle", "true"), ("try", "git log -1 || true")]),
        dir.path(),
    )
    .await;

    run.result.as_ref().unwrap();
    assert!(run.commits_created().is_empty());
    assert!(run
        .journal()
        .iter()
        .any(|e| e.data.type_name() == "PipelineCompleted"));
}

// AC5: with an unborn HEAD, the stage that makes the first commit does not
// fail the Run (and emits nothing, as the spec skips unborn HEAD); the next
// stage's commit is recorded.
#[tokio::test]
async fn first_commit_in_unborn_repo_does_not_fail_run() {
    let repo = tempfile::tempdir().unwrap();
    init_repo(repo.path(), false);
    assert!(
        !git(repo.path(), &["rev-parse", "--verify", "--quiet", "HEAD"])
            .status
            .success()
    );
    let first = commit_command(&["first"]);
    let second = commit_command(&["second"]);

    let run = CommitRun::start(
        tool_registry(),
        &tool_graph(&[("first", &first), ("second", &second)]),
        repo.path(),
    )
    .await;

    let result = run.result.as_ref().unwrap();
    assert_eq!(result.completed_nodes, ["start", "first", "second", "done"]);
    let created = run.commits_created();
    assert_eq!(created.len(), 1, "{created:?}");
    assert_eq!(created[0].0, "second");
    let head = git_ok(repo.path(), &["rev-parse", "HEAD"]);
    let shas: Vec<&str> = created[0].2.iter().map(|c| c.sha.as_str()).collect();
    assert_eq!(shas, [head.as_str()]);
}

// A retried attempt keeps its own commits: one CommitsCreated per attempt,
// each before that attempt's StageRetrying / StageCompleted.
#[tokio::test]
async fn each_attempt_reports_its_own_commits() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CommitThenRetry {
        dir: PathBuf,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl NodeHandler for CommitThenRetry {
        fn handler_type(&self) -> &str {
            "codergen"
        }

        async fn execute(
            &self,
            _node: &crate::graph::PipelineNode,
            _ctx: &Context,
            _graph: &PipelineGraph,
        ) -> Result<Outcome> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            git_ok(
                &self.dir,
                &["commit", "--allow-empty", "-qm", &format!("attempt {call}")],
            );
            let mut outcome = Outcome::success("attempt");
            if call == 0 {
                outcome.status = StageStatus::Retry;
            }
            Ok(outcome)
        }
    }

    let repo = tempfile::tempdir().unwrap();
    init_repo(repo.path(), true);
    let mut registry = tool_registry();
    registry.register(CommitThenRetry {
        dir: repo.path().to_path_buf(),
        calls: AtomicUsize::new(0),
    });
    let graph = parse_graph(
        r#"digraph G {
            start [shape="Mdiamond"]
            work [shape="box", prompt="work", llm_provider="claude", max_retries=1]
            done [shape="Msquare"]
            start -> work -> done
        }"#,
    );

    let run = CommitRun::start(registry, &graph, repo.path()).await;
    run.result.as_ref().unwrap();

    let types: Vec<String> = run
        .journal()
        .into_iter()
        .map(|event| event.data.type_name().to_string())
        .filter(|name| {
            matches!(
                name.as_str(),
                "CommitsCreated" | "StageRetrying" | "StageCompleted"
            )
        })
        .collect();
    assert_eq!(
        types,
        [
            "StageCompleted", // start
            "CommitsCreated",
            "StageRetrying",
            "CommitsCreated",
            "StageCompleted", // work
            "StageCompleted", // done
        ]
    );
    let subjects: Vec<Vec<String>> = run
        .commits_created()
        .into_iter()
        .map(|(_, _, commits)| commits.into_iter().map(|c| c.subject).collect())
        .collect();
    assert_eq!(subjects, [["attempt 0"], ["attempt 1"]]);
}

// HEAD moving backwards (reset) lists no commits and emits nothing.
#[tokio::test]
async fn head_moving_backwards_emits_no_commits_created() {
    let repo = tempfile::tempdir().unwrap();
    init_repo(repo.path(), true);
    git_ok(repo.path(), &["commit", "--allow-empty", "-qm", "second"]);

    let run = CommitRun::start(
        tool_registry(),
        &tool_graph(&[("undo", "git reset -q --soft HEAD~1")]),
        repo.path(),
    )
    .await;

    run.result.as_ref().unwrap();
    assert!(run.commits_created().is_empty());
}

/// Records the Run folder each provider stage receives.
struct RunDirRecorder(Arc<std::sync::Mutex<Vec<Option<PathBuf>>>>);

#[async_trait]
impl NodeHandler for RunDirRecorder {
    fn handler_type(&self) -> &str {
        "codergen"
    }

    fn provider_handler(&self) -> Option<&dyn crate::handler::ProviderNodeHandler> {
        Some(self)
    }

    async fn execute(
        &self,
        _node: &crate::graph::PipelineNode,
        _ctx: &Context,
        _graph: &PipelineGraph,
    ) -> Result<Outcome> {
        unreachable!("canonical execution uses execute_configured")
    }
}

#[async_trait]
impl crate::handler::ProviderNodeHandler for RunDirRecorder {
    async fn execute_resolved(
        &self,
        _node: &crate::graph::PipelineNode,
        _resolved: &crate::execution_plan::ResolvedNode,
        _context: &Context,
        _graph: &PipelineGraph,
    ) -> Result<Outcome> {
        unreachable!("canonical execution uses execute_configured")
    }

    async fn execute_configured(
        &self,
        _node: &crate::graph::PipelineNode,
        _resolved: &crate::execution_plan::ResolvedNode,
        execution: HandlerExecutionContext<'_>,
        _graph: &PipelineGraph,
    ) -> Result<Outcome> {
        self.0
            .lock()
            .unwrap()
            .push(execution.run_dir().map(Path::to_path_buf));
        Ok(Outcome::success("recorded"))
    }
}

fn run_dir_recorder_registry() -> (HandlerRegistry, Arc<std::sync::Mutex<Vec<Option<PathBuf>>>>) {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(ExitHandler);
    registry.register(RunDirRecorder(seen.clone()));
    (registry, seen)
}

// T2-2: provider handlers get the journal's Run folder for Transcripts, and
// no Run folder when the executor has no journal.
#[tokio::test]
async fn journaled_executor_passes_run_dir_to_provider_handlers() {
    let tmp = tempfile::tempdir().unwrap();
    let run_dir = tmp.path().join("runs").join(JOURNAL_RUN_ID);
    let journal = JournalWriter::open(&run_dir, JOURNAL_RUN_ID, 1).unwrap();
    let (registry, seen) = run_dir_recorder_registry();

    PipelineExecutor::new(registry)
        .with_journal(journal)
        .run_with_checkpoint(
            &three_stage_graph(),
            Context::new(),
            &tmp.path().join("logs"),
        )
        .await
        .unwrap();

    assert_eq!(*seen.lock().unwrap(), vec![Some(run_dir); 3]);
}

#[tokio::test]
async fn executor_without_journal_passes_no_run_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let (registry, seen) = run_dir_recorder_registry();

    PipelineExecutor::new(registry)
        .run_with_checkpoint(
            &three_stage_graph(),
            Context::new(),
            &tmp.path().join("logs"),
        )
        .await
        .unwrap();

    assert_eq!(*seen.lock().unwrap(), vec![None; 3]);
}

/// A provider handler that reports one Model Invocation per stage through
/// the engine's Event path, as the codergen handler does.
struct InvokingProvider;

#[async_trait]
impl NodeHandler for InvokingProvider {
    fn handler_type(&self) -> &str {
        "codergen"
    }

    fn provider_handler(&self) -> Option<&dyn crate::handler::ProviderNodeHandler> {
        Some(self)
    }

    async fn execute(
        &self,
        _node: &crate::graph::PipelineNode,
        _ctx: &Context,
        _graph: &PipelineGraph,
    ) -> Result<Outcome> {
        unreachable!("canonical execution uses execute_configured")
    }
}

#[async_trait]
impl crate::handler::ProviderNodeHandler for InvokingProvider {
    async fn execute_resolved(
        &self,
        _node: &crate::graph::PipelineNode,
        _resolved: &crate::execution_plan::ResolvedNode,
        _context: &Context,
        _graph: &PipelineGraph,
    ) -> Result<Outcome> {
        unreachable!("canonical execution uses execute_configured")
    }

    async fn execute_configured(
        &self,
        node: &crate::graph::PipelineNode,
        _resolved: &crate::execution_plan::ResolvedNode,
        execution: HandlerExecutionContext<'_>,
        _graph: &PipelineGraph,
    ) -> Result<Outcome> {
        let events = execution.events().expect("engine passes its Event path");
        events.emit(PipelineEvent::LlmInvoked {
            invocation_id: format!("inv-{}", node.id),
            node_id: node.id.clone(),
            provider: "claude".into(),
            model_requested: None,
            model_actual: Some("claude-haiku-4-5".into()),
            input_tokens: Some(1),
            output_tokens: Some(2),
            cost_usd: None,
            duration_ms: 5,
            transcript: attractor_journal::transcript_rel_path(&format!("inv-{}", node.id)),
            status: "success".into(),
        });
        Ok(Outcome::success("invoked"))
    }
}

// T2-4: a handler's LlmInvoked goes through the engine's Event path: it is
// journaled between StageStarted and StageCompleted, and broadcast after.
#[tokio::test]
async fn handler_llm_invoked_is_journaled_inside_its_stage_and_broadcast() {
    let tmp = tempfile::tempdir().unwrap();
    let run_dir = tmp.path().join("runs").join(JOURNAL_RUN_ID);
    let journal = JournalWriter::open(&run_dir, JOURNAL_RUN_ID, 1).unwrap();
    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(ExitHandler);
    registry.register(InvokingProvider);
    let emitter = EventEmitter::new(256);
    let mut receiver = emitter.subscribe();

    PipelineExecutor::new(registry)
        .with_event_emitter(emitter)
        .with_journal(journal)
        .run_with_checkpoint(
            &three_stage_graph(),
            Context::new(),
            &tmp.path().join("logs"),
        )
        .await
        .unwrap();

    let journal = attractor_journal::read_all(run_dir.join(EVENTS_FILE)).unwrap();
    let stage_events: Vec<(String, String)> = journal
        .iter()
        .filter_map(|event| match &event.data {
            EventData::StageStarted { node_id, .. } => Some(("StageStarted", node_id)),
            EventData::LlmInvoked { node_id, .. } => Some(("LlmInvoked", node_id)),
            EventData::StageCompleted { node_id, .. } => Some(("StageCompleted", node_id)),
            _ => None,
        })
        .map(|(kind, node)| (kind.to_owned(), node.clone()))
        .collect();
    let mut expected = Vec::new();
    for node in ["start", "plan", "implement", "review", "done"] {
        expected.push(("StageStarted".to_owned(), node.to_owned()));
        if !matches!(node, "start" | "done") {
            expected.push(("LlmInvoked".to_owned(), node.to_owned()));
        }
        expected.push(("StageCompleted".to_owned(), node.to_owned()));
    }
    assert_eq!(stage_events, expected);

    let invoked = journal
        .iter()
        .find_map(|event| match &event.data {
            EventData::LlmInvoked {
                invocation_id,
                transcript,
                model_requested,
                ..
            } => Some((
                invocation_id.clone(),
                transcript.clone(),
                model_requested.clone(),
            )),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        invoked,
        ("inv-plan".into(), "transcripts/inv-plan.jsonl".into(), None)
    );

    let mut broadcast = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        if let PipelineEvent::LlmInvoked { node_id, .. } = event {
            broadcast.push(node_id);
        }
    }
    assert_eq!(broadcast, vec!["plan", "implement", "review"]);
}

/// The real codergen handler, reached through the engine's canonical path,
/// with a stub provider executable in place of the `claude` binary.
struct StubbedCodergen(PathBuf);

#[async_trait]
impl NodeHandler for StubbedCodergen {
    fn handler_type(&self) -> &str {
        "codergen"
    }

    fn provider_handler(&self) -> Option<&dyn crate::handler::ProviderNodeHandler> {
        Some(self)
    }

    async fn execute(
        &self,
        _node: &crate::graph::PipelineNode,
        _ctx: &Context,
        _graph: &PipelineGraph,
    ) -> Result<Outcome> {
        unreachable!("canonical execution uses execute_configured")
    }
}

#[async_trait]
impl crate::handler::ProviderNodeHandler for StubbedCodergen {
    async fn execute_resolved(
        &self,
        _node: &crate::graph::PipelineNode,
        _resolved: &crate::execution_plan::ResolvedNode,
        _context: &Context,
        _graph: &PipelineGraph,
    ) -> Result<Outcome> {
        unreachable!("canonical execution uses execute_configured")
    }

    async fn execute_configured(
        &self,
        node: &crate::graph::PipelineNode,
        resolved: &crate::execution_plan::ResolvedNode,
        execution: HandlerExecutionContext<'_>,
        graph: &PipelineGraph,
    ) -> Result<Outcome> {
        crate::handlers::CodergenHandler
            .execute_configured_with_program(node, resolved, execution, graph, Some(self.0.clone()))
            .await
    }
}

// T2-4: a real codergen Model Invocation run by the engine writes LlmInvoked
// to the Run Journal inside its stage, naming a Transcript that exists.
#[tokio::test]
async fn codergen_stage_journals_llm_invoked_with_existing_transcript() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let program = tmp.path().join("claude-stub");
    std::fs::write(
        &program,
        "#!/bin/sh\necho '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"done\",\"total_cost_usd\":0.01,\"num_turns\":1}'\n",
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let run_dir = tmp.path().join("runs").join(JOURNAL_RUN_ID);
    let journal = JournalWriter::open(&run_dir, JOURNAL_RUN_ID, 1).unwrap();
    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(ExitHandler);
    registry.register(StubbedCodergen(program));

    PipelineExecutor::new(registry)
        .with_journal(journal)
        .run_with_checkpoint(
            &parse_graph(
                r#"digraph G {
                    node [llm_provider="claude"]
                    start [shape="Mdiamond"]
                    work  [shape="box", prompt="work"]
                    done  [shape="Msquare"]
                    start -> work -> done
                }"#,
            ),
            Context::new(),
            &tmp.path().join("logs"),
        )
        .await
        .unwrap();

    let journal = attractor_journal::read_all(run_dir.join(EVENTS_FILE)).unwrap();
    let work_events: Vec<&str> = journal
        .iter()
        .filter_map(|event| match &event.data {
            EventData::StageStarted { node_id, .. } if node_id == "work" => Some("StageStarted"),
            EventData::LlmInvoked { node_id, .. } if node_id == "work" => Some("LlmInvoked"),
            EventData::StageCompleted { node_id, .. } if node_id == "work" => {
                Some("StageCompleted")
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        work_events,
        ["StageStarted", "LlmInvoked", "StageCompleted"]
    );

    let (invocation_id, provider, transcript, status) = journal
        .iter()
        .find_map(|event| match &event.data {
            EventData::LlmInvoked {
                invocation_id,
                provider,
                transcript,
                status,
                ..
            } => Some((
                invocation_id.clone(),
                provider.clone(),
                transcript.clone(),
                status.clone(),
            )),
            _ => None,
        })
        .unwrap();
    assert_eq!(provider, "claude");
    assert_eq!(status, "success");
    assert_eq!(
        transcript,
        attractor_journal::transcript_rel_path(&invocation_id)
    );
    assert!(Path::new(&transcript).is_relative());
    assert!(
        run_dir.join(&transcript).is_file(),
        "Transcript {transcript} missing"
    );
}

/// Removes `task.id` from the Context it is given, and records what the
/// next stage sees.
struct RemovingStage(Arc<std::sync::Mutex<Vec<Option<serde_json::Value>>>>);

#[async_trait]
impl NodeHandler for RemovingStage {
    fn handler_type(&self) -> &str {
        "test.remove"
    }

    async fn execute(
        &self,
        node: &crate::graph::PipelineNode,
        ctx: &Context,
        _graph: &PipelineGraph,
    ) -> Result<Outcome> {
        let task_id = ctx.get("task.id").await;
        self.0.lock().unwrap().push(task_id);
        if node.id == "clear" {
            ctx.remove("task.id").await;
        }
        Ok(Outcome::success("removed"))
    }
}

// T3-3: a key a handler removes from its Context is gone for later stages and
// in the final Context; other keys are untouched.
#[tokio::test]
async fn direct_context_removal_reaches_later_stages() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(ExitHandler);
    registry.register(RemovingStage(seen.clone()));
    let graph = parse_graph(
        r#"digraph G {
            start [shape="Mdiamond"]
            clear [type="test.remove"]
            after [type="test.remove"]
            done  [shape="Msquare"]
            start -> clear -> after -> done
        }"#,
    );
    let workdir = tempfile::tempdir().unwrap();
    let context = Context::new();
    context.set("task.id", serde_json::json!("e.1")).await;
    context.set("keep", serde_json::json!("me")).await;
    context
        .set(
            "workdir",
            serde_json::json!(workdir.path().display().to_string()),
        )
        .await;

    let result = PipelineExecutor::new(registry)
        .run_with_context(&graph, context)
        .await
        .unwrap();

    assert_eq!(
        *seen.lock().unwrap(),
        vec![Some(serde_json::json!("e.1")), None]
    );
    assert!(!result.final_context.contains_key("task.id"));
    assert_eq!(result.final_context["keep"], "me");
}
