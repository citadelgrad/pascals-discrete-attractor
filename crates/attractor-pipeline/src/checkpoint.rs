//! Checkpoint save/restore and crash recovery for pipeline execution.
//!
//! After each node completion the executor can persist a [`PipelineCheckpoint`]
//! to disk.  On restart, [`load_checkpoint`] discovers the latest snapshot so
//! the pipeline can resume from the last completed node instead of starting
//! over.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Snapshot of pipeline execution state for crash recovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineCheckpoint {
    /// The node that was being executed (or about to be executed) when the
    /// checkpoint was taken.
    pub current_node_id: String,
    /// IDs of nodes that have already finished successfully.
    pub completed_nodes: Vec<String>,
    /// Outcome produced by each completed node, keyed by node ID.
    pub node_outcomes: HashMap<String, attractor_types::Outcome>,
    /// Serialised snapshot of the pipeline [`Context`](attractor_types::Context).
    pub context_snapshot: HashMap<String, serde_json::Value>,
    /// RFC 3339 timestamp of when the checkpoint was created.
    pub timestamp: String,
    /// Optional session ID for tracking execution sessions (e.g., for SSE streaming).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Number of steps executed so far (for enforcing max_steps across resume).
    #[serde(default)]
    pub step_count: u64,
    /// Total cost accrued so far in USD (for enforcing max_budget_usd across resume).
    #[serde(default)]
    pub total_cost: f64,
}

impl PipelineCheckpoint {
    /// Create a new checkpoint from current execution state.
    pub fn new(
        current_node_id: String,
        completed_nodes: Vec<String>,
        node_outcomes: HashMap<String, attractor_types::Outcome>,
        context_snapshot: HashMap<String, serde_json::Value>,
    ) -> Self {
        Self {
            current_node_id,
            completed_nodes,
            node_outcomes,
            context_snapshot,
            timestamp: chrono::Utc::now().to_rfc3339(),
            session_id: None,
            step_count: 0,
            total_cost: 0.0,
        }
    }

    /// Create a new checkpoint with step count and total cost preserved.
    pub fn with_counters(
        current_node_id: String,
        completed_nodes: Vec<String>,
        node_outcomes: HashMap<String, attractor_types::Outcome>,
        context_snapshot: HashMap<String, serde_json::Value>,
        step_count: u64,
        total_cost: f64,
    ) -> Self {
        Self {
            current_node_id,
            completed_nodes,
            node_outcomes,
            context_snapshot,
            timestamp: chrono::Utc::now().to_rfc3339(),
            session_id: None,
            step_count,
            total_cost,
        }
    }

    /// Create a new checkpoint with a session ID and preserved counters.
    pub fn with_session_id(
        current_node_id: String,
        completed_nodes: Vec<String>,
        node_outcomes: HashMap<String, attractor_types::Outcome>,
        context_snapshot: HashMap<String, serde_json::Value>,
        session_id: String,
        step_count: u64,
        total_cost: f64,
    ) -> Self {
        Self {
            current_node_id,
            completed_nodes,
            node_outcomes,
            context_snapshot,
            timestamp: chrono::Utc::now().to_rfc3339(),
            session_id: Some(session_id),
            step_count,
            total_cost,
        }
    }
}

/// Save a checkpoint to the given directory.
///
/// The directory is created if it does not already exist.  The checkpoint is
/// written to `<logs_root>/checkpoint.json`.
pub async fn save_checkpoint(
    checkpoint: &PipelineCheckpoint,
    logs_root: &Path,
) -> attractor_types::Result<PathBuf> {
    tokio::fs::create_dir_all(logs_root).await?;
    let path = logs_root.join("checkpoint.json");
    let json = serde_json::to_string_pretty(checkpoint)?;
    tokio::fs::write(&path, json).await?;
    tracing::debug!(path = %path.display(), "Checkpoint saved");
    Ok(path)
}

