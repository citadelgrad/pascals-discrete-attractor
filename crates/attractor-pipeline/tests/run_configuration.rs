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
