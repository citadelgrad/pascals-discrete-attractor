use async_trait::async_trait;
use futures_core::Stream;
use serde_json::json;
use std::pin::Pin;

use crate::{
    ContentPart, FinishReason, Message, ProviderAdapter, Request, Response, Role, StreamEvent,
    ToolCallResult, ToolDefinition, Usage,
};
use attractor_types::AttractorError;

// ---------------------------------------------------------------------------
// AnthropicAdapter
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct AnthropicAdapter {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl AnthropicAdapter {
    pub fn new(api_key: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            base_url: "https://api.anthropic.com".to_string(),
        }
    }

    pub fn with_base_url(mut self, url: String) -> Self {
        self.base_url = url;
        self
    }

    pub fn from_env() -> Result<Self, AttractorError> {
        let key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| AttractorError::AuthError {
            provider: "anthropic".into(),
        })?;
        Ok(Self::new(key))
    }
}

// ---------------------------------------------------------------------------
// Request translation (Unified → Anthropic JSON)
// ---------------------------------------------------------------------------

fn build_request_body(request: &Request) -> serde_json::Value {
    // 1. Extract system messages
    let system_parts: Vec<serde_json::Value> = request
        .messages
        .iter()
        .filter(|m| m.role == Role::System)
        .flat_map(|m| {
            m.content.iter().filter_map(|p| match p {
                ContentPart::Text { text } => Some(json!({
                    "type": "text",
                    "text": text,
                    "cache_control": { "type": "ephemeral" }
                })),
                _ => None,
            })
        })
        .collect();

    // 2. Convert non-system messages
    let messages: Vec<serde_json::Value> = convert_messages(
        &request
            .messages
            .iter()
            .filter(|m| m.role != Role::System)
            .cloned()
            .collect::<Vec<_>>(),
    );

    // 3. Build body
    let mut body = json!({
        "model": request.model,
        "max_tokens": request.max_tokens.unwrap_or(4096),
        "messages": messages,
    });

    if !system_parts.is_empty() {
        body["system"] = json!(system_parts);
    }

    // 4. Convert tools
    if !request.tools.is_empty() {
        body["tools"] = json!(request
            .tools
            .iter()
            .map(convert_tool_definition)
            .collect::<Vec<_>>());
    }

    // 5. Stop sequences
    if !request.stop_sequences.is_empty() {
        body["stop_sequences"] = json!(request.stop_sequences);
    }

    // 6. Temperature
    if let Some(temp) = request.temperature {
        body["temperature"] = json!(temp);
    }

    body
}

fn convert_messages(messages: &[Message]) -> Vec<serde_json::Value> {
    let mut result: Vec<serde_json::Value> = Vec::new();

    for (i, msg) in messages.iter().enumerate() {
        match msg.role {
            Role::User => {
                let mut content = convert_content_parts(&msg.content);
                // Inject cache_control on last user message
                if is_last_user_message(messages, i) {
                    inject_cache_control_on_last_part(&mut content);
                }
                result.push(json!({ "role": "user", "content": content }));
            }
            Role::Assistant => {
                let content = convert_content_parts(&msg.content);
                result.push(json!({ "role": "assistant", "content": content }));
            }
            Role::Tool => {
                // Tool results must be sent as user messages with tool_result blocks
                let content = convert_content_parts(&msg.content);
                // Merge into previous user message or create a new user message
                if let Some(last) = result.last_mut() {
                    if last["role"] == "user" {
                        if let Some(arr) = last["content"].as_array_mut() {
                            arr.extend(content);
                            continue;
                        }
                    }
                }
                result.push(json!({ "role": "user", "content": content }));
            }
            Role::System | Role::Developer => {
                // System messages handled separately; Developer mapped to user
                if msg.role == Role::Developer {
                    let content = convert_content_parts(&msg.content);
                    result.push(json!({ "role": "user", "content": content }));
                }
            }
        }
    }

    result
}

fn is_last_user_message(messages: &[Message], index: usize) -> bool {
    for msg in messages[index + 1..].iter() {
        if msg.role == Role::User {
            return false;
        }
    }
    messages[index].role == Role::User
}

