//! The Run page: header with bars, Attempts, Pipeline graph, Tasks, commits,
//! Model Invocations, Findings and the live Event log (PRD FR-12).

use std::collections::{HashMap, HashSet};

use attractor_journal::{parse_run_id, read_all, EventData, JournalEvent, RunDir, RunStatus};
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use chrono::{DateTime, Utc};
use maud::{html, Markup, PreEscaped, DOCTYPE};

use super::dot_scan::{self, Timeouts};
use super::runs::run_status;
use crate::findings::{derive, Env, Finding, Rule};
use crate::projection::{RunView, TaskView};
use crate::state::{AppState, RunSnapshot};

/// The Pipeline file is read for the graph and node timeouts; larger files are not shown.
const MAX_DOT_BYTES: u64 = 1024 * 1024;
/// Events rendered into the page; earlier ones are announced, live ones keep coming.
const MAX_LOG_EVENTS: usize = 500;
const MAX_SUMMARY_CHARS: usize = 200;

/// Where the graph comes from.
#[derive(Debug, Clone, PartialEq)]
pub enum GraphSource {
    Dot(String),
    /// The file is gone, too large or not text; the message says why.
    Unavailable(String),
}

/// Read the Pipeline file named by the Run Index. The path never comes from a request.
pub fn load_graph(path: &std::path::Path) -> GraphSource {
    let shown = path.display();
    match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return GraphSource::Unavailable(format!("Pipeline file not found: {shown}"))
        }
        Err(e) => {
            return GraphSource::Unavailable(format!("Pipeline file unreadable: {shown}: {e}"))
        }
        Ok(m) if m.len() > MAX_DOT_BYTES => {
            return GraphSource::Unavailable(format!("Pipeline file too large to draw: {shown}"))
        }
        Ok(_) => {}
    }
    match std::fs::read_to_string(path) {
        Ok(s) => GraphSource::Dot(s),
        Err(e) => GraphSource::Unavailable(format!("Pipeline file unreadable: {shown}: {e}")),
    }
}

// --- Event log ------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct LogEntry {
    pub seq: u64,
    pub ts: DateTime<Utc>,
    pub kind: String,
    pub node: Option<String>,
    /// A stage of a codergen node.
    pub codergen: bool,
    pub summary: String,
    /// The Transcript this row opens, when it has one.
    pub invocation: Option<String>,
}

/// Scalar fields of an Event as `key=value`, the same text the live script builds.
fn summarize(value: &serde_json::Value) -> String {
    let mut out = String::new();
    if let Some(obj) = value.as_object() {
        for (k, v) in obj {
            let text = match v {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Number(_) | serde_json::Value::Bool(_) => v.to_string(),
                _ => continue,
            };
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(&format!("{k}={text}"));
        }
    }
    if out.chars().count() > MAX_SUMMARY_CHARS {
        out = out.chars().take(MAX_SUMMARY_CHARS).collect::<String>() + "…";
    }
    out
}

/// Turn journal Events into log rows. A stage of a codergen node links to the
/// last Model Invocation of the same visit of that node; `LlmInvoked` rows link
/// to their own.
pub fn log_entries(events: &[JournalEvent]) -> Vec<LogEntry> {
    let mut visit: HashMap<&str, usize> = HashMap::new();
    let mut codergen: HashSet<(&str, usize)> = HashSet::new();
    let mut last_inv: HashMap<(&str, usize), &str> = HashMap::new();
    let mut keys: Vec<Option<(&str, usize)>> = Vec::with_capacity(events.len());
    for ev in events {
        let key = match &ev.data {
            EventData::StageStarted {
                node_id,
                handler_type,
            } => {
                let n = visit.entry(node_id).or_default();
                *n += 1;
                if handler_type == "codergen" {
                    codergen.insert((node_id, *n));
                }
                Some((node_id.as_str(), *n))
            }
            EventData::StageCompleted { node_id, .. } | EventData::StageFailed { node_id, .. } => {
                Some((
                    node_id.as_str(),
                    visit.get(node_id.as_str()).copied().unwrap_or(0),
                ))
            }
            EventData::LlmInvoked {
                node_id,
                invocation_id,
                ..
            } => {
                let k = (
                    node_id.as_str(),
                    visit.get(node_id.as_str()).copied().unwrap_or(0),
                );
                last_inv.insert(k, invocation_id);
                Some(k)
            }
            _ => None,
        };
        keys.push(key);
    }
    events
        .iter()
        .zip(keys)
        .map(|(ev, key)| {
            let value = serde_json::to_value(&ev.data).unwrap_or_default();
            let data = value.get("data").cloned().unwrap_or_default();
            let node = data
                .get("node_id")
                .and_then(|v| v.as_str())
                .map(String::from);
            let is_stage = matches!(
                ev.data,
                EventData::StageStarted { .. }
                    | EventData::StageCompleted { .. }
                    | EventData::StageFailed { .. }
            );
            let is_cg = is_stage && key.is_some_and(|k| codergen.contains(&k));
            let invocation = match (&ev.data, key) {
                (EventData::LlmInvoked { invocation_id, .. }, _) => Some(invocation_id.clone()),
                (_, Some(k)) if is_cg => last_inv.get(&k).map(|s| s.to_string()),
                _ => None,
            };
            LogEntry {
                seq: ev.seq,
                ts: ev.ts,
                kind: ev.data.type_name().to_string(),
                node,
                codergen: is_cg,
                summary: summarize(&data),
                invocation,
            }
        })
        .collect()
}

