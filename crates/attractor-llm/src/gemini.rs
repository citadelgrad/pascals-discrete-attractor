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
// GeminiAdapter
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct GeminiAdapter {
    api_key: String,
    client: reqwest::Client,
    base_url: String,
    default_model: String,
}

impl GeminiAdapter {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            client: reqwest::Client::new(),
            base_url: "https://generativelanguage.googleapis.com/v1beta".to_string(),
            default_model: "gemini-2.5-pro".to_string(),
        }
    }

    pub fn with_base_url(mut self, url: String) -> Self {
        self.base_url = url;
        self
    }

    pub fn from_env() -> Result<Self, AttractorError> {
        let key = std::env::var("GOOGLE_API_KEY")
            .or_else(|_| std::env::var("GEMINI_API_KEY"))
            .map_err(|_| AttractorError::AuthError {
                provider: "google".into(),
            })?;
        Ok(Self::new(key))
    }

    fn build_request_body(&self, request: &Request) -> serde_json::Value {
        // 1. Extract system messages into systemInstruction
        let system_texts: Vec<String> = request
            .messages
            .iter()
            .filter(|m| m.role == Role::System)
            .flat_map(|m| {
                m.content.iter().filter_map(|p| match p {
                    ContentPart::Text { text } => Some(text.clone()),
                    _ => None,
                })
            })
            .collect();

        // 2. Convert non-system messages to contents
        let contents: Vec<serde_json::Value> = request
            .messages
            .iter()
            .filter(|m| m.role != Role::System)
            .map(convert_message)
            .collect();

        // 3. Build body
        let mut body = json!({
            "contents": contents,
        });

        if !system_texts.is_empty() {
            let parts: Vec<serde_json::Value> =
                system_texts.iter().map(|t| json!({ "text": t })).collect();
            body["systemInstruction"] = json!({ "parts": parts });
        }

        // 4. Tools (functionDeclarations)
        if !request.tools.is_empty() {
            let declarations: Vec<serde_json::Value> =
                request.tools.iter().map(convert_tool_definition).collect();
            body["tools"] = json!([{ "functionDeclarations": declarations }]);
        }

        // 5. Generation config
        let mut gen_config = json!({});
        if let Some(max_tokens) = request.max_tokens {
            gen_config["maxOutputTokens"] = json!(max_tokens);
        }
        if let Some(temp) = request.temperature {
            gen_config["temperature"] = json!(temp);
        }
        if !request.stop_sequences.is_empty() {
            gen_config["stopSequences"] = json!(request.stop_sequences);
        }
        if gen_config.as_object().is_some_and(|o| !o.is_empty()) {
            body["generationConfig"] = gen_config;
        }

        body
    }

    fn parse_response(&self, json: serde_json::Value) -> Result<Response, AttractorError> {
        let candidates =
            json["candidates"]
                .as_array()
                .ok_or_else(|| AttractorError::ProviderError {
                    provider: "google".into(),
                    status: 0,
                    message: "Missing candidates in response".into(),
                    retryable: false,
                })?;

        let candidate = candidates
            .first()
            .ok_or_else(|| AttractorError::ProviderError {
                provider: "google".into(),
                status: 0,
                message: "Empty candidates array".into(),
                retryable: false,
            })?;

        // Parse finish reason
        let finish_reason = match candidate["finishReason"].as_str() {
            Some("STOP") => FinishReason::EndTurn,
            Some("MAX_TOKENS") => FinishReason::MaxTokens,
            Some("SAFETY") => FinishReason::EndTurn,
            Some("STOP_SEQUENCE") => FinishReason::StopSequence,
            _ => FinishReason::EndTurn,
        };

        // Parse content parts
        let mut text_parts: Vec<String> = Vec::new();
        let mut tool_calls: Vec<ToolCallResult> = Vec::new();

        if let Some(parts) = candidate["content"]["parts"].as_array() {
            for part in parts {
                if let Some(text) = part["text"].as_str() {
                    text_parts.push(text.to_string());
                }
                if let Some(fc) = part.get("functionCall") {
                    let name = fc["name"].as_str().unwrap_or("").to_string();
                    let args = fc["args"].clone();
                    tool_calls.push(ToolCallResult {
                        id: uuid::Uuid::new_v4().to_string(),
                        name,
                        arguments: args,
                    });
                }
            }
        }

        // Parse usage
        let usage_meta = &json["usageMetadata"];
        let input_tokens = usage_meta["promptTokenCount"].as_u64().unwrap_or(0);
        let output_tokens = usage_meta["candidatesTokenCount"].as_u64().unwrap_or(0);
        let total_tokens = usage_meta["totalTokenCount"]
            .as_u64()
            .unwrap_or(input_tokens + output_tokens);

        let usage = Usage {
            input_tokens,
            output_tokens,
            reasoning_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            total_tokens,
        };

        // Determine finish reason override for tool calls
        let final_finish_reason = if !tool_calls.is_empty() {
            FinishReason::ToolUse
        } else {
            finish_reason
        };

        Ok(Response {
            id: uuid::Uuid::new_v4().to_string(),
            text: text_parts.join(""),
            tool_calls,
            reasoning: None,
            usage,
            model: String::new(),
            finish_reason: final_finish_reason,
        })
    }
}

