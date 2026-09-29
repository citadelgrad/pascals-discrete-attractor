//! The Runs page: active Runs first, then history, with filters.

use std::path::Path;

use attractor_journal::RunStatus;
use axum::extract::{Query, State};
use axum::response::Html;
use chrono::{DateTime, Duration, Utc};
use maud::{html, Markup, DOCTYPE};
use serde::Deserialize;

use crate::findings::{derive, Env, Rule, Severity};
use crate::projection::ViewStatus;
use crate::state::{AppState, RunSnapshot};

/// Query parameters of the Runs page. An empty value means "no filter".
#[derive(Debug, Default, Clone, Deserialize)]
pub struct RunsQuery {
    pub repo: Option<String>,
    pub status: Option<String>,
}

impl RunsQuery {
    fn repo(&self) -> Option<&str> {
        self.repo.as_deref().filter(|s| !s.is_empty())
    }

    /// An unknown status value is ignored rather than rejected.
    fn status(&self) -> Option<&str> {
        self.status
            .as_deref()
            .filter(|s| STATUSES.iter().any(|k| k.as_str() == *s))
    }
}

const STATUSES: [RunStatus; 6] = [
    RunStatus::Running,
    RunStatus::Completed,
    RunStatus::Failed,
    RunStatus::Stopped,
    RunStatus::Crashed,
    RunStatus::Missing,
];

/// One row of the Runs table.
#[derive(Debug, Clone, PartialEq)]
pub struct RunRow {
    pub run_id: String,
    /// The Run's workdir from the Run Index.
    pub repository: String,
    pub pipeline: String,
    pub status: RunStatus,
    /// Current Task: id and title.
    pub task: Option<(String, String)>,
    pub node: Option<String>,
    pub elapsed: Option<Duration>,
    pub cost_usd: f64,
    /// Finding counts: `[info, warn, critical]`.
    pub findings: [usize; 3],
    started_at: DateTime<Utc>,
}

impl RunRow {
    /// Only a running Run is active; a crashed one is not executing.
    pub fn is_active(&self) -> bool {
        self.status == RunStatus::Running
    }
}

/// Build the ordered rows: active Runs first, then history, each group newest first.
pub fn rows(snaps: &[RunSnapshot], now: DateTime<Utc>, env: &Env) -> Vec<RunRow> {
    let mut out: Vec<RunRow> = snaps.iter().map(|s| row(s, now, env)).collect();
    out.sort_by(|a, b| {
        (!a.is_active(), std::cmp::Reverse(a.started_at), &a.run_id).cmp(&(
            !b.is_active(),
            std::cmp::Reverse(b.started_at),
            &b.run_id,
        ))
    });
    out
}

/// A Run's status as shown to the user: `Crashed` and `Missing` are derived here.
pub(crate) fn run_status(s: &RunSnapshot, crashed: bool) -> RunStatus {
    if s.missing {
        RunStatus::Missing
    } else if crashed {
        RunStatus::Crashed
    } else {
        match s.view.status {
            ViewStatus::Unknown | ViewStatus::Running => RunStatus::Running,
            ViewStatus::Completed => RunStatus::Completed,
            ViewStatus::Failed => RunStatus::Failed,
            ViewStatus::Stopped => RunStatus::Stopped,
        }
    }
}

fn row(s: &RunSnapshot, now: DateTime<Utc>, env: &Env) -> RunRow {
    let v = &s.view;
    let mut findings = [0usize; 3];
    let mut crashed = false;
    if !s.missing {
        for f in derive(v, now, env) {
            crashed |= f.rule == Rule::Crashed;
            findings[match f.severity {
                Severity::Info => 0,
                Severity::Warn => 1,
                Severity::Critical => 2,
            }] += 1;
        }
    }
    let status = run_status(s, crashed);
    let started = s.entry.started_at;
    let elapsed = match status {
        RunStatus::Missing => None,
        RunStatus::Running => Some(now - started),
        _ => v.last_ts.map(|t| t - started),
    };
    RunRow {
        run_id: s.entry.run_id.clone(),
        repository: s.entry.workdir.display().to_string(),
        pipeline: v.pipeline_name.clone().unwrap_or_default(),
        status,
        task: v
            .current_task
            .as_deref()
            .and_then(|id| v.task(id))
            .map(|t| (t.id.clone(), t.title.clone())),
        node: v.current_node.clone(),
        elapsed,
        cost_usd: v.cost_usd,
        findings,
        started_at: started,
    }
}

/// Does the row pass the filters? The repository filter matches a workdir
/// that is, or lies inside, the chosen path.
pub fn matches(row: &RunRow, q: &RunsQuery) -> bool {
    q.repo()
        .is_none_or(|r| Path::new(&row.repository).starts_with(r))
        && q.status().is_none_or(|s| row.status.as_str() == s)
}

