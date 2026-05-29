use async_trait::async_trait;
use futures_core::Stream;
use serde_json::json;
use std::pin::Pin;

use crate::{
    ContentPart, FinishReason, Message, ProviderAdapter, Request, Response, Role, StreamEvent,
    ToolCallResult, Usage,
};
use attractor_types::AttractorError;

// ---------------------------------------------------------------------------
// OpenAiAdapter
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct OpenAiAdapter {
    api_key: String,
    client: reqwest::Client,
    base_url: String,
    default_model: String,
}

impl OpenAiAdapter {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            client: reqwest::Client::new(),
            base_url: "https://api.openai.com".to_string(),
            default_model: "gpt-4o".to_string(),
        }
    }

    pub fn from_env() -> Result<Self, AttractorError> {
        let key = std::env::var("OPENAI_API_KEY").map_err(|_| AttractorError::AuthError {
            provider: "openai".into(),
        })?;
        Ok(Self::new(key))
    }

    pub fn with_base_url(mut self, url: String) -> Self {
        self.base_url = url;
        self
    }

    fn build_request_body(&self, request: &Request) -> serde_json::Value {
        // 1. Convert messages to input array
        let input: Vec<serde_json::Value> = request
            .messages
            .iter()
            .map(convert_message)
            .collect();

        // 2. Build body
        let mut body = json!({
            "model": request.model,
            "input": input,
        });

        // 3. max_tokens -> max_output_tokens
        if let Some(max_tokens) = request.max_tokens {
            body["max_output_tokens"] = json!(max_tokens);
        }

        // 4. Temperature
        if let Some(temp) = request.temperature {
            body["temperature"] = json!(temp);
        }

        // 5. Tools
        if !request.tools.is_empty() {
            let tools: Vec<serde_json::Value> = request
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.parameters,
                        }
                    })
                })
                .collect();
            body["tools"] = json!(tools);
        }

        // 6. Reasoning effort
        if let Some(ref effort) = request.reasoning_effort {
            let effort_str = match effort {
                crate::ReasoningEffort::Low => "low",
                crate::ReasoningEffort::Medium => "medium",
                crate::ReasoningEffort::High => "high",
            };
            body["reasoning"] = json!({ "effort": effort_str });
        }

        body
    }

    fn parse_response(&self, body: serde_json::Value) -> Result<Response, AttractorError> {
        let id = body["id"].as_str().unwrap_or("").to_string();
        let model = body["model"].as_str().unwrap_or("").to_string();

        // Map status to finish reason
        let finish_reason = match body["status"].as_str() {
            Some("completed") => {
                // Check if there are function_call items in output — that means ToolUse
                let has_tool_calls = body["output"]
                    .as_array()
                    .map(|arr| arr.iter().any(|item| item["type"] == "function_call"))
                    .unwrap_or(false);
                if has_tool_calls {
                    FinishReason::ToolUse
                } else {
                    FinishReason::EndTurn
                }
            }
            Some("incomplete") => FinishReason::MaxTokens,
            _ => FinishReason::EndTurn,
        };

        // Extract text and tool calls from output array
        let mut text_parts: Vec<String> = Vec::new();
        let mut tool_calls: Vec<ToolCallResult> = Vec::new();

        if let Some(output) = body["output"].as_array() {
            for item in output {
                match item["type"].as_str() {
                    Some("message") => {
                        if let Some(content) = item["content"].as_array() {
                            for block in content {
                                if block["type"] == "output_text" {
                                    if let Some(t) = block["text"].as_str() {
                                        text_parts.push(t.to_string());
                                    }
                                }
                            }
                        }
                    }
                    Some("function_call") => {
                        let call_id = item["id"].as_str().unwrap_or("").to_string();
                        let name = item["name"].as_str().unwrap_or("").to_string();
                        let arguments_str = item["arguments"].as_str().unwrap_or("{}");
                        let arguments: serde_json::Value =
                            serde_json::from_str(arguments_str).unwrap_or(json!({}));
                        tool_calls.push(ToolCallResult {
                            id: call_id,
                            name,
                            arguments,
                        });
                    }
                    _ => {}
                }
            }
        }

        // Parse usage
        let usage_obj = &body["usage"];
        let input_tokens = usage_obj["input_tokens"].as_u64().unwrap_or(0);
        let output_tokens = usage_obj["output_tokens"].as_u64().unwrap_or(0);
        let cached_tokens = usage_obj["input_tokens_details"]["cached_tokens"].as_u64();
        let reasoning_tokens = usage_obj["output_tokens_details"]["reasoning_tokens"].as_u64();

        let usage = Usage {
            input_tokens,
            output_tokens,
            reasoning_tokens,
            cache_read_tokens: cached_tokens,
            cache_write_tokens: None,
            total_tokens: input_tokens + output_tokens,
        };

        Ok(Response {
            id,
            text: text_parts.join(""),
            tool_calls,
            reasoning: None,
            usage,
            model,
            finish_reason,
        })
    }
}

