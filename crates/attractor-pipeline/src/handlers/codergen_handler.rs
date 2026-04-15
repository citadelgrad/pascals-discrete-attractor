use std::collections::HashMap;

use async_trait::async_trait;
use attractor_dot::AttributeValue;
use attractor_types::{AttractorError, Context, Outcome, Result, StageStatus};
use serde::Deserialize;

use crate::graph::{PipelineGraph, PipelineNode};
use crate::handler::NodeHandler;

// ---------------------------------------------------------------------------
// LlmCliProvider — which CLI tool to invoke for an LLM node
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LlmCliProvider {
    Claude,
    Codex,
    Gemini,
}

impl std::str::FromStr for LlmCliProvider {
    type Err = (); // Never fails — defaults to Claude with warning

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "claude" | "anthropic" => Ok(Self::Claude),
            "codex" | "openai" => Ok(Self::Codex),
            "gemini" | "google" => Ok(Self::Gemini),
            other => {
                tracing::warn!(
                    provider = other,
                    "Unknown llm_provider, defaulting to Claude"
                );
                Ok(Self::Claude)
            }
        }
    }
}

impl LlmCliProvider {
    fn from_node(node: &PipelineNode) -> Self {
        node.llm_provider
            .as_deref()
            .map(|s| s.parse().unwrap_or(Self::Claude))
            .unwrap_or(Self::Claude)
    }

    fn binary_name(&self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
        }
    }

    fn display_name(&self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex CLI",
            Self::Gemini => "Gemini CLI",
        }
    }
}

// ---------------------------------------------------------------------------
// CLI output structs
// ---------------------------------------------------------------------------

/// Result shape from `claude -p --output-format json`
#[derive(Deserialize)]
struct ClaudeOutput {
    #[serde(default)]
    result: String,
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    subtype: String,
    #[serde(default)]
    total_cost_usd: f64,
    #[serde(default)]
    num_turns: u32,
}

/// Codex JSONL event (tagged enum for streaming deserializer).
/// Source: codex-rs/exec/src/exec_events.rs — ThreadEvent has 8 variants.
#[derive(Deserialize)]
#[serde(tag = "type")]
enum CodexEvent {
    #[serde(rename = "item.completed")]
    ItemCompleted { item: CodexItem },
    #[serde(rename = "turn.completed")]
    TurnCompleted {
        #[allow(dead_code)]
        usage: Option<CodexUsage>,
    },
    #[serde(rename = "turn.failed")]
    TurnFailed { error: Option<CodexError> },
    /// Top-level fatal stream error — distinct from turn.failed.
    #[serde(rename = "error")]
    Error { message: String },
    #[serde(other)]
    Other, // Absorbs thread.started, turn.started, item.started, item.updated
}

#[derive(Deserialize)]
struct CodexItem {
    #[serde(rename = "type")]
    item_type: String,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct CodexUsage {
    input_tokens: i64,
    output_tokens: i64,
    #[serde(default)]
    cached_input_tokens: i64,
}

#[derive(Deserialize)]
struct CodexError {
    message: String,
}

/// Gemini JSON output (single object).
/// Source: packages/core/src/output/types.ts — JsonOutput interface.
#[derive(Deserialize)]
struct GeminiOutput {
    #[serde(default)]
    #[allow(dead_code)]
    session_id: Option<String>,
    #[serde(default)]
    response: Option<String>,
    #[serde(default)]
    error: Option<GeminiError>,
}

#[derive(Deserialize)]
struct GeminiError {
    #[serde(rename = "type")]
    #[allow(dead_code)]
    error_type: String,
    message: String,
    #[serde(default)]
    #[allow(dead_code)]
    code: Option<serde_json::Value>,
}

/// Normalized result from any CLI provider.
#[derive(Debug)]
struct NormalizedCliResult {
    text: String,
    is_error: bool,
    cost_usd: Option<f64>,
    turns: Option<u32>,
    #[allow(dead_code)]
    raw_output: String,
}

// ---------------------------------------------------------------------------
// CLI command builder
// ---------------------------------------------------------------------------

struct CliRunConfig<'a> {
    provider: LlmCliProvider,
    prompt: &'a str,
    model: Option<&'a str>,
    workdir: Option<&'a str>,
    node: &'a PipelineNode,
    #[allow(dead_code)]
    graph: &'a PipelineGraph,
}