// ---------------------------------------------------------------------------
// Message conversion helpers
// ---------------------------------------------------------------------------

fn convert_message(msg: &Message) -> serde_json::Value {
    let role = match msg.role {
        Role::User | Role::Developer => "user",
        Role::Assistant => "model",
        Role::Tool => "user",
        Role::System => "user", // should not happen, filtered above
    };

    let parts: Vec<serde_json::Value> = msg
        .content
        .iter()
        .map(|p| match p {
            ContentPart::Text { text } => json!({ "text": text }),
            ContentPart::ToolCall {
                name, arguments, ..
            } => json!({
                "functionCall": {
                    "name": name,
                    "args": arguments
                }
            }),
            ContentPart::ToolResult {
                tool_name,
                content,
                ..
            } => json!({
                "functionResponse": {
                    "name": tool_name,
                    "response": {
                        "content": content
                    }
                }
            }),
            ContentPart::Image { url, .. } => {
                if let Some(url) = url {
                    json!({ "text": format!("[image: {}]", url) })
                } else {
                    json!({ "text": "[unsupported image content]" })
                }
            }
            ContentPart::Thinking { text, .. } => json!({ "text": text }),
            ContentPart::RedactedThinking { .. } => json!({ "text": "[redacted]" }),
            ContentPart::Audio { .. } | ContentPart::Document { .. } => {
                json!({ "text": "[unsupported content type]" })
            }
        })
        .collect();

    json!({
        "role": role,
        "parts": parts
    })
}

fn convert_tool_definition(tool: &ToolDefinition) -> serde_json::Value {
    json!({
        "name": tool.name,
        "description": tool.description,
        "parameters": tool.parameters
    })
}

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

fn map_error(status: reqwest::StatusCode, body: &str) -> AttractorError {
    let status_u16 = status.as_u16();
    match status_u16 {
        429 => AttractorError::RateLimited {
            provider: "google".into(),
            retry_after_ms: 1000,
        },
        401 | 403 => AttractorError::AuthError {
            provider: "google".into(),
        },
        400 => AttractorError::ProviderError {
            provider: "google".into(),
            status: 400,
            message: extract_error_message(body),
            retryable: false,
        },
        500 | 503 => AttractorError::ProviderError {
            provider: "google".into(),
            status: status_u16,
            message: extract_error_message(body),
            retryable: true,
        },
        _ => AttractorError::ProviderError {
            provider: "google".into(),
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
impl ProviderAdapter for GeminiAdapter {
    async fn complete(&self, request: &Request) -> Result<Response, AttractorError> {
        let body = self.build_request_body(request);
        let model = if request.model.is_empty() {
            &self.default_model
        } else {
            &request.model
        };

        let url = format!(
            "{}/models/{}:generateContent?key={}",
            self.base_url, model, self.api_key
        );

        let resp = self
            .client
            .post(&url)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| AttractorError::ProviderError {
                provider: "google".into(),
                status: 0,
                message: e.to_string(),
                retryable: true,
            })?;

        let status = resp.status();
        let response_body = resp
            .text()
            .await
            .map_err(|e| AttractorError::ProviderError {
                provider: "google".into(),
                status: 0,
                message: e.to_string(),
                retryable: true,
            })?;

        if !status.is_success() {
            return Err(map_error(status, &response_body));
        }

        let json: serde_json::Value =
            serde_json::from_str(&response_body).map_err(|e| AttractorError::ProviderError {
                provider: "google".into(),
                status: status.as_u16(),
                message: format!("Failed to parse response JSON: {e}"),
                retryable: false,
            })?;

        let mut response = self.parse_response(json)?;
        response.model = model.to_string();
        Ok(response)
    }

    fn stream(&self, _request: &Request) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + '_>> {
        Box::pin(tokio_stream::empty::<StreamEvent>())
    }

    fn name(&self) -> &str {
        "google"
    }

    fn default_model(&self) -> &str {
        &self.default_model
    }

    fn supports_tools(&self) -> bool {
        true
    }

    fn supports_streaming(&self) -> bool {
        false
    }

    fn supports_reasoning(&self) -> bool {
        true
    }

    fn context_window_size(&self) -> usize {
        1_000_000
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DynProvider, Message, ToolDefinition};

    fn make_basic_request() -> Request {
        Request {
            model: "gemini-2.5-pro".into(),
            messages: vec![Message::system("You are helpful."), Message::user("Hello")],
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
        assert!(adapter
            .base_url
            .contains("generativelanguage.googleapis.com"));
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
        let adapter =
            GeminiAdapter::new("key".into()).with_base_url("https://custom.example.com".into());
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
}
