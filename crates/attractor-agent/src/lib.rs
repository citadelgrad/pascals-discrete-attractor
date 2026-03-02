//! Autonomous coding agent loop: LLM + tool execution cycle.
//!
//! Provides `AgentSession` with the core agentic loop: build request -> call LLM ->
//! extract tool calls -> execute tools -> append results -> repeat.

pub mod fidelity;
pub mod loop_detection;
pub mod prompt_builder;
pub mod subagent;
#[cfg(test)]
mod test_utils;
pub use fidelity::{apply_fidelity, FidelityMode};
pub use loop_detection::{LoopDetector, SteeringInjector};
pub use prompt_builder::{discover_project_docs, ProjectDoc, SystemPromptBuilder};
pub use subagent::{SubagentConfig, SubagentManager, SubagentStatus};

use std::collections::VecDeque;

use attractor_llm::{
    ContentPart, Message, Request, ToolCallResult,
};
use attractor_tools::{ExecutionEnvironment, ToolRegistry};
use attractor_types::AttractorError;

// ---------------------------------------------------------------------------
// SessionConfig
// ---------------------------------------------------------------------------

/// Configuration for an agent session.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub model: String,
    pub system_prompt: String,
    /// Maximum number of user turns (0 = unlimited).
    pub max_turns: usize,
    /// Maximum number of tool-use rounds per `process_input` call.
    pub max_tool_rounds: usize,
    /// Default timeout for shell commands in milliseconds.
    pub default_command_timeout_ms: u64,
    /// Whether to detect tool-call loops.
    pub enable_loop_detection: bool,
    /// Window size for loop detection (consecutive identical calls).
    pub loop_detection_window: usize,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            model: "claude-sonnet-4-5-20250929".to_string(),
            system_prompt: "You are a helpful coding assistant.".to_string(),
            max_turns: 0,
            max_tool_rounds: 200,
            default_command_timeout_ms: 10_000,
            enable_loop_detection: true,
            loop_detection_window: 10,
        }
    }
}

// ---------------------------------------------------------------------------
// SessionState
// ---------------------------------------------------------------------------

/// Current state of the agent session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Idle,
    Processing,
    AwaitingInput,
    Closed,
}

// ---------------------------------------------------------------------------
// Turn
// ---------------------------------------------------------------------------

/// A single turn in the conversation history.
#[derive(Debug, Clone)]
pub enum Turn {
    User {
        content: String,
    },
    Assistant {
        content: String,
        tool_calls: Vec<ToolCallResult>,
    },
    ToolResults {
        results: Vec<ToolResultEntry>,
    },
    System {
        content: String,
    },
    Steering {
        content: String,
    },
}

/// Result of executing a single tool call.
#[derive(Debug, Clone)]
pub struct ToolResultEntry {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: String,
    pub is_error: bool,
}

// ---------------------------------------------------------------------------
// AgentSession
// ---------------------------------------------------------------------------

const MAX_TOOL_OUTPUT_LEN: usize = 30_000;

/// The core agent session that coordinates LLM calls, tool execution, and state.
pub struct AgentSession {
    id: String,
    llm_client: attractor_llm::LlmClient,
    tool_registry: ToolRegistry,
    env: Box<dyn ExecutionEnvironment>,
    history: Vec<Turn>,
    config: SessionConfig,
    state: SessionState,
    /// Steering messages injected between tool rounds.
    steering_queue: Vec<String>,
    /// Follow-up queue processed after current input.
    followup_queue: VecDeque<String>,
    /// Running count of user turns (for max_turns enforcement).
    user_turn_count: usize,
}

