//! Pipeline event system for observability.
//!
//! Emits [`PipelineEvent`]s via a [`tokio::sync::broadcast`] channel so that
//! external observers (loggers, metrics collectors, UI, etc.) can subscribe to
//! pipeline execution progress without coupling to the engine internals.

use attractor_journal::{CommitRef, EventData};
use serde::{Deserialize, Serialize};

/// Events emitted during pipeline execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PipelineEvent {
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
    /// HEAD moved during a stage attempt (spec File Change 5). `commits` is
    /// `git log old..new`, newest first, and never empty.
    CommitsCreated {
        node_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
        commits: Vec<CommitRef>,
    },
}

impl PipelineEvent {
    /// The Run Journal form of this Event (spec C3). Names and fields are
    /// unchanged; the match is exhaustive so a new variant cannot be added
    /// without a journal mapping.
    pub fn to_journal_data(&self) -> EventData {
        match self.clone() {
            Self::PipelineStarted {
                pipeline_name,
                node_count,
            } => EventData::PipelineStarted {
                pipeline_name,
                node_count,
            },
            Self::PipelineCompleted {
                pipeline_name,
                completed_nodes,
                duration_ms,
            } => EventData::PipelineCompleted {
                pipeline_name,
                completed_nodes,
                duration_ms,
            },
            Self::PipelineFailed {
                pipeline_name,
                error,
            } => EventData::PipelineFailed {
                pipeline_name,
                error,
            },
            Self::StageStarted {
                node_id,
                handler_type,
            } => EventData::StageStarted {
                node_id,
                handler_type,
            },
            Self::StageCompleted {
                node_id,
                status,
                duration_ms,
            } => EventData::StageCompleted {
                node_id,
                status,
                duration_ms,
            },
            Self::StageFailed { node_id, error } => EventData::StageFailed { node_id, error },
            Self::StageRetrying { node_id, attempt } => {
                EventData::StageRetrying { node_id, attempt }
            }
            Self::EdgeSelected {
                from_node,
                to_node,
                edge_label,
            } => EventData::EdgeSelected {
                from_node,
                to_node,
                edge_label,
            },
            Self::GoalGateChecked { node_id, satisfied } => {
                EventData::GoalGateChecked { node_id, satisfied }
            }
            Self::CheckpointSaved { node_id } => EventData::CheckpointSaved { node_id },
            Self::ContextUpdated { node_id, keys } => EventData::ContextUpdated { node_id, keys },
            Self::CommitsCreated {
                node_id,
                task_id,
                commits,
            } => EventData::CommitsCreated {
                node_id,
                task_id,
                commits,
            },
        }
    }
}

/// Event emitter wrapping a broadcast sender.
#[derive(Clone)]
pub struct EventEmitter {
    sender: tokio::sync::broadcast::Sender<PipelineEvent>,
}

impl EventEmitter {
    /// Create a new emitter with the given channel capacity.
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = tokio::sync::broadcast::channel(capacity);
        Self { sender }
    }

    /// Emit an event to all current subscribers.
    ///
    /// If there are no active receivers the event is silently dropped.
    pub fn emit(&self, event: PipelineEvent) {
        let _ = self.sender.send(event);
    }

    /// Subscribe to events. Returns a broadcast receiver.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<PipelineEvent> {
        self.sender.subscribe()
    }
}