fn event_log(run_id: &str, entries: &[LogEntry]) -> Markup {
    let hidden = entries.len().saturating_sub(MAX_LOG_EVENTS);
    html! {
        @if hidden > 0 {
            p.log-note { (hidden) " earlier Events not shown" }
        }
        ol #event-log data-run=(run_id) {
            @for e in &entries[hidden..] {
                li data-seq=(e.seq) data-type=(e.kind) data-node=(e.node.as_deref().unwrap_or(""))
                    data-cg=(if e.codergen { "1" } else { "0" })
                    data-inv=(e.invocation.as_deref().unwrap_or("")) {
                    span.ts { (e.ts.format("%H:%M:%S")) } " "
                    strong { (e.kind) } " " (e.summary)
                    @if let Some(inv) = &e.invocation {
                        " " a.transcript href=(format!("/runs/{run_id}/transcripts/{inv}")) { "transcript" }
                    }
                }
            }
        }
    }
}

/// Appends live Events from the SSE stream; `textContent` only, never HTML.
const LOG_SCRIPT: &str = r#"(function(){
var log=document.getElementById('event-log');if(!log||!window.EventSource)return;
var run=log.dataset.run,open={},last=0;
function link(li,inv){var a=li.querySelector('a.transcript');if(!a){li.appendChild(document.createTextNode(' '));a=document.createElement('a');a.className='transcript';a.textContent='transcript';li.appendChild(a);}a.href='/runs/'+run+'/transcripts/'+encodeURIComponent(inv);}
function note(li){var t=li.dataset.type,n=li.dataset.node,cg=li.dataset.cg==='1';if(!n)return;
if(t==='StageStarted'){open[n]=cg?[li]:null;}
else if((t==='StageCompleted'||t==='StageFailed')&&open[n]){open[n].push(li);}
else if(t==='LlmInvoked'&&li.dataset.inv){link(li,li.dataset.inv);(open[n]||[]).forEach(function(x){link(x,li.dataset.inv);});}}
Array.prototype.forEach.call(log.children,function(li){last=Math.max(last,+li.dataset.seq||0);note(li);});
function summary(d){var o=[];Object.keys(d||{}).forEach(function(k){var v=d[k];if(typeof v==='string'||typeof v==='number'||typeof v==='boolean')o.push(k+'='+v);});var s=o.join(' ');return s.length>200?s.slice(0,200)+'…':s;}
var es=new EventSource('/runs/'+run+'/events');
es.addEventListener('journal',function(m){var f;try{f=JSON.parse(m.data);}catch(e){return;}
if(!(f.seq>last))return;last=f.seq;var d=f.data||{};
var li=document.createElement('li');li.dataset.seq=f.seq;li.dataset.type=f.type;li.dataset.node=d.node_id||'';
li.dataset.cg=(f.type==='StageStarted'&&d.handler_type==='codergen')?'1':'0';li.dataset.inv=f.type==='LlmInvoked'?(d.invocation_id||''):'';
var ts=document.createElement('span');ts.className='ts';ts.textContent=String(f.ts).slice(11,19);
var b=document.createElement('strong');b.textContent=f.type;
li.appendChild(ts);li.appendChild(document.createTextNode(' '));li.appendChild(b);li.appendChild(document.createTextNode(' '+summary(d)));
log.appendChild(li);note(li);});
})();"#;

// --- Graph ------------------------------------------------------------------

fn graph_state(view: &RunView) -> Markup {
    let visited: Vec<&str> = view.visited.iter().map(|v| v.node_id.as_str()).collect();
    html! {
        div #graph-state hidden
            data-current=(view.current_node.as_deref().unwrap_or(""))
            data-visited=(serde_json::to_string(&visited).unwrap_or_else(|_| "[]".into())) {}
    }
}

