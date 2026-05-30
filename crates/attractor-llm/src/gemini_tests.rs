use super::*;
use crate::{DynProvider, Message, ToolDefinition};

fn make_basic_request() -> Request {
    Request {
        model: "gemini-2.5-pro".into(),
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
        provider: Some("google".into()),
        provider_options: None,
    }
}

// Test 1: new() constructor sets api_key correctly
#[test]
fn new_sets_api_key() {
    let adapter = GeminiAdapter::new("test-google-key".into());
    assert_eq!(adapter.api_key, "test-google-key");
    assert_eq!(adapter.default_model, "gemini-2.5-pro");
    assert!(adapter.base_url.contains("generativelanguage.googleapis.com"));
}

// Test 2: from_env without any key returns Err
// Note: This test must run alone to avoid env var races with parallel tests.
// We only test the error case since it's deterministic (we remove both vars).
#[test]
fn from_env_without_key_returns_error() {
    // This is inherently racy with parallel tests but the error case is
    // safe: if another test sets GOOGLE_API_KEY concurrently, from_env
    // would succeed and we'd get a false positive. Use a unique check.
    let google_was_set = std::env::var("GOOGLE_API_KEY").is_ok();
    let gemini_was_set = std::env::var("GEMINI_API_KEY").is_ok();

    if google_was_set || gemini_was_set {
        // Another test has the env var set; skip this test silently
        return;
    }

    let result = GeminiAdapter::from_env();
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(matches!(err, AttractorError::AuthError { provider } if provider == "google"));
}

// Test 3: build_request_body with system message extracts systemInstruction
#[test]
fn build_request_body_extracts_system_instruction() {
    let adapter = GeminiAdapter::new("test-key".into());
    let req = make_basic_request();
    let body = adapter.build_request_body(&req);

    // systemInstruction should be present
    let sys = &body["systemInstruction"];
    assert!(sys.is_object(), "systemInstruction should be an object");
    let parts = sys["parts"].as_array().expect("parts should be an array");
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0]["text"], "You are helpful.");

    // contents should only contain the user message (no system)
    let contents = body["contents"].as_array().unwrap();
    assert_eq!(contents.len(), 1);
    assert_eq!(contents[0]["role"], "user");
    let msg_parts = contents[0]["parts"].as_array().unwrap();
    assert_eq!(msg_parts[0]["text"], "Hello");
}

// Test 4: parse_response handles candidates correctly
#[test]
fn parse_response_handles_text_response() {
    let adapter = GeminiAdapter::new("test-key".into());
    let json = json!({
        "candidates": [{
            "content": {
                "parts": [{ "text": "Hello there!" }],
                "role": "model"
            },
            "finishReason": "STOP"
        }],
        "usageMetadata": {
            "promptTokenCount": 10,
            "candidatesTokenCount": 20,
            "totalTokenCount": 30
        }
    });

    let resp = adapter.parse_response(json).unwrap();
    assert_eq!(resp.text, "Hello there!");
    assert_eq!(resp.finish_reason, FinishReason::EndTurn);
    assert_eq!(resp.usage.input_tokens, 10);
    assert_eq!(resp.usage.output_tokens, 20);
    assert_eq!(resp.usage.total_tokens, 30);
    assert!(resp.tool_calls.is_empty());
}

// Test 5: parse_response handles function calls
#[test]
fn parse_response_handles_function_calls() {
    let adapter = GeminiAdapter::new("test-key".into());
    let json = json!({
        "candidates": [{
            "content": {
                "parts": [
                    { "text": "Let me search." },
                    { "functionCall": { "name": "search", "args": { "query": "rust" } } }
                ],
                "role": "model"
            },
            "finishReason": "STOP"
        }],
        "usageMetadata": {
            "promptTokenCount": 15,
            "candidatesTokenCount": 25,
            "totalTokenCount": 40
        }
    });

    let resp = adapter.parse_response(json).unwrap();
    assert_eq!(resp.text, "Let me search.");
    assert_eq!(resp.finish_reason, FinishReason::ToolUse);
    assert_eq!(resp.tool_calls.len(), 1);
    assert_eq!(resp.tool_calls[0].name, "search");
    assert_eq!(resp.tool_calls[0].arguments["query"], "rust");
    assert!(!resp.tool_calls[0].id.is_empty());
}

