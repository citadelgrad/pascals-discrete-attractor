//! Pipeline execution engine — the core traversal loop.
//!
//! Implements the 5-phase lifecycle: parse, validate, initialize, execute, finalize.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use attractor_types::{AttractorError, Context, Outcome, Result, StageStatus};

use crate::checkpoint::{clear_checkpoint, load_checkpoint, save_checkpoint, PipelineCheckpoint};
use crate::edge_selection::select_edge;
use crate::execution_plan::{ExecutionPlan, HandlerIdentity};
use crate::goal_gate::enforce_goal_gates;
use crate::graph::PipelineGraph;
use crate::handler::{default_registry, HandlerExecutionContext, HandlerRegistry};
use crate::run_configuration::{
    is_reserved_key, ClaudeExecutionOptions, ExecutionOptions, RunConfiguration,
};
use crate::validation::validate_plan;

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

fn prepare(plan: &ExecutionPlan, options: ExecutionOptions) -> Result<RunConfiguration> {
    RunConfiguration::prepare(plan.clone(), options)
        .map_err(|error| AttractorError::ValidationError(error.to_string()))
}

async fn apply_handler_updates(context: &Context, node_id: &str, outcome: &Outcome) -> Result<()> {
    let mut reserved_updates = outcome
        .context_updates
        .keys()
        .filter(|key| is_reserved_key(key))
        .cloned()
        .collect::<Vec<_>>();
    reserved_updates.sort();
    if !reserved_updates.is_empty() {
        return Err(AttractorError::ValidationError(format!(
            "handler '{node_id}' attempted to write reserved context key(s): {}",
            reserved_updates.join(", ")
        )));
    }
    context.apply_updates(outcome.context_updates.clone()).await;
    Ok(())
}

