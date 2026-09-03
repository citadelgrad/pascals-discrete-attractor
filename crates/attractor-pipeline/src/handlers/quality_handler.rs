use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use async_trait::async_trait;
use attractor_dot::AttributeValue;
use attractor_quality::telemetry::{self, StageEvent};
use attractor_types::{AttractorError, Context, Outcome, Result, StageStatus};

use crate::execution_plan::ResolvedNode;
use crate::graph::{PipelineGraph, PipelineNode};
use crate::handler::{HandlerExecutionContext, NodeHandler, ResolvedNodeHandler};

use super::process_group::{self, ProcessGroupGuard};

// Environment variables passed through to quality stage processes.
const ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "USER",
    "LOGNAME",
    "TMPDIR",
    "TEMP",
    "TMP",
    "TERM",
    "SHELL",
    "CARGO_HOME",
    "RUSTUP_HOME",
];

// Lines kept from head and tail of each stage's output for truncation.
const HEAD_LINES: usize = 50;
const TAIL_LINES: usize = 50;

pub struct QualityHandler;

struct StageSpec {
    name: String,
    argv: Vec<String>,
    timeout_secs: Option<u64>,
    allow_failure: bool,
}

#[async_trait]
impl NodeHandler for QualityHandler {
    fn handler_type(&self) -> &str {
        "quality"
    }

    fn resolved_handler(&self) -> Option<&dyn ResolvedNodeHandler> {
        Some(self)
    }

    async fn execute(
        &self,
        node: &PipelineNode,
        context: &Context,
        _graph: &PipelineGraph,
    ) -> Result<Outcome> {
        let quality_disabled = context
            .get("quality_disabled")
            .await
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let workdir: PathBuf = context
            .get("workdir")
            .await
            .and_then(|value| value.as_str().map(PathBuf::from))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let manifest = attractor_quality::resolve(&workdir).ok();
        self.execute_with_controls(node, quality_disabled, &workdir, manifest.as_ref())
            .await
    }
}

#[async_trait]
impl ResolvedNodeHandler for QualityHandler {
    async fn execute_resolved(
        &self,
        node: &PipelineNode,
        _resolved: &ResolvedNode,
        context: &Context,
        graph: &PipelineGraph,
    ) -> Result<Outcome> {
        self.execute(node, context, graph).await
    }

    async fn execute_configured(
        &self,
        node: &PipelineNode,
        _resolved: &ResolvedNode,
        execution: HandlerExecutionContext<'_>,
        _graph: &PipelineGraph,
    ) -> Result<Outcome> {
        self.execute_with_controls(
            node,
            *execution.config().quality_disabled().value(),
            execution.config().workdir().value(),
            execution.config().manifest(),
        )
        .await
    }
}