impl AgentSession {
    /// Create a new agent session with the given components and config.
    pub fn new(
        llm_client: attractor_llm::LlmClient,
        tool_registry: ToolRegistry,
        env: Box<dyn ExecutionEnvironment>,
        config: SessionConfig,
    ) -> Self {
        let id = uuid::Uuid::new_v4().to_string();
        tracing::info!(session_id = %id, model = %config.model, "Agent session created");
        Self {
            id,
            llm_client,
            tool_registry,
            env,
            history: Vec::new(),
            config,
            state: SessionState::Idle,
            steering_queue: Vec::new(),
            followup_queue: VecDeque::new(),
            user_turn_count: 0,
        }
    }

    /// Returns the session ID.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the current session state.
    pub fn state(&self) -> &SessionState {
        &self.state
    }

    /// Returns the conversation history.
    pub fn history(&self) -> &[Turn] {
        &self.history
    }

    /// Push a steering message to be injected at the next tool round boundary.
    pub fn steer(&mut self, message: String) {
        tracing::debug!("Steering message queued: {}", &message[..message.len().min(80)]);
        self.steering_queue.push(message);
    }

    /// Push a follow-up input to be processed after the current input completes.
    pub fn follow_up(&mut self, message: String) {
        tracing::debug!("Follow-up queued: {}", &message[..message.len().min(80)]);
        self.followup_queue.push_back(message);
    }

    /// Drain all pending steering messages into the history as Steering turns.
    fn drain_steering(&mut self) {
        for msg in self.steering_queue.drain(..) {
            tracing::debug!("Injecting steering turn");
            self.history.push(Turn::Steering { content: msg });
        }
    }

    /// Core agentic loop: process user input through LLM + tool cycles.
    ///
    /// Returns the assistant's final text response.
    /// Core agentic loop: process user input through LLM + tool cycles.
    ///
    /// Returns the assistant's final text response. After completion, any
    /// queued follow-up messages are processed in order.
    pub async fn process_input(&mut self, user_input: &str) -> attractor_types::Result<String> {
        let mut current_input = user_input.to_string();

        loop {
            let result = self.process_single_input(&current_input).await?;

            // Check for follow-up messages
            if let Some(followup) = self.followup_queue.pop_front() {
                tracing::debug!("Processing follow-up message");
                current_input = followup;
                continue;
            }

            self.state = SessionState::Idle;
            return Ok(result);
        }
    }

    /// Process a single user input through the LLM + tool loop.
    async fn process_single_input(&mut self, user_input: &str) -> attractor_types::Result<String> {
        // Check turn limits
        self.user_turn_count += 1;
        if self.config.max_turns > 0 && self.user_turn_count > self.config.max_turns {
            return Err(AttractorError::TurnLimitReached {
                turns: self.user_turn_count,
            });
        }

        self.state = SessionState::Processing;

        // Append user turn
        self.history.push(Turn::User {
            content: user_input.to_string(),
        });

        // Drain any pending steering messages
        self.drain_steering();

        let mut last_assistant_text = String::new();

        // Tool-use loop
        for round in 0..self.config.max_tool_rounds {
            tracing::debug!(round, "Starting tool round");

            // Build LLM request from history
            let request = self.build_request();

            // Call LLM
            let response = self.llm_client.complete(&request).await?;

            tracing::info!(
                round,
                input_tokens = response.usage.input_tokens,
                output_tokens = response.usage.output_tokens,
                finish_reason = ?response.finish_reason,
                tool_calls = response.tool_calls.len(),
                "LLM response received"
            );

            // Record assistant turn
            last_assistant_text = response.text.clone();
            self.history.push(Turn::Assistant {
                content: response.text.clone(),
                tool_calls: response.tool_calls.clone(),
            });

            // If no tool calls, we are done (natural completion)
            if response.tool_calls.is_empty() {
                tracing::debug!("No tool calls, ending loop");
                break;
            }

            // Check if this is the last allowed round
            if round + 1 >= self.config.max_tool_rounds {
                tracing::info!(
                    max_rounds = self.config.max_tool_rounds,
                    "Max tool rounds reached, stopping loop"
                );
                break;
            }

            // Execute each tool call
            let results = self.execute_tool_calls(&response.tool_calls).await;

            // Append tool results turn
            self.history.push(Turn::ToolResults { results });

            // Drain steering queue between rounds
            self.drain_steering();
        }

        self.state = SessionState::AwaitingInput;
        Ok(last_assistant_text)
    }

