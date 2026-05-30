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
                            &result.tool_name,
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
#[path = "tests.rs"]
mod tests;
