//! Pipeline execution engine — the core traversal loop.
//!
//! Implements the 5-phase lifecycle: parse, validate, initialize, execute, finalize.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Instant;

use attractor_types::{AttractorError, Context, Outcome, Result, StageStatus};

use crate::checkpoint::{
    clear_checkpoint, load_checkpoint, save_checkpoint, validate_checkpoint, PipelineCheckpoint,
};
use crate::edge_selection::select_edge;
use crate::events::{EventEmitter, PipelineEvent};
use crate::execution_plan::{ExecutionPlan, HandlerIdentity};
use crate::goal_gate::enforce_goal_gates;
use crate::graph::PipelineGraph;
use crate::handler::{default_registry, HandlerExecutionContext, HandlerRegistry};
use crate::retry::retry_delay;
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
    events: Option<EventEmitter>,
}

/// The result of a completed pipeline execution.
#[derive(Debug)]
pub struct PipelineResult {
    pub completed_nodes: Vec<String>,
    pub node_outcomes: HashMap<String, Outcome>,
    pub final_context: HashMap<String, serde_json::Value>,
    pub total_cost: f64,
}

#[derive(Default)]
struct ExecutionProgress {
    step_count: u64,
    total_cost: f64,
    total_handler_attempts: u64,
    active_node_id: Option<String>,
    active_node_attempts: usize,
}

struct CheckpointData<'a> {
    logs_root: Option<&'a Path>,
    completed_nodes: &'a [String],
    node_outcomes: &'a HashMap<String, Outcome>,
    context: &'a Context,
    quality_loop_counters: &'a HashMap<String, u32>,
    quality_last_footprint: &'a HashMap<String, String>,
    previous_node_id: Option<&'a str>,
    execution_fingerprint: Option<&'a str>,
    events: Option<&'a EventEmitter>,
}

impl CheckpointData<'_> {
    async fn save(&self, current_node_id: &str, progress: &ExecutionProgress) -> Result<()> {
        let Some(logs_root) = self.logs_root else {
            return Ok(());
        };
        let mut checkpoint = PipelineCheckpoint::with_quality_counters(
            current_node_id.to_string(),
            self.completed_nodes.to_vec(),
            self.node_outcomes.clone(),
            self.context.snapshot().await,
            progress.step_count,
            progress.total_cost,
            self.quality_loop_counters.clone(),
            self.quality_last_footprint.clone(),
            self.previous_node_id.map(str::to_string),
            self.execution_fingerprint.map(str::to_string),
        );
        checkpoint.total_handler_attempts = progress.total_handler_attempts;
        checkpoint.active_node_id = progress.active_node_id.clone();
        checkpoint.active_node_attempts = progress.active_node_attempts;
        save_checkpoint(&checkpoint, logs_root).await?;
        if let Some(events) = self.events {
            events.emit(PipelineEvent::CheckpointSaved {
                node_id: current_node_id.to_string(),
            });
        }
        Ok(())
    }
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

