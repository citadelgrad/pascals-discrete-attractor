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
            let parts: Vec<serde_json::Value> = system_texts
                .iter()
                .map(|t| json!({ "text": t }))
                .collect();
            body["systemInstruction"] = json!({ "parts": parts });
        }

        // 4. Tools (functionDeclarations)
        if !request.tools.is_empty() {
            let declarations: Vec<serde_json::Value> = request
                .tools
                .iter()
                .map(convert_tool_definition)
                .collect();
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
        let candidates = json["candidates"]
            .as_array()
            .ok_or_else(|| AttractorError::ProviderError {
                provider: "google".into(),
                status: 0,
                message: "Missing candidates in response".into(),
                retryable: false,
            })?;

        let candidate = candidates.first().ok_or_else(|| AttractorError::ProviderError {
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
                tool_call_id,
                content,
                ..
            } => json!({
                "functionResponse": {
                    "name": tool_call_id,
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
        let response_body = resp.text().await.map_err(|e| AttractorError::ProviderError {
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

    fn stream(
        &self,
        _request: &Request,
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + '_>> {
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
#[path = "gemini_tests.rs"]
mod tests;
