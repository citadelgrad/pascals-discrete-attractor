//! Node handler trait, dynamic dispatch wrapper, and handler registry.

use std::collections::HashMap;

use async_trait::async_trait;

use attractor_types::{Context, Outcome, Result};

use crate::execution_plan::ResolvedNode;
use crate::graph::{PipelineGraph, PipelineNode};

// ---------------------------------------------------------------------------
// Handler traits
// ---------------------------------------------------------------------------

/// Optional typed execution contract for non-provider handlers that inspect
/// canonical node semantics.
#[async_trait]
pub trait ResolvedNodeHandler: Send + Sync {
    async fn execute_resolved(
        &self,
        node: &PipelineNode,
        resolved: &ResolvedNode,
        context: &Context,
        graph: &PipelineGraph,
    ) -> Result<Outcome>;
}

/// Typed execution contract for handlers that consume compiled provider semantics.
///
/// A provider-consuming handler is identified by returning this interface from
/// [`NodeHandler::provider_handler`]. This couples the capability declaration to
/// an execution method that receives the canonical [`ResolvedNode`], preventing
/// provider-backed handlers from falling through to raw stringly dispatch.
#[async_trait]
pub trait ProviderNodeHandler: Send + Sync {
    async fn execute_resolved(
        &self,
        node: &PipelineNode,
        resolved: &ResolvedNode,
        context: &Context,
        graph: &PipelineGraph,
    ) -> Result<Outcome>;
}

#[async_trait]
pub trait NodeHandler: Send + Sync {
    /// The handler type identifier (e.g. "start", "exit", "codergen").
    fn handler_type(&self) -> &str;

    /// Return the typed executor when this handler consumes an LLM provider.
    fn provider_handler(&self) -> Option<&dyn ProviderNodeHandler> {
        None
    }

    /// Return an optional typed executor for a non-provider handler.
    fn resolved_handler(&self) -> Option<&dyn ResolvedNodeHandler> {
        None
    }

    /// Execute this handler for a given node.
    async fn execute(
        &self,
        node: &PipelineNode,
        context: &Context,
        graph: &PipelineGraph,
    ) -> Result<Outcome>;
}

// ---------------------------------------------------------------------------
// DynHandler — object-safe wrapper
// ---------------------------------------------------------------------------

pub struct DynHandler(Box<dyn NodeHandler>);

impl DynHandler {
    pub fn new(handler: impl NodeHandler + 'static) -> Self {
        Self(Box::new(handler))
    }

    pub fn handler_type(&self) -> &str {
        self.0.handler_type()
    }

    pub async fn execute(
        &self,
        node: &PipelineNode,
        context: &Context,
        graph: &PipelineGraph,
    ) -> Result<Outcome> {
        self.0.execute(node, context, graph).await
    }

