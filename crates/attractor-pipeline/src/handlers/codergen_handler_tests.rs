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
fn provider_from_str_unknown_defaults_to_claude() {
    assert_eq!(
        "llama".parse::<LlmCliProvider>(),
        Ok(LlmCliProvider::Claude)
    );
}

#[test]
fn provider_from_node_defaults_to_claude() {
    let node = make_node("n", "box", Some("test"), HashMap::new());
    assert_eq!(LlmCliProvider::from_node(&node), LlmCliProvider::Claude);
}

#[test]
fn provider_from_node_reads_llm_provider() {
    let mut node = make_node("n", "box", Some("test"), HashMap::new());
    node.llm_provider = Some("codex".into());
    assert_eq!(LlmCliProvider::from_node(&node), LlmCliProvider::Codex);
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
    };
    let cmd = build_cli_command(&cfg);
    let args: Vec<_> = cmd
        .as_std()
        .get_args()
        .map(|a| a.to_str().unwrap())
        .collect();
    assert!(args.contains(&"--output-format"));
    assert!(args.contains(&"json"));
    assert!(args.contains(&"--model"));
    assert!(args.contains(&"sonnet"));
    assert!(args.contains(&"-p"));
}

#[test]
fn build_cli_command_codex_prompt_is_positional() {
    let node = make_node("n", "box", Some("do work"), HashMap::new());
    let graph = make_minimal_graph();
    let cfg = CliRunConfig {
        provider: LlmCliProvider::Codex,
        prompt: "test prompt",
        model: None,
        workdir: Some("/tmp"),
        node: &node,
        graph: &graph,
    };
    let cmd = build_cli_command(&cfg);
    let args: Vec<_> = cmd
        .as_std()
        .get_args()
        .map(|a| a.to_str().unwrap())
        .collect();
    assert!(args.contains(&"--json"));
    assert!(args.contains(&"--yolo"));
    // Prompt should be last (positional)
    assert_eq!(args.last(), Some(&"test prompt"));
    // Should NOT contain -p flag
    assert!(!args.contains(&"-p"));
}

#[test]
fn build_cli_command_gemini_uses_approval_mode() {
    let node = make_node("n", "box", Some("do work"), HashMap::new());
    let graph = make_minimal_graph();
    let cfg = CliRunConfig {
        provider: LlmCliProvider::Gemini,
        prompt: "test prompt",
        model: Some("gemini-2.5-pro"),
        workdir: None,
        node: &node,
        graph: &graph,
    };
    let cmd = build_cli_command(&cfg);
    let args: Vec<_> = cmd
        .as_std()
        .get_args()
        .map(|a| a.to_str().unwrap())
        .collect();
    assert!(args.contains(&"--approval-mode"));
    assert!(args.contains(&"yolo"));
    assert!(args.contains(&"--model"));
    assert!(args.contains(&"gemini-2.5-pro"));
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

    let outcome = handler.execute(&node, &ctx, &graph).await.unwrap();
    assert_eq!(outcome.status, StageStatus::Success);
    assert_eq!(
        outcome.context_updates.get("llm_step.provider"),
        Some(&serde_json::Value::String("Gemini CLI".into()))
    );
    assert!(outcome.notes.contains("Gemini CLI"));
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

// --- Response cache ---

use attractor_dot::AttributeValue;

fn parse_graph(dot: &str) -> crate::graph::PipelineGraph {
    crate::graph::PipelineGraph::from_dot(attractor_dot::parse(dot).unwrap()).unwrap()
}

#[test]
fn is_cacheable_conditional_by_default() {
    let graph = make_minimal_graph(); // "route" is not in this graph → acyclic
    let mut node = make_node("route", "diamond", Some("decide"), HashMap::new());
    assert!(is_cacheable_node(&node, &graph));
    node.shape = "box".into();
    node.node_type = Some("conditional".into());
    assert!(is_cacheable_node(&node, &graph));
}

#[test]
fn is_cacheable_box_node_default_off() {
    let graph = make_minimal_graph();
    let node = make_node("work", "box", Some("do it"), HashMap::new());
    assert!(!is_cacheable_node(&node, &graph));
}

#[test]
fn is_cacheable_respects_ro_and_off_attrs() {
    let graph = make_minimal_graph();

    let mut ro = HashMap::new();
    ro.insert("cache".to_string(), AttributeValue::String("ro".into()));
    let node = make_node("analyze", "box", Some("read"), ro);
    assert!(is_cacheable_node(&node, &graph));

    let mut off = HashMap::new();
    off.insert("cache".to_string(), AttributeValue::String("off".into()));
    let node = make_node("route", "diamond", Some("decide"), off);
    assert!(!is_cacheable_node(&node, &graph)); // explicit off overrides the diamond default
}

#[test]
fn is_cacheable_excludes_conditional_in_a_loop() {
    // A routing node inside a fix loop must NOT be cached by default — a cached
    // label would pin the loop until max_steps. `cache="ro"` overrides.
    let graph = parse_graph(
        r#"digraph L {
            check [shape="diamond"]
            fix   [shape="box"]
            done  [shape="Msquare"]
            check -> fix  [label="FIXME"]
            fix   -> check
            check -> done [label="DONE"]
        }"#,
    );
    assert!(graph.node_in_cycle("check"));

    let node = make_node("check", "diamond", Some("evaluate"), HashMap::new());
    assert!(!is_cacheable_node(&node, &graph));

    // Explicit ro opt-in is honoured even inside the loop.
    let mut ro = HashMap::new();
    ro.insert("cache".to_string(), AttributeValue::String("ro".into()));
    let ro_node = make_node("check", "diamond", Some("evaluate"), ro);
    assert!(is_cacheable_node(&ro_node, &graph));
}