fn inject_cache_control_on_last_part(content: &mut [serde_json::Value]) {
    if let Some(last) = content.last_mut() {
        last["cache_control"] = json!({ "type": "ephemeral" });
    }
}

fn convert_content_parts(parts: &[ContentPart]) -> Vec<serde_json::Value> {
    parts
        .iter()
        .filter_map(|p| {
            let v = match p {
                ContentPart::Text { text } => json!({
                    "type": "text",
                    "text": text
                }),
                ContentPart::ToolCall {
                    id,
                    name,
                    arguments,
                } => json!({
                    "type": "tool_use",
                    "id": id,
                    "name": name,
                    "input": arguments
                }),
                ContentPart::ToolResult {
                    tool_call_id,
                    content,
                    is_error,
                    ..
                } => {
                    let mut v = json!({
                        "type": "tool_result",
                        "tool_use_id": tool_call_id,
                        "content": content
                    });
                    if *is_error {
                        v["is_error"] = json!(true);
                    }
                    v
                }
                // Thinking blocks without a signature must be omitted —
                // the API rejects thinking blocks that lack a signature field.
                ContentPart::Thinking { signature: None, .. } => return None,
                ContentPart::Thinking {
                    text,
                    signature: Some(sig),
                } => json!({
                    "type": "thinking",
                    "thinking": text,
                    "signature": sig
                }),
                ContentPart::RedactedThinking { data } => json!({
                    "type": "redacted_thinking",
                    "data": data
                }),
                ContentPart::Image { url, .. } => {
                    if let Some(url) = url {
                        json!({
                            "type": "image",
                            "source": {
                                "type": "url",
                                "url": url
                            }
                        })
                    } else {
                        json!({"type": "text", "text": "[unsupported image content]"})
                    }
                }
                ContentPart::Audio { .. } | ContentPart::Document { .. } => {
                    json!({"type": "text", "text": "[unsupported content type]"})
                }
            };
            Some(v)
        })
        .collect()
}

fn convert_tool_definition(tool: &ToolDefinition) -> serde_json::Value {
    json!({
        "name": tool.name,
        "description": tool.description,
        "input_schema": tool.parameters
    })
}

// ---------------------------------------------------------------------------
// Response translation (Anthropic JSON → Unified Response)
// ---------------------------------------------------------------------------

fn parse_response(body: &serde_json::Value) -> Result<Response, AttractorError> {
    let id = body["id"].as_str().unwrap_or("").to_string();
    let model = body["model"].as_str().unwrap_or("").to_string();

    let stop_reason = match body["stop_reason"].as_str() {
        Some("end_turn") => FinishReason::EndTurn,
        Some("max_tokens") => FinishReason::MaxTokens,
        Some("stop_sequence") => FinishReason::StopSequence,
        Some("tool_use") => FinishReason::ToolUse,
        _ => FinishReason::EndTurn,
    };

    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCallResult> = Vec::new();
    let mut reasoning: Option<String> = None;

    if let Some(content) = body["content"].as_array() {
        for block in content {
            match block["type"].as_str() {
                Some("text") => {
                    if let Some(t) = block["text"].as_str() {
                        text_parts.push(t.to_string());
                    }
                }
                Some("tool_use") => {
                    tool_calls.push(ToolCallResult {
                        id: block["id"].as_str().unwrap_or("").to_string(),
                        name: block["name"].as_str().unwrap_or("").to_string(),
                        arguments: block["input"].clone(),
                    });
                }
                Some("thinking") => {
                    if let Some(t) = block["thinking"].as_str() {
                        reasoning = Some(t.to_string());
                    }
                }
                _ => {}
            }
        }
    }

    // Parse usage
    let usage_obj = &body["usage"];
    let input_tokens = usage_obj["input_tokens"].as_u64().unwrap_or(0);
    let output_tokens = usage_obj["output_tokens"].as_u64().unwrap_or(0);
    let cache_creation = usage_obj["cache_creation_input_tokens"].as_u64();
    let cache_read = usage_obj["cache_read_input_tokens"].as_u64();

    let usage = Usage {
        input_tokens,
        output_tokens,
        reasoning_tokens: None,
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_creation,
        total_tokens: input_tokens + output_tokens,
    };

    Ok(Response {
        id,
        text: text_parts.join(""),
        tool_calls,
        reasoning,
        usage,
        model,
        finish_reason: stop_reason,
    })
}

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

