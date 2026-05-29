use super::*;
use crate::{ContentPart, DynProvider, Message, ToolDefinition};

fn make_basic_request() -> Request {
    Request {
        model: "claude-sonnet-4-5-20250929".into(),
        messages: vec![
            Message::system("You are helpful."),
            Message::user("Hello"),
        ],
        tools: vec![],
        tool_choice: None,
        max_tokens: Some(1024),
        temperature: None,
        stop_sequences: vec![],
        reasoning_effort: None,
        provider: Some("anthropic".into()),
        provider_options: None,
    }
}

#[test]
fn build_request_body_extracts_system_messages() {
    let req = make_basic_request();
    let body = build_request_body(&req);

    // System should be a top-level array
    let system = body["system"].as_array().expect("system should be an array");
    assert_eq!(system.len(), 1);
    assert_eq!(system[0]["type"], "text");
    assert_eq!(system[0]["text"], "You are helpful.");
    // Cache control should be injected on system
    assert_eq!(system[0]["cache_control"]["type"], "ephemeral");

    // Messages should only contain the user message
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], "user");
}

#[test]
fn build_request_body_converts_tool_calls() {
    let mut req = make_basic_request();
    req.messages.push(Message {
        role: Role::Assistant,
        content: vec![ContentPart::ToolCall {
            id: "tc_1".into(),
            name: "search".into(),
            arguments: json!({"query": "rust"}),
        }],
        name: None,
        tool_call_id: None,
    });
    req.tools = vec![ToolDefinition {
        name: "search".into(),
        description: "Search the web".into(),
        parameters: json!({"type": "object", "properties": {"query": {"type": "string"}}}),
    }];

    let body = build_request_body(&req);

    // Check the assistant message has tool_use
    let messages = body["messages"].as_array().unwrap();
    let assistant_msg = &messages[1];
    assert_eq!(assistant_msg["role"], "assistant");
    let content = assistant_msg["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "tool_use");
    assert_eq!(content[0]["id"], "tc_1");
    assert_eq!(content[0]["name"], "search");
    assert_eq!(content[0]["input"]["query"], "rust");

    // Check tools
    let tools = body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "search");
    assert_eq!(tools[0]["input_schema"]["type"], "object");
}

#[test]
fn parse_response_handles_text_and_tool_use() {
    let body = json!({
        "id": "msg_123",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-5-20250929",
        "content": [
            {"type": "text", "text": "Let me search for that."},
            {"type": "tool_use", "id": "tc_1", "name": "search", "input": {"q": "rust"}}
        ],
        "stop_reason": "tool_use",
        "usage": {
            "input_tokens": 100,
            "output_tokens": 50,
            "cache_creation_input_tokens": 10,
            "cache_read_input_tokens": 20
        }
    });

    let resp = parse_response(&body).unwrap();
    assert_eq!(resp.id, "msg_123");
    assert_eq!(resp.model, "claude-sonnet-4-5-20250929");
    assert_eq!(resp.text, "Let me search for that.");
    assert_eq!(resp.finish_reason, FinishReason::ToolUse);
    assert_eq!(resp.tool_calls.len(), 1);
    assert_eq!(resp.tool_calls[0].id, "tc_1");
    assert_eq!(resp.tool_calls[0].name, "search");
    assert_eq!(resp.tool_calls[0].arguments["q"], "rust");
    assert_eq!(resp.usage.input_tokens, 100);
    assert_eq!(resp.usage.output_tokens, 50);
    assert_eq!(resp.usage.cache_write_tokens, Some(10));
    assert_eq!(resp.usage.cache_read_tokens, Some(20));
    assert_eq!(resp.usage.total_tokens, 150);
}

#[test]
fn from_env_returns_auth_error_when_key_not_set() {
    // Remove the env var if it exists
    std::env::remove_var("ANTHROPIC_API_KEY");
    let result = AnthropicAdapter::from_env();
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(matches!(err, AttractorError::AuthError { provider } if provider == "anthropic"));
}

#[test]
fn dyn_provider_wrapping_works() {
    let adapter = AnthropicAdapter::new("test-key".into());
    let provider = DynProvider::new(adapter);
    assert_eq!(provider.name(), "anthropic");
    assert_eq!(provider.default_model(), "claude-sonnet-4-5-20250929");
    assert!(provider.supports_tools());
    assert!(provider.supports_streaming());
    assert!(provider.supports_reasoning());
    assert_eq!(provider.context_window_size(), 200_000);
}

#[test]
fn parse_response_handles_thinking_blocks() {
    let body = json!({
        "id": "msg_456",
        "model": "claude-sonnet-4-5-20250929",
        "content": [
            {"type": "thinking", "thinking": "Let me think about this..."},
            {"type": "text", "text": "Here is my answer."}
        ],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 50, "output_tokens": 30}
    });

    let resp = parse_response(&body).unwrap();
    assert_eq!(resp.reasoning, Some("Let me think about this...".into()));
    assert_eq!(resp.text, "Here is my answer.");
    assert_eq!(resp.finish_reason, FinishReason::EndTurn);
}

#[test]
fn error_mapping_429_rate_limited() {
    let err = map_error(
        reqwest::StatusCode::TOO_MANY_REQUESTS,
        r#"{"error": {"message": "rate limited", "retry_after": 2.5}}"#,
    );
    assert!(matches!(err, AttractorError::RateLimited { retry_after_ms: 2500, .. }));
}

#[test]
fn error_mapping_401_auth() {
    let err = map_error(
        reqwest::StatusCode::UNAUTHORIZED,
        r#"{"error": {"message": "invalid api key"}}"#,
    );
    assert!(matches!(err, AttractorError::AuthError { .. }));
}

#[test]
fn error_mapping_400_not_retryable() {
    let err = map_error(
        reqwest::StatusCode::BAD_REQUEST,
        r#"{"error": {"message": "bad request"}}"#,
    );
    match &err {
        AttractorError::ProviderError { retryable, status, .. } => {
            assert!(!retryable);
            assert_eq!(*status, 400);
        }
        _ => panic!("expected ProviderError"),
    }
}

#[test]
fn error_mapping_500_retryable() {
    let err = map_error(
        reqwest::StatusCode::INTERNAL_SERVER_ERROR,
        r#"{"error": {"message": "server error"}}"#,
    );
    match &err {
        AttractorError::ProviderError { retryable, status, .. } => {
            assert!(*retryable);
            assert_eq!(*status, 500);
        }
        _ => panic!("expected ProviderError"),
    }
}

#[test]
fn tool_result_messages_merge_into_user() {
    let messages = vec![
        Message::user("Use the tool"),
        Message {
            role: Role::Assistant,
            content: vec![ContentPart::ToolCall {
                id: "tc_1".into(),
                name: "search".into(),
                arguments: json!({"q": "test"}),
            }],
            name: None,
            tool_call_id: None,
        },
        Message::tool_result("tc_1", "result data", false),
    ];

    let converted = convert_messages(&messages);
    // Tool result should create a new user message (since previous is assistant)
    assert_eq!(converted.len(), 3);
    assert_eq!(converted[2]["role"], "user");
    let content = converted[2]["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "tool_result");
    assert_eq!(content[0]["tool_use_id"], "tc_1");
}