    /// Build an LLM Request from the conversation history.
    fn build_request(&self) -> Request {
        let mut messages = Vec::new();

        // System message
        if !self.config.system_prompt.is_empty() {
            messages.push(Message::system(&self.config.system_prompt));
        }

        // Map each Turn to LLM messages
        for turn in &self.history {
            match turn {
                Turn::User { content } => {
                    messages.push(Message::user(content));
                }
                Turn::Assistant {
                    content,
                    tool_calls,
                } => {
                    let mut parts = Vec::new();
                    if !content.is_empty() {
                        parts.push(ContentPart::Text {
                            text: content.clone(),
                        });
                    }
                    for tc in tool_calls {
                        parts.push(ContentPart::ToolCall {
                            id: tc.id.clone(),
                            name: tc.name.clone(),
                            arguments: tc.arguments.clone(),
                        });
                    }
                    messages.push(Message {
                        role: attractor_llm::Role::Assistant,
                        content: parts,
                        name: None,
                        tool_call_id: None,
                    });
                }
                Turn::ToolResults { results } => {
                    for result in results {
                        messages.push(Message::tool_result(
                            &result.tool_call_id,
                            &result.content,
                            result.is_error,
                        ));
                    }
                }
                Turn::System { content } => {
                    messages.push(Message::system(content));
                }
                Turn::Steering { content } => {
                    messages.push(Message::system(content));
                }
            }
        }

        // Convert tool definitions from tools crate to LLM crate format
        let tools: Vec<attractor_llm::ToolDefinition> = self
            .tool_registry
            .definitions()
            .into_iter()
            .map(|td| attractor_llm::ToolDefinition {
                name: td.name,
                description: td.description,
                parameters: td.parameters,
            })
            .collect();

        Request {
            model: self.config.model.clone(),
            messages,
            tools,
            tool_choice: None,
            max_tokens: None,
            temperature: None,
            stop_sequences: vec![],
            reasoning_effort: None,
            provider: None,
            provider_options: None,
        }
    }

