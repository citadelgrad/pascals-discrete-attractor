//! Checkpoint save/restore and crash recovery for pipeline execution.
//!
//! After each node completion the executor can persist a [`PipelineCheckpoint`]
//! to disk.  On restart, [`load_checkpoint`] discovers the latest snapshot so
//! the pipeline can resume from the last completed node instead of starting
//! over.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

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
    /// Serialised workflow-data snapshot from pipeline [`Context`](attractor_types::Context).
    /// Typed run controls are not authoritative here and are filtered from
    /// legacy snapshots during restore.
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
    /// Schema version for forward-compatibility. Defaults to 0 on old checkpoints.
    #[serde(default)]
    pub schema_version: u32,
    /// Per-(quality-node-ID, upstream-node-ID) re-entry counters.
    /// Key format: "<node_id>::<upstream_id>". Persisted so loop budgets
    /// survive checkpoint resume.
    #[serde(default)]
    pub quality_loop_counters: HashMap<String, u32>,
    /// Last failure_footprint seen for each quality node, keyed by node ID.
    #[serde(default)]
    pub quality_last_footprint: HashMap<String, String>,
    /// Node that produced the edge into `current_node_id`.
    ///
    /// Persisted so quality loop keys (`quality_node::upstream_node`) survive
    /// resume, even after `loop_restart` clears completed node history.
    #[serde(default)]
    pub previous_node_id: Option<String>,
    /// Total handler attempts begun across the pipeline, including retries.
    #[serde(default)]
    pub total_handler_attempts: u64,
    /// Node whose current visit has begun attempts but has not completed.
    #[serde(default)]
    pub active_node_id: Option<String>,
    /// Attempts already begun during the active node visit.
    #[serde(default)]
    pub active_node_attempts: usize,
    /// Fingerprint of the compiled execution plan this checkpoint belongs to.
    ///
    /// On resume the engine recomputes the fingerprint of the current plan
    /// and rejects the checkpoint when it does not match, instead of
    /// replaying stale loop/retry state onto a materially changed graph.
    /// Legacy checkpoints (schema_version < 2) omit it and are accepted
    /// with a warning, since their provenance cannot be verified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_fingerprint: Option<String>,
}

/// Checkpoint schema version written by this build.
pub const CHECKPOINT_SCHEMA_VERSION: u32 = 2;

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
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            quality_loop_counters: HashMap::new(),
            quality_last_footprint: HashMap::new(),
            previous_node_id: None,
            total_handler_attempts: 0,
            active_node_id: None,
            active_node_attempts: 0,
            execution_fingerprint: None,
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
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            quality_loop_counters: HashMap::new(),
            quality_last_footprint: HashMap::new(),
            previous_node_id: None,
            total_handler_attempts: 0,
            active_node_id: None,
            active_node_attempts: 0,
            execution_fingerprint: None,
        }
    }

    /// Create a checkpoint preserving quality loop counters (used by the engine
    /// when saving mid-loop state).
    #[allow(clippy::too_many_arguments)]
    pub fn with_quality_counters(
        current_node_id: String,
        completed_nodes: Vec<String>,
        node_outcomes: HashMap<String, attractor_types::Outcome>,
        context_snapshot: HashMap<String, serde_json::Value>,
        step_count: u64,
        total_cost: f64,
        quality_loop_counters: HashMap<String, u32>,
        quality_last_footprint: HashMap<String, String>,
        previous_node_id: Option<String>,
        execution_fingerprint: Option<String>,
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
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            quality_loop_counters,
            quality_last_footprint,
            previous_node_id,
            total_handler_attempts: 0,
            active_node_id: None,
            active_node_attempts: 0,
            execution_fingerprint,
        }
    }
}

/// Save a checkpoint to the given directory.
///
/// The directory is created if it does not already exist.  The checkpoint is
/// written atomically: JSON is written to `checkpoint.json.tmp`, fsynced,
/// then renamed over `checkpoint.json`. A crash mid-write therefore leaves
/// the previous checkpoint intact rather than corrupting the sole recovery
/// artifact.
pub async fn save_checkpoint(
    checkpoint: &PipelineCheckpoint,
    logs_root: &Path,
) -> attractor_types::Result<PathBuf> {
    tokio::fs::create_dir_all(logs_root).await?;
    let path = logs_root.join("checkpoint.json");
    let tmp = logs_root.join("checkpoint.json.tmp");
    let json = serde_json::to_string_pretty(checkpoint)?;
    let mut file = tokio::fs::File::create(&tmp).await?;
    file.write_all(json.as_bytes()).await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(&tmp, &path).await?;
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

/// Validate a loaded checkpoint against the plan about to resume it.
///
/// Fails closed on:
///
/// - a schema version newer than this build can understand, and
/// - a recorded execution fingerprint that does not match the current
///   plan's fingerprint (the DOT was changed in a way that alters
///   execution semantics).
///
/// Legacy checkpoints (`schema_version` below [`CHECKPOINT_SCHEMA_VERSION`],
/// no fingerprint) are accepted with a warning: their provenance cannot be
/// verified, so resume continues rather than blocking every pre-fingerprint
/// checkpoint, but the caller is told the state is unverified.
pub fn validate_checkpoint(
    checkpoint: &PipelineCheckpoint,
    current_fingerprint: &str,
    path: &Path,
) -> attractor_types::Result<()> {
    if checkpoint.schema_version > CHECKPOINT_SCHEMA_VERSION {
        return Err(attractor_types::AttractorError::CheckpointIncompatible {
            path: path.display().to_string(),
            reason: format!(
                "schema version {} was written by a newer PAS than this build (understands up to {}); \
                 run with that PAS or pass --fresh to discard it",
                checkpoint.schema_version, CHECKPOINT_SCHEMA_VERSION
            ),
        });
    }
    match &checkpoint.execution_fingerprint {
        Some(recorded) if recorded != current_fingerprint => {
            Err(attractor_types::AttractorError::CheckpointIncompatible {
                path: path.display().to_string(),
                reason: format!(
                    "execution fingerprint {recorded} does not match the current pipeline ({current_fingerprint}); \
                     the DOT changed in a way that alters execution semantics. \
                     Re-run with --fresh or restore the original DOT"
                ),
            })
        }
        Some(_) => Ok(()),
        None => {
            tracing::warn!(
                path = %path.display(),
                schema_version = checkpoint.schema_version,
                "Resuming legacy checkpoint without an execution fingerprint; \
                 provenance is unverified"
            );
            Ok(())
        }
    }
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
        assert_eq!(restored.total_handler_attempts, 0);
        assert_eq!(restored.active_node_id, None);
        assert_eq!(restored.active_node_attempts, 0);
    }
}