async fn legacy_options(context: Context) -> Result<(ExecutionOptions, Context)> {
    let snapshot = context.snapshot().await;
    let setting_sources = snapshot
        .get("codergen.claude.setting_sources")
        .and_then(|value| value.as_str())
        .map(|sources| {
            sources
                .split(',')
                .map(str::trim)
                .filter(|source| !source.is_empty())
                .map(attractor_quality::ClaudeSettingSource::from_str)
                .collect::<std::result::Result<Vec<_>, _>>()
        })
        .transpose()
        .map_err(AttractorError::ValidationError)?;
    let settings_mode = snapshot
        .get("codergen.claude.settings_mode")
        .and_then(|value| value.as_str())
        .map(attractor_quality::ClaudeSettingsMode::from_str)
        .transpose()
        .map_err(AttractorError::ValidationError)?;
    let plugin_dirs = snapshot
        .get("codergen.claude.plugin_dirs")
        .and_then(|value| value.as_array())
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str().map(PathBuf::from))
                .collect()
        });
    let string = |key: &str| {
        snapshot
            .get(key)
            .and_then(|value| value.as_str())
            .map(str::to_owned)
    };
    let options = ExecutionOptions {
        dry_run: snapshot.get("dry_run").and_then(|value| value.as_bool()),
        max_steps: snapshot.get("max_steps").and_then(|value| value.as_u64()),
        max_budget_usd: snapshot
            .get("max_budget_usd")
            .and_then(|value| value.as_f64()),
        workdir: string("workdir").map(PathBuf::from),
        quality_disabled: snapshot
            .get("quality_disabled")
            .and_then(|value| value.as_bool()),
        quality_max_fix_iterations: snapshot
            .get("quality_max_fix_iterations")
            .and_then(|value| value.as_u64())
            .and_then(|value| u32::try_from(value).ok()),
        claude: ClaudeExecutionOptions {
            settings_mode,
            setting_sources,
            settings: string("codergen.claude.settings"),
            tools: string("codergen.claude.tools"),
            agents: string("codergen.claude.agents"),
            plugin_dirs,
            mcp_config: string("codergen.claude.mcp_config"),
        },
    };
    let workflow = Context::new();
    workflow
        .apply_updates(
            snapshot
                .into_iter()
                .filter(|(key, _)| !is_reserved_key(key))
                .collect(),
        )
        .await;
    Ok((options, workflow))
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
        let plan = self.compile(graph)?;
        self.run_plan(&plan).await
    }

    /// Run the pipeline with a pre-seeded context (e.g. workdir, dry_run).
    pub async fn run_with_context(
        &self,
        graph: &PipelineGraph,
        context: Context,
    ) -> Result<PipelineResult> {
        let plan = self.compile(graph)?;
        self.run_plan_with_context(&plan, context).await
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
        let plan = self.compile(graph)?;
        self.run_plan_with_checkpoint(&plan, context, logs_root)
            .await
    }

    pub async fn run_plan(&self, plan: &ExecutionPlan) -> Result<PipelineResult> {
        let configured = prepare(plan, ExecutionOptions::default())?;
        self.run_configuration(&configured).await
    }

    pub async fn run_plan_with_context(
        &self,
        plan: &ExecutionPlan,
        context: Context,
    ) -> Result<PipelineResult> {
        let (options, workflow) = legacy_options(context).await?;
        let configured = prepare(plan, options)?;
        self.run_configuration_with_context(&configured, workflow)
            .await
    }

    pub async fn run_plan_with_checkpoint(
        &self,
        plan: &ExecutionPlan,
        context: Context,
        logs_root: &Path,
    ) -> Result<PipelineResult> {
        let (options, workflow) = legacy_options(context).await?;
        let configured = prepare(plan, options)?;
        self.run_configuration_with_checkpoint(&configured, workflow, logs_root)
            .await
    }

    pub async fn run_configuration(&self, configured: &RunConfiguration) -> Result<PipelineResult> {
        self.run_configuration_with_context(configured, Context::new())
            .await
    }

    pub async fn run_configuration_with_context(
        &self,
        configured: &RunConfiguration,
        context: Context,
    ) -> Result<PipelineResult> {
        self.run_inner(configured, context, None).await
    }

    pub async fn run_configuration_with_checkpoint(
        &self,
        configured: &RunConfiguration,
        context: Context,
        logs_root: &Path,
    ) -> Result<PipelineResult> {
        self.run_inner(configured, context, Some(logs_root)).await
    }

    fn compile(&self, graph: &PipelineGraph) -> Result<ExecutionPlan> {
        ExecutionPlan::compile_with_registry(graph.clone(), &self.registry).map_err(|error| {
            AttractorError::ValidationError(
                error
                    .diagnostics
                    .iter()
                    .map(|diagnostic| diagnostic.message.as_str())
                    .collect::<Vec<_>>()
                    .join("; "),
            )
        })
    }

    /// Core execution loop. When `logs_root` is `Some`, checkpoints are
    /// saved after each node and an existing checkpoint triggers resume.
    async fn run_inner(
        &self,
        configured: &RunConfiguration,
        context: Context,
        logs_root: Option<&Path>,
    ) -> Result<PipelineResult> {
        let plan = configured.plan();
        plan.ensure_registry_compatible(&self.registry)
            .map_err(|error| {
                AttractorError::ValidationError(
                    error
                        .diagnostics
                        .iter()
                        .map(|diagnostic| diagnostic.message.as_str())
                        .collect::<Vec<_>>()
                        .join("; "),
                )
            })?;

        let graph = plan.graph();
        // Phase 2: Validate
        let diagnostics = validate_plan(plan);
        let errors = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity == crate::validation::Severity::Error)
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>();
        if !errors.is_empty() {
            return Err(AttractorError::ValidationError(errors.join("; ")));
        }

        let mut reserved_input = context
            .snapshot()
            .await
            .into_keys()
            .filter(|key| is_reserved_key(key))
            .collect::<Vec<_>>();
        reserved_input.sort();
        if !reserved_input.is_empty() {
            return Err(AttractorError::ValidationError(format!(
                "canonical workflow input contains reserved context key(s): {}",
                reserved_input.join(", ")
            )));
        }

        // Phase 3: Initialize (merge graph attrs into existing context)
        for (key, value) in configured.graph_context_defaults() {
            context.set(key, value.clone()).await;
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
            .node(plan.start_id())
            .expect("compiled start node must exist in source graph");
        let mut current_node = start;

        // Safety limits from the immutable run configuration.
        let max_budget = *configured.controls().max_budget_usd().value();
        let max_steps = *configured.controls().max_steps().value();
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
                let mut restored = cp.context_snapshot;
                restored.retain(|key, _| !is_reserved_key(key));
                context.apply_updates(restored).await;
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
            if plan.is_exit(&current_node.id) {
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
                let resolved_node = plan
                    .node(&current_node.id)
                    .expect("executed node must be compiled");
                let handler_type = resolved_node.handler.as_str();
                let handler = self.registry.get(handler_type).ok_or_else(|| {
                    AttractorError::HandlerError {
                        handler: handler_type.to_string(),
                        node: current_node.id.clone(),
                        message: format!("No handler registered for type '{}'", handler_type),
                    }
                })?;
                let outcome = handler
                    .execute_configured(
                        current_node,
                        resolved_node,
                        HandlerExecutionContext::new(&context, configured.controls()),
                        graph,
                    )
                    .await?;
                apply_handler_updates(&context, &current_node.id, &outcome).await?;
                completed_nodes.push(current_node.id.clone());
                node_outcomes.insert(current_node.id.clone(), outcome);
                break;
            }

            // Execute handler
            let resolved_node = plan
                .node(&current_node.id)
                .expect("executed node must be compiled");
            let handler_type = resolved_node.handler.as_str();
            let handler =
                self.registry
                    .get(handler_type)
                    .ok_or_else(|| AttractorError::HandlerError {
                        handler: handler_type.to_string(),
                        node: current_node.id.clone(),
                        message: format!("No handler registered for type '{}'", handler_type),
                    })?;

            // Quality loop control: track entries and enforce max_fix_iterations
            let is_quality = resolved_node.handler == HandlerIdentity::Quality;
            if is_quality {
                let upstream = prev_node_id.as_deref().unwrap_or("__start__");
                let loop_key = format!("{}::{}", current_node.id, upstream);
                let counter = quality_loop_counters.entry(loop_key).or_insert(0);
                *counter += 1;
                let iteration = *counter;

                let max_iters = *configured
                    .controls()
                    .quality_max_fix_iterations(&current_node.id)
                    .value();

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
                    tracing::warn!(
                        node = %current_node.id,
                        iteration = iteration,
                        max = max_iters,
                        footprint = %last_fp,
                        warning = %warning,
                        "Quality retry loop"
                    );
                }
            }

            let outcome = handler
                .execute_configured(
                    current_node,
                    resolved_node,
                    HandlerExecutionContext::new(&context, configured.controls()),
                    graph,
                )
                .await?;

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
            apply_handler_updates(&context, &current_node.id, &outcome).await?;

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
                            handler: handler_type.to_string(),
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
