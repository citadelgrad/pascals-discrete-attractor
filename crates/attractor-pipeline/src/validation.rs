//! Pipeline validation: lint rules and diagnostics.
//!
//! Provides 12 built-in rules that check structural and semantic correctness of
//! a [`PipelineGraph`].  Call [`validate`] for advisory diagnostics or
//! [`validate_or_raise`] to fail on the first `Error`-severity issue.

use std::collections::{HashSet, VecDeque};

use crate::graph::PipelineGraph;
use crate::parse_condition;

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

// ---------------------------------------------------------------------------
// Helper predicates
// ---------------------------------------------------------------------------

fn is_start_node(id: &str, shape: &str) -> bool {
    shape == "Mdiamond" || id == "start" || id == "Start"
}

fn is_terminal_node(id: &str, shape: &str) -> bool {
    shape == "Msquare" || id == "exit" || id == "end" || id == "done"
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

fn is_llm_node(shape: &str) -> bool {
    matches!(shape, "box" | "cds" | "component" | "note")
}

// ---------------------------------------------------------------------------
// Rules
// ---------------------------------------------------------------------------

struct StartNodeRule;
impl LintRule for StartNodeRule {
    fn name(&self) -> &str { "start_node" }
    fn apply(&self, graph: &PipelineGraph) -> Vec<Diagnostic> {
        let starts: Vec<_> = graph
            .all_nodes()
            .filter(|n| is_start_node(&n.id, &n.shape))
            .collect();
        if starts.is_empty() {
            vec![Diagnostic {
                rule: self.name().into(),
                severity: Severity::Error,
                message: "Pipeline has no start node (shape=Mdiamond or id start/Start)".into(),
                node_id: None,
                edge: None,
                fix: Some("Add a node with shape=\"Mdiamond\" or id=\"start\"".into()),
            }]
        } else if starts.len() > 1 {
            vec![Diagnostic {
                rule: self.name().into(),
                severity: Severity::Error,
                message: format!(
                    "Pipeline has {} start nodes: {}; expected exactly one",
                    starts.len(),
                    starts.iter().map(|n| n.id.as_str()).collect::<Vec<_>>().join(", ")
                ),
                node_id: None,
                edge: None,
                fix: Some("Remove extra start nodes so only one remains".into()),
            }]
        } else {
            vec![]
        }
    }
}

struct TerminalNodeRule;
impl LintRule for TerminalNodeRule {
    fn name(&self) -> &str { "terminal_node" }
    fn apply(&self, graph: &PipelineGraph) -> Vec<Diagnostic> {
        let has_terminal = graph
            .all_nodes()
            .any(|n| is_terminal_node(&n.id, &n.shape));
        if !has_terminal {
            vec![Diagnostic {
                rule: self.name().into(),
                severity: Severity::Error,
                message: "Pipeline has no terminal node (shape=Msquare or id exit/end/done)".into(),
                node_id: None,
                edge: None,
                fix: Some("Add a node with shape=\"Msquare\" or id=\"done\"".into()),
            }]
        } else {
            vec![]
        }
    }
}

struct ReachabilityRule;
impl LintRule for ReachabilityRule {
    fn name(&self) -> &str { "reachability" }
    fn apply(&self, graph: &PipelineGraph) -> Vec<Diagnostic> {
        let start = graph.start_node();
        let start_id = match start {
            Some(n) => n.id.clone(),
            None => return vec![], // StartNodeRule will catch this
        };

        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        visited.insert(start_id.clone());
        queue.push_back(start_id);

        while let Some(current) = queue.pop_front() {
            for edge in graph.outgoing_edges(&current) {
                if visited.insert(edge.to.clone()) {
                    queue.push_back(edge.to.clone());
                }
            }
        }

        let all_ids: HashSet<_> = graph.all_nodes().map(|n| n.id.clone()).collect();
        let unreachable: Vec<_> = all_ids.difference(&visited).collect();

        unreachable
            .into_iter()
            .map(|id| Diagnostic {
                rule: self.name().into(),
                severity: Severity::Error,
                message: format!("Node '{id}' is not reachable from the start node"),
                node_id: Some(id.clone()),
                edge: None,
                fix: Some(format!("Add an edge leading to '{id}' or remove it")),
            })
            .collect()
    }
}

struct EdgeTargetExistsRule;
impl LintRule for EdgeTargetExistsRule {
    fn name(&self) -> &str { "edge_target_exists" }
    fn apply(&self, graph: &PipelineGraph) -> Vec<Diagnostic> {
        graph
            .all_edges()
            .iter()
            .filter(|e| graph.node(&e.to).is_none())
            .map(|e| Diagnostic {
                rule: self.name().into(),
                severity: Severity::Error,
                message: format!("Edge {} -> {} references non-existent target '{}'", e.from, e.to, e.to),
                node_id: None,
                edge: Some((e.from.clone(), e.to.clone())),
                fix: Some(format!("Add node '{}' or fix the edge target", e.to)),
            })
            .collect()
    }
}

struct StartNoIncomingRule;
impl LintRule for StartNoIncomingRule {
    fn name(&self) -> &str { "start_no_incoming" }
    fn apply(&self, graph: &PipelineGraph) -> Vec<Diagnostic> {
        let start = match graph.start_node() {
            Some(n) => n.id.clone(),
            None => return vec![],
        };
        let has_incoming = graph.all_edges().iter().any(|e| e.to == start);
        if has_incoming {
            vec![Diagnostic {
                rule: self.name().into(),
                severity: Severity::Error,
                message: format!("Start node '{start}' has incoming edges"),
                node_id: Some(start),
                edge: None,
                fix: Some("Remove edges pointing to the start node".into()),
            }]
        } else {
            vec![]
        }
    }
}

struct ExitNoOutgoingRule;
impl LintRule for ExitNoOutgoingRule {
    fn name(&self) -> &str { "exit_no_outgoing" }
    fn apply(&self, graph: &PipelineGraph) -> Vec<Diagnostic> {
        graph
            .all_nodes()
            .filter(|n| is_terminal_node(&n.id, &n.shape))
            .filter(|n| !graph.outgoing_edges(&n.id).is_empty())
            .map(|n| Diagnostic {
                rule: self.name().into(),
                severity: Severity::Error,
                message: format!("Terminal node '{}' has outgoing edges", n.id),
                node_id: Some(n.id.clone()),
                edge: None,
                fix: Some(format!("Remove outgoing edges from '{}'", n.id)),
            })
            .collect()
    }
}

struct ConditionSyntaxRule;
impl LintRule for ConditionSyntaxRule {
    fn name(&self) -> &str { "condition_syntax" }
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
    fn name(&self) -> &str { "fidelity_valid" }
    fn apply(&self, graph: &PipelineGraph) -> Vec<Diagnostic> {
        let mut diags = Vec::new();
        for node in graph.all_nodes() {
            if let Some(ref f) = node.fidelity {
                if !is_valid_fidelity(f) {
                    diags.push(Diagnostic {
                        rule: self.name().into(),
                        severity: Severity::Warning,
                        message: format!(
                            "Node '{}' has invalid fidelity value '{f}'",
                            node.id
                        ),
                        node_id: Some(node.id.clone()),
                        edge: None,
                        fix: Some("Use one of: full, truncate, compact, summary, summary:<level>".into()),
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
                        fix: Some("Use one of: full, truncate, compact, summary, summary:<level>".into()),
                    });
                }
            }
        }
        diags
    }
}

struct RetryTargetExistsRule;
impl LintRule for RetryTargetExistsRule {
    fn name(&self) -> &str { "retry_target_exists" }
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
    fn name(&self) -> &str { "goal_gate_has_retry" }
    fn apply(&self, graph: &PipelineGraph) -> Vec<Diagnostic> {
        graph
            .all_nodes()
            .filter(|n| n.goal_gate && n.retry_target.is_none())
            .map(|n| Diagnostic {
                rule: self.name().into(),
                severity: Severity::Warning,
                message: format!(
                    "Node '{}' has goal_gate=true but no retry_target",
                    n.id
                ),
                node_id: Some(n.id.clone()),
                edge: None,
                fix: Some("Add a retry_target attribute so the goal gate can retry".into()),
            })
            .collect()
    }
}

struct ProviderValidRule;
impl LintRule for ProviderValidRule {
    fn name(&self) -> &str {
        "provider_valid"
    }
    fn apply(&self, graph: &PipelineGraph) -> Vec<Diagnostic> {
        const KNOWN: &[&str] = &["claude", "anthropic", "codex", "openai", "gemini", "google"];
        graph
            .all_nodes()
            .filter(|n| is_llm_node(&n.shape))
            .filter_map(|n| {
                let provider = n.llm_provider.as_deref()?;
                if KNOWN.contains(&provider) {
                    return None;
                }
                Some(Diagnostic {
                    rule: self.name().into(),
                    severity: Severity::Warning,
                    message: format!(
                        "Node '{}' has unknown llm_provider '{}'; known: claude, codex, gemini",
                        n.id, provider
                    ),
                    node_id: Some(n.id.clone()),
                    edge: None,
                    fix: Some(
                        "Use one of: claude, codex, gemini (aliases: anthropic, openai, google)"
                            .into(),
                    ),
                })
            })
            .collect()
    }
}

struct PromptOnLlmNodesRule;
impl LintRule for PromptOnLlmNodesRule {
    fn name(&self) -> &str { "prompt_on_llm_nodes" }
    fn apply(&self, graph: &PipelineGraph) -> Vec<Diagnostic> {
        graph
            .all_nodes()
            .filter(|n| is_llm_node(&n.shape))
            .filter(|n| {
                // Skip start/terminal nodes — they don't need prompts
                !is_start_node(&n.id, &n.shape) && !is_terminal_node(&n.id, &n.shape)
            })
            .filter(|n| n.prompt.is_none() && n.label == n.id)
            .map(|n| Diagnostic {
                rule: self.name().into(),
                severity: Severity::Warning,
                message: format!(
                    "Node '{}' (shape={}) has no prompt and label matches id",
                    n.id, n.shape
                ),
                node_id: Some(n.id.clone()),
                edge: None,
                fix: Some("Add a prompt or a descriptive label attribute".into()),
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Run all built-in lint rules and return collected diagnostics.
pub fn validate(graph: &PipelineGraph) -> Vec<Diagnostic> {
    let rules: Vec<Box<dyn LintRule>> = vec![
        Box::new(StartNodeRule),
        Box::new(TerminalNodeRule),
        Box::new(ReachabilityRule),
        Box::new(EdgeTargetExistsRule),
        Box::new(StartNoIncomingRule),
        Box::new(ExitNoOutgoingRule),
        Box::new(ConditionSyntaxRule),
        Box::new(FidelityValidRule),
        Box::new(RetryTargetExistsRule),
        Box::new(GoalGateHasRetryRule),
        Box::new(ProviderValidRule),
        Box::new(PromptOnLlmNodesRule),
    ];

    let mut diagnostics = Vec::new();
    for rule in &rules {
        diagnostics.extend(rule.apply(graph));
    }
    diagnostics
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
