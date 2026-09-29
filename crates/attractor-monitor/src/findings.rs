//! Findings: pure rules over a [`RunView`] (PRD FR-11).
//!
//! Findings are derived, never stored, so a rule change applies to old Runs.
//! No clock and no I/O: time comes in as `now`, and the two facts the journal
//! cannot give (is the PID alive, what is a node's timeout) come in through
//! [`Env`]. This is one argument more than the spec's `derive(&RunView, now)`.
//! Output order is fixed: rules in [`Rule`] order, then view order.

use std::collections::BTreeSet;

use attractor_journal::CRASH_AFTER;
use chrono::{DateTime, Duration, Utc};

use crate::projection::{AttemptView, RunView};

/// One node visited more than this many times within one Task is suspicious.
pub const LOOP_K: u32 = 5;
/// Spend or steps at or above this share of the limit raise a Budget Finding.
pub const BUDGET_PERCENT: u32 = 80;
/// Node timeout used when [`Env::node_timeout`] knows nothing (the Monitor
/// cannot read the Pipeline, ADR 0001).
pub const DEFAULT_NODE_TIMEOUT: Duration = Duration::seconds(600);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Info,
    Warn,
    Critical,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Critical => "critical",
        }
    }
}

/// Declaration order is the output order of [`derive`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rule {
    Crashed,
    Stalled,
    StageFailed,
    Retrying,
    LoopSuspicion,
    Budget,
    Blocked,
    ModelMismatch,
    UnsafeCommit,
    SharedWorkdir,
    HumanGateWaiting,
}

impl Rule {
    pub const ALL: [Rule; 11] = [
        Self::Crashed,
        Self::Stalled,
        Self::StageFailed,
        Self::Retrying,
        Self::LoopSuspicion,
        Self::Budget,
        Self::Blocked,
        Self::ModelMismatch,
        Self::UnsafeCommit,
        Self::SharedWorkdir,
        Self::HumanGateWaiting,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Crashed => "crashed",
            Self::Stalled => "stalled",
            Self::StageFailed => "stage_failed",
            Self::Retrying => "retrying",
            Self::LoopSuspicion => "loop_suspicion",
            Self::Budget => "budget",
            Self::Blocked => "blocked",
            Self::ModelMismatch => "model_mismatch",
            Self::UnsafeCommit => "unsafe_commit",
            Self::SharedWorkdir => "shared_workdir",
            Self::HumanGateWaiting => "human_gate_waiting",
        }
    }

    /// The only place a severity is decided (FR-11 table).
    pub fn severity(self) -> Severity {
        match self {
            Self::Crashed | Self::StageFailed | Self::UnsafeCommit => Severity::Critical,
            Self::Stalled
            | Self::Retrying
            | Self::LoopSuspicion
            | Self::Budget
            | Self::Blocked
            | Self::ModelMismatch
            | Self::SharedWorkdir => Severity::Warn,
            Self::HumanGateWaiting => Severity::Info,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub rule: Rule,
    pub severity: Severity,
    pub message: String,
    pub node_id: Option<String>,
    pub task_id: Option<String>,
    /// Journal `seq` of the Event that caused it, when there is one.
    pub seq: Option<u64>,
}

impl Finding {
    fn new(rule: Rule, message: String) -> Self {
        Self {
            rule,
            severity: rule.severity(),
            message,
            node_id: None,
            task_id: None,
            seq: None,
        }
    }

    fn node(mut self, node_id: &str) -> Self {
        self.node_id = Some(node_id.to_string());
        self
    }

    fn task(mut self, task_id: Option<&str>) -> Self {
        self.task_id = task_id.map(str::to_string);
        self
    }

    fn at(mut self, seq: u64) -> Self {
        self.seq = Some(seq);
        self
    }
}

/// Facts the journal cannot supply.
pub struct Env<'a> {
    /// Is this PID a live process?
    pub pid_alive: &'a dyn Fn(u32) -> bool,
    /// Timeout of a node, when known.
    pub node_timeout: &'a dyn Fn(&str) -> Option<Duration>,
    /// Nodes allowed to commit. `None` means unknown: only the "Task closed
    /// without its commit on upstream" clause of Unsafe commit applies.
    pub commit_nodes: Option<&'a BTreeSet<String>>,
}

