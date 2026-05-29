//! Unified error type and Result alias for Attractor.

/// Unified error type for all Attractor subsystems.
#[derive(Debug, thiserror::Error)]
pub enum AttractorError {
    // === LLM Provider Errors ===
    #[error("Provider {provider} returned HTTP {status}: {message}")]
    ProviderError {
        provider: String,
        status: u16,
        message: String,
        retryable: bool,
    },

    #[error("Rate limited by {provider}, retry after {retry_after_ms}ms")]
    RateLimited {
        provider: String,
        retry_after_ms: u64,
    },

    #[error("Authentication failed for provider {provider}")]
    AuthError { provider: String },

    #[error("Request to {provider} timed out after {timeout_ms}ms")]
    RequestTimeout {
        provider: String,
        timeout_ms: u64,
    },

    #[error("Context length exceeded for {provider}: {message}")]
    ContextLengthExceeded { provider: String, message: String },

    // === Parser Errors ===
    #[error("DOT parse error at line {line}, col {col}: {message}")]
    ParseError {
        line: usize,
        col: usize,
        message: String,
        source_snippet: Option<String>,
    },

    // === Pipeline Errors ===
    #[error("Pipeline validation failed: {0}")]
    ValidationError(String),

    #[error("Handler '{handler}' failed on node '{node}': {message}")]
    HandlerError {
        handler: String,
        node: String,
        message: String,
    },

    #[error("Goal gate unsatisfied: node '{node}' did not reach SUCCESS")]
    GoalGateUnsatisfied { node: String },

    #[error("No retry target for failed goal gate '{node}'")]
    NoRetryTarget { node: String },

    #[error("Max retries exhausted for node '{node}' after {attempts} attempts")]
    RetriesExhausted { node: String, attempts: usize },

    // === Tool Errors ===
    #[error("Tool '{tool}' error: {message}")]
    ToolError { tool: String, message: String },

    #[error("Command timed out after {timeout_ms}ms")]
    CommandTimeout { timeout_ms: u64 },

    #[error("CLI binary '{binary}' not found — ensure it is installed and on PATH")]
    CliNotFound { binary: String },

    // === Agent Errors ===
    #[error("Agent loop detected after {window} consecutive identical tool calls")]
    LoopDetected { window: usize },

    #[error("Turn limit reached: {turns} turns")]
    TurnLimitReached { turns: usize },

    // === Generic ===
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("{0}")]
    Other(String),
}

impl AttractorError {
    /// Returns `true` if the error is transient and the operation may succeed on retry.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            AttractorError::RateLimited { .. }
                | AttractorError::CommandTimeout { .. }
                | AttractorError::ProviderError { retryable: true, .. }
        )
    }

    /// Returns `true` if the error is permanent and retrying will not help.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            AttractorError::AuthError { .. }
                | AttractorError::ValidationError(_)
                | AttractorError::ContextLengthExceeded { .. }
                | AttractorError::CliNotFound { .. }
        )
    }

    /// Maps the error to an HTTP status code for server mode.
    pub fn http_status(&self) -> Option<u16> {
        match self {
            AttractorError::RateLimited { .. } => Some(429),
            AttractorError::AuthError { .. } => Some(401),
            AttractorError::ProviderError { status, .. } => Some(*status),
            AttractorError::RequestTimeout { .. } | AttractorError::CommandTimeout { .. } => {
                Some(504)
            }
            AttractorError::ValidationError(_) => Some(400),
            AttractorError::ContextLengthExceeded { .. } => Some(413),
            _ => None,
        }
    }
}

