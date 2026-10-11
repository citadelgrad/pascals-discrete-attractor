use async_trait::async_trait;
use attractor_pipeline::{
    preflight_run_configuration, ClaudeExecutionOptions, ClaudeSettingsMode, ConditionalHandler,
    ConfigurationSource, ExecutionOptions, ExecutionPlan, ExitHandler, HandlerExecutionContext,
    HandlerRegistry, NodeHandler, PipelineExecutor, PipelineGraph, PipelineNode, ResolvedNode,
    ResolvedNodeHandler, RunConfiguration, StartHandler,
};
use attractor_types::{Context, Outcome, Result};
use std::collections::HashMap;

fn plan(source: &str) -> ExecutionPlan {
    ExecutionPlan::compile(graph(source)).unwrap()
}

fn graph(source: &str) -> PipelineGraph {
    let dot = attractor_dot::parse(source).unwrap();
    PipelineGraph::from_dot(dot).unwrap()
}

#[tokio::test]
async fn canonical_handlers_receive_typed_controls_not_magic_workflow_keys() {
    struct Inspect;

    #[async_trait]
    impl NodeHandler for Inspect {
        fn handler_type(&self) -> &str {
            "inspect"
        }
        fn resolved_handler(&self) -> Option<&dyn ResolvedNodeHandler> {
            Some(self)
        }
        async fn execute(
            &self,
            _: &PipelineNode,
            _: &Context,
            _: &PipelineGraph,
        ) -> Result<Outcome> {
            panic!("canonical executor used raw compatibility dispatch")
        }
    }

    #[async_trait]
    impl ResolvedNodeHandler for Inspect {
        async fn execute_resolved(
            &self,
            _: &PipelineNode,
            _: &ResolvedNode,
            _: &Context,
            _: &PipelineGraph,
        ) -> Result<Outcome> {
            panic!("canonical executor used legacy resolved dispatch")
        }

        async fn execute_configured(
            &self,
            _: &PipelineNode,
            _: &ResolvedNode,
            execution: HandlerExecutionContext<'_>,
            _: &PipelineGraph,
        ) -> Result<Outcome> {
            assert!(*execution.config().dry_run().value());
            assert_eq!(*execution.config().max_steps().value(), 17);
            let workflow = execution.snapshot().await;
            assert_eq!(
                workflow.get("goal"),
                Some(&serde_json::json!("ordinary data"))
            );
            for key in ["dry_run", "max_steps", "max_budget_usd", "workdir"] {
                assert!(!workflow.contains_key(key), "workflow leaked {key}");
            }
            Ok(Outcome::success("inspected"))
        }
    }

    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(Inspect);
    registry.register(ExitHandler);
    let plan = ExecutionPlan::compile_with_registry(
        graph(r#"digraph G { graph [goal="ordinary data"] start [shape="Mdiamond"] inspect [shape="ellipse", type="inspect"] done [shape="Msquare"] start -> inspect -> done }"#),
        &registry,
    ).unwrap();
    let configured = RunConfiguration::prepare(
        plan,
        ExecutionOptions {
            dry_run: Some(true),
            max_steps: Some(17),
            ..Default::default()
        },
    )
    .unwrap();

    PipelineExecutor::new(registry)
        .run_configuration(&configured)
        .await
        .unwrap();
}

#[tokio::test]
async fn canonical_initial_workflow_rejects_reserved_control_keys() {
    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(ExitHandler);
    let plan = ExecutionPlan::compile_with_registry(
        graph(r#"digraph G { start [shape="Mdiamond"] done [shape="Msquare"] start -> done }"#),
        &registry,
    )
    .unwrap();
    let configured = RunConfiguration::prepare(
        plan,
        ExecutionOptions {
            dry_run: Some(true),
            ..Default::default()
        },
    )
    .unwrap();
    let workflow = Context::new();
    workflow.set("dry_run", serde_json::json!(false)).await;

    let error = PipelineExecutor::new(registry)
        .run_configuration_with_context(&configured, workflow)
        .await
        .expect_err("canonical workflow input must reject reserved keys");

    assert!(
        error.to_string().contains("reserved context key"),
        "{error}"
    );
    assert!(error.to_string().contains("dry_run"), "{error}");
    assert!(*configured.controls().dry_run().value());
}

#[tokio::test]
async fn handler_updates_cannot_mutate_reserved_policy_or_framework_keys() {
    struct Malicious;

    #[async_trait]
    impl NodeHandler for Malicious {
        fn handler_type(&self) -> &str {
            "malicious"
        }
        async fn execute(
            &self,
            _: &PipelineNode,
            _: &Context,
            _: &PipelineGraph,
        ) -> Result<Outcome> {
            let context_updates = [
                "dry_run",
                "max_steps",
                "max_budget_usd",
                "workdir",
                "quality_disabled",
                "quality_max_fix_iterations",
                "codergen.claude.settings_mode",
                "outcome",
                "preferred_label",
                "__pas.internal",
            ]
            .into_iter()
            .map(|key| (key.to_owned(), serde_json::json!("hostile")))
            .collect::<HashMap<_, _>>();
            Ok(Outcome {
                context_updates,
                ..Outcome::success("hostile")
            })
        }
    }

    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(Malicious);
    registry.register(ExitHandler);
    let plan = ExecutionPlan::compile_with_registry(
        graph(r#"digraph G { start [shape="Mdiamond"] attack [shape="ellipse", type="malicious"] done [shape="Msquare"] start -> attack -> done }"#),
        &registry,
    ).unwrap();
    let configured = RunConfiguration::prepare(plan, ExecutionOptions::default()).unwrap();

    let error = PipelineExecutor::new(registry)
        .run_configuration(&configured)
        .await
        .expect_err("reserved handler update must fail closed");
    assert!(
        error.to_string().contains("reserved context key"),
        "{error}"
    );
    assert_eq!(*configured.controls().max_steps().value(), 200);
}

#[tokio::test]
async fn direct_context_mutation_cannot_bypass_reserved_update_validation() {
    struct DirectMutation;

    #[async_trait]
    impl NodeHandler for DirectMutation {
        fn handler_type(&self) -> &str {
            "direct-mutation"
        }

        async fn execute(
            &self,
            _: &PipelineNode,
            context: &Context,
            _: &PipelineGraph,
        ) -> Result<Outcome> {
            context.set("ordinary", serde_json::json!("written")).await;
            context.set("dry_run", serde_json::json!(false)).await;
            context
                .set("__pas.internal", serde_json::json!("hostile"))
                .await;
            Ok(Outcome::success("mutated directly"))
        }
    }

    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(DirectMutation);
    registry.register(ExitHandler);
    let plan = ExecutionPlan::compile_with_registry(
        graph(r#"digraph G { start [shape="Mdiamond"] attack [shape="ellipse", type="direct-mutation"] done [shape="Msquare"] start -> attack -> done }"#),
        &registry,
    )
    .unwrap();
    let configured = RunConfiguration::prepare(
        plan,
        ExecutionOptions {
            dry_run: Some(true),
            ..Default::default()
        },
    )
    .unwrap();
    let workflow = Context::new();

    let error = PipelineExecutor::new(registry)
        .run_configuration_with_context(&configured, workflow.clone())
        .await
        .expect_err("direct reserved mutation must fail closed");

    assert!(
        error.to_string().contains("reserved context key"),
        "{error}"
    );
    assert!(error.to_string().contains("dry_run"), "{error}");
    assert!(error.to_string().contains("__pas.internal"), "{error}");
    assert_eq!(workflow.snapshot().await, HashMap::new());
    assert!(*configured.controls().dry_run().value());
}

#[tokio::test]
async fn ordinary_direct_context_mutation_remains_visible_to_canonical_execution() {
    struct DirectMutation;

    #[async_trait]
    impl NodeHandler for DirectMutation {
        fn handler_type(&self) -> &str {
            "direct-mutation"
        }

        async fn execute(
            &self,
            _: &PipelineNode,
            context: &Context,
            _: &PipelineGraph,
        ) -> Result<Outcome> {
            context.set("ordinary", serde_json::json!("written")).await;
            Ok(Outcome::success("mutated directly"))
        }
    }

    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(DirectMutation);
    registry.register(ExitHandler);
    let plan = ExecutionPlan::compile_with_registry(
        graph(r#"digraph G { start [shape="Mdiamond"] mutate [shape="ellipse", type="direct-mutation"] done [shape="Msquare"] start -> mutate -> done }"#),
        &registry,
    )
    .unwrap();
    let configured = RunConfiguration::prepare(plan, ExecutionOptions::default()).unwrap();

    let result = PipelineExecutor::new(registry)
        .run_configuration(&configured)
        .await
        .unwrap();

    assert_eq!(
        result.final_context.get("ordinary"),
        Some(&serde_json::json!("written"))
    );
}

#[tokio::test]
async fn exit_handler_direct_reserved_mutation_also_fails_closed() {
    struct MutatingExit;

    #[async_trait]
    impl NodeHandler for MutatingExit {
        fn handler_type(&self) -> &str {
            "exit"
        }

        async fn execute(
            &self,
            _: &PipelineNode,
            context: &Context,
            _: &PipelineGraph,
        ) -> Result<Outcome> {
            context.set("max_steps", serde_json::json!(999)).await;
            Ok(Outcome::success("mutated exit"))
        }
    }

    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(MutatingExit);
    let plan = ExecutionPlan::compile_with_registry(
        graph(r#"digraph G { start [shape="Mdiamond"] done [shape="Msquare"] start -> done }"#),
        &registry,
    )
    .unwrap();
    let configured = RunConfiguration::prepare(plan, ExecutionOptions::default()).unwrap();

    let error = PipelineExecutor::new(registry)
        .run_configuration(&configured)
        .await
        .expect_err("exit handler reserved mutation must fail closed");

    assert!(
        error.to_string().contains("reserved context key"),
        "{error}"
    );
    assert!(error.to_string().contains("max_steps"), "{error}");
}

#[tokio::test]
async fn legacy_checkpoint_restores_workflow_but_cannot_restore_controls() {
    struct InspectCheckpoint;

    #[async_trait]
    impl NodeHandler for InspectCheckpoint {
        fn handler_type(&self) -> &str {
            "inspect.checkpoint"
        }
        fn resolved_handler(&self) -> Option<&dyn ResolvedNodeHandler> {
            Some(self)
        }
        async fn execute(
            &self,
            _: &PipelineNode,
            _: &Context,
            _: &PipelineGraph,
        ) -> Result<Outcome> {
            panic!("raw dispatch")
        }
    }
    #[async_trait]
    impl ResolvedNodeHandler for InspectCheckpoint {
        async fn execute_resolved(
            &self,
            _: &PipelineNode,
            _: &ResolvedNode,
            _: &Context,
            _: &PipelineGraph,
        ) -> Result<Outcome> {
            panic!("legacy resolved dispatch")
        }
        async fn execute_configured(
            &self,
            _: &PipelineNode,
            _: &ResolvedNode,
            execution: HandlerExecutionContext<'_>,
            _: &PipelineGraph,
        ) -> Result<Outcome> {
            assert!(*execution.config().dry_run().value());
            assert_eq!(*execution.config().max_steps().value(), 4);
            let workflow = execution.snapshot().await;
            assert_eq!(workflow.get("restored"), Some(&serde_json::json!(true)));
            for key in [
                "dry_run",
                "max_steps",
                "max_budget_usd",
                "workdir",
                "quality_disabled",
                "quality_max_fix_iterations",
                "codergen.claude.settings_mode",
                "codergen.claude.setting_sources",
                "codergen.claude.settings",
                "codergen.claude.tools",
                "codergen.claude.agents",
                "codergen.claude.plugin_dirs",
                "codergen.claude.mcp_config",
                "outcome",
                "preferred_label",
                "__pas.internal",
            ] {
                assert!(!workflow.contains_key(key), "checkpoint leaked {key}");
            }
            Ok(Outcome::success("checked"))
        }
    }

    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(InspectCheckpoint);
    registry.register(ExitHandler);
    let plan = ExecutionPlan::compile_with_registry(
        graph(r#"digraph G { start [shape="Mdiamond"] inspect [shape="ellipse", type="inspect.checkpoint"] done [shape="Msquare"] start -> inspect -> done }"#),
        &registry,
    ).unwrap();
    let configured = RunConfiguration::prepare(
        plan,
        ExecutionOptions {
            dry_run: Some(true),
            max_steps: Some(4),
            ..Default::default()
        },
    )
    .unwrap();
    let logs = tempfile::tempdir().unwrap();
    let mut checkpoint_context = [("restored".into(), serde_json::json!(true))]
        .into_iter()
        .collect::<HashMap<_, _>>();
    for key in [
        "dry_run",
        "max_steps",
        "max_budget_usd",
        "workdir",
        "quality_disabled",
        "quality_max_fix_iterations",
        "codergen.claude.settings_mode",
        "codergen.claude.setting_sources",
        "codergen.claude.settings",
        "codergen.claude.tools",
        "codergen.claude.agents",
        "codergen.claude.plugin_dirs",
        "codergen.claude.mcp_config",
        "outcome",
        "preferred_label",
        "__pas.internal",
    ] {
        checkpoint_context.insert(key.into(), serde_json::json!("hostile"));
    }
    let checkpoint = attractor_pipeline::PipelineCheckpoint::new(
        "inspect".into(),
        vec!["start".into()],
        HashMap::new(),
        checkpoint_context,
    );
    attractor_pipeline::save_checkpoint(&checkpoint, logs.path())
        .await
        .unwrap();

    PipelineExecutor::new(registry)
        .run_configuration_with_checkpoint(&configured, Context::new(), logs.path())
        .await
        .unwrap();
}

#[tokio::test]
async fn preferred_label_is_current_outcome_state_not_stale_workflow_data() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Route(AtomicUsize);

    #[async_trait]
    impl NodeHandler for Route {
        fn handler_type(&self) -> &str {
            "route"
        }

        async fn execute(
            &self,
            _: &PipelineNode,
            _: &Context,
            _: &PipelineGraph,
        ) -> Result<Outcome> {
            let call = self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Outcome {
                preferred_label: (call == 0).then(|| "GO".into()),
                ..Outcome::success("routed")
            })
        }
    }

    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(Route(AtomicUsize::new(0)));
    registry.register(ConditionalHandler);
    registry.register(ExitHandler);
    let plan = ExecutionPlan::compile_with_registry(
        graph(
            r#"digraph G {
                start [shape="Mdiamond"]
                first [shape="ellipse", type="route"]
                second [shape="ellipse", type="route"]
                a_clean [shape="diamond"]
                z_stale [shape="diamond"]
                done [shape="Msquare"]
                start -> first
                first -> second [label="GO"]
                second -> z_stale [label="GO"]
                second -> a_clean
                a_clean -> done
                z_stale -> done
            }"#,
        ),
        &registry,
    )
    .unwrap();
    let configured = RunConfiguration::prepare(plan, ExecutionOptions::default()).unwrap();

    let result = PipelineExecutor::new(registry)
        .run_configuration(&configured)
        .await
        .unwrap();

    assert!(result.completed_nodes.contains(&"a_clean".into()));
    assert!(!result.completed_nodes.contains(&"z_stale".into()));
    assert!(!result.final_context.contains_key("preferred_label"));
}

#[test]
fn caller_manifest_graph_and_built_in_precedence_is_resolved_per_field() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("pas.toml"),
        r#"
[project]
name = "precedence"

[quality]
stages = []
max_fix_iterations = 7

[codergen.claude]
settings_mode = "strict_bare"
setting_sources = ["user"]
tools = "manifest-tools"
settings_json = "{\"secret\":\"manifest-secret\"}"
agents_json = "{\"manifest-agent\":true}"
plugin_dirs = ["manifest-plugin"]
mcp_config_json = "{\"manifest-mcp\":true}"
"#,
    )
    .unwrap();
    let quality_plan = plan(
        r#"digraph G {
            start [shape="Mdiamond"]
            check [shape="ellipse", type="quality", max_fix_iterations=5]
            done [shape="Msquare"]
            start -> check -> done
        }"#,
    );

    let configured = RunConfiguration::prepare(
        quality_plan.clone(),
        ExecutionOptions {
            workdir: Some(root.path().into()),
            quality_max_fix_iterations: Some(9),
            claude: ClaudeExecutionOptions {
                settings_mode: Some(ClaudeSettingsMode::SubscriptionBare),
                setting_sources: Some(vec![attractor_pipeline::ClaudeSettingSource::Project]),
                settings: Some("{\"caller\":\"caller-secret\"}".into()),
                agents: Some("{\"caller-agent\":true}".into()),
                plugin_dirs: Some(vec![root.path().join("caller-plugin")]),
                mcp_config: Some("{\"caller-mcp\":true}".into()),
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        *configured
            .controls()
            .quality_max_fix_iterations("check")
            .value(),
        9
    );
    assert_eq!(
        configured
            .controls()
            .quality_max_fix_iterations("check")
            .source(),
        ConfigurationSource::Caller
    );
    assert_eq!(
        *configured.controls().claude().settings_mode().value(),
        ClaudeSettingsMode::SubscriptionBare
    );
    assert_eq!(
        configured.controls().claude().settings_mode().source(),
        ConfigurationSource::Caller
    );
    assert_eq!(
        configured.controls().claude().tools().value().as_deref(),
        Some("manifest-tools")
    );
    assert_eq!(
        configured.controls().claude().tools().source(),
        ConfigurationSource::Manifest
    );
    assert_eq!(
        configured.controls().claude().setting_sources().source(),
        ConfigurationSource::Caller
    );
    assert_eq!(
        configured.controls().claude().settings().source(),
        ConfigurationSource::Caller
    );
    assert_eq!(
        configured.controls().claude().agents().source(),
        ConfigurationSource::Caller
    );
    assert_eq!(
        configured.controls().claude().plugin_dirs().source(),
        ConfigurationSource::Caller
    );
    assert_eq!(
        configured.controls().claude().mcp_config().source(),
        ConfigurationSource::Caller
    );

    let manifest = RunConfiguration::prepare(
        quality_plan.clone(),
        ExecutionOptions {
            workdir: Some(root.path().into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        *manifest
            .controls()
            .quality_max_fix_iterations("check")
            .value(),
        7
    );
    assert_eq!(
        manifest
            .controls()
            .quality_max_fix_iterations("check")
            .source(),
        ConfigurationSource::Manifest
    );

    let no_manifest = tempfile::tempdir().unwrap();
    std::fs::create_dir(no_manifest.path().join(".git")).unwrap();
    let graph = RunConfiguration::prepare(
        quality_plan,
        ExecutionOptions {
            workdir: Some(no_manifest.path().into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        *graph.controls().quality_max_fix_iterations("check").value(),
        5
    );
    assert_eq!(
        graph
            .controls()
            .quality_max_fix_iterations("check")
            .source(),
        ConfigurationSource::Graph
    );

    let built_in = RunConfiguration::prepare(
        plan(r#"digraph G { start [shape="Mdiamond"] check [shape="ellipse", type="quality"] done [shape="Msquare"] start -> check -> done }"#),
        ExecutionOptions { workdir: Some(no_manifest.path().into()), ..Default::default() },
    )
    .unwrap();
    assert_eq!(
        *built_in
            .controls()
            .quality_max_fix_iterations("check")
            .value(),
        3
    );
    assert_eq!(
        built_in
            .controls()
            .quality_max_fix_iterations("check")
            .source(),
        ConfigurationSource::BuiltIn
    );

    let debug = format!("{configured:?}");
    assert!(!debug.contains("caller-secret"));
    assert!(!debug.contains("manifest-secret"));
}

#[test]
fn sensitive_resolved_value_debug_is_redacted_for_caller_and_manifest_sources() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("pas.toml"),
        r#"
[project]
name = "debug-redaction"

[codergen.claude]
settings_json = "{\"manifest-settings-secret\":true}"
tools = "manifest-tools-secret"
agents_json = "{\"manifest-agents-secret\":true}"
plugin_dirs = ["manifest-plugin-secret"]
mcp_config_json = "{\"manifest-mcp-secret\":true}"
"#,
    )
    .unwrap();
    let pipeline =
        plan(r#"digraph G { start [shape="Mdiamond"] done [shape="Msquare"] start -> done }"#);

    let configurations = [
        RunConfiguration::prepare(
            pipeline.clone(),
            ExecutionOptions {
                workdir: Some(root.path().into()),
                claude: ClaudeExecutionOptions {
                    settings: Some("{\"caller-settings-secret\":true}".into()),
                    tools: Some("caller-tools-secret".into()),
                    agents: Some("{\"caller-agents-secret\":true}".into()),
                    plugin_dirs: Some(vec![root.path().join("caller-plugin-secret")]),
                    mcp_config: Some("{\"caller-mcp-secret\":true}".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap(),
        RunConfiguration::prepare(
            pipeline,
            ExecutionOptions {
                workdir: Some(root.path().into()),
                ..Default::default()
            },
        )
        .unwrap(),
    ];

    for configured in &configurations {
        let claude = configured.controls().claude();
        let independently_formatted = [
            format!("{:?}", claude.settings()),
            format!("{:?}", claude.tools()),
            format!("{:?}", claude.agents()),
            format!("{:?}", claude.plugin_dirs()),
            format!("{:?}", claude.mcp_config()),
        ];

        for debug in independently_formatted {
            assert!(debug.contains("<redacted>"), "{debug}");
            for secret in [
                "caller-settings-secret",
                "caller-tools-secret",
                "caller-agents-secret",
                "caller-plugin-secret",
                "caller-mcp-secret",
                "manifest-settings-secret",
                "manifest-tools-secret",
                "manifest-agents-secret",
                "manifest-plugin-secret",
                "manifest-mcp-secret",
            ] {
                assert!(!debug.contains(secret), "{debug}");
            }
        }
    }
}

#[test]
fn invalid_graph_quality_limits_fail_preparation_instead_of_using_built_in() {
    for authored in ["0", "-1", "3.5", "\"3\"", "4294967296"] {
        let source = format!(
            r#"digraph G {{
                start [shape="Mdiamond"]
                check [shape="ellipse", type="quality", max_fix_iterations={authored}]
                done [shape="Msquare"]
                start -> check -> done
            }}"#
        );

        let error = RunConfiguration::prepare(plan(&source), ExecutionOptions::default())
            .expect_err(authored);
        assert!(error.to_string().contains("max_fix_iterations"), "{error}");
        assert!(error.to_string().contains("check"), "{error}");
    }
}

#[test]
fn prepared_preflight_preserves_built_in_and_caller_budget_provenance() {
    let plan = plan(
        r#"digraph G {
            start [shape="Mdiamond"]
            work [label="Do work", timeout="60s", llm_provider="codex"]
            done [shape="Msquare"]
            start -> work -> done
        }"#,
    );

    let built_in = RunConfiguration::prepare(plan.clone(), ExecutionOptions::default()).unwrap();
    let built_in_warning = preflight_run_configuration(&built_in)
        .into_iter()
        .find(|finding| finding.code == "PROVIDER_COST_UNTRACKED")
        .unwrap();
    assert!(
        built_in_warning.message.contains("implicit $200"),
        "{}",
        built_in_warning.message
    );
    assert!(
        !built_in_warning.message.contains("--max-budget-usd"),
        "{}",
        built_in_warning.message
    );

    let caller = RunConfiguration::prepare(
        plan,
        ExecutionOptions {
            max_budget_usd: Some(42.5),
            ..Default::default()
        },
    )
    .unwrap();
    let caller_warning = preflight_run_configuration(&caller)
        .into_iter()
        .find(|finding| finding.code == "PROVIDER_COST_UNTRACKED")
        .unwrap();
    assert!(
        caller_warning
            .message
            .contains("explicit $42.50 budget from --max-budget-usd"),
        "{}",
        caller_warning.message
    );
}

#[test]
fn built_in_controls_are_typed_and_inspectable() {
    let configured = RunConfiguration::prepare(
        plan(
            r#"digraph G {
                graph [goal="ship safely"]
                start [shape="Mdiamond"]
                done [shape="Msquare"]
                start -> done
            }"#,
        ),
        ExecutionOptions::default(),
    )
    .unwrap();

    assert!(!*configured.controls().dry_run().value());
    assert_eq!(
        configured.controls().dry_run().source(),
        ConfigurationSource::BuiltIn
    );
    assert_eq!(*configured.controls().max_steps().value(), 200);
    assert_eq!(*configured.controls().max_budget_usd().value(), 200.0);
    assert_eq!(
        configured.controls().workdir().value(),
        &std::env::current_dir().unwrap().canonicalize().unwrap()
    );
}

#[test]
fn caller_core_controls_are_validated_and_keep_caller_provenance() {
    let root = tempfile::tempdir().unwrap();
    let base =
        plan(r#"digraph G { start [shape="Mdiamond"] done [shape="Msquare"] start -> done }"#);
    let configured = RunConfiguration::prepare(
        base.clone(),
        ExecutionOptions {
            dry_run: Some(false),
            max_steps: Some(12),
            max_budget_usd: Some(4.5),
            workdir: Some(root.path().into()),
            quality_disabled: Some(true),
            ..Default::default()
        },
    )
    .unwrap();
    for source in [
        configured.controls().dry_run().source(),
        configured.controls().max_steps().source(),
        configured.controls().max_budget_usd().source(),
        configured.controls().workdir().source(),
        configured.controls().quality_disabled().source(),
    ] {
        assert_eq!(source, ConfigurationSource::Caller);
    }

    for (options, expected) in [
        (
            ExecutionOptions {
                max_steps: Some(0),
                ..Default::default()
            },
            "max_steps",
        ),
        (
            ExecutionOptions {
                max_budget_usd: Some(-0.01),
                ..Default::default()
            },
            "max_budget_usd",
        ),
        (
            ExecutionOptions {
                max_budget_usd: Some(f64::NAN),
                ..Default::default()
            },
            "max_budget_usd",
        ),
    ] {
        let error = RunConfiguration::prepare(base.clone(), options).unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
    }
}

#[test]
fn graph_cannot_author_run_controls_or_provider_isolation() {
    for key in [
        "dry_run",
        "workdir",
        "max_steps",
        "max_budget_usd",
        "quality_disabled",
        "quality_max_fix_iterations",
        "codergen.claude.settings_mode",
        "codergen.claude.setting_sources",
        "codergen.claude.settings",
        "codergen.claude.tools",
        "codergen.claude.agents",
        "codergen.claude.plugin_dirs",
        "codergen.claude.mcp_config",
        "outcome",
        "preferred_label",
        "__pas.internal",
    ] {
        let source = format!(
            "digraph G {{ graph [{key}=\"hostile\"] start [shape=\"Mdiamond\"] done [shape=\"Msquare\"] start -> done }}"
        );
        let error =
            RunConfiguration::prepare(plan(&source), ExecutionOptions::default()).expect_err(key);
        assert!(error.to_string().contains(key), "{key}: {error}");
        assert!(error.to_string().contains("reserved"), "{key}: {error}");
    }
}

// --- U8: skill, pi extension and pi prompt template lists ---

use attractor_pipeline::ConfigurationError;
use std::path::{Path, PathBuf};

fn trivial_plan() -> ExecutionPlan {
    plan(r#"digraph G { start [shape="Mdiamond"] done [shape="Msquare"] start -> done }"#)
}

fn skill_dir(root: &Path, relative: &str) -> PathBuf {
    let dir = root.join(relative);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), "---\nname: s\n---\nbody\n").unwrap();
    dir
}

fn write_manifest(root: &Path, body: &str) {
    std::fs::write(
        root.join("pas.toml"),
        format!("[project]\nname = \"u8\"\n\n{body}\n"),
    )
    .unwrap();
}

fn prepare_in(
    root: &Path,
    options: ExecutionOptions,
) -> std::result::Result<RunConfiguration, ConfigurationError> {
    RunConfiguration::prepare(
        trivial_plan(),
        ExecutionOptions {
            workdir: Some(root.into()),
            ..options
        },
    )
}

fn invalid_message(result: std::result::Result<RunConfiguration, ConfigurationError>) -> String {
    match result {
        Err(ConfigurationError::Invalid(message)) => message,
        other => panic!("expected ConfigurationError::Invalid, got {other:?}"),
    }
}

// Joined to the directory of the manifest that `resolve` found, whose form
// is not the temp path on macOS (`/private/var`).
fn canonical(root: &tempfile::TempDir) -> PathBuf {
    root.path().canonicalize().unwrap()
}

#[test]
fn manifest_skill_resolves_under_pas_toml_dir() {
    let root = tempfile::tempdir().unwrap();
    let base = canonical(&root);
    skill_dir(&base, "skills/review");
    write_manifest(&base, "[codergen]\nskills = [\"skills/review\"]");

    let configured = prepare_in(&base, ExecutionOptions::default()).unwrap();

    let skills = configured.controls().skills();
    assert_eq!(skills.value(), &vec![base.join("skills/review")]);
    assert_eq!(skills.source(), ConfigurationSource::Manifest);
}

#[test]
fn caller_skills_replace_manifest_list() {
    let root = tempfile::tempdir().unwrap();
    let base = canonical(&root);
    skill_dir(&base, "skills/manifest-one");
    let a = skill_dir(&base, "other/a");
    let b = skill_dir(&base, "other/b");
    write_manifest(&base, "[codergen]\nskills = [\"skills/manifest-one\"]");

    let configured = prepare_in(
        &base,
        ExecutionOptions {
            skills: Some(vec![a.clone(), b.clone()]),
            ..Default::default()
        },
    )
    .unwrap();

    let skills = configured.controls().skills();
    assert_eq!(skills.value(), &vec![a, b]);
    assert_eq!(skills.source(), ConfigurationSource::Caller);
}

#[test]
fn empty_caller_list_replaces_manifest_list() {
    let root = tempfile::tempdir().unwrap();
    let base = canonical(&root);
    skill_dir(&base, "skills/review");
    write_manifest(&base, "[codergen]\nskills = [\"skills/review\"]");

    let configured = prepare_in(
        &base,
        ExecutionOptions {
            skills: Some(Vec::new()),
            ..Default::default()
        },
    )
    .unwrap();

    assert!(configured.controls().skills().value().is_empty());
    assert_eq!(
        configured.controls().skills().source(),
        ConfigurationSource::Caller
    );
}

#[test]
fn absolute_manifest_skill_path_is_unchanged() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let skill = skill_dir(outside.path(), "elsewhere/review");
    write_manifest(
        &canonical(&root),
        &format!("[codergen]\nskills = [{:?}]", skill.display().to_string()),
    );

    let configured = prepare_in(&canonical(&root), ExecutionOptions::default()).unwrap();

    assert_eq!(configured.controls().skills().value(), &vec![skill]);
}

#[test]
fn list_order_is_kept() {
    let root = tempfile::tempdir().unwrap();
    let base = canonical(&root);
    for name in ["z", "a", "m"] {
        skill_dir(&base, &format!("skills/{name}"));
    }
    write_manifest(
        &base,
        "[codergen]\nskills = [\"skills/z\", \"skills/a\", \"skills/m\"]",
    );

    let configured = prepare_in(&base, ExecutionOptions::default()).unwrap();

    assert_eq!(
        configured.controls().skills().value(),
        &vec![
            base.join("skills/z"),
            base.join("skills/a"),
            base.join("skills/m")
        ]
    );
}

#[test]
fn missing_skill_path_error_names_path_and_source() {
    let root = tempfile::tempdir().unwrap();
    let base = canonical(&root);
    write_manifest(&base, "[codergen]\nskills = [\"skills/gone\"]");

    let message = invalid_message(prepare_in(&base, ExecutionOptions::default()));
    assert!(
        message.contains(&base.join("skills/gone").display().to_string()),
        "{message}"
    );
    assert!(message.contains("Manifest"), "{message}");

    let missing = base.join("caller/missing");
    let message = invalid_message(prepare_in(
        &base,
        ExecutionOptions {
            skills: Some(vec![missing.clone()]),
            ..Default::default()
        },
    ));
    assert!(
        message.contains(&missing.display().to_string()),
        "{message}"
    );
    assert!(message.contains("Caller"), "{message}");
}

#[test]
fn skill_dir_without_skill_md_fails() {
    let root = tempfile::tempdir().unwrap();
    let base = canonical(&root);
    std::fs::create_dir_all(base.join("skills/empty")).unwrap();
    write_manifest(&base, "[codergen]\nskills = [\"skills/empty\"]");

    let message = invalid_message(prepare_in(&base, ExecutionOptions::default()));
    assert!(message.contains("SKILL.md"), "{message}");
    assert!(message.contains("skills/empty"), "{message}");
}

#[test]
fn skill_entry_that_is_a_file_fails() {
    let root = tempfile::tempdir().unwrap();
    let base = canonical(&root);
    std::fs::write(base.join("file-skill"), "x").unwrap();
    write_manifest(&base, "[codergen]\nskills = [\"file-skill\"]");

    let message = invalid_message(prepare_in(&base, ExecutionOptions::default()));
    assert!(message.contains("not a directory"), "{message}");
}

#[test]
fn duplicate_skill_directory_names_fail() {
    let root = tempfile::tempdir().unwrap();
    let base = canonical(&root);
    skill_dir(&base, "a/review");
    skill_dir(&base, "b/review");
    write_manifest(&base, "[codergen]\nskills = [\"a/review\", \"b/review\"]");

    let message = invalid_message(prepare_in(&base, ExecutionOptions::default()));
    assert!(message.contains("same directory name"), "{message}");
    assert!(
        message.contains("a/review") && message.contains("b/review"),
        "{message}"
    );
}

#[cfg(unix)]
#[test]
fn symlinked_skill_entry_resolves() {
    let root = tempfile::tempdir().unwrap();
    let base = canonical(&root);
    let target = skill_dir(&base, "real/target-name");
    let link = base.join("linked-skill");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    write_manifest(&base, "[codergen]\nskills = [\"linked-skill\"]");

    let configured = prepare_in(&base, ExecutionOptions::default()).unwrap();

    assert_eq!(configured.controls().skills().value(), &vec![link]);
}

#[cfg(unix)]
#[test]
fn symlink_inside_skill_dir_fails() {
    let root = tempfile::tempdir().unwrap();
    let base = canonical(&root);
    let skill = skill_dir(&base, "skills/review");
    std::fs::create_dir_all(skill.join("nested")).unwrap();
    std::fs::write(base.join("secret.txt"), "x").unwrap();
    let link = skill.join("nested/leak");
    std::os::unix::fs::symlink(base.join("secret.txt"), &link).unwrap();
    write_manifest(&base, "[codergen]\nskills = [\"skills/review\"]");

    let message = invalid_message(prepare_in(&base, ExecutionOptions::default()));
    assert!(message.contains(&link.display().to_string()), "{message}");
    assert!(message.contains("symbolic link"), "{message}");
}

#[cfg(unix)]
#[test]
fn broken_symlink_entry_fails() {
    let root = tempfile::tempdir().unwrap();
    let base = canonical(&root);
    let link = base.join("dangling");
    std::os::unix::fs::symlink(base.join("nowhere"), &link).unwrap();
    write_manifest(&base, "[codergen]\nskills = [\"dangling\"]");

    let message = invalid_message(prepare_in(&base, ExecutionOptions::default()));
    assert!(message.contains("does not exist"), "{message}");
}

#[test]
fn pi_extensions_and_prompt_templates_resolve_and_validate() {
    let root = tempfile::tempdir().unwrap();
    let base = canonical(&root);
    std::fs::create_dir_all(base.join("ext")).unwrap();
    std::fs::write(base.join("ext/guard.ts"), "export default {}").unwrap();
    std::fs::create_dir_all(base.join("ext-dir")).unwrap();
    std::fs::create_dir_all(base.join("prompts")).unwrap();
    std::fs::write(base.join("prompts/review.md"), "review").unwrap();
    write_manifest(
        &base,
        "[codergen.pi]\nextensions = [\"ext/guard.ts\", \"ext-dir\"]\nprompt_templates = [\"prompts/review.md\"]",
    );

    let configured = prepare_in(&base, ExecutionOptions::default()).unwrap();
    let controls = configured.controls();
    assert_eq!(
        controls.pi_extensions().value(),
        &vec![base.join("ext/guard.ts"), base.join("ext-dir")]
    );
    assert_eq!(
        controls.pi_extensions().source(),
        ConfigurationSource::Manifest
    );
    assert_eq!(
        controls.pi_prompt_templates().value(),
        &vec![base.join("prompts/review.md")]
    );
    assert_eq!(
        controls.pi_prompt_templates().source(),
        ConfigurationSource::Manifest
    );
    assert!(controls.skills().value().is_empty());

    // Caller lists replace the manifest lists.
    let only = base.join("ext/guard.ts");
    let configured = prepare_in(
        &base,
        ExecutionOptions {
            pi_extensions: Some(vec![only.clone()]),
            pi_prompt_templates: Some(Vec::new()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(configured.controls().pi_extensions().value(), &vec![only]);
    assert_eq!(
        configured.controls().pi_extensions().source(),
        ConfigurationSource::Caller
    );
    assert!(configured
        .controls()
        .pi_prompt_templates()
        .value()
        .is_empty());
}

#[test]
fn missing_pi_extension_and_prompt_template_fail_naming_path_and_source() {
    let root = tempfile::tempdir().unwrap();
    let base = canonical(&root);
    write_manifest(&base, "[codergen.pi]\nextensions = [\"ext/gone.ts\"]");
    let message = invalid_message(prepare_in(&base, ExecutionOptions::default()));
    assert!(message.contains("ext/gone.ts"), "{message}");
    assert!(message.contains("Manifest"), "{message}");

    let missing = base.join("prompts/gone.md");
    let message = invalid_message(prepare_in(
        &base,
        ExecutionOptions {
            pi_extensions: Some(Vec::new()),
            pi_prompt_templates: Some(vec![missing.clone()]),
            ..Default::default()
        },
    ));
    assert!(
        message.contains(&missing.display().to_string()),
        "{message}"
    );
    assert!(message.contains("Caller"), "{message}");
}

#[test]
fn no_lists_resolve_empty_built_in() {
    let root = tempfile::tempdir().unwrap();
    let base = canonical(&root);

    let configured = prepare_in(&base, ExecutionOptions::default()).unwrap();
    let controls = configured.controls();
    for list in [
        controls.skills(),
        controls.pi_extensions(),
        controls.pi_prompt_templates(),
    ] {
        assert!(list.value().is_empty());
        assert_eq!(list.source(), ConfigurationSource::BuiltIn);
    }
}

#[test]
fn graph_attribute_resource_lists_are_reserved() {
    for key in [
        "codergen.skills",
        "codergen.pi.extensions",
        "codergen.pi.prompt_templates",
    ] {
        let source = format!(
            "digraph G {{ graph [{key}=\"hostile\"] start [shape=\"Mdiamond\"] done [shape=\"Msquare\"] start -> done }}"
        );
        let error =
            RunConfiguration::prepare(plan(&source), ExecutionOptions::default()).expect_err(key);
        assert!(
            matches!(&error, ConfigurationError::ReservedGraphAttribute(found) if found == key),
            "{key}: {error}"
        );
    }
}

#[tokio::test]
async fn node_context_update_of_pi_extensions_is_filtered_or_fails_closed() {
    struct Hostile;

    #[async_trait]
    impl NodeHandler for Hostile {
        fn handler_type(&self) -> &str {
            "hostile"
        }
        async fn execute(
            &self,
            _: &PipelineNode,
            _: &Context,
            _: &PipelineGraph,
        ) -> Result<Outcome> {
            let context_updates = [("codergen.pi.extensions", serde_json::json!(["/evil.ts"]))]
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect::<HashMap<_, _>>();
            Ok(Outcome {
                context_updates,
                ..Outcome::success("hostile")
            })
        }
    }

    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(Hostile);
    registry.register(ExitHandler);
    let plan = ExecutionPlan::compile_with_registry(
        graph(r#"digraph G { start [shape="Mdiamond"] attack [shape="ellipse", type="hostile"] done [shape="Msquare"] start -> attack -> done }"#),
        &registry,
    )
    .unwrap();
    let configured = RunConfiguration::prepare(plan, ExecutionOptions::default()).unwrap();

    let error = PipelineExecutor::new(registry)
        .run_configuration(&configured)
        .await
        .expect_err("a reserved resource key must not reach the context");
    assert!(
        error.to_string().contains("reserved context key"),
        "{error}"
    );
    assert!(configured.controls().pi_extensions().value().is_empty());
}

fn extension_trust_hash(base: &Path, options: ExecutionOptions) -> Option<String> {
    prepare_in(base, options)
        .unwrap()
        .manifest_extension_trust()
        .unwrap()
        .map(|trust| trust.hash)
}

#[test]
fn extension_trust_is_none_without_manifest_extensions_or_for_flag_lists() {
    let root = tempfile::tempdir().unwrap();
    let base = canonical(&root);
    std::fs::write(base.join("guard.ts"), "export default {}").unwrap();

    assert_eq!(
        extension_trust_hash(&base, ExecutionOptions::default()),
        None
    );

    write_manifest(&base, "[codergen.pi]\nextensions = [\"guard.ts\"]");
    assert!(extension_trust_hash(&base, ExecutionOptions::default()).is_some());
    let caller = ExecutionOptions {
        pi_extensions: Some(vec![base.join("guard.ts")]),
        ..Default::default()
    };
    assert_eq!(extension_trust_hash(&base, caller), None);
}

#[test]
fn extension_trust_hash_covers_manifest_extension_bytes_and_list_order() {
    let root = tempfile::tempdir().unwrap();
    let base = canonical(&root);
    std::fs::write(base.join("a.ts"), "aaa").unwrap();
    std::fs::write(base.join("b.ts"), "bbb").unwrap();
    write_manifest(&base, "[codergen.pi]\nextensions = [\"a.ts\", \"b.ts\"]");

    let manifest = std::fs::read(base.join("pas.toml")).unwrap();
    let mut expected = blake3::Hasher::new();
    expected.update(&manifest);
    expected.update(b"aaa");
    expected.update(b"bbb");
    let first = extension_trust_hash(&base, ExecutionOptions::default()).unwrap();
    assert_eq!(first, expected.finalize().to_hex().to_string());

    std::fs::write(base.join("b.ts"), "changed").unwrap();
    let changed = extension_trust_hash(&base, ExecutionOptions::default()).unwrap();
    assert_ne!(first, changed);

    std::fs::write(base.join("b.ts"), "bbb").unwrap();
    write_manifest(&base, "[codergen.pi]\nextensions = [\"b.ts\", \"a.ts\"]");
    let reordered = extension_trust_hash(&base, ExecutionOptions::default()).unwrap();
    assert_ne!(first, reordered);
}

#[test]
fn extension_trust_hash_walks_directories_in_sorted_order() {
    let root = tempfile::tempdir().unwrap();
    let base = canonical(&root);
    let dir = base.join("ext");
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("z.ts"), "zzz").unwrap();
    std::fs::write(dir.join("a.ts"), "aaa").unwrap();
    std::fs::write(dir.join("sub/m.ts"), "mmm").unwrap();
    write_manifest(&base, "[codergen.pi]\nextensions = [\"ext\"]");

    let manifest = std::fs::read(base.join("pas.toml")).unwrap();
    let mut expected = blake3::Hasher::new();
    expected.update(&manifest);
    expected.update(b"aaa");
    expected.update(b"mmm");
    expected.update(b"zzz");
    assert_eq!(
        extension_trust_hash(&base, ExecutionOptions::default()).unwrap(),
        expected.finalize().to_hex().to_string()
    );
}

#[cfg(unix)]
#[test]
fn extension_trust_rejects_symlink_inside_extension_directory() {
    let root = tempfile::tempdir().unwrap();
    let base = canonical(&root);
    let dir = base.join("ext");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(base.join("real.ts"), "x").unwrap();
    std::os::unix::fs::symlink(base.join("real.ts"), dir.join("link.ts")).unwrap();
    write_manifest(&base, "[codergen.pi]\nextensions = [\"ext\"]");

    let error = prepare_in(&base, ExecutionOptions::default())
        .unwrap()
        .manifest_extension_trust()
        .unwrap_err();
    assert!(error.to_string().contains("symbolic link"), "{error}");
}
