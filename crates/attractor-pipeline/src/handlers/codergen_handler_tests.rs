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
fn build_cli_command_claude_has_stream_json_output() {
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
    let format = args.iter().position(|a| *a == "--output-format").unwrap();
    assert_eq!(args[format + 1], "stream-json");
    assert!(args.contains(&"--verbose"));
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

// --- Claude stream-json parsing ---

const CLAUDE_RESULT_LINE: &str = r#"{"type":"result","subtype":"success","is_error":false,"result":"done","total_cost_usd":0.01,"num_turns":2}"#;

#[test]
fn parse_claude_stream_uses_final_result_line_like_json_mode() {
    let stream = format!(
        "{}\n{}\n{}\n",
        r#"{"type":"system","subtype":"init","model":"claude-haiku"}"#,
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"working"}]}}"#,
        CLAUDE_RESULT_LINE
    );
    let streamed = parse_claude_output(&stream, "n").unwrap();
    let single = parse_claude_output(CLAUDE_RESULT_LINE, "n").unwrap();
    assert_eq!(streamed.text, "done");
    assert_eq!(streamed.text, single.text);
    assert_eq!(streamed.is_error, single.is_error);
    assert_eq!(streamed.cost_usd, single.cost_usd);
    assert_eq!(streamed.turns, single.turns);
}

#[test]
fn parse_claude_stream_last_result_line_wins_and_crlf_is_accepted() {
    let stream = format!(
        "{}\r\n{}\r\n",
        r#"{"type":"result","result":"first","is_error":false}"#,
        r#"{"type":"result","result":"second","is_error":true,"subtype":"error"}"#
    );
    let parsed = parse_claude_output(&stream, "n").unwrap();
    assert_eq!(parsed.text, "second");
    assert!(parsed.is_error);
}

#[test]
fn parse_claude_stream_without_result_line_is_a_parse_error() {
    let stream = "{\"type\":\"system\"}\n{\"type\":\"assistant\"}\n";
    assert_eq!(claude_result_line(stream), None);
    let error = parse_claude_output(stream, "n").unwrap_err().to_string();
    assert!(error.contains("Failed to parse Claude output"), "{error}");
}

#[test]
fn claude_result_line_ignores_non_json_and_non_result_lines() {
    let stream = format!("{CLAUDE_RESULT_LINE}\nnot json\n{{\"type\":\"rate_limit_event\"}}\nlast");
    assert_eq!(claude_result_line(&stream), Some(CLAUDE_RESULT_LINE));
}

// --- Streaming Transcripts with stub providers ---

#[cfg(unix)]
mod transcripts {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use attractor_types::{AttractorError, Context, Outcome, Result};

    use super::*;

    fn stub(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("provider-stub");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn transcripts(run_dir: &Path) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(run_dir.join("transcripts")) else {
            return vec![];
        };
        let mut files: Vec<_> = entries.map(|e| e.unwrap().path()).collect();
        files.sort();
        files
    }

    fn only_transcript(run_dir: &Path) -> PathBuf {
        let files = transcripts(run_dir);
        assert_eq!(files.len(), 1, "expected one Transcript: {files:?}");
        files.into_iter().next().unwrap()
    }

    async fn run(
        provider: LlmCliProvider,
        program: PathBuf,
        run_dir: Option<&Path>,
        dry_run: bool,
        timeout: Option<Duration>,
    ) -> Result<Outcome> {
        let mut node = make_node("step", "box", Some("do work"), HashMap::new());
        node.timeout = timeout;
        let resolved = ResolvedNode {
            node_id: node.id.clone(),
            kind: ResolvedNodeKind::Task,
            handler: crate::HandlerIdentity::Codergen,
            provider: Some(provider),
            invocation: Default::default(),
        };
        CodergenHandler
            .execute_with_controls(
                &node,
                &resolved,
                &Context::default(),
                &make_minimal_graph(),
                CodergenExecutionControls {
                    dry_run,
                    workdir: None,
                    claude: ClaudeCliConfig::default(),
                    run_dir: run_dir.map(Path::to_path_buf),
                    program: Some(program),
                },
            )
            .await
    }

    async fn run_claude(program: PathBuf, run_dir: Option<&Path>) -> Result<Outcome> {
        run(LlmCliProvider::Claude, program, run_dir, false, None).await
    }

    fn handler_error(message: &str) -> String {
        AttractorError::HandlerError {
            handler: "codergen".into(),
            node: "step".into(),
            message: message.into(),
        }
        .to_string()
    }

