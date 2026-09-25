//! [T2-5] Streaming codergen gives the same Outcomes as before.
//!
//! Each test runs the real handler on a stub provider that prints a recorded
//! fixture, and compares its Outcome with the Outcome the pre-streaming
//! handler (`legacy`) builds from the same provider response.

use std::collections::HashMap;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use attractor_types::{Context, Outcome, Result, StageStatus};

use super::*;
use crate::handlers::tests::make_node;

/// The pre-streaming codergen output path, frozen verbatim from `e89c2fe`
/// (the last commit before Epic attractor-ino):
/// - structs and parsers: `handlers/codergen_provider.rs` lines 13-104 and 243-372;
/// - `outcome`: `handlers/codergen_handler.rs` lines 260-345, with
///   `child.wait_with_output()` replaced by its `status`/`stdout`/`stderr`
///   and the `tracing::info!` log line dropped;
/// - `extract_label`: `handlers/codergen_handler.rs` lines 588-610.
///
/// It shares no code with the streaming handler, so a bug in a shared helper
/// cannot hide on both sides. Do not "fix" it: it is the reference.
mod legacy {
    use std::collections::HashMap;

    use attractor_types::{AttractorError, Outcome, Result, StageStatus};
    use serde::Deserialize;

    use crate::execution_plan::LlmProvider as LlmCliProvider;
    use crate::execution_plan::{ResolvedNode, ResolvedNodeKind};
    use crate::graph::{PipelineGraph, PipelineNode};

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

    /// Borrow the first `max` characters of `s`. Unlike byte slicing (`&s[..500]`),
    /// this never panics on a multi-byte UTF-8 boundary — CLI output is arbitrary
    /// text and may contain non-ASCII bytes exactly at the cutoff.
    fn head(s: &str, max: usize) -> &str {
        match s.char_indices().nth(max) {
            Some((idx, _)) => &s[..idx],
            None => s,
        }
    }

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
                    head(stderr, 500)
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
                    head(stdout, 500)
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
                    head(stdout, 500)
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

