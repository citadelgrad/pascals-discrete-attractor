//! Pipeline execution engine, node handlers, validation, and edge selection.
//!
//! This crate implements the core Attractor pipeline runner: DOT graph traversal,
//! handler dispatch, edge selection, goal gate enforcement, checkpoint/resume,
//! and canonical semantic compilation followed by nine structural checks.

pub mod checkpoint;
pub mod condition;
pub mod edge_selection;
pub mod engine;
pub mod events;
pub mod execution_plan;
pub mod goal_gate;
pub mod graph;
pub mod handler;
pub mod handlers;
pub mod interviewer;
pub mod preflight;
pub mod provider_defaults;
pub mod retry;
pub mod run_configuration;
pub mod stylesheet;
pub mod transforms;
pub mod validation;

pub use attractor_quality::{ClaudeSettingSource, ClaudeSettingsMode};
pub use checkpoint::{clear_checkpoint, load_checkpoint, save_checkpoint, PipelineCheckpoint};
pub use condition::{evaluate_condition, parse_condition, Clause, ConditionExpr, Operator};
pub use edge_selection::select_edge;
pub use engine::{PipelineExecutor, PipelineResult, DEFAULT_MAX_BUDGET_USD};
pub use events::{EventEmitter, PipelineEvent};
pub use execution_plan::{
    ExecutionPlan, HandlerIdentity, LlmProvider, MissingProviderPolicy, PlanCompilation,
    ResolvedNode, ResolvedNodeKind, SemanticDiagnostic, SemanticDiagnosticKind, SemanticError,
};
pub use goal_gate::{check_goal_gates, enforce_goal_gates, GoalGateResult};
pub use graph::{PipelineEdge, PipelineGraph, PipelineNode};
pub use handler::{
    default_registry, default_registry_with_interviewer, ConditionalHandler, DynHandler,
    ExitHandler, HandlerExecutionContext, HandlerRegistry, NodeHandler, ProviderNodeHandler,
    ResolvedNodeHandler, StartHandler,
};
pub use handlers::wait_human::WaitHumanHandler;
pub use handlers::{
    CodergenHandler, FanInHandler, ManagerLoopHandler, ParallelHandler, QualityHandler, ToolHandler,
};
pub use interviewer::{
    Answer, AutoApproveInterviewer, ConsoleInterviewer, Interviewer, Question, RecordingInterviewer,
};
pub use preflight::{
    run as preflight_run, run_configuration as preflight_run_configuration,
    run_plan as preflight_run_plan, run_plan_with_budget as preflight_run_plan_with_budget,
    run_with_budget as preflight_run_with_budget, PreflightFinding, Severity as PreflightSeverity,
};
pub use provider_defaults::fill_missing_llm_providers;
pub use retry::{execute_with_retry, BackoffPolicy};
pub use run_configuration::{
    ClaudeExecutionOptions, ConfigurationError, ConfigurationSource, ExecutionOptions,
    ResolvedClaudeConfig, ResolvedConfig, ResolvedValue, RunConfiguration,
};
pub use stylesheet::{apply_stylesheet, parse_stylesheet, Declaration, Rule, Selector, Stylesheet};
pub use transforms::{apply_transforms, expand_variables};
pub use validation::{validate, validate_or_raise, validate_plan, Diagnostic, LintRule, Severity};