fn build_cli_command(cfg: &CliRunConfig<'_>) -> tokio::process::Command {
    let mut cmd = match cfg.provider {
        LlmCliProvider::Claude => {
            let mut cmd = tokio::process::Command::new("claude");
            cmd.arg("-p")
                .arg(cfg.prompt)
                .arg("--output-format")
                .arg("json")
                .arg("--no-session-persistence")
                .arg("--dangerously-skip-permissions")
                .arg("--strict-mcp-config")
                .arg("--disable-slash-commands");
            if let Some(model) = cfg.model {
                cmd.arg("--model").arg(model);
            }
            if let Some(AttributeValue::String(tools)) = cfg.node.raw_attrs.get("allowed_tools") {
                cmd.arg("--allowedTools").arg(tools);
            }
            if let Some(AttributeValue::String(budget)) = cfg.node.raw_attrs.get("max_budget_usd") {
                cmd.arg("--max-budget-usd").arg(budget);
            }
            cmd
        }
        LlmCliProvider::Codex => {
            let mut cmd = tokio::process::Command::new("codex");
            cmd.arg("--json")
                .arg("--yolo")
                .arg("--skip-git-repo-check")
                .arg("--ephemeral");
            if let Some(model) = cfg.model {
                cmd.arg("--model").arg(model);
            }
            if let Some(dir) = cfg.workdir {
                cmd.arg("--cd").arg(dir);
            }
            // Prompt is POSITIONAL (last arg) — NOT -p (that's --profile in Codex)
            cmd.arg(cfg.prompt);
            cmd
        }
        LlmCliProvider::Gemini => {
            let mut cmd = tokio::process::Command::new("gemini");
            cmd.arg("--output-format")
                .arg("json")
                .arg("--approval-mode")
                .arg("yolo");
            if let Some(model) = cfg.model {
                cmd.arg("--model").arg(model);
            }
            // Prompt is POSITIONAL (preferred) — -p/--prompt is deprecated
            cmd.arg(cfg.prompt);
            // Gemini has NO --cwd flag — working dir set via cmd.current_dir() only
            cmd
        }
    };

    if let Some(dir) = cfg.workdir {
        cmd.current_dir(dir);
    }
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd
}

// ---------------------------------------------------------------------------
// CLI output parsers
// ---------------------------------------------------------------------------

fn parse_cli_output(
    provider: LlmCliProvider,
    stdout: &str,
    stderr: &str,
    node_id: &str,
) -> Result<NormalizedCliResult> {
    if stdout.trim().is_empty() {
        let stderr_preview: String = stderr.chars().take(500).collect();
        return Err(AttractorError::HandlerError {
            handler: "codergen".into(),
            node: node_id.into(),
            message: format!(
                "{} produced no output. stderr: {}",
                provider.display_name(),
                stderr_preview
            ),
        });
    }

    match provider {
        LlmCliProvider::Claude => parse_claude_output(stdout, node_id),
        LlmCliProvider::Codex => parse_codex_output(stdout, node_id),
        LlmCliProvider::Gemini => parse_gemini_output(stdout, node_id),
    }
}

fn parse_claude_output(stdout: &str, node_id: &str) -> Result<NormalizedCliResult> {
    let parsed: ClaudeOutput =
        serde_json::from_str(stdout).map_err(|e| AttractorError::HandlerError {
            handler: "codergen".into(),
            node: node_id.into(),
            message: {
                let preview: String = stdout.chars().take(500).collect();
                format!("Failed to parse Claude output: {} — raw: {}", e, preview)
            },
        })?;
    Ok(NormalizedCliResult {
        text: parsed.result,
        is_error: parsed.is_error || parsed.subtype == "error",
        cost_usd: Some(parsed.total_cost_usd),
        turns: Some(parsed.num_turns),
        raw_output: stdout.to_string(),
    })
}

