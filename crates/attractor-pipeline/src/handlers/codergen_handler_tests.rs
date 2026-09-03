use std::collections::HashMap;

use attractor_types::StageStatus;

use super::*;
use crate::handlers::tests::{make_minimal_graph, make_node};

// --- LlmCliProvider ---

#[test]
fn provider_from_str_claude_variants() {
    assert_eq!(
        "claude".parse::<LlmCliProvider>(),
        Ok(LlmCliProvider::Claude)
    );
    assert_eq!(
        "anthropic".parse::<LlmCliProvider>(),
        Ok(LlmCliProvider::Claude)
    );
    assert_eq!(
        "CLAUDE".parse::<LlmCliProvider>(),
        Ok(LlmCliProvider::Claude)
    );
}

#[test]
fn provider_from_str_codex_variants() {
    assert_eq!("codex".parse::<LlmCliProvider>(), Ok(LlmCliProvider::Codex));
    assert_eq!(
        "openai".parse::<LlmCliProvider>(),
        Ok(LlmCliProvider::Codex)
    );
}

#[test]
fn provider_from_str_gemini_variants() {
    assert_eq!(
        "gemini".parse::<LlmCliProvider>(),
        Ok(LlmCliProvider::Gemini)
    );
    assert_eq!(
        "google".parse::<LlmCliProvider>(),
        Ok(LlmCliProvider::Gemini)
    );
}

#[test]
fn provider_parse_unknown_is_rejected() {
    assert!("llama".parse::<LlmCliProvider>().is_err());
}

#[test]
fn provider_binary_names() {
    assert_eq!(LlmCliProvider::Claude.binary_name(), "claude");
    assert_eq!(LlmCliProvider::Codex.binary_name(), "codex");
    assert_eq!(LlmCliProvider::Gemini.binary_name(), "gemini");
}

// --- Output parsers ---

#[test]
fn parse_claude_output_success() {
    let json = r#"{"result":"Hello world","is_error":false,"subtype":"","total_cost_usd":0.05,"num_turns":3}"#;
    let result = parse_claude_output(json, "test_node").unwrap();
    assert_eq!(result.text, "Hello world");
    assert!(!result.is_error);
    assert_eq!(result.cost_usd, Some(0.05));
    assert_eq!(result.turns, Some(3));
}

#[test]
fn parse_claude_output_error() {
    let json = r#"{"result":"Something failed","is_error":true,"subtype":"error","total_cost_usd":0.01,"num_turns":1}"#;
    let result = parse_claude_output(json, "test_node").unwrap();
    assert!(result.is_error);
}

#[test]
fn parse_claude_output_invalid_json() {
    let result = parse_claude_output("not json", "test_node");
    assert!(result.is_err());
}

#[test]
fn parse_codex_output_extracts_last_message() {
    let jsonl = concat!(
        r#"{"type":"item.completed","item":{"type":"agent_message","text":"First message"}}"#,
        "\n",
        r#"{"type":"item.completed","item":{"type":"agent_message","text":"Final answer"}}"#,
    );
    let result = parse_codex_output(jsonl, "test_node").unwrap();
    assert_eq!(result.text, "Final answer");
    assert!(!result.is_error);
}

#[test]
fn parse_codex_output_handles_turn_failed() {
    let jsonl = r#"{"type":"turn.failed","error":{"message":"Rate limited"}}"#;
    let result = parse_codex_output(jsonl, "test_node").unwrap();
    assert!(result.is_error);
    assert_eq!(result.text, "Rate limited");
}

#[test]
fn parse_codex_output_handles_stream_error() {
    let jsonl = r#"{"type":"error","message":"Connection lost"}"#;
    let result = parse_codex_output(jsonl, "test_node").unwrap();
    assert!(result.is_error);
    assert_eq!(result.text, "Connection lost");
}

#[test]
fn parse_codex_output_skips_unknown_events() {
    let jsonl = concat!(
        r#"{"type":"thread.started"}"#,
        "\n",
        r#"{"type":"turn.started"}"#,
        "\n",
        r#"{"type":"item.completed","item":{"type":"agent_message","text":"Done"}}"#,
        "\n",
        r#"{"type":"turn.completed","usage":{"input_tokens":100,"output_tokens":50}}"#,
    );
    let result = parse_codex_output(jsonl, "test_node").unwrap();
    assert_eq!(result.text, "Done");
    assert!(!result.is_error);
}