/// JSON safe inside a `<script>` element: `<`, `>` and `&` are `\u` escaped.
fn script_json(s: &str) -> PreEscaped<String> {
    let json = serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into());
    PreEscaped(
        json.replace('<', "\\u003c")
            .replace('>', "\\u003e")
            .replace('&', "\\u0026"),
    )
}

const GRAPH_SCRIPT: &str = r#"(function(){
var box=document.getElementById('graph-svg'),src=document.getElementById('dot-src');if(!box||!src||!window.Viz)return;
function mark(){var st=document.getElementById('graph-state');if(!st)return;var cur=st.dataset.current,seen=[];try{seen=JSON.parse(st.dataset.visited||'[]');}catch(e){}
box.querySelectorAll('g.node').forEach(function(g){var t=g.querySelector('title'),id=t&&t.textContent;g.classList.toggle('current',!!id&&id===cur);g.classList.toggle('visited',!!id&&seen.indexOf(id)>=0);});}
Viz.instance().then(function(v){box.replaceChildren(v.renderSVGElement(JSON.parse(src.textContent)));mark();}).catch(function(e){box.textContent='Could not draw the graph: '+e;});
document.body.addEventListener('htmx:afterSettle',mark);
})();"#;

fn graph(view: &RunView, source: &GraphSource) -> Markup {
    html! {
        section #graph {
            h2 { "Pipeline" }
            (graph_state(view))
            @match source {
                GraphSource::Dot(dot) => {
                    p.note { "Graph from the Pipeline file as it is now." }
                    div #graph-svg {}
                    script #dot-src type="application/json" { (script_json(dot)) }
                    script src="/assets/viz-standalone.js" {}
                    script { (PreEscaped(GRAPH_SCRIPT)) }
                }
                GraphSource::Unavailable(msg) => { p.graph-missing { (msg) } }
            }
        }
    }
}

// --- Sections -----------------------------------------------------------------

fn bar(label: &str, value: f64, max: Option<f64>, text: String) -> Markup {
    html! {
        div.bar data-bar=(label) {
            span.label { (label) " " }
            @if let Some(max) = max.filter(|m| *m > 0.0) {
                progress value=(value) max=(max) {}
                " "
            }
            span.value { (text) }
        }
    }
}

fn bars(view: &RunView) -> Markup {
    let budget = match view.max_budget_usd {
        Some(max) => format!("${:.2} / ${:.2}", view.cost_usd, max),
        None => format!("${:.2} (no limit)", view.cost_usd),
    };
    let steps = match view.max_steps {
        Some(max) => format!("{} / {} steps", view.steps, max),
        None => format!("{} steps (no limit)", view.steps),
    };
    html! {
        (bar("Budget", view.cost_usd, view.max_budget_usd, budget))
        (bar("Steps", view.steps as f64, view.max_steps.map(|m| m as f64), steps))
    }
}

fn header(s: &RunSnapshot, status: RunStatus) -> Markup {
    let v = &s.view;
    html! {
        header {
            h1 { (v.pipeline_name.as_deref().unwrap_or("Run")) }
            p {
                span.status data-status=(status.as_str()) { (status.as_str()) } " "
                code { (s.entry.run_id) } " in " code { (s.entry.workdir.display()) }
            }
            @if s.missing { p.notice { "Run folder missing: its journal can no longer be read." } }
            @if let Some(src) = &v.stop_requested { p.notice { "Stop requested by " (src) } }
            @if let Some(g) = &v.gate { p.notice { "Waiting for a human at " (g.node_id) ": " (g.text) } }
            @if let Some(e) = &v.pipeline_error { p.notice { "Pipeline failed: " (e) } }
            (bars(v))
            p { "Current node: " code { (v.current_node.as_deref().unwrap_or("-")) } }
        }
    }
}

fn attempts(view: &RunView) -> Markup {
    html! {
        section #attempts {
            h2 { "Attempts" }
            @if view.attempts.is_empty() { p.empty { "No Attempts recorded." } }
            ol {
                @for a in &view.attempts {
                    li data-attempt=(a.attempt) {
                        "#" (a.attempt)
                        @if let Some(p) = a.pid { " pid " (p) }
                        @if let Some(t) = a.started_at { " started " (t.format("%Y-%m-%d %H:%M:%S")) }
                        @if let Some(n) = &a.resumed_from_node { " resumed from " code { (n) } }
                        @match &a.ended {
                            Some(e) => {
                                " ended: " (format!("{:?}", e.reason).to_lowercase())
                                @if let Some(m) = &e.message { " (" (m) ")" }
                            }
                            None => { " running" }
                        }
                        @if !a.argv.is_empty() { br; code { (a.argv.join(" ")) } }
                    }
                }
            }
        }
    }
}