/// A convenience alias for `Result<T, AttractorError>`.
pub type Result<T> = std::result::Result<T, AttractorError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_provider_error() {
        let err = AttractorError::ProviderError {
            provider: "openai".into(),
            status: 500,
            message: "internal server error".into(),
            retryable: true,
        };
        assert_eq!(
            err.to_string(),
            "Provider openai returned HTTP 500: internal server error"
        );
    }

    #[test]
    fn error_display_rate_limited() {
        let err = AttractorError::RateLimited {
            provider: "anthropic".into(),
            retry_after_ms: 3000,
        };
        assert_eq!(
            err.to_string(),
            "Rate limited by anthropic, retry after 3000ms"
        );
    }

    #[test]
    fn error_display_auth_error() {
        let err = AttractorError::AuthError {
            provider: "openai".into(),
        };
        assert_eq!(err.to_string(), "Authentication failed for provider openai");
    }

    #[test]
    fn error_display_parse_error() {
        let err = AttractorError::ParseError {
            line: 10,
            col: 5,
            message: "unexpected token".into(),
            source_snippet: Some("digraph {".into()),
        };
        assert_eq!(
            err.to_string(),
            "DOT parse error at line 10, col 5: unexpected token"
        );
    }

    #[test]
    fn error_display_validation() {
        let err = AttractorError::ValidationError("cycle detected".into());
        assert_eq!(err.to_string(), "Pipeline validation failed: cycle detected");
    }

    #[test]
    fn error_display_handler_error() {
        let err = AttractorError::HandlerError {
            handler: "llm".into(),
            node: "summarize".into(),
            message: "prompt too long".into(),
        };
        assert_eq!(
            err.to_string(),
            "Handler 'llm' failed on node 'summarize': prompt too long"
        );
    }

    #[test]
    fn error_display_goal_gate() {
        let err = AttractorError::GoalGateUnsatisfied {
            node: "review".into(),
        };
        assert_eq!(
            err.to_string(),
            "Goal gate unsatisfied: node 'review' did not reach SUCCESS"
        );
    }

    #[test]
    fn error_display_retries_exhausted() {
        let err = AttractorError::RetriesExhausted {
            node: "compile".into(),
            attempts: 3,
        };
        assert_eq!(
            err.to_string(),
            "Max retries exhausted for node 'compile' after 3 attempts"
        );
    }

    #[test]
    fn error_display_loop_detected() {
        let err = AttractorError::LoopDetected { window: 5 };
        assert_eq!(
            err.to_string(),
            "Agent loop detected after 5 consecutive identical tool calls"
        );
    }

    #[test]
    fn error_display_turn_limit() {
        let err = AttractorError::TurnLimitReached { turns: 100 };
        assert_eq!(err.to_string(), "Turn limit reached: 100 turns");
    }

    #[test]
    fn error_display_other() {
        let err = AttractorError::Other("something went wrong".into());
        assert_eq!(err.to_string(), "something went wrong");
    }

    #[test]
    fn retryable_rate_limited() {
        let err = AttractorError::RateLimited {
            provider: "x".into(),
            retry_after_ms: 1000,
        };
        assert!(err.is_retryable());
    }

    #[test]
    fn retryable_provider_error_when_flagged() {
        let err = AttractorError::ProviderError {
            provider: "x".into(),
            status: 503,
            message: "unavailable".into(),
            retryable: true,
        };
        assert!(err.is_retryable());
    }

    #[test]
    fn not_retryable_provider_error_when_not_flagged() {
        let err = AttractorError::ProviderError {
            provider: "x".into(),
            status: 400,
            message: "bad request".into(),
            retryable: false,
        };
        assert!(!err.is_retryable());
    }

    #[test]
    fn retryable_command_timeout() {
        let err = AttractorError::CommandTimeout { timeout_ms: 5000 };
        assert!(err.is_retryable());
    }

    #[test]
    fn not_retryable_auth_error() {
        let err = AttractorError::AuthError {
            provider: "x".into(),
        };
        assert!(!err.is_retryable());
    }

    #[test]
    fn terminal_auth_error() {
        let err = AttractorError::AuthError {
            provider: "x".into(),
        };
        assert!(err.is_terminal());
    }

    #[test]
    fn terminal_validation_error() {
        let err = AttractorError::ValidationError("bad".into());
        assert!(err.is_terminal());
    }

    #[test]
    fn terminal_context_length_exceeded() {
        let err = AttractorError::ContextLengthExceeded {
            provider: "x".into(),
            message: "too long".into(),
        };
        assert!(err.is_terminal());
    }

    #[test]
    fn not_terminal_rate_limited() {
        let err = AttractorError::RateLimited {
            provider: "x".into(),
            retry_after_ms: 1000,
        };
        assert!(!err.is_terminal());
    }

    #[test]
    fn http_status_rate_limited_429() {
        let err = AttractorError::RateLimited {
            provider: "x".into(),
            retry_after_ms: 0,
        };
        assert_eq!(err.http_status(), Some(429));
    }

    #[test]
    fn http_status_auth_401() {
        let err = AttractorError::AuthError {
            provider: "x".into(),
        };
        assert_eq!(err.http_status(), Some(401));
    }

    #[test]
    fn http_status_provider_passes_through() {
        let err = AttractorError::ProviderError {
            provider: "x".into(),
            status: 502,
            message: "bad gateway".into(),
            retryable: true,
        };
        assert_eq!(err.http_status(), Some(502));
    }

    #[test]
    fn http_status_timeout_504() {
        let err = AttractorError::RequestTimeout {
            provider: "x".into(),
            timeout_ms: 5000,
        };
        assert_eq!(err.http_status(), Some(504));
    }

    #[test]
    fn http_status_command_timeout_504() {
        let err = AttractorError::CommandTimeout { timeout_ms: 5000 };
        assert_eq!(err.http_status(), Some(504));
    }

    #[test]
    fn http_status_validation_400() {
        let err = AttractorError::ValidationError("bad".into());
        assert_eq!(err.http_status(), Some(400));
    }

    #[test]
    fn http_status_context_length_413() {
        let err = AttractorError::ContextLengthExceeded {
            provider: "x".into(),
            message: "too long".into(),
        };
        assert_eq!(err.http_status(), Some(413));
    }

    #[test]
    fn http_status_none_for_other() {
        let err = AttractorError::Other("something".into());
        assert_eq!(err.http_status(), None);
    }

    #[test]
    fn from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let err: AttractorError = io_err.into();
        assert!(matches!(err, AttractorError::Io(_)));
        assert!(err.to_string().contains("file not found"));
    }

    #[test]
    fn from_serde_json_error() {
        let json_err = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
        let err: AttractorError = json_err.into();
        assert!(matches!(err, AttractorError::Json(_)));
    }

    #[test]
    fn result_alias_works() {
        fn example() -> Result<u32> {
            Ok(42)
        }
        assert_eq!(example().unwrap(), 42);
    }

    #[test]
    fn result_alias_err() {
        fn example() -> Result<()> {
            Err(AttractorError::Other("fail".into()))
        }
        assert!(example().is_err());
    }
}