#[test]
fn parse_gemini_output_success() {
    let json = r#"{"session_id":"abc","response":"Gemini says hi"}"#;
    let result = parse_gemini_output(json, "test_node").unwrap();
    assert_eq!(result.text, "Gemini says hi");
    assert!(!result.is_error);
}

#[test]
fn parse_gemini_output_error() {
    let json = r#"{"error":{"type":"api_error","message":"Model not found","code":404}}"#;
    let result = parse_gemini_output(json, "test_node").unwrap();
    assert!(result.is_error);
    assert_eq!(result.text, "Model not found");
}

#[test]
fn parse_gemini_output_invalid_json() {
    let result = parse_gemini_output("not json", "test_node");
    assert!(result.is_err());
}

#[test]
fn parse_cli_output_empty_stdout_errors() {
    let result = parse_cli_output(LlmCliProvider::Claude, "", "some error", "n");
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("produced no output"));
}

// --- build_cli_command ---

#[test]
fn build_cli_command_claude_has_json_output() {
    let node = make_node("n", "box", Some("do work"), HashMap::new());
    let graph = make_minimal_graph();
    let cfg = CliRunConfig {
        provider: LlmCliProvider::Claude,
        prompt: "test prompt",
        model: Some("sonnet"),
        workdir: None,
        node: &node,
        graph: &graph,
        claude: ClaudeCliConfig::default(),
    };
    let cmd = build_cli_command(&cfg);
    let args: Vec<_> = cmd
        .as_std()
        .get_args()
        .map(|a| a.to_str().unwrap())
        .collect();
    assert!(args.contains(&"--output-format"));
    assert!(args.contains(&"json"));
    assert!(args.contains(&"--safe-mode"));
    assert!(!args.contains(&"--bare"));
    assert!(args.contains(&"--strict-mcp-config"));
    assert!(args.contains(&"--disable-slash-commands"));
    assert!(!args.contains(&"--setting-sources"));
    assert!(args.contains(&"--model"));
    assert!(args.contains(&"sonnet"));
    assert!(args.contains(&"-p"));
}

#[test]
fn build_cli_command_claude_strict_bare_is_opt_in() {
    let node = make_node("n", "box", Some("do work"), HashMap::new());
    let graph = make_minimal_graph();
    let cfg = CliRunConfig {
        provider: LlmCliProvider::Claude,
        prompt: "test prompt",
        model: None,
        workdir: None,
        node: &node,
        graph: &graph,
        claude: ClaudeCliConfig {
            settings_mode: ClaudeSettingsMode::StrictBare,
            ..ClaudeCliConfig::default()
        },
    };

    let cmd = build_cli_command(&cfg);
    let args: Vec<_> = cmd
        .as_std()
        .get_args()
        .map(|a| a.to_str().unwrap())
        .collect();

    assert!(args.contains(&"--bare"));
    assert!(!args.contains(&"--safe-mode"));
    assert!(!args.contains(&"--setting-sources"));
}

#[test]
fn build_cli_command_claude_inherit_uses_setting_sources() {
    let node = make_node("n", "box", Some("do work"), HashMap::new());
    let graph = make_minimal_graph();
    let cfg = CliRunConfig {
        provider: LlmCliProvider::Claude,
        prompt: "test prompt",
        model: None,
        workdir: None,
        node: &node,
        graph: &graph,
        claude: ClaudeCliConfig {
            settings_mode: ClaudeSettingsMode::Inherit,
            setting_sources: vec!["user".into(), "project".into()],
            ..ClaudeCliConfig::default()
        },
    };

    let cmd = build_cli_command(&cfg);
    let args: Vec<_> = cmd
        .as_std()
        .get_args()
        .map(|a| a.to_str().unwrap())
        .collect();

    assert!(!args.contains(&"--bare"));
    assert!(!args.contains(&"--safe-mode"));
    assert!(args.contains(&"--setting-sources"));
    assert!(args.contains(&"user,project"));
}