fn parse_codex_output(stdout: &str, node_id: &str) -> Result<NormalizedCliResult> {
    let mut last_message: Option<String> = None;
    let mut is_error = false;
    let mut error_message: Option<String> = None;

    for event in serde_json::Deserializer::from_str(stdout).into_iter::<CodexEvent>() {
        match event {
            Ok(CodexEvent::ItemCompleted { item }) => {
                if item.item_type == "agent_message" {
                    if let Some(text) = item.text {
                        last_message = Some(text);
                    }
                }
            }
            Ok(CodexEvent::TurnFailed { error }) => {
                is_error = true;
                error_message = error.map(|e| e.message);
            }
            Ok(CodexEvent::Error { message }) => {
                is_error = true;
                error_message = Some(message);
            }
            Ok(_) => {}
            Err(e) => {
                tracing::debug!(node = node_id, error = %e, "Skipping malformed Codex JSONL event");
            }
        }
    }

    let text = last_message
        .or(error_message)
        .unwrap_or_else(|| "No agent message found in Codex output".into());

    Ok(NormalizedCliResult {
        text,
        is_error,
        cost_usd: None,
        turns: None,
        raw_output: stdout.to_string(),
    })
}

fn parse_gemini_output(stdout: &str, node_id: &str) -> Result<NormalizedCliResult> {
    let parsed: GeminiOutput =
        serde_json::from_str(stdout).map_err(|e| AttractorError::HandlerError {
            handler: "codergen".into(),
            node: node_id.into(),
            message: {
                let preview: String = stdout.chars().take(500).collect();
                format!("Failed to parse Gemini output: {} — raw: {}", e, preview)
            },
        })?;

    if let Some(err) = parsed.error {
        return Ok(NormalizedCliResult {
            text: err.message,
            is_error: true,
            cost_usd: None,
            turns: None,
            raw_output: stdout.to_string(),
        });
    }

    Ok(NormalizedCliResult {
        text: parsed.response.unwrap_or_default(),
        is_error: false,
        cost_usd: None,
        turns: None,
        raw_output: stdout.to_string(),
    })
}

// ---------------------------------------------------------------------------
// CodergenHandler — LLM task handler (box shape)
//
// Shells out to a CLI tool (Claude Code, Codex CLI, or Gemini CLI) for each
// node, passing the node's prompt. The provider is selected via the
// `llm_provider` node attribute (default: claude).
//
// Supported node attributes:
//   - prompt (required): The task prompt sent to the CLI
//   - llm_provider: "claude", "codex", or "gemini" (default: "claude")
//   - llm_model: Override the model (e.g. "sonnet", "o3", "gemini-2.5-pro")
//   - allowed_tools: Comma-separated tool list (Claude only)
//   - max_budget_usd: Spending cap for this node (Claude only)
//   - timeout: Duration before the CLI invocation is killed (default: 10m)
//
// The pipeline context key "workdir" controls the working directory.
// ---------------------------------------------------------------------------

pub struct CodergenHandler;

#[async_trait]
impl NodeHandler for CodergenHandler {
    fn handler_type(&self) -> &str {
        "codergen"
    }

