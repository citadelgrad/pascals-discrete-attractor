//! Canonical compilation of parsed pipeline attributes into executable semantics.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::num::NonZeroUsize;
use std::time::Duration;

use attractor_dot::AttributeValue;

use crate::graph::{PipelineEdge, PipelineGraph, PipelineNode};
use crate::handler::HandlerRegistry;
use crate::transforms::apply_transforms;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LlmProvider {
    Claude,
    Codex,
    Gemini,
}

impl LlmProvider {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "claude" | "anthropic" => Some(Self::Claude),
            "codex" | "openai" => Some(Self::Codex),
            "gemini" | "google" => Some(Self::Gemini),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
        }
    }

    pub fn binary_name(self) -> &'static str {
        self.as_str()
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex CLI",
            Self::Gemini => "Gemini CLI",
        }
    }
}

impl std::str::FromStr for LlmProvider {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value).ok_or("unknown LLM provider")
    }
}

impl fmt::Display for LlmProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum HandlerIdentity {
    Start,
    Exit,
    Codergen,
    Conditional,
    WaitHuman,
    Tool,
    Parallel,
    FanIn,
    ManagerLoop,
    Quality,
    Custom(String),
}

impl HandlerIdentity {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Start => "start",
            Self::Exit => "exit",
            Self::Codergen => "codergen",
            Self::Conditional => "conditional",
            Self::WaitHuman => "wait.human",
            Self::Tool => "tool",
            Self::Parallel => "parallel",
            Self::FanIn => "parallel.fan_in",
            Self::ManagerLoop => "stack.manager_loop",
            Self::Quality => "quality",
            Self::Custom(name) => name,
        }
    }
}

