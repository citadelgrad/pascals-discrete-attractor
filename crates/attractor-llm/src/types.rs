use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Role
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
    Developer,
}

// ---------------------------------------------------------------------------
// ContentPart
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    Image {
        url: Option<String>,
        data: Option<Vec<u8>>,
        media_type: Option<String>,
        detail: Option<String>,
    },
    Audio {
        url: Option<String>,
        data: Option<Vec<u8>>,
        media_type: Option<String>,
    },
    Document {
        url: Option<String>,
        data: Option<Vec<u8>>,
        media_type: Option<String>,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
    ToolResult {
        tool_call_id: String,
        /// The actual function/tool name, required by some providers (e.g. Gemini `functionResponse.name`).
        tool_name: String,
        content: String,
        is_error: bool,
    },
    Thinking {
        text: String,
        signature: Option<String>,
    },
    RedactedThinking {
        data: String,
    },
}

// ---------------------------------------------------------------------------
// Message
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentPart>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: vec![ContentPart::Text { text: text.into() }],
            name: None,
            tool_call_id: None,
        }
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentPart::Text { text: text.into() }],
            name: None,
            tool_call_id: None,
        }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![ContentPart::Text { text: text.into() }],
            name: None,
            tool_call_id: None,
        }
    }

    pub fn tool_result(
        id: impl Into<String>,
        tool_name: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        let id_str = id.into();
        Self {
            role: Role::Tool,
            content: vec![ContentPart::ToolResult {
                tool_call_id: id_str.clone(),
                tool_name: tool_name.into(),
                content: content.into(),
                is_error,
            }],
            name: None,
            tool_call_id: Some(id_str),
        }
    }
}

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,
    pub tool_choice: Option<ToolChoice>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    #[serde(default)]
    pub stop_sequences: Vec<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub provider: Option<String>,
    pub provider_options: Option<HashMap<String, serde_json::Value>>,
}

// ---------------------------------------------------------------------------
// Response
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: String,
    pub text: String,
    pub tool_calls: Vec<ToolCallResult>,
    pub reasoning: Option<String>,
    pub usage: Usage,
    pub model: String,
    pub finish_reason: FinishReason,
}

// ---------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub total_tokens: u64,
}

// ---------------------------------------------------------------------------
// FinishReason
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    EndTurn,
    MaxTokens,
    StopSequence,
    ToolUse,
    ContentFiltered,
}

// ---------------------------------------------------------------------------
// Tool types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallResult {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultData {
    pub tool_call_id: String,
    pub content: String,
    pub is_error: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Specific(String),
}

// ---------------------------------------------------------------------------
// ReasoningEffort
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
}