    // AC1: one Transcript per Model Invocation, named by Invocation ID.
    #[tokio::test]
    async fn each_invocation_writes_one_transcript_named_by_invocation_id() {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path().join("run");
        let program = stub(tmp.path(), &format!("echo '{CLAUDE_RESULT_LINE}'"));

        for _ in 0..2 {
            let outcome = run_claude(program.clone(), Some(&run_dir)).await.unwrap();
            assert_eq!(outcome.status, StageStatus::Success);
            assert_eq!(outcome.notes, "done");
            assert_eq!(
                outcome.context_updates.get("step.result"),
                Some(&serde_json::json!("done"))
            );
            assert_eq!(
                outcome.context_updates.get("step.turns"),
                Some(&serde_json::json!(2))
            );
        }

        let files = transcripts(&run_dir);
        assert_eq!(files.len(), 2, "{files:?}");
        let mut ids = Vec::new();
        for file in &files {
            assert_eq!(file.extension().unwrap(), "jsonl");
            let id = file.file_stem().unwrap().to_str().unwrap();
            let uuid = uuid::Uuid::parse_str(id).unwrap();
            assert_eq!(uuid.get_version_num(), 7);
            assert_eq!(id, uuid.hyphenated().to_string());
            assert_eq!(
                std::fs::read_to_string(file).unwrap(),
                format!("{CLAUDE_RESULT_LINE}\n")
            );
            ids.push(id.to_string());
        }
        assert_ne!(ids[0], ids[1]);
    }

