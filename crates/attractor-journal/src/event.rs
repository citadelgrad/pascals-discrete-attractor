//! The Run Journal envelope and Event types (spec C3).
//!
//! Every journal line is one [`JournalEvent`]:
//!
//! ```json
//! {"v":1,"seq":42,"ts":"2026-09-24T10:03:11.123Z","run_id":"0192...","attempt":2,"type":"StageStarted","data":{"node_id":"implement","handler_type":"codergen"}}
//! ```
//!
//! Event types that this version does not know decode to
//! [`EventData::Unknown`], which keeps the type name and data so the line can
//! be passed through without loss.

use std::collections::BTreeMap;

use chrono::{DateTime, SecondsFormat, SubsecRound, Utc};
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::JOURNAL_VERSION;

/// One Event of a Run, with its envelope.
#[derive(Debug, Clone, PartialEq)]
pub struct JournalEvent {
    /// Envelope version; always [`JOURNAL_VERSION`].
    pub v: u32,
    /// Strictly increasing across all Attempts of the Run, starting at 1.
    pub seq: u64,
    /// When the Event was recorded, at millisecond precision.
    pub ts: DateTime<Utc>,
    pub run_id: String,
    pub attempt: u32,
    pub data: EventData,
}

impl JournalEvent {
    /// Build an Event with the current envelope version. `ts` is truncated to
    /// milliseconds so the Event equals itself after a round trip.
    pub fn new(
        seq: u64,
        ts: DateTime<Utc>,
        run_id: impl Into<String>,
        attempt: u32,
        data: EventData,
    ) -> Self {
        Self {
            v: JOURNAL_VERSION,
            seq,
            ts: ts.trunc_subsecs(3),
            run_id: run_id.into(),
            attempt,
            data,
        }
    }
}

/// Why an Attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptEndReason {
    Completed,
    Failed,
    Stopped,
    BudgetExhausted,
    MaxSteps,
    Error,
}

/// Where a Human Gate answer came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerSource {
    Terminal,
    Monitor,
    Cli,
}

/// A Task as listed in an [`EventData::EpicSnapshot`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSummary {
    pub id: String,
    pub title: String,
    pub status: String,
}

/// A Run Commit as listed in [`EventData::CommitsCreated`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitRef {
    pub sha: String,
    pub subject: String,
    pub author: String,
    /// Committer date in strict ISO 8601, as printed by git (`%cI`).
    pub ts: String,
}

/// The `type` and `data` of a journal line.
///
/// The existing engine events keep the names, field names, and field types of
/// `PipelineEvent` in `attractor-pipeline`, so converting one is a
/// field-by-field copy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum EventData {
    // --- Run lifecycle ---
    RunStarted {
        pipeline_name: String,
        pipeline_path: String,
        workdir: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        epic_id: Option<String>,
        /// `null` when the Run has no budget limit.
        #[serde(default)]
        max_budget_usd: Option<f64>,
        /// `null` when the Run has no step limit.
        #[serde(default)]
        max_steps: Option<u64>,
        /// Set when the Run was started with `--allow-shared-workdir` (C5).
        #[serde(default)]
        shared_workdir: bool,
    },
    AttemptStarted {
        attempt: u32,
        pid: u32,
        pas_version: String,
        argv: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        git_head: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resumed_from_node: Option<String>,
    },
    /// Never written when the process is killed; its absence is how a
    /// crashed Attempt is detected.
    AttemptEnded {
        attempt: u32,
        reason: AttemptEndReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    Heartbeat {
        pid: u32,
    },

    // --- Existing engine events (as `PipelineEvent`) ---
    PipelineStarted {
        pipeline_name: String,
        node_count: usize,
    },
    PipelineCompleted {
        pipeline_name: String,
        completed_nodes: Vec<String>,
        duration_ms: u64,
    },
    PipelineFailed {
        pipeline_name: String,
        error: String,
    },
    StageStarted {
        node_id: String,
        handler_type: String,
    },
    StageCompleted {
        node_id: String,
        status: String,
        duration_ms: u64,
    },
    StageFailed {
        node_id: String,
        error: String,
    },
    StageRetrying {
        node_id: String,
        attempt: usize,
    },
    EdgeSelected {
        from_node: String,
        to_node: String,
        edge_label: Option<String>,
    },
    GoalGateChecked {
        node_id: String,
        satisfied: bool,
    },
    CheckpointSaved {
        node_id: String,
    },
    ContextUpdated {
        node_id: String,
        keys: Vec<String>,
    },

    // --- Tasks ---
    EpicSnapshot {
        epic_id: String,
        title: String,
        tasks: Vec<TaskSummary>,
    },
    TaskClaimed {
        task_id: String,
        title: String,
        epic_id: String,
        node_id: String,
    },
    TaskSelectionBlocked {
        epic_id: String,
        open: Vec<String>,
        /// Open Task ID -> IDs of the issues blocking it.
        blocked_by: BTreeMap<String, Vec<String>>,
    },
    TaskClosed {
        task_id: String,
        reason: String,
        upstream_verified: bool,
        /// SHAs of the Run Commits attributed to the Task.
        commits: Vec<String>,
    },

    // --- Model Invocations and Run Commits ---
    LlmInvoked {
        invocation_id: String,
        node_id: String,
        provider: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_requested: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_actual: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost_usd: Option<f64>,
        duration_ms: u64,
        /// Transcript path relative to the Run folder.
        transcript: String,
        status: String,
    },
    CommitsCreated {
        node_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
        commits: Vec<CommitRef>,
    },

    // --- Human Gates and stops ---
    HumanInputRequested {
        question_id: String,
        node_id: String,
        text: String,
        choices: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
    },
    HumanInputAnswered {
        question_id: String,
        choice: String,
        source: AnswerSource,
    },
    StopRequested {
        source: String,
    },

    /// An Event type this version does not know. Only produced by
    /// deserialization; readers skip it unless they ask for raw lines.
    #[serde(skip)]
    Unknown {
        type_name: String,
        data: Value,
    },
}

