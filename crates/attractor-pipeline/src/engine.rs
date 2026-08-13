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
use crate::validation::validate_or_raise;

pub const DEFAULT_MAX_BUDGET_USD: f64 = 200.0;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// The core pipeline executor. Owns a handler registry and drives graph traversal.
pub struct PipelineExecutor {
    registry: HandlerRegistry,
}

/// The result of a completed pipeline execution.
#[derive(Debug)]
pub struct PipelineResult {
    pub completed_nodes: Vec<String>,
    pub node_outcomes: HashMap<String, Outcome>,
    pub final_context: HashMap<String, serde_json::Value>,
    pub total_cost: f64,
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

async fn manifest_max_fix_iterations(context: &Context) -> Option<u32> {
    let workdir = context
        .get("workdir")
        .await
        .and_then(|v| v.as_str().map(PathBuf::from))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    attractor_quality::resolve(&workdir)
        .ok()
        .and_then(|resolved| resolved.manifest.quality)
        .and_then(|quality| quality.max_fix_iterations)
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

        // Quality loop state: per-(node_id::upstream_id) entry counters
        let mut quality_loop_counters: HashMap<String, u32> = HashMap::new();
        let mut quality_last_footprint: HashMap<String, String> = HashMap::new();
        // Tracks the node we came from (upstream) for loop-key construction
        let mut prev_node_id: Option<String> = None;

        // Phase 4: Execute — check for checkpoint to resume from
        let start = graph
            .start_node()
            .ok_or_else(|| AttractorError::ValidationError("No start node found".into()))?;
        let mut current_node = start;

        // Safety limits from context (set by CLI flags)
        let max_budget: f64 = context
            .get("max_budget_usd")
            .await
            .and_then(|v| v.as_f64())
            .unwrap_or(DEFAULT_MAX_BUDGET_USD);
        let max_steps: u64 = context
            .get("max_steps")
            .await
            .and_then(|v| v.as_u64())
            .unwrap_or(200);
        let mut total_cost: f64 = 0.0;
        let mut step_count: u64 = 0;

        if let Some(logs) = logs_root {
            if let Some(cp) = load_checkpoint(logs).await? {
                tracing::info!(
                    node = %cp.current_node_id,
                    completed = cp.completed_nodes.len(),
                    "Resuming from checkpoint"
                );
                // Restore context
                context.apply_updates(cp.context_snapshot).await;
                // Restore completed state
                completed_nodes = cp.completed_nodes;
                node_outcomes = cp.node_outcomes;
                // Restore counters from checkpoint
                step_count = cp.step_count;
                total_cost = cp.total_cost;
                quality_loop_counters = cp.quality_loop_counters;
                quality_last_footprint = cp.quality_last_footprint;
                prev_node_id = cp.previous_node_id;
                // Jump to the node that was about to execute
                current_node = graph.node(&cp.current_node_id).ok_or_else(|| {
                    AttractorError::Other(format!(
                        "Checkpoint node '{}' not found in graph — was the .dot file changed?",
                        cp.current_node_id
                    ))
                })?;
            }
        }

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
                let outcome = handler.execute(current_node, &context, graph).await?;
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

            // Quality loop control: track entries and enforce max_fix_iterations
            let is_quality = handler_type == "quality";
            if is_quality {
                let upstream = prev_node_id.as_deref().unwrap_or("__start__");
                let loop_key = format!("{}::{}", current_node.id, upstream);
                let counter = quality_loop_counters.entry(loop_key).or_insert(0);
                *counter += 1;
                let iteration = *counter;

                // Resolve max_fix_iterations: node attr → manifest → context → default 3
                let manifest_max_iters = manifest_max_fix_iterations(&context).await;
                let context_max_iters = context
                    .get("quality_max_fix_iterations")
                    .await
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32);
                let max_iters = match current_node.raw_attrs.get("max_fix_iterations") {
                    Some(attractor_dot::AttributeValue::Integer(n)) => *n as u32,
                    _ => manifest_max_iters.or(context_max_iters).unwrap_or(3),
                };

                if iteration > max_iters {
                    return Err(AttractorError::Other(format!(
                        "Quality node '{}' exceeded max_fix_iterations ({max_iters}) — aborting pipeline",
                        current_node.id
                    )));
                }

                if iteration >= 2 {
                    // 1-second cooldown between loop iterations
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

                    // Inject structured retry-warning with sentinel tags
                    let last_fp = quality_last_footprint
                        .get(&current_node.id)
                        .cloned()
                        .unwrap_or_default();
                    let warning = format!(
                        "<retry-warning iteration=\"{iteration}\" node=\"{}\" footprint=\"{last_fp}\">\n\
                         Quality stage failed on the previous attempt. Review the failure output \
                         and fix the root cause before proceeding.\n\
                         </retry-warning>",
                        current_node.id
                    );
                    context
                        .set(
                            format!("__quality_retry_warning::{}", current_node.id),
                            serde_json::Value::String(warning),
                        )
                        .await;
                    tracing::warn!(
                        node = %current_node.id,
                        iteration = iteration,
                        max = max_iters,
                        footprint = %last_fp,
                        "Quality retry loop"
                    );
                }
            }

            let outcome = handler.execute(current_node, &context, graph).await?;

            // Extract failure_footprint for the quality loop tracker
            if is_quality && outcome.status == StageStatus::Fail {
                if let Some(results) = outcome
                    .context_updates
                    .get(&format!("{}.results", current_node.id))
                    .and_then(|v| v.as_array())
                {
                    for r in results {
                        if let Some(fp) = r.get("failure_footprint").and_then(|v| v.as_str()) {
                            quality_last_footprint.insert(current_node.id.clone(), fp.to_string());
                            break;
                        }
                    }
                }
            }

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
            if let Some(ref label) = outcome.preferred_label {
                context
                    .set("preferred_label", serde_json::Value::String(label.clone()))
                    .await;
            }

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
                    // Capture the just-completed node before any clear so prev_node_id
                    // is always set to the node that actually executed, not whatever
                    // remains at the tail of a post-clear completed_nodes.
                    let just_completed = current_node.id.clone();

                    // Handle loop_restart
                    if edge.loop_restart {
                        completed_nodes.clear();
                        node_outcomes.clear();
                    }
                    let next_id = edge.to.clone();
                    current_node = graph.node(&next_id).ok_or_else(|| {
                        AttractorError::Other(format!("Edge target '{}' not found", next_id))
                    })?;

                    // Save checkpoint: the *next* node to execute
                    if let Some(logs) = logs_root {
                        let cp = PipelineCheckpoint::with_quality_counters(
                            current_node.id.clone(),
                            completed_nodes.clone(),
                            node_outcomes.clone(),
                            context.snapshot().await,
                            step_count,
                            total_cost,
                            quality_loop_counters.clone(),
                            quality_last_footprint.clone(),
                            Some(just_completed.clone()),
                        );
                        save_checkpoint(&cp, logs).await?;
                    }
                    prev_node_id = Some(just_completed);
                }
                None => {
                    // No outgoing edge and not an exit node
                    if outcome.status == StageStatus::Fail {
                        return Err(AttractorError::HandlerError {
                            handler: handler_type,
                            node: current_node.id.clone(),
                            message: "Handler failed with no outgoing edge".into(),
                        });
                    }
                    break;
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
            total_cost,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