impl Default for EventEmitter {
    fn default() -> Self {
        Self::new(256)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn emitter_sends_and_receives() {
        let emitter = EventEmitter::new(16);
        let mut rx = emitter.subscribe();

        emitter.emit(PipelineEvent::PipelineStarted {
            pipeline_name: "test".into(),
            node_count: 3,
        });

        let event = rx.recv().await.unwrap();
        match event {
            PipelineEvent::PipelineStarted {
                pipeline_name,
                node_count,
            } => {
                assert_eq!(pipeline_name, "test");
                assert_eq!(node_count, 3);
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[tokio::test]
    async fn multiple_subscribers_receive_same_event() {
        let emitter = EventEmitter::new(16);
        let mut rx1 = emitter.subscribe();
        let mut rx2 = emitter.subscribe();

        emitter.emit(PipelineEvent::CheckpointSaved {
            node_id: "n1".into(),
        });

        let e1 = rx1.recv().await.unwrap();
        let e2 = rx2.recv().await.unwrap();

        // Both subscribers should get the same event content.
        let json1 = serde_json::to_string(&e1).unwrap();
        let json2 = serde_json::to_string(&e2).unwrap();
        assert_eq!(json1, json2);
    }

    #[test]
    fn emit_with_no_subscribers_does_not_panic() {
        let emitter = EventEmitter::new(16);
        // No subscriber — this must not panic.
        emitter.emit(PipelineEvent::PipelineFailed {
            pipeline_name: "oops".into(),
            error: "something went wrong".into(),
        });
    }

    #[test]
    fn event_serialization_round_trip() {
        let event = PipelineEvent::StageCompleted {
            node_id: "node_42".into(),
            status: "ok".into(),
            duration_ms: 123,
        };

        let json = serde_json::to_string(&event).unwrap();
        let deserialized: PipelineEvent = serde_json::from_str(&json).unwrap();

        match deserialized {
            PipelineEvent::StageCompleted {
                node_id,
                status,
                duration_ms,
            } => {
                assert_eq!(node_id, "node_42");
                assert_eq!(status, "ok");
                assert_eq!(duration_ms, 123);
            }
            other => panic!("unexpected variant after round-trip: {:?}", other),
        }
    }

    /// C3: existing engine Events keep their names and fields in the journal.
    #[test]
    fn journal_data_matches_pipeline_event_serialization() {
        use attractor_journal::JournalEvent;

        let events = vec![
            PipelineEvent::PipelineStarted {
                pipeline_name: "p".into(),
                node_count: 5,
            },
            PipelineEvent::PipelineCompleted {
                pipeline_name: "p".into(),
                completed_nodes: vec!["start".into(), "done".into()],
                duration_ms: 42,
            },
            PipelineEvent::PipelineFailed {
                pipeline_name: "p".into(),
                error: "boom".into(),
            },
            PipelineEvent::StageStarted {
                node_id: "n".into(),
                handler_type: "codergen".into(),
            },
            PipelineEvent::StageCompleted {
                node_id: "n".into(),
                status: "success".into(),
                duration_ms: 7,
            },
            PipelineEvent::StageFailed {
                node_id: "n".into(),
                error: "bad".into(),
            },
            PipelineEvent::StageRetrying {
                node_id: "n".into(),
                attempt: 2,
            },
            PipelineEvent::EdgeSelected {
                from_node: "a".into(),
                to_node: "b".into(),
                edge_label: Some("yes".into()),
            },
            PipelineEvent::EdgeSelected {
                from_node: "a".into(),
                to_node: "b".into(),
                edge_label: None,
            },
            PipelineEvent::GoalGateChecked {
                node_id: "n".into(),
                satisfied: true,
            },
            PipelineEvent::CheckpointSaved {
                node_id: "n".into(),
            },
            PipelineEvent::ContextUpdated {
                node_id: "n".into(),
                keys: vec!["k1".into(), "k2".into()],
            },
            PipelineEvent::CommitsCreated {
                node_id: "n".into(),
                task_id: Some("e.1".into()),
                commits: vec![
                    CommitRef {
                        sha: "bbb".into(),
                        subject: "two".into(),
                        author: "Ann".into(),
                        ts: "2026-09-25T10:00:01+00:00".into(),
                    },
                    CommitRef {
                        sha: "aaa".into(),
                        subject: "one".into(),
                        author: "Ann".into(),
                        ts: "2026-09-25T10:00:00+00:00".into(),
                    },
                ],
            },
            PipelineEvent::CommitsCreated {
                node_id: "n".into(),
                task_id: None,
                commits: vec![CommitRef {
                    sha: "aaa".into(),
                    subject: "one".into(),
                    author: "Ann".into(),
                    ts: "2026-09-25T10:00:00+00:00".into(),
                }],
            },
        ];

        for event in events {
            let serde_json::Value::Object(tagged) = serde_json::to_value(&event).unwrap() else {
                panic!("PipelineEvent is externally tagged");
            };
            let (name, payload) = tagged.into_iter().next().unwrap();

            let data = event.to_journal_data();
            assert_eq!(data.type_name(), name);
            let line =
                serde_json::to_value(JournalEvent::new(1, chrono::Utc::now(), "run", 1, data))
                    .unwrap();
            assert_eq!(line["type"], serde_json::Value::String(name.clone()));
            assert_eq!(line["data"], payload, "fields of {name}");
        }
    }
}