impl QualityHandler {
    async fn execute_with_controls(
        &self,
        node: &PipelineNode,
        quality_disabled: bool,
        workdir: &Path,
        resolved_manifest: Option<&attractor_quality::ResolvedManifest>,
    ) -> Result<Outcome> {
        let node_id = &node.id;

        // Check 1: enabled=false in node attrs → skip with success
        if let Some(AttributeValue::Boolean(false)) = node.raw_attrs.get("enabled") {
            return Ok(skip_outcome("Quality checks disabled via node attribute"));
        }

        // Check 2: quality_disabled=true in runtime context → skip with success
        if quality_disabled {
            return Ok(skip_outcome("Quality checks disabled via runtime flag"));
        }

        // Primary: manifest-driven stages; fallback: quality_checks attribute
        let stages: Vec<StageSpec> = match resolved_manifest {
            Some(resolved) => {
                if let Some(quality) = &resolved.manifest.quality {
                    if !quality.stages.is_empty() {
                        ensure_manifest_trusted(&resolved.path, &resolved.blake3_hash, node_id)?;
                        quality
                            .stages
                            .iter()
                            .map(|s| {
                                let hook = quality.hooks.get(s);
                                StageSpec {
                                    name: s.clone(),
                                    argv: resolve_argv(s, hook),
                                    timeout_secs: hook.and_then(|h| h.timeout_secs),
                                    allow_failure: hook
                                        .and_then(|h| h.allow_failure)
                                        .unwrap_or(false),
                                }
                            })
                            .collect()
                    } else {
                        stages_from_attr(node, node_id)?
                    }
                } else {
                    stages_from_attr(node, node_id)?
                }
            }
            None => stages_from_attr(node, node_id)?,
        };

        let default_timeout = node.timeout.unwrap_or(std::time::Duration::from_secs(600));

        let mut results: Vec<serde_json::Value> = Vec::new();
        let mut all_passed = true;
        let mut failure_summaries: Vec<String> = Vec::new();

        for stage in &stages {
            let stage_timeout = stage
                .timeout_secs
                .map(std::time::Duration::from_secs)
                .unwrap_or(default_timeout);

            let start = Instant::now();

            let (program, args) = match stage.argv.split_first() {
                Some(pair) => pair,
                None => {
                    return Err(AttractorError::HandlerError {
                        handler: "quality".into(),
                        node: node_id.clone(),
                        message: format!("stage '{}' has empty argv", stage.name),
                    })
                }
            };

            let mut cmd = tokio::process::Command::new(program);
            cmd.args(args);
            cmd.current_dir(workdir);
            cmd.env_clear();
            cmd.kill_on_drop(true);
            cmd.stdout(std::process::Stdio::piped());
            cmd.stderr(std::process::Stdio::piped());

            for key in ENV_ALLOWLIST {
                if let Ok(val) = std::env::var(key) {
                    cmd.env(key, val);
                }
            }

            process_group::configure(&mut cmd);

            let child = cmd.spawn().map_err(|e| AttractorError::HandlerError {
                handler: "quality".into(),
                node: node_id.clone(),
                message: format!("failed to spawn stage '{}': {e}", stage.name),
            })?;

            let mut process_group = ProcessGroupGuard::new(child.id());

            let output = match tokio::time::timeout(stage_timeout, child.wait_with_output()).await {
                Ok(Ok(o)) => {
                    process_group.disarm();
                    o
                }
                Ok(Err(e)) => {
                    return Err(AttractorError::HandlerError {
                        handler: "quality".into(),
                        node: node_id.clone(),
                        message: format!("stage '{}' I/O error: {e}", stage.name),
                    })
                }
                Err(_) => {
                    return Err(AttractorError::CommandTimeout {
                        timeout_ms: stage_timeout.as_millis() as u64,
                    });
                }
            };

            let duration_ms = start.elapsed().as_millis() as u64;
            let exit_code = output.status.code().unwrap_or(-1);
            let stage_ok = output.status.success();
            let passed = stage_ok || stage.allow_failure;

            // Cap stderr at 1 MB before UTF-8 conversion to bound memory usage.
            const MAX_STDERR_BYTES: usize = 1024 * 1024;
            let stderr_bytes = if output.stderr.len() > MAX_STDERR_BYTES {
                &output.stderr[..MAX_STDERR_BYTES]
            } else {
                &output.stderr
            };
            let stderr_raw = String::from_utf8_lossy(stderr_bytes);
            let stderr_clean = strip_ansi_escapes(&stderr_raw);
            let stderr_display = truncate_head_tail(&stderr_clean, HEAD_LINES, TAIL_LINES);

            // failure_footprint = blake3(stage_name || "|" || first_2KB(stderr_without_ansi))[..16]
            let failure_footprint = if !stage_ok {
                // Find a safe UTF-8 char boundary at or before 2048 bytes.
                let max = stderr_clean.len().min(2048);
                let safe_end = (0..=max)
                    .rev()
                    .find(|&i| stderr_clean.is_char_boundary(i))
                    .unwrap_or(0);
                let slice = &stderr_clean[..safe_end];
                let input = format!("{}|{}", stage.name, slice);
                let hash = blake3::hash(input.as_bytes());
                Some(hash.to_hex()[..16].to_string())
            } else {
                None
            };

            telemetry::record(
                node_id,
                &StageEvent {
                    stage: stage.name.clone(),
                    passed,
                    exit_code,
                    failure_footprint: failure_footprint.clone(),
                    duration_ms,
                },
            );

            results.push(serde_json::json!({
                "stage": stage.name,
                "exit_code": exit_code,
                "passed": passed,
                "stderr": stderr_display,
                "failure_footprint": failure_footprint,
                "duration_ms": duration_ms,
            }));

            if !passed {
                all_passed = false;
                if !stderr_display.is_empty() {
                    failure_summaries.push(stderr_display);
                }
                break; // fail-fast on first failure
            }
        }

        let mut context_updates: HashMap<String, serde_json::Value> = HashMap::new();
        context_updates.insert(
            format!("{node_id}.results"),
            serde_json::Value::Array(results),
        );
        context_updates.insert(
            format!("{node_id}.completed"),
            serde_json::Value::Bool(all_passed),
        );

        if !all_passed {
            let summary = failure_summaries.join("\n");
            context_updates.insert(
                format!("{node_id}.failure_summary"),
                serde_json::Value::String(summary),
            );
            Ok(Outcome {
                status: StageStatus::Fail,
                preferred_label: None,
                suggested_next_ids: vec![],
                context_updates,
                notes: String::new(),
                failure_reason: Some("one or more quality stages failed".into()),
            })
        } else {
            Ok(Outcome {
                status: StageStatus::Success,
                preferred_label: None,
                suggested_next_ids: vec![],
                context_updates,
                notes: String::new(),
                failure_reason: None,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn skip_outcome(notes: &str) -> Outcome {
    Outcome {
        status: StageStatus::Success,
        preferred_label: None,
        suggested_next_ids: vec![],
        context_updates: HashMap::new(),
        notes: notes.into(),
        failure_reason: None,
    }
}

fn ensure_manifest_trusted(path: &Path, blake3_hash: &str, node_id: &str) -> Result<()> {
    if attractor_quality::is_trusted(path, blake3_hash) {
        return Ok(());
    }

    match attractor_quality::prompt_and_add(path, blake3_hash) {
        Ok(true) => Ok(()),
        Ok(false) => Err(AttractorError::HandlerError {
            handler: "quality".into(),
            node: node_id.to_string(),
            message: format!(
                "pas.toml at {} is not trusted; run `pas trust add {} {}` or set PAS_TRUST_THIS=1",
                path.display(),
                path.display(),
                blake3_hash
            ),
        }),
        Err(e) => Err(AttractorError::HandlerError {
            handler: "quality".into(),
            node: node_id.to_string(),
            message: format!("failed to check pas.toml trust: {e}"),
        }),
    }
}

fn strip_ansi_escapes(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            out.push(ch);
            continue;
        }

        match chars.peek().copied() {
            Some('[') => {
                chars.next();
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            _ => out.push(ch),
        }
    }

    out
}

/// Resolve argv for a manifest stage: prefer cmd_argv, then shlex-split cmd,
/// then fall back to `sh -c <stage_name>`.
fn resolve_argv(stage: &str, hook: Option<&attractor_quality::HookConfig>) -> Vec<String> {
    if let Some(h) = hook {
        if let Some(argv) = &h.cmd_argv {
            if !argv.is_empty() {
                return argv.clone();
            }
        }
        if let Some(cmd) = &h.cmd {
            if let Some(parts) = shlex::split(cmd) {
                if !parts.is_empty() {
                    return parts;
                }
            }
            return vec!["sh".into(), "-c".into(), cmd.clone()];
        }
    }
    // No hook config: treat stage name as a shell command
    vec!["sh".into(), "-c".into(), stage.to_string()]
}

/// Build stages from the pipe-separated `quality_checks` node attribute (legacy fallback).
fn stages_from_attr(node: &PipelineNode, node_id: &str) -> Result<Vec<StageSpec>> {
    let checks_str = match node.raw_attrs.get("quality_checks") {
        Some(AttributeValue::String(s)) => s.clone(),
        _ => {
            return Err(AttractorError::HandlerError {
                handler: "quality".into(),
                node: node_id.to_string(),
                message: "missing required attribute 'quality_checks'".into(),
            })
        }
    };

    Ok(checks_str
        .split('|')
        .map(|cmd| {
            let cmd = cmd.trim().to_string();
            StageSpec {
                name: cmd.clone(),
                argv: vec!["sh".into(), "-c".into(), cmd],
                timeout_secs: None,
                allow_failure: false,
            }
        })
        .collect())
}

/// Keep at most `head` lines from the start and `tail` lines from the end.
/// Returns the full text if it fits within head+tail lines.
pub(crate) fn truncate_head_tail(text: &str, head: usize, tail: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    if total <= head + tail {
        return text.trim_end().to_string();
    }
    let omitted = total - head - tail;
    let mut buf = lines[..head].join("\n");
    buf.push_str(&format!("\n... ({omitted} lines omitted) ...\n"));
    buf.push_str(&lines[total - tail..].join("\n"));
    buf
}
