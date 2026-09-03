use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use async_trait::async_trait;
use attractor_dot::AttributeValue;
use attractor_quality::{
    ClaudeCodergenConfig, ClaudeSettingSource, ClaudeSettingsMode, ResolutionError,
};
use attractor_types::{AttractorError, Context, Outcome, Result, StageStatus};

use crate::execution_plan::{HandlerIdentity, LlmProvider, ResolvedNode, ResolvedNodeKind};
use crate::graph::{PipelineGraph, PipelineNode};
use crate::handler::{HandlerExecutionContext, NodeHandler, ProviderNodeHandler};

use super::process_group::{self, ProcessGroupGuard};

#[path = "codergen_provider.rs"]
mod provider;
use provider::{build_cli_command, parse_cli_output, ClaudeCliConfig, CliRunConfig};
#[cfg(test)]
use provider::{parse_claude_output, parse_codex_output, parse_gemini_output, LlmCliProvider};

// ---------------------------------------------------------------------------
// CodergenHandler — LLM task handler (box shape)
//
// Shells out to a CLI tool (Claude Code, Codex CLI, or Gemini CLI) for each
// node, passing the node's prompt. The provider is supplied by the canonical
// ExecutionPlan after strict validation.
//
// Supported node attributes:
//   - prompt: Optional task prompt sent to the CLI
//   - llm_provider: "claude", "codex", or "gemini" (required)
//   - llm_model: Override the model (e.g. "sonnet", "o3", "gemini-2.5-pro")
//   - allowed_tools: Comma-separated tool list (Claude only)
//   - max_budget_usd: Spending cap for this node (Claude only)
//   - timeout: Duration before the CLI invocation is killed (default: 10m)
//
// The pipeline context key "workdir" controls the working directory.
// ---------------------------------------------------------------------------

pub struct CodergenHandler;

struct CodergenExecutionControls {
    dry_run: bool,
    workdir: Option<String>,
    claude: ClaudeCliConfig,
}

#[async_trait]
impl NodeHandler for CodergenHandler {
    fn handler_type(&self) -> &str {
        "codergen"
    }

    fn provider_handler(&self) -> Option<&dyn ProviderNodeHandler> {
        Some(self)
    }

    async fn execute(
        &self,
        node: &PipelineNode,
        context: &Context,
        graph: &PipelineGraph,
    ) -> Result<Outcome> {
        // Compatibility path for consumers that still dispatch raw nodes (the
        // web executor). Preserve their historical fallback behavior until
        // they are explicitly migrated to ExecutionPlan.
        let provider = node
            .llm_provider
            .as_deref()
            .and_then(LlmProvider::parse)
            .unwrap_or(LlmProvider::Claude);
        let resolved = ResolvedNode {
            node_id: node.id.clone(),
            kind: if node.shape == "diamond" || node.node_type.as_deref() == Some("conditional") {
                ResolvedNodeKind::Conditional { llm_backed: true }
            } else {
                ResolvedNodeKind::Task
            },
            handler: HandlerIdentity::Codergen,
            provider: Some(provider),
            invocation: Default::default(),
        };
        ProviderNodeHandler::execute_resolved(self, node, &resolved, context, graph).await
    }
}