/// All Findings for `view` at time `now`.
pub fn derive(view: &RunView, now: DateTime<Utc>, env: &Env) -> Vec<Finding> {
    let mut out = Vec::new();
    let live = view.attempts.last().filter(|a| a.ended.is_none());
    let crashed = live.is_some_and(|a| is_crashed(a, now, env));
    if crashed {
        out.push(crashed_finding(live.expect("crashed implies attempt")));
    }
    if let (false, Some(a)) = (crashed, live) {
        out.extend(stalled(view, a, now, env));
    }
    out.extend(stage_failed(view));
    out.extend(retrying(view));
    out.extend(loop_suspicion(view));
    out.extend(budget(view));
    out.extend(blocked(view));
    out.extend(model_mismatch(view));
    out.extend(unsafe_commit(view, env));
    out.extend(shared_workdir(view));
    out.extend(human_gate_waiting(view));
    out
}

fn sign_of_life(a: &AttemptView) -> DateTime<Utc> {
    a.last_heartbeat.or(a.started_at).unwrap_or(a.last_event_at)
}

/// Same rule as `attractor_journal::derive_status`: strict `>`.
fn is_crashed(a: &AttemptView, now: DateTime<Utc>, env: &Env) -> bool {
    now - sign_of_life(a) > CRASH_AFTER && !a.pid.is_some_and(|p| (env.pid_alive)(p))
}

fn crashed_finding(a: &AttemptView) -> Finding {
    Finding::new(
        Rule::Crashed,
        format!(
            "Attempt {} stopped without ending: no heartbeat for over 2 minutes and its process is gone",
            a.attempt
        ),
    )
}

fn stalled(view: &RunView, a: &AttemptView, now: DateTime<Utc>, env: &Env) -> Option<Finding> {
    let node = view.current_node.as_deref()?;
    let beating = a.last_heartbeat.is_some_and(|h| now - h <= CRASH_AFTER);
    let timeout = (env.node_timeout)(node).unwrap_or(DEFAULT_NODE_TIMEOUT);
    (beating && now - a.last_activity_at > timeout).then(|| {
        Finding::new(
            Rule::Stalled,
            format!(
                "Node {node} is alive but has produced no Event for over {} s",
                timeout.num_seconds()
            ),
        )
        .node(node)
    })
}

fn stage_failed(view: &RunView) -> impl Iterator<Item = Finding> + '_ {
    view.failures.iter().map(|f| {
        Finding::new(
            Rule::StageFailed,
            format!("Node {} failed: {}", f.node_id, f.detail),
        )
        .node(&f.node_id)
        .at(f.seq)
    })
}

fn retrying(view: &RunView) -> impl Iterator<Item = Finding> + '_ {
    view.retries
        .iter()
        .filter(|f| f.attempt.is_some_and(|n| n >= 2))
        .map(|f| {
            Finding::new(
                Rule::Retrying,
                format!("Node {} is on retry {}", f.node_id, f.detail),
            )
            .node(&f.node_id)
            .at(f.seq)
        })
}

fn loop_suspicion(view: &RunView) -> impl Iterator<Item = Finding> + '_ {
    view.tasks.iter().flat_map(|t| {
        t.node_visits
            .iter()
            .filter(|(_, n)| **n > LOOP_K)
            .map(|(node, n)| {
                Finding::new(
                    Rule::LoopSuspicion,
                    format!("Node {node} visited {n} times within Task {}", t.id),
                )
                .node(node)
                .task(Some(&t.id))
            })
    })
}

fn budget(view: &RunView) -> Vec<Finding> {
    let mut out = Vec::new();
    if let Some(max) = view.max_budget_usd.filter(|m| *m > 0.0) {
        if view.cost_usd >= max * f64::from(BUDGET_PERCENT) / 100.0 {
            out.push(Finding::new(
                Rule::Budget,
                format!("Spent ${:.2} of the ${max:.2} budget", view.cost_usd),
            ));
        }
    }
    if let Some(max) = view.max_steps.filter(|m| *m > 0) {
        if view.steps.saturating_mul(100) >= max.saturating_mul(u64::from(BUDGET_PERCENT)) {
            out.push(Finding::new(
                Rule::Budget,
                format!("Used {} of {max} steps", view.steps),
            ));
        }
    }
    out
}

fn blocked(view: &RunView) -> Option<Finding> {
    view.task_blocked.as_ref().map(|b| {
        Finding::new(
            Rule::Blocked,
            format!("{} open Task(s), none ready", b.open.len()),
        )
        .at(b.seq)
    })
}

