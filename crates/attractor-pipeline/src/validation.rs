//! Pipeline validation: lint rules and diagnostics.
//!
//! Performs canonical semantic compilation followed by nine structural checks
//! for a [`PipelineGraph`]. Call [`validate`] for advisory diagnostics or
//! [`validate_or_raise`] to fail on the first `Error`-severity issue.

use std::collections::{HashSet, VecDeque};

use crate::graph::PipelineGraph;
use crate::parse_condition;
use crate::{ExecutionPlan, LlmProvider, SemanticDiagnostic, SemanticDiagnosticKind};

// ---------------------------------------------------------------------------
// Diagnostic types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub rule: String,
    pub severity: Severity,
    pub message: String,
    pub node_id: Option<String>,
    pub edge: Option<(String, String)>,
    pub fix: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Info,
}

// ---------------------------------------------------------------------------
// LintRule trait
// ---------------------------------------------------------------------------

pub trait LintRule: Send + Sync {
    fn name(&self) -> &str;
    fn apply(&self, graph: &PipelineGraph) -> Vec<Diagnostic>;
}

const VALID_FIDELITY_PREFIXES: &[&str] = &["full", "truncate", "compact", "summary"];

fn is_valid_fidelity(val: &str) -> bool {
    let val = val.trim();
    if val.is_empty() {
        return false;
    }
    // "summary:low", "summary:medium", "truncate:5" etc. or bare prefix
    if let Some((prefix, _suffix)) = val.split_once(':') {
        VALID_FIDELITY_PREFIXES.contains(&prefix)
    } else if let Some((prefix, _suffix)) = val.split_once('(') {
        // Also accept "truncate(5)" parenthesized syntax
        VALID_FIDELITY_PREFIXES.contains(&prefix)
    } else {
        VALID_FIDELITY_PREFIXES.contains(&val)
    }
}

// ---------------------------------------------------------------------------
// Rules
// ---------------------------------------------------------------------------

struct EdgeTargetExistsRule;
impl LintRule for EdgeTargetExistsRule {
    fn name(&self) -> &str {
        "edge_target_exists"
    }
    fn apply(&self, graph: &PipelineGraph) -> Vec<Diagnostic> {
        graph
            .all_edges()
            .iter()
            .filter(|e| graph.node(&e.to).is_none())
            .map(|e| Diagnostic {
                rule: self.name().into(),
                severity: Severity::Error,
                message: format!(
                    "Edge {} -> {} references non-existent target '{}'",
                    e.from, e.to, e.to
                ),
                node_id: None,
                edge: Some((e.from.clone(), e.to.clone())),
                fix: Some(format!("Add node '{}' or fix the edge target", e.to)),
            })
            .collect()
    }
}

struct ConditionSyntaxRule;
impl LintRule for ConditionSyntaxRule {
    fn name(&self) -> &str {
        "condition_syntax"
    }
    fn apply(&self, graph: &PipelineGraph) -> Vec<Diagnostic> {
        graph
            .all_edges()
            .iter()
            .filter_map(|e| {
                let cond = e.condition.as_deref()?;
                match parse_condition(cond) {
                    Ok(_) => None,
                    Err(err) => Some(Diagnostic {
                        rule: self.name().into(),
                        severity: Severity::Error,
                        message: format!(
                            "Edge {} -> {} has invalid condition '{}': {}",
                            e.from, e.to, cond, err
                        ),
                        node_id: None,
                        edge: Some((e.from.clone(), e.to.clone())),
                        fix: Some("Fix the condition expression syntax".into()),
                    }),
                }
            })
            .collect()
    }
}

struct FidelityValidRule;
impl LintRule for FidelityValidRule {
    fn name(&self) -> &str {
        "fidelity_valid"
    }
    fn apply(&self, graph: &PipelineGraph) -> Vec<Diagnostic> {
        let mut diags = Vec::new();
        for node in graph.all_nodes() {
            if let Some(ref f) = node.fidelity {
                if !is_valid_fidelity(f) {
                    diags.push(Diagnostic {
                        rule: self.name().into(),
                        severity: Severity::Warning,
                        message: format!("Node '{}' has invalid fidelity value '{f}'", node.id),
                        node_id: Some(node.id.clone()),
                        edge: None,
                        fix: Some(
                            "Use one of: full, truncate, compact, summary, summary:<level>".into(),
                        ),
                    });
                }
            }
        }
        for edge in graph.all_edges() {
            if let Some(ref f) = edge.fidelity {
                if !is_valid_fidelity(f) {
                    diags.push(Diagnostic {
                        rule: self.name().into(),
                        severity: Severity::Warning,
                        message: format!(
                            "Edge {} -> {} has invalid fidelity value '{f}'",
                            edge.from, edge.to
                        ),
                        node_id: None,
                        edge: Some((edge.from.clone(), edge.to.clone())),
                        fix: Some(
                            "Use one of: full, truncate, compact, summary, summary:<level>".into(),
                        ),
                    });
                }
            }
        }
        diags
    }
}

