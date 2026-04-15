//! Pipeline execution engine — the core traversal loop.
//!
//! Implements the 5-phase lifecycle: parse, validate, initialize, execute, finalize.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use attractor_types::{AttractorError, Context, Outcome, Result, StageStatus};

use crate::checkpoint::{clear_checkpoint, load_checkpoint, save_checkpoint, PipelineCheckpoint};
use crate::edge_selection::select_edge;
use crate::goal_gate::enforce_goal_gates;
use crate::graph::PipelineGraph;
use crate::handler::{default_registry, HandlerRegistry};
use crate::retry::{execute_with_retry, BackoffPolicy};
use crate::validation::validate_or_raise;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// The core pipeline executor. Owns a handler registry and drives graph traversal.
pub struct PipelineExecutor {
    registry: HandlerRegistry,
}

/// Configuration for a pipeline run.
pub struct PipelineConfig {
    pub logs_root: PathBuf,
}

/// The result of a completed pipeline execution.
#[derive(Debug)]
pub struct PipelineResult {
    pub completed_nodes: Vec<String>,
    pub node_outcomes: HashMap<String, Outcome>,
    pub final_context: HashMap<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert an `attractor_dot::AttributeValue` to a `serde_json::Value`.
fn attr_to_json(val: &attractor_dot::AttributeValue) -> serde_json::Value {
    match val {
        attractor_dot::AttributeValue::String(s) => serde_json::Value::String(s.clone()),
        attractor_dot::AttributeValue::Integer(i) => serde_json::json!(*i),
        attractor_dot::AttributeValue::Float(f) => serde_json::json!(*f),
        attractor_dot::AttributeValue::Boolean(b) => serde_json::Value::Bool(*b),
        attractor_dot::AttributeValue::Duration(d) => serde_json::json!(d.as_millis() as u64),
    }
}

/// Map a `StageStatus` to the lowercase string used in edge conditions.
fn status_to_string(status: StageStatus) -> String {
    match status {
        StageStatus::Success => "success".to_string(),
        StageStatus::PartialSuccess => "partial_success".to_string(),
        StageStatus::Retry => "retry".to_string(),
        StageStatus::Fail => "fail".to_string(),
        StageStatus::Skipped => "skipped".to_string(),
    }
}

// ---------------------------------------------------------------------------
// PipelineExecutor
// ---------------------------------------------------------------------------

impl PipelineExecutor {
    /// Create an executor with the given handler registry.
    pub fn new(registry: HandlerRegistry) -> Self {
        Self { registry }
    }

    /// Create an executor pre-loaded with the default built-in handlers.
    pub fn with_default_registry() -> Self {
        Self {
            registry: default_registry(),
        }
    }

    /// Run the full 5-phase pipeline lifecycle on the given graph.
    pub async fn run(&self, graph: &PipelineGraph) -> Result<PipelineResult> {
        self.run_with_context(graph, Context::new()).await
    }

    /// Run the pipeline with a pre-seeded context (e.g. workdir, dry_run).
    pub async fn run_with_context(
        &self,
        graph: &PipelineGraph,
        context: Context,
    ) -> Result<PipelineResult> {
        self.run_inner(graph, context, None).await
    }

    /// Run the pipeline with checkpoint-based resume.
    ///
    /// If `logs_root` points to a directory containing `checkpoint.json`,
    /// execution resumes from the last saved node. A checkpoint is saved
    /// after every node completion and cleared on successful finish.
    pub async fn run_with_checkpoint(
        &self,
        graph: &PipelineGraph,
        context: Context,
        logs_root: &Path,
    ) -> Result<PipelineResult> {
        self.run_inner(graph, context, Some(logs_root)).await
    }