    async fn execute(
        &self,
        node: &PipelineNode,
        context: &Context,
        graph: &PipelineGraph,
    ) -> Result<Outcome> {
        let prompt = node.prompt.as_deref().unwrap_or("No prompt specified");
        let label = node.label.clone();
        let provider = LlmCliProvider::from_node(node);

        tracing::info!(
            node = %node.id,
            label = %label,
            provider = provider.display_name(),
            "Executing codergen handler"
        );

        // Check if dry_run is set in context
        let dry_run = context
            .get("dry_run")
            .await
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if dry_run {
            tracing::info!(node = %node.id, provider = provider.display_name(), "Dry run — skipping CLI execution");
            return Ok(Outcome {
                status: StageStatus::Success,
                preferred_label: None,
                suggested_next_ids: vec![],
                context_updates: {
                    let mut m = HashMap::new();
                    m.insert(
                        format!("{}.result", node.id),
                        serde_json::Value::String(format!("Dry run — prompt not sent: {}", prompt)),
                    );
                    m.insert(
                        format!("{}.completed", node.id),
                        serde_json::Value::Bool(true),
                    );
                    m.insert(
                        format!("{}.dry_run", node.id),
                        serde_json::Value::Bool(true),
                    );
                    m.insert(
                        format!("{}.provider", node.id),
                        serde_json::Value::String(provider.display_name().into()),
                    );
                    m
                },
                notes: format!(
                    "Dry run — {} not invoked for: {}",
                    provider.display_name(),
                    label
                ),
                failure_reason: None,
            });
        }

        // Build the full prompt with pipeline context
        let goal = &graph.goal;
        let mut full_prompt = String::new();

        if !goal.is_empty() {
            full_prompt.push_str(&format!("Pipeline goal: {}\n\n", goal));
        }

        // Inject relevant context from prior nodes
        let snapshot = context.snapshot().await;
        let context_keys: Vec<_> = snapshot
            .iter()
            .filter(|(k, _)| k.ends_with(".result") || k.ends_with(".output"))
            .collect();
        if !context_keys.is_empty() {
            full_prompt.push_str("Context from prior pipeline steps:\n");
            for (k, v) in &context_keys {
                if let serde_json::Value::String(s) = v {
                    full_prompt.push_str(&format!("- {}: {}\n", k, s));
                } else {
                    full_prompt.push_str(&format!("- {}: {}\n", k, v));
                }
            }
            full_prompt.push('\n');
        }

        full_prompt.push_str(&format!("Task ({}): {}", label, prompt));

        // If this is a conditional node, instruct the LLM to output a label
        if node.shape == "diamond" || node.node_type.as_deref() == Some("conditional") {
            let edges = graph.outgoing_edges(&node.id);
            let labels: Vec<_> = edges.iter().filter_map(|e| e.label.as_deref()).collect();
            if !labels.is_empty() {
                full_prompt.push_str(&format!(
                    "\n\nYou MUST end your response with exactly one of these labels on its own line: {}",
                    labels.join(", ")
                ));
            }
        }

        // Resolve model: node attribute, then graph-level fallback
        let model = node
            .llm_model
            .as_deref()
            .or_else(|| match graph.attrs.get("model") {
                Some(AttributeValue::String(m)) => Some(m.as_str()),
                _ => None,
            });

        // Resolve working directory from context
        let workdir = snapshot
            .get("workdir")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Build the CLI command via the provider-specific builder
        let mut cmd = build_cli_command(&CliRunConfig {
            provider,
            prompt: &full_prompt,
            model,
            workdir: workdir.as_deref(),
            node,
            graph,
        });

        // Spawn the CLI process — detect missing binary
        let child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                AttractorError::CliNotFound {
                    binary: provider.binary_name().to_string(),
                }
            } else {
                AttractorError::HandlerError {
                    handler: "codergen".into(),
                    node: node.id.clone(),
                    message: format!("Failed to spawn {}: {}", provider.display_name(), e),
                }
            }
        })?;

        // Apply timeout (default 10 minutes, configurable via node.timeout).
        // IMPORTANT: We capture the PID before wait_with_output() consumes the
        // Child. On timeout, we kill the process tree — tokio::time::timeout
        // only drops the future, it does NOT kill the child process.
        let child_pid = child.id();
        let timeout_dur = node.timeout.unwrap_or(std::time::Duration::from_secs(600));
        let output = match tokio::time::timeout(timeout_dur, child.wait_with_output()).await {
            Ok(result) => result.map_err(|e| AttractorError::HandlerError {
                handler: "codergen".into(),
                node: node.id.clone(),
                message: format!("{} execution failed: {}", provider.display_name(), e),
            })?,
            Err(_elapsed) => {
                // Timeout fired — kill the child process and its descendants
                if let Some(pid) = child_pid {
                    tracing::warn!(
                        node = %node.id,
                        pid = pid,
                        timeout_secs = timeout_dur.as_secs(),
                        "Killing timed-out {} process",
                        provider.display_name()
                    );
                    // SIGKILL the entire process group so MCP server children
                    // are also terminated (matches tool_handler behavior).
                    #[cfg(unix)]
                    unsafe {
                        libc::kill(-(pid as i32), libc::SIGKILL);
                    }
                }
                return Err(AttractorError::CommandTimeout {
                    timeout_ms: timeout_dur.as_millis() as u64,
                });
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !output.status.success() {
            return Err(AttractorError::HandlerError {
                handler: "codergen".into(),
                node: node.id.clone(),
                message: format!(
                    "{} exited with {}: {}",
                    provider.display_name(),
                    output.status,
                    stderr.trim()
                ),
            });
        }

        // Parse output via the provider-specific parser
        let cli_result = parse_cli_output(provider, &stdout, &stderr, &node.id)?;

        tracing::info!(
            node = %node.id,
            provider = provider.display_name(),
            is_error = cli_result.is_error,
            has_cost = cli_result.cost_usd.is_some(),
            "{} completed",
            provider.display_name()
        );

        // Determine status
        let status = if cli_result.is_error {
            StageStatus::Fail
        } else {
            StageStatus::Success
        };

        // Extract preferred_label from the response for conditional routing
        let preferred_label =
            if node.shape == "diamond" || node.node_type.as_deref() == Some("conditional") {
                let edges = graph.outgoing_edges(&node.id);
                let labels: Vec<String> = edges.iter().filter_map(|e| e.label.clone()).collect();
                extract_label(&cli_result.text, &labels)
            } else {
                None
            };

        // Build context updates
        let mut updates = HashMap::new();
        updates.insert(
            format!("{}.completed", node.id),
            serde_json::Value::Bool(true),
        );
        updates.insert(
            format!("{}.result", node.id),
            serde_json::Value::String(cli_result.text.clone()),
        );
        updates.insert(
            format!("{}.provider", node.id),
            serde_json::Value::String(provider.display_name().into()),
        );
        if let Some(cost) = cli_result.cost_usd {
            updates.insert(format!("{}.cost_usd", node.id), serde_json::json!(cost));
        }
        if let Some(turns) = cli_result.turns {
            updates.insert(format!("{}.turns", node.id), serde_json::json!(turns));
        }
        if let Some(ref lbl) = preferred_label {
            updates.insert(
                format!("{}.label", node.id),
                serde_json::Value::String(lbl.clone()),
            );
        }

        Ok(Outcome {
            status,
            preferred_label,
            suggested_next_ids: vec![],
            context_updates: updates,
            notes: cli_result.text,
            failure_reason: if status == StageStatus::Fail {
                Some(format!("{} returned an error", provider.display_name()))
            } else {
                None
            },
        })
    }
}