/// Body text of the quality retry warning injected at iteration >= 2.
///
/// A recorded failure footprint means an objective quality stage genuinely
/// failed on the previous attempt. An empty footprint means the pipeline
/// re-entered the quality node after a downstream review/fixup cycle where
/// every stage passed, and the wording must not claim otherwise.
fn retry_warning_reason(last_footprint: &str) -> &'static str {
    if last_footprint.is_empty() {
        "The pipeline re-entered this quality node for another \
         verification pass after a downstream fixup. Re-run the \
         stages and review the current diff carefully."
    } else {
        "Quality stage failed on the previous attempt. Review the failure output \
         and fix the root cause before proceeding."
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
    fn emit(&self, event: PipelineEvent) {
        if let Some(events) = &self.events {
            events.emit(event);
        }
    }

    /// Create an executor with the given handler registry.
    pub fn new(registry: HandlerRegistry) -> Self {
        Self {
            registry,
            events: None,
        }
    }

    /// Create an executor pre-loaded with the default built-in handlers.
    pub fn with_default_registry() -> Self {
        Self {
            registry: default_registry(),
            events: None,
        }
    }

    /// Observe execution events without making delivery part of pipeline state.
    pub fn with_event_emitter(mut self, events: EventEmitter) -> Self {
        self.events = Some(events);
        self
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

    async fn invoke_node(
        &self,
        node: &crate::graph::PipelineNode,
        resolved: &crate::execution_plan::ResolvedNode,
        configured: &RunConfiguration,
        progress: &mut ExecutionProgress,
        checkpoint: CheckpointData<'_>,
    ) -> Result<Outcome> {
        let handler_type = resolved.handler.as_str();
        let handler =
            self.registry
                .get(handler_type)
                .ok_or_else(|| AttractorError::HandlerError {
                    handler: handler_type.to_string(),
                    node: node.id.clone(),
                    message: format!("No handler registered for type '{handler_type}'"),
                })?;
        let max_attempts = resolved.invocation.max_attempts.get();
        let max_steps = *configured.controls().max_steps().value();
        let max_budget = *configured.controls().max_budget_usd().value();
        if progress.active_node_id.as_deref() != Some(&node.id) {
            progress.active_node_id = Some(node.id.clone());
            progress.active_node_attempts = 0;
        }
        if progress.active_node_attempts >= max_attempts {
            return Err(AttractorError::RetriesExhausted {
                node: node.id.clone(),
                attempts: max_attempts,
            });
        }

        for attempt in progress.active_node_attempts..max_attempts {
            if progress.step_count >= max_steps {
                return Err(AttractorError::Other(format!(
                    "Pipeline exceeded maximum step count ({max_steps}). Use --max-steps to increase."
                )));
            }
            if progress.total_cost > max_budget {
                return Err(AttractorError::Other(format!(
                    "Pipeline exceeded budget (${:.2} > ${:.2}). Use --max-budget-usd to increase.",
                    progress.total_cost, max_budget
                )));
            }
            progress.step_count += 1;
            progress.total_handler_attempts += 1;
            progress.active_node_attempts = attempt + 1;
            checkpoint.save(&node.id, progress).await?;

            self.emit(PipelineEvent::StageStarted {
                node_id: node.id.clone(),
                handler_type: handler_type.to_string(),
            });
            let attempt_started = Instant::now();

            let execution = handler.execute_configured(
                node,
                resolved,
                HandlerExecutionContext::new(checkpoint.context, configured.controls()),
                configured.plan().graph(),
            );
            let result = if let Some(timeout) = resolved.invocation.timeout {
                match tokio::time::timeout(timeout, execution).await {
                    Ok(result) => result,
                    Err(_) => Err(AttractorError::CommandTimeout {
                        timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                    }),
                }
            } else {
                execution.await
            };

            let has_more_attempts = attempt + 1 < max_attempts;
            match result {
                Ok(outcome) if outcome.status == StageStatus::Retry && has_more_attempts => {
                    if let Some(cost) = outcome
                        .context_updates
                        .get(&format!("{}.cost_usd", node.id))
                        .and_then(serde_json::Value::as_f64)
                    {
                        progress.total_cost += cost;
                        checkpoint.save(&node.id, progress).await?;
                    }
                    let next_attempt = attempt + 2;
                    tracing::info!(
                        node = %node.id,
                        attempt = next_attempt,
                        "Retrying stage after retry outcome"
                    );
                    self.emit(PipelineEvent::StageRetrying {
                        node_id: node.id.clone(),
                        attempt: next_attempt,
                    });
                }
                Err(error) if error.is_retryable() && has_more_attempts => {
                    self.emit(PipelineEvent::StageFailed {
                        node_id: node.id.clone(),
                        error: error.to_string(),
                    });
                    let next_attempt = attempt + 2;
                    tracing::warn!(
                        node = %node.id,
                        attempt = next_attempt,
                        error = %error,
                        "Retrying stage after retryable error"
                    );
                    self.emit(PipelineEvent::StageRetrying {
                        node_id: node.id.clone(),
                        attempt: next_attempt,
                    });
                }
                Ok(outcome) => {
                    self.emit(PipelineEvent::StageCompleted {
                        node_id: node.id.clone(),
                        status: status_to_string(outcome.status),
                        duration_ms: u64::try_from(attempt_started.elapsed().as_millis())
                            .unwrap_or(u64::MAX),
                    });
                    return Ok(outcome);
                }
                Err(error) => {
                    self.emit(PipelineEvent::StageFailed {
                        node_id: node.id.clone(),
                        error: error.to_string(),
                    });
                    return Err(error);
                }
            }

            tokio::time::sleep(retry_delay(attempt)).await;
        }

        Err(AttractorError::RetriesExhausted {
            node: node.id.clone(),
            attempts: max_attempts,
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

        let pipeline_started = Instant::now();
        self.emit(PipelineEvent::PipelineStarted {
            pipeline_name: graph.name.clone(),
            node_count: graph.all_nodes().count(),
        });

        let execution_result: Result<PipelineResult> = async {

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
        let mut progress = ExecutionProgress::default();

        // Deterministic fingerprint of the compiled plan this run executes.
        // Recorded in every checkpoint and verified on resume so a materially
        // changed DOT cannot silently inherit stale loop/retry state.
        let execution_fingerprint = plan.fingerprint();

        if let Some(logs) = logs_root {
            if let Some(cp) = load_checkpoint(logs).await? {
                validate_checkpoint(
                    &cp,
                    &execution_fingerprint,
                    &logs.join("checkpoint.json"),
                )?;
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
                progress.step_count = cp.step_count;
                progress.total_cost = cp.total_cost;
                progress.total_handler_attempts = cp.total_handler_attempts;
                progress.active_node_id = cp.active_node_id;
                progress.active_node_attempts = cp.active_node_attempts;
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
            if progress.total_cost > max_budget {
                tracing::error!(cost = progress.total_cost, max = max_budget, "Budget exceeded");
                return Err(AttractorError::Other(format!(
                    "Pipeline exceeded budget (${:.2} > ${:.2}). Use --max-budget-usd to increase.",
                    progress.total_cost, max_budget
                )));
            }

            // Terminal check (exit node)
            if plan.is_exit(&current_node.id) {
                // Check goal gates
                let mut gates = node_outcomes
                    .iter()
                    .filter_map(|(node_id, outcome)| {
                        graph
                            .node(node_id)
                            .filter(|node| node.goal_gate)
                            .map(|_| (node_id, outcome))
                    })
                    .collect::<Vec<_>>();
                gates.sort_by(|left, right| left.0.cmp(right.0));
                for (node_id, outcome) in gates {
                    self.emit(PipelineEvent::GoalGateChecked {
                        node_id: node_id.clone(),
                        satisfied: matches!(
                            outcome.status,
                            StageStatus::Success | StageStatus::PartialSuccess
                        ),
                    });
                }
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
                let outcome = self
                    .invoke_node(
                        current_node,
                        resolved_node,
                        configured,
                        &mut progress,
                        CheckpointData {
                            logs_root,
                            completed_nodes: &completed_nodes,
                            node_outcomes: &node_outcomes,
                            context: &context,
                            quality_loop_counters: &quality_loop_counters,
                            quality_last_footprint: &quality_last_footprint,
                            previous_node_id: prev_node_id.as_deref(),
                            execution_fingerprint: Some(&execution_fingerprint),
                            events: self.events.as_ref(),
                        },
                    )
                    .await?;
                apply_handler_updates(&context, &current_node.id, &outcome).await?;
                if !outcome.context_updates.is_empty() {
                    let mut keys = outcome.context_updates.keys().cloned().collect::<Vec<_>>();
                    keys.sort();
                    self.emit(PipelineEvent::ContextUpdated {
                        node_id: current_node.id.clone(),
                        keys,
                    });
                }
                completed_nodes.push(current_node.id.clone());
                node_outcomes.insert(current_node.id.clone(), outcome);
                progress.active_node_id = None;
                progress.active_node_attempts = 0;
                break;
            }

            // Execute handler
            let resolved_node = plan
                .node(&current_node.id)
                .expect("executed node must be compiled");
            let handler_type = resolved_node.handler.as_str();
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

                    // Inject structured retry-warning with sentinel tags. The
                    // reason text is derived from the recorded failure
                    // footprint so re-entry driven by a downstream review/
                    // fixup cycle is not misreported as an objective failure.
                    let last_fp = quality_last_footprint
                        .get(&current_node.id)
                        .cloned()
                        .unwrap_or_default();
                    let reason = retry_warning_reason(&last_fp);
                    let warning = format!(
                        "<retry-warning iteration=\"{iteration}\" node=\"{}\" footprint=\"{last_fp}\">\n\
                         {reason}\n\
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

            let outcome = self
                .invoke_node(
                    current_node,
                    resolved_node,
                    configured,
                    &mut progress,
                    CheckpointData {
                        logs_root,
                        completed_nodes: &completed_nodes,
                        node_outcomes: &node_outcomes,
                        context: &context,
                        quality_loop_counters: &quality_loop_counters,
                        quality_last_footprint: &quality_last_footprint,
                        previous_node_id: prev_node_id.as_deref(),
                        execution_fingerprint: Some(&execution_fingerprint),
                        events: self.events.as_ref(),
                    },
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
                    progress.total_cost += c;
                    tracing::info!(
                        node = %current_node.id,
                        node_cost = c,
                        total_cost = progress.total_cost,
                        budget_remaining = max_budget - progress.total_cost,
                        "Cost update"
                    );
                }
            }

            // Apply context updates
            apply_handler_updates(&context, &current_node.id, &outcome).await?;
            if !outcome.context_updates.is_empty() {
                let mut keys = outcome.context_updates.keys().cloned().collect::<Vec<_>>();
                keys.sort();
                self.emit(PipelineEvent::ContextUpdated {
                    node_id: current_node.id.clone(),
                    keys,
                });
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
                    self.emit(PipelineEvent::EdgeSelected {
                        from_node: edge.from.clone(),
                        to_node: edge.to.clone(),
                        edge_label: edge.label.clone(),
                    });
                    // Capture the just-completed node before any clear so prev_node_id
                    // is always set to the node that actually executed, not whatever
                    // remains at the tail of a post-clear completed_nodes.
                    let just_completed = current_node.id.clone();

                    // Handle loop_restart
                    if edge.loop_restart {
                        completed_nodes.clear();
                        node_outcomes.clear();
                    }

                    // Explicit outer work-cycle boundary: a completed cycle's
                    // consumed quality retry budget must not leak into the
                    // next cycle. Reset before the checkpoint save so the
                    // fresh budget is what resume restores.
                    if edge.reset_quality_loop_state {
                        quality_loop_counters.clear();
                        quality_last_footprint.clear();
                    }
                    let next_id = edge.to.clone();
                    current_node = graph.node(&next_id).ok_or_else(|| {
                        AttractorError::Other(format!("Edge target '{}' not found", next_id))
                    })?;
                    progress.active_node_id = None;
                    progress.active_node_attempts = 0;

                    // Save checkpoint: the *next* node to execute
                    CheckpointData {
                        logs_root,
                        completed_nodes: &completed_nodes,
                        node_outcomes: &node_outcomes,
                        context: &context,
                        quality_loop_counters: &quality_loop_counters,
                        quality_last_footprint: &quality_last_footprint,
                        previous_node_id: Some(&just_completed),
                        execution_fingerprint: Some(&execution_fingerprint),
                        events: self.events.as_ref(),
                    }
                    .save(&current_node.id, &progress)
                    .await?;
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
                    progress.active_node_id = None;
                    progress.active_node_attempts = 0;
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
            total_cost: progress.total_cost,
        })
        }
        .await;

        match execution_result {
            Ok(result) => {
                self.emit(PipelineEvent::PipelineCompleted {
                    pipeline_name: graph.name.clone(),
                    completed_nodes: result.completed_nodes.clone(),
                    duration_ms: u64::try_from(pipeline_started.elapsed().as_millis())
                        .unwrap_or(u64::MAX),
                });
                Ok(result)
            }
            Err(error) => {
                self.emit(PipelineEvent::PipelineFailed {
                    pipeline_name: graph.name.clone(),
                    error: error.to_string(),
                });
                Err(error)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
