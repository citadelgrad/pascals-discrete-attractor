use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Mutex;

use async_trait::async_trait;
use attractor_llm::{FinishReason, ProviderAdapter, Request, Response, StreamEvent, Usage};
use attractor_tools::{
    DirEntry, ExecResult, ExecutionEnvironment, GrepOptions, Tool,
    ToolDefinition as ToolsToolDef,
};
use attractor_types::AttractorError;
use futures_core::Stream;

// -----------------------------------------------------------------------
// Mock ExecutionEnvironment
// -----------------------------------------------------------------------

pub struct MockEnv;

#[async_trait]
impl ExecutionEnvironment for MockEnv {
    async fn read_file(&self, _path: &Path) -> attractor_types::Result<String> {
        Ok("mock file content".to_string())
    }
    async fn write_file(&self, _path: &Path, _content: &str) -> attractor_types::Result<()> {
        Ok(())
    }
    async fn file_exists(&self, _path: &Path) -> attractor_types::Result<bool> {
        Ok(false)
    }
    async fn list_directory(
        &self,
        _path: &Path,
        _depth: usize,
    ) -> attractor_types::Result<Vec<DirEntry>> {
        Ok(vec![])
    }
    async fn exec_command(
        &self,
        _command: &str,
        _timeout_ms: u64,
        _cwd: Option<&Path>,
        _env_vars: Option<&HashMap<String, String>>,
    ) -> attractor_types::Result<ExecResult> {
        Ok(ExecResult {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
            duration_ms: 0,
        })
    }
    async fn grep(
        &self,
        _pattern: &str,
        _path: &Path,
        _options: &GrepOptions,
    ) -> attractor_types::Result<String> {
        Ok(String::new())
    }
    async fn glob_files(
        &self,
        _pattern: &str,
        _base: &Path,
    ) -> attractor_types::Result<Vec<PathBuf>> {
        Ok(vec![])
    }
    fn working_directory(&self) -> &Path {
        Path::new("/tmp")
    }
    fn platform(&self) -> &str {
        "test"
    }
}

// -----------------------------------------------------------------------
// Mock LLM Provider
// -----------------------------------------------------------------------

/// A mock provider that returns responses from a pre-configured sequence.
/// Each call to `complete` pops the next response from the queue.
pub struct SequenceMockProvider {
    responses: Mutex<VecDeque<Response>>,
}

impl SequenceMockProvider {
    pub fn new(responses: Vec<Response>) -> Self {
        Self {
            responses: Mutex::new(VecDeque::from(responses)),
        }
    }

    pub fn single_text(text: &str) -> Self {
        Self::new(vec![Response {
            id: "resp-1".into(),
            text: text.to_string(),
            tool_calls: vec![],
            reasoning: None,
            usage: Usage::default(),
            model: "mock-model".into(),
            finish_reason: FinishReason::EndTurn,
        }])
    }
}

#[async_trait]
impl ProviderAdapter for SequenceMockProvider {
    async fn complete(&self, _request: &Request) -> Result<Response, AttractorError> {
        let mut queue = self.responses.lock().unwrap();
        match queue.pop_front() {
            Some(resp) => Ok(resp),
            None => Ok(Response {
                id: "resp-fallback".into(),
                text: "No more responses".into(),
                tool_calls: vec![],
                reasoning: None,
                usage: Usage::default(),
                model: "mock-model".into(),
                finish_reason: FinishReason::EndTurn,
            }),
        }
    }
    fn stream(
        &self,
        _request: &Request,
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + '_>> {
        Box::pin(tokio_stream::empty::<StreamEvent>())
    }
    fn name(&self) -> &str {
        "mock"
    }
    fn default_model(&self) -> &str {
        "mock-model"
    }
    fn supports_tools(&self) -> bool {
        true
    }
    fn supports_streaming(&self) -> bool {
        false
    }
    fn supports_reasoning(&self) -> bool {
        false
    }
    fn context_window_size(&self) -> usize {
        128_000
    }
}

/// Helper: build an LlmClient with the given mock provider.
pub fn make_client(provider: SequenceMockProvider) -> attractor_llm::LlmClient {
    let mut client = attractor_llm::LlmClient::new();
    client.register_provider(provider);
    client
}

// -----------------------------------------------------------------------
// Mock Tool
// -----------------------------------------------------------------------

pub struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn definition(&self) -> ToolsToolDef {
        ToolsToolDef {
            name: "echo".to_string(),
            description: "Echoes the input text".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string" }
                }
            }),
        }
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
        _env: &dyn ExecutionEnvironment,
    ) -> attractor_types::Result<String> {
        let text = arguments
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("(empty)");
        Ok(text.to_string())
    }
}
