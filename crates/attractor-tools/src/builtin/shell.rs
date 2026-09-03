use async_trait::async_trait;
use serde_json::json;

use crate::environment::ExecutionEnvironment;
use crate::tool::{Tool, ToolDefinition, ToolExecutionOptions};
use crate::truncation::{truncate_output, TruncationMode};

const MAX_OUTPUT_CHARS: usize = 30_000;

pub struct ShellTool;

#[async_trait]
impl Tool for ShellTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "shell".to_string(),
            description: "Execute a shell command.".to_string(),
            parameters: json!({
                "type": "object",
                "required": ["command"],
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute"
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "Timeout in milliseconds (default: 10000)",
                        "default": 10000
                    }
                }
            }),
        }
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
        env: &dyn ExecutionEnvironment,
    ) -> attractor_types::Result<String> {
        self.execute_with_options(arguments, env, ToolExecutionOptions::default())
            .await
    }

    async fn execute_with_options(
        &self,
        arguments: serde_json::Value,
        env: &dyn ExecutionEnvironment,
        options: ToolExecutionOptions,
    ) -> attractor_types::Result<String> {
        let command = arguments
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| attractor_types::AttractorError::ToolError {
                tool: "shell".into(),
                message: "command is required".into(),
            })?;

        let timeout_ms = arguments
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .or(options.default_command_timeout_ms)
            .unwrap_or(10_000);

        let result = env.exec_command(command, timeout_ms, None, None).await?;

        let output = format!(
            "Exit code: {}\nStdout:\n{}\nStderr:\n{}",
            result.exit_code, result.stdout, result.stderr
        );

        Ok(truncate_output(
            &output,
            MAX_OUTPUT_CHARS,
            TruncationMode::HeadTail,
        ))
    }
}