    /// Core execution loop. When `logs_root` is `Some`, checkpoints are
    /// saved after each node and an existing checkpoint triggers resume.
    async fn run_inner(
        &self,
        graph: &PipelineGraph,
        context: Context,
        logs_root: Option<&Path>,
    ) -> Result<PipelineResult> {
        // Phase 2: Validate
        validate_or_raise(graph)?;

        // Phase 3: Initialize (merge graph attrs into existing context)
        for (key, val) in &graph.attrs {
            context.set(key, attr_to_json(val)).await;
        }
        let mut completed_nodes: Vec<String> = Vec::new();
        let mut node_outcomes: HashMap<String, Outcome> = HashMap::new();

        // Phase 4: Execute — check for checkpoint to resume from
        let start = graph
            .start_node()
            .ok_or_else(|| AttractorError::ValidationError("No start node found".into()))?;
        let mut current_node = start;

        // (step_count, total_cost) restored from checkpoint; default 0 on fresh run.
        let mut checkpoint_counters: (u64, f64) = (0, 0.0);

        if let Some(logs) = logs_root {
            if let Some(cp) = load_checkpoint(logs).await? {
                tracing::info!(
                    node = %cp.current_node_id,
                    completed = cp.completed_nodes.len(),
                    step_count = cp.step_count,
                    total_cost = cp.total_cost,
                    "Resuming from checkpoint"
                );
                // Restore context
                context.apply_updates(cp.context_snapshot).await;
                // Restore completed state
                completed_nodes = cp.completed_nodes;
                node_outcomes = cp.node_outcomes;
                // Restore safety counters so limits are enforced correctly on resume
                checkpoint_counters = (cp.step_count, cp.total_cost);
                // Jump to the node that was about to execute
                current_node = graph.node(&cp.current_node_id).ok_or_else(|| {
                    AttractorError::Other(format!(
                        "Checkpoint node '{}' not found in graph — was the .dot file changed?",
                        cp.current_node_id
                    ))
                })?;
            }
        }

        // Safety limits from context (set by CLI flags)
        let max_budget: f64 = context
            .get("max_budget_usd")
            .await
            .and_then(|v| v.as_f64())
            .unwrap_or(200.0);
        let max_steps: u64 = context
            .get("max_steps")
            .await
            .and_then(|v| v.as_u64())
            .unwrap_or(200);
        // These are populated from checkpoint if resuming, so that limits are
        // correctly enforced across resume (not reset to zero each restart).
        let mut total_cost: f64 = checkpoint_counters.1;
        let mut step_count: u64 = checkpoint_counters.0;

        loop {
            // Check safety limits
            step_count += 1;
            if step_count > max_steps {
                tracing::error!(steps = step_count, max = max_steps, "Step limit exceeded");
                return Err(AttractorError::Other(format!(
                    "Pipeline exceeded maximum step count ({max_steps}). Use --max-steps to increase."
                )));
            }
            if total_cost > max_budget {
                tracing::error!(cost = total_cost, max = max_budget, "Budget exceeded");
                return Err(AttractorError::Other(format!(
                    "Pipeline exceeded budget (${:.2} > ${:.2}). Use --max-budget-usd to increase.",
                    total_cost, max_budget
                )));
            }

            // Terminal check (exit node)
            if current_node.shape == "Msquare" {
                // Check goal gates
                let gate_result = enforce_goal_gates(graph, &node_outcomes)?;
                if !gate_result.all_satisfied {
                    if let Some(ref target) = gate_result.retry_target {
                        current_node = graph.node(target).ok_or_else(|| {
                            AttractorError::Other(format!("Retry target '{}' not found", target))
                        })?;
                        continue;
                    }
                }

                // Execute the exit handler
                let handler_type = self.registry.resolve_type(current_node);
                let handler = self.registry.get(&handler_type).ok_or_else(|| {
                    AttractorError::HandlerError {
                        handler: handler_type.clone(),
                        node: current_node.id.clone(),
                        message: format!("No handler registered for type '{}'", handler_type),
                    }
                })?;
                let outcome = if current_node.max_retries > 0 {
                    execute_with_retry(
                        || handler.execute(current_node, &context, graph),
                        current_node.max_retries,
                        &BackoffPolicy::default(),
                        &current_node.id,
                    )
                    .await?
                } else {
                    handler.execute(current_node, &context, graph).await?
                };
                completed_nodes.push(current_node.id.clone());
                node_outcomes.insert(current_node.id.clone(), outcome);
                break;
            }

            // Execute handler
            let handler_type = self.registry.resolve_type(current_node);
            let handler =
                self.registry
                    .get(&handler_type)
                    .ok_or_else(|| AttractorError::HandlerError {
                        handler: handler_type.clone(),
                        node: current_node.id.clone(),
                        message: format!("No handler registered for type '{}'", handler_type),
                    })?;
            let outcome = if current_node.max_retries > 0 {
                execute_with_retry(
                    || handler.execute(current_node, &context, graph),
                    current_node.max_retries,
                    &BackoffPolicy::default(),
                    &current_node.id,
                )
                .await?
            } else {
                handler.execute(current_node, &context, graph).await?
            };

            // Record
            completed_nodes.push(current_node.id.clone());
            node_outcomes.insert(current_node.id.clone(), outcome.clone());

            // Track cost from this node
            if let Some(cost) = outcome
                .context_updates
                .get(&format!("{}.cost_usd", current_node.id))
            {
                if let Some(c) = cost.as_f64() {
                    total_cost += c;
                    tracing::info!(
                        node = %current_node.id,
                        node_cost = c,
                        total_cost = total_cost,
                        budget_remaining = max_budget - total_cost,
                        "Cost update"
                    );
                }
            }

            // Apply context updates
            context.apply_updates(outcome.context_updates.clone()).await;
            context
                .set(
                    "outcome",
                    serde_json::Value::String(status_to_string(outcome.status)),
                )
                .await;
            // Always overwrite preferred_label to prevent stale values from
            // a previous node persisting into the next node's context.
            context
                .set(
                    "preferred_label",
                    serde_json::Value::String(
                        outcome.preferred_label.clone().unwrap_or_default(),
                    ),
                )
                .await;

            // Select next edge — resolve condition keys from outcome and context
            let ctx_snapshot = context.snapshot().await;
            let resolve = |key: &str| -> String {
                match key {
                    "outcome" => status_to_string(outcome.status),
                    "preferred_label" => outcome.preferred_label.clone().unwrap_or_default(),
                    _ => ctx_snapshot
                        .get(key)
                        .map(|v| match v {
                            serde_json::Value::String(s) => s.clone(),
                            serde_json::Value::Bool(b) => b.to_string(),
                            serde_json::Value::Number(n) => n.to_string(),
                            _ => v.to_string(),
                        })
                        .unwrap_or_default(),
                }
            };
            let next_edge = select_edge(&current_node.id, &outcome, &resolve, graph);

            match next_edge {
                Some(edge) => {
                    // Handle loop_restart: snapshot outcomes BEFORE clearing so
                    // that if the next node is an exit node, goal gate evaluation
                    // sees the completed loop's outcomes rather than an empty map.
                    if edge.loop_restart {
                        let pre_restart_outcomes = node_outcomes.clone();
                        completed_nodes.clear();
                        node_outcomes.clear();
                        // If the target is an exit node we need the pre-restart
                        // outcomes available for enforce_goal_gates. Re-insert
                        // them temporarily under a side-channel key isn't clean,
                        // so we stash them back and let the exit node check use
                        // them — the exit check runs before any new execution.
                        // We achieve this by not clearing if the target is Msquare.
                        let next_node_shape = graph
                            .node(&edge.to)
                            .map(|n| n.shape.as_str())
                            .unwrap_or("");
                        if next_node_shape == "Msquare" {
                            // Restore outcomes so the goal gate check at the top
                            // of the next loop iteration sees the loop's work.
                            node_outcomes = pre_restart_outcomes;
                        }
                    }
                    let next_id = edge.to.clone();
                    current_node = graph.node(&next_id).ok_or_else(|| {
                        AttractorError::Other(format!("Edge target '{}' not found", next_id))
                    })?;

                    // Save checkpoint: the *next* node to execute, including
                    // counters so that resume correctly enforces limits.
                    if let Some(logs) = logs_root {
                        let cp = PipelineCheckpoint::with_counters(
                            current_node.id.clone(),
                            completed_nodes.clone(),
                            node_outcomes.clone(),
                            context.snapshot().await,
                            step_count,
                            total_cost,
                        );
                        save_checkpoint(&cp, logs).await?;
                    }
                }
                None => {
                    // No outgoing edge and not an exit node
                    match outcome.status {
                        StageStatus::Fail => {
                            return Err(AttractorError::HandlerError {
                                handler: handler_type,
                                node: current_node.id.clone(),
                                message: "Handler failed with no outgoing edge".into(),
                            });
                        }
                        StageStatus::Retry => {
                            return Err(AttractorError::HandlerError {
                                handler: handler_type,
                                node: current_node.id.clone(),
                                message: "Handler requested retry but there is no outgoing edge to retry from".into(),
                            });
                        }
                        StageStatus::Skipped => {
                            tracing::warn!(
                                node = %current_node.id,
                                "Node completed with Skipped status but has no outgoing edge — treating as terminal"
                            );
                            break;
                        }
                        _ => break,
                    }
                }
            }
        }

        // Phase 5: Finalize — clear checkpoint on success
        if let Some(logs) = logs_root {
            clear_checkpoint(logs).await?;
        }
        let final_context = context.snapshot().await;
        Ok(PipelineResult {
            completed_nodes,
            node_outcomes,
            final_context,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::PipelineGraph;
    use crate::handler::{
        ConditionalHandler, ExitHandler, HandlerRegistry, NodeHandler, StartHandler,
    };
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
}