impl CodergenHandler {
    async fn execute_with_controls(
        &self,
        node: &PipelineNode,
        resolved: &ResolvedNode,
        context: &Context,
        graph: &PipelineGraph,
        controls: CodergenExecutionControls,
    ) -> Result<Outcome> {
        let prompt = node.prompt.as_deref().unwrap_or("No prompt specified");
        let label = node.label.clone();
        let provider = resolved
            .provider
            .ok_or_else(|| AttractorError::HandlerError {
                handler: "codergen".into(),
                node: node.id.clone(),
                message: "compiled codergen node has no provider".into(),
            })?;

        tracing::info!(
            node = %node.id,
            label = %label,
            provider = provider.display_name(),
            "Executing codergen handler"
        );

        if controls.dry_run {
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
        if matches!(
            resolved.kind,
            ResolvedNodeKind::Conditional { llm_backed: true }
        ) {
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

        // Build the CLI command via the provider-specific builder
        let mut cmd = build_cli_command(&CliRunConfig {
            provider,
            prompt: &full_prompt,
            model,
            workdir: controls.workdir.as_deref(),
            node,
            graph,
            claude: controls.claude,
        });
        cmd.kill_on_drop(true);
        process_group::configure(&mut cmd);

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
        // The guard owns process-tree cleanup even if the executor's outer
        // deadline drops this handler future before its local timeout fires.
        let mut process_group = ProcessGroupGuard::new(child.id());
        let timeout_dur = node.timeout.unwrap_or(std::time::Duration::from_secs(600));
        let output = match tokio::time::timeout(timeout_dur, child.wait_with_output()).await {
            Ok(Ok(output)) => {
                process_group.disarm();
                output
            }
            Ok(Err(error)) => {
                return Err(AttractorError::HandlerError {
                    handler: "codergen".into(),
                    node: node.id.clone(),
                    message: format!("{} execution failed: {error}", provider.display_name()),
                });
            }
            Err(_elapsed) => {
                tracing::warn!(
                    node = %node.id,
                    timeout_secs = timeout_dur.as_secs(),
                    "Killing timed-out {} process group",
                    provider.display_name()
                );
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
}

#[async_trait]
impl ProviderNodeHandler for CodergenHandler {
    async fn execute_resolved(
        &self,
        node: &PipelineNode,
        resolved: &ResolvedNode,
        context: &Context,
        graph: &PipelineGraph,
    ) -> Result<Outcome> {
        let snapshot = context.snapshot().await;
        let dry_run = snapshot
            .get("dry_run")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let workdir = snapshot
            .get("workdir")
            .and_then(|value| value.as_str())
            .map(str::to_owned);
        let claude = resolve_claude_cli_config(&snapshot, workdir.as_deref(), &node.id)?;
        self.execute_with_controls(
            node,
            resolved,
            context,
            graph,
            CodergenExecutionControls {
                dry_run,
                workdir,
                claude,
            },
        )
        .await
    }

    async fn execute_configured(
        &self,
        node: &PipelineNode,
        resolved: &ResolvedNode,
        execution: HandlerExecutionContext<'_>,
        graph: &PipelineGraph,
    ) -> Result<Outcome> {
        let config = execution.config();
        let claude = ClaudeCliConfig {
            settings_mode: *config.claude().settings_mode().value(),
            setting_sources: config
                .claude()
                .setting_sources()
                .value()
                .iter()
                .map(|source| source.as_str().to_owned())
                .collect(),
            settings: config.claude().settings().value().clone(),
            tools: config.claude().tools().value().clone(),
            agents: config.claude().agents().value().clone(),
            plugin_dirs: config
                .claude()
                .plugin_dirs()
                .value()
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
            mcp_config: config.claude().mcp_config().value().clone(),
        };
        self.execute_with_controls(
            node,
            resolved,
            execution.workflow(),
            graph,
            CodergenExecutionControls {
                dry_run: *config.dry_run().value(),
                workdir: Some(config.workdir().value().to_string_lossy().into_owned()),
                claude,
            },
        )
        .await
    }
}

const CLAUDE_MODE_KEY: &str = "codergen.claude.settings_mode";
const CLAUDE_SOURCES_KEY: &str = "codergen.claude.setting_sources";
const CLAUDE_SETTINGS_KEY: &str = "codergen.claude.settings";
const CLAUDE_TOOLS_KEY: &str = "codergen.claude.tools";
const CLAUDE_AGENTS_KEY: &str = "codergen.claude.agents";
const CLAUDE_PLUGIN_DIRS_KEY: &str = "codergen.claude.plugin_dirs";
const CLAUDE_MCP_CONFIG_KEY: &str = "codergen.claude.mcp_config";

fn resolve_claude_cli_config(
    snapshot: &HashMap<String, serde_json::Value>,
    workdir: Option<&str>,
    node_id: &str,
) -> Result<ClaudeCliConfig> {
    let mut cfg = ClaudeCliConfig::default();

    if let Some(dir) = workdir {
        match attractor_quality::resolve(Path::new(dir)) {
            Ok(resolved) => {
                if let Some(claude) = resolved.manifest.codergen.and_then(|c| c.claude) {
                    apply_manifest_claude_config(&mut cfg, claude, resolved.path.parent());
                }
            }
            Err(ResolutionError::NotFound) => {}
            Err(err) => {
                return Err(AttractorError::HandlerError {
                    handler: "codergen".into(),
                    node: node_id.into(),
                    message: format!(
                        "Failed to resolve pas.toml for Claude codergen config: {err}"
                    ),
                });
            }
        }
    }

    apply_context_claude_overrides(&mut cfg, snapshot, node_id)?;

    if cfg.settings_mode == ClaudeSettingsMode::Inherit && cfg.setting_sources.is_empty() {
        return Err(AttractorError::HandlerError {
            handler: "codergen".into(),
            node: node_id.into(),
            message: "Claude settings_mode=inherit requires explicit setting_sources".into(),
        });
    }
    if cfg.settings_mode == ClaudeSettingsMode::Inherit {
        tracing::warn!(
            node = %node_id,
            setting_sources = %cfg.setting_sources.join(","),
            "Claude codergen inherit mode may load personal hooks/settings"
        );
    }

    Ok(cfg)
}

fn apply_manifest_claude_config(
    cfg: &mut ClaudeCliConfig,
    claude: ClaudeCodergenConfig,
    manifest_dir: Option<&Path>,
) {
    if let Some(mode) = claude.settings_mode {
        cfg.settings_mode = mode;
    }
    if let Some(sources) = claude.setting_sources {
        cfg.setting_sources = sources
            .into_iter()
            .map(|source| source.as_str().to_string())
            .collect();
    }
    cfg.settings = claude.settings_json.or(cfg.settings.take());
    cfg.tools = claude.tools.or(cfg.tools.take());
    cfg.agents = claude.agents_json.or(cfg.agents.take());
    cfg.mcp_config = claude.mcp_config_json.or(cfg.mcp_config.take());
    if !claude.plugin_dirs.is_empty() {
        cfg.plugin_dirs = claude
            .plugin_dirs
            .into_iter()
            .map(|path| resolve_manifest_relative_path(path, manifest_dir))
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
    }
}

fn resolve_manifest_relative_path(path: PathBuf, manifest_dir: Option<&Path>) -> PathBuf {
    if path.is_absolute() {
        path
    } else if let Some(dir) = manifest_dir {
        dir.join(path)
    } else {
        path
    }
}

fn apply_context_claude_overrides(
    cfg: &mut ClaudeCliConfig,
    snapshot: &HashMap<String, serde_json::Value>,
    node_id: &str,
) -> Result<()> {
    if let Some(mode) = snapshot.get(CLAUDE_MODE_KEY).and_then(|v| v.as_str()) {
        cfg.settings_mode =
            ClaudeSettingsMode::from_str(mode).map_err(|err| AttractorError::HandlerError {
                handler: "codergen".into(),
                node: node_id.into(),
                message: err,
            })?;
    }
    if let Some(sources) = string_list_from_value(snapshot.get(CLAUDE_SOURCES_KEY)) {
        cfg.setting_sources = parse_setting_sources(sources, node_id)?;
    }
    if let Some(value) = snapshot.get(CLAUDE_SETTINGS_KEY).and_then(|v| v.as_str()) {
        cfg.settings = Some(value.to_string());
    }
    if let Some(value) = snapshot.get(CLAUDE_TOOLS_KEY).and_then(|v| v.as_str()) {
        cfg.tools = Some(value.to_string());
    }
    if let Some(value) = snapshot.get(CLAUDE_AGENTS_KEY).and_then(|v| v.as_str()) {
        cfg.agents = Some(value.to_string());
    }
    if let Some(plugin_dirs) = string_list_from_value(snapshot.get(CLAUDE_PLUGIN_DIRS_KEY)) {
        cfg.plugin_dirs = plugin_dirs;
    }
    if let Some(value) = snapshot.get(CLAUDE_MCP_CONFIG_KEY).and_then(|v| v.as_str()) {
        cfg.mcp_config = Some(value.to_string());
    }

    Ok(())
}

fn parse_setting_sources(sources: Vec<String>, node_id: &str) -> Result<Vec<String>> {
    sources
        .into_iter()
        .map(|source| {
            ClaudeSettingSource::from_str(&source)
                .map(|parsed| parsed.as_str().to_string())
                .map_err(|err| AttractorError::HandlerError {
                    handler: "codergen".into(),
                    node: node_id.into(),
                    message: err,
                })
        })
        .collect()
}

fn string_list_from_value(value: Option<&serde_json::Value>) -> Option<Vec<String>> {
    match value {
        Some(serde_json::Value::String(s)) => Some(
            s.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned)
                .collect(),
        ),
        Some(serde_json::Value::Array(values)) => Some(
            values
                .iter()
                .filter_map(|value| value.as_str().map(ToOwned::to_owned))
                .collect(),
        ),
        _ => None,
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

#[cfg(test)]
#[path = "codergen_handler_tests.rs"]
mod tests;