    /// Execute a batch of tool calls and return the results.
    async fn execute_tool_calls(&self, tool_calls: &[ToolCallResult]) -> Vec<ToolResultEntry> {
        let mut results = Vec::with_capacity(tool_calls.len());

        for tc in tool_calls {
            tracing::debug!(tool = %tc.name, id = %tc.id, "Executing tool call");

            let (content, is_error) = match self.tool_registry.get(&tc.name) {
                Some(tool) => {
                    match tool.execute(tc.arguments.clone(), self.env.as_ref()).await {
                        Ok(output) => {
                            let truncated = if output.len() > MAX_TOOL_OUTPUT_LEN {
                                let mut t = output[..MAX_TOOL_OUTPUT_LEN].to_string();
                                t.push_str(&format!(
                                    "\n\n[WARNING: Output truncated. {} characters removed.]",
                                    output.len() - MAX_TOOL_OUTPUT_LEN
                                ));
                                t
                            } else {
                                output
                            };
                            (truncated, false)
                        }
                        Err(e) => {
                            tracing::debug!(tool = %tc.name, error = %e, "Tool execution failed");
                            (format!("Error: {}", e), true)
                        }
                    }
                }
                None => {
                    let msg = format!("Unknown tool: {}", tc.name);
                    tracing::debug!("{}", msg);
                    (msg, true)
                }
            };

            results.push(ToolResultEntry {
                tool_call_id: tc.id.clone(),
                tool_name: tc.name.clone(),
                content,
                is_error,
            });
        }

        results
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use attractor_llm::{FinishReason, Response, Usage};
    use attractor_tools::{Tool, ToolDefinition as ToolsToolDef};
    use crate::test_utils::{make_client, EchoTool, MockEnv, SequenceMockProvider};

    // -----------------------------------------------------------------------
    // Test 1: Session creation with config
    // -----------------------------------------------------------------------

    #[test]
    fn session_creation_with_config() {
        let client = make_client(SequenceMockProvider::single_text("hello"));
        let registry = ToolRegistry::new();
        let env = Box::new(MockEnv);
        let config = SessionConfig {
            model: "test-model".to_string(),
            system_prompt: "You are helpful.".to_string(),
            max_turns: 10,
            max_tool_rounds: 50,
            ..Default::default()
        };

        let session = AgentSession::new(client, registry, env, config);

        assert!(!session.id().is_empty());
        assert_eq!(*session.state(), SessionState::Idle);
        assert!(session.history().is_empty());
    }

    // -----------------------------------------------------------------------
    // Test 2: Process input with no tools -> returns LLM text
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn process_input_no_tools_returns_text() {
        let provider = SequenceMockProvider::single_text("Hello, world!");
        let client = make_client(provider);
        let registry = ToolRegistry::new();
        let env = Box::new(MockEnv);
        let config = SessionConfig::default();

        let mut session = AgentSession::new(client, registry, env, config);
        let result = session.process_input("Hi there").await.unwrap();

        assert_eq!(result, "Hello, world!");
        assert_eq!(session.history().len(), 2); // User + Assistant
        assert!(matches!(&session.history()[0], Turn::User { content } if content == "Hi there"));
        assert!(
            matches!(&session.history()[1], Turn::Assistant { content, tool_calls } if content == "Hello, world!" && tool_calls.is_empty())
        );
    }

    // -----------------------------------------------------------------------
    // Test 3: Process input with tool call -> executes tool and returns
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn process_input_with_tool_call() {
        // First response: tool call. Second response: final text.
        let responses = vec![
            Response {
                id: "resp-1".into(),
                text: String::new(),
                tool_calls: vec![ToolCallResult {
                    id: "tc-1".into(),
                    name: "echo".into(),
                    arguments: serde_json::json!({"text": "ping"}),
                }],
                reasoning: None,
                usage: Usage::default(),
                model: "mock-model".into(),
                finish_reason: FinishReason::ToolUse,
            },
            Response {
                id: "resp-2".into(),
                text: "The echo returned: ping".into(),
                tool_calls: vec![],
                reasoning: None,
                usage: Usage::default(),
                model: "mock-model".into(),
                finish_reason: FinishReason::EndTurn,
            },
        ];

        let client = make_client(SequenceMockProvider::new(responses));
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool);
        let env = Box::new(MockEnv);
        let config = SessionConfig::default();

        let mut session = AgentSession::new(client, registry, env, config);
        let result = session.process_input("Echo ping for me").await.unwrap();

        assert_eq!(result, "The echo returned: ping");

        // History: User, Assistant(tool_call), ToolResults, Assistant(final)
        assert_eq!(session.history().len(), 4);
        assert!(matches!(&session.history()[0], Turn::User { .. }));
        assert!(matches!(
            &session.history()[1],
            Turn::Assistant { tool_calls, .. } if tool_calls.len() == 1
        ));
        assert!(matches!(&session.history()[2], Turn::ToolResults { results } if results.len() == 1 && !results[0].is_error && results[0].content == "ping"));
        assert!(matches!(&session.history()[3], Turn::Assistant { content, .. } if content == "The echo returned: ping"));
    }