// Test 6: build_request_body includes tools as functionDeclarations
#[test]
fn build_request_body_includes_tools() {
    let adapter = GeminiAdapter::new("test-key".into());
    let mut req = make_basic_request();
    req.tools = vec![ToolDefinition {
        name: "search".into(),
        description: "Search the web".into(),
        parameters: json!({"type": "object", "properties": {"query": {"type": "string"}}}),
    }];

    let body = adapter.build_request_body(&req);

    let tools = body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    let decls = tools[0]["functionDeclarations"].as_array().unwrap();
    assert_eq!(decls.len(), 1);
    assert_eq!(decls[0]["name"], "search");
    assert_eq!(decls[0]["description"], "Search the web");
}

// Test 7: build_request_body includes generationConfig
#[test]
fn build_request_body_includes_generation_config() {
    let adapter = GeminiAdapter::new("test-key".into());
    let mut req = make_basic_request();
    req.max_tokens = Some(2048);
    req.temperature = Some(0.5);
    req.stop_sequences = vec!["STOP".into()];

    let body = adapter.build_request_body(&req);

    let config = &body["generationConfig"];
    assert_eq!(config["maxOutputTokens"], 2048);
    assert_eq!(config["temperature"], 0.5);
    let stops = config["stopSequences"].as_array().unwrap();
    assert_eq!(stops.len(), 1);
    assert_eq!(stops[0], "STOP");
}

// Test 8: with_base_url overrides the default URL
#[test]
fn with_base_url_overrides_default() {
    let adapter = GeminiAdapter::new("key".into())
        .with_base_url("https://custom.example.com".into());
    assert_eq!(adapter.base_url, "https://custom.example.com");
}

// Test 9: dyn_provider wrapping works
#[test]
fn dyn_provider_wrapping_works() {
    let adapter = GeminiAdapter::new("test-key".into());
    let provider = DynProvider::new(adapter);
    assert_eq!(provider.name(), "google");
    assert_eq!(provider.default_model(), "gemini-2.5-pro");
    assert!(provider.supports_tools());
    assert!(!provider.supports_streaming());
    assert!(provider.supports_reasoning());
    assert_eq!(provider.context_window_size(), 1_000_000);
}

// Test 10: parse_response with MAX_TOKENS finish reason
#[test]
fn parse_response_max_tokens_finish_reason() {
    let adapter = GeminiAdapter::new("test-key".into());
    let json = json!({
        "candidates": [{
            "content": {
                "parts": [{ "text": "Truncated output" }],
                "role": "model"
            },
            "finishReason": "MAX_TOKENS"
        }],
        "usageMetadata": {
            "promptTokenCount": 5,
            "candidatesTokenCount": 100,
            "totalTokenCount": 105
        }
    });

    let resp = adapter.parse_response(json).unwrap();
    assert_eq!(resp.finish_reason, FinishReason::MaxTokens);
}

// Test 11: error mapping
#[test]
fn error_mapping_429_rate_limited() {
    let err = map_error(
        reqwest::StatusCode::TOO_MANY_REQUESTS,
        r#"{"error": {"message": "rate limited"}}"#,
    );
    assert!(matches!(err, AttractorError::RateLimited { .. }));
}

#[test]
fn error_mapping_401_auth() {
    let err = map_error(
        reqwest::StatusCode::UNAUTHORIZED,
        r#"{"error": {"message": "invalid key"}}"#,
    );
    assert!(matches!(err, AttractorError::AuthError { .. }));
}
