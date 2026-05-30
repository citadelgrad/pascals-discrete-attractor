//! Thread-safe key-value store for pipeline state.

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