struct RetryTargetExistsRule;
impl LintRule for RetryTargetExistsRule {
    fn name(&self) -> &str {
        "retry_target_exists"
    }
    fn apply(&self, graph: &PipelineGraph) -> Vec<Diagnostic> {
        let mut diags = Vec::new();
        for node in graph.all_nodes() {
            if let Some(ref target) = node.retry_target {
                if graph.node(target).is_none() {
                    diags.push(Diagnostic {
                        rule: self.name().into(),
                        severity: Severity::Warning,
                        message: format!(
                            "Node '{}' has retry_target '{}' which does not exist",
                            node.id, target
                        ),
                        node_id: Some(node.id.clone()),
                        edge: None,
                        fix: Some(format!("Add node '{target}' or fix retry_target")),
                    });
                }
            }
            if let Some(ref target) = node.fallback_retry_target {
                if graph.node(target).is_none() {
                    diags.push(Diagnostic {
                        rule: self.name().into(),
                        severity: Severity::Warning,
                        message: format!(
                            "Node '{}' has fallback_retry_target '{}' which does not exist",
                            node.id, target
                        ),
                        node_id: Some(node.id.clone()),
                        edge: None,
                        fix: Some(format!("Add node '{target}' or fix fallback_retry_target")),
                    });
                }
            }
        }
        diags
    }
}

