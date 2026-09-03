use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use attractor_pipeline::{
    preflight_run_plan, validate_plan, ExecutionPlan, ExitHandler, HandlerIdentity,
    HandlerRegistry, LlmProvider, NodeHandler, PipelineExecutor, PipelineGraph, PipelineNode,
    ProviderNodeHandler, ResolvedNode, ResolvedNodeHandler, ResolvedNodeKind, StartHandler,
};
use attractor_types::{Context, Outcome, Result};

struct RecordingCodergen {
    calls: Arc<Mutex<Vec<(String, LlmProvider)>>>,
}

#[async_trait]
impl NodeHandler for RecordingCodergen {
    fn handler_type(&self) -> &str {
        "codergen"
    }

    fn provider_handler(&self) -> Option<&dyn ProviderNodeHandler> {
        Some(self)
    }

    async fn execute(
        &self,
        _node: &PipelineNode,
        _context: &Context,
        _graph: &PipelineGraph,
    ) -> Result<Outcome> {
        panic!("engine used the legacy raw-node dispatch path")
    }
}

#[async_trait]
impl ProviderNodeHandler for RecordingCodergen {
    async fn execute_resolved(
        &self,
        node: &PipelineNode,
        resolved: &ResolvedNode,
        _context: &Context,
        _graph: &PipelineGraph,
    ) -> Result<Outcome> {
        self.calls
            .lock()
            .unwrap()
            .push((node.id.clone(), resolved.provider.unwrap()));
        Ok(Outcome::success("recorded"))
    }
}

#[test]
fn registry_compilation_rejects_unavailable_builtin_handlers() {
    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(ExitHandler);

    let error = ExecutionPlan::compile_with_registry(
        graph(
            r#"digraph G {
                start [shape="Mdiamond"]
                work [shape="box", llm_provider="codex"]
                done [shape="Msquare"]
                start -> work -> done
            }"#,
        ),
        &registry,
    )
    .unwrap_err();

    assert!(error.diagnostics.iter().any(|diagnostic| {
        diagnostic.kind == attractor_pipeline::SemanticDiagnosticKind::UnknownHandler
            && diagnostic.node_id.as_deref() == Some("work")
            && diagnostic.message.contains("codergen")
    }));
}

#[tokio::test]
async fn precompiled_plan_rejects_missing_later_handler_before_execution_or_checkpoint() {
    struct CountingHandler {
        name: &'static str,
        consumes_provider: bool,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl NodeHandler for CountingHandler {
        fn handler_type(&self) -> &str {
            self.name
        }

        fn provider_handler(&self) -> Option<&dyn ProviderNodeHandler> {
            self.consumes_provider
                .then_some(self as &dyn ProviderNodeHandler)
        }

        async fn execute(
            &self,
            _node: &PipelineNode,
            _context: &Context,
            _graph: &PipelineGraph,
        ) -> Result<Outcome> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Outcome::success("unexpected"))
        }
    }

    #[async_trait]
    impl ProviderNodeHandler for CountingHandler {
        async fn execute_resolved(
            &self,
            _node: &PipelineNode,
            _resolved: &ResolvedNode,
            _context: &Context,
            _graph: &PipelineGraph,
        ) -> Result<Outcome> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Outcome::success("unexpected"))
        }
    }

    let plan = ExecutionPlan::compile(graph(
        r#"digraph G {
            start [shape="Mdiamond"]
            work [shape="box", llm_provider="codex"]
            review [shape="hexagon"]
            done [shape="Msquare"]
            start -> work -> review -> done
        }"#,
    ))
    .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = HandlerRegistry::new();
    for (name, consumes_provider) in [("start", false), ("codergen", true), ("exit", false)] {
        registry.register(CountingHandler {
            name,
            consumes_provider,
            calls: Arc::clone(&calls),
        });
    }
    let logs = tempfile::tempdir().unwrap();

    let error = PipelineExecutor::new(registry)
        .run_plan_with_checkpoint(&plan, Context::new(), logs.path())
        .await
        .unwrap_err();

    assert!(error.to_string().contains("wait.human"), "{error}");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(!logs.path().join("checkpoint.json").exists());
}