#[test]
fn build_cli_command_claude_emits_explicit_pas_owned_config() {
    let node = make_node("n", "box", Some("do work"), HashMap::new());
    let graph = make_minimal_graph();
    let cfg = CliRunConfig {
        provider: LlmCliProvider::Claude,
        prompt: "test prompt",
        model: None,
        workdir: None,
        node: &node,
        graph: &graph,
        claude: ClaudeCliConfig {
            settings: Some(r#"{"enabledPlugins":{}}"#.into()),
            tools: Some("Read,Edit".into()),
            agents: Some(r#"{"reviewer":{"prompt":"review"}}"#.into()),
            plugin_dirs: vec!["/tmp/pas-plugin".into()],
            mcp_config: Some("{}".into()),
            ..ClaudeCliConfig::default()
        },
    };

    let cmd = build_cli_command(&cfg);
    let args: Vec<_> = cmd
        .as_std()
        .get_args()
        .map(|a| a.to_str().unwrap())
        .collect();

    assert!(args.contains(&"--settings"));
    assert!(args.contains(&r#"{"enabledPlugins":{}}"#));
    assert!(args.contains(&"--tools"));
    assert!(args.contains(&"Read,Edit"));
    assert!(args.contains(&"--agents"));
    assert!(args.contains(&r#"{"reviewer":{"prompt":"review"}}"#));
    assert!(args.contains(&"--plugin-dir"));
    assert!(args.contains(&"/tmp/pas-plugin"));
    assert!(args.contains(&"--mcp-config"));
    assert!(args.contains(&"{}"));
}

#[test]
fn resolve_claude_cli_config_reads_pas_toml_and_resolves_plugin_dirs() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("pas.toml"),
        r#"
[project]
name = "test"

[codergen.claude]
settings_mode = "inherit"
setting_sources = ["user"]
settings_json = "{}"
tools = "Read,Edit"
plugin_dirs = [".pas/plugin"]
"#,
    )
    .unwrap();
    let snapshot = HashMap::new();

    let cfg = resolve_claude_cli_config(&snapshot, dir.path().to_str(), "code").unwrap();

    assert_eq!(cfg.settings_mode, ClaudeSettingsMode::Inherit);
    assert_eq!(cfg.setting_sources, vec!["user"]);
    assert_eq!(cfg.settings.as_deref(), Some("{}"));
    assert_eq!(cfg.tools.as_deref(), Some("Read,Edit"));
    assert_eq!(cfg.plugin_dirs, vec![dir.path().join(".pas/plugin")]);
}

#[test]
fn resolve_claude_cli_config_cli_overrides_pas_toml() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("pas.toml"),
        r#"
[project]
name = "test"

[codergen.claude]
settings_mode = "strict_bare"
"#,
    )
    .unwrap();
    let mut snapshot = HashMap::new();
    snapshot.insert(
        "codergen.claude.settings_mode".into(),
        serde_json::json!("subscription-bare"),
    );

    let cfg = resolve_claude_cli_config(&snapshot, dir.path().to_str(), "code").unwrap();

    assert_eq!(cfg.settings_mode, ClaudeSettingsMode::SubscriptionBare);
}

#[test]
fn resolve_claude_cli_config_requires_sources_for_inherit() {
    let mut snapshot = HashMap::new();
    snapshot.insert(
        "codergen.claude.settings_mode".into(),
        serde_json::json!("inherit"),
    );

    let err = resolve_claude_cli_config(&snapshot, None, "code").unwrap_err();

    assert!(err
        .to_string()
        .contains("requires explicit setting_sources"));
}

#[test]
fn build_cli_command_codex_uses_exec_with_positional_prompt() {
    let node = make_node("n", "box", Some("do work"), HashMap::new());
    let graph = make_minimal_graph();
    let cfg = CliRunConfig {
        provider: LlmCliProvider::Codex,
        prompt: "test prompt",
        model: None,
        workdir: Some("/tmp"),
        node: &node,
        graph: &graph,
        claude: ClaudeCliConfig::default(),
    };
    let cmd = build_cli_command(&cfg);
    let args: Vec<_> = cmd
        .as_std()
        .get_args()
        .map(|a| a.to_str().unwrap())
        .collect();
    assert_eq!(args.first(), Some(&"exec"));
    assert!(args.contains(&"--json"));
    assert!(args.contains(&"--yolo"));
    // Prompt should be last (positional)
    assert_eq!(args.last(), Some(&"test prompt"));
    // Should NOT contain -p flag
    assert!(!args.contains(&"-p"));
}

#[test]
fn build_cli_command_gemini_matches_documented_invocation() {
    let node = make_node("n", "box", Some("do work"), HashMap::new());
    let graph = make_minimal_graph();
    let cfg = CliRunConfig {
        provider: LlmCliProvider::Gemini,
        prompt: "test prompt",
        model: Some("gemini-2.5-pro"),
        workdir: None,
        node: &node,
        graph: &graph,
        claude: ClaudeCliConfig::default(),
    };
    let cmd = build_cli_command(&cfg);
    let args: Vec<_> = cmd
        .as_std()
        .get_args()
        .map(|a| a.to_str().unwrap())
        .collect();
    assert_eq!(
        args,
        vec![
            "--output-format",
            "json",
            "--approval-mode",
            "yolo",
            "--model",
            "gemini-2.5-pro",
            "test prompt",
        ]
    );
}

// --- CodergenHandler dry-run with provider ---

#[tokio::test]
async fn codergen_dry_run_includes_provider() {
    use attractor_types::Context;
    let handler = CodergenHandler;
    let mut node = make_node("llm_step", "box", Some("Do the thing"), HashMap::new());
    node.llm_provider = Some("gemini".into());
    let ctx = Context::default();
    ctx.set("dry_run", serde_json::Value::Bool(true)).await;
    let graph = make_minimal_graph();
    let resolved = ResolvedNode {
        node_id: node.id.clone(),
        kind: ResolvedNodeKind::Task,
        handler: crate::HandlerIdentity::Codergen,
        provider: Some(LlmCliProvider::Gemini),
        invocation: Default::default(),
    };

    let outcome = handler
        .execute_resolved(&node, &resolved, &ctx, &graph)
        .await
        .unwrap();
    assert_eq!(outcome.status, StageStatus::Success);
    assert_eq!(
        outcome.context_updates.get("llm_step.provider"),
        Some(&serde_json::Value::String("Gemini CLI".into()))
    );
    assert!(outcome.notes.contains("Gemini CLI"));
}

#[tokio::test]
async fn codergen_rejects_missing_provider_even_in_dry_run() {
    use attractor_types::Context;
    let handler = CodergenHandler;
    let node = make_node("llm_step", "box", Some("Do the thing"), HashMap::new());
    let ctx = Context::default();
    ctx.set("dry_run", serde_json::Value::Bool(true)).await;
    let graph = make_minimal_graph();
    let resolved = ResolvedNode {
        node_id: node.id.clone(),
        kind: ResolvedNodeKind::Task,
        handler: crate::HandlerIdentity::Codergen,
        provider: None,
        invocation: Default::default(),
    };

    let error = handler
        .execute_resolved(&node, &resolved, &ctx, &graph)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("no provider"));
}

#[test]
fn extract_label_finds_exact_last_line() {
    let labels = vec!["BUY".into(), "HOLD".into(), "SELL".into()];
    let response = "Based on analysis, I recommend:\n\nBUY";
    assert_eq!(extract_label(response, &labels), Some("BUY".into()));
}

#[test]
fn extract_label_case_insensitive() {
    let labels = vec!["BUY".into(), "HOLD".into(), "SELL".into()];
    let response = "The recommendation is:\n\nhold";
    assert_eq!(extract_label(response, &labels), Some("HOLD".into()));
}

#[test]
fn extract_label_fallback_to_body_scan() {
    let labels = vec!["BUY".into(), "HOLD".into(), "SELL".into()];
    let response = "I recommend a SELL rating because the player is declining.";
    assert_eq!(extract_label(response, &labels), Some("SELL".into()));
}

#[test]
fn extract_label_returns_none_when_no_match() {
    let labels = vec!["BUY".into(), "HOLD".into(), "SELL".into()];
    let response = "This player is interesting but I need more data.";
    assert_eq!(extract_label(response, &labels), None);
}