/// Scan the Claude response for one of the expected edge labels.
/// Checks the last few lines first (where we asked Claude to put it),
/// then falls back to scanning the full text.
fn extract_label(response: &str, labels: &[String]) -> Option<String> {
    let lines: Vec<&str> = response.lines().rev().take(5).collect();
    // Check last lines for an exact match
    for line in &lines {
        let trimmed = line.trim();
        for label in labels {
            if trimmed.eq_ignore_ascii_case(label) {
                return Some(label.clone());
            }
        }
    }
    // Fallback: search full response for label as a standalone word.
    // Sort labels longest-first to avoid substring false matches
    // (e.g. "BUY" matching inside "BUYOUT").
    let mut sorted_labels: Vec<&String> = labels.iter().collect();
    sorted_labels.sort_by_key(|b| std::cmp::Reverse(b.len()));
    let upper = response.to_uppercase();
    for label in sorted_labels {
        let label_upper = label.to_uppercase();
        if let Some(pos) = upper.find(&label_upper) {
            // Check word boundaries to avoid substring matches
            let before_ok = pos == 0
                || !upper.as_bytes()[pos - 1].is_ascii_alphanumeric();
            let end = pos + label_upper.len();
            let after_ok = end >= upper.len()
                || !upper.as_bytes()[end].is_ascii_alphanumeric();
            if before_ok && after_ok {
                return Some(label.clone());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
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
}