struct GoalGateHasRetryRule;
impl LintRule for GoalGateHasRetryRule {
    fn name(&self) -> &str {
        "goal_gate_has_retry"
    }
    fn apply(&self, graph: &PipelineGraph) -> Vec<Diagnostic> {
        graph
            .all_nodes()
            .filter(|n| n.goal_gate && n.retry_target.is_none())
            .map(|n| Diagnostic {
                rule: self.name().into(),
                severity: Severity::Warning,
                message: format!("Node '{}' has goal_gate=true but no retry_target", n.id),
                node_id: Some(n.id.clone()),
                edge: None,
                fix: Some("Add a retry_target attribute so the goal gate can retry".into()),
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Run all built-in lint rules and return collected diagnostics.
pub fn validate(graph: &PipelineGraph) -> Vec<Diagnostic> {
    match ExecutionPlan::compile(graph.clone()) {
        Ok(plan) => validate_plan(&plan),
        Err(error) => {
            let mut diagnostics = validate_nonsemantic_structure(graph);
            if let Ok(compilation) =
                ExecutionPlan::compile_for_generation(graph.clone(), LlmProvider::Claude)
            {
                diagnostics.extend(validate_plan_structure(&compilation.plan));
            }
            diagnostics.extend(error.diagnostics.into_iter().map(semantic_diagnostic));
            diagnostics
        }
    }
}

/// Run structural lint rules against an already compiled semantic plan.
pub fn validate_plan(plan: &ExecutionPlan) -> Vec<Diagnostic> {
    let mut diagnostics = validate_nonsemantic_structure(plan.graph());
    diagnostics.extend(validate_plan_structure(plan));
    diagnostics
}

fn validate_nonsemantic_structure(graph: &PipelineGraph) -> Vec<Diagnostic> {
    let rules: Vec<Box<dyn LintRule>> = vec![
        Box::new(EdgeTargetExistsRule),
        Box::new(ConditionSyntaxRule),
        Box::new(FidelityValidRule),
        Box::new(RetryTargetExistsRule),
        Box::new(GoalGateHasRetryRule),
    ];

    let mut diagnostics = Vec::new();
    for rule in &rules {
        diagnostics.extend(rule.apply(graph));
    }
    diagnostics
}

fn validate_plan_structure(plan: &ExecutionPlan) -> Vec<Diagnostic> {
    let graph = plan.graph();
    let mut diagnostics = Vec::new();

    if graph
        .all_edges()
        .iter()
        .any(|edge| edge.to == plan.start_id())
    {
        diagnostics.push(Diagnostic {
            rule: "start_no_incoming".into(),
            severity: Severity::Error,
            message: format!("Start node '{}' has incoming edges", plan.start_id()),
            node_id: Some(plan.start_id().to_string()),
            edge: None,
            fix: Some("Remove edges pointing to the start node".into()),
        });
    }

    for exit_id in plan.exit_ids() {
        if !plan.outgoing_edges(exit_id).is_empty() {
            diagnostics.push(Diagnostic {
                rule: "exit_no_outgoing".into(),
                severity: Severity::Error,
                message: format!("Terminal node '{exit_id}' has outgoing edges"),
                node_id: Some(exit_id.clone()),
                edge: None,
                fix: Some(format!("Remove outgoing edges from '{exit_id}'")),
            });
        }
    }

    let mut visited = HashSet::new();
    let mut queue = VecDeque::from([plan.start_id().to_string()]);
    while let Some(current) = queue.pop_front() {
        if !visited.insert(current.clone()) {
            continue;
        }
        queue.extend(
            plan.outgoing_edges(&current)
                .iter()
                .map(|edge| edge.to.clone()),
        );
    }
    let mut unreachable = plan
        .all_nodes()
        .map(|node| node.node_id.as_str())
        .filter(|node_id| !visited.contains(*node_id))
        .collect::<Vec<_>>();
    unreachable.sort_unstable();
    diagnostics.extend(unreachable.into_iter().map(|node_id| Diagnostic {
        rule: "reachability".into(),
        severity: Severity::Error,
        message: format!("Node '{node_id}' is not reachable from the start node"),
        node_id: Some(node_id.to_string()),
        edge: None,
        fix: Some(format!("Add an edge leading to '{node_id}' or remove it")),
    }));

    let mut llm_nodes = plan
        .all_nodes()
        .filter(|node| node.handler == crate::HandlerIdentity::Codergen)
        .filter_map(|node| plan.source_node(&node.node_id))
        .filter(|node| node.prompt.is_none() && node.label == node.id)
        .collect::<Vec<_>>();
    llm_nodes.sort_by(|left, right| left.id.cmp(&right.id));
    diagnostics.extend(llm_nodes.into_iter().map(|node| Diagnostic {
        rule: "prompt_on_llm_nodes".into(),
        severity: Severity::Warning,
        message: format!(
            "Node '{}' (handler=codergen) has no prompt and label matches id",
            node.id
        ),
        node_id: Some(node.id.clone()),
        edge: None,
        fix: Some("Add a prompt or a descriptive label attribute".into()),
    }));

    diagnostics
}

fn semantic_diagnostic(diagnostic: SemanticDiagnostic) -> Diagnostic {
    let rule = match diagnostic.kind {
        SemanticDiagnosticKind::InvalidAttributeType => "attribute_type",
        SemanticDiagnosticKind::MissingProvider => "provider_required",
        SemanticDiagnosticKind::UnknownProvider => "provider_valid",
        SemanticDiagnosticKind::MissingStart | SemanticDiagnosticKind::MultipleStarts => {
            "start_node"
        }
        SemanticDiagnosticKind::MissingExit | SemanticDiagnosticKind::MultipleExits => {
            "terminal_node"
        }
        SemanticDiagnosticKind::ConflictingRoleSignals => "semantic_conflict",
        SemanticDiagnosticKind::ConflictingAttributeAliases => "attribute_alias_conflict",
        SemanticDiagnosticKind::UnknownShape | SemanticDiagnosticKind::UnknownHandler => {
            "semantic_unknown"
        }
        SemanticDiagnosticKind::HandlerCapabilityMismatch => "handler_registry",
        SemanticDiagnosticKind::TransformError => "transform",
    };
    Diagnostic {
        rule: rule.into(),
        severity: Severity::Error,
        message: diagnostic.message,
        node_id: diagnostic.node_id,
        edge: None,
        fix: Some(diagnostic.fix),
    }
}

/// Run all lint rules; return `Err` if any `Error`-severity diagnostic found.
pub fn validate_or_raise(graph: &PipelineGraph) -> attractor_types::Result<Vec<Diagnostic>> {
    let diagnostics = validate(graph);
    let errors: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .collect();
    if !errors.is_empty() {
        let messages: Vec<_> = errors.iter().map(|d| d.message.clone()).collect();
        return Err(attractor_types::AttractorError::ValidationError(
            messages.join("; "),
        ));
    }
    Ok(diagnostics)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "validation_tests.rs"]
mod tests;
