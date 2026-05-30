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
        .map(|p| match p {
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
            ContentPart::Thinking { text, signature } => {
                let mut v = json!({
                    "type": "thinking",
                    "thinking": text
                });
                if let Some(sig) = signature {
                    v["signature"] = json!(sig);
                }
                v
            }
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
        let response_body = resp.text().await.map_err(|e| AttractorError::ProviderError {
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

    fn stream(
        &self,
        _request: &Request,
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + '_>> {
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
#[path = "anthropic_tests.rs"]
mod tests;
