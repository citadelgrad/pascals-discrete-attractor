use std::collections::HashMap;

use async_trait::async_trait;
use attractor_dot::AttributeValue;
use attractor_types::{AttractorError, Context, Outcome, Result, StageStatus};

use crate::graph::{PipelineGraph, PipelineNode};
use crate::handler::NodeHandler;

#[path = "codergen_provider.rs"]
mod provider;
use provider::{build_cli_command, parse_cli_output, CliRunConfig, LlmCliProvider};
#[cfg(test)]
use provider::{parse_claude_output, parse_codex_output, parse_gemini_output};

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

        // Build the full prompt with pipeline context (deterministic ordering
        // so the same inputs always yield the same bytes — required for caching
        // and beneficial for the CLI's own prompt cache).
        let snapshot = context.snapshot().await;
        let full_prompt = assemble_prompt(node, graph, &snapshot);

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

        // Cross-run response cache. Only cacheable nodes (routing/conditional
        // nodes, or nodes the author marked `cache="ro"`) participate, and only
        // when the run enabled caching. On a hit we skip the CLI entirely.
        let cache = cache_from_context(context).await;
        let cache_key = if cache.mode().is_enabled() && is_cacheable_node(node, graph) {
            Some(build_cache_key(
                provider,
                model,
                node,
                &effective_workdir(workdir.as_deref()),
                &full_prompt,
            ))
        } else {
            None
        };

        if let Some(ref key) = cache_key {
            if let Some(entry) = cache.get(key) {
                tracing::info!(
                    node = %node.id,
                    provider = provider.display_name(),
                    cache = "hit",
                    saved_usd = entry.cost_usd.unwrap_or(0.0),
                    "Cache hit — skipping {} invocation",
                    provider.display_name()
                );
                return Ok(cached_outcome(node, provider, &entry));
            }
            tracing::debug!(node = %node.id, cache = "miss", "No cache entry — invoking CLI");
        }

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
                    // SIGKILL the child process — its MCP server children will
                    // get SIGHUP when their parent exits.
                    #[cfg(unix)]
                    unsafe {
                        libc::kill(pid as i32, libc::SIGKILL);
                    }
                }
                return Err(AttractorError::CommandTimeout {
                    timeout_ms: timeout_dur.as_millis() as u64,
                });
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !output.status.success() && stdout.is_empty() {
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

        // Persist successful results to the cross-run cache. Errors are never
        // cached (they could pin transient rate-limit/timeout failures).
        if let Some(ref key) = cache_key {
            if !cli_result.is_error {
                let entry = attractor_cache::CacheEntry::new(
                    provider.display_name(),
                    cli_result.text.clone(),
                    cli_result.cost_usd,
                    cli_result.turns,
                    preferred_label.clone(),
                );
                if let Err(e) = cache.put(key, &entry) {
                    tracing::warn!(node = %node.id, error = %e, "Failed to write cache entry");
                }
            }
        }

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
    // Fallback: search full response for label as a standalone word
    let upper = response.to_uppercase();
    for label in labels {
        if upper.contains(&label.to_uppercase()) {
            return Some(label.clone());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Prompt assembly + response caching
// ---------------------------------------------------------------------------

/// Assemble the full prompt sent to the CLI: pipeline goal, prior-node context,
/// the task itself, and (for conditional nodes) the routing-label instruction.
///
/// Prior-context keys are sorted so the assembled bytes are deterministic for a
/// given input state — this is what makes the response cache safe to key on, and
/// it also stabilises the CLI's own prompt cache.
fn assemble_prompt(
    node: &PipelineNode,
    graph: &PipelineGraph,
    snapshot: &HashMap<String, serde_json::Value>,
) -> String {
    let prompt = node.prompt.as_deref().unwrap_or("No prompt specified");
    let mut full_prompt = String::new();

    if !graph.goal.is_empty() {
        full_prompt.push_str(&format!("Pipeline goal: {}\n\n", graph.goal));
    }

    let mut context_keys: Vec<_> = snapshot
        .iter()
        .filter(|(k, _)| k.ends_with(".result") || k.ends_with(".output"))
        .collect();
    context_keys.sort_by(|a, b| a.0.cmp(b.0));
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

    full_prompt.push_str(&format!("Task ({}): {}", node.label, prompt));

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

    full_prompt
}

/// Whether a node's codergen result may be cached.
///
/// Opt-out via `cache="off"`; explicit opt-in via `cache="ro"` (the author
/// asserts the node's output is a deterministic function of its prompt with no
/// un-replayed filesystem side effects — honoured even inside a loop).
///
/// Conditional/routing nodes are cacheable by default because their output is a
/// single label consumed by edge selection, with nothing to replay — **except**
/// when the node sits in a directed cycle. A cached routing label would pin a
/// retry/fix loop forever (a live, non-deterministic run eventually routes out;
/// a cached one cannot), forcing a `max_steps` abort. Loop-resident routing must
/// re-evaluate live, so those nodes are not cached unless explicitly `cache="ro"`.
fn is_cacheable_node(node: &PipelineNode, graph: &PipelineGraph) -> bool {
    match node.raw_attrs.get("cache") {
        Some(AttributeValue::String(s)) if s.eq_ignore_ascii_case("off") => return false,
        Some(AttributeValue::String(s)) if s.eq_ignore_ascii_case("ro") => return true,
        Some(AttributeValue::Boolean(false)) => return false,
        _ => {}
    }
    let is_conditional =
        node.shape == "diamond" || node.node_type.as_deref() == Some("conditional");
    is_conditional && !graph.node_in_cycle(&node.id)
}

fn string_attr<'a>(node: &'a PipelineNode, key: &str) -> Option<&'a str> {
    match node.raw_attrs.get(key) {
        Some(AttributeValue::String(s)) => Some(s.as_str()),
        _ => None,
    }
}

/// Derive the cache key from every deterministic input that feeds the CLI call.
///
/// `workdir` must be the *effective* execution directory — the resolved current
/// directory when the node has no explicit workdir — so results from different
/// project trees can never collide (the CLI reads/writes files relative to it).
fn build_cache_key(
    provider: LlmCliProvider,
    model: Option<&str>,
    node: &PipelineNode,
    workdir: &str,
    full_prompt: &str,
) -> String {
    attractor_cache::CacheKey::new()
        .field("provider", provider.binary_name())
        .field_opt("model", model)
        .field_opt("allowed_tools", string_attr(node, "allowed_tools"))
        .field_opt("max_budget_usd", string_attr(node, "max_budget_usd"))
        .field("workdir", workdir)
        .field("prompt", full_prompt)
        .finish()
}

/// The directory the CLI will actually run in: the node's explicit workdir, or
/// the process current directory when unset (which the spawned CLI inherits).
fn effective_workdir(workdir: Option<&str>) -> String {
    workdir.map(str::to_string).unwrap_or_else(|| {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    })
}

/// Build a [`Cache`](attractor_cache::Cache) from run-level settings carried in
/// the pipeline context (`__cache_mode`, `__cache_dir`, `__cache_ttl_days`).
async fn cache_from_context(context: &Context) -> attractor_cache::Cache {
    let mode = context
        .get("__cache_mode")
        .await
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .map(|s| attractor_cache::CacheMode::parse(&s))
        .unwrap_or_default();
    let root = context
        .get("__cache_dir")
        .await
        .and_then(|v| v.as_str().map(std::path::PathBuf::from));
    let ttl = context
        .get("__cache_ttl_days")
        .await
        .and_then(|v| v.as_u64())
        // 0 days means "no expiry" (matches the unset default); saturate to avoid
        // an overflow panic on absurd values.
        .filter(|&d| d > 0)
        .map(|d| std::time::Duration::from_secs(d.saturating_mul(86_400)));
    attractor_cache::Cache::new(attractor_cache::CacheConfig::new(mode, root, ttl))
}

/// Synthesize the same `Outcome` a live call would produce, from a cache hit.
/// Cost is reported as 0 (the saved amount is surfaced under `.cache_saved_usd`).
fn cached_outcome(
    node: &PipelineNode,
    provider: LlmCliProvider,
    entry: &attractor_cache::CacheEntry,
) -> Outcome {
    let mut updates = HashMap::new();
    updates.insert(
        format!("{}.completed", node.id),
        serde_json::Value::Bool(true),
    );
    updates.insert(
        format!("{}.result", node.id),
        serde_json::Value::String(entry.result_text.clone()),
    );
    updates.insert(
        format!("{}.provider", node.id),
        serde_json::Value::String(provider.display_name().into()),
    );
    // A hit costs nothing; record the avoided spend for the run summary.
    updates.insert(format!("{}.cost_usd", node.id), serde_json::json!(0.0));
    updates.insert(
        format!("{}.cache_hit", node.id),
        serde_json::Value::Bool(true),
    );
    updates.insert(
        format!("{}.cache_saved_usd", node.id),
        serde_json::json!(entry.cost_usd.unwrap_or(0.0)),
    );
    if let Some(turns) = entry.turns {
        updates.insert(format!("{}.turns", node.id), serde_json::json!(turns));
    }
    if let Some(ref lbl) = entry.label {
        updates.insert(
            format!("{}.label", node.id),
            serde_json::Value::String(lbl.clone()),
        );
    }

    Outcome {
        status: StageStatus::Success,
        preferred_label: entry.label.clone(),
        suggested_next_ids: vec![],
        context_updates: updates,
        notes: entry.result_text.clone(),
        failure_reason: None,
    }
}

#[cfg(test)]
#[path = "codergen_handler_tests.rs"]
mod tests;