#[tokio::test]
async fn precompiled_plan_rejects_handler_capability_drift_before_execution() {
    struct CustomHandler {
        consumes_provider: bool,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl NodeHandler for CustomHandler {
        fn handler_type(&self) -> &str {
            "custom.provider"
        }

        fn provider_handler(&self) -> Option<&dyn ProviderNodeHandler> {
            self.consumes_provider
                .then_some(self as &dyn ProviderNodeHandler)
        }

        async fn execute(
            &self,
            _node: &PipelineNode,
            _context: &Context,
            _graph: &PipelineGraph,
        ) -> Result<Outcome> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Outcome::success("unexpected"))
        }
    }

    #[async_trait]
    impl ProviderNodeHandler for CustomHandler {
        async fn execute_resolved(
            &self,
            _node: &PipelineNode,
            _resolved: &ResolvedNode,
            _context: &Context,
            _graph: &PipelineGraph,
        ) -> Result<Outcome> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Outcome::success("unexpected"))
        }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let mut compile_registry = HandlerRegistry::new();
    compile_registry.register(StartHandler);
    compile_registry.register(ExitHandler);
    compile_registry.register(CustomHandler {
        consumes_provider: true,
        calls: Arc::clone(&calls),
    });
    let plan = ExecutionPlan::compile_with_registry(
        graph(
            r#"digraph G {
                start [shape="Mdiamond"]
                work [shape="ellipse", type="custom.provider", llm_provider="codex"]
                done [shape="Msquare"]
                start -> work -> done
            }"#,
        ),
        &compile_registry,
    )
    .unwrap();

    let mut execution_registry = HandlerRegistry::new();
    execution_registry.register(StartHandler);
    execution_registry.register(ExitHandler);
    execution_registry.register(CustomHandler {
        consumes_provider: false,
        calls: Arc::clone(&calls),
    });

    let error = PipelineExecutor::new(execution_registry)
        .run_plan(&plan)
        .await
        .unwrap_err();

    assert!(error.to_string().contains("provider capability"), "{error}");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn custom_handler_capability_controls_provider_requirement_and_normalization() {
    struct ProviderCustom;

    #[async_trait]
    impl NodeHandler for ProviderCustom {
        fn handler_type(&self) -> &str {
            "custom.provider"
        }

        fn provider_handler(&self) -> Option<&dyn ProviderNodeHandler> {
            Some(self)
        }

        async fn execute(
            &self,
            _node: &PipelineNode,
            _context: &Context,
            _graph: &PipelineGraph,
        ) -> Result<Outcome> {
            Ok(Outcome::success("custom provider"))
        }
    }

    #[async_trait]
    impl ProviderNodeHandler for ProviderCustom {
        async fn execute_resolved(
            &self,
            _node: &PipelineNode,
            _resolved: &ResolvedNode,
            _context: &Context,
            _graph: &PipelineGraph,
        ) -> Result<Outcome> {
            Ok(Outcome::success("custom provider"))
        }
    }

    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(ExitHandler);
    registry.register(ProviderCustom);

    let missing = ExecutionPlan::compile_with_registry(
        graph(
            r#"digraph G {
                start [shape="Mdiamond"]
                review [shape="ellipse", type="custom.provider"]
                done [shape="Msquare"]
                start -> review -> done
            }"#,
        ),
        &registry,
    )
    .unwrap_err();
    assert!(missing.diagnostics.iter().any(|diagnostic| {
        diagnostic.kind == attractor_pipeline::SemanticDiagnosticKind::MissingProvider
            && diagnostic.node_id.as_deref() == Some("review")
    }));

    let plan = ExecutionPlan::compile_with_registry(
        graph(
            r#"digraph G {
                start [shape="Mdiamond"]
                review [shape="ellipse", type="custom.provider", llm_provider="OpenAI"]
                done [shape="Msquare"]
                start -> review -> done
            }"#,
        ),
        &registry,
    )
    .unwrap();
    assert_eq!(
        plan.node("review").unwrap().provider,
        Some(LlmProvider::Codex)
    );
}