#[test]
fn assemble_prompt_is_deterministic_and_contains_task() {
    let node = make_node("n", "box", Some("Do the thing"), HashMap::new());
    let graph = make_minimal_graph();
    let mut snap = HashMap::new();
    snap.insert("a.result".to_string(), serde_json::json!("alpha"));
    snap.insert("b.output".to_string(), serde_json::json!("beta"));

    let p1 = assemble_prompt(&node, &graph, &snap);
    let p2 = assemble_prompt(&node, &graph, &snap);
    assert_eq!(p1, p2);
    assert!(p1.contains("Task (n): Do the thing"));
    assert!(p1.contains("a.result"));
}

#[test]
fn build_cache_key_is_stable_and_input_sensitive() {
    let node = make_node("n", "box", Some("x"), HashMap::new());
    let k1 = build_cache_key(
        LlmCliProvider::Claude,
        Some("opus"),
        &node,
        "/w",
        "prompt A",
    );
    let k2 = build_cache_key(
        LlmCliProvider::Claude,
        Some("opus"),
        &node,
        "/w",
        "prompt A",
    );
    assert_eq!(k1, k2);

    let k_diff_prompt = build_cache_key(
        LlmCliProvider::Claude,
        Some("opus"),
        &node,
        "/w",
        "prompt B",
    );
    assert_ne!(k1, k_diff_prompt);

    let k_diff_model = build_cache_key(
        LlmCliProvider::Claude,
        Some("sonnet"),
        &node,
        "/w",
        "prompt A",
    );
    assert_ne!(k1, k_diff_model);

    // Different working directories must not collide, even with identical prompts.
    let k_diff_workdir = build_cache_key(
        LlmCliProvider::Claude,
        Some("opus"),
        &node,
        "/other",
        "prompt A",
    );
    assert_ne!(k1, k_diff_workdir);
}

#[tokio::test]
async fn codergen_cache_hit_skips_cli() {
    use attractor_types::Context;

    let tmp = tempfile::tempdir().unwrap();
    let handler = CodergenHandler;

    let mut node = make_node("route", "diamond", Some("Decide the route"), HashMap::new());
    node.node_type = Some("conditional".into());
    let graph = make_minimal_graph();

    let ctx = Context::default();
    ctx.set(
        "__cache_mode",
        serde_json::Value::String("readwrite".into()),
    )
    .await;
    ctx.set(
        "__cache_dir",
        serde_json::Value::String(tmp.path().to_string_lossy().into_owned()),
    )
    .await;
    // Set an explicit workdir so the effective-workdir key input is deterministic
    // (otherwise it would resolve to the test process's current directory).
    let workdir = tmp.path().to_string_lossy().into_owned();
    ctx.set("workdir", serde_json::Value::String(workdir.clone()))
        .await;

    // Compute the key exactly as execute() will, and seed a result at it.
    let snapshot = ctx.snapshot().await;
    let full_prompt = assemble_prompt(&node, &graph, &snapshot);
    let key = build_cache_key(LlmCliProvider::Claude, None, &node, &workdir, &full_prompt);

    let cache = attractor_cache::Cache::new(attractor_cache::CacheConfig::new(
        attractor_cache::CacheMode::ReadWrite,
        Some(tmp.path().to_path_buf()),
        None,
    ));
    let entry = attractor_cache::CacheEntry::new(
        "Claude Code",
        "Decision: GO\nGO",
        Some(0.42),
        Some(2),
        Some("GO".into()),
    );
    cache.put(&key, &entry).unwrap();

    // Execute — a hit must return the cached result without spawning `claude`.
    let outcome = handler.execute(&node, &ctx, &graph).await.unwrap();
    assert_eq!(outcome.status, StageStatus::Success);
    assert_eq!(outcome.preferred_label.as_deref(), Some("GO"));
    assert_eq!(outcome.notes, "Decision: GO\nGO");
    assert_eq!(
        outcome.context_updates.get("route.cache_hit"),
        Some(&serde_json::Value::Bool(true))
    );
    assert_eq!(
        outcome.context_updates.get("route.cost_usd"),
        Some(&serde_json::json!(0.0))
    );
    assert_eq!(
        outcome.context_updates.get("route.cache_saved_usd"),
        Some(&serde_json::json!(0.42))
    );
}

#[test]
fn cached_outcome_reports_zero_cost_and_saved() {
    let node = make_node("route", "diamond", Some("decide"), HashMap::new());
    let entry = attractor_cache::CacheEntry::new(
        "Claude Code",
        "result text",
        Some(0.5),
        Some(1),
        Some("YES".into()),
    );
    let outcome = cached_outcome(&node, LlmCliProvider::Claude, &entry);
    assert_eq!(outcome.preferred_label.as_deref(), Some("YES"));
    assert_eq!(
        outcome.context_updates.get("route.completed"),
        Some(&serde_json::Value::Bool(true))
    );
    assert_eq!(
        outcome.context_updates.get("route.cost_usd"),
        Some(&serde_json::json!(0.0))
    );
    assert_eq!(
        outcome.context_updates.get("route.cache_saved_usd"),
        Some(&serde_json::json!(0.5))
    );
}