    // -----------------------------------------------------------------------
    // Test 4: Steering queue drained between rounds
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn steering_queue_drained_between_rounds() {
        // Response sequence: tool call -> final text
        let responses = vec![
            Response {
                id: "resp-1".into(),
                text: String::new(),
                tool_calls: vec![ToolCallResult {
                    id: "tc-1".into(),
                    name: "echo".into(),
                    arguments: serde_json::json!({"text": "hello"}),
                }],
                reasoning: None,
                usage: Usage::default(),
                model: "mock-model".into(),
                finish_reason: FinishReason::ToolUse,
            },
            Response {
                id: "resp-2".into(),
                text: "Done".into(),
                tool_calls: vec![],
                reasoning: None,
                usage: Usage::default(),
                model: "mock-model".into(),
                finish_reason: FinishReason::EndTurn,
            },
        ];

        let client = make_client(SequenceMockProvider::new(responses));
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool);
        let env = Box::new(MockEnv);
        let config = SessionConfig::default();

        let mut session = AgentSession::new(client, registry, env, config);

        // Queue steering before processing
        session.steer("Focus on security.".to_string());

        let result = session.process_input("Do something").await.unwrap();
        assert_eq!(result, "Done");

        // Verify steering turns appear in history.
        // History should be: User, Steering("Focus on security."), Assistant(tool), ToolResults, Assistant(final)
        let steering_count = session
            .history()
            .iter()
            .filter(|t| matches!(t, Turn::Steering { .. }))
            .count();
        assert!(
            steering_count >= 1,
            "Expected at least 1 steering turn, found {}",
            steering_count
        );