/// Load the latest checkpoint from a directory.
///
/// Returns `Ok(None)` when no checkpoint file exists (i.e. first run or after
/// [`clear_checkpoint`]).
pub async fn load_checkpoint(
    logs_root: &Path,
) -> attractor_types::Result<Option<PipelineCheckpoint>> {
    let path = logs_root.join("checkpoint.json");
    if !tokio::fs::try_exists(&path).await? {
        return Ok(None);
    }
    let json = tokio::fs::read_to_string(&path).await?;
    let checkpoint: PipelineCheckpoint = serde_json::from_str(&json)?;
    Ok(Some(checkpoint))
}

/// Delete checkpoint after successful pipeline completion.
pub async fn clear_checkpoint(logs_root: &Path) -> attractor_types::Result<()> {
    let path = logs_root.join("checkpoint.json");
    if tokio::fs::try_exists(&path).await? {
        tokio::fs::remove_file(&path).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use attractor_types::Outcome;

    fn sample_checkpoint() -> PipelineCheckpoint {
        let mut outcomes = HashMap::new();
        outcomes.insert("node_a".into(), Outcome::success("done"));

        let mut ctx = HashMap::new();
        ctx.insert("key".into(), serde_json::json!("value"));

        PipelineCheckpoint::new("node_b".into(), vec!["node_a".into()], outcomes, ctx)
    }

    #[tokio::test]
    async fn save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let cp = sample_checkpoint();

        let path = save_checkpoint(&cp, dir.path()).await.unwrap();
        assert!(path.exists());

        let loaded = load_checkpoint(dir.path()).await.unwrap().unwrap();
        assert_eq!(loaded.current_node_id, "node_b");
        assert_eq!(loaded.completed_nodes, vec!["node_a".to_string()]);
        assert_eq!(loaded.context_snapshot.get("key").unwrap(), "value");
    }

    #[tokio::test]
    async fn load_from_nonexistent_directory_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does_not_exist");

        let result = load_checkpoint(&missing).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn clear_removes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let cp = sample_checkpoint();

        save_checkpoint(&cp, dir.path()).await.unwrap();
        assert!(dir.path().join("checkpoint.json").exists());

        clear_checkpoint(dir.path()).await.unwrap();
        assert!(!dir.path().join("checkpoint.json").exists());
    }

    #[tokio::test]
    async fn serialization_preserves_all_fields() {
        let cp = sample_checkpoint();
        let json = serde_json::to_string(&cp).unwrap();
        let restored: PipelineCheckpoint = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.current_node_id, cp.current_node_id);
        assert_eq!(restored.completed_nodes, cp.completed_nodes);
        assert_eq!(restored.timestamp, cp.timestamp);
        assert_eq!(
            restored.context_snapshot.get("key"),
            cp.context_snapshot.get("key"),
        );

        // Verify the outcome was preserved
        let orig_outcome = cp.node_outcomes.get("node_a").unwrap();
        let rest_outcome = restored.node_outcomes.get("node_a").unwrap();
        assert_eq!(rest_outcome.notes, orig_outcome.notes);
    }

    #[tokio::test]
    async fn session_id_serialization() {
        let mut outcomes = HashMap::new();
        outcomes.insert("node_a".into(), Outcome::success("done"));

        let mut ctx = HashMap::new();
        ctx.insert("key".into(), serde_json::json!("value"));

        let cp = PipelineCheckpoint::with_session_id(
            "node_b".into(),
            vec!["node_a".into()],
            outcomes,
            ctx,
            "test-session-123".into(),
            5,
            1.25,
        );

        let json = serde_json::to_string(&cp).unwrap();
        let restored: PipelineCheckpoint = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.session_id, Some("test-session-123".to_string()));
    }

    #[tokio::test]
    async fn backward_compatibility_without_session_id() {
        // Simulate old checkpoint JSON without session_id field
        let json = r#"{
            "current_node_id": "node_b",
            "completed_nodes": ["node_a"],
            "node_outcomes": {},
            "context_snapshot": {},
            "timestamp": "2024-01-01T00:00:00Z"
        }"#;

        let restored: PipelineCheckpoint = serde_json::from_str(json).unwrap();
        assert_eq!(restored.session_id, None);
    }
}