// ---------------------------------------------------------------------------
// StreamEvent
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum StreamEvent {
    ContentStart,
    ContentDelta {
        text: String,
    },
    ContentEnd,
    ToolCallStart {
        id: String,
        name: String,
    },
    ToolCallDelta {
        id: String,
        json_chunk: String,
    },
    ToolCallEnd {
        id: String,
    },
    ThinkingDelta {
        text: String,
    },
    MessageStart {
        id: String,
        model: String,
    },
    MessageEnd {
        usage: Usage,
        finish_reason: FinishReason,
    },
    Error(Arc<attractor_types::AttractorError>),
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_system_constructor() {
        let msg = Message::system("You are helpful.");
        assert_eq!(msg.role, Role::System);
        assert_eq!(msg.content.len(), 1);
        assert!(msg.name.is_none());
        match &msg.content[0] {
            ContentPart::Text { text } => assert_eq!(text, "You are helpful."),
            _ => panic!("expected Text content part"),
        }
    }

    #[test]
    fn message_user_constructor() {
        let msg = Message::user("Hello");
        assert_eq!(msg.role, Role::User);
        match &msg.content[0] {
            ContentPart::Text { text } => assert_eq!(text, "Hello"),
            _ => panic!("expected Text content part"),
        }
    }

    #[test]
    fn message_assistant_constructor() {
        let msg = Message::assistant("Hi there");
        assert_eq!(msg.role, Role::Assistant);
        match &msg.content[0] {
            ContentPart::Text { text } => assert_eq!(text, "Hi there"),
            _ => panic!("expected Text content part"),
        }
    }

    #[test]
    fn message_tool_result_constructor() {
        let msg = Message::tool_result("call_123", "my_tool", "result text", false);
        assert_eq!(msg.role, Role::Tool);
        assert_eq!(msg.tool_call_id.as_deref(), Some("call_123"));
        match &msg.content[0] {
            ContentPart::ToolResult {
                tool_call_id,
                tool_name,
                content,
                is_error,
            } => {
                assert_eq!(tool_call_id, "call_123");
                assert_eq!(tool_name, "my_tool");
                assert_eq!(content, "result text");
                assert!(!is_error);
            }
            _ => panic!("expected ToolResult content part"),
        }
    }

    #[test]
    fn content_part_serializes_as_tagged_union() {
        let part = ContentPart::Text {
            text: "hello".into(),
        };
        let json = serde_json::to_value(&part).unwrap();
        assert_eq!(json["type"], "text");
        assert_eq!(json["text"], "hello");

        let tool = ContentPart::ToolCall {
            id: "t1".into(),
            name: "search".into(),
            arguments: serde_json::json!({"q": "rust"}),
        };
        let json = serde_json::to_value(&tool).unwrap();
        assert_eq!(json["type"], "tool_call");
        assert_eq!(json["name"], "search");
    }

    #[test]
    fn role_serialization() {
        assert_eq!(serde_json::to_string(&Role::System).unwrap(), "\"system\"");
        assert_eq!(serde_json::to_string(&Role::User).unwrap(), "\"user\"");
        assert_eq!(
            serde_json::to_string(&Role::Assistant).unwrap(),
            "\"assistant\""
        );
        assert_eq!(serde_json::to_string(&Role::Tool).unwrap(), "\"tool\"");
        assert_eq!(
            serde_json::to_string(&Role::Developer).unwrap(),
            "\"developer\""
        );

        let role: Role = serde_json::from_str("\"system\"").unwrap();
        assert_eq!(role, Role::System);
    }

    #[test]
    fn usage_default() {
        let usage = Usage::default();
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.output_tokens, 0);
        assert!(usage.reasoning_tokens.is_none());
        assert!(usage.cache_read_tokens.is_none());
        assert!(usage.cache_write_tokens.is_none());
        assert_eq!(usage.total_tokens, 0);
    }

    #[test]
    fn finish_reason_variants() {
        let reasons = [
            (FinishReason::EndTurn, "\"end_turn\""),
            (FinishReason::MaxTokens, "\"max_tokens\""),
            (FinishReason::StopSequence, "\"stop_sequence\""),
            (FinishReason::ToolUse, "\"tool_use\""),
            (FinishReason::ContentFiltered, "\"content_filtered\""),
        ];
        for (reason, expected) in &reasons {
            let json = serde_json::to_string(reason).unwrap();
            assert_eq!(&json, expected);
            let back: FinishReason = serde_json::from_str(expected).unwrap();
            assert_eq!(&back, reason);
        }
    }

    #[test]
    fn request_response_round_trip() {
        let req = Request {
            model: "gpt-4".into(),
            messages: vec![Message::user("Hello"), Message::assistant("Hi")],
            tools: vec![ToolDefinition {
                name: "search".into(),
                description: "Search the web".into(),
                parameters: serde_json::json!({"type": "object"}),
            }],
            tool_choice: Some(ToolChoice::Auto),
            max_tokens: Some(1024),
            temperature: Some(0.7),
            stop_sequences: vec!["STOP".into()],
            reasoning_effort: Some(ReasoningEffort::High),
            provider: Some("openai".into()),
            provider_options: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(back.model, "gpt-4");
        assert_eq!(back.messages.len(), 2);
        assert_eq!(back.tools.len(), 1);
        assert_eq!(back.max_tokens, Some(1024));

        let resp = Response {
            id: "resp_1".into(),
            text: "Hello!".into(),
            tool_calls: vec![ToolCallResult {
                id: "tc_1".into(),
                name: "search".into(),
                arguments: serde_json::json!({"q": "test"}),
            }],
            reasoning: Some("thinking...".into()),
            usage: Usage {
                input_tokens: 10,
                output_tokens: 20,
                reasoning_tokens: Some(5),
                cache_read_tokens: None,
                cache_write_tokens: None,
                total_tokens: 30,
            },
            model: "gpt-4".into(),
            finish_reason: FinishReason::EndTurn,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: Response = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "resp_1");
        assert_eq!(back.text, "Hello!");
        assert_eq!(back.tool_calls.len(), 1);
        assert_eq!(back.usage.total_tokens, 30);
        assert_eq!(back.finish_reason, FinishReason::EndTurn);
    }
}