        // The first steering should be right after the user turn (drained before loop starts)
        assert!(matches!(
            &session.history()[1],
            Turn::Steering { content } if content == "Focus on security."
        ));
    }

    // -----------------------------------------------------------------------
    // Test 5: Max tool rounds limit stops loop
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn max_tool_rounds_stops_loop() {
        // Provider always returns a tool call, never stops on its own.
        let infinite_tool_calls: Vec<Response> = (0..10)
            .map(|i| Response {
                id: format!("resp-{}", i),
                text: format!("round {}", i),
                tool_calls: vec![ToolCallResult {
                    id: format!("tc-{}", i),
                    name: "echo".into(),
                    arguments: serde_json::json!({"text": "loop"}),
                }],
                reasoning: None,
                usage: Usage::default(),
                model: "mock-model".into(),
                finish_reason: FinishReason::ToolUse,
            })
            .collect();

        let client = make_client(SequenceMockProvider::new(infinite_tool_calls));
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool);
        let env = Box::new(MockEnv);
        let config = SessionConfig {
            max_tool_rounds: 3,
            ..Default::default()
        };

        let mut session = AgentSession::new(client, registry, env, config);
        let result = session.process_input("Loop forever").await.unwrap();

        // Should have stopped after 3 rounds. The last response's text is returned.
        assert_eq!(result, "round 2"); // 0-indexed: rounds 0, 1, 2

        // Count assistant turns to verify we only had 3 LLM calls
        let assistant_count = session
            .history()
            .iter()
            .filter(|t| matches!(t, Turn::Assistant { .. }))
            .count();
        assert_eq!(assistant_count, 3);
    }

    // -----------------------------------------------------------------------
    // Test 6: Unknown tool returns error result
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn unknown_tool_returns_error_result() {
        let responses = vec![
            Response {
                id: "resp-1".into(),
                text: String::new(),
                tool_calls: vec![ToolCallResult {
                    id: "tc-1".into(),
                    name: "nonexistent_tool".into(),
                    arguments: serde_json::json!({}),
                }],
                reasoning: None,
                usage: Usage::default(),
                model: "mock-model".into(),
                finish_reason: FinishReason::ToolUse,
            },
            Response {
                id: "resp-2".into(),
                text: "Tool not found, sorry.".into(),
                tool_calls: vec![],
                reasoning: None,
                usage: Usage::default(),
                model: "mock-model".into(),
                finish_reason: FinishReason::EndTurn,
            },
        ];

        let client = make_client(SequenceMockProvider::new(responses));
        let registry = ToolRegistry::new(); // No tools registered
        let env = Box::new(MockEnv);
        let config = SessionConfig::default();

        let mut session = AgentSession::new(client, registry, env, config);
        let result = session.process_input("Use nonexistent tool").await.unwrap();

        assert_eq!(result, "Tool not found, sorry.");

        // Check the tool result was an error
        let tool_results = session.history().iter().find_map(|t| {
            if let Turn::ToolResults { results } = t {
                Some(results)
            } else {
                None
            }
        });
        let results = tool_results.expect("Expected ToolResults turn");
        assert_eq!(results.len(), 1);
        assert!(results[0].is_error);
        assert!(results[0].content.contains("Unknown tool"));
    }

    // -----------------------------------------------------------------------
    // Test 7: Turn limit enforcement
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn turn_limit_enforcement() {
        let provider = SequenceMockProvider::new(vec![
            Response {
                id: "r1".into(),
                text: "first".into(),
                tool_calls: vec![],
                reasoning: None,
                usage: Usage::default(),
                model: "m".into(),
                finish_reason: FinishReason::EndTurn,
            },
            Response {
                id: "r2".into(),
                text: "second".into(),
                tool_calls: vec![],
                reasoning: None,
                usage: Usage::default(),
                model: "m".into(),
                finish_reason: FinishReason::EndTurn,
            },
        ]);
        let client = make_client(provider);
        let registry = ToolRegistry::new();
        let env = Box::new(MockEnv);
        let config = SessionConfig {
            max_turns: 1,
            ..Default::default()
        };

        let mut session = AgentSession::new(client, registry, env, config);

        // First turn should work
        let r1 = session.process_input("first").await.unwrap();
        assert_eq!(r1, "first");

        // Second turn should fail with TurnLimitReached
        let r2 = session.process_input("second").await;
        assert!(r2.is_err());
        assert!(matches!(
            r2.unwrap_err(),
            AttractorError::TurnLimitReached { .. }
        ));
    }

    // -----------------------------------------------------------------------
    // Test 8: Tool output truncation
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn tool_output_truncation() {
        // Create a tool that returns a very long output
        struct BigOutputTool;

        #[async_trait]
        impl Tool for BigOutputTool {
            fn definition(&self) -> ToolsToolDef {
                ToolsToolDef {
                    name: "big_output".to_string(),
                    description: "Returns a large output".to_string(),
                    parameters: serde_json::json!({"type": "object"}),
                }
            }
            async fn execute(
                &self,
                _arguments: serde_json::Value,
                _env: &dyn ExecutionEnvironment,
            ) -> attractor_types::Result<String> {
                Ok("x".repeat(50_000))
            }
        }

        let responses = vec![
            Response {
                id: "r1".into(),
                text: String::new(),
                tool_calls: vec![ToolCallResult {
                    id: "tc-1".into(),
                    name: "big_output".into(),
                    arguments: serde_json::json!({}),
                }],
                reasoning: None,
                usage: Usage::default(),
                model: "m".into(),
                finish_reason: FinishReason::ToolUse,
            },
            Response {
                id: "r2".into(),
                text: "Got it".into(),
                tool_calls: vec![],
                reasoning: None,
                usage: Usage::default(),
                model: "m".into(),
                finish_reason: FinishReason::EndTurn,
            },
        ];

        let client = make_client(SequenceMockProvider::new(responses));
        let mut registry = ToolRegistry::new();
        registry.register(BigOutputTool);
        let env = Box::new(MockEnv);
        let config = SessionConfig::default();

        let mut session = AgentSession::new(client, registry, env, config);
        let result = session.process_input("big output").await.unwrap();
        assert_eq!(result, "Got it");

        // Verify the tool result was truncated
        let tool_results = session.history().iter().find_map(|t| {
            if let Turn::ToolResults { results } = t {
                Some(results)
            } else {
                None
            }
        });
        let results = tool_results.unwrap();
        assert!(results[0].content.contains("[WARNING: Output truncated."));
        assert!(results[0].content.contains("20000 characters removed"));
    }
}