impl EventData {
    /// Every `type` this version understands.
    pub const KNOWN_TYPES: &'static [&'static str] = &[
        "RunStarted",
        "AttemptStarted",
        "AttemptEnded",
        "Heartbeat",
        "PipelineStarted",
        "PipelineCompleted",
        "PipelineFailed",
        "StageStarted",
        "StageCompleted",
        "StageFailed",
        "StageRetrying",
        "EdgeSelected",
        "GoalGateChecked",
        "CheckpointSaved",
        "ContextUpdated",
        "EpicSnapshot",
        "TaskClaimed",
        "TaskSelectionBlocked",
        "TaskClosed",
        "LlmInvoked",
        "CommitsCreated",
        "HumanInputRequested",
        "HumanInputAnswered",
        "StopRequested",
    ];

    /// The journal `type` of this Event.
    pub fn type_name(&self) -> &str {
        match self {
            Self::RunStarted { .. } => "RunStarted",
            Self::AttemptStarted { .. } => "AttemptStarted",
            Self::AttemptEnded { .. } => "AttemptEnded",
            Self::Heartbeat { .. } => "Heartbeat",
            Self::PipelineStarted { .. } => "PipelineStarted",
            Self::PipelineCompleted { .. } => "PipelineCompleted",
            Self::PipelineFailed { .. } => "PipelineFailed",
            Self::StageStarted { .. } => "StageStarted",
            Self::StageCompleted { .. } => "StageCompleted",
            Self::StageFailed { .. } => "StageFailed",
            Self::StageRetrying { .. } => "StageRetrying",
            Self::EdgeSelected { .. } => "EdgeSelected",
            Self::GoalGateChecked { .. } => "GoalGateChecked",
            Self::CheckpointSaved { .. } => "CheckpointSaved",
            Self::ContextUpdated { .. } => "ContextUpdated",
            Self::EpicSnapshot { .. } => "EpicSnapshot",
            Self::TaskClaimed { .. } => "TaskClaimed",
            Self::TaskSelectionBlocked { .. } => "TaskSelectionBlocked",
            Self::TaskClosed { .. } => "TaskClosed",
            Self::LlmInvoked { .. } => "LlmInvoked",
            Self::CommitsCreated { .. } => "CommitsCreated",
            Self::HumanInputRequested { .. } => "HumanInputRequested",
            Self::HumanInputAnswered { .. } => "HumanInputAnswered",
            Self::StopRequested { .. } => "StopRequested",
            Self::Unknown { type_name, .. } => type_name,
        }
    }

    pub fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown { .. })
    }

    /// Events after which the writer forces the journal to disk (C3).
    pub fn needs_fsync(&self) -> bool {
        matches!(
            self,
            Self::AttemptEnded { .. } | Self::HumanInputRequested { .. }
        )
    }

    /// Rebuild from the wire `type` and `data`. Unknown types never fail;
    /// a known type with malformed data does.
    fn from_wire_parts(type_name: String, data: Value) -> Result<Self, serde_json::Error> {
        if !Self::KNOWN_TYPES.contains(&type_name.as_str()) {
            return Ok(Self::Unknown { type_name, data });
        }
        serde_json::from_value(serde_json::json!({ "type": type_name, "data": data }))
    }
}

