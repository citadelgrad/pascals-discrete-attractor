use attractor_dot::AttributeValue;
use attractor_types::{AttractorError, Result};
use serde::Deserialize;

use crate::graph::{PipelineGraph, PipelineNode};

// ---------------------------------------------------------------------------
// LlmCliProvider — which CLI tool to invoke for an LLM node
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LlmCliProvider {
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
                tracing::warn!(provider = other, "Unknown llm_provider, defaulting to Claude");
                Ok(Self::Claude)
            }
        }
    }
}

impl LlmCliProvider {
    pub(super) fn from_node(node: &PipelineNode) -> Self {
        node.llm_provider
            .as_deref()
            .map(|s| s.parse().unwrap_or(Self::Claude))
            .unwrap_or(Self::Claude)
    }

    pub(super) fn binary_name(&self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
        }
    }

    pub(super) fn display_name(&self) -> &'static str {
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
pub(super) struct ClaudeOutput {
    #[serde(default)]
    pub(super) result: String,
    #[serde(default)]
    pub(super) is_error: bool,
    #[serde(default)]
    pub(super) subtype: String,
    #[serde(default)]
    pub(super) total_cost_usd: f64,
    #[serde(default)]
    pub(super) num_turns: u32,
}

/// Codex JSONL event (tagged enum for streaming deserializer).
/// Source: codex-rs/exec/src/exec_events.rs — ThreadEvent has 8 variants.
#[derive(Deserialize)]
#[serde(tag = "type")]
pub(super) enum CodexEvent {
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
pub(super) struct CodexItem {
    #[serde(rename = "type")]
    pub(super) item_type: String,
    #[serde(default)]
    pub(super) text: Option<String>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
pub(super) struct CodexUsage {
    pub(super) input_tokens: i64,
    pub(super) output_tokens: i64,
    #[serde(default)]
    pub(super) cached_input_tokens: i64,
}

#[derive(Deserialize)]
pub(super) struct CodexError {
    pub(super) message: String,
}

/// Gemini JSON output (single object).
/// Source: packages/core/src/output/types.ts — JsonOutput interface.
#[derive(Deserialize)]
pub(super) struct GeminiOutput {
    #[serde(default)]
    #[allow(dead_code)]
    pub(super) session_id: Option<String>,
    #[serde(default)]
    pub(super) response: Option<String>,
    #[serde(default)]
    pub(super) error: Option<GeminiError>,
}

#[derive(Deserialize)]
pub(super) struct GeminiError {
    #[serde(rename = "type")]
    #[allow(dead_code)]
    pub(super) error_type: String,
    pub(super) message: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub(super) code: Option<serde_json::Value>,
}

/// Normalized result from any CLI provider.
#[derive(Debug)]
pub(super) struct NormalizedCliResult {
    pub(super) text: String,
    pub(super) is_error: bool,
    pub(super) cost_usd: Option<f64>,
    pub(super) turns: Option<u32>,
    #[allow(dead_code)]
    pub(super) raw_output: String,
}

// ---------------------------------------------------------------------------
// CLI command builder
// ---------------------------------------------------------------------------

pub(super) struct CliRunConfig<'a> {
    pub(super) provider: LlmCliProvider,
    pub(super) prompt: &'a str,
    pub(super) model: Option<&'a str>,
    pub(super) workdir: Option<&'a str>,
    pub(super) node: &'a PipelineNode,
    #[allow(dead_code)]
    pub(super) graph: &'a PipelineGraph,
}

pub(super) fn build_cli_command(cfg: &CliRunConfig<'_>) -> tokio::process::Command {
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
            if let Some(AttributeValue::String(budget)) = cfg.node.raw_attrs.get("max_budget_usd")
            {
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

pub(super) fn parse_cli_output(
    provider: LlmCliProvider,
    stdout: &str,
    stderr: &str,
    node_id: &str,
) -> Result<NormalizedCliResult> {
    if stdout.trim().is_empty() {
        return Err(AttractorError::HandlerError {
            handler: "codergen".into(),
            node: node_id.into(),
            message: format!(
                "{} produced no output. stderr: {}",
                provider.display_name(),
                &stderr[..stderr.len().min(500)]
            ),
        });
    }

    match provider {
        LlmCliProvider::Claude => parse_claude_output(stdout, node_id),
        LlmCliProvider::Codex => parse_codex_output(stdout, node_id),
        LlmCliProvider::Gemini => parse_gemini_output(stdout, node_id),
    }
}

pub(super) fn parse_claude_output(stdout: &str, node_id: &str) -> Result<NormalizedCliResult> {
    let parsed: ClaudeOutput =
        serde_json::from_str(stdout).map_err(|e| AttractorError::HandlerError {
            handler: "codergen".into(),
            node: node_id.into(),
            message: format!(
                "Failed to parse Claude output: {} — raw: {}",
                e,
                &stdout[..stdout.len().min(500)]
            ),
        })?;
    Ok(NormalizedCliResult {
        text: parsed.result,
        is_error: parsed.is_error || parsed.subtype == "error",
        cost_usd: Some(parsed.total_cost_usd),
        turns: Some(parsed.num_turns),
        raw_output: stdout.to_string(),
    })
}

pub(super) fn parse_codex_output(stdout: &str, node_id: &str) -> Result<NormalizedCliResult> {
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

pub(super) fn parse_gemini_output(stdout: &str, node_id: &str) -> Result<NormalizedCliResult> {
    let parsed: GeminiOutput =
        serde_json::from_str(stdout).map_err(|e| AttractorError::HandlerError {
            handler: "codergen".into(),
            node: node_id.into(),
            message: format!(
                "Failed to parse Gemini output: {} — raw: {}",
                e,
                &stdout[..stdout.len().min(500)]
            ),
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