fn graph(dot: &str) -> PipelineGraph {
    PipelineGraph::from_dot(attractor_dot::parse(dot).unwrap()).unwrap()
}

#[tokio::test]
async fn supported_matrix_agrees_across_all_semantic_consumers() {
    #[derive(Clone)]
    struct Row {
        name: &'static str,
        node: &'static str,
        kind: ResolvedNodeKind,
        handler: HandlerIdentity,
        handler_name: &'static str,
        consumes_provider: bool,
    }

    type RecordedCall = (String, HandlerIdentity, Option<LlmProvider>);

    struct RecordingHandler {
        name: &'static str,
        consumes_provider: bool,
        calls: Arc<Mutex<Vec<RecordedCall>>>,
    }

    #[async_trait]
    impl NodeHandler for RecordingHandler {
        fn handler_type(&self) -> &str {
            self.name
        }

        fn provider_handler(&self) -> Option<&dyn ProviderNodeHandler> {
            self.consumes_provider
                .then_some(self as &dyn ProviderNodeHandler)
        }

        fn resolved_handler(&self) -> Option<&dyn ResolvedNodeHandler> {
            (!self.consumes_provider).then_some(self as &dyn ResolvedNodeHandler)
        }

        async fn execute(
            &self,
            _node: &PipelineNode,
            _context: &Context,
            _graph: &PipelineGraph,
        ) -> Result<Outcome> {
            panic!("matrix execution bypassed resolved semantics")
        }
    }

    #[async_trait]
    impl ResolvedNodeHandler for RecordingHandler {
        async fn execute_resolved(
            &self,
            node: &PipelineNode,
            resolved: &ResolvedNode,
            _context: &Context,
            _graph: &PipelineGraph,
        ) -> Result<Outcome> {
            self.calls.lock().unwrap().push((
                node.id.clone(),
                resolved.handler.clone(),
                resolved.provider,
            ));
            Ok(Outcome::success("recorded"))
        }
    }

    #[async_trait]
    impl ProviderNodeHandler for RecordingHandler {
        async fn execute_resolved(
            &self,
            node: &PipelineNode,
            resolved: &ResolvedNode,
            context: &Context,
            graph: &PipelineGraph,
        ) -> Result<Outcome> {
            ResolvedNodeHandler::execute_resolved(self, node, resolved, context, graph).await
        }
    }

    let rows = [
        Row {
            name: "default task",
            node: r#"subject [prompt="work"]"#,
            kind: ResolvedNodeKind::Task,
            handler: HandlerIdentity::Codergen,
            handler_name: "codergen",
            consumes_provider: true,
        },
        Row {
            name: "box task",
            node: r#"subject [shape="box", prompt="work"]"#,
            kind: ResolvedNodeKind::Task,
            handler: HandlerIdentity::Codergen,
            handler_name: "codergen",
            consumes_provider: true,
        },
        Row {
            name: "typed task",
            node: r#"subject [shape="ellipse", type="codergen", prompt="work"]"#,
            kind: ResolvedNodeKind::Task,
            handler: HandlerIdentity::Codergen,
            handler_name: "codergen",
            consumes_provider: true,
        },
        Row {
            name: "prompted diamond",
            node: r#"subject [shape="diamond", prompt="choose"]"#,
            kind: ResolvedNodeKind::Conditional { llm_backed: true },
            handler: HandlerIdentity::Codergen,
            handler_name: "codergen",
            consumes_provider: true,
        },
        Row {
            name: "diamond with explicit codergen handler",
            node: r#"subject [shape="diamond", type="codergen", prompt="choose"]"#,
            kind: ResolvedNodeKind::Conditional { llm_backed: true },
            handler: HandlerIdentity::Codergen,
            handler_name: "codergen",
            consumes_provider: true,
        },
        Row {
            name: "unprompted diamond with explicit codergen handler",
            node: r#"subject [shape="diamond", type="codergen"]"#,
            kind: ResolvedNodeKind::Conditional { llm_backed: true },
            handler: HandlerIdentity::Codergen,
            handler_name: "codergen",
            consumes_provider: true,
        },
        Row {
            name: "typed prompted conditional",
            node: r#"subject [type="conditional", prompt="choose"]"#,
            kind: ResolvedNodeKind::Conditional { llm_backed: true },
            handler: HandlerIdentity::Codergen,
            handler_name: "codergen",
            consumes_provider: true,
        },
        Row {
            name: "diamond router",
            node: r#"subject [shape="diamond"]"#,
            kind: ResolvedNodeKind::Conditional { llm_backed: false },
            handler: HandlerIdentity::Conditional,
            handler_name: "conditional",
            consumes_provider: false,
        },
        Row {
            name: "typed router",
            node: r#"subject [type="conditional"]"#,
            kind: ResolvedNodeKind::Conditional { llm_backed: false },
            handler: HandlerIdentity::Conditional,
            handler_name: "conditional",
            consumes_provider: false,
        },
        Row {
            name: "human shape",
            node: r#"subject [shape="hexagon"]"#,
            kind: ResolvedNodeKind::HumanGate,
            handler: HandlerIdentity::WaitHuman,
            handler_name: "wait.human",
            consumes_provider: false,
        },
        Row {
            name: "human type",
            node: r#"subject [type="wait.human"]"#,
            kind: ResolvedNodeKind::HumanGate,
            handler: HandlerIdentity::WaitHuman,
            handler_name: "wait.human",
            consumes_provider: false,
        },
        Row {
            name: "tool shape",
            node: r#"subject [shape="parallelogram"]"#,
            kind: ResolvedNodeKind::Tool,
            handler: HandlerIdentity::Tool,
            handler_name: "tool",
            consumes_provider: false,
        },
        Row {
            name: "tool type",
            node: r#"subject [type="tool"]"#,
            kind: ResolvedNodeKind::Tool,
            handler: HandlerIdentity::Tool,
            handler_name: "tool",
            consumes_provider: false,
        },
        Row {
            name: "parallel shape",
            node: r#"subject [shape="component"]"#,
            kind: ResolvedNodeKind::Parallel,
            handler: HandlerIdentity::Parallel,
            handler_name: "parallel",
            consumes_provider: false,
        },
        Row {
            name: "parallel type",
            node: r#"subject [type="parallel"]"#,
            kind: ResolvedNodeKind::Parallel,
            handler: HandlerIdentity::Parallel,
            handler_name: "parallel",
            consumes_provider: false,
        },
        Row {
            name: "fan-in shape",
            node: r#"subject [shape="tripleoctagon"]"#,
            kind: ResolvedNodeKind::FanIn,
            handler: HandlerIdentity::FanIn,
            handler_name: "parallel.fan_in",
            consumes_provider: false,
        },
        Row {
            name: "fan-in alias",
            node: r#"subject [type="fan_in"]"#,
            kind: ResolvedNodeKind::FanIn,
            handler: HandlerIdentity::FanIn,
            handler_name: "parallel.fan_in",
            consumes_provider: false,
        },
        Row {
            name: "fan-in type",
            node: r#"subject [type="parallel.fan_in"]"#,
            kind: ResolvedNodeKind::FanIn,
            handler: HandlerIdentity::FanIn,
            handler_name: "parallel.fan_in",
            consumes_provider: false,
        },
        Row {
            name: "manager shape",
            node: r#"subject [shape="house"]"#,
            kind: ResolvedNodeKind::ManagerLoop,
            handler: HandlerIdentity::ManagerLoop,
            handler_name: "stack.manager_loop",
            consumes_provider: false,
        },
        Row {
            name: "manager alias",
            node: r#"subject [type="manager"]"#,
            kind: ResolvedNodeKind::ManagerLoop,
            handler: HandlerIdentity::ManagerLoop,
            handler_name: "stack.manager_loop",
            consumes_provider: false,
        },
        Row {
            name: "manager type",
            node: r#"subject [type="stack.manager_loop"]"#,
            kind: ResolvedNodeKind::ManagerLoop,
            handler: HandlerIdentity::ManagerLoop,
            handler_name: "stack.manager_loop",
            consumes_provider: false,
        },
        Row {
            name: "quality",
            node: r#"subject [shape="box", type="quality"]"#,
            kind: ResolvedNodeKind::Quality,
            handler: HandlerIdentity::Quality,
            handler_name: "quality",
            consumes_provider: false,
        },
        Row {
            name: "custom provider",
            node: r#"subject [shape="ellipse", type="custom.provider"]"#,
            kind: ResolvedNodeKind::Custom,
            handler: HandlerIdentity::Custom("custom.provider".into()),
            handler_name: "custom.provider",
            consumes_provider: true,
        },
    ];

    for row in rows {
        let source = format!(
            "digraph G {{ start [shape=\"Mdiamond\"] {} done [shape=\"Msquare\"] start -> subject -> done }}",
            row.node
        );
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut registry = HandlerRegistry::new();
        registry.register(StartHandler);
        registry.register(ExitHandler);
        registry.register(RecordingHandler {
            name: row.handler_name,
            consumes_provider: row.consumes_provider,
            calls: Arc::clone(&calls),
        });

        let compilation = ExecutionPlan::compile_for_generation_with_registry(
            graph(&source),
            &registry,
            LlmProvider::Codex,
        )
        .unwrap_or_else(|error| panic!("{} did not compile: {error}", row.name));
        let expected_defaulted = if row.consumes_provider {
            vec!["subject".to_string()]
        } else {
            Vec::new()
        };
        assert_eq!(
            compilation.defaulted_provider_nodes, expected_defaulted,
            "generation/defaulting for {}",
            row.name
        );

        let plan = compilation.plan;
        let subject = plan.node("subject").unwrap();
        assert_eq!(subject.kind, row.kind, "kind for {}", row.name);
        assert_eq!(subject.handler, row.handler, "handler for {}", row.name);
        let expected_provider = row.consumes_provider.then_some(LlmProvider::Codex);
        assert_eq!(
            subject.provider, expected_provider,
            "provider for {}",
            row.name
        );
        assert!(
            validate_plan(&plan)
                .iter()
                .all(|diagnostic| diagnostic.severity != attractor_pipeline::Severity::Error),
            "validation for {}",
            row.name
        );

        let workdir = tempfile::tempdir().unwrap();
        let preflight = preflight_run_plan(&plan, workdir.path());
        assert_eq!(
            preflight
                .iter()
                .any(|finding| finding.code == "PROVIDER_COST_UNTRACKED"),
            row.consumes_provider,
            "preflight provider for {}",
            row.name
        );

        let result = PipelineExecutor::new(registry).run_plan(&plan).await;
        assert!(result.is_ok(), "execution for {}: {result:?}", row.name);
        assert_eq!(
            *calls.lock().unwrap(),
            vec![("subject".into(), row.handler, expected_provider)],
            "dispatch for {}",
            row.name
        );
    }
}