    #[tokio::test]
    async fn no_transcript_without_run_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let program = stub(tmp.path(), &format!("echo '{CLAUDE_RESULT_LINE}'"));
        let outcome = run_claude(program, None).await.unwrap();
        assert_eq!(outcome.status, StageStatus::Success);
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("provider-stub")]);
    }

    #[tokio::test]
    async fn no_transcript_in_dry_run() {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path().join("run");
        let program = stub(tmp.path(), &format!("echo '{CLAUDE_RESULT_LINE}'"));
        let outcome = run(LlmCliProvider::Claude, program, Some(&run_dir), true, None)
            .await
            .unwrap();
        assert_eq!(
            outcome.context_updates.get("step.dry_run"),
            Some(&serde_json::json!(true))
        );
        assert!(!run_dir.exists());
    }

    #[tokio::test]
    async fn missing_binary_leaves_no_transcript() {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path().join("run");
        let error = run_claude(tmp.path().join("no-such-provider"), Some(&run_dir))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, AttractorError::CliNotFound { binary } if binary == "claude"),
            "{error}"
        );
        assert_eq!(transcripts(&run_dir), Vec::<PathBuf>::new());
    }

    // AC2: the Transcript grows while the provider is still running.
    #[tokio::test]
    async fn transcript_grows_while_provider_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path().join("run");
        let marker = tmp.path().join("provider-exiting");
        let program = stub(
            tmp.path(),
            &format!(
                "echo '{{\"type\":\"system\"}}'; sleep 0.3\n\
                 echo '{{\"type\":\"assistant\",\"n\":1}}'; sleep 0.3\n\
                 echo '{{\"type\":\"assistant\",\"n\":2}}'; sleep 0.3\n\
                 echo '{CLAUDE_RESULT_LINE}'; sleep 0.3\n\
                 touch '{}'",
                marker.display()
            ),
        );

        let poll = async {
            let mut sizes: Vec<u64> = Vec::new();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
            loop {
                // Read the size first, then check the marker: a size read
                // before the marker exists was observed before the stub exited.
                let size = transcripts(&run_dir)
                    .first()
                    .and_then(|path| std::fs::metadata(path).ok())
                    .map(|meta| meta.len());
                if marker.exists() || tokio::time::Instant::now() > deadline {
                    break sizes;
                }
                if let Some(size) = size.filter(|size| *size > 0) {
                    if sizes.last() != Some(&size) {
                        sizes.push(size);
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        let (outcome, sizes) = tokio::join!(run_claude(program, Some(&run_dir)), poll);

        assert_eq!(outcome.unwrap().notes, "done");
        assert!(sizes.len() >= 2, "sizes seen before exit: {sizes:?}");
        assert!(sizes.windows(2).all(|w| w[0] < w[1]), "{sizes:?}");
        let final_size = std::fs::metadata(only_transcript(&run_dir)).unwrap().len();
        assert!(*sizes.last().unwrap() <= final_size);
    }

    // AC3: the Transcript is the provider's stdout byte for byte.
    #[tokio::test]
    async fn transcript_matches_provider_stdout_byte_for_byte() {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path().join("run");
        let fixture = tmp.path().join("stdout.bin");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"{\"type\":\"system\",\"note\":\"caf\xc3\xa9 \xe2\x9c\x93\"}\n");
        bytes.extend_from_slice(b"{\"type\":\"assistant\",\"text\":\"a\\tb\"}\t\r\n");
        bytes.extend_from_slice(b"invalid utf-8: \xff\xfe\n");
        bytes.extend_from_slice(b"\n");
        bytes.extend_from_slice(CLAUDE_RESULT_LINE.as_bytes());
        bytes.extend_from_slice(b"\nno trailing newline");
        std::fs::write(&fixture, &bytes).unwrap();
        let program = stub(
            tmp.path(),
            &format!("cat '{}'; echo 'stderr noise' >&2", fixture.display()),
        );

        let outcome = run_claude(program, Some(&run_dir)).await.unwrap();

        assert_eq!(outcome.status, StageStatus::Success);
        assert_eq!(outcome.notes, "done");
        assert_eq!(std::fs::read(only_transcript(&run_dir)).unwrap(), bytes);
    }

    #[tokio::test]
    async fn codex_and_gemini_output_is_streamed_to_transcripts() {
        let tmp = tempfile::tempdir().unwrap();

        let codex_dir = tmp.path().join("codex-run");
        let codex_out = "{\"type\":\"thread.started\"}\n\
            {\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"codex done\"}}\n\
            {\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}\n";
        let program = stub(tmp.path(), &format!("printf '%s' '{codex_out}'"));
        let outcome = run(
            LlmCliProvider::Codex,
            program,
            Some(&codex_dir),
            false,
            None,
        )
        .await
        .unwrap();
        assert_eq!(outcome.notes, "codex done");
        assert_eq!(
            std::fs::read_to_string(only_transcript(&codex_dir)).unwrap(),
            codex_out
        );

        let gemini_dir = tmp.path().join("gemini-run");
        let gemini_out = "{\n  \"response\": \"gemini done\"\n}";
        let program = stub(tmp.path(), &format!("printf '%s' '{gemini_out}'"));
        let outcome = run(
            LlmCliProvider::Gemini,
            program,
            Some(&gemini_dir),
            false,
            None,
        )
        .await
        .unwrap();
        assert_eq!(outcome.notes, "gemini done");
        assert_eq!(
            std::fs::read_to_string(only_transcript(&gemini_dir)).unwrap(),
            gemini_out
        );
    }

    // AC4: a provider that exits non-zero keeps its partial Transcript.
    #[tokio::test]
    async fn nonzero_exit_keeps_partial_transcript() {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path().join("run");
        let partial = "{\"type\":\"system\"}\n{\"type\":\"assistant\"}\n";
        let program = stub(
            tmp.path(),
            &format!("printf '%s' '{partial}'; echo 'crashed' >&2; exit 3"),
        );

        let error = run_claude(program, Some(&run_dir)).await.unwrap_err();

        assert_eq!(
            error.to_string(),
            handler_error("Claude Code exited with exit status: 3: crashed")
        );
        assert_eq!(
            std::fs::read_to_string(only_transcript(&run_dir)).unwrap(),
            partial
        );
    }

    #[tokio::test]
    async fn nonzero_exit_with_final_result_is_parsed_and_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path().join("run");
        let result =
            r#"{"type":"result","subtype":"error","is_error":true,"result":"budget exceeded"}"#;
        let program = stub(
            tmp.path(),
            &format!("echo '{{\"type\":\"system\"}}'; echo '{result}'; exit 1"),
        );

        let outcome = run_claude(program, Some(&run_dir)).await.unwrap();

        assert_eq!(outcome.status, StageStatus::Fail);
        assert_eq!(outcome.notes, "budget exceeded");
        assert_eq!(
            outcome.failure_reason.as_deref(),
            Some("Claude Code returned an error")
        );
        assert_eq!(
            std::fs::read_to_string(only_transcript(&run_dir)).unwrap(),
            format!("{{\"type\":\"system\"}}\n{result}\n")
        );
    }

    #[tokio::test]
    async fn timeout_keeps_partial_transcript() {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path().join("run");
        let program = stub(tmp.path(), "echo '{\"type\":\"system\"}'; sleep 10");

        let error = run(
            LlmCliProvider::Claude,
            program,
            Some(&run_dir),
            false,
            Some(Duration::from_millis(500)),
        )
        .await
        .unwrap_err();

        assert!(
            matches!(error, AttractorError::CommandTimeout { timeout_ms: 500 }),
            "{error}"
        );
        assert_eq!(
            std::fs::read_to_string(only_transcript(&run_dir)).unwrap(),
            "{\"type\":\"system\"}\n"
        );
    }

    // AC5: no output → empty Transcript and the same error as before.
    #[tokio::test]
    async fn silent_provider_leaves_empty_transcript_and_same_error() {
        let cases = [
            (
                "echo boom >&2; exit 0",
                handler_error("Claude Code produced no output. stderr: boom\n"),
            ),
            (
                "echo boom >&2; exit 2",
                handler_error("Claude Code exited with exit status: 2: boom"),
            ),
        ];
        for (body, expected) in cases {
            let tmp = tempfile::tempdir().unwrap();
            let run_dir = tmp.path().join("run");
            let program = stub(tmp.path(), body);

            let error = run_claude(program, Some(&run_dir)).await.unwrap_err();

            assert_eq!(error.to_string(), expected, "stub: {body}");
            let transcript = only_transcript(&run_dir);
            assert_eq!(std::fs::metadata(transcript).unwrap().len(), 0);
        }
    }

    #[tokio::test]
    async fn transcript_write_failure_does_not_fail_stage() {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path().join("run");
        std::fs::create_dir_all(&run_dir).unwrap();
        // `transcripts` is a regular file, so no Transcript can be created.
        std::fs::write(run_dir.join("transcripts"), "").unwrap();
        let program = stub(tmp.path(), &format!("echo '{CLAUDE_RESULT_LINE}'"));

        let outcome = run_claude(program, Some(&run_dir)).await.unwrap();

        assert_eq!(outcome.status, StageStatus::Success);
        assert_eq!(outcome.notes, "done");
        assert!(run_dir.join("transcripts").is_file());
    }
}
