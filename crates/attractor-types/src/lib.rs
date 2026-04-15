//! Shared types, errors, context, and outcome for the Attractor pipeline engine.
//!
//! This crate provides the foundational types used across all other Attractor crates:
//! - `AttractorError` — unified error taxonomy
//! - `Context` — thread-safe key-value store for pipeline state
//! - `Outcome` — result of executing a node handler
//! - `Checkpoint` — serializable snapshot for crash recovery

use serde::{Deserialize, Serialize};
use std::path::Path;

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
    RequestTimeout { provider: String, timeout_ms: u64 },

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
                | AttractorError::RequestTimeout { .. }
                | AttractorError::ProviderError {
                    retryable: true,
                    ..
                }
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

// ---------------------------------------------------------------------------
// Context — thread-safe key-value store for pipeline state
// ---------------------------------------------------------------------------

use std::collections::HashMap;
use std::sync::Arc;

/// Thread-safe key-value store shared across pipeline nodes.
///
/// Cloning a `Context` yields another handle to the **same** inner state.
/// Use [`clone_isolated`](Context::clone_isolated) to get a deep copy for
/// parallel branch isolation.
#[derive(Clone)]
pub struct Context {
    inner: Arc<tokio::sync::RwLock<ContextInner>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ContextInner {
    values: HashMap<String, serde_json::Value>,
    logs: Vec<String>,
}

impl Context {
    /// Create an empty context.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(tokio::sync::RwLock::new(ContextInner {
                values: HashMap::new(),
                logs: Vec::new(),
            })),
        }
    }

    /// Insert or overwrite a key.
    pub async fn set(&self, key: impl Into<String>, value: serde_json::Value) {
        self.inner.write().await.values.insert(key.into(), value);
    }

    /// Read a value by key (cloned).
    pub async fn get(&self, key: &str) -> Option<serde_json::Value> {
        self.inner.read().await.values.get(key).cloned()
    }

    /// Convenience accessor that returns a `String`. Falls back to `default`
    /// when the key is absent or not a JSON string.
    pub async fn get_string(&self, key: &str, default: &str) -> String {
        self.inner
            .read()
            .await
            .values
            .get(key)
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| default.to_owned())
    }

    /// Append a free-form log entry.
    pub async fn append_log(&self, entry: impl Into<String>) {
        self.inner.write().await.logs.push(entry.into());
    }

    /// Shallow copy of the current values map.
    pub async fn snapshot(&self) -> HashMap<String, serde_json::Value> {
        self.inner.read().await.values.clone()
    }

    /// Deep copy that is fully independent of the original context.
    pub async fn clone_isolated(&self) -> Context {
        let guard = self.inner.read().await;
        Context {
            inner: Arc::new(tokio::sync::RwLock::new(guard.clone())),
        }
    }

    /// Merge `updates` into the context. Existing keys not present in
    /// `updates` are preserved.
    pub async fn apply_updates(&self, updates: HashMap<String, serde_json::Value>) {
        let mut guard = self.inner.write().await;
        guard.values.extend(updates);
    }
}

impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// StageStatus — outcome status of a pipeline node
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageStatus {
    Success,
    PartialSuccess,
    Retry,
    Fail,
    Skipped,
}

// ---------------------------------------------------------------------------
// Outcome — result of executing a node handler
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outcome {
    pub status: StageStatus,
    pub preferred_label: Option<String>,
    pub suggested_next_ids: Vec<String>,
    pub context_updates: HashMap<String, serde_json::Value>,
    pub notes: String,
    pub failure_reason: Option<String>,
}

impl Outcome {
    /// Create a successful outcome with the given notes.
    pub fn success(notes: impl Into<String>) -> Self {
        Self {
            status: StageStatus::Success,
            preferred_label: None,
            suggested_next_ids: Vec::new(),
            context_updates: HashMap::new(),
            notes: notes.into(),
            failure_reason: None,
        }
    }