impl fmt::Display for HandlerIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedNodeKind {
    Start,
    Exit,
    Task,
    Conditional { llm_backed: bool },
    HumanGate,
    Tool,
    Parallel,
    FanIn,
    ManagerLoop,
    Quality,
    Custom,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedNode {
    pub node_id: String,
    pub kind: ResolvedNodeKind,
    pub handler: HandlerIdentity,
    pub provider: Option<LlmProvider>,
    pub invocation: NodeInvocationPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeInvocationPolicy {
    pub max_attempts: NonZeroUsize,
    pub timeout: Option<Duration>,
}

impl Default for NodeInvocationPolicy {
    fn default() -> Self {
        Self {
            max_attempts: NonZeroUsize::MIN,
            timeout: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SemanticDiagnosticKind {
    InvalidAttributeType,
    InvalidAttributeValue,
    ConflictingRoleSignals,
    ConflictingAttributeAliases,
    UnknownShape,
    UnknownHandler,
    HandlerCapabilityMismatch,
    MissingProvider,
    UnknownProvider,
    UnsupportedExecutionTopology,
    UnsupportedExecutionCapability,
    MultipleStarts,
    MissingStart,
    MultipleExits,
    MissingExit,
    TransformError,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticDiagnostic {
    pub kind: SemanticDiagnosticKind,
    pub node_id: Option<String>,
    pub message: String,
    pub fix: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticError {
    pub diagnostics: Vec<SemanticDiagnostic>,
}

impl fmt::Display for SemanticError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let messages = self
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        write!(f, "pipeline semantic compilation failed: {messages}")
    }
}

impl std::error::Error for SemanticError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingProviderPolicy {
    Reject,
    Insert(LlmProvider),
}

#[derive(Debug, Clone)]
pub struct PlanCompilation {
    pub plan: ExecutionPlan,
    pub defaulted_provider_nodes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ExecutionPlan {
    graph: PipelineGraph,
    nodes: HashMap<String, ResolvedNode>,
    handler_capabilities: HashMap<String, bool>,
    start_id: String,
    exit_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Start,
    Exit,
    Task,
    Conditional,
    HumanGate,
    Tool,
    Parallel,
    FanIn,
    ManagerLoop,
    Quality,
    Custom,
}

impl ExecutionPlan {
    pub fn compile(graph: PipelineGraph) -> Result<Self, SemanticError> {
        Ok(Self::compile_with_policy(
            graph,
            &builtin_handler_catalog(),
            MissingProviderPolicy::Reject,
        )?
        .plan)
    }

    pub fn compile_with_registry(
        graph: PipelineGraph,
        registry: &HandlerRegistry,
    ) -> Result<Self, SemanticError> {
        Ok(Self::compile_with_policy(
            graph,
            &registry.handler_capabilities(),
            MissingProviderPolicy::Reject,
        )?
        .plan)
    }

    pub fn compile_for_generation(
        graph: PipelineGraph,
        default_provider: LlmProvider,
    ) -> Result<PlanCompilation, SemanticError> {
        Self::compile_with_policy(
            graph,
            &builtin_handler_catalog(),
            MissingProviderPolicy::Insert(default_provider),
        )
    }

    pub fn compile_for_generation_with_registry(
        graph: PipelineGraph,
        registry: &HandlerRegistry,
        default_provider: LlmProvider,
    ) -> Result<PlanCompilation, SemanticError> {
        Self::compile_with_policy(
            graph,
            &registry.handler_capabilities(),
            MissingProviderPolicy::Insert(default_provider),
        )
    }

    fn compile_with_policy(
        mut graph: PipelineGraph,
        handler_catalog: &HashMap<String, bool>,
        policy: MissingProviderPolicy,
    ) -> Result<PlanCompilation, SemanticError> {
        let mut diagnostics = Vec::new();
        validate_semantic_attribute_types(&graph, &mut diagnostics);
        normalize_aliases(&mut graph, &mut diagnostics);

        if diagnostics.is_empty() {
            if let Err(error) = apply_transforms(&mut graph) {
                diagnostics.push(SemanticDiagnostic {
                    kind: SemanticDiagnosticKind::TransformError,
                    node_id: None,
                    message: error.to_string(),
                    fix: "Correct the stylesheet or prompt transform input".into(),
                });
            }
        }
        validate_unsupported_attributes(&graph, &mut diagnostics);

        let mut node_ids = graph
            .all_nodes()
            .map(|node| node.id.clone())
            .collect::<Vec<_>>();
        node_ids.sort();

        let mut nodes = HashMap::new();
        let mut starts = Vec::new();
        let mut exits = Vec::new();
        let mut defaulted_provider_nodes = Vec::new();

        for node_id in node_ids {
            let source = graph.node(&node_id).expect("collected node must exist");
            match resolve_node(source, handler_catalog, policy) {
                Ok((resolved, defaulted)) => {
                    if matches!(resolved.kind, ResolvedNodeKind::Start) {
                        starts.push(node_id.clone());
                    }
                    if matches!(resolved.kind, ResolvedNodeKind::Exit) {
                        exits.push(node_id.clone());
                    }
                    if defaulted {
                        defaulted_provider_nodes.push(node_id.clone());
                    }
                    nodes.insert(node_id, resolved);
                }
                Err(mut node_diagnostics) => diagnostics.append(&mut node_diagnostics),
            }
        }

        validate_supported_execution_topology(&graph, &nodes, &mut diagnostics);
        validate_supported_execution_capabilities(&graph, &nodes, &mut diagnostics);

        starts.sort();
        exits.sort();
        defaulted_provider_nodes.sort();

        if starts.is_empty() {
            diagnostics.push(SemanticDiagnostic {
                kind: SemanticDiagnosticKind::MissingStart,
                node_id: None,
                message: "Pipeline has no canonical start node".into(),
                fix: "Add exactly one shape=\"Mdiamond\" start node".into(),
            });
        } else if starts.len() > 1 {
            diagnostics.push(SemanticDiagnostic {
                kind: SemanticDiagnosticKind::MultipleStarts,
                node_id: None,
                message: format!("Pipeline has multiple start nodes: {}", starts.join(", ")),
                fix: "Keep exactly one start node".into(),
            });
        }

        if exits.is_empty() {
            diagnostics.push(SemanticDiagnostic {
                kind: SemanticDiagnosticKind::MissingExit,
                node_id: None,
                message: "Pipeline has no canonical exit node".into(),
                fix: "Add exactly one shape=\"Msquare\" exit node".into(),
            });
        } else if exits.len() > 1 {
            diagnostics.push(SemanticDiagnostic {
                kind: SemanticDiagnosticKind::MultipleExits,
                node_id: None,
                message: format!("Pipeline has multiple exit nodes: {}", exits.join(", ")),
                fix: "Keep exactly one exit node".into(),
            });
        }

        diagnostics.sort_by(|left, right| {
            left.node_id
                .cmp(&right.node_id)
                .then(left.kind.cmp(&right.kind))
                .then(left.message.cmp(&right.message))
        });
        if !diagnostics.is_empty() {
            return Err(SemanticError { diagnostics });
        }

        for node_id in &defaulted_provider_nodes {
            let provider = nodes
                .get(node_id)
                .and_then(|node| node.provider)
                .expect("defaulted node has provider");
            if let Some(source) = graph.all_nodes_mut().find(|node| node.id == *node_id) {
                source.llm_provider = Some(provider.as_str().into());
            }
        }

        let handler_capabilities = nodes
            .values()
            .map(|node| {
                let name = node.handler.as_str();
                (
                    name.to_string(),
                    *handler_catalog
                        .get(name)
                        .expect("resolved handler capability must exist"),
                )
            })
            .collect();

        Ok(PlanCompilation {
            plan: Self {
                graph,
                nodes,
                handler_capabilities,
                start_id: starts.pop().expect("cardinality checked"),
                exit_ids: exits,
            },
            defaulted_provider_nodes,
        })
    }

    pub fn graph(&self) -> &PipelineGraph {
        &self.graph
    }

    pub fn node(&self, id: &str) -> Option<&ResolvedNode> {
        self.nodes.get(id)
    }

    pub fn source_node(&self, id: &str) -> Option<&PipelineNode> {
        self.graph.node(id)
    }

    pub fn start_id(&self) -> &str {
        &self.start_id
    }

    pub fn start_node(&self) -> &ResolvedNode {
        self.nodes
            .get(&self.start_id)
            .expect("compiled start must exist")
    }

    pub fn is_exit(&self, id: &str) -> bool {
        self.exit_ids
            .binary_search_by(|candidate| candidate.as_str().cmp(id))
            .is_ok()
    }

    pub fn exit_ids(&self) -> &[String] {
        &self.exit_ids
    }

    pub fn all_nodes(&self) -> impl Iterator<Item = &ResolvedNode> {
        self.nodes.values()
    }

    pub fn outgoing_edges(&self, id: &str) -> &[PipelineEdge] {
        self.graph.outgoing_edges(id)
    }

    pub(crate) fn ensure_registry_compatible(
        &self,
        registry: &HandlerRegistry,
    ) -> Result<(), SemanticError> {
        let actual_capabilities = registry.handler_capabilities();
        let mut node_ids = self.nodes.keys().collect::<Vec<_>>();
        node_ids.sort();
        let mut diagnostics = Vec::new();

        for node_id in node_ids {
            let node = self
                .nodes
                .get(node_id)
                .expect("collected resolved node must exist");
            let handler = node.handler.as_str();
            let expected = self
                .handler_capabilities
                .get(handler)
                .copied()
                .expect("compiled handler capability must exist");
            match actual_capabilities.get(handler).copied() {
                None => diagnostics.push(SemanticDiagnostic {
                    kind: SemanticDiagnosticKind::UnknownHandler,
                    node_id: Some(node.node_id.clone()),
                    message: format!(
                        "Node '{}' requires handler '{}' but the executor has not registered it",
                        node.node_id, handler
                    ),
                    fix: format!("Register handler '{}' before executing this plan", handler),
                }),
                Some(actual) if actual != expected => diagnostics.push(SemanticDiagnostic {
                    kind: SemanticDiagnosticKind::HandlerCapabilityMismatch,
                    node_id: Some(node.node_id.clone()),
                    message: format!(
                        "Node '{}' compiled handler '{}' with provider capability {} but the executor registered capability {}",
                        node.node_id, handler, expected, actual
                    ),
                    fix: "Recompile the plan with the executor's handler registry".into(),
                }),
                Some(_) => {}
            }
        }

        if diagnostics.is_empty() {
            Ok(())
        } else {
            Err(SemanticError { diagnostics })
        }
    }
}

fn validate_supported_execution_topology(
    graph: &PipelineGraph,
    nodes: &HashMap<String, ResolvedNode>,
    diagnostics: &mut Vec<SemanticDiagnostic>,
) {
    let mut node_ids = nodes.keys().collect::<Vec<_>>();
    node_ids.sort();

    for node_id in node_ids {
        let node = &nodes[node_id];
        match node.kind {
            ResolvedNodeKind::Parallel => {
                let outgoing = graph.outgoing_edges(node_id);
                if outgoing.len() <= 1 {
                    continue;
                }
                let mut targets = outgoing
                    .iter()
                    .map(|edge| edge.to.as_str())
                    .collect::<Vec<_>>();
                targets.sort();
                diagnostics.push(SemanticDiagnostic {
                    kind: SemanticDiagnosticKind::UnsupportedExecutionTopology,
                    node_id: Some(node_id.clone()),
                    message: format!(
                        "Node '{node_id}' resolves to parallel but has {} outgoing edges ({}); PAS executes only one successor per step",
                        outgoing.len(),
                        targets.join(", ")
                    ),
                    fix: "Rewrite this node as a linear sequence until parallel branch execution is supported"
                        .into(),
                });
            }
            ResolvedNodeKind::FanIn => diagnostics.push(SemanticDiagnostic {
                kind: SemanticDiagnosticKind::UnsupportedExecutionTopology,
                node_id: Some(node_id.clone()),
                message: format!(
                    "Node '{node_id}' resolves to fan-in, but PAS cannot merge branch results"
                ),
                fix: "Remove the fan-in node and use a linear sequence until branch merging is supported"
                    .into(),
            }),
            _ => {}
        }
    }
}

fn validate_supported_execution_capabilities(
    graph: &PipelineGraph,
    nodes: &HashMap<String, ResolvedNode>,
    diagnostics: &mut Vec<SemanticDiagnostic>,
) {
    let mut node_ids = nodes.keys().collect::<Vec<_>>();
    node_ids.sort();
    for node_id in node_ids {
        let resolved = &nodes[node_id];
        let source = graph.node(node_id).expect("resolved node must have source");

        if matches!(resolved.kind, ResolvedNodeKind::ManagerLoop) {
            diagnostics.push(unsupported_node_capability(
                node_id,
                "manager-loop execution",
                "Replace the manager node with an explicit linear sequence",
            ));
        }

        let supports_claude_controls = resolved.handler == HandlerIdentity::Codergen
            && resolved.provider == Some(LlmProvider::Claude);
        if !supports_claude_controls {
            for attribute in ["allowed_tools", "max_budget_usd"] {
                if source.raw_attrs.contains_key(attribute) {
                    diagnostics.push(unsupported_node_capability(
                        node_id,
                        attribute,
                        &format!("Remove '{attribute}' or use it on a Claude-backed codergen node"),
                    ));
                }
            }
        } else {
            for attribute in ["allowed_tools", "max_budget_usd"] {
                if let Some(value) = source.raw_attrs.get(attribute) {
                    if !matches!(value, AttributeValue::String(_)) {
                        diagnostics.push(invalid_attribute_type(Some(node_id), attribute, value));
                    }
                }
            }
        }
    }
}

fn validate_unsupported_attributes(
    graph: &PipelineGraph,
    diagnostics: &mut Vec<SemanticDiagnostic>,
) {
    let mut nodes = graph.all_nodes().collect::<Vec<_>>();
    nodes.sort_by(|left, right| left.id.cmp(&right.id));
    for node in nodes {
        for attribute in [
            "fidelity",
            "reasoning_effort",
            "auto_status",
            "allow_partial",
            "thread_id",
        ] {
            if node.raw_attrs.contains_key(attribute) {
                diagnostics.push(unsupported_node_capability(
                    &node.id,
                    attribute,
                    &format!("Remove the '{attribute}' attribute or stylesheet declaration"),
                ));
            }
        }
    }

    for edge in graph.all_edges() {
        for (attribute, present) in [
            ("fidelity", edge.raw_attrs.contains_key("fidelity")),
            ("thread_id", edge.raw_attrs.contains_key("thread_id")),
        ] {
            if present {
                diagnostics.push(SemanticDiagnostic {
                    kind: SemanticDiagnosticKind::UnsupportedExecutionCapability,
                    node_id: None,
                    message: format!(
                        "Edge '{} -> {}' uses unsupported execution capability '{attribute}'",
                        edge.from, edge.to
                    ),
                    fix: format!("Remove the '{attribute}' edge attribute"),
                });
            }
        }
    }
}

fn unsupported_node_capability(node_id: &str, capability: &str, fix: &str) -> SemanticDiagnostic {
    SemanticDiagnostic {
        kind: SemanticDiagnosticKind::UnsupportedExecutionCapability,
        node_id: Some(node_id.to_string()),
        message: format!("Node '{node_id}' uses unsupported execution capability '{capability}'"),
        fix: fix.to_string(),
    }
}

fn builtin_handler_catalog() -> HashMap<String, bool> {
    [
        ("start", false),
        ("exit", false),
        ("codergen", true),
        ("conditional", false),
        ("wait.human", false),
        ("tool", false),
        ("parallel", false),
        ("parallel.fan_in", false),
        ("stack.manager_loop", false),
        ("quality", false),
    ]
    .into_iter()
    .map(|(name, consumes_provider)| (name.to_string(), consumes_provider))
    .collect()
}

const GRAPH_STRING_SEMANTIC_ATTRS: &[&str] = &["model_stylesheet", "stylesheet"];
const NODE_STRING_SEMANTIC_ATTRS: &[&str] = &[
    "shape",
    "type",
    "node_type",
    "handler",
    "prompt",
    "llm_provider",
    "class",
    "classes",
];

fn validate_semantic_attribute_types(
    graph: &PipelineGraph,
    diagnostics: &mut Vec<SemanticDiagnostic>,
) {
    for key in GRAPH_STRING_SEMANTIC_ATTRS {
        if let Some(value) = graph.attrs.get(*key) {
            if !matches!(value, AttributeValue::String(_)) {
                diagnostics.push(invalid_attribute_type(None, key, value));
            }
        }
    }

    for node in graph.all_nodes() {
        for key in NODE_STRING_SEMANTIC_ATTRS {
            if let Some(value) = node.raw_attrs.get(*key) {
                if !matches!(value, AttributeValue::String(_)) {
                    diagnostics.push(invalid_attribute_type(Some(&node.id), key, value));
                }
            }
        }
    }
}

fn invalid_attribute_type(
    node_id: Option<&str>,
    key: &str,
    value: &AttributeValue,
) -> SemanticDiagnostic {
    let (value_type, authored_value) = attribute_type_and_value(value);
    let subject = node_id
        .map(|id| format!("Node '{id}'"))
        .unwrap_or_else(|| "Graph".to_string());
    SemanticDiagnostic {
        kind: SemanticDiagnosticKind::InvalidAttributeType,
        node_id: node_id.map(str::to_string),
        message: format!(
            "{subject} semantic attribute '{key}' must be a string, found {value_type} value {authored_value}"
        ),
        fix: format!("Quote the '{key}' value, for example {key}=\"{authored_value}\""),
    }
}

fn attribute_type_and_value(value: &AttributeValue) -> (&'static str, String) {
    match value {
        AttributeValue::String(value) => ("string", value.clone()),
        AttributeValue::Integer(value) => ("integer", value.to_string()),
        AttributeValue::Float(value) => ("float", value.to_string()),
        AttributeValue::Boolean(value) => ("boolean", value.to_string()),
        AttributeValue::Duration(value) => ("duration", format!("{value:?}")),
    }
}

fn normalize_aliases(graph: &mut PipelineGraph, diagnostics: &mut Vec<SemanticDiagnostic>) {
    normalize_graph_alias(graph, "model_stylesheet", "stylesheet", diagnostics);

    for node in graph.all_nodes_mut() {
        if let Some(value) =
            resolve_node_alias(node, &["type", "node_type", "handler"], diagnostics)
        {
            node.node_type = Some(value);
        }
        if let Some(value) = resolve_node_alias(node, &["class", "classes"], diagnostics) {
            node.classes = value.split_whitespace().map(str::to_string).collect();
        }
    }
}

fn normalize_graph_alias(
    graph: &mut PipelineGraph,
    canonical: &str,
    alias: &str,
    diagnostics: &mut Vec<SemanticDiagnostic>,
) {
    let canonical_value = string_attr(&graph.attrs, canonical);
    let alias_value = string_attr(&graph.attrs, alias);
    match (canonical_value, alias_value) {
        (Some(left), Some(right)) if left != right => diagnostics.push(SemanticDiagnostic {
            kind: SemanticDiagnosticKind::ConflictingAttributeAliases,
            node_id: None,
            message: format!("Graph attributes '{canonical}' and '{alias}' disagree"),
            fix: format!("Use only '{canonical}', or give both attributes the same value"),
        }),
        (None, Some(value)) => {
            graph
                .attrs
                .insert(canonical.into(), AttributeValue::String(value));
        }
        _ => {}
    }
}

fn resolve_node_alias(
    node: &PipelineNode,
    keys: &[&str],
    diagnostics: &mut Vec<SemanticDiagnostic>,
) -> Option<String> {
    let values = keys
        .iter()
        .filter_map(|key| string_attr(&node.raw_attrs, key).map(|value| (*key, value)))
        .collect::<Vec<_>>();
    let distinct = values
        .iter()
        .map(|(_, value)| value.as_str())
        .collect::<HashSet<_>>();
    if distinct.len() > 1 {
        diagnostics.push(SemanticDiagnostic {
            kind: SemanticDiagnosticKind::ConflictingAttributeAliases,
            node_id: Some(node.id.clone()),
            message: format!(
                "Node '{}' has conflicting aliases: {}",
                node.id,
                values
                    .iter()
                    .map(|(key, value)| format!("{key}={value}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            fix: format!("Use only '{}', or give all aliases the same value", keys[0]),
        });
        None
    } else {
        values.first().map(|(_, value)| value.clone())
    }
}

fn string_attr(attrs: &HashMap<String, AttributeValue>, key: &str) -> Option<String> {
    match attrs.get(key) {
        Some(AttributeValue::String(value)) => Some(value.clone()),
        _ => None,
    }
}

fn resolve_node(
    node: &PipelineNode,
    handler_catalog: &HashMap<String, bool>,
    policy: MissingProviderPolicy,
) -> Result<(ResolvedNode, bool), Vec<SemanticDiagnostic>> {
    let mut diagnostics = Vec::new();
    let explicit_semantics = node.raw_attrs.contains_key("shape")
        || node.raw_attrs.contains_key("type")
        || node.raw_attrs.contains_key("node_type")
        || node.raw_attrs.contains_key("handler");
    let magic_role = magic_role(&node.id);
    let configured_shape = string_attr(&node.raw_attrs, "shape");
    let shape_role = configured_shape.as_deref().and_then(shape_role);
    let type_name = node.node_type.as_deref();
    let type_role = type_name.and_then(type_role);
    let registered_custom_handler =
        type_name.is_some_and(|name| type_role.is_none() && handler_catalog.contains_key(name));
    let known_explicit_handler = type_role.is_some() || registered_custom_handler;

    if let Some(name) = type_name {
        if type_role.is_none() && !handler_catalog.contains_key(name) {
            diagnostics.push(SemanticDiagnostic {
                kind: SemanticDiagnosticKind::UnknownHandler,
                node_id: Some(node.id.clone()),
                message: format!("Node '{}' names unknown handler '{name}'", node.id),
                fix: "Use a built-in handler or register the custom handler before compiling"
                    .into(),
            });
        }
    }

    if configured_shape.is_some() && shape_role.is_none() && !known_explicit_handler {
        diagnostics.push(SemanticDiagnostic {
            kind: SemanticDiagnosticKind::UnknownShape,
            node_id: Some(node.id.clone()),
            message: format!("Node '{}' has unknown shape '{}'", node.id, node.shape),
            fix: "Use a supported shape or name a registered handler with type=".into(),
        });
    }

    let signal_role = if let Some(type_role) = type_role {
        if let Some(shape_role) = shape_role {
            if !signals_compatible(shape_role, type_role, type_name) {
                diagnostics.push(role_conflict(node, shape_role, type_role));
            }
            if shape_role == Role::Conditional && type_name == Some("codergen") {
                shape_role
            } else {
                type_role
            }
        } else {
            type_role
        }
    } else if registered_custom_handler {
        if let Some(shape_role) = shape_role {
            diagnostics.push(role_conflict(node, shape_role, Role::Custom));
        }
        Role::Custom
    } else {
        shape_role.unwrap_or(Role::Task)
    };

    let role = if let Some(magic_role) = magic_role {
        if explicit_semantics {
            if !roles_compatible(magic_role, signal_role) {
                diagnostics.push(role_conflict(node, magic_role, signal_role));
            }
            signal_role
        } else {
            magic_role
        }
    } else {
        signal_role
    };

    if let Some(provider) = node.llm_provider.as_deref() {
        if LlmProvider::parse(provider).is_none() {
            diagnostics.push(SemanticDiagnostic {
                kind: SemanticDiagnosticKind::UnknownProvider,
                node_id: Some(node.id.clone()),
                message: format!("Node '{}' has unknown llm_provider '{provider}'", node.id),
                fix: "Use claude/anthropic, codex/openai, or gemini/google".into(),
            });
        }
    }

    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }

    let invocation = resolve_invocation_policy(node).map_err(|diagnostic| vec![diagnostic])?;

    let explicit_codergen = type_name == Some("codergen");
    let llm_conditional = role == Role::Conditional && (node.prompt.is_some() || explicit_codergen);
    let (kind, handler) = match role {
        Role::Start => (ResolvedNodeKind::Start, HandlerIdentity::Start),
        Role::Exit => (ResolvedNodeKind::Exit, HandlerIdentity::Exit),
        Role::Task => (ResolvedNodeKind::Task, HandlerIdentity::Codergen),
        Role::Conditional if llm_conditional => (
            ResolvedNodeKind::Conditional { llm_backed: true },
            HandlerIdentity::Codergen,
        ),
        Role::Conditional => (
            ResolvedNodeKind::Conditional { llm_backed: false },
            HandlerIdentity::Conditional,
        ),
        Role::HumanGate => (ResolvedNodeKind::HumanGate, HandlerIdentity::WaitHuman),
        Role::Tool => (ResolvedNodeKind::Tool, HandlerIdentity::Tool),
        Role::Parallel => (ResolvedNodeKind::Parallel, HandlerIdentity::Parallel),
        Role::FanIn => (ResolvedNodeKind::FanIn, HandlerIdentity::FanIn),
        Role::ManagerLoop => (ResolvedNodeKind::ManagerLoop, HandlerIdentity::ManagerLoop),
        Role::Quality => (ResolvedNodeKind::Quality, HandlerIdentity::Quality),
        Role::Custom => (
            ResolvedNodeKind::Custom,
            HandlerIdentity::Custom(type_name.expect("custom role has type").to_string()),
        ),
    };

    let Some(consumes_provider) = handler_catalog.get(handler.as_str()).copied() else {
        return Err(vec![SemanticDiagnostic {
            kind: SemanticDiagnosticKind::UnknownHandler,
            node_id: Some(node.id.clone()),
            message: format!(
                "Node '{}' resolves to unavailable handler '{}'",
                node.id, handler
            ),
            fix: format!("Register handler '{}' before compiling", handler),
        }]);
    };

    let (provider, defaulted) = if consumes_provider {
        match node.llm_provider.as_deref().and_then(LlmProvider::parse) {
            Some(provider) => (Some(provider), false),
            None => match policy {
                MissingProviderPolicy::Reject => {
                    return Err(vec![SemanticDiagnostic {
                        kind: SemanticDiagnosticKind::MissingProvider,
                        node_id: Some(node.id.clone()),
                        message: format!(
                            "Node '{}' resolves to handler '{}' but has no llm_provider",
                            node.id, handler
                        ),
                        fix: "Add llm_provider=\"claude\", \"codex\", or \"gemini\"".into(),
                    }]);
                }
                MissingProviderPolicy::Insert(provider) => (Some(provider), true),
            },
        }
    } else {
        (None, false)
    };

    Ok((
        ResolvedNode {
            node_id: node.id.clone(),
            kind,
            handler,
            provider,
            invocation,
        },
        defaulted,
    ))
}

fn resolve_invocation_policy(
    node: &PipelineNode,
) -> Result<NodeInvocationPolicy, SemanticDiagnostic> {
    let max_retries = match node.raw_attrs.get("max_retries") {
        None => 0,
        Some(AttributeValue::Integer(value)) if *value >= 0 => usize::try_from(*value)
            .map_err(|_| invalid_retry_budget(node, *value, "does not fit this platform"))?,
        Some(AttributeValue::Integer(value)) => {
            return Err(invalid_retry_budget(node, *value, "must be non-negative"));
        }
        Some(value) => {
            return Err(invalid_attribute_type(Some(&node.id), "max_retries", value));
        }
    };
    let max_attempts = max_retries
        .checked_add(1)
        .and_then(NonZeroUsize::new)
        .ok_or_else(|| invalid_retry_budget(node, i64::MAX, "is too large"))?;
    let timeout = match node.raw_attrs.get("timeout") {
        None => None,
        Some(AttributeValue::Duration(value)) => Some(*value),
        Some(AttributeValue::String(value)) => Some(
            attractor_dot::duration_serde::parse_duration_str(value).map_err(|_| {
                SemanticDiagnostic {
                    kind: SemanticDiagnosticKind::InvalidAttributeValue,
                    node_id: Some(node.id.clone()),
                    message: format!(
                        "Node '{}' execution attribute 'timeout' must be a valid duration, found '{value}'",
                        node.id
                    ),
                    fix: "Use a duration such as timeout=120s or timeout=5m".into(),
                }
            })?,
        ),
        Some(value) => {
            return Err(invalid_attribute_type(Some(&node.id), "timeout", value));
        }
    };

    Ok(NodeInvocationPolicy {
        max_attempts,
        timeout,
    })
}

fn invalid_retry_budget(node: &PipelineNode, value: i64, reason: &str) -> SemanticDiagnostic {
    SemanticDiagnostic {
        kind: SemanticDiagnosticKind::InvalidAttributeValue,
        node_id: Some(node.id.clone()),
        message: format!(
            "Node '{}' execution attribute 'max_retries' {reason}, found {value}",
            node.id
        ),
        fix: "Use a non-negative integer retry count small enough for this platform".into(),
    }
}

fn magic_role(id: &str) -> Option<Role> {
    if id.eq_ignore_ascii_case("start") {
        Some(Role::Start)
    } else if ["exit", "end", "done"]
        .iter()
        .any(|candidate| id.eq_ignore_ascii_case(candidate))
    {
        Some(Role::Exit)
    } else {
        None
    }
}

fn shape_role(shape: &str) -> Option<Role> {
    match shape {
        "Mdiamond" => Some(Role::Start),
        "Msquare" => Some(Role::Exit),
        "box" => Some(Role::Task),
        "diamond" => Some(Role::Conditional),
        "hexagon" => Some(Role::HumanGate),
        "parallelogram" => Some(Role::Tool),
        "component" => Some(Role::Parallel),
        "tripleoctagon" => Some(Role::FanIn),
        "house" => Some(Role::ManagerLoop),
        _ => None,
    }
}

fn type_role(node_type: &str) -> Option<Role> {
    match node_type {
        "start" => Some(Role::Start),
        "exit" => Some(Role::Exit),
        "codergen" => Some(Role::Task),
        "conditional" => Some(Role::Conditional),
        "wait.human" => Some(Role::HumanGate),
        "tool" => Some(Role::Tool),
        "parallel" => Some(Role::Parallel),
        "fan_in" | "parallel.fan_in" => Some(Role::FanIn),
        "manager" | "stack.manager_loop" => Some(Role::ManagerLoop),
        "quality" => Some(Role::Quality),
        _ => None,
    }
}

fn roles_compatible(left: Role, right: Role) -> bool {
    left == right
        || matches!(
            (left, right),
            (Role::Task, Role::Quality) | (Role::Quality, Role::Task)
        )
}

fn signals_compatible(shape: Role, node_type: Role, type_name: Option<&str>) -> bool {
    roles_compatible(shape, node_type)
        || (shape == Role::Conditional && node_type == Role::Task && type_name == Some("codergen"))
}

fn role_conflict(node: &PipelineNode, left: Role, right: Role) -> SemanticDiagnostic {
    SemanticDiagnostic {
        kind: SemanticDiagnosticKind::ConflictingRoleSignals,
        node_id: Some(node.id.clone()),
        message: format!(
            "Node '{}' has conflicting role signals ({left:?} versus {right:?})",
            node.id
        ),
        fix: "Make id, shape, and type identify the same node role".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(dot: &str) -> PipelineGraph {
        PipelineGraph::from_dot(attractor_dot::parse(dot).unwrap()).unwrap()
    }

    #[test]
    fn incompatible_shape_and_type_return_typed_conflict() {
        let error = ExecutionPlan::compile(graph(
            r#"digraph G {
                start [shape="Mdiamond"]
                work [shape="box", type="conditional"]
                done [shape="Msquare"]
                start -> work -> done
            }"#,
        ))
        .unwrap_err();

        assert!(error.diagnostics.iter().any(|diagnostic| {
            diagnostic.kind == SemanticDiagnosticKind::ConflictingRoleSignals
                && diagnostic.node_id.as_deref() == Some("work")
        }));
    }

    #[test]
    fn registered_custom_handler_conflicts_with_every_known_role_shape() {
        let mut catalog = builtin_handler_catalog();
        catalog.insert("custom.review".into(), false);

        for shape in [
            "Mdiamond",
            "Msquare",
            "box",
            "diamond",
            "hexagon",
            "parallelogram",
            "component",
            "tripleoctagon",
            "house",
        ] {
            let source = format!(
                r#"digraph G {{
                    start [shape="Mdiamond"]
                    disguised [shape="{shape}", type="custom.review"]
                    done [shape="Msquare"]
                    start -> disguised -> done
                }}"#
            );
            let error = ExecutionPlan::compile_with_policy(
                graph(&source),
                &catalog,
                MissingProviderPolicy::Reject,
            )
            .unwrap_err();

            assert!(
                error.diagnostics.iter().any(|diagnostic| {
                    diagnostic.kind == SemanticDiagnosticKind::ConflictingRoleSignals
                        && diagnostic.node_id.as_deref() == Some("disguised")
                }),
                "shape {shape} bypassed custom-handler conflict detection: {error:?}"
            );
        }
    }

    #[test]
    fn conflicting_aliases_and_role_cardinality_are_typed_and_deterministic() {
        let source = r#"digraph G {
            alpha [shape="Mdiamond"]
            beta [shape="Mdiamond"]
            work [shape="box", type="quality", node_type="codergen"]
            omega [shape="Msquare"]
            done [shape="Msquare"]
            alpha -> work -> omega
            beta -> done
        }"#;

        let first = ExecutionPlan::compile(graph(source)).unwrap_err();
        let second = ExecutionPlan::compile(graph(source)).unwrap_err();

        assert_eq!(first.diagnostics, second.diagnostics);
        assert!(first.diagnostics.iter().any(|diagnostic| {
            diagnostic.kind == SemanticDiagnosticKind::ConflictingAttributeAliases
                && diagnostic.node_id.as_deref() == Some("work")
        }));
        assert!(first
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.kind == SemanticDiagnosticKind::MultipleStarts));
        assert!(first
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.kind == SemanticDiagnosticKind::MultipleExits));
    }

    #[test]
    fn supported_semantic_matrix_compiles_to_typed_nodes() {
        let plan = ExecutionPlan::compile(graph(
            r#"digraph G {
                start [shape="Mdiamond"]
                task [shape="box", prompt="task", llm_provider="anthropic"]
                llm_choice [shape="diamond", prompt="choose", llm_provider="OPENAI"]
                route [shape="diamond"]
                human [shape="hexagon"]
                tool [shape="parallelogram"]
                parallel [shape="component"]
                quality [shape="box", type="quality"]
                done [shape="Msquare"]
                start -> task -> llm_choice -> route -> human -> tool -> parallel -> quality -> done
            }"#,
        ))
        .unwrap();

        let expected = [
            (
                "start",
                ResolvedNodeKind::Start,
                HandlerIdentity::Start,
                None,
            ),
            (
                "task",
                ResolvedNodeKind::Task,
                HandlerIdentity::Codergen,
                Some(LlmProvider::Claude),
            ),
            (
                "llm_choice",
                ResolvedNodeKind::Conditional { llm_backed: true },
                HandlerIdentity::Codergen,
                Some(LlmProvider::Codex),
            ),
            (
                "route",
                ResolvedNodeKind::Conditional { llm_backed: false },
                HandlerIdentity::Conditional,
                None,
            ),
            (
                "human",
                ResolvedNodeKind::HumanGate,
                HandlerIdentity::WaitHuman,
                None,
            ),
            ("tool", ResolvedNodeKind::Tool, HandlerIdentity::Tool, None),
            (
                "parallel",
                ResolvedNodeKind::Parallel,
                HandlerIdentity::Parallel,
                None,
            ),
            (
                "quality",
                ResolvedNodeKind::Quality,
                HandlerIdentity::Quality,
                None,
            ),
            ("done", ResolvedNodeKind::Exit, HandlerIdentity::Exit, None),
        ];

        for (id, kind, handler, provider) in expected {
            let node = plan.node(id).unwrap();
            assert_eq!(node.kind, kind, "kind for {id}");
            assert_eq!(node.handler, handler, "handler for {id}");
            assert_eq!(node.provider, provider, "provider for {id}");
        }
    }

    #[test]
    fn multi_branch_parallel_topology_is_rejected() {
        let error = ExecutionPlan::compile(graph(
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
        ))
        .unwrap_err();

        assert_eq!(error.diagnostics.len(), 1);
        assert_eq!(error.diagnostics[0].node_id.as_deref(), Some("fork"));
        assert!(error.diagnostics[0].message.contains("2 outgoing edges"));
        assert!(error.diagnostics[0].message.contains("left, right"));
        assert!(error.diagnostics[0].fix.contains("linear"));
    }

    #[test]
    fn fan_in_topology_is_rejected() {
        let error = ExecutionPlan::compile(graph(
            r#"digraph G {
                start [shape="Mdiamond"]
                merge [shape="tripleoctagon"]
                done [shape="Msquare"]
                start -> merge -> done
            }"#,
        ))
        .unwrap_err();

        assert_eq!(error.diagnostics.len(), 1);
        assert_eq!(
            error.diagnostics[0].kind,
            SemanticDiagnosticKind::UnsupportedExecutionTopology
        );
        assert_eq!(error.diagnostics[0].node_id.as_deref(), Some("merge"));
        assert!(error.diagnostics[0]
            .message
            .contains("cannot merge branch results"));
        assert!(error.diagnostics[0].fix.contains("Remove"));
    }

    #[test]
    fn negative_max_retries_is_rejected_before_execution() {
        let result = ExecutionPlan::compile(graph(
            r#"digraph G {
                start [shape="Mdiamond"]
                work [shape="box", prompt="work", llm_provider="claude", max_retries=-1]
                done [shape="Msquare"]
                start -> work -> done
            }"#,
        ));

        let error = result.expect_err("negative retry budgets must fail closed");
        assert!(error
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("max_retries")
                && diagnostic.message.contains("non-negative")));
    }

    #[test]
    fn malformed_timeout_is_rejected_instead_of_becoming_unbounded() {
        let result = ExecutionPlan::compile(graph(
            r#"digraph G {
                start [shape="Mdiamond"]
                work [shape="box", prompt="work", llm_provider="claude", timeout="eventually"]
                done [shape="Msquare"]
                start -> work -> done
            }"#,
        ));

        let error = result.expect_err("invalid timeout must fail closed");
        assert!(error
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("timeout")
                && diagnostic.message.contains("valid duration")));
    }

    #[test]
    fn invocation_policy_rejects_wrong_attribute_types() {
        for (declaration, attribute) in [
            (r#"max_retries="two""#, "max_retries"),
            ("max_retries=1.5", "max_retries"),
            ("timeout=true", "timeout"),
        ] {
            let source = format!(
                r#"digraph G {{
                    start [shape="Mdiamond"]
                    work [shape="box", prompt="work", llm_provider="claude", {declaration}]
                    done [shape="Msquare"]
                    start -> work -> done
                }}"#
            );

            let error = ExecutionPlan::compile(graph(&source))
                .expect_err("malformed invocation policy must fail closed");
            assert!(error.diagnostics.iter().any(|diagnostic| {
                diagnostic.kind == SemanticDiagnosticKind::InvalidAttributeType
                    && diagnostic.message.contains(attribute)
            }));
        }
    }

    #[test]
    fn recognized_unsupported_execution_capabilities_fail_closed() {
        let cases = [
            (
                r#"work [shape="box", prompt="work", llm_provider="claude", fidelity="compact"]"#,
                "fidelity",
            ),
            (
                r#"work [shape="box", prompt="work", llm_provider="claude", reasoning_effort="high"]"#,
                "reasoning_effort",
            ),
            (
                r#"work [shape="box", prompt="work", llm_provider="claude", auto_status=true]"#,
                "auto_status",
            ),
            (
                r#"work [shape="box", prompt="work", llm_provider="claude", allow_partial=true]"#,
                "allow_partial",
            ),
            (
                r#"work [shape="box", prompt="work", llm_provider="claude", thread_id="thread"]"#,
                "thread_id",
            ),
            (r#"work [shape="house"]"#, "manager-loop"),
            (
                r#"work [shape="box", prompt="work", llm_provider="codex", allowed_tools="Read"]"#,
                "allowed_tools",
            ),
            (
                r#"work [shape="box", prompt="work", llm_provider="gemini", max_budget_usd=1.0]"#,
                "max_budget_usd",
            ),
        ];

        for (node, capability) in cases {
            let source = format!(
                "digraph G {{ start [shape=\"Mdiamond\"] {node} done [shape=\"Msquare\"] start -> work -> done }}"
            );
            let error = ExecutionPlan::compile(graph(&source))
                .expect_err("recognized no-op capability must be rejected");
            assert!(
                error
                    .diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.message.contains(capability)),
                "missing {capability} diagnostic: {error:?}"
            );
        }

        for (edge, capability) in [
            (r#"start -> work [fidelity="compact"]"#, "fidelity"),
            (r#"start -> work [thread_id="thread"]"#, "thread_id"),
        ] {
            let source = format!(
                "digraph G {{ start [shape=\"Mdiamond\"] work [shape=\"diamond\"] done [shape=\"Msquare\"] {edge} work -> done }}"
            );
            let error = ExecutionPlan::compile(graph(&source))
                .expect_err("unsupported edge capability must be rejected");
            assert!(
                error
                    .diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.message.contains(capability)),
                "missing edge {capability} diagnostic: {error:?}"
            );
        }
    }

    #[test]
    fn stylesheet_reasoning_effort_is_rejected_instead_of_ignored() {
        let error = ExecutionPlan::compile(graph(
            r##"digraph G {
                model_stylesheet="#work { reasoning_effort: high; }"
                start [shape="Mdiamond"]
                work [shape="box", prompt="work", llm_provider="claude"]
                done [shape="Msquare"]
                start -> work -> done
            }"##,
        ))
        .expect_err("unsupported stylesheet declarations must fail closed");

        assert!(error.diagnostics.iter().any(|diagnostic| {
            diagnostic.kind == SemanticDiagnosticKind::UnsupportedExecutionCapability
                && diagnostic.node_id.as_deref() == Some("work")
                && diagnostic.message.contains("reasoning_effort")
        }));
    }

    #[test]
    fn claude_execution_controls_require_string_values() {
        for (attribute, declaration) in [
            ("allowed_tools", "allowed_tools=true"),
            ("max_budget_usd", "max_budget_usd=1.0"),
        ] {
            let source = format!(
                r#"digraph G {{
                    start [shape="Mdiamond"]
                    work [shape="box", prompt="work", llm_provider="claude", {declaration}]
                    done [shape="Msquare"]
                    start -> work -> done
                }}"#
            );

            let error = ExecutionPlan::compile(graph(&source))
                .expect_err("malformed Claude controls must fail closed");
            assert!(error.diagnostics.iter().any(|diagnostic| {
                diagnostic.kind == SemanticDiagnosticKind::InvalidAttributeType
                    && diagnostic.message.contains(attribute)
            }));
        }
    }

    #[test]
    fn claude_controls_are_rejected_for_non_codergen_handlers() {
        let mut catalog = builtin_handler_catalog();
        catalog.insert("custom.llm".into(), true);
        let error = ExecutionPlan::compile_with_policy(
            graph(
                r#"digraph G {
                    start [shape="Mdiamond"]
                    work [type="custom.llm", llm_provider="claude", allowed_tools="Read"]
                    done [shape="Msquare"]
                    start -> work -> done
                }"#,
            ),
            &catalog,
            MissingProviderPolicy::Reject,
        )
        .expect_err("Claude controls are implemented only by the codergen handler");

        assert!(error.diagnostics.iter().any(|diagnostic| {
            diagnostic.kind == SemanticDiagnosticKind::UnsupportedExecutionCapability
                && diagnostic.message.contains("allowed_tools")
        }));
    }

    #[test]
    fn every_resolved_parallel_spelling_counts_authored_edges() {
        let cases = [
            r#"fork [shape="component"]"#,
            r#"fork [type="parallel"]"#,
            r#"fork [node_type="parallel"]"#,
            r#"fork [handler="parallel"]"#,
        ];

        for declaration in cases {
            let source = format!(
                r#"digraph G {{
                    start [shape="Mdiamond"]
                    {declaration}
                    branch [shape="diamond"]
                    done [shape="Msquare"]
                    start -> fork
                    fork -> branch
                    fork -> branch
                    branch -> done
                }}"#
            );
            let error = ExecutionPlan::compile_for_generation(graph(&source), LlmProvider::Codex)
                .unwrap_err();
            assert!(
                error.diagnostics.iter().any(|diagnostic| {
                    diagnostic.kind == SemanticDiagnosticKind::UnsupportedExecutionTopology
                        && diagnostic.node_id.as_deref() == Some("fork")
                        && diagnostic
                            .message
                            .contains("2 outgoing edges (branch, branch)")
                }),
                "{declaration}: {error:?}"
            );
        }

        let inherited = ExecutionPlan::compile(graph(
            r#"digraph G {
                node [shape="component"]
                start [shape="Mdiamond"]
                fork
                left [shape="diamond"]
                right [shape="diamond"]
                done [shape="Msquare"]
                start -> fork
                fork -> right
                fork -> left
                left -> done
                right -> done
            }"#,
        ))
        .unwrap_err();
        assert!(inherited.diagnostics.iter().any(|diagnostic| {
            diagnostic.kind == SemanticDiagnosticKind::UnsupportedExecutionTopology
                && diagnostic.node_id.as_deref() == Some("fork")
                && diagnostic.message.contains("left, right")
        }));
    }

    #[test]
    fn unsupported_topology_diagnostics_are_complete_and_deterministic() {
        let source = r#"digraph G {
            start [shape="Mdiamond"]
            z_fork [shape="component"]
            a_merge [shape="tripleoctagon"]
            route [shape="diamond"]
            done [shape="Msquare"]
            start -> z_fork
            z_fork -> route
            z_fork -> a_merge
            a_merge -> done
            route -> done
        }"#;

        let first = ExecutionPlan::compile(graph(source)).unwrap_err();
        let second = ExecutionPlan::compile(graph(source)).unwrap_err();
        assert_eq!(first.diagnostics, second.diagnostics);
        assert_eq!(first.diagnostics.len(), 2);
        assert_eq!(first.diagnostics[0].node_id.as_deref(), Some("a_merge"));
        assert_eq!(first.diagnostics[1].node_id.as_deref(), Some("z_fork"));
    }

    #[test]
    fn explicit_type_without_shape_is_the_semantic_signal() {
        let plan = ExecutionPlan::compile(graph(
            r#"digraph G {
                entry [type="start"]
                tool_step [node_type="tool"]
                route [handler="conditional"]
                finish [type="exit"]
                entry -> tool_step -> route -> finish
            }"#,
        ))
        .unwrap();

        assert_eq!(plan.node("entry").unwrap().kind, ResolvedNodeKind::Start);
        assert_eq!(
            plan.node("tool_step").unwrap().handler,
            HandlerIdentity::Tool
        );
        assert_eq!(
            plan.node("route").unwrap().kind,
            ResolvedNodeKind::Conditional { llm_backed: false }
        );
        assert_eq!(plan.node("finish").unwrap().kind, ResolvedNodeKind::Exit);
    }

    #[test]
    fn aliases_styles_and_prompt_transforms_precede_compilation() {
        let plan = ExecutionPlan::compile(graph(
            r#"digraph G {
                goal="ship safely"
                stylesheet=".llm { llm_provider: GoOgLe; llm_model: gemini-test; }"
                start [shape="Mdiamond"]
                work [shape="box", node_type="codergen", classes="llm", prompt="Goal: ${goal}"]
                done [shape="Msquare"]
                start -> work -> done
            }"#,
        ))
        .unwrap();

        let resolved = plan.node("work").unwrap();
        assert_eq!(resolved.kind, ResolvedNodeKind::Task);
        assert_eq!(resolved.provider, Some(LlmProvider::Gemini));
        let source = plan.source_node("work").unwrap();
        assert_eq!(source.prompt.as_deref(), Some("Goal: ship safely"));
        assert_eq!(source.llm_model.as_deref(), Some("gemini-test"));
    }

    #[test]
    fn stylesheet_provider_overrides_dot_default_but_not_explicit_node_value() {
        let plan = ExecutionPlan::compile(graph(
            r#"digraph G {
                stylesheet=".codex { llm_provider: openai; }"
                node [llm_provider="claude"]
                start [shape="Mdiamond"]
                styled [shape="box", classes="codex"]
                explicit [shape="box", classes="codex", llm_provider="google"]
                done [shape="Msquare"]
                start -> styled -> explicit -> done
            }"#,
        ))
        .unwrap();

        assert_eq!(
            plan.node("styled").unwrap().provider,
            Some(LlmProvider::Codex)
        );
        assert_eq!(
            plan.node("explicit").unwrap().provider,
            Some(LlmProvider::Gemini)
        );
    }

    #[test]
    fn magic_ids_are_roles_only_when_semantics_are_omitted() {
        let plan = ExecutionPlan::compile(graph(
            r#"digraph G {
                Start
                done
                Start -> done
            }"#,
        ))
        .unwrap();

        assert_eq!(plan.start_id(), "Start");
        assert!(plan.is_exit("done"));
        assert_eq!(plan.node("Start").unwrap().kind, ResolvedNodeKind::Start);
        assert_eq!(plan.node("done").unwrap().kind, ResolvedNodeKind::Exit);
    }

    #[test]
    fn unknown_and_missing_semantics_have_typed_diagnostics() {
        let cases = [
            (
                r#"digraph G { start [shape="Mdiamond"] work [shape="ellipse"] done [shape="Msquare"] start -> work -> done }"#,
                SemanticDiagnosticKind::UnknownShape,
            ),
            (
                r#"digraph G { start [shape="Mdiamond"] work [shape="ellipse", type="typo"] done [shape="Msquare"] start -> work -> done }"#,
                SemanticDiagnosticKind::UnknownHandler,
            ),
            (
                r#"digraph G { start [shape="Mdiamond"] work [shape="box", llm_provider="llama"] done [shape="Msquare"] start -> work -> done }"#,
                SemanticDiagnosticKind::UnknownProvider,
            ),
            (
                r#"digraph G { start [shape="Mdiamond"] work [shape="box"] done [shape="Msquare"] start -> work -> done }"#,
                SemanticDiagnosticKind::MissingProvider,
            ),
        ];

        for (dot, expected_kind) in cases {
            let error = ExecutionPlan::compile(graph(dot)).unwrap_err();
            assert!(
                error.diagnostics.iter().any(|diagnostic| {
                    diagnostic.kind == expected_kind
                        && diagnostic.node_id.as_deref() == Some("work")
                        && !diagnostic.fix.is_empty()
                }),
                "expected {expected_kind:?}, got {:?}",
                error.diagnostics
            );
        }
    }

    #[test]
    fn non_string_semantic_attributes_fail_with_typed_diagnostics() {
        let node_cases = [
            ("shape", "123", "integer"),
            ("type", "true", "boolean"),
            ("node_type", "1.5", "float"),
            ("handler", "1s", "duration"),
            ("prompt", "false", "boolean"),
            ("llm_provider", "7", "integer"),
            ("class", "2.5", "float"),
            ("classes", "3s", "duration"),
        ];

        for (key, value, value_type) in node_cases {
            let source = format!(
                r#"digraph G {{
                    start [shape="Mdiamond"]
                    work [shape="box", llm_provider="claude", {key}={value}]
                    done [shape="Msquare"]
                    start -> work -> done
                }}"#
            );
            let error = ExecutionPlan::compile(graph(&source)).unwrap_err();
            assert!(
                error.diagnostics.iter().any(|diagnostic| {
                    diagnostic.kind == SemanticDiagnosticKind::InvalidAttributeType
                        && diagnostic.node_id.as_deref() == Some("work")
                        && diagnostic.message.contains(key)
                        && diagnostic.message.contains(value_type)
                        && !diagnostic.fix.is_empty()
                }),
                "expected invalid type for {key}={value}, got {:?}",
                error.diagnostics
            );
        }

        for (key, value, value_type) in [
            ("stylesheet", "true", "boolean"),
            ("model_stylesheet", "2s", "duration"),
        ] {
            let source = format!(
                r#"digraph G {{
                    {key}={value}
                    start [shape="Mdiamond"]
                    done [shape="Msquare"]
                    start -> done
                }}"#
            );
            let error = ExecutionPlan::compile(graph(&source)).unwrap_err();
            assert!(
                error.diagnostics.iter().any(|diagnostic| {
                    diagnostic.kind == SemanticDiagnosticKind::InvalidAttributeType
                        && diagnostic.node_id.is_none()
                        && diagnostic.message.contains(key)
                        && diagnostic.message.contains(value_type)
                        && !diagnostic.fix.is_empty()
                }),
                "expected invalid graph type for {key}={value}, got {:?}",
                error.diagnostics
            );
        }
    }

    #[test]
    fn generation_defaults_exactly_provider_consuming_nodes() {
        let compilation = ExecutionPlan::compile_for_generation(
            graph(
                r#"digraph G {
                    start [shape="Mdiamond"]
                    task [shape="ellipse", type="codergen"]
                    route [shape="diamond"]
                    llm_route [shape="diamond", prompt="choose"]
                    quality [shape="box", type="quality"]
                    done [shape="Msquare"]
                    start -> task -> route -> llm_route -> quality -> done
                }"#,
            ),
            LlmProvider::Claude,
        )
        .unwrap();

        assert_eq!(
            compilation.defaulted_provider_nodes,
            vec!["llm_route".to_string(), "task".to_string()]
        );
        assert_eq!(
            compilation.plan.node("route").unwrap().provider,
            None,
            "pass-through conditionals do not consume providers"
        );
        assert_eq!(compilation.plan.node("quality").unwrap().provider, None);
    }
}