/// Wire form of the envelope. Field order is the contract's key order.
#[derive(Deserialize)]
struct Wire {
    v: u32,
    seq: u64,
    ts: String,
    run_id: String,
    attempt: u32,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    data: Value,
}

/// Serialization counterpart of [`Wire`]. Known Events are flattened
/// directly (not through a `Value`), so `data` keeps its field order.
#[derive(Serialize)]
struct WireOut<'a, T: Serialize> {
    v: u32,
    seq: u64,
    ts: String,
    run_id: &'a str,
    attempt: u32,
    #[serde(flatten)]
    body: T,
}

#[derive(Serialize)]
struct UnknownBody<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    data: &'a Value,
}

impl Serialize for JournalEvent {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let ts = self.ts.to_rfc3339_opts(SecondsFormat::Millis, true);
        match &self.data {
            EventData::Unknown { type_name, data } => WireOut {
                v: self.v,
                seq: self.seq,
                ts,
                run_id: &self.run_id,
                attempt: self.attempt,
                body: UnknownBody {
                    kind: type_name,
                    data,
                },
            }
            .serialize(serializer),
            known => WireOut {
                v: self.v,
                seq: self.seq,
                ts,
                run_id: &self.run_id,
                attempt: self.attempt,
                body: known,
            }
            .serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for JournalEvent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = Wire::deserialize(deserializer)?;
        if wire.v != JOURNAL_VERSION {
            return Err(de::Error::custom(format!(
                "unsupported journal version {} (expected {JOURNAL_VERSION})",
                wire.v
            )));
        }
        let ts = DateTime::parse_from_rfc3339(&wire.ts)
            .map_err(|e| de::Error::custom(format!("invalid ts {:?}: {e}", wire.ts)))?
            .with_timezone(&Utc);
        let data = EventData::from_wire_parts(wire.kind, wire.data).map_err(de::Error::custom)?;
        Ok(Self {
            v: wire.v,
            seq: wire.seq,
            ts,
            run_id: wire.run_id,
            attempt: wire.attempt,
            data,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> String {
        v.to_string()
    }

    /// One sample of every known variant.
    fn samples() -> Vec<EventData> {
        vec![
            EventData::RunStarted {
                pipeline_name: s("X"),
                pipeline_path: s("/p.dot"),
                workdir: s("/w"),
                epic_id: None,
                max_budget_usd: None,
                max_steps: None,
                shared_workdir: false,
            },
            EventData::AttemptStarted {
                attempt: 2,
                pid: 42,
                pas_version: s("0.11.0"),
                argv: vec![s("pas"), s("run")],
                git_head: Some(s("abc")),
                resumed_from_node: None,
            },
            EventData::AttemptEnded {
                attempt: 1,
                reason: AttemptEndReason::Completed,
                message: None,
            },
            EventData::Heartbeat { pid: 42 },
            EventData::PipelineStarted {
                pipeline_name: s("X"),
                node_count: 3,
            },
            EventData::PipelineCompleted {
                pipeline_name: s("X"),
                completed_nodes: vec![s("a")],
                duration_ms: 5,
            },
            EventData::PipelineFailed {
                pipeline_name: s("X"),
                error: s("boom"),
            },
            EventData::StageStarted {
                node_id: s("a"),
                handler_type: s("codergen"),
            },
            EventData::StageCompleted {
                node_id: s("a"),
                status: s("success"),
                duration_ms: 1,
            },
            EventData::StageFailed {
                node_id: s("a"),
                error: s("e"),
            },
            EventData::StageRetrying {
                node_id: s("a"),
                attempt: 2,
            },
            EventData::EdgeSelected {
                from_node: s("a"),
                to_node: s("b"),
                edge_label: Some(s("MORE")),
            },
            EventData::GoalGateChecked {
                node_id: s("a"),
                satisfied: true,
            },
            EventData::CheckpointSaved { node_id: s("a") },
            EventData::ContextUpdated {
                node_id: s("a"),
                keys: vec![s("k")],
            },
            EventData::EpicSnapshot {
                epic_id: s("e"),
                title: s("t"),
                tasks: vec![TaskSummary {
                    id: s("e.1"),
                    title: s("t1"),
                    status: s("open"),
                }],
            },
            EventData::TaskClaimed {
                task_id: s("e.1"),
                title: s("t1"),
                epic_id: s("e"),
                node_id: s("pick"),
            },
            EventData::TaskSelectionBlocked {
                epic_id: s("e"),
                open: vec![s("e.2")],
                blocked_by: BTreeMap::from([(s("e.2"), vec![s("e.1")])]),
            },
            EventData::TaskClosed {
                task_id: s("e.1"),
                reason: s("done"),
                upstream_verified: true,
                commits: vec![s("abc")],
            },
            EventData::LlmInvoked {
                invocation_id: s("i"),
                node_id: s("a"),
                provider: s("claude"),
                model_requested: Some(s("opus")),
                model_actual: Some(s("claude-opus-5-5")),
                input_tokens: Some(10),
                output_tokens: Some(20),
                cost_usd: Some(0.25),
                duration_ms: 100,
                transcript: s("transcripts/i.jsonl"),
                status: s("success"),
            },
            EventData::CommitsCreated {
                node_id: s("a"),
                task_id: None,
                commits: vec![CommitRef {
                    sha: s("abc"),
                    subject: s("fix"),
                    author: s("A"),
                    ts: s("2026-09-24T10:00:00+02:00"),
                }],
            },
            EventData::HumanInputRequested {
                question_id: s("q"),
                node_id: s("gate"),
                text: s("ok?"),
                choices: vec![s("yes"), s("no")],
                default: None,
            },
            EventData::HumanInputAnswered {
                question_id: s("q"),
                choice: s("yes"),
                source: AnswerSource::Monitor,
            },
            EventData::StopRequested { source: s("cli") },
        ]
    }

    #[test]
    fn known_types_cover_every_variant() {
        let samples = samples();
        let names: Vec<&str> = samples.iter().map(EventData::type_name).collect();
        assert_eq!(names, EventData::KNOWN_TYPES);
    }

    #[test]
    fn every_variant_round_trips_through_the_envelope() {
        for (i, data) in samples().into_iter().enumerate() {
            let e = JournalEvent::new(i as u64 + 1, Utc::now(), "r", 1, data);
            let line = serde_json::to_string(&e).unwrap();
            let wire: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(wire["type"], e.data.type_name(), "{line}");
            assert!(wire["data"].is_object(), "{line}");
            let back: JournalEvent = serde_json::from_str(&line).unwrap();
            assert_eq!(back, e, "{line}");
        }
    }

    #[test]
    fn unknown_type_without_data_decodes() {
        let line = r#"{"v":1,"seq":1,"ts":"2026-09-24T10:03:11.123Z","run_id":"r","attempt":1,"type":"Later"}"#;
        let e: JournalEvent = serde_json::from_str(line).unwrap();
        assert_eq!(e.data.type_name(), "Later");
        assert!(e.data.is_unknown());
    }

    #[test]
    fn new_truncates_ts_to_millis() {
        let ts = DateTime::parse_from_rfc3339("2026-09-24T10:03:11.123456789Z")
            .unwrap()
            .with_timezone(&Utc);
        let e = JournalEvent::new(1, ts, "r", 1, EventData::Heartbeat { pid: 1 });
        assert_eq!(
            e.ts.to_rfc3339_opts(SecondsFormat::Nanos, true),
            "2026-09-24T10:03:11.123000000Z"
        );
    }

    #[test]
    fn invalid_ts_is_rejected() {
        let line = r#"{"v":1,"seq":1,"ts":"yesterday","run_id":"r","attempt":1,"type":"Heartbeat","data":{"pid":1}}"#;
        assert!(serde_json::from_str::<JournalEvent>(line).is_err());
    }

    #[test]
    fn fsync_events() {
        let samples = samples();
        let fsynced: Vec<&str> = samples
            .iter()
            .filter(|d| d.needs_fsync())
            .map(EventData::type_name)
            .collect();
        assert_eq!(fsynced, ["AttemptEnded", "HumanInputRequested"]);
    }
}