    /// The pre-streaming handler after the provider process exited.
    pub(super) fn outcome(
        provider: LlmCliProvider,
        exit: std::process::ExitStatus,
        stdout: &str,
        stderr: &str,
        node: &PipelineNode,
        resolved: &ResolvedNode,
        graph: &PipelineGraph,
    ) -> Result<Outcome> {
        if !exit.success() && stdout.is_empty() {
            return Err(AttractorError::HandlerError {
                handler: "codergen".into(),
                node: node.id.clone(),
                message: format!(
                    "{} exited with {}: {}",
                    provider.display_name(),
                    exit,
                    stderr.trim()
                ),
            });
        }

        // Parse output via the provider-specific parser
        let cli_result = parse_cli_output(provider, stdout, stderr, &node.id)?;

        // Determine status
        let status = if cli_result.is_error {
            StageStatus::Fail
        } else {
            StageStatus::Success
        };

        // Extract preferred_label from the response for conditional routing
        let preferred_label = if matches!(
            resolved.kind,
            ResolvedNodeKind::Conditional { llm_backed: true }
        ) {
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
        // Fallback: search full response for label as a standalone word
        let upper = response.to_uppercase();
        for label in labels {
            if upper.contains(&label.to_uppercase()) {
                return Some(label.clone());
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Provider responses: what the streaming handler reads, and what the
// pre-streaming handler read for the same response.
// ---------------------------------------------------------------------------

const STDERR: &str = "provider warning on stderr";

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/providers")
        .join(name);
    std::fs::read_to_string(path).unwrap()
}

/// One provider response in both output modes.
struct Response {
    case: &'static str,
    provider: LlmProvider,
    /// Gemini only: whether the stub's `--help` advertises `stream-json`.
    gemini_stream: bool,
    /// Stdout of the streaming handler's command.
    streamed: String,
    /// Stdout the pre-streaming handler's command printed for the same response.
    legacy: String,
}

/// The last non-empty line of a Claude `stream-json` stdout: the final result
/// message, which is all `claude -p --output-format json` prints.
fn claude_final_line(stream: &str) -> String {
    let last = stream.lines().rev().find(|l| !l.trim().is_empty()).unwrap();
    let value: serde_json::Value = serde_json::from_str(last).unwrap();
    assert_eq!(
        value["type"], "result",
        "last Claude line is the final result"
    );
    last.to_owned()
}

/// Replace the last non-empty line of a JSONL stream.
fn with_last_line(stream: &str, last: &str) -> String {
    let mut lines: Vec<&str> = stream.lines().filter(|l| !l.trim().is_empty()).collect();
    lines.pop();
    lines.push(last);
    lines.join("\n") + "\n"
}

fn claude(edit: impl Fn(&mut serde_json::Value)) -> Response {
    let stream = fixture("claude-2.1.282.stream.jsonl");
    let mut result: serde_json::Value = serde_json::from_str(&claude_final_line(&stream)).unwrap();
    edit(&mut result);
    let result = result.to_string();
    Response {
        case: "claude",
        provider: LlmProvider::Claude,
        gemini_stream: false,
        streamed: with_last_line(&stream, &result),
        legacy: result,
    }
}

fn codex(extra: &str) -> Response {
    let stream = fixture("codex-0.151.0.jsonl") + extra;
    Response {
        case: "codex",
        provider: LlmProvider::Codex,
        gemini_stream: false,
        streamed: stream.clone(),
        legacy: stream,
    }
}

fn gemini_stream(stream: String, json: String) -> Response {
    Response {
        case: "gemini stream-json",
        provider: LlmProvider::Gemini,
        gemini_stream: true,
        streamed: stream,
        legacy: json,
    }
}

fn gemini_json(json: String) -> Response {
    Response {
        case: "gemini json fallback",
        provider: LlmProvider::Gemini,
        gemini_stream: false,
        streamed: json.clone(),
        legacy: json,
    }
}

/// The four recorded (or constructed) fixture responses, unchanged.
fn recorded_responses() -> Vec<Response> {
    vec![
        claude(|_| {}),
        codex(""),
        gemini_stream(
            fixture("gemini-0.61.0.stream.jsonl"),
            fixture("gemini-0.61.0.json"),
        ),
        gemini_json(fixture("gemini-0.61.0.json")),
    ]
}

/// The same fixtures turned into error responses of the same shape.
fn error_responses() -> Vec<Response> {
    let gemini_error_json = serde_json::json!({
        "session_id": "00000000-0000-4000-8000-000000000001",
        "error": {"type": "FatalTurnLimitedError", "message": "quota exceeded", "code": 53},
    })
    .to_string();
    let gemini_error_stream = with_last_line(
        &fixture("gemini-0.61.0.stream.jsonl"),
        r#"{"type":"result","timestamp":"2026-09-25T08:30:02.000Z","status":"error","error":{"type":"FatalTurnLimitedError","message":"quota exceeded"},"stats":{"total_tokens":0,"input_tokens":0,"output_tokens":0,"cached":0,"input":0,"duration_ms":1500,"tool_calls":1}}"#,
    );
    vec![
        claude(|r| {
            r["is_error"] = true.into();
            r["subtype"] = "error_during_execution".into();
            r["result"] = "tool failed".into();
        }),
        claude(|r| {
            r["subtype"] = "error".into();
            r["result"] = "subtype error".into();
        }),
        codex("{\"type\":\"turn.failed\",\"error\":{\"message\":\"boom\"}}\n"),
        codex("{\"type\":\"error\",\"message\":\"stream disconnected\"}\n"),
        gemini_stream(gemini_error_stream, gemini_error_json.clone()),
        gemini_json(gemini_error_json),
    ]
}

// ---------------------------------------------------------------------------
// Running both handlers
// ---------------------------------------------------------------------------

const OLD_HELP: &str = "--output-format  [choices: \"text\", \"json\"]";
const NEW_HELP: &str = "--output-format  [choices: \"text\", \"json\", \"stream-json\"]";

/// A stub provider in its own directory (so the Gemini `--help` probe cache,
/// keyed by program path, never leaks between cases). It logs its argv,
/// prints `stdout` and [`STDERR`], and exits `code`.
fn stub(dir: &Path, stdout: &str, code: i32) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let out = dir.join("stdout");
    std::fs::write(&out, stdout).unwrap();
    let path = dir.join("provider-stub");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\n\
             if [ \"$1\" = --help ]; then cat '{help}'; exit 0; fi\n\
             echo \"$@\" > '{argv}'\n\
             cat '{out}'\n\
             echo '{STDERR}' >&2\n\
             exit {code}\n",
            help = dir.join("help").display(),
            argv = dir.join("argv").display(),
            out = out.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A node and graph: a task node, or an LLM-backed conditional whose
/// outgoing edges carry `labels`.
struct Stage {
    node: PipelineNode,
    resolved: ResolvedNode,
    graph: PipelineGraph,
}

fn stage(provider: LlmProvider, labels: Option<[&str; 2]>) -> Stage {
    let (shape, kind, dot) = match labels {
        None => (
            "box",
            ResolvedNodeKind::Task,
            "digraph G { step -> done }".to_owned(),
        ),
        Some([a, b]) => (
            "diamond",
            ResolvedNodeKind::Conditional { llm_backed: true },
            format!(
                "digraph G {{\n  step [shape=\"diamond\"]\n  step -> left [label=\"{a}\"]\n  step -> right [label=\"{b}\"]\n}}"
            ),
        ),
    };
    let node = make_node("step", shape, Some("do work"), HashMap::new());
    let resolved = ResolvedNode {
        node_id: node.id.clone(),
        kind,
        handler: crate::HandlerIdentity::Codergen,
        provider: Some(provider),
        invocation: Default::default(),
    };
    let graph = PipelineGraph::from_dot(attractor_dot::parse(&dot).unwrap()).unwrap();
    Stage {
        node,
        resolved,
        graph,
    }
}

/// Both Outcomes for one response: (streaming handler, pre-streaming handler).
struct Both {
    new: Result<Outcome>,
    old: Result<Outcome>,
    /// Transcript files the streaming handler wrote.
    transcripts: Vec<Vec<u8>>,
}

async fn run_both(
    response: &Response,
    stage: &Stage,
    code: i32,
    with_run_dir: bool,
) -> (Both, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let help = if response.gemini_stream {
        NEW_HELP
    } else {
        OLD_HELP
    };
    std::fs::write(dir.path().join("help"), help).unwrap();
    let program = stub(dir.path(), &response.streamed, code);
    let run_dir = dir.path().join("run");

    let new = CodergenHandler
        .execute_with_controls(
            &stage.node,
            &stage.resolved,
            &Context::default(),
            &stage.graph,
            CodergenExecutionControls {
                dry_run: false,
                workdir: None,
                claude: ClaudeCliConfig::default(),
                run_dir: with_run_dir.then(|| run_dir.clone()),
                program: Some(program),
                events: None,
            },
        )
        .await;

    if response.provider == LlmProvider::Gemini {
        let format = if response.gemini_stream {
            "stream-json"
        } else {
            "json"
        };
        let argv = std::fs::read_to_string(dir.path().join("argv")).unwrap();
        assert!(
            argv.starts_with(&format!("--output-format {format} ")),
            "{}: ran with {argv}",
            response.case
        );
    }

    let old = legacy::outcome(
        response.provider,
        ExitStatus::from_raw(code << 8),
        &response.legacy,
        // What `wait_with_output` captured from the stub's `echo`.
        &format!("{STDERR}\n"),
        &stage.node,
        &stage.resolved,
        &stage.graph,
    );

    let transcripts = match std::fs::read_dir(run_dir.join("transcripts")) {
        Ok(entries) => entries
            .map(|entry| std::fs::read(entry.unwrap().path()).unwrap())
            .collect(),
        Err(_) => Vec::new(),
    };
    (
        Both {
            new,
            old,
            transcripts,
        },
        dir,
    )
}

/// `Outcome` has no `PartialEq`, so compare every field the engine reads.
fn assert_same_outcome(new: &Outcome, old: &Outcome, case: &str) {
    assert_eq!(new.status, old.status, "{case}: status");
    assert_eq!(
        new.preferred_label, old.preferred_label,
        "{case}: preferred_label"
    );
    assert_eq!(
        new.context_updates.get("step.cost_usd"),
        old.context_updates.get("step.cost_usd"),
        "{case}: step.cost_usd"
    );
    assert_eq!(
        new.context_updates, old.context_updates,
        "{case}: context_updates"
    );
    assert_eq!(
        new.suggested_next_ids, old.suggested_next_ids,
        "{case}: suggested_next_ids"
    );
    assert_eq!(new.notes, old.notes, "{case}: notes");
    assert_eq!(
        new.failure_reason, old.failure_reason,
        "{case}: failure_reason"
    );
}

fn both_ok<'a>(both: &'a Both, case: &str) -> (&'a Outcome, &'a Outcome) {
    match (&both.new, &both.old) {
        (Ok(new), Ok(old)) => (new, old),
        (new, old) => panic!("{case}: expected two Outcomes, got new={new:?} old={old:?}"),
    }
}

// ---------------------------------------------------------------------------
// AC1: for each fixture, the Outcome equals the pre-streaming Outcome.
// ---------------------------------------------------------------------------

#[test]
fn gemini_fixtures_hold_the_same_response() {
    let json: serde_json::Value = serde_json::from_str(&fixture("gemini-0.61.0.json")).unwrap();
    let streamed: String = fixture("gemini-0.61.0.stream.jsonl")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|line| line["type"] == "message" && line["role"] == "assistant")
        .map(|line| line["content"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(json["response"].as_str(), Some(streamed.as_str()));
}

#[tokio::test]
async fn recorded_fixtures_give_pre_streaming_outcomes_for_task_nodes() {
    for response in recorded_responses() {
        let stage = stage(response.provider, None);
        let (both, _dir) = run_both(&response, &stage, 0, true).await;
        let (new, old) = both_ok(&both, response.case);
        assert_same_outcome(new, old, response.case);
        assert_eq!(new.status, StageStatus::Success, "{}", response.case);
        // The Transcript is the streamed stdout, byte for byte.
        assert_eq!(
            both.transcripts,
            vec![response.streamed.clone().into_bytes()],
            "{}: Transcript",
            response.case
        );
    }
}

/// Pins the reference itself, so the comparison cannot pass vacuously.
#[tokio::test]
async fn recorded_fixture_outcomes_have_the_expected_values() {
    let mut outcomes = HashMap::new();
    for response in recorded_responses() {
        let stage = stage(response.provider, None);
        let (both, _dir) = run_both(&response, &stage, 0, true).await;
        outcomes.insert(response.case, both.new.unwrap());
    }

    let claude = &outcomes["claude"];
    assert_eq!(claude.notes, "OK");
    assert_eq!(
        claude.context_updates.get("step.cost_usd"),
        Some(&serde_json::json!(0.03932))
    );
    assert_eq!(
        claude.context_updates.get("step.turns"),
        Some(&serde_json::json!(1))
    );
    assert_eq!(
        claude.context_updates.get("step.provider"),
        Some(&serde_json::json!("Claude Code"))
    );

    assert_eq!(outcomes["codex"].notes, "OK");
    for case in ["gemini stream-json", "gemini json fallback"] {
        assert_eq!(outcomes[case].notes, "Checked the file.\nOK", "{case}");
    }
    for case in ["codex", "gemini stream-json", "gemini json fallback"] {
        let updates = &outcomes[case].context_updates;
        assert!(!updates.contains_key("step.cost_usd"), "{case}: no cost");
        assert!(!updates.contains_key("step.turns"), "{case}: no turns");
    }
    for outcome in outcomes.values() {
        assert_eq!(outcome.preferred_label, None);
        assert_eq!(outcome.failure_reason, None);
        assert!(!outcome.context_updates.contains_key("step.label"));
        assert_eq!(
            outcome.context_updates.get("step.completed"),
            Some(&serde_json::json!(true))
        );
    }
}

#[tokio::test]
async fn recorded_fixtures_give_pre_streaming_labels_for_conditional_nodes() {
    for response in recorded_responses() {
        // A label the response ends with.
        let stage_ok = stage(response.provider, Some(["RETRY", "OK"]));
        let (both, _dir) = run_both(&response, &stage_ok, 0, true).await;
        let (new, old) = both_ok(&both, response.case);
        assert_same_outcome(new, old, response.case);
        assert_eq!(
            new.preferred_label.as_deref(),
            Some("OK"),
            "{}",
            response.case
        );
        assert_eq!(
            new.context_updates.get("step.label"),
            Some(&serde_json::json!("OK")),
            "{}",
            response.case
        );

        // No label matches.
        let stage_none = stage(response.provider, Some(["PASS", "FAIL"]));
        let (both, _dir) = run_both(&response, &stage_none, 0, true).await;
        let (new, old) = both_ok(&both, response.case);
        assert_same_outcome(new, old, response.case);
        assert_eq!(new.preferred_label, None, "{}", response.case);
        assert!(!new.context_updates.contains_key("step.label"));
    }
}

#[tokio::test]
async fn recorded_fixtures_without_run_folder_give_same_outcome() {
    for response in recorded_responses() {
        let stage = stage(response.provider, None);
        let (both, _dir) = run_both(&response, &stage, 0, false).await;
        let (new, old) = both_ok(&both, response.case);
        assert_same_outcome(new, old, response.case);
        assert!(both.transcripts.is_empty(), "{}", response.case);
    }
}

#[tokio::test]
async fn error_responses_give_pre_streaming_fail_outcomes() {
    for response in error_responses() {
        for labels in [None, Some(["RETRY", "OK"])] {
            let stage = stage(response.provider, labels);
            let (both, _dir) = run_both(&response, &stage, 0, true).await;
            let (new, old) = both_ok(&both, response.case);
            assert_same_outcome(new, old, response.case);
            assert_eq!(new.status, StageStatus::Fail, "{}", response.case);
            assert_eq!(
                new.failure_reason,
                Some(format!(
                    "{} returned an error",
                    response.provider.display_name()
                )),
                "{}",
                response.case
            );
        }
    }
}

#[tokio::test]
async fn nonzero_exit_with_final_result_gives_pre_streaming_outcome() {
    for response in recorded_responses().into_iter().chain(error_responses()) {
        let stage = stage(response.provider, None);
        let (both, _dir) = run_both(&response, &stage, 1, true).await;
        let (new, old) = both_ok(&both, response.case);
        assert_same_outcome(new, old, response.case);
    }
}

/// With no output there is no Outcome, before or after streaming, and the
/// error is the same.
#[tokio::test]
async fn empty_stdout_is_the_same_error_on_both_sides() {
    for provider in [LlmProvider::Claude, LlmProvider::Codex, LlmProvider::Gemini] {
        for gemini_stream in [false, true] {
            if gemini_stream && provider != LlmProvider::Gemini {
                continue;
            }
            let response = Response {
                case: provider.as_str(),
                provider,
                gemini_stream,
                streamed: String::new(),
                legacy: String::new(),
            };
            let stage = stage(provider, None);
            for code in [0, 1] {
                let (both, _dir) = run_both(&response, &stage, code, true).await;
                let case = format!("{} exit {code} stream={gemini_stream}", response.case);
                match (&both.new, &both.old) {
                    (Err(new), Err(old)) => {
                        assert_eq!(new.to_string(), old.to_string(), "{case}")
                    }
                    (new, old) => panic!("{case}: expected errors, got {new:?} / {old:?}"),
                }
            }
        }
    }
}