fn model_mismatch(view: &RunView) -> impl Iterator<Item = Finding> + '_ {
    view.invocations.iter().filter_map(|i| {
        let (req, act) = (i.model_requested.as_ref()?, i.model_actual.as_ref()?);
        (req != act).then(|| {
            Finding::new(
                Rule::ModelMismatch,
                format!("Requested model {req}, got {act}"),
            )
            .node(&i.node_id)
            .task(i.task_id.as_deref())
        })
    })
}

fn unsafe_commit(view: &RunView, env: &Env) -> Vec<Finding> {
    let mut out = Vec::new();
    if let Some(allowed) = env.commit_nodes {
        for c in view
            .commits
            .iter()
            .filter(|c| !allowed.contains(&c.node_id))
        {
            out.push(
                Finding::new(
                    Rule::UnsafeCommit,
                    format!("Commit {} came from non-commit node {}", c.sha, c.node_id),
                )
                .node(&c.node_id)
                .task(c.task_id.as_deref()),
            );
        }
    }
    for t in view
        .tasks
        .iter()
        .filter(|t| t.upstream_verified == Some(false))
    {
        out.push(
            Finding::new(
                Rule::UnsafeCommit,
                format!("Task {} closed without its commit on upstream", t.id),
            )
            .task(Some(&t.id)),
        );
    }
    out
}

fn shared_workdir(view: &RunView) -> Option<Finding> {
    view.shared_workdir.then(|| {
        Finding::new(
            Rule::SharedWorkdir,
            "Run started with the shared-workdir override".into(),
        )
    })
}

