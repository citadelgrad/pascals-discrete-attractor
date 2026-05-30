use super::*;
use crate::{Message, ReasoningEffort, ToolDefinition};

fn make_basic_request() -> Request {
    Request {
        model: "gpt-4o".into(),
        messages: vec![
            Message::system("You are helpful."),
            Message::user("Hello"),
        ],
        tools: vec![],
        tool_choice: None,
        max_tokens: Some(4096),
        temperature: Some(0.7),
        stop_sequences: vec![],
        reasoning_effort: None,
        provider: Some("openai".into()),
        provider_options: None,
    }
}

// Note: from_env tests use serial execution to avoid env var races.
// We test them together in a single test.
#[test]
fn from_env_with_key_returns_ok_and_without_key_returns_err() {
    // First test: with key set
    std::env::set_var("OPENAI_API_KEY", "test-key-12345");
    let result = OpenAiAdapter::from_env();
    assert!(result.is_ok());
    let adapter = result.unwrap();
    assert_eq!(adapter.name(), "openai");
    assert_eq!(adapter.default_model(), "gpt-4o");

    // Second test: without key
    std::env::remove_var("OPENAI_API_KEY");
    let result = OpenAiAdapter::from_env();
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(matches!(err, AttractorError::AuthError { provider } if provider == "openai"));
}

#[test]
fn build_request_body_produces_correct_structure() {
    let adapter = OpenAiAdapter::new("test-key".into());
    let mut req = make_basic_request();
    req.tools = vec![ToolDefinition {
        name: "search".into(),
        description: "Search files".into(),
        parameters: json!({"type": "object", "properties": {"query": {"type": "string"}}}),
    }];
    req.reasoning_effort = Some(ReasoningEffort::Medium);

    let body = adapter.build_request_body(&req);

    // Check model
    assert_eq!(body["model"], "gpt-4o");

    // Check input array
    let input = body["input"].as_array().expect("input should be an array");
    assert_eq!(input.len(), 2);
    assert_eq!(input[0]["role"], "system");
    assert_eq!(input[0]["content"], "You are helpful.");
    assert_eq!(input[1]["role"], "user");
    assert_eq!(input[1]["content"], "Hello");

    // Check max_output_tokens
    assert_eq!(body["max_output_tokens"], 4096);

    // Check temperature (compare as f64 to avoid float precision issues)
    let temp = body["temperature"].as_f64().unwrap();
    assert!((temp - 0.7).abs() < 0.01);

    // Check tools
    let tools = body["tools"].as_array().expect("tools should be an array");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["type"], "function");
    assert_eq!(tools[0]["function"]["name"], "search");
    assert_eq!(tools[0]["function"]["description"], "Search files");
    assert_eq!(tools[0]["function"]["parameters"]["type"], "object");

    // Check reasoning
    assert_eq!(body["reasoning"]["effort"], "medium");
}

#[test]
fn parse_response_handles_complete_response() {
    let adapter = OpenAiAdapter::new("test-key".into());
    let response_json = json!({
        "id": "resp_abc123",
        "output": [
            {
                "type": "message",
                "content": [
                    { "type": "output_text", "text": "Hello! How can I help you?" }
                ]
            }
        ],
        "usage": {
            "input_tokens": 100,
            "output_tokens": 50,
            "input_tokens_details": { "cached_tokens": 10 },
            "output_tokens_details": { "reasoning_tokens": 5 }
        },
        "model": "gpt-4o",
        "status": "completed"
    });

    let resp = adapter.parse_response(response_json).unwrap();
    assert_eq!(resp.id, "resp_abc123");
    assert_eq!(resp.model, "gpt-4o");
    assert_eq!(resp.text, "Hello! How can I help you?");
    assert_eq!(resp.finish_reason, FinishReason::EndTurn);
    assert!(resp.tool_calls.is_empty());
    assert_eq!(resp.usage.input_tokens, 100);
    assert_eq!(resp.usage.output_tokens, 50);
    assert_eq!(resp.usage.cache_read_tokens, Some(10));
    assert_eq!(resp.usage.reasoning_tokens, Some(5));
    assert_eq!(resp.usage.total_tokens, 150);
}

#[test]
fn parse_response_handles_tool_calls() {
    let adapter = OpenAiAdapter::new("test-key".into());
    let response_json = json!({
        "id": "resp_tool123",
        "output": [
            {
                "type": "message",
                "content": [
                    { "type": "output_text", "text": "Let me search for that." }
                ]
            },
            {
                "type": "function_call",
                "id": "fc_001",
                "name": "search",
                "arguments": "{\"query\": \"rust programming\"}"
            }
        ],
        "usage": {
            "input_tokens": 80,
            "output_tokens": 40
        },
        "model": "gpt-4o",
        "status": "completed"
    });

    let resp = adapter.parse_response(response_json).unwrap();
    assert_eq!(resp.id, "resp_tool123");
    assert_eq!(resp.text, "Let me search for that.");
    assert_eq!(resp.finish_reason, FinishReason::ToolUse);
    assert_eq!(resp.tool_calls.len(), 1);
    assert_eq!(resp.tool_calls[0].id, "fc_001");
    assert_eq!(resp.tool_calls[0].name, "search");
    assert_eq!(resp.tool_calls[0].arguments["query"], "rust programming");
}

#[test]
fn parse_response_handles_incomplete_status() {
    let adapter = OpenAiAdapter::new("test-key".into());
    let response_json = json!({
        "id": "resp_inc",
        "output": [
            {
                "type": "message",
                "content": [
                    { "type": "output_text", "text": "Partial response..." }
                ]
            }
        ],
        "usage": {
            "input_tokens": 50,
            "output_tokens": 4096
        },
        "model": "gpt-4o",
        "status": "incomplete"
    });

    let resp = adapter.parse_response(response_json).unwrap();
    assert_eq!(resp.finish_reason, FinishReason::MaxTokens);
    assert_eq!(resp.text, "Partial response...");
}

#[test]
fn with_base_url_sets_custom_url() {
    let adapter = OpenAiAdapter::new("key".into())
        .with_base_url("https://custom.api.com".into());
    assert_eq!(adapter.base_url, "https://custom.api.com");
}

#[test]
fn error_mapping_429_rate_limited() {
    let err = map_error(
        reqwest::StatusCode::TOO_MANY_REQUESTS,
        r#"{"error": {"message": "rate limited", "retry_after": 3.0}}"#,
    );
    assert!(matches!(err, AttractorError::RateLimited { retry_after_ms: 3000, .. }));
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
fn build_request_body_without_optional_fields() {
    let adapter = OpenAiAdapter::new("test-key".into());
    let req = Request {
        model: "gpt-4o".into(),
        messages: vec![Message::user("Hi")],
        tools: vec![],
        tool_choice: None,
        max_tokens: None,
        temperature: None,
        stop_sequences: vec![],
        reasoning_effort: None,
        provider: None,
        provider_options: None,
    };

    let body = adapter.build_request_body(&req);

    // Should have model and input, but not max_output_tokens, temperature, tools, or reasoning
    assert_eq!(body["model"], "gpt-4o");
    assert!(body["input"].is_array());
    assert!(body.get("max_output_tokens").is_none() || body["max_output_tokens"].is_null());
    assert!(body.get("temperature").is_none() || body["temperature"].is_null());
    assert!(body.get("tools").is_none() || body["tools"].is_null());
    assert!(body.get("reasoning").is_none() || body["reasoning"].is_null());
}