// ---------------------------------------------------------------------------
// Message conversion helpers
// ---------------------------------------------------------------------------

fn convert_message(msg: &Message) -> serde_json::Value {
    let role = match msg.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
        Role::Developer => "developer",
    };

    // For tool messages, we need to include the tool_call_id
    if msg.role == Role::Tool {
        // Extract tool result content
        for part in &msg.content {
            if let ContentPart::ToolResult {
                tool_call_id,
                content,
                ..
            } = part
            {
                return json!({
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "content": content,
                });
            }
        }
    }

    // For assistant messages with tool calls, we need special handling
    if msg.role == Role::Assistant {
        let has_tool_calls = msg.content.iter().any(|p| matches!(p, ContentPart::ToolCall { .. }));
        if has_tool_calls {
            let mut text_parts = Vec::new();
            let mut tool_call_parts = Vec::new();

            for part in &msg.content {
                match part {
                    ContentPart::Text { text } => text_parts.push(text.clone()),
                    ContentPart::ToolCall {
                        id,
                        name,
                        arguments,
                    } => {
                        tool_call_parts.push(json!({
                            "type": "function_call",
                            "id": id,
                            "name": name,
                            "arguments": arguments.to_string(),
                        }));
                    }
                    _ => {}
                }
            }

            // Return as multiple output items in a simplified form
            let content_text = text_parts.join("");
            let mut result = json!({
                "role": "assistant",
                "content": content_text,
            });
            if !tool_call_parts.is_empty() {
                result["tool_calls"] = json!(tool_call_parts);
            }
            return result;
        }
    }

    // Default: extract text content
    let text = extract_text_content(&msg.content);
    json!({
        "role": role,
        "content": text,
    })
}

fn extract_text_content(parts: &[ContentPart]) -> String {
    parts
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

fn map_error(status: reqwest::StatusCode, body: &str) -> AttractorError {
    let status_u16 = status.as_u16();
    match status_u16 {
        429 => {
            let retry_ms = serde_json::from_str::<serde_json::Value>(body)
                .ok()
                .and_then(|v| v["error"]["retry_after"].as_f64())
                .map(|s| (s * 1000.0) as u64)
                .unwrap_or(1000);
            AttractorError::RateLimited {
                provider: "openai".into(),
                retry_after_ms: retry_ms,
            }
        }
        401 => AttractorError::AuthError {
            provider: "openai".into(),
        },
        400 => AttractorError::ProviderError {
            provider: "openai".into(),
            status: 400,
            message: extract_error_message(body),
            retryable: false,
        },
        500 | 502 | 503 => AttractorError::ProviderError {
            provider: "openai".into(),
            status: status_u16,
            message: extract_error_message(body),
            retryable: true,
        },
        _ => AttractorError::ProviderError {
            provider: "openai".into(),
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
impl ProviderAdapter for OpenAiAdapter {
    async fn complete(&self, request: &Request) -> Result<Response, AttractorError> {
        let body = self.build_request_body(request);

        let resp = self
            .client
            .post(format!("{}/v1/responses", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| AttractorError::ProviderError {
                provider: "openai".into(),
                status: 0,
                message: e.to_string(),
                retryable: true,
            })?;

        let status = resp.status();
        let response_body = resp.text().await.map_err(|e| AttractorError::ProviderError {
            provider: "openai".into(),
            status: 0,
            message: e.to_string(),
            retryable: true,
        })?;

        if !status.is_success() {
            return Err(map_error(status, &response_body));
        }

        let json: serde_json::Value =
            serde_json::from_str(&response_body).map_err(|e| AttractorError::ProviderError {
                provider: "openai".into(),
                status: status.as_u16(),
                message: format!("Failed to parse response JSON: {e}"),
                retryable: false,
            })?;

        self.parse_response(json)
    }

    fn stream(
        &self,
        _request: &Request,
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + '_>> {
        Box::pin(tokio_stream::empty::<StreamEvent>())
    }

    fn name(&self) -> &str {
        "openai"
    }

    fn default_model(&self) -> &str {
        &self.default_model
    }

    fn supports_tools(&self) -> bool {
        true
    }

    fn supports_streaming(&self) -> bool {
        false // Stub for now
    }

    fn supports_reasoning(&self) -> bool {
        true
    }

    fn context_window_size(&self) -> usize {
        128_000
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "openai_tests.rs"]
mod tests;