    /// Create a failed outcome with the given reason.
    pub fn fail(reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self {
            status: StageStatus::Fail,
            preferred_label: None,
            suggested_next_ids: Vec::new(),
            context_updates: HashMap::new(),
            notes: String::new(),
            failure_reason: Some(reason),
        }
    }

    /// Create an outcome with a specific status and preferred label.
    pub fn with_label(status: StageStatus, label: impl Into<String>) -> Self {
        Self {
            status,
            preferred_label: Some(label.into()),
            suggested_next_ids: Vec::new(),
            context_updates: HashMap::new(),
            notes: String::new(),
            failure_reason: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Checkpoint — serializable snapshot for crash recovery
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub current_node: String,
    pub completed_nodes: Vec<String>,
    pub node_retries: HashMap<String, usize>,
    pub context_values: HashMap<String, serde_json::Value>,
    pub logs: Vec<String>,
}

impl Checkpoint {
    /// Serialize this checkpoint to JSON and write it to `path`.
    pub fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Read a checkpoint from a JSON file at `path`.
    pub fn load(path: &Path) -> Result<Self> {
        let data = std::fs::read_to_string(path)?;
        let checkpoint: Self = serde_json::from_str(&data)?;
        Ok(checkpoint)
    }
}

// ---------------------------------------------------------------------------
// FidelityMode — controls output fidelity/compression level
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FidelityMode {
    Full,
    Truncate,
    Compact,
    SummaryLow,
    SummaryMedium,
    SummaryHigh,
}

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
        assert_eq!(
            err.to_string(),
            "Pipeline validation failed: cycle detected"
        );
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

    // --- is_retryable ---

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

    // --- is_terminal ---

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

    // --- http_status ---

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

    // --- From impls ---

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

    // --- Result alias ---

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

    // --- Context ---

    #[tokio::test]
    async fn context_set_and_get_round_trip() {
        let ctx = Context::new();
        ctx.set("key", serde_json::json!("hello")).await;
        let val = ctx.get("key").await;
        assert_eq!(val, Some(serde_json::json!("hello")));
    }

    #[tokio::test]
    async fn context_get_string_returns_default_when_missing() {
        let ctx = Context::new();
        let val = ctx.get_string("missing", "fallback").await;
        assert_eq!(val, "fallback");
    }

    #[tokio::test]
    async fn context_clone_isolated_is_independent() {
        let ctx = Context::new();
        ctx.set("a", serde_json::json!(1)).await;

        let isolated = ctx.clone_isolated().await;
        isolated.set("a", serde_json::json!(999)).await;
        isolated.set("b", serde_json::json!(2)).await;

        // Original is unaffected
        assert_eq!(ctx.get("a").await, Some(serde_json::json!(1)));
        assert_eq!(ctx.get("b").await, None);
    }

    #[tokio::test]
    async fn context_apply_updates_merges() {
        let ctx = Context::new();
        ctx.set("keep", serde_json::json!("old")).await;
        ctx.set("overwrite", serde_json::json!("old")).await;

        let mut updates = std::collections::HashMap::new();
        updates.insert("overwrite".into(), serde_json::json!("new"));
        updates.insert("added".into(), serde_json::json!("fresh"));
        ctx.apply_updates(updates).await;

        assert_eq!(ctx.get("keep").await, Some(serde_json::json!("old")));
        assert_eq!(ctx.get("overwrite").await, Some(serde_json::json!("new")));
        assert_eq!(ctx.get("added").await, Some(serde_json::json!("fresh")));
    }

    #[tokio::test]
    async fn context_snapshot_returns_current_values() {
        let ctx = Context::new();
        ctx.set("x", serde_json::json!(10)).await;
        ctx.set("y", serde_json::json!(20)).await;

        let snap = ctx.snapshot().await;
        assert_eq!(snap.len(), 2);
        assert_eq!(snap.get("x"), Some(&serde_json::json!(10)));
        assert_eq!(snap.get("y"), Some(&serde_json::json!(20)));
    }

    // --- StageStatus ---

    #[test]
    fn stage_status_serializes_to_snake_case() {
        assert_eq!(
            serde_json::to_string(&StageStatus::Success).unwrap(),
            "\"success\""
        );
        assert_eq!(
            serde_json::to_string(&StageStatus::PartialSuccess).unwrap(),
            "\"partial_success\""
        );
        assert_eq!(
            serde_json::to_string(&StageStatus::Retry).unwrap(),
            "\"retry\""
        );
        assert_eq!(
            serde_json::to_string(&StageStatus::Fail).unwrap(),
            "\"fail\""
        );
        assert_eq!(
            serde_json::to_string(&StageStatus::Skipped).unwrap(),
            "\"skipped\""
        );
    }

    #[test]
    fn stage_status_deserializes_from_snake_case() {
        let status: StageStatus = serde_json::from_str("\"partial_success\"").unwrap();
        assert_eq!(status, StageStatus::PartialSuccess);
    }

    // --- Outcome ---

    #[test]
    fn outcome_success_constructor() {
        let o = Outcome::success("all good");
        assert_eq!(o.status, StageStatus::Success);
        assert_eq!(o.notes, "all good");
        assert!(o.preferred_label.is_none());
        assert!(o.failure_reason.is_none());
        assert!(o.suggested_next_ids.is_empty());
        assert!(o.context_updates.is_empty());
    }

    #[test]
    fn outcome_fail_constructor() {
        let o = Outcome::fail("something broke");
        assert_eq!(o.status, StageStatus::Fail);
        assert_eq!(o.failure_reason, Some("something broke".to_string()));
        assert!(o.notes.is_empty());
    }

    #[test]
    fn outcome_with_label_constructor() {
        let o = Outcome::with_label(StageStatus::Retry, "try_again");
        assert_eq!(o.status, StageStatus::Retry);
        assert_eq!(o.preferred_label, Some("try_again".to_string()));
    }

    // --- Checkpoint ---

    #[test]
    fn checkpoint_save_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("checkpoint.json");

        let cp = Checkpoint {
            timestamp: chrono::Utc::now(),
            current_node: "node_a".into(),
            completed_nodes: vec!["node_0".into()],
            node_retries: {
                let mut m = HashMap::new();
                m.insert("node_a".into(), 2);
                m
            },
            context_values: {
                let mut m = HashMap::new();
                m.insert("key".into(), serde_json::json!("val"));
                m
            },
            logs: vec!["started".into()],
        };

        cp.save(&path).unwrap();
        let loaded = Checkpoint::load(&path).unwrap();

        assert_eq!(loaded.current_node, "node_a");
        assert_eq!(loaded.completed_nodes, vec!["node_0".to_string()]);
        assert_eq!(loaded.node_retries.get("node_a"), Some(&2));
        assert_eq!(
            loaded.context_values.get("key"),
            Some(&serde_json::json!("val"))
        );
        assert_eq!(loaded.logs, vec!["started".to_string()]);
    }

    // --- FidelityMode ---

    #[test]
    fn fidelity_mode_serialization() {
        assert_eq!(
            serde_json::to_string(&FidelityMode::Full).unwrap(),
            "\"full\""
        );
        assert_eq!(
            serde_json::to_string(&FidelityMode::Truncate).unwrap(),
            "\"truncate\""
        );
        assert_eq!(
            serde_json::to_string(&FidelityMode::Compact).unwrap(),
            "\"compact\""
        );
        assert_eq!(
            serde_json::to_string(&FidelityMode::SummaryLow).unwrap(),
            "\"summary_low\""
        );
        assert_eq!(
            serde_json::to_string(&FidelityMode::SummaryMedium).unwrap(),
            "\"summary_medium\""
        );
        assert_eq!(
            serde_json::to_string(&FidelityMode::SummaryHigh).unwrap(),
            "\"summary_high\""
        );
    }

    #[test]
    fn fidelity_mode_deserialization() {
        let mode: FidelityMode = serde_json::from_str("\"summary_high\"").unwrap();
        assert_eq!(mode, FidelityMode::SummaryHigh);
    }
}
