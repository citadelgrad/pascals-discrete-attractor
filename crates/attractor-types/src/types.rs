//! Pipeline outcome, status, checkpoint, and fidelity types.

use std::collections::HashMap;
use std::path::Path;
use serde::{Deserialize, Serialize};

use crate::error::Result;

// ---------------------------------------------------------------------------
// StageStatus
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
// Outcome
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
// Checkpoint
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
// FidelityMode
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
