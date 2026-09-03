use super::*;
use crate::test_utils::{
    make_client, EchoTool, MockEnv, RecordingTimeoutEnv, SequenceMockProvider,
};
use async_trait::async_trait;
use attractor_llm::{ContentPart, FinishReason, Response, Role, Usage};
use attractor_tools::{Tool, ToolDefinition as ToolsToolDef};

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

#[tokio::test]
async fn session_model_and_system_prompt_reach_the_provider_request() {
    let (provider, requests) = SequenceMockProvider::recording(vec![Response {
        id: "response".into(),
        text: "done".into(),
        tool_calls: vec![],
        reasoning: None,
        usage: Usage::default(),
        model: "configured-model".into(),
        finish_reason: FinishReason::EndTurn,
    }]);
    let config = SessionConfig {
        model: "configured-model".into(),
        system_prompt: "Configured system prompt".into(),
        ..Default::default()
    };
    let mut session = AgentSession::new(
        make_client(provider),
        ToolRegistry::new(),
        Box::new(MockEnv),
        config,
    );

    session.process_input("hello").await.unwrap();

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].model, "configured-model");
    assert!(matches!(
        requests[0].messages.first(),
        Some(attractor_llm::Message {
            role: Role::System,
            content,
            ..
        }) if matches!(content.as_slice(), [ContentPart::Text { text }] if text == "Configured system prompt")
    ));
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
    assert!(
        matches!(&session.history()[2], Turn::ToolResults { results } if results.len() == 1 && !results[0].is_error && results[0].content == "ping")
    );
    assert!(
        matches!(&session.history()[3], Turn::Assistant { content, .. } if content == "The echo returned: ping")
    );
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

#[tokio::test]
async fn repeated_tool_calls_inject_one_loop_steering_turn() {
    let responses = repeated_tool_responses(3);
    let client = make_client(SequenceMockProvider::new(responses));
    let mut registry = ToolRegistry::new();
    registry.register(EchoTool);
    let config = SessionConfig {
        loop_detection_window: 3,
        ..Default::default()
    };
    let mut session = AgentSession::new(client, registry, Box::new(MockEnv), config);

    assert_eq!(session.process_input("Repeat").await.unwrap(), "Done");

    let warnings = session
        .history()
        .iter()
        .filter_map(|turn| match turn {
            Turn::Steering { content } if content.contains("called the 'echo' tool 3 times") => {
                Some(content)
            }
            _ => None,
        })
        .count();
    assert_eq!(warnings, 1);
}

fn repeated_tool_responses(count: usize) -> Vec<Response> {
    (0..count)
        .map(|i| Response {
            id: format!("resp-{i}"),
            text: String::new(),
            tool_calls: vec![ToolCallResult {
                id: format!("tc-{i}"),
                name: "echo".into(),
                arguments: serde_json::json!({"text": "same"}),
            }],
            reasoning: None,
            usage: Usage::default(),
            model: "mock-model".into(),
            finish_reason: FinishReason::ToolUse,
        })
        .chain(std::iter::once(Response {
            id: "resp-final".into(),
            text: "Done".into(),
            tool_calls: vec![],
            reasoning: None,
            usage: Usage::default(),
            model: "mock-model".into(),
            finish_reason: FinishReason::EndTurn,
        }))
        .collect()
}

#[tokio::test]
async fn disabled_loop_detection_never_injects_steering() {
    let client = make_client(SequenceMockProvider::new(repeated_tool_responses(3)));
    let mut registry = ToolRegistry::new();
    registry.register(EchoTool);
    let config = SessionConfig {
        enable_loop_detection: false,
        loop_detection_window: 2,
        ..Default::default()
    };
    let mut session = AgentSession::new(client, registry, Box::new(MockEnv), config);

    assert_eq!(session.process_input("Repeat").await.unwrap(), "Done");
    assert!(!session
        .history()
        .iter()
        .any(|turn| matches!(turn, Turn::Steering { .. })));
}

#[tokio::test]
async fn zero_loop_detection_window_is_safe_and_never_injects_steering() {
    let client = make_client(SequenceMockProvider::new(repeated_tool_responses(3)));
    let mut registry = ToolRegistry::new();
    registry.register(EchoTool);
    let config = SessionConfig {
        enable_loop_detection: true,
        loop_detection_window: 0,
        ..Default::default()
    };
    let mut session = AgentSession::new(client, registry, Box::new(MockEnv), config);

    assert_eq!(session.process_input("Repeat").await.unwrap(), "Done");
    assert!(!session
        .history()
        .iter()
        .any(|turn| matches!(turn, Turn::Steering { .. })));
}

#[tokio::test]
async fn shell_tool_uses_session_default_command_timeout() {
    let responses = vec![
        Response {
            id: "resp-tool".into(),
            text: String::new(),
            tool_calls: vec![ToolCallResult {
                id: "tc-shell".into(),
                name: "shell".into(),
                arguments: serde_json::json!({"command": "true"}),
            }],
            reasoning: None,
            usage: Usage::default(),
            model: "mock-model".into(),
            finish_reason: FinishReason::ToolUse,
        },
        Response {
            id: "resp-final".into(),
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
    registry.register(attractor_tools::ShellTool);
    let (env, observed_timeouts) = RecordingTimeoutEnv::new();
    let config = SessionConfig {
        default_command_timeout_ms: 321,
        ..Default::default()
    };
    let mut session = AgentSession::new(client, registry, Box::new(env), config);

    assert_eq!(session.process_input("Run it").await.unwrap(), "Done");
    assert_eq!(*observed_timeouts.lock().unwrap(), vec![321]);
}

#[tokio::test]
async fn shell_tool_explicit_timeout_overrides_session_default() {
    let mut responses = repeated_tool_responses(1);
    responses[0].tool_calls[0].name = "shell".into();
    responses[0].tool_calls[0].arguments =
        serde_json::json!({"command": "true", "timeout_ms": 654});
    let client = make_client(SequenceMockProvider::new(responses));
    let mut registry = ToolRegistry::new();
    registry.register(attractor_tools::ShellTool);
    let (env, observed_timeouts) = RecordingTimeoutEnv::new();
    let config = SessionConfig {
        default_command_timeout_ms: 321,
        ..Default::default()
    };
    let mut session = AgentSession::new(client, registry, Box::new(env), config);

    assert_eq!(session.process_input("Run it").await.unwrap(), "Done");
    assert_eq!(*observed_timeouts.lock().unwrap(), vec![654]);
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