fn fmt_elapsed(d: Duration) -> String {
    let s = d.num_seconds().max(0);
    match (s / 3600, s / 60 % 60, s % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m {s:02}s"),
        (h, m, _) => format!("{h}h {m:02}m"),
    }
}

/// The table, or the empty state. `total` is the number of Runs before filtering.
pub fn table(rows: &[RunRow], total: usize) -> Markup {
    if total == 0 {
        return html! {
            div.empty {
                p { "No Runs yet." }
                p { "Start one with " code { "pas run <pipeline.dot>" } }
            }
        };
    }
    if rows.is_empty() {
        return html! {
            div.empty {
                p { "No Runs match these filters." }
                p { a href="/" { "Clear filters" } }
            }
        };
    }
    let first_history = rows.iter().position(|r| !r.is_active());
    html! {
        table {
            thead { tr {
                th { "Repository" } th { "Pipeline" } th { "Status" } th { "Task" }
                th { "Node" } th { "Elapsed" } th { "Cost" } th { "Findings" }
            } }
            tbody {
                @for (i, r) in rows.iter().enumerate() {
                    tr class=(if Some(i) == first_history && i > 0 { "history-start" } else { "" })
                        data-run-id=(r.run_id) {
                        td.repository { (r.repository) }
                        td.pipeline { a href=(format!("/runs/{}", r.run_id)) { (r.pipeline) } }
                        td.status data-status=(r.status.as_str()) { (r.status.as_str()) }
                        td.task {
                            @if let Some((id, title)) = &r.task { (id) " " (title) }
                        }
                        td.node { (r.node.as_deref().unwrap_or("")) }
                        td.elapsed { (r.elapsed.map(fmt_elapsed).unwrap_or_default()) }
                        td.cost { (format!("${:.2}", r.cost_usd)) }
                        td.findings {
                            @for (n, name) in r.findings.iter().zip(["info", "warn", "critical"]) {
                                span class=(format!("badge {name}{}", if *n == 0 { " zero" } else { "" }))
                                    title=(name) { (name) " " (n) }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The whole page: filter form plus the live table.
pub fn page(rows: &[RunRow], total: usize, repos: &[String], q: &RunsQuery) -> Markup {
    html! {
        (DOCTYPE)
        html {
            head {
                meta charset="utf-8";
                title { "PAS Monitor" }
                link rel="stylesheet" href="/assets/monitor.css";
                script src="/assets/htmx.min.js" {}
            }
            body {
                h1 { "PAS Monitor" }
                form #filters method="get" action="/" {
                    label { "Repository "
                        select name="repo" {
                            option value="" { "All" }
                            @for r in repos {
                                option value=(r) selected[q.repo() == Some(r.as_str())] { (r) }
                            }
                        }
                    }
                    label { "Status "
                        select name="status" {
                            option value="" { "All" }
                            @for s in STATUSES {
                                option value=(s.as_str()) selected[q.status() == Some(s.as_str())] { (s.as_str()) }
                            }
                        }
                    }
                    button type="submit" { "Filter" }
                }
                div #runs hx-get="/runs" hx-trigger="every 2s" hx-include="#filters" hx-swap="innerHTML" {
                    (table(rows, total))
                }
            }
        }
    }
}

fn filtered(state: &AppState, q: &RunsQuery) -> (Vec<RunRow>, usize, Vec<String>) {
    let no_timeout = |_: &str| None;
    let env = Env {
        pid_alive: &super::pid_alive,
        node_timeout: &no_timeout,
        commit_nodes: None,
    };
    let all = rows(&state.list(), Utc::now(), &env);
    let total = all.len();
    let mut repos: Vec<String> = all.iter().map(|r| r.repository.clone()).collect();
    repos.sort();
    repos.dedup();
    let shown = all.into_iter().filter(|r| matches(r, q)).collect();
    (shown, total, repos)
}

pub async fn page_handler(
    State(state): State<AppState>,
    Query(q): Query<RunsQuery>,
) -> Html<String> {
    let (shown, total, repos) = filtered(&state, &q);
    Html(page(&shown, total, &repos, &q).into_string())
}

pub async fn table_handler(
    State(state): State<AppState>,
    Query(q): Query<RunsQuery>,
) -> Html<String> {
    let (shown, total, _) = filtered(&state, &q);
    Html(table(&shown, total).into_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projection::fold;
    use attractor_journal::{AttemptEndReason, EventData, IndexEntry, JournalEvent};
    use chrono::TimeZone;

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap()
    }

    const ALIVE: fn(u32) -> bool = |_| true;
    const DEAD: fn(u32) -> bool = |_| false;

    fn env(alive: &dyn Fn(u32) -> bool) -> Env<'_> {
        Env {
            pid_alive: alive,
            node_timeout: &|_| None,
            commit_nodes: None,
        }
    }

    /// A Run started `start` seconds after t0, with Events at the given offsets.
    fn snap(id: &str, workdir: &str, start: i64, events: Vec<(i64, EventData)>) -> RunSnapshot {
        let evs: Vec<JournalEvent> = events
            .into_iter()
            .enumerate()
            .map(|(i, (secs, d))| {
                JournalEvent::new(i as u64 + 1, t0() + Duration::seconds(secs), id, 0, d)
            })
            .collect();
        RunSnapshot {
            entry: IndexEntry::new(
                id,
                t0() + Duration::seconds(start),
                workdir,
                "p.dot",
                "/nonexistent",
            ),
            view: fold(&evs),
            missing: false,
        }
    }

    fn started(workdir: &str) -> EventData {
        EventData::RunStarted {
            pipeline_name: "pipe".into(),
            pipeline_path: "p.dot".into(),
            workdir: workdir.into(),
            epic_id: None,
            max_budget_usd: None,
            max_steps: None,
            shared_workdir: false,
        }
    }

    fn attempt() -> EventData {
        EventData::AttemptStarted {
            attempt: 0,
            pid: 42,
            pas_version: "1".into(),
            argv: vec![],
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

    fn running(id: &str, workdir: &str, start: i64) -> RunSnapshot {
        snap(
            id,
            workdir,
            start,
            vec![(start, started(workdir)), (start, attempt())],
        )
    }

    fn finished(id: &str, workdir: &str, start: i64, r: AttemptEndReason) -> RunSnapshot {
        snap(
            id,
            workdir,
            start,
            vec![
                (start, started(workdir)),
                (start, attempt()),
                (start + 60, ended(r)),
            ],
        )
    }

    fn all(rows: &[RunRow]) -> Vec<&str> {
        rows.iter().map(|r| r.run_id.as_str()).collect()
    }

    fn status_of(rows: &[RunRow], id: &str) -> RunStatus {
        rows.iter().find(|r| r.run_id == id).unwrap().status
    }

    fn q(repo: &str, status: &str) -> RunsQuery {
        RunsQuery {
            repo: Some(repo.into()),
            status: Some(status.into()),
        }
    }

    // AC1
    #[test]
    fn active_runs_listed_before_history() {
        let snaps = vec![
            finished("f1", "/r/a", 10, AttemptEndReason::Completed),
            running("a1", "/r/a", 20),
            finished("f2", "/r/a", 30, AttemptEndReason::Failed),
            running("a2", "/r/a", 40),
            finished("f3", "/r/a", 50, AttemptEndReason::Stopped),
        ];
        let now = t0() + Duration::seconds(100);
        let r = rows(&snaps, now, &env(&ALIVE));
        assert_eq!(all(&r), ["a2", "a1", "f3", "f2", "f1"]);
        assert!(r[..2].iter().all(RunRow::is_active));
        assert!(r[2..].iter().all(|r| !r.is_active()));
    }

    // AC2
    #[test]
    fn row_has_all_columns() {
        let events = vec![
            (0, started("/r/a")),
            (0, attempt()),
            (
                1,
                EventData::TaskClaimed {
                    task_id: "T-1".into(),
                    title: "Do the thing".into(),
                    epic_id: "E".into(),
                    node_id: "pick".into(),
                },
            ),
            (
                2,
                EventData::StageStarted {
                    node_id: "build".into(),
                    handler_type: "codergen".into(),
                },
            ),
            (
                3,
                EventData::StageFailed {
                    node_id: "build".into(),
                    error: "x".into(),
                },
            ),
            (
                4,
                EventData::StageRetrying {
                    node_id: "build".into(),
                    attempt: 2,
                },
            ),
            (
                5,
                EventData::StageStarted {
                    node_id: "build".into(),
                    handler_type: "codergen".into(),
                },
            ),
        ];
        let mut s = snap("r1", "/r/a", 0, events);
        s.view.cost_usd = 1.5;
        let now = t0() + Duration::seconds(3720);
        let r = rows(&[s], now, &env(&ALIVE));
        let row = &r[0];
        assert_eq!(row.repository, "/r/a");
        assert_eq!(row.pipeline, "pipe");
        assert_eq!(row.status, RunStatus::Running);
        assert_eq!(row.task, Some(("T-1".into(), "Do the thing".into())));
        assert_eq!(row.node.as_deref(), Some("build"));
        assert_eq!(row.elapsed, Some(Duration::seconds(3720)));
        assert_eq!(row.findings[2], 1, "StageFailed is critical");
        assert!(row.findings[1] >= 1, "Retrying is a warning");

        let html = table(&r, 1).into_string();
        for want in [
            "/r/a",
            "pipe",
            "running",
            "T-1",
            "Do the thing",
            "build",
            "1h 02m",
            "$1.50",
            "info 0",
            "warn 1",
            "critical 1",
        ] {
            assert!(html.contains(want), "missing {want:?} in {html}");
        }
    }

    // AC3
    #[test]
    fn repo_filter_matches_workdir_inside_repo() {
        let snaps = vec![
            running("a", "/r/a", 1),
            running("sub", "/r/a/sub", 2),
            running("ab", "/r/ab", 3),
            running("b", "/r/b", 4),
        ];
        let r = rows(&snaps, t0() + Duration::seconds(10), &env(&ALIVE));
        let shown: Vec<_> = r.iter().filter(|r| matches(r, &q("/r/a", ""))).collect();
        let mut ids: Vec<_> = shown.iter().map(|r| r.run_id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, ["a", "sub"]);
    }

    // AC4
    #[test]
    fn status_filter_crashed_only() {
        let now = t0() + Duration::seconds(1000);
        let snaps = vec![
            running("old", "/r/a", 0), // last sign of life 1000 s ago
            snap(
                "fresh",
                "/r/a",
                990,
                vec![(990, started("/r/a")), (990, attempt())],
            ),
            finished("done", "/r/a", 100, AttemptEndReason::Completed),
            finished("bad", "/r/a", 200, AttemptEndReason::Failed),
        ];
        let r = rows(&snaps, now, &env(&DEAD));
        assert_eq!(status_of(&r, "old"), RunStatus::Crashed);
        assert_eq!(status_of(&r, "fresh"), RunStatus::Running);
        let shown: Vec<_> = r.iter().filter(|r| matches(r, &q("", "crashed"))).collect();
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].run_id, "old");
        assert!(!shown[0].is_active());
        // With a live process the same Run is still running.
        let r = rows(&snaps, now, &env(&ALIVE));
        assert_eq!(status_of(&r, "old"), RunStatus::Running);
    }

    // AC6
    #[test]
    fn empty_state_has_start_command() {
        let html = table(&[], 0).into_string();
        assert!(html.contains("No Runs yet"));
        assert!(
            html.contains("<code>pas run &lt;pipeline.dot&gt;</code>"),
            "{html}"
        );
        let page = page(&[], 0, &[], &RunsQuery::default()).into_string();
        assert!(page.contains("pas run"));

        let html = table(&[], 3).into_string();
        assert!(html.contains("No Runs match"));
        assert!(!html.contains("pas run"));
    }

    #[test]
    fn missing_run_has_no_findings() {
        let mut s = running("m", "/r/a", 0);
        s.missing = true;
        let r = rows(&[s], t0() + Duration::seconds(9999), &env(&DEAD));
        assert_eq!(r[0].status, RunStatus::Missing);
        assert_eq!(r[0].findings, [0, 0, 0]);
        assert_eq!(r[0].elapsed, None);
        assert!(matches(&r[0], &q("", "missing")));
    }

    #[test]
    fn workdir_is_escaped() {
        let s = running("x", "/r/<script>alert(1)</script>", 0);
        let r = rows(&[s], t0() + Duration::seconds(5), &env(&ALIVE));
        let html = page(&r, 1, &[r[0].repository.clone()], &RunsQuery::default()).into_string();
        assert!(!html.contains("<script>alert"), "{html}");
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn unknown_status_is_ignored_and_empty_journal_is_running() {
        let s = RunSnapshot {
            entry: IndexEntry::new("e", t0(), "/r/a", "p.dot", "/nonexistent"),
            view: Default::default(),
            missing: false,
        };
        let r = rows(&[s], t0() + Duration::seconds(5), &env(&ALIVE));
        assert_eq!(r[0].status, RunStatus::Running);
        assert!(matches(&r[0], &q("", "bogus")));
        assert!(matches(&r[0], &q("", "")));
    }

    #[test]
    fn finished_run_elapsed_freezes_at_last_event() {
        let s = finished("f", "/r/a", 0, AttemptEndReason::Completed);
        let r = rows(&[s], t0() + Duration::seconds(99999), &env(&ALIVE));
        assert_eq!(r[0].elapsed, Some(Duration::seconds(60)));
        assert_eq!(fmt_elapsed(Duration::seconds(75)), "1m 15s");
        assert_eq!(fmt_elapsed(Duration::seconds(-5)), "0s");
    }
}