    /// Execute through the canonical resolved-semantic dispatch path.
    pub async fn execute_resolved(
        &self,
        node: &PipelineNode,
        resolved: &ResolvedNode,
        context: &Context,
        graph: &PipelineGraph,
    ) -> Result<Outcome> {
        match self.0.provider_handler() {
            Some(handler) => {
                handler
                    .execute_resolved(node, resolved, context, graph)
                    .await
            }
            None => match self.0.resolved_handler() {
                Some(handler) => {
                    handler
                        .execute_resolved(node, resolved, context, graph)
                        .await
                }
                None => self.0.execute(node, context, graph).await,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// HandlerRegistry
// ---------------------------------------------------------------------------

pub struct HandlerRegistry {
    handlers: HashMap<String, DynHandler>,
}

impl HandlerRegistry {
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
        }
    }

    pub fn register(&mut self, handler: impl NodeHandler + 'static) {
        let t = handler.handler_type().to_string();
        self.handlers.insert(t, DynHandler::new(handler));
    }

    pub fn get(&self, handler_type: &str) -> Option<&DynHandler> {
        self.handlers.get(handler_type)
    }

    pub fn has(&self, handler_type: &str) -> bool {
        self.handlers.contains_key(handler_type)
    }

    pub fn handler_catalog(&self) -> std::collections::HashSet<String> {
        self.handlers.keys().cloned().collect()
    }

    pub(crate) fn handler_capabilities(&self) -> HashMap<String, bool> {
        self.handlers
            .iter()
            .map(|(name, handler)| (name.clone(), handler.0.provider_handler().is_some()))
            .collect()
    }

    /// Legacy raw-node classification retained for consumers that have not
    /// migrated to `ExecutionPlan` yet. Non-web execution compiles semantics
    /// once and never calls this compatibility method.
    pub fn resolve_type(&self, node: &PipelineNode) -> String {
        if let Some(node_type) = &node.node_type {
            if node_type == "conditional" && node.prompt.is_some() {
                return "codergen".to_string();
            }
            return node_type.clone();
        }

        match node.shape.as_str() {
            "Mdiamond" => "start",
            "Msquare" => "exit",
            "diamond" if node.prompt.is_some() => "codergen",
            "diamond" => "conditional",
            "hexagon" => "wait.human",
            "parallelogram" => "tool",
            "component" => "parallel",
            "tripleoctagon" => "parallel.fan_in",
            "house" => "stack.manager_loop",
            _ => "codergen",
        }
        .to_string()
    }
}

impl Default for HandlerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Built-in handlers
// ---------------------------------------------------------------------------

pub struct StartHandler;

#[async_trait]
impl NodeHandler for StartHandler {
    fn handler_type(&self) -> &str {
        "start"
    }

    async fn execute(
        &self,
        _node: &PipelineNode,
        _ctx: &Context,
        _graph: &PipelineGraph,
    ) -> Result<Outcome> {
        Ok(Outcome::success("Pipeline started"))
    }
}

pub struct ExitHandler;

#[async_trait]
impl NodeHandler for ExitHandler {
    fn handler_type(&self) -> &str {
        "exit"
    }

    async fn execute(
        &self,
        _node: &PipelineNode,
        _ctx: &Context,
        _graph: &PipelineGraph,
    ) -> Result<Outcome> {
        Ok(Outcome::success("Pipeline completed"))
    }
}

pub struct ConditionalHandler;

#[async_trait]
impl NodeHandler for ConditionalHandler {
    fn handler_type(&self) -> &str {
        "conditional"
    }

    async fn execute(
        &self,
        _node: &PipelineNode,
        _ctx: &Context,
        _graph: &PipelineGraph,
    ) -> Result<Outcome> {
        Ok(Outcome::success("Conditional pass-through"))
    }
}

// ---------------------------------------------------------------------------
// Default registry factory
// ---------------------------------------------------------------------------

pub fn default_registry() -> HandlerRegistry {
    let mut reg = HandlerRegistry::new();
    reg.register(StartHandler);
    reg.register(ExitHandler);
    reg.register(ConditionalHandler);
    reg.register(crate::handlers::ToolHandler);
    reg.register(crate::handlers::QualityHandler);
    reg.register(crate::handlers::CodergenHandler);
    reg.register(crate::handlers::ParallelHandler);
    reg.register(crate::handlers::FanInHandler);
    reg.register(crate::handlers::ManagerLoopHandler);
    reg
}

/// Create the default handler registry with WaitHumanHandler registered.
///
/// This factory function creates a registry with all the standard handlers
/// plus WaitHumanHandler configured with the provided interviewer.
/// Use this when you need to support hexagon (human review) nodes in pipelines.
pub fn default_registry_with_interviewer(
    interviewer: std::sync::Arc<dyn crate::interviewer::Interviewer>,
) -> HandlerRegistry {
    let mut reg = default_registry();
    reg.register(crate::handlers::wait_human::WaitHumanHandler::new(
        interviewer,
    ));
    reg
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[cfg(unix)]
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    use std::process::Command;

    fn make_node(id: &str, shape: &str, node_type: Option<&str>) -> PipelineNode {
        PipelineNode {
            id: id.to_string(),
            label: id.to_string(),
            shape: shape.to_string(),
            node_type: node_type.map(String::from),
            prompt: None,
            max_retries: 0,
            goal_gate: false,
            retry_target: None,
            fallback_retry_target: None,
            fidelity: None,
            thread_id: None,
            classes: Vec::new(),
            timeout: None,
            llm_model: None,
            llm_provider: None,
            reasoning_effort: None,
            auto_status: true,
            allow_partial: false,
            raw_attrs: HashMap::new(),
        }
    }

    #[test]
    fn register_and_get_handler() {
        let mut reg = HandlerRegistry::new();
        reg.register(StartHandler);
        assert!(reg.has("start"));
        assert!(reg.get("start").is_some());
        assert!(!reg.has("nonexistent"));
        assert!(reg.get("nonexistent").is_none());
    }

    #[tokio::test]
    async fn legacy_dynamic_api_remains_available_for_unchanged_consumers() {
        let mut reg = HandlerRegistry::new();
        reg.register(StartHandler);
        let node = make_node("s", "Mdiamond", None);
        let graph = make_minimal_graph();
        let context = Context::default();

        assert_eq!(reg.resolve_type(&node), "start");
        let outcome = reg
            .get("start")
            .unwrap()
            .execute(&node, &context, &graph)
            .await
            .unwrap();
        assert_eq!(outcome.status, attractor_types::StageStatus::Success);
    }

    #[tokio::test]
    async fn legacy_codergen_call_preserves_claude_fallback_for_unknown_provider() {
        let graph = PipelineGraph::from_dot(
            attractor_dot::parse(
                r#"digraph G {
                    start [shape="Mdiamond"]
                    work [shape="box", prompt="work", llm_provider="legacy-provider"]
                    done [shape="Msquare"]
                    start -> work -> done
                }"#,
            )
            .unwrap(),
        )
        .unwrap();
        let context = Context::default();
        context.set("dry_run", serde_json::Value::Bool(true)).await;
        let registry = default_registry();
        let node = graph.node("work").unwrap();

        let outcome = registry
            .get(&registry.resolve_type(node))
            .unwrap()
            .execute(node, &context, &graph)
            .await
            .unwrap();

        assert_eq!(
            outcome.context_updates["work.provider"],
            serde_json::Value::String("Claude Code".into())
        );
    }

    #[cfg(unix)]
    #[test]
    fn legacy_prompted_conditional_executes_codergen_and_extracts_label() {
        let shims = tempfile::tempdir().unwrap();
        let claude = shims.path().join("claude");
        fs::write(
            &claude,
            "#!/bin/sh\nprintf '%s\\n' '{\"result\":\"APPROVE\",\"is_error\":false,\"subtype\":\"\",\"total_cost_usd\":0.0,\"num_turns\":1}'\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&claude).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&claude, permissions).unwrap();

        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "handler::tests::legacy_prompted_conditional_child",
                "--nocapture",
            ])
            .env("PATH", shims.path())
            .env("PAS_LEGACY_CONDITIONAL_CHILD", "1")
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "child stdout={}\nchild stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn legacy_prompted_conditional_child() {
        if std::env::var_os("PAS_LEGACY_CONDITIONAL_CHILD").is_none() {
            return;
        }

        let graph = PipelineGraph::from_dot(
            attractor_dot::parse(
                r#"digraph G {
                    start [shape="Mdiamond"]
                    choice [type="conditional", prompt="choose", llm_provider="claude"]
                    approved [shape="diamond"]
                    rejected [shape="diamond"]
                    done [shape="Msquare"]
                    start -> choice
                    choice -> rejected [label="REJECT"]
                    choice -> approved [label="APPROVE"]
                    approved -> done
                    rejected -> done
                }"#,
            )
            .unwrap(),
        )
        .unwrap();
        let registry = default_registry();
        let node = graph.node("choice").unwrap();