fn map_error(status: reqwest::StatusCode, body: &str) -> AttractorError {
    let status_u16 = status.as_u16();
    match status_u16 {
        429 => {
            // Try to extract retry-after from the error body
            let retry_ms = serde_json::from_str::<serde_json::Value>(body)
                .ok()
                .and_then(|v| v["error"]["retry_after"].as_f64())
                .map(|s| (s * 1000.0) as u64)
                .unwrap_or(1000);
            AttractorError::RateLimited {
                provider: "anthropic".into(),
                retry_after_ms: retry_ms,
            }
        }
        401 => AttractorError::AuthError {
            provider: "anthropic".into(),
        },
        400 => AttractorError::ProviderError {
            provider: "anthropic".into(),
            status: 400,
            message: extract_error_message(body),
            retryable: false,
        },
        500 | 529 => AttractorError::ProviderError {
            provider: "anthropic".into(),
            status: status_u16,
            message: extract_error_message(body),
            retryable: true,
        },
        _ => AttractorError::ProviderError {
            provider: "anthropic".into(),
            status: status_u16,
            message: extract_error_message(body),
            retryable: false,
        },
    }
}

fn extract_error_message(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(String::from))
        .unwrap_or_else(|| body.to_string())
}

// ---------------------------------------------------------------------------
// ProviderAdapter implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl ProviderAdapter for AnthropicAdapter {
    async fn complete(&self, request: &Request) -> Result<Response, AttractorError> {
        let body = build_request_body(request);

        let resp = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| AttractorError::ProviderError {
                provider: "anthropic".into(),
                status: 0,
                message: e.to_string(),
                retryable: true,
            })?;

        let status = resp.status();
        let response_body = resp
            .text()
            .await
            .map_err(|e| AttractorError::ProviderError {
                provider: "anthropic".into(),
                status: 0,
                message: e.to_string(),
                retryable: true,
            })?;

        if !status.is_success() {
            return Err(map_error(status, &response_body));
        }

        let json: serde_json::Value =
            serde_json::from_str(&response_body).map_err(|e| AttractorError::ProviderError {
                provider: "anthropic".into(),
                status: status.as_u16(),
                message: format!("Failed to parse response JSON: {e}"),
                retryable: false,
            })?;

        parse_response(&json)
    }

    fn stream(&self, _request: &Request) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + '_>> {
        Box::pin(tokio_stream::empty::<StreamEvent>())
    }

    fn name(&self) -> &str {
        "anthropic"
    }

    fn default_model(&self) -> &str {
        "claude-sonnet-4-5-20250929"
    }

    fn supports_tools(&self) -> bool {
        true
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn supports_reasoning(&self) -> bool {
        true
    }

    fn context_window_size(&self) -> usize {
        200_000
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContentPart, DynProvider, Message, ToolDefinition};

    fn make_basic_request() -> Request {
        Request {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![Message::system("You are helpful."), Message::user("Hello")],
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
        let system = body["system"]
            .as_array()
            .expect("system should be an array");
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
        assert!(matches!(
            err,
            AttractorError::RateLimited {
                retry_after_ms: 2500,
                ..
            }
        ));
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
            AttractorError::ProviderError {
                retryable, status, ..
            } => {
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
            AttractorError::ProviderError {
                retryable, status, ..
            } => {
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
            Message::tool_result("tc_1", "search", "result data", false),
        ];

        let converted = convert_messages(&messages);
        // Tool result should create a new user message (since previous is assistant)
        assert_eq!(converted.len(), 3);
        assert_eq!(converted[2]["role"], "user");
        let content = converted[2]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "tool_result");
        assert_eq!(content[0]["tool_use_id"], "tc_1");
    }
}