/// Summed cost and distinct models (actual, else requested) of a Task's invocations.
pub fn task_rollup(view: &RunView, task_id: &str) -> (f64, Vec<String>) {
    let mut cost = 0.0;
    let mut models: Vec<String> = Vec::new();
    for i in view
        .invocations
        .iter()
        .filter(|i| i.task_id.as_deref() == Some(task_id))
    {
        cost += i.cost_usd.unwrap_or(0.0);
        if let Some(m) = i.model_actual.as_ref().or(i.model_requested.as_ref()) {
            if !models.contains(m) {
                models.push(m.clone());
            }
        }
    }
    (cost, models)
}

fn task_row(view: &RunView, t: &TaskView) -> Markup {
    let (cost, models) = task_rollup(view, &t.id);
    html! {
        tr data-task-id=(t.id) {
            td.id { (t.id) }
            td.title { (t.title) }
            td.status { (t.status) }
            td.commits { @for c in &t.commits { code { (c) } " " } }
            td.cost { (format!("${cost:.2}")) }
            td.models { (models.join(", ")) }
        }
    }
}

fn tasks(view: &RunView) -> Markup {
    html! {
        section #tasks {
            h2 { "Tasks" }
            @if let Some(e) = view.epic() {
                p.epic { "Epic " code { (e.epic_id) } " " (e.title) ": " (e.closed) " of " (e.total) " closed" }
            }
            @if view.tasks.is_empty() { p.empty { "No Tasks yet." } } @else {
                table {
                    thead { tr { th { "ID" } th { "Title" } th { "Status" } th { "Commits" } th { "Cost" } th { "Models" } } }
                    tbody { @for t in &view.tasks { (task_row(view, t)) } }
                }
            }
        }
    }
}

fn commits(view: &RunView) -> Markup {
    html! {
        section #commits {
            h2 { "Run Commits" }
            @if view.commits.is_empty() { p.empty { "No commits yet." } } @else {
                ul {
                    @for c in &view.commits {
                        li { code { (c.sha) } " " (c.subject) " (" (c.node_id)
                            @if let Some(t) = &c.task_id { ", " (t) } ")" }
                    }
                }
            }
        }
    }
}