#[tokio::test]
async fn one_compiled_plan_drives_validation_preflight_dispatch_and_execution() {
    let plan = ExecutionPlan::compile(graph(
        r#"digraph G {
            start [shape="Mdiamond"]
            work [shape="box", prompt="work", llm_provider="OpenAI", timeout=1s]
            done [shape="Msquare"]
            start -> work -> done
        }"#,
    ))
    .unwrap();

    assert!(validate_plan(&plan).is_empty());
    let workdir = tempfile::tempdir().unwrap();
    let findings = preflight_run_plan(&plan, workdir.path());
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].code, "PROVIDER_COST_UNTRACKED");
    assert!(findings[0].message.contains("Codex"));

    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(ExitHandler);
    registry.register(RecordingCodergen {
        calls: Arc::clone(&calls),
    });
    let result = PipelineExecutor::new(registry)
        .run_plan(&plan)
        .await
        .unwrap();

    assert_eq!(
        *calls.lock().unwrap(),
        vec![("work".to_string(), LlmProvider::Codex)]
    );
    assert_eq!(result.completed_nodes, vec!["start", "work", "done"]);
    assert_eq!(
        plan.node("work").unwrap().provider.unwrap().as_str(),
        "codex"
    );
}

#[tokio::test]
async fn validation_and_execution_share_magic_start_and_exit_membership() {
    let plan = ExecutionPlan::compile(graph(
        r#"digraph G {
            start
            done
            start -> done
        }"#,
    ))
    .unwrap();
    assert!(validate_plan(&plan).is_empty());

    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(ExitHandler);
    let result = PipelineExecutor::new(registry)
        .run_plan(&plan)
        .await
        .unwrap();

    assert_eq!(result.completed_nodes, vec!["start", "done"]);
    assert_eq!(plan.start_id(), "start");
    assert!(plan.is_exit("done"));
}

