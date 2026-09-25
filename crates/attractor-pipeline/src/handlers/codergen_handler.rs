use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Instant;

use async_trait::async_trait;
use attractor_dot::AttributeValue;
use attractor_quality::{
    ClaudeCodergenConfig, ClaudeSettingSource, ClaudeSettingsMode, ResolutionError,
};
use attractor_types::{AttractorError, Context, Outcome, Result, StageStatus};

use crate::events::PipelineEvent;
use crate::execution_plan::{HandlerIdentity, LlmProvider, ResolvedNode, ResolvedNodeKind};
use crate::graph::{PipelineGraph, PipelineNode};
use crate::handler::{EventSink, HandlerExecutionContext, NodeHandler, ProviderNodeHandler};

use super::process_group::{self, ProcessGroupGuard};
use super::provider_stream::{run_streaming, Transcript};

#[path = "codergen_provider.rs"]
mod provider;
#[cfg(test)]
use provider::{
    build_cli_command, claude_result_line, parse_claude_output, parse_codex_output,
    parse_gemini_output, parse_gemini_stream_output, LlmCliProvider,
};
use provider::{
    build_cli_command_with_program, gemini_output_format, has_final_result, parse_cli_output,
    summarize_stream, ClaudeCliConfig, CliRunConfig, GeminiOutputFormat, InvocationUsage,
};

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
//
// When the executor has a Run folder, each provider process (Model Invocation)
// gets a new Invocation ID and its raw stdout is streamed, line by line, to
// `transcripts/<invocation-id>.jsonl` while it runs. When the engine also
// passes its Event path, each Model Invocation emits exactly one `LlmInvoked`
// once the provider exits, fails, or times out.
// ---------------------------------------------------------------------------

pub struct CodergenHandler;

struct CodergenExecutionControls<'a> {
    dry_run: bool,
    workdir: Option<String>,
    claude: ClaudeCliConfig,
    /// Run folder that receives Transcripts; `None` writes no Transcript.
    run_dir: Option<PathBuf>,
    /// Executable to start instead of the provider's binary (test stubs).
    program: Option<PathBuf>,
    /// Receives `LlmInvoked`; `None` emits nothing.
    events: Option<&'a dyn EventSink>,
}

/// `LlmInvoked.status` values (spec C3).
const INVOKED_SUCCESS: &str = "success";
const INVOKED_FAILED: &str = "failed";
const INVOKED_TIMEOUT: &str = "timeout";

/// The one `LlmInvoked` Event of a Model Invocation, armed once the provider
/// process has started. [`Self::finish`] emits it; if the handler future is
/// dropped first (its own timeout, or the engine's outer deadline) `Drop`
/// emits it with status `timeout`, so either timer yields exactly one Event.
struct LlmInvocation<'a> {
    events: &'a dyn EventSink,
    run_dir: PathBuf,
    invocation_id: String,
    node_id: String,
    provider: LlmProvider,
    model_requested: Option<String>,
    started: Instant,
    emitted: bool,
}

impl LlmInvocation<'_> {
    fn finish(mut self, status: &str, usage: InvocationUsage) {
        self.emit(status, usage);
    }

    /// Usage read back from the Transcript, which holds the provider's stdout
    /// so far. Missing or unreadable → all `None`.
    fn transcript_usage(&self) -> InvocationUsage {
        let path =
            attractor_journal::RunDir::from_path(&self.run_dir).transcript(&self.invocation_id);
        std::fs::read(path)
            .map(|bytes| summarize_stream(self.provider, &String::from_utf8_lossy(&bytes)))
            .unwrap_or_default()
    }

    fn emit(&mut self, status: &str, usage: InvocationUsage) {
        if std::mem::replace(&mut self.emitted, true) {
            return;
        }
        self.events.emit(PipelineEvent::LlmInvoked {
            invocation_id: self.invocation_id.clone(),
            node_id: self.node_id.clone(),
            provider: self.provider.as_str().to_owned(),
            model_requested: self.model_requested.clone(),
            model_actual: usage.model_actual,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cost_usd: usage.cost_usd,
            duration_ms: u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            transcript: attractor_journal::transcript_rel_path(&self.invocation_id),
            status: status.to_owned(),
        });
    }
}