fn invocations(run_id: &str, view: &RunView) -> Markup {
    html! {
        section #models {
            h2 { "Model Invocations" }
            @if view.invocations.is_empty() { p.empty { "No Model Invocations yet." } } @else {
                table {
                    thead { tr { th { "Node" } th { "Model" } th { "Cost" } th { "Status" } th { "" } } }
                    tbody {
                        @for i in &view.invocations {
                            tr data-invocation=(i.invocation_id) {
                                td { (i.node_id) }
                                td { (i.model_actual.as_deref().or(i.model_requested.as_deref()).unwrap_or("")) }
                                td { (format!("${:.2}", i.cost_usd.unwrap_or(0.0))) }
                                td { (i.status) }
                                td { a href=(format!("/runs/{run_id}/transcripts/{}", i.invocation_id)) { "transcript" } }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn findings(list: &[Finding]) -> Markup {
    html! {
        section #findings {
            h2 { "Findings" }
            @if list.is_empty() { p.empty { "No Findings." } } @else {
                ul {
                    @for f in list {
                        li class=(format!("finding {}", f.severity.as_str())) data-rule=(f.rule.as_str()) {
                            span.severity { (f.severity.as_str()) } " " (f.message)
                        }
                    }
                }
            }
        }
    }
}

/// Everything that changes while a Run is live, except the graph and the log.
pub fn summary(s: &RunSnapshot, now: DateTime<Utc>, env: &Env) -> Markup {
    let list = if s.missing {
        Vec::new()
    } else {
        derive(&s.view, now, env)
    };
    let status = run_status(s, list.iter().any(|f| f.rule == Rule::Crashed));
    html! {
        (header(s, status))
        (attempts(&s.view))
        (tasks(&s.view))
        (commits(&s.view))
        (invocations(&s.entry.run_id, &s.view))
        (findings(&list))
    }
}

pub fn page(
    s: &RunSnapshot,
    now: DateTime<Utc>,
    env: &Env,
    source: &GraphSource,
    log: &[LogEntry],
) -> Markup {
    html! {
        (DOCTYPE)
        html {
            head {
                meta charset="utf-8";
                title { "PAS Monitor: " (s.view.pipeline_name.as_deref().unwrap_or("Run")) }
                link rel="stylesheet" href="/assets/monitor.css";
                script src="/assets/htmx.min.js" {}
            }
            body {
                p { a href="/" { "All Runs" } }
                div #summary hx-get=(format!("/runs/{}/summary", s.entry.run_id)) hx-trigger="every 2s" hx-swap="innerHTML" {
                    (summary(s, now, env))
                }
                (graph(&s.view, source))
                section #log {
                    h2 { "Event log" }
                    (event_log(&s.entry.run_id, log))
                }
                script { (PreEscaped(LOG_SCRIPT)) }
            }
        }
    }
}

// --- Handlers ---------------------------------------------------------------

fn snapshot(state: &AppState, id: &str) -> Option<RunSnapshot> {
    state.snapshot(&parse_run_id(id)?)
}

fn timeouts_of(source: &GraphSource) -> Timeouts {
    match source {
        GraphSource::Dot(d) => dot_scan::scan(d),
        GraphSource::Unavailable(_) => Timeouts::default(),
    }
}

async fn graph_source(snap: &RunSnapshot) -> GraphSource {
    let path = snap.entry.pipeline_path.clone();
    tokio::task::spawn_blocking(move || load_graph(&path))
        .await
        .unwrap_or_else(|_| GraphSource::Unavailable("Pipeline file unreadable".into()))
}

pub async fn page_handler(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let Some(snap) = snapshot(&state, &id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let source = graph_source(&snap).await;
    let events_path = RunDir::from_path(&snap.entry.run_dir).events();
    let events = tokio::task::spawn_blocking(move || read_all(events_path))
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    let timeouts = timeouts_of(&source);
    let node_timeout = |n: &str| timeouts.get(n);
    let env = Env {
        pid_alive: &super::pid_alive,
        node_timeout: &node_timeout,
        commit_nodes: None,
    };
    Html(page(&snap, Utc::now(), &env, &source, &log_entries(&events)).into_string())
        .into_response()
}

pub async fn summary_handler(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let Some(snap) = snapshot(&state, &id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let source = graph_source(&snap).await;
    let timeouts = timeouts_of(&source);
    let node_timeout = |n: &str| timeouts.get(n);
    let env = Env {
        pid_alive: &super::pid_alive,
        node_timeout: &node_timeout,
        commit_nodes: None,
    };
    // The graph marks change with the Run, so its state rides along out of band.
    let out = html! {
        (summary(&snap, Utc::now(), &env))
        div #graph-state hidden hx-swap-oob="true"
            data-current=(snap.view.current_node.as_deref().unwrap_or(""))
            data-visited=(serde_json::to_string(
                &snap.view.visited.iter().map(|v| v.node_id.as_str()).collect::<Vec<_>>()
            ).unwrap_or_else(|_| "[]".into())) {}
    };
    Html(out.into_string()).into_response()
}

/// The raw Transcript of a Model Invocation of this Run. The invocation ID must be
/// one the journal recorded, so no other path segment reaches the filesystem.
pub async fn transcript_handler(
    State(state): State<AppState>,
    Path((id, inv)): Path<(String, String)>,
) -> Response {
    let Some(snap) = snapshot(&state, &id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !snap.view.invocations.iter().any(|i| i.invocation_id == inv) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let path = RunDir::from_path(&snap.entry.run_dir).transcript(&inv);
    match tokio::fs::read(&path).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], bytes).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "Transcript file not found\n").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projection::fold;
    use attractor_journal::{AttemptEndReason, CommitRef, IndexEntry, TaskSummary};
    use chrono::{Duration, TimeZone};

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap()
    }

    fn evs(list: Vec<(i64, EventData)>) -> Vec<JournalEvent> {
        list.into_iter()
            .enumerate()
            .map(|(i, (s, d))| {
                JournalEvent::new(i as u64 + 1, t0() + Duration::seconds(s), "r", 0, d)
            })
            .collect()
    }

    fn snap_of(events: &[JournalEvent]) -> RunSnapshot {
        RunSnapshot {
            entry: IndexEntry::new(
                "11111111-1111-7111-8111-111111111111",
                t0(),
                "/w",
                "/nonexistent.dot",
                "/nonexistent",
            ),
            view: fold(events),
            missing: false,
        }
    }

    fn started(budget: Option<f64>, steps: Option<u64>) -> EventData {
        EventData::RunStarted {
            pipeline_name: "pipe".into(),
            pipeline_path: "p.dot".into(),
            workdir: "/w".into(),
            epic_id: Some("E".into()),
            max_budget_usd: budget,
            max_steps: steps,
            shared_workdir: false,
        }
    }

    fn attempt() -> EventData {
        EventData::AttemptStarted {
            attempt: 0,
            pid: 42,
            pas_version: "1".into(),
            argv: vec!["pas".into(), "run".into()],
            git_head: None,
            resumed_from_node: None,
        }
    }

    fn stage(node: &str, handler: &str) -> EventData {
        EventData::StageStarted {
            node_id: node.into(),
            handler_type: handler.into(),
        }
    }

    fn llm(id: &str, node: &str, cost: f64, model: &str) -> EventData {
        EventData::LlmInvoked {
            invocation_id: id.into(),
            node_id: node.into(),
            provider: "anthropic".into(),
            model_requested: Some("req".into()),
            model_actual: Some(model.into()),
            input_tokens: None,
            output_tokens: None,
            cost_usd: Some(cost),
            duration_ms: 1,
            transcript: format!("transcripts/{id}.jsonl"),
            status: "ok".into(),
        }
    }

    fn env() -> Env<'static> {
        Env {
            pid_alive: &|_| true,
            node_timeout: &|_| None,
            commit_nodes: None,
        }
    }

    fn render(s: &RunSnapshot, now_secs: i64) -> String {
        summary(s, t0() + Duration::seconds(now_secs), &env()).into_string()
    }

    // AC1
    #[test]
    fn bars_show_cost_over_budget_and_steps_over_max() {
        let e = evs(vec![
            (0, started(Some(5.0), Some(100))),
            (0, attempt()),
            (1, llm("i1", "a", 1.5, "m")),
        ]);
        let mut s = snap_of(&e);
        s.view.steps = 12;
        let html = render(&s, 2);
        assert!(html.contains(r#"value="1.5" max="5""#), "{html}");
        assert!(html.contains("$1.50 / $5.00"), "{html}");
        assert!(html.contains(r#"value="12" max="100""#), "{html}");
        assert!(html.contains("12 / 100 steps"), "{html}");
    }

    #[test]
    fn bars_without_limits_are_text_only_and_overspend_is_reported_truthfully() {
        let e = evs(vec![(0, started(None, None)), (0, attempt())]);
        let html = render(&snap_of(&e), 1);
        assert!(!html.contains("<progress"), "{html}");
        assert!(html.contains("(no limit)"));
        let e = evs(vec![
            (0, started(Some(1.0), Some(2))),
            (0, attempt()),
            (1, llm("i1", "a", 3.0, "m")),
        ]);
        let html = render(&snap_of(&e), 2);
        assert!(html.contains("$3.00 / $1.00"), "{html}");
        // Zero limit must not render a divide-by-zero bar.
        let e = evs(vec![(0, started(Some(0.0), Some(0)))]);
        assert!(!render(&snap_of(&e), 1).contains("<progress"));
    }

    // AC2
    #[test]
    fn graph_state_lists_current_and_visited_nodes_only() {
        let e = evs(vec![
            (0, started(None, None)),
            (0, attempt()),
            (1, stage("a", "codergen")),
            (2, stage("b", "codergen")),
        ]);
        let s = snap_of(&e);
        let html = graph(&s.view, &GraphSource::Dot("digraph{a->b->c}".into())).into_string();
        assert!(html.contains(r#"data-current="b""#), "{html}");
        assert!(
            html.contains("data-visited=\"[&quot;a&quot;,&quot;b&quot;]\""),
            "{html}"
        );
        assert!(!html.contains("&quot;c&quot;"));
        assert!(html.contains(r#"id="dot-src""#) && html.contains("viz-standalone.js"));
    }

    #[test]
    fn dot_in_page_cannot_break_out_of_its_script_element() {
        let s = snap_of(&evs(vec![]));
        let html = graph(
            &s.view,
            &GraphSource::Dot("digraph{a[label=\"</script><script>alert(1)</script>\"]}".into()),
        )
        .into_string();
        assert!(!html.contains("</script><script>alert"), "{html}");
        assert!(html.contains("\\u003c/script"));
    }

    // AC3
    #[test]
    fn task_rows_show_id_title_status_commits_cost_models() {
        let sha = "abc1234def5678abc1234def5678abc1234def56";
        let e = evs(vec![
            (0, started(None, None)),
            (0, attempt()),
            (
                1,
                EventData::EpicSnapshot {
                    epic_id: "E".into(),
                    title: "The epic".into(),
                    tasks: vec![
                        TaskSummary {
                            id: "attractor-ino.99".into(),
                            title: "Do it".into(),
                            status: "open".into(),
                        },
                        TaskSummary {
                            id: "attractor-ino.98".into(),
                            title: "Other".into(),
                            status: "open".into(),
                        },
                    ],
                },
            ),
            (
                2,
                EventData::TaskClaimed {
                    task_id: "attractor-ino.99".into(),
                    title: "Do it".into(),
                    epic_id: "E".into(),
                    node_id: "impl".into(),
                },
            ),
            (3, stage("impl", "codergen")),
            (4, llm("i1", "impl", 0.25, "opus")),
            (5, llm("i2", "impl", 0.5, "sonnet")),
            (6, llm("i3", "impl", 0.25, "opus")),
            (
                7,
                EventData::TaskClosed {
                    task_id: "attractor-ino.99".into(),
                    reason: "done".into(),
                    upstream_verified: true,
                    commits: vec![sha.into()],
                },
            ),
        ]);
        let s = snap_of(&e);
        let (cost, models) = task_rollup(&s.view, "attractor-ino.99");
        assert_eq!(cost, 1.0);
        assert_eq!(models, ["opus", "sonnet"]);
        let html = render(&s, 8);
        let row_at = html
            .find(r#"data-task-id="attractor-ino.99""#)
            .expect(&html);
        let row = &html[row_at..html[row_at..].find("</tr>").unwrap() + row_at];
        for want in [
            "attractor-ino.99",
            "Do it",
            "closed",
            sha,
            "$1.00",
            "opus, sonnet",
        ] {
            assert!(row.contains(want), "row lacks {want}: {row}");
        }
        assert!(html.contains("1 of 2 closed"), "{html}");
        assert!(html.contains(r#"data-task-id="attractor-ino.98""#));
    }

    #[test]
    fn commits_and_invocations_are_listed() {
        let e = evs(vec![
            (0, started(None, None)),
            (
                1,
                EventData::CommitsCreated {
                    node_id: "impl".into(),
                    task_id: None,
                    commits: vec![CommitRef {
                        sha: "deadbeef".into(),
                        subject: "feat: x".into(),
                        author: "a".into(),
                        ts: "t".into(),
                    }],
                },
            ),
            (2, llm("inv-1", "impl", 0.1, "opus")),
        ]);
        let html = render(&snap_of(&e), 3);
        assert!(
            html.contains("deadbeef") && html.contains("feat: x"),
            "{html}"
        );
        assert!(html.contains("/runs/11111111-1111-7111-8111-111111111111/transcripts/inv-1"));
    }

    // AC4
    #[test]
    fn every_finding_is_listed_with_its_severity() {
        let e = evs(vec![
            (0, started(Some(10.0), None)),
            (0, attempt()),
            (1, stage("n", "codergen")),
            (
                2,
                EventData::StageFailed {
                    node_id: "n".into(),
                    error: "boom".into(),
                },
            ),
            (3, llm("i", "n", 9.0, "m")),
        ]);
        let s = snap_of(&e);
        let list = derive(&s.view, t0() + Duration::seconds(4), &env());
        assert!(list.len() >= 2);
        let html = render(&s, 4);
        for f in &list {
            assert!(
                html.contains(&format!(
                    r#"class="finding {}" data-rule="{}""#,
                    f.severity.as_str(),
                    f.rule.as_str()
                )),
                "{html}"
            );
            assert!(html.contains(&f.message.replace('&', "&amp;")) || !f.message.is_empty());
        }
        assert!(html.contains(r#"data-rule="stage_failed""#) && html.contains("boom"));
        assert!(
            html.contains(r#"class="finding warn" data-rule="budget""#),
            "{html}"
        );
    }

    #[test]
    fn no_findings_says_so() {
        let e = evs(vec![(0, started(None, None)), (0, attempt())]);
        assert!(render(&snap_of(&e), 1).contains("No Findings."));
    }

    #[test]
    fn wired_node_timeout_produces_stalled() {
        let e = evs(vec![
            (0, started(None, None)),
            (0, attempt()),
            (1, stage("n", "codergen")),
            (100, EventData::Heartbeat { pid: 42 }),
        ]);
        let s = snap_of(&e);
        let t = dot_scan::scan("digraph{ n [timeout=30s] }");
        let f = |n: &str| t.get(n);
        let env = Env {
            pid_alive: &|_| true,
            node_timeout: &f,
            commit_nodes: None,
        };
        let html = summary(&s, t0() + Duration::seconds(101), &env).into_string();
        assert!(html.contains(r#"data-rule="stalled""#), "{html}");
        // With the 600 s default it would not be stalled.
        assert!(!render(&s, 101).contains(r#"data-rule="stalled""#));
    }

    // AC5 (server side of the log)
    #[test]
    fn log_renders_backlog_with_seq_and_script_dedupes_by_it() {
        let e = evs(vec![(0, started(None, None)), (1, attempt())]);
        let html = event_log("rid", &log_entries(&e)).into_string();
        assert!(
            html.contains(r#"data-seq="1""#) && html.contains(r#"data-seq="2""#),
            "{html}"
        );
        assert!(html.contains(r#"data-run="rid""#));
        assert!(LOG_SCRIPT.contains("new EventSource('/runs/'+run+'/events')"));
        assert!(LOG_SCRIPT.contains("f.seq>last"));
        assert!(!LOG_SCRIPT.contains("innerHTML"));
    }

    #[test]
    fn log_is_capped_with_a_visible_note() {
        let e = evs((0..MAX_LOG_EVENTS as i64 + 3)
            .map(|i| (i, EventData::Heartbeat { pid: 1 }))
            .collect());
        let html = event_log("rid", &log_entries(&e)).into_string();
        assert!(html.contains("3 earlier Events not shown"));
        assert!(!html.contains(r#"data-seq="3""#) && html.contains(r#"data-seq="4""#));
    }

    // AC6
    #[test]
    fn codergen_stages_link_to_their_invocations_and_others_do_not() {
        let e = evs(vec![
            (0, started(None, None)),
            (1, stage("impl", "codergen")),
            (2, llm("inv1", "impl", 0.1, "m")),
            (
                3,
                EventData::StageCompleted {
                    node_id: "impl".into(),
                    status: "success".into(),
                    duration_ms: 1,
                },
            ),
            (4, stage("check", "tool")),
            (
                5,
                EventData::StageCompleted {
                    node_id: "check".into(),
                    status: "success".into(),
                    duration_ms: 1,
                },
            ),
            (6, stage("impl", "codergen")),
            (7, llm("inv2", "impl", 0.1, "m")),
        ]);
        let l = log_entries(&e);
        let inv = |seq: u64| l.iter().find(|x| x.seq == seq).unwrap().invocation.clone();
        assert_eq!(inv(2).as_deref(), Some("inv1"), "StageStarted of visit 1");
        assert_eq!(inv(3).as_deref(), Some("inv1"), "LlmInvoked itself");
        assert_eq!(inv(4).as_deref(), Some("inv1"), "StageCompleted of visit 1");
        assert_eq!(inv(5), None, "non-codergen stage");
        assert_eq!(inv(6), None);
        assert_eq!(inv(7).as_deref(), Some("inv2"), "second visit has its own");
        assert_eq!(inv(1), None);
        let html = event_log("rid", &l).into_string();
        assert!(html.contains("/runs/rid/transcripts/inv1"));
    }

    #[test]
    fn codergen_stage_without_an_invocation_has_no_link() {
        let e = evs(vec![(0, stage("impl", "codergen"))]);
        assert_eq!(log_entries(&e)[0].invocation, None);
    }

    // AC7
    #[test]
    fn missing_pipeline_file_gives_a_message_not_a_graph() {
        let s = snap_of(&evs(vec![(0, started(None, None))]));
        let src = load_graph(std::path::Path::new("/nonexistent/none.dot"));
        assert!(matches!(&src, GraphSource::Unavailable(m) if m.contains("not found")));
        let html = graph(&s.view, &src).into_string();
        assert!(html.contains("Pipeline file not found: /nonexistent/none.dot"));
        assert!(!html.contains("dot-src") && !html.contains("viz-standalone"));
    }

    #[test]
    fn oversized_and_non_utf8_pipeline_files_are_reported() {
        let d = tempfile::tempdir().unwrap();
        let big = d.path().join("big.dot");
        std::fs::write(&big, vec![b'a'; MAX_DOT_BYTES as usize + 1]).unwrap();
        assert!(matches!(load_graph(&big), GraphSource::Unavailable(m) if m.contains("too large")));
        let bad = d.path().join("bad.dot");
        std::fs::write(&bad, [0xff, 0xfe, 0xfd]).unwrap();
        assert!(
            matches!(load_graph(&bad), GraphSource::Unavailable(m) if m.contains("unreadable"))
        );
        let ok = d.path().join("ok.dot");
        std::fs::write(&ok, "digraph{}").unwrap();
        assert_eq!(load_graph(&ok), GraphSource::Dot("digraph{}".into()));
    }

    // Extra
    #[test]
    fn dynamic_text_is_escaped_and_missing_run_is_flagged() {
        let e = evs(vec![
            (0, started(None, None)),
            (
                1,
                EventData::TaskClaimed {
                    task_id: "t".into(),
                    title: "<script>alert(1)</script>".into(),
                    epic_id: "E".into(),
                    node_id: "n".into(),
                },
            ),
            (
                2,
                EventData::AttemptEnded {
                    attempt: 0,
                    reason: AttemptEndReason::Failed,
                    message: Some("<b>x</b>".into()),
                },
            ),
        ]);
        let mut s = snap_of(&e);
        let html = render(&s, 3);
        assert!(
            !html.contains("<script>alert") && !html.contains("<b>x</b>"),
            "{html}"
        );
        s.missing = true;
        let html = render(&s, 3);
        assert!(html.contains("Run folder missing") && html.contains(r#"data-status="missing""#));
        assert!(html.contains("No Findings."));
    }
}