#[tokio::test]
async fn validation_and_execution_share_case_insensitive_magic_membership() {
    let plan = ExecutionPlan::compile(graph(
        r#"digraph G {
            START
            DONE
            START -> DONE
        }"#,
    ))
    .unwrap();

    assert!(validate_plan(&plan).is_empty());

    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(ExitHandler);
    let result = PipelineExecutor::new(registry)
        .run_plan(&plan)
        .await
        .unwrap();

    assert_eq!(result.completed_nodes, vec!["START", "DONE"]);
    assert_eq!(plan.start_id(), "START");
    assert!(plan.is_exit("DONE"));
}

#[tokio::test]
async fn provider_consuming_custom_handler_receives_normalized_provider() {
    struct RecordingCustomProvider(Arc<Mutex<Vec<LlmProvider>>>);

    #[async_trait]
    impl NodeHandler for RecordingCustomProvider {
        fn handler_type(&self) -> &str {
            "custom.provider"
        }

        fn provider_handler(&self) -> Option<&dyn ProviderNodeHandler> {
            Some(self)
        }

        async fn execute(
            &self,
            _node: &PipelineNode,
            _context: &Context,
            _graph: &PipelineGraph,
        ) -> Result<Outcome> {
            panic!("provider-consuming handler was dispatched without resolved semantics")
        }
    }

    #[async_trait]
    impl ProviderNodeHandler for RecordingCustomProvider {
        async fn execute_resolved(
            &self,
            _node: &PipelineNode,
            resolved: &ResolvedNode,
            _context: &Context,
            _graph: &PipelineGraph,
        ) -> Result<Outcome> {
            self.0.lock().unwrap().push(resolved.provider.unwrap());
            Ok(Outcome::success("custom provider"))
        }
    }

    let providers = Arc::new(Mutex::new(Vec::new()));
    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(ExitHandler);
    registry.register(RecordingCustomProvider(Arc::clone(&providers)));
    let plan = ExecutionPlan::compile_with_registry(
        graph(
            r#"digraph G {
                start [shape="Mdiamond"]
                work [shape="ellipse", type="custom.provider", llm_provider="OpenAI"]
                done [shape="Msquare"]
                start -> work -> done
            }"#,
        ),
        &registry,
    )
    .unwrap();

    PipelineExecutor::new(registry)
        .run_plan(&plan)
        .await
        .unwrap();

    assert_eq!(*providers.lock().unwrap(), vec![LlmProvider::Codex]);
}

#[tokio::test]
async fn registered_custom_handler_compiles_and_dispatches_by_identity() {
    struct RecordingCustom(Arc<Mutex<Vec<String>>>);

    #[async_trait]
    impl NodeHandler for RecordingCustom {
        fn handler_type(&self) -> &str {
            "custom.review"
        }

        async fn execute(
            &self,
            node: &PipelineNode,
            _context: &Context,
            _graph: &PipelineGraph,
        ) -> Result<Outcome> {
            self.0.lock().unwrap().push(node.id.clone());
            Ok(Outcome::success("custom"))
        }
    }

    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(ExitHandler);
    registry.register(RecordingCustom(Arc::clone(&calls)));
    let plan = ExecutionPlan::compile_with_registry(
        graph(
            r#"digraph G {
                start [shape="Mdiamond"]
                review [shape="ellipse", type="custom.review"]
                done [shape="Msquare"]
                start -> review -> done
            }"#,
        ),
        &registry,
    )
    .unwrap();

    let result = PipelineExecutor::new(registry)
        .run_plan(&plan)
        .await
        .unwrap();

    assert_eq!(*calls.lock().unwrap(), vec!["review"]);
    assert_eq!(result.completed_nodes, vec!["start", "review", "done"]);
}
