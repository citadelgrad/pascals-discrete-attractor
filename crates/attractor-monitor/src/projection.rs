//! Projection: a pure fold from Run Journal Events to a [`RunView`].
//!
//! Reads only journal Events (ADR 0001). No clock, no I/O. Never fails: odd
//! orderings yield a view, and Event types this version does not know change
//! nothing at all.

use std::collections::BTreeMap;

use attractor_journal::{AttemptEndReason, EventData, JournalEvent};
use chrono::{DateTime, Utc};

/// Status of a Run as far as its Events alone can tell.
///
/// `Crashed` needs a clock and a PID probe, so it is not produced here; use
/// `attractor_journal::derive_status` (or the Findings rules) for that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewStatus {
    /// No Events.
    #[default]
    Unknown,
    Running,
    Completed,
    Failed,
    Stopped,
}

impl ViewStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AttemptEnd {
    pub reason: AttemptEndReason,
    pub message: Option<String>,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AttemptView {
    pub attempt: u32,
    pub pid: Option<u32>,
    pub pas_version: Option<String>,
    pub argv: Vec<String>,
    pub git_head: Option<String>,
    pub resumed_from_node: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended: Option<AttemptEnd>,
    pub last_heartbeat: Option<DateTime<Utc>>,
    pub last_event_at: DateTime<Utc>,
    /// Like `last_event_at`, but Heartbeats do not move it.
    pub last_activity_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodeVisit {
    pub node_id: String,
    pub visits: u32,
    pub last_status: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TaskBlocked {
    pub open: Vec<String>,
    pub blocked_by: BTreeMap<String, Vec<String>>,
    pub seq: u64,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TaskView {
    pub id: String,
    pub title: String,
    /// `closed`, `in_progress` or whatever the Epic snapshot reported.
    pub status: String,
    pub node_id: Option<String>,
    pub reason: Option<String>,
    pub upstream_verified: Option<bool>,
    /// SHAs from `TaskClosed`.
    pub commits: Vec<String>,
    /// Visits per node while this Task was current.
    pub node_visits: BTreeMap<String, u32>,
}

impl TaskView {
    pub fn is_closed(&self) -> bool {
        self.status == "closed"
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpicProgress {
    pub epic_id: String,
    pub title: String,
    pub closed: usize,
    pub total: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InvocationView {
    pub invocation_id: String,
    pub node_id: String,
    pub task_id: Option<String>,
    pub provider: String,
    pub model_requested: Option<String>,
    pub model_actual: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
    pub duration_ms: u64,
    pub transcript: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CommitView {
    pub sha: String,
    pub subject: String,
    pub author: String,
    pub ts: String,
    pub node_id: String,
    pub task_id: Option<String>,
}

/// A pending Human Gate.
#[derive(Debug, Clone, PartialEq)]
pub struct GateView {
    pub question_id: String,
    pub node_id: String,
    pub text: String,
    pub choices: Vec<String>,
    pub default: Option<String>,
    pub requested_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AnsweredGate {
    pub question_id: String,
    pub choice: String,
    pub source: attractor_journal::AnswerSource,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StageFact {
    pub node_id: String,
    /// Error text for failures; the retry number for retries.
    pub detail: String,
    /// Retry number for `StageRetrying`; `None` for failures.
    pub attempt: Option<usize>,
    pub seq: u64,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct RunView {
    pub run_id: Option<String>,
    pub last_seq: u64,
    pub last_ts: Option<DateTime<Utc>>,
    pub status: ViewStatus,
    pub pipeline_name: Option<String>,
    pub pipeline_path: Option<String>,
    pub workdir: Option<String>,
    pub epic_id: Option<String>,
    pub max_budget_usd: Option<f64>,
    pub max_steps: Option<u64>,
    pub shared_workdir: bool,
    /// Ordered by Attempt number.
    pub attempts: Vec<AttemptView>,
    pub current_node: Option<String>,
    pub visited: Vec<NodeVisit>,
    /// Number of `StageStarted` Events. The engine increments its step count
    /// once per node execution, just before the stage starts, so this is the
    /// number `max_steps` is compared against.
    pub steps: u64,
    pub cost_usd: f64,
    pub invocations: Vec<InvocationView>,
    pub retries: Vec<StageFact>,
    pub failures: Vec<StageFact>,
    pub pipeline_error: Option<String>,
    pub tasks: Vec<TaskView>,
    pub current_task: Option<String>,
    pub task_blocked: Option<TaskBlocked>,
    pub epic_title: Option<String>,
    /// IDs of the children of the latest `EpicSnapshot`.
    epic_children: Vec<String>,
    pub commits: Vec<CommitView>,
    pub gate: Option<GateView>,
    pub answered: Vec<AnsweredGate>,
    pub stop_requested: Option<String>,
}

/// Fold Events, in journal order, into a [`RunView`].
pub fn fold<'a>(events: impl IntoIterator<Item = &'a JournalEvent>) -> RunView {
    let mut view = RunView::default();
    for event in events {
        view.apply(event);
    }
    view
}

impl RunView {
    /// Epic progress: closed children over all children of the latest
    /// `EpicSnapshot`, with later `TaskClosed` Events applied. `None` before
    /// any snapshot.
    pub fn epic(&self) -> Option<EpicProgress> {
        let epic_id = self.epic_id.clone()?;
        let closed = self
            .epic_children
            .iter()
            .filter(|id| self.task(id).is_some_and(TaskView::is_closed))
            .count();
        Some(EpicProgress {
            epic_id,
            title: self.epic_title.clone().unwrap_or_default(),
            closed,
            total: self.epic_children.len(),
        })
    }

    pub fn task(&self, id: &str) -> Option<&TaskView> {
        self.tasks.iter().find(|t| t.id == id)
    }

    fn task_mut(&mut self, id: &str) -> &mut TaskView {
        if let Some(i) = self.tasks.iter().position(|t| t.id == id) {
            return &mut self.tasks[i];
        }
        self.tasks.push(TaskView {
            id: id.to_string(),
            ..TaskView::default()
        });
        self.tasks.last_mut().expect("just pushed")
    }

    fn attempt_mut(&mut self, attempt: u32, ts: DateTime<Utc>) -> &mut AttemptView {
        let i = match self.attempts.iter().position(|a| a.attempt == attempt) {
            Some(i) => i,
            None => {
                let at = self
                    .attempts
                    .iter()
                    .position(|a| a.attempt > attempt)
                    .unwrap_or(self.attempts.len());
                self.attempts.insert(
                    at,
                    AttemptView {
                        attempt,
                        pid: None,
                        pas_version: None,
                        argv: Vec::new(),
                        git_head: None,
                        resumed_from_node: None,
                        started_at: None,
                        ended: None,
                        last_heartbeat: None,
                        last_event_at: ts,
                        last_activity_at: ts,
                    },
                );
                at
            }
        };
        &mut self.attempts[i]
    }

    fn visit(&mut self, node_id: &str, status: Option<&str>) {
        let i = match self.visited.iter().position(|v| v.node_id == node_id) {
            Some(i) => i,
            None => {
                self.visited.push(NodeVisit {
                    node_id: node_id.to_string(),
                    visits: 0,
                    last_status: None,
                });
                self.visited.len() - 1
            }
        };
        match status {
            Some(s) => self.visited[i].last_status = Some(s.to_string()),
            None => self.visited[i].visits += 1,
        }
    }

    fn recompute_status(&mut self) {
        self.status = match self.attempts.last().and_then(|a| a.ended.as_ref()) {
            None => ViewStatus::Running,
            Some(end) => match end.reason {
                AttemptEndReason::Completed => ViewStatus::Completed,
                AttemptEndReason::Stopped => ViewStatus::Stopped,
                AttemptEndReason::Failed
                | AttemptEndReason::Error
                | AttemptEndReason::BudgetExhausted
                | AttemptEndReason::MaxSteps => ViewStatus::Failed,
            },
        };
    }

    /// Apply one Event. Unknown Events return before any bookkeeping.
    pub fn apply(&mut self, event: &JournalEvent) {
        if event.data.is_unknown() {
            return;
        }
        self.run_id.get_or_insert_with(|| event.run_id.clone());
        self.last_seq = self.last_seq.max(event.seq);
        self.last_ts = Some(self.last_ts.map_or(event.ts, |t| t.max(event.ts)));
        let (seq, ts) = (event.seq, event.ts);
        let attempt = self.attempt_mut(event.attempt, ts);
        attempt.last_event_at = attempt.last_event_at.max(ts);
        if !matches!(event.data, EventData::Heartbeat { .. }) {
            attempt.last_activity_at = attempt.last_activity_at.max(ts);
        }

        match &event.data {
            EventData::RunStarted {
                pipeline_name,
                pipeline_path,
                workdir,
                epic_id,
                max_budget_usd,
                max_steps,
                shared_workdir,
            } => {
                self.pipeline_name = Some(pipeline_name.clone());
                self.pipeline_path = Some(pipeline_path.clone());
                self.workdir = Some(workdir.clone());
                self.epic_id = self.epic_id.take().or_else(|| epic_id.clone());
                self.max_budget_usd = *max_budget_usd;
                self.max_steps = *max_steps;
                self.shared_workdir = *shared_workdir;
            }
            EventData::AttemptStarted {
                pid,
                pas_version,
                argv,
                git_head,
                resumed_from_node,
                ..
            } => {
                let a = self.attempt_mut(event.attempt, ts);
                a.pid = Some(*pid);
                a.pas_version = Some(pas_version.clone());
                a.argv = argv.clone();
                a.git_head = git_head.clone();
                a.resumed_from_node = resumed_from_node.clone();
                a.started_at = Some(ts);
            }
            EventData::AttemptEnded {
                reason, message, ..
            } => {
                self.attempt_mut(event.attempt, ts).ended = Some(AttemptEnd {
                    reason: *reason,
                    message: message.clone(),
                    at: ts,
                });
                self.current_node = None;
                // A Human Gate cannot be answered once its Attempt is over.
                self.gate = None;
            }
            EventData::Heartbeat { .. } => {
                self.attempt_mut(event.attempt, ts).last_heartbeat = Some(ts);
            }
            EventData::PipelineCompleted { .. } => self.current_node = None,
            EventData::PipelineFailed { error, .. } => {
                self.current_node = None;
                self.pipeline_error = Some(error.clone());
            }
            EventData::StageStarted { node_id, .. } => {
                self.steps += 1;
                self.current_node = Some(node_id.clone());
                self.visit(node_id, None);
                if let Some(t) = self.current_task.clone() {
                    *self
                        .task_mut(&t)
                        .node_visits
                        .entry(node_id.clone())
                        .or_insert(0) += 1;
                }
            }
            EventData::StageCompleted {
                node_id, status, ..
            } => self.visit(node_id, Some(status)),
            EventData::StageFailed { node_id, error } => {
                self.visit(node_id, Some("failed"));
                self.failures.push(StageFact {
                    node_id: node_id.clone(),
                    detail: error.clone(),
                    attempt: None,
                    seq,
                });
            }
            EventData::StageRetrying { node_id, attempt } => self.retries.push(StageFact {
                node_id: node_id.clone(),
                detail: attempt.to_string(),
                attempt: Some(*attempt),
                seq,
            }),
            EventData::EpicSnapshot {
                epic_id,
                title,
                tasks,
            } => {
                let mut old = std::mem::take(&mut self.tasks);
                self.tasks = tasks
                    .iter()
                    .map(|s| {
                        let mut t = old
                            .iter()
                            .position(|o| o.id == s.id)
                            .map(|i| old.swap_remove(i))
                            .unwrap_or_default();
                        t.id = s.id.clone();
                        t.title = s.title.clone();
                        t.status = s.status.clone();
                        t
                    })
                    .collect();
                self.epic_children = tasks.iter().map(|s| s.id.clone()).collect();
                self.epic_id = Some(epic_id.clone());
                self.epic_title = Some(title.clone());
            }
            EventData::TaskClaimed {
                task_id,
                title,
                node_id,
                ..
            } => {
                let t = self.task_mut(task_id);
                t.title = title.clone();
                t.status = "in_progress".into();
                t.node_id = Some(node_id.clone());
                self.current_task = Some(task_id.clone());
                self.task_blocked = None;
            }
            EventData::TaskSelectionBlocked {
                open, blocked_by, ..
            } => {
                self.task_blocked = Some(TaskBlocked {
                    open: open.clone(),
                    blocked_by: blocked_by.clone(),
                    seq,
                });
            }
            EventData::TaskClosed {
                task_id,
                reason,
                upstream_verified,
                commits,
            } => {
                let t = self.task_mut(task_id);
                t.status = "closed".into();
                t.reason = Some(reason.clone());
                t.upstream_verified = Some(*upstream_verified);
                t.commits = commits.clone();
                if self.current_task.as_deref() == Some(task_id) {
                    self.current_task = None;
                }
            }
            EventData::LlmInvoked {
                invocation_id,
                node_id,
                provider,
                model_requested,
                model_actual,
                input_tokens,
                output_tokens,
                cost_usd,
                duration_ms,
                transcript,
                status,
            } => {
                self.cost_usd += cost_usd.unwrap_or(0.0);
                self.invocations.push(InvocationView {
                    invocation_id: invocation_id.clone(),
                    node_id: node_id.clone(),
                    task_id: self.current_task.clone(),
                    provider: provider.clone(),
                    model_requested: model_requested.clone(),
                    model_actual: model_actual.clone(),
                    input_tokens: *input_tokens,
                    output_tokens: *output_tokens,
                    cost_usd: *cost_usd,
                    duration_ms: *duration_ms,
                    transcript: transcript.clone(),
                    status: status.clone(),
                });
            }
            EventData::CommitsCreated {
                node_id,
                task_id,
                commits,
            } => {
                for c in commits {
                    if self.commits.iter().any(|x| x.sha == c.sha) {
                        continue;
                    }
                    self.commits.push(CommitView {
                        sha: c.sha.clone(),
                        subject: c.subject.clone(),
                        author: c.author.clone(),
                        ts: c.ts.clone(),
                        node_id: node_id.clone(),
                        task_id: task_id.clone(),
                    });
                }
            }
            EventData::HumanInputRequested {
                question_id,
                node_id,
                text,
                choices,
                default,
            } => {
                self.gate = Some(GateView {
                    question_id: question_id.clone(),
                    node_id: node_id.clone(),
                    text: text.clone(),
                    choices: choices.clone(),
                    default: default.clone(),
                    requested_at: ts,
                });
            }
            EventData::HumanInputAnswered {
                question_id,
                choice,
                source,
            } => {
                if self
                    .gate
                    .as_ref()
                    .is_some_and(|g| &g.question_id == question_id)
                {
                    self.gate = None;
                }
                self.answered.push(AnsweredGate {
                    question_id: question_id.clone(),
                    choice: choice.clone(),
                    source: *source,
                });
            }
            EventData::StopRequested { source } => self.stop_requested = Some(source.clone()),
            EventData::PipelineStarted { .. }
            | EventData::EdgeSelected { .. }
            | EventData::GoalGateChecked { .. }
            | EventData::CheckpointSaved { .. }
            | EventData::ContextUpdated { .. }
            | EventData::Unknown { .. } => {}
        }
        self.recompute_status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use attractor_journal::{AnswerSource, CommitRef, TaskSummary};
    use chrono::TimeZone;

    fn ev(seq: u64, attempt: u32, data: EventData) -> JournalEvent {
        let base = Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap();
        JournalEvent::new(
            seq,
            base + chrono::Duration::seconds(seq as i64),
            "run-1",
            attempt,
            data,
        )
    }

    fn stage(node: &str) -> EventData {
        EventData::StageStarted {
            node_id: node.into(),
            handler_type: "codergen".into(),
        }
    }

    fn llm(id: &str, cost: Option<f64>) -> EventData {
        EventData::LlmInvoked {
            invocation_id: id.into(),
            node_id: "n".into(),
            provider: "anthropic".into(),
            model_requested: Some("opus".into()),
            model_actual: Some("sonnet".into()),
            input_tokens: Some(1),
            output_tokens: Some(2),
            cost_usd: cost,
            duration_ms: 5,
            transcript: format!("t/{id}.jsonl"),
            status: "ok".into(),
        }
    }

    fn started(pid: u32) -> EventData {
        EventData::AttemptStarted {
            attempt: 0,
            pid,
            pas_version: "1".into(),
            argv: vec!["pas".into()],
            git_head: None,
            resumed_from_node: None,
        }
    }

    fn ended(reason: AttemptEndReason) -> EventData {
        EventData::AttemptEnded {
            attempt: 0,
            reason,
            message: None,
        }
    }

    fn claim(id: &str) -> EventData {
        EventData::TaskClaimed {
            task_id: id.into(),
            title: format!("title {id}"),
            epic_id: "E".into(),
            node_id: "pick".into(),
        }
    }

    fn close(id: &str) -> EventData {
        EventData::TaskClosed {
            task_id: id.into(),
            reason: "done".into(),
            upstream_verified: true,
            commits: vec!["abc".into()],
        }
    }

    fn snapshot(tasks: &[(&str, &str)]) -> EventData {
        EventData::EpicSnapshot {
            epic_id: "E".into(),
            title: "Epic".into(),
            tasks: tasks
                .iter()
                .map(|(id, status)| TaskSummary {
                    id: (*id).into(),
                    title: format!("title {id}"),
                    status: (*status).into(),
                })
                .collect(),
        }
    }

    fn number(events: Vec<(u32, EventData)>) -> Vec<JournalEvent> {
        events
            .into_iter()
            .enumerate()
            .map(|(i, (a, d))| ev(i as u64 + 1, a, d))
            .collect()
    }

    fn gate_request(q: &str) -> EventData {
        EventData::HumanInputRequested {
            question_id: q.into(),
            node_id: "ask".into(),
            text: "Proceed?".into(),
            choices: vec!["yes".into(), "no".into()],
            default: Some("yes".into()),
        }
    }

    fn unknown() -> EventData {
        EventData::Unknown {
            type_name: "Future".into(),
            data: serde_json::json!({"x": 1}),
        }
    }

    fn completed_journal() -> Vec<JournalEvent> {
        number(vec![
            (1, started(10)),
            (1, stage("a")),
            (1, llm("i1", Some(0.25))),
            (
                1,
                EventData::StopRequested {
                    source: "cli".into(),
                },
            ),
            (1, ended(AttemptEndReason::Stopped)),
            (2, started(11)),
            (2, stage("b")),
            (2, llm("i2", Some(0.5))),
            (2, stage("c")),
            (2, ended(AttemptEndReason::Completed)),
        ])
    }

    #[test]
    fn completed_run_folds_to_completed() {
        let v = fold(&completed_journal());
        assert_eq!(v.status, ViewStatus::Completed);
        assert_eq!(v.attempts.len(), 2);
        assert!((v.cost_usd - 0.75).abs() < 1e-9);
        assert_eq!(v.steps, 3);
        assert_eq!(v.run_id.as_deref(), Some("run-1"));
        assert_eq!(v.last_seq, 10);
        assert_eq!(v.current_node, None);
    }

    #[test]
    fn last_attempt_decides_status() {
        for (r, want) in [
            (AttemptEndReason::Failed, ViewStatus::Failed),
            (AttemptEndReason::BudgetExhausted, ViewStatus::Failed),
            (AttemptEndReason::MaxSteps, ViewStatus::Failed),
            (AttemptEndReason::Error, ViewStatus::Failed),
            (AttemptEndReason::Stopped, ViewStatus::Stopped),
        ] {
            let ev = number(vec![(1, started(1)), (1, ended(r))]);
            assert_eq!(fold(&ev).status, want, "{r:?}");
        }
        // A resumed Attempt that has not ended makes the Run Running again.
        let ev = number(vec![
            (1, started(1)),
            (1, ended(AttemptEndReason::Failed)),
            (2, started(2)),
        ]);
        assert_eq!(fold(&ev).status, ViewStatus::Running);
        // No AttemptEnded (killed process) stays Running.
        let ev = number(vec![(1, started(1)), (1, stage("a"))]);
        assert_eq!(fold(&ev).status, ViewStatus::Running);
    }

    #[test]
    fn missing_attempt_started_still_counts_attempt() {
        let v = fold(&number(vec![(1, stage("a")), (2, stage("b"))]));
        assert_eq!(v.attempts.len(), 2);
    }

    #[test]
    fn current_task_between_claim_and_close() {
        let all = number(vec![
            (1, snapshot(&[("X", "open"), ("Y", "open")])),
            (1, claim("X")),
            (1, stage("code")),
            (1, close("X")),
        ]);
        let v = fold(&all[..2]);
        assert_eq!(v.current_task.as_deref(), Some("X"));
        assert_eq!(v.task("X").unwrap().status, "in_progress");

        let v = fold(&all[..3]);
        assert_eq!(v.task("X").unwrap().node_visits["code"], 1);

        let v = fold(&all);
        assert_eq!(v.current_task, None);
        let x = v.task("X").unwrap();
        assert!(x.is_closed());
        assert_eq!(x.reason.as_deref(), Some("done"));
        assert_eq!(x.commits, vec!["abc".to_string()]);
        // Per-Task visit counts survive the close.
        assert_eq!(x.node_visits["code"], 1);

        // Closing a different Task leaves the current Task alone.
        let v = fold(&number(vec![(1, claim("X")), (1, close("Y"))]));
        assert_eq!(v.current_task.as_deref(), Some("X"));
    }

    #[test]
    fn selection_blocked_clears_on_claim() {
        let blocked = EventData::TaskSelectionBlocked {
            epic_id: "E".into(),
            open: vec!["A".into()],
            blocked_by: BTreeMap::from([("A".into(), vec!["B".into()])]),
        };
        let v = fold(&number(vec![(1, blocked.clone())]));
        assert_eq!(v.task_blocked.unwrap().open, vec!["A".to_string()]);
        let v = fold(&number(vec![(1, blocked), (1, claim("A"))]));
        assert!(v.task_blocked.is_none());
    }

    #[test]
    fn epic_progress_from_latest_snapshot_plus_closes() {
        let epic = |evs: Vec<(u32, EventData)>| fold(&number(evs)).epic();
        assert_eq!(epic(vec![(1, close("A"))]), None);

        let snap = snapshot(&[("A", "closed"), ("B", "open"), ("C", "open"), ("D", "open")]);
        let e = epic(vec![(1, snap.clone()), (1, close("B")), (1, close("C"))]).unwrap();
        assert_eq!((e.closed, e.total), (3, 4));
        assert_eq!(e.epic_id, "E");

        // Duplicate close does not double count.
        let e = epic(vec![(1, snap.clone()), (1, close("B")), (1, close("B"))]).unwrap();
        assert_eq!((e.closed, e.total), (2, 4));

        // A close for an id outside the snapshot is ignored for the count.
        let e = epic(vec![(1, snap.clone()), (1, close("ZZ"))]).unwrap();
        assert_eq!((e.closed, e.total), (1, 4));

        // The latest snapshot resets the baseline; earlier closes do not count.
        let snap2 = snapshot(&[
            ("A", "closed"),
            ("B", "open"),
            ("C", "closed"),
            ("D", "open"),
            ("E1", "open"),
        ]);
        let e = epic(vec![
            (1, snap),
            (1, close("B")),
            (1, close("D")),
            (2, snap2),
            (2, close("E1")),
        ])
        .unwrap();
        assert_eq!((e.closed, e.total), (3, 5));
        assert!(e.closed <= e.total);
    }

    #[test]
    fn gate_pending_until_answered() {
        let req = ev(1, 1, gate_request("q1"));
        let v = fold([&req]);
        let g = v.gate.as_ref().unwrap();
        assert_eq!(g.question_id, "q1");
        assert_eq!(g.choices, vec!["yes".to_string(), "no".to_string()]);
        assert_eq!(g.default.as_deref(), Some("yes"));

        let answer = |q: &str| {
            ev(
                2,
                1,
                EventData::HumanInputAnswered {
                    question_id: q.into(),
                    choice: "yes".into(),
                    source: AnswerSource::Monitor,
                },
            )
        };
        let v = fold([&req, &answer("other")]);
        assert!(v.gate.is_some());
        let v = fold([&req, &answer("q1")]);
        assert!(v.gate.is_none());
        assert_eq!(v.answered[0].source, AnswerSource::Monitor);

        // An answer with no request is harmless.
        assert!(fold([&answer("q1")]).gate.is_none());

        // A Human Gate cannot outlive its Attempt.
        let end = ev(3, 1, ended(AttemptEndReason::Stopped));
        assert!(fold([&req, &end]).gate.is_none());
    }

    #[test]
    fn unknown_events_do_not_change_view() {
        let base = completed_journal();
        let want = fold(&base);
        let mut with = vec![ev(100, 9, unknown())];
        with.extend(base[..4].iter().cloned());
        with.push(ev(101, 9, unknown()));
        with.extend(base[4..].iter().cloned());
        with.push(ev(102, 9, unknown()));
        assert_eq!(fold(&with), want);

        let only = vec![ev(1, 1, unknown()), ev(2, 1, unknown())];
        assert_eq!(fold(&only), RunView::default());

        let line = r#"{"v":1,"seq":5,"ts":"2026-09-01T00:00:00.000Z","run_id":"r","attempt":1,"type":"FutureThing","data":{"a":1}}"#;
        let decoded: JournalEvent = serde_json::from_str(line).unwrap();
        assert!(decoded.data.is_unknown());
        assert_eq!(fold([&decoded]), RunView::default());
    }

    #[test]
    fn empty_journal_is_unknown_status() {
        let v = fold(&[]);
        assert_eq!(v, RunView::default());
        assert_eq!(v.status, ViewStatus::Unknown);
        assert_eq!(v.status.as_str(), "unknown");
        assert!(v.attempts.is_empty());
        assert!(v.epic().is_none());
    }

    #[test]
    fn visited_nodes_keep_order_and_counts() {
        let v = fold(&number(vec![
            (1, stage("a")),
            (1, stage("b")),
            (1, stage("a")),
            (
                1,
                EventData::StageCompleted {
                    node_id: "a".into(),
                    status: "success".into(),
                    duration_ms: 1,
                },
            ),
        ]));
        let ids: Vec<_> = v
            .visited
            .iter()
            .map(|n| (n.node_id.as_str(), n.visits))
            .collect();
        assert_eq!(ids, vec![("a", 2), ("b", 1)]);
        assert_eq!(v.visited[0].last_status.as_deref(), Some("success"));
        assert_eq!(v.current_node.as_deref(), Some("a"));
    }

    #[test]
    fn invocations_commits_and_failures() {
        let commit = |sha: &str| CommitRef {
            sha: sha.into(),
            subject: "s".into(),
            author: "a".into(),
            ts: "2026-09-01T00:00:00Z".into(),
        };
        let v = fold(&number(vec![
            (1, claim("X")),
            (1, llm("i1", None)),
            (
                1,
                EventData::CommitsCreated {
                    node_id: "n".into(),
                    task_id: Some("X".into()),
                    commits: vec![commit("c1"), commit("c2")],
                },
            ),
            (
                1,
                EventData::CommitsCreated {
                    node_id: "n".into(),
                    task_id: Some("X".into()),
                    commits: vec![commit("c2")],
                },
            ),
            (
                1,
                EventData::StageFailed {
                    node_id: "n".into(),
                    error: "boom".into(),
                },
            ),
            (
                1,
                EventData::StageRetrying {
                    node_id: "n".into(),
                    attempt: 2,
                },
            ),
            (
                1,
                EventData::PipelineFailed {
                    pipeline_name: "p".into(),
                    error: "bad".into(),
                },
            ),
        ]));
        assert_eq!(v.cost_usd, 0.0);
        let inv = &v.invocations[0];
        assert_eq!(inv.model_requested.as_deref(), Some("opus"));
        assert_eq!(inv.model_actual.as_deref(), Some("sonnet"));
        assert_eq!(inv.task_id.as_deref(), Some("X"));
        assert_eq!(v.commits.len(), 2);
        assert_eq!(v.failures[0].detail, "boom");
        assert_eq!(v.retries[0].detail, "2");
        assert_eq!(v.pipeline_error.as_deref(), Some("bad"));
    }

    #[test]
    fn incremental_apply_matches_fold() {
        let evs = completed_journal();
        let mut v = RunView::default();
        for e in &evs {
            v.apply(e);
        }
        assert_eq!(v, fold(&evs));
    }

    #[test]
    fn odd_orderings_do_not_panic() {
        let v = fold(&number(vec![
            (3, ended(AttemptEndReason::Completed)),
            (1, close("Q")),
            (2, EventData::Heartbeat { pid: 1 }),
        ]));
        assert_eq!(
            v.attempts.iter().map(|a| a.attempt).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(v.status, ViewStatus::Completed);
    }

    #[test]
    fn heartbeats_do_not_move_last_activity_and_retries_carry_attempt() {
        let view = fold(&number(vec![
            (0, started(1)),
            (0, stage("n")),
            (0, EventData::Heartbeat { pid: 1 }),
            (
                0,
                EventData::StageRetrying {
                    node_id: "n".into(),
                    attempt: 3,
                },
            ),
            (0, EventData::Heartbeat { pid: 1 }),
        ]));
        let a = &view.attempts[0];
        assert_eq!(a.last_event_at, a.last_heartbeat.unwrap());
        assert!(a.last_activity_at < a.last_event_at);
        assert_eq!(view.retries[0].attempt, Some(3));
    }
}