        assert_eq!(registry.resolve_type(node), "codergen");
        let outcome = registry
            .get(&registry.resolve_type(node))
            .unwrap()
            .execute(node, &Context::default(), &graph)
            .await
            .unwrap();
        assert_eq!(outcome.preferred_label.as_deref(), Some("APPROVE"));
    }

    #[tokio::test]
    async fn start_handler_returns_success() {
        let handler = StartHandler;
        let node = make_node("s", "Mdiamond", None);
        let ctx = Context::default();
        let graph = make_minimal_graph();
        let outcome = handler.execute(&node, &ctx, &graph).await.unwrap();
        assert_eq!(outcome.status, attractor_types::StageStatus::Success);
        assert_eq!(outcome.notes, "Pipeline started");
    }

    #[tokio::test]
    async fn exit_handler_returns_success() {
        let handler = ExitHandler;
        let node = make_node("e", "Msquare", None);
        let ctx = Context::default();
        let graph = make_minimal_graph();
        let outcome = handler.execute(&node, &ctx, &graph).await.unwrap();
        assert_eq!(outcome.status, attractor_types::StageStatus::Success);
        assert_eq!(outcome.notes, "Pipeline completed");
    }

    #[test]
    fn default_registry_has_builtins() {
        let reg = default_registry();
        assert!(reg.has("start"));
        assert!(reg.has("exit"));
        assert!(reg.has("conditional"));
        assert!(reg.has("tool"));
        assert!(reg.has("codergen"));
        assert!(reg.has("parallel"));
        assert!(reg.has("parallel.fan_in"));
        assert!(reg.has("stack.manager_loop"));
    }

    fn make_minimal_graph() -> PipelineGraph {
        let dot = r#"digraph G { A -> B }"#;
        let parsed = attractor_dot::parse(dot).unwrap();
        PipelineGraph::from_dot(parsed).unwrap()
    }
}