impl Drop for LlmInvocation<'_> {
    fn drop(&mut self) {
        if !self.emitted {
            let usage = self.transcript_usage();
            self.emit(INVOKED_TIMEOUT, usage);
        }
    }
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
        controls: CodergenExecutionControls<'_>,
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
        let program = controls
            .program
            .clone()
            .unwrap_or_else(|| PathBuf::from(provider.binary_name()));
        let gemini_format = if provider == LlmProvider::Gemini {
            gemini_output_format(&program).await
        } else {
            GeminiOutputFormat::Json
        };
        let mut cmd = build_cli_command_with_program(
            &CliRunConfig {
                provider,
                prompt: &full_prompt,
                model,
                workdir: controls.workdir.as_deref(),
                node,
                graph,
                claude: controls.claude,
            },
            program.as_os_str(),
            gemini_format,
        );
        cmd.kill_on_drop(true);
        process_group::configure(&mut cmd);

        // One Model Invocation per spawn. The Transcript exists before any
        // output arrives, so a silent provider still leaves an empty file.
        let invocation_id = attractor_journal::new_invocation_id();
        let transcript = match &controls.run_dir {
            Some(run_dir) => {
                let path = attractor_journal::RunDir::from_path(run_dir).transcript(&invocation_id);
                Transcript::create(path).await
            }
            None => None,
        };

        // Spawn the CLI process — detect missing binary
        let child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                // No Model Invocation happened, so no Transcript either.
                if let Some(transcript) = transcript {
                    transcript.discard().await;
                }
                return Err(if e.kind() == std::io::ErrorKind::NotFound {
                    AttractorError::CliNotFound {
                        binary: provider.binary_name().to_string(),
                    }
                } else {
                    AttractorError::HandlerError {
                        handler: "codergen".into(),
                        node: node.id.clone(),
                        message: format!("Failed to spawn {}: {}", provider.display_name(), e),
                    }
                });
            }
        };
        tracing::debug!(
            node = %node.id,
            invocation_id = %invocation_id,
            transcript = ?transcript.as_ref().map(Transcript::path),
            "Started {}",
            provider.display_name()
        );
        // `LlmInvoked` needs a Run folder: its `transcript` is relative to it.
        let invocation = match (controls.events, &controls.run_dir) {
            (Some(events), Some(run_dir)) => Some(LlmInvocation {
                events,
                run_dir: run_dir.clone(),
                invocation_id: invocation_id.clone(),
                node_id: node.id.clone(),
                provider,
                model_requested: model.map(str::to_owned),
                started: Instant::now(),
                emitted: false,
            }),
            _ => None,
        };
        let finish =
            |invocation: Option<LlmInvocation<'_>>, status: &str, usage: InvocationUsage| {
                if let Some(invocation) = invocation {
                    invocation.finish(status, usage);
                }
            };

        // Apply timeout (default 10 minutes, configurable via node.timeout).
        // The guard owns process-tree cleanup even if the executor's outer
        // deadline drops this handler future before its local timeout fires.
        // Every stdout line is flushed to the Transcript as it arrives, so a
        // timeout or cancellation keeps the partial Transcript.
        let mut process_group = ProcessGroupGuard::new(child.id());
        let timeout_dur = node.timeout.unwrap_or(std::time::Duration::from_secs(600));
        let output = match tokio::time::timeout(timeout_dur, run_streaming(child, transcript)).await
        {
            Ok(Ok(output)) => {
                process_group.disarm();
                output
            }
            Ok(Err(error)) => {
                if let Some(invocation) = invocation {
                    let usage = invocation.transcript_usage();
                    invocation.finish(INVOKED_FAILED, usage);
                }
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
                // Dropping `invocation` emits `LlmInvoked` with status `timeout`.
                drop(invocation);
                return Err(AttractorError::CommandTimeout {
                    timeout_ms: timeout_dur.as_millis() as u64,
                });
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        // A stream that ends without its final `result` line carries no
        // answer; report the exit like an empty stdout, as before streaming.
        if !output.status.success() && !has_final_result(provider, &stdout) {
            finish(
                invocation,
                INVOKED_FAILED,
                summarize_stream(provider, &stdout),
            );
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
        let cli_result = match parse_cli_output(provider, &stdout, &stderr, &node.id) {
            Ok(cli_result) => cli_result,
            Err(error) => {
                finish(
                    invocation,
                    INVOKED_FAILED,
                    summarize_stream(provider, &stdout),
                );
                return Err(error);
            }
        };
        finish(
            invocation,
            if cli_result.is_error {
                INVOKED_FAILED
            } else {
                INVOKED_SUCCESS
            },
            cli_result.usage.clone(),
        );

        tracing::info!(
            node = %node.id,
            provider = provider.display_name(),
            is_error = cli_result.is_error,
            has_cost = cli_result.cost_usd.is_some(),
            model_actual = cli_result.usage.model_actual.as_deref(),
            input_tokens = cli_result.usage.input_tokens,
            output_tokens = cli_result.usage.output_tokens,
            usage_cost_usd = cli_result.usage.cost_usd,
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
                run_dir: None,
                program: None,
                events: None,
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
        self.execute_configured_with_program(node, resolved, execution, graph, None)
            .await
    }
}

impl CodergenHandler {
    /// `execute_configured`, starting `program` instead of the provider's
    /// binary when given (engine-level tests with stub providers).
    pub(crate) async fn execute_configured_with_program(
        &self,
        node: &PipelineNode,
        resolved: &ResolvedNode,
        execution: HandlerExecutionContext<'_>,
        graph: &PipelineGraph,
        program: Option<PathBuf>,
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
                run_dir: execution.run_dir().map(Path::to_path_buf),
                program,
                events: execution.events(),
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

#[cfg(test)]
#[path = "codergen_regression_tests.rs"]
mod regression_tests;
