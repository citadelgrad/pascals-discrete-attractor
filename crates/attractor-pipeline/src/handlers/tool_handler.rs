use std::collections::HashMap;

use async_trait::async_trait;
use attractor_dot::AttributeValue;
use attractor_types::{AttractorError, Context, Outcome, Result, StageStatus};

use crate::graph::{PipelineGraph, PipelineNode};
use crate::handler::NodeHandler;

// ---------------------------------------------------------------------------
// ToolHandler — executes a shell command (parallelogram shape)
// ---------------------------------------------------------------------------

pub struct ToolHandler;

#[async_trait]
impl NodeHandler for ToolHandler {
    fn handler_type(&self) -> &str {
        "tool"
    }

    async fn execute(
        &self,
        node: &PipelineNode,
        context: &Context,
        _graph: &PipelineGraph,
    ) -> Result<Outcome> {
        let command = node
            .raw_attrs
            .get("tool_command")
            .and_then(|v| match v {
                AttributeValue::String(s) => Some(s.clone()),
                _ => None,
            })
            .ok_or_else(|| AttractorError::HandlerError {
                handler: "tool".into(),
                node: node.id.clone(),
                message: "Missing tool_command attribute".into(),
            })?;

        tracing::info!(node = %node.id, label = %node.label, command = %command, "Executing tool command");

        // Check if dry_run is set in context
        let dry_run = context
            .get("dry_run")
            .await
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if dry_run {
            tracing::info!(node = %node.id, "Dry run — skipping command execution");
            return Ok(Outcome {
                status: StageStatus::Success,
                preferred_label: None,
                suggested_next_ids: vec![],
                context_updates: {
                    let mut m = HashMap::new();
                    m.insert(
                        "last_tool_command".into(),
                        serde_json::Value::String(command.clone()),
                    );
                    m.insert(
                        format!("{}.completed", node.id),
                        serde_json::Value::Bool(true),
                    );
                    m.insert(
                        format!("{}.dry_run", node.id),
                        serde_json::Value::Bool(true),
                    );
                    m
                },
                notes: format!("Dry run — command not executed: {}", command),
                failure_reason: None,
            });
        }

        // Build the shell command
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg(&command);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        // Set working directory from context
        let snapshot = context.snapshot().await;
        if let Some(serde_json::Value::String(dir)) = snapshot.get("workdir") {
            cmd.current_dir(dir);
        }

        let child = cmd.spawn().map_err(|e| AttractorError::HandlerError {
            handler: "tool".into(),
            node: node.id.clone(),
            message: format!("Failed to spawn command: {}", e),
        })?;

        // Capture the PID before waiting so we can kill on timeout
        let child_pid = child.id();

        // Apply timeout if configured on the node, default 5 minutes
        let timeout_dur = node.timeout.unwrap_or(std::time::Duration::from_secs(300));
        let output = match tokio::time::timeout(timeout_dur, child.wait_with_output()).await {
            Ok(result) => result.map_err(|e| AttractorError::HandlerError {
                handler: "tool".into(),
                node: node.id.clone(),
                message: format!("Command execution failed: {}", e),
            })?,
            Err(_) => {
                // Kill the child process group on timeout to avoid leaking processes
                if let Some(pid) = child_pid {
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

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let exit_code = output.status.code().unwrap_or(-1);

        tracing::info!(
            node = %node.id,
            exit_code = exit_code,
            stdout_len = stdout.len(),
            stderr_len = stderr.len(),
            "Tool command completed"
        );

        let status = if output.status.success() {
            StageStatus::Success
        } else {
            StageStatus::Fail
        };

        let mut updates = HashMap::new();
        updates.insert(
            "last_tool_command".into(),
            serde_json::Value::String(command.clone()),
        );
        updates.insert(
            format!("{}.completed", node.id),
            serde_json::Value::Bool(true),
        );
        updates.insert(
            format!("{}.exit_code", node.id),
            serde_json::json!(exit_code),
        );
        updates.insert(
            format!("{}.stdout", node.id),
            serde_json::Value::String(stdout.clone()),
        );
        if !stderr.is_empty() {
            updates.insert(
                format!("{}.stderr", node.id),
                serde_json::Value::String(stderr.clone()),
            );
        }

        // Combine stdout + stderr for notes, truncating if very long
        let combined = if stderr.is_empty() {
            stdout
        } else {
            format!("{}\n--- stderr ---\n{}", stdout, stderr)
        };
        let notes = if combined.len() > 4096 {
            // Find a valid UTF-8 boundary at or before byte 4096
            let truncate_at = combined
                .char_indices()
                .map(|(i, _)| i)
                .take_while(|&i| i <= 4096)
                .last()
                .unwrap_or(0);
            format!("{}...(truncated)", &combined[..truncate_at])
        } else {
            combined
        };

        Ok(Outcome {
            status,
            preferred_label: None,
            suggested_next_ids: vec![],
            context_updates: updates,
            notes,
            failure_reason: if status == StageStatus::Fail {
                Some(format!("Command exited with code {}", exit_code))
            } else {
                None
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::tests::{make_minimal_graph, make_node};

    #[tokio::test]
    async fn tool_handler_dry_run_skips_execution() {
        let handler = ToolHandler;
        let mut attrs = HashMap::new();
        attrs.insert(
            "tool_command".into(),
            AttributeValue::String("cargo test".into()),
        );
        let node = make_node("t", "parallelogram", None, attrs);
        let ctx = Context::default();
        ctx.set("dry_run", serde_json::Value::Bool(true)).await;
        let graph = make_minimal_graph();

        let outcome = handler.execute(&node, &ctx, &graph).await.unwrap();
        assert_eq!(outcome.status, StageStatus::Success);
        assert_eq!(
            outcome.context_updates.get("last_tool_command"),
            Some(&serde_json::Value::String("cargo test".into()))
        );
        assert_eq!(
            outcome.context_updates.get("t.completed"),
            Some(&serde_json::Value::Bool(true))
        );
        assert_eq!(
            outcome.context_updates.get("t.dry_run"),
            Some(&serde_json::Value::Bool(true))
        );
        assert!(outcome.notes.contains("Dry run"));
    }

    #[tokio::test]
    async fn tool_handler_errors_on_missing_command() {
        let handler = ToolHandler;
        let node = make_node("t", "parallelogram", None, HashMap::new());
        let ctx = Context::default();
        let graph = make_minimal_graph();

        let result = handler.execute(&node, &ctx, &graph).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("Missing tool_command"),
            "Expected error about missing tool_command, got: {err}"
        );
    }

    #[tokio::test]
    async fn tool_handler_executes_command() {
        let handler = ToolHandler;
        let mut attrs = HashMap::new();
        attrs.insert(
            "tool_command".into(),
            AttributeValue::String("echo hello".into()),
        );
        let node = make_node("run_echo", "parallelogram", None, attrs);
        let ctx = Context::default();
        let graph = make_minimal_graph();

        let outcome = handler.execute(&node, &ctx, &graph).await.unwrap();
        assert_eq!(outcome.status, StageStatus::Success);
        assert!(outcome.failure_reason.is_none());
        assert!(outcome.notes.contains("hello"));
        assert_eq!(
            outcome.context_updates.get("run_echo.exit_code"),
            Some(&serde_json::json!(0))
        );
        assert!(outcome
            .context_updates
            .get("run_echo.stdout")
            .unwrap()
            .as_str()
            .unwrap()
            .contains("hello"));
    }

    #[tokio::test]
    async fn tool_handler_captures_failure() {
        let handler = ToolHandler;
        let mut attrs = HashMap::new();
        attrs.insert(
            "tool_command".into(),
            AttributeValue::String("exit 42".into()),
        );
        let node = make_node("fail_cmd", "parallelogram", None, attrs);
        let ctx = Context::default();
        let graph = make_minimal_graph();

        let outcome = handler.execute(&node, &ctx, &graph).await.unwrap();
        assert_eq!(outcome.status, StageStatus::Fail);
        assert!(outcome.failure_reason.is_some());
        assert!(outcome.failure_reason.unwrap().contains("42"));
        assert_eq!(
            outcome.context_updates.get("fail_cmd.exit_code"),
            Some(&serde_json::json!(42))
        );
    }
}