fn human_gate_waiting(view: &RunView) -> Option<Finding> {
    view.gate.as_ref().map(|g| {
        Finding::new(
            Rule::HumanGateWaiting,
            format!("Waiting for an answer at node {}", g.node_id),
        )
        .node(&g.node_id)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projection::fold;
    use attractor_journal::{CommitRef, EventData, JournalEvent, TaskSummary};
    use chrono::TimeZone;

    fn base() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap()
    }

    /// Events at `base + secs`, numbered from 1.
    fn journal(events: Vec<(i64, EventData)>) -> RunView {
        let evs: Vec<JournalEvent> = events
            .into_iter()
            .enumerate()
            .map(|(i, (secs, d))| {
                JournalEvent::new(
                    i as u64 + 1,
                    base() + Duration::seconds(secs),
                    "run-1",
                    0,
                    d,
                )
            })
            .collect();
        fold(&evs)
    }

    fn run_started(budget: Option<f64>, steps: Option<u64>, shared: bool) -> EventData {
        EventData::RunStarted {
            pipeline_name: "p".into(),
            pipeline_path: "p.dot".into(),
            workdir: "/w".into(),
            epic_id: None,
            max_budget_usd: budget,
            max_steps: steps,
            shared_workdir: shared,
        }
    }

    fn attempt_started() -> EventData {
        EventData::AttemptStarted {
            attempt: 0,
            pid: 42,
            pas_version: "1".into(),
            argv: vec![],
            git_head: None,
            resumed_from_node: None,
        }
    }

    fn heartbeat() -> EventData {
        EventData::Heartbeat { pid: 42 }
    }

    fn stage(node: &str) -> EventData {
        EventData::StageStarted {
            node_id: node.into(),
            handler_type: "codergen".into(),
        }
    }

    fn claim(id: &str) -> EventData {
        EventData::TaskClaimed {
            task_id: id.into(),
            title: id.into(),
            epic_id: "E".into(),
            node_id: "pick".into(),
        }
    }

    fn close(id: &str, upstream: bool) -> EventData {
        EventData::TaskClosed {
            task_id: id.into(),
            reason: "done".into(),
            upstream_verified: upstream,
            commits: vec![],
        }
    }

    fn llm(req: Option<&str>, act: Option<&str>, cost: Option<f64>) -> EventData {
        EventData::LlmInvoked {
            invocation_id: "i".into(),
            node_id: "n".into(),
            provider: "anthropic".into(),
            model_requested: req.map(Into::into),
            model_actual: act.map(Into::into),
            input_tokens: None,
            output_tokens: None,
            cost_usd: cost,
            duration_ms: 1,
            transcript: "t".into(),
            status: "ok".into(),
        }
    }

    fn commits(node: &str) -> EventData {
        EventData::CommitsCreated {
            node_id: node.into(),
            task_id: None,
            commits: vec![CommitRef {
                sha: "abc".into(),
                subject: "s".into(),
                author: "a".into(),
                ts: "t".into(),
            }],
        }
    }

    fn gone(_: u32) -> bool {
        false
    }
    fn alive(_: u32) -> bool {
        true
    }
    fn no_timeout(_: &str) -> Option<Duration> {
        None
    }

    fn run(view: &RunView, now_secs: i64, pid_alive: fn(u32) -> bool) -> Vec<Finding> {
        let env = Env {
            pid_alive: &pid_alive,
            node_timeout: &no_timeout,
            commit_nodes: None,
        };
        derive(view, base() + Duration::seconds(now_secs), &env)
    }

    fn rules(f: &[Finding]) -> Vec<(Rule, Severity)> {
        f.iter().map(|f| (f.rule, f.severity)).collect()
    }

    fn has(f: &[Finding], r: Rule) -> bool {
        f.iter().any(|f| f.rule == r)
    }

    fn visits(node: &str, n: usize) -> Vec<(i64, EventData)> {
        let mut v = vec![(0, claim("T1"))];
        v.extend((0..n).map(|_| (1, stage(node))));
        v
    }

    #[test]
    fn severities_match_fr11_table() {
        use Rule::*;
        use Severity::*;
        let table = [
            (Crashed, Critical),
            (Stalled, Warn),
            (StageFailed, Critical),
            (Retrying, Warn),
            (LoopSuspicion, Warn),
            (Budget, Warn),
            (Blocked, Warn),
            (ModelMismatch, Warn),
            (UnsafeCommit, Critical),
            (SharedWorkdir, Warn),
            (HumanGateWaiting, Info),
        ];
        assert_eq!(table.len(), Rule::ALL.len());
        for (rule, sev) in table {
            assert_eq!(rule.severity(), sev, "{rule:?}");
        }
        assert!(Rule::ALL.windows(2).all(|w| w[0] < w[1]));
    }

    /// (name, rule, matching journal, now, non-matching journal)
    #[test]
    fn each_rule_fires_with_its_severity_and_only_then() {
        type Row = (
            &'static str,
            Rule,
            Vec<(i64, EventData)>,
            i64,
            Vec<(i64, EventData)>,
            i64,
        );
        let rows: Vec<Row> = vec![
            (
                "crashed",
                Rule::Crashed,
                vec![(0, attempt_started()), (10, heartbeat())],
                10 + 121,
                vec![(0, attempt_started()), (10, heartbeat())],
                10 + 119,
            ),
            (
                "stalled",
                Rule::Stalled,
                vec![(0, attempt_started()), (1, stage("n")), (700, heartbeat())],
                701,
                vec![(0, attempt_started()), (1, stage("n")), (500, heartbeat())],
                501,
            ),
            (
                "stage failed",
                Rule::StageFailed,
                vec![(
                    0,
                    EventData::StageFailed {
                        node_id: "n".into(),
                        error: "boom".into(),
                    },
                )],
                1,
                vec![(0, stage("n"))],
                1,
            ),
            (
                "retrying",
                Rule::Retrying,
                vec![(
                    0,
                    EventData::StageRetrying {
                        node_id: "n".into(),
                        attempt: 2,
                    },
                )],
                1,
                vec![(
                    0,
                    EventData::StageRetrying {
                        node_id: "n".into(),
                        attempt: 1,
                    },
                )],
                1,
            ),
            (
                "loop suspicion",
                Rule::LoopSuspicion,
                visits("n", 6),
                2,
                visits("n", 5),
                2,
            ),
            (
                "budget",
                Rule::Budget,
                vec![
                    (0, run_started(Some(100.0), None, false)),
                    (1, llm(None, None, Some(80.0))),
                ],
                2,
                vec![
                    (0, run_started(Some(100.0), None, false)),
                    (1, llm(None, None, Some(79.9))),
                ],
                2,
            ),
            (
                "blocked",
                Rule::Blocked,
                vec![(
                    0,
                    EventData::TaskSelectionBlocked {
                        epic_id: "E".into(),
                        open: vec!["T1".into()],
                        blocked_by: Default::default(),
                    },
                )],
                1,
                vec![(0, claim("T1"))],
                1,
            ),
            (
                "model mismatch",
                Rule::ModelMismatch,
                vec![(0, llm(Some("opus"), Some("sonnet"), None))],
                1,
                vec![(0, llm(Some("opus"), Some("opus"), None))],
                1,
            ),
            (
                "unsafe commit",
                Rule::UnsafeCommit,
                vec![(0, claim("T1")), (1, close("T1", false))],
                2,
                vec![(0, claim("T1")), (1, close("T1", true))],
                2,
            ),
            (
                "shared workdir",
                Rule::SharedWorkdir,
                vec![(0, run_started(None, None, true))],
                1,
                vec![(0, run_started(None, None, false))],
                1,
            ),
            (
                "human gate",
                Rule::HumanGateWaiting,
                vec![(
                    0,
                    EventData::HumanInputRequested {
                        question_id: "q".into(),
                        node_id: "gate".into(),
                        text: "ok?".into(),
                        choices: vec![],
                        default: None,
                    },
                )],
                1,
                vec![],
                1,
            ),
        ];
        let mut seen = BTreeSet::new();
        for (name, rule, yes, yes_now, no, no_now) in rows {
            let hit = run(&journal(yes), yes_now, gone);
            let f: Vec<_> = hit.iter().filter(|f| f.rule == rule).collect();
            assert!(!f.is_empty(), "{name}: expected a Finding");
            assert!(f.iter().all(|f| f.severity == rule.severity()), "{name}");
            let miss = run(&journal(no), no_now, gone);
            assert!(!has(&miss, rule), "{name}: unexpected Finding");
            seen.insert(rule);
        }
        assert_eq!(seen.len(), Rule::ALL.len(), "every rule has a row");
    }

    #[test]
    fn crashed_boundary() {
        let j = || vec![(0, attempt_started()), (10, heartbeat())];
        let v = journal(j());
        // Heartbeat at t=10.
        assert!(has(&run(&v, 10 + 121, gone), Rule::Crashed), "2m01s fires");
        assert!(!has(&run(&v, 10 + 119, gone), Rule::Crashed), "1m59s");
        assert!(!has(&run(&v, 10 + 120, gone), Rule::Crashed), "exactly 2m");
        assert!(!has(&run(&v, 10 + 121, alive), Rule::Crashed), "pid alive");
        let mut ended = j();
        ended.push((
            11,
            EventData::AttemptEnded {
                attempt: 0,
                reason: attractor_journal::AttemptEndReason::Failed,
                message: None,
            },
        ));
        assert!(!has(&run(&journal(ended), 10 + 300, gone), Rule::Crashed));
    }

    #[test]
    fn crashed_without_heartbeat_uses_attempt_start() {
        let v = journal(vec![(0, attempt_started())]);
        assert!(has(&run(&v, 121, gone), Rule::Crashed));
        assert!(!has(&run(&v, 119, gone), Rule::Crashed));
    }

    #[test]
    fn budget_boundary() {
        let cost = |c| {
            journal(vec![
                (0, run_started(Some(100.0), None, false)),
                (1, llm(None, None, Some(c))),
            ])
        };
        assert!(has(&run(&cost(80.0), 2, gone), Rule::Budget));
        assert!(!has(&run(&cost(79.9), 2, gone), Rule::Budget));
        let unlimited = journal(vec![
            (0, run_started(None, None, false)),
            (1, llm(None, None, Some(1e6))),
        ]);
        assert!(!has(&run(&unlimited, 2, gone), Rule::Budget));

        let steps = |n: usize| {
            let mut v = vec![(0, run_started(None, Some(10), false))];
            v.extend((0..n).map(|_| (1, stage("n"))));
            journal(v)
        };
        assert!(has(&run(&steps(8), 2, gone), Rule::Budget));
        assert!(!has(&run(&steps(7), 2, gone), Rule::Budget));
    }

    #[test]
    fn loop_boundary() {
        assert!(has(
            &run(&journal(visits("n", 6)), 2, gone),
            Rule::LoopSuspicion
        ));
        assert!(!has(
            &run(&journal(visits("n", 5)), 2, gone),
            Rule::LoopSuspicion
        ));
        let mut split = visits("n", 3);
        split.push((2, close("T1", true)));
        split.push((2, claim("T2")));
        split.extend((0..3).map(|_| (3, stage("n"))));
        assert!(!has(&run(&journal(split), 4, gone), Rule::LoopSuspicion));
        // Visits outside any Task do not count.
        let outside = journal((0..9).map(|_| (0, stage("n"))).collect());
        assert!(!has(&run(&outside, 1, gone), Rule::LoopSuspicion));
    }

    #[test]
    fn retrying_boundary() {
        let retry = |n| {
            journal(vec![(
                0,
                EventData::StageRetrying {
                    node_id: "n".into(),
                    attempt: n,
                },
            )])
        };
        assert!(has(&run(&retry(2), 1, gone), Rule::Retrying));
        assert!(!has(&run(&retry(1), 1, gone), Rule::Retrying));
    }

    #[test]
    fn model_mismatch_needs_actual() {
        let m = |req, act| journal(vec![(0, llm(req, act, None))]);
        assert!(has(
            &run(&m(Some("a"), Some("b")), 1, gone),
            Rule::ModelMismatch
        ));
        assert!(!has(
            &run(&m(Some("a"), None), 1, gone),
            Rule::ModelMismatch
        ));
        assert!(!has(
            &run(&m(Some("a"), Some("a")), 1, gone),
            Rule::ModelMismatch
        ));
        assert!(!has(
            &run(&m(None, Some("b")), 1, gone),
            Rule::ModelMismatch
        ));
    }

    #[test]
    fn stalled_needs_heartbeats_and_idle() {
        // Idle past the 600 s default, heartbeat fresh: Stalled, not Crashed.
        let v = journal(vec![
            (0, attempt_started()),
            (1, stage("n")),
            (700, heartbeat()),
        ]);
        let f = run(&v, 701, gone);
        assert_eq!(rules(&f), vec![(Rule::Stalled, Severity::Warn)]);
        assert_eq!(f[0].node_id.as_deref(), Some("n"));
        // Stale heartbeat and no PID: Crashed only.
        let f = run(&v, 700 + 121, gone);
        assert_eq!(rules(&f), vec![(Rule::Crashed, Severity::Critical)]);
        // A known shorter timeout is honoured.
        let short = |_: &str| Some(Duration::seconds(30));
        let env = Env {
            pid_alive: &alive,
            node_timeout: &short,
            commit_nodes: None,
        };
        let v = journal(vec![
            (0, attempt_started()),
            (1, stage("n")),
            (50, heartbeat()),
        ]);
        assert!(has(
            &derive(&v, base() + Duration::seconds(51), &env),
            Rule::Stalled
        ));
        // No current node: nothing to stall.
        let v = journal(vec![(0, attempt_started()), (700, heartbeat())]);
        assert!(!has(&run(&v, 701, gone), Rule::Stalled));
    }

    #[test]
    fn unsafe_commit_clauses() {
        let v = journal(vec![(0, commits("worker"))]);
        assert!(
            !has(&run(&v, 1, gone), Rule::UnsafeCommit),
            "unknown commit node"
        );
        let allowed: BTreeSet<String> = ["commit".to_string()].into();
        let env = |set| Env {
            pid_alive: &gone,
            node_timeout: &no_timeout,
            commit_nodes: Some(set),
        };
        assert!(has(&derive(&v, base(), &env(&allowed)), Rule::UnsafeCommit));
        let ok = journal(vec![(0, commits("commit"))]);
        assert!(!has(
            &derive(&ok, base(), &env(&allowed)),
            Rule::UnsafeCommit
        ));
    }

    #[test]
    fn empty_view_has_no_findings() {
        assert!(run(&RunView::default(), 0, gone).is_empty());
    }

    #[test]
    fn epic_snapshot_tasks_do_not_trip_rules() {
        let v = journal(vec![(
            0,
            EventData::EpicSnapshot {
                epic_id: "E".into(),
                title: "E".into(),
                tasks: vec![TaskSummary {
                    id: "T1".into(),
                    title: "t".into(),
                    status: "open".into(),
                }],
            },
        )]);
        assert!(run(&v, 1, gone).is_empty());
    }

    #[test]
    fn derive_is_deterministic_and_ordered() {
        let v = journal(vec![
            (0, run_started(Some(10.0), None, true)),
            (0, attempt_started()),
            (0, claim("T1")),
            (1, llm(Some("a"), Some("b"), Some(9.0))),
            (
                2,
                EventData::StageFailed {
                    node_id: "n".into(),
                    error: "x".into(),
                },
            ),
            (3, close("T1", false)),
        ]);
        let a = run(&v, 1000, gone);
        let b = run(&v, 1000, gone);
        assert_eq!(a, b);
        let order: Vec<Rule> = a.iter().map(|f| f.rule).collect();
        assert_eq!(
            order,
            vec![
                Rule::Crashed,
                Rule::StageFailed,
                Rule::Budget,
                Rule::ModelMismatch,
                Rule::UnsafeCommit,
                Rule::SharedWorkdir
            ]
        );
        assert!(order.windows(2).all(|w| w[0] <= w[1]));
    }
}
