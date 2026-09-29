//! The Transcript view of one Model Invocation: rendered messages and tool calls,
//! a raw view, and `?follow=1` live follow over SSE (PRD FR-6).
//!
//! Only files named `<token>.jsonl` under the Run's `transcripts/` folder are opened,
//! and only through [`resolve`].

use std::convert::Infallible;
use std::path::PathBuf;
use std::time::Duration;

use attractor_journal::{parse_run_id, RunDir};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use maud::{html, Markup, PreEscaped, DOCTYPE};
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::projection::ViewStatus;
use crate::state::{AppState, RunSnapshot};

/// The rendered page reads at most this much of a Transcript; the raw view has all of it.
const MAX_PAGE_BYTES: u64 = 8 * 1024 * 1024;
/// Tool inputs and results longer than this are cut for display.
const MAX_ITEM_CHARS: usize = 4096;
const POLL: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Role {
    Assistant,
    User,
}

/// One displayable piece of a Transcript line.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    Text {
        role: Role,
        text: String,
    },
    Thinking(String),
    ToolCall {
        name: String,
        input: String,
    },
    ToolResult {
        text: String,
        is_error: bool,
    },
    /// Session markers: init, turn boundaries, result summaries.
    Note(String),
    /// A line the renderer does not understand, shown as it is.
    Raw(String),
}

fn cut(s: &str) -> String {
    match s.char_indices().nth(MAX_ITEM_CHARS) {
        Some((i, _)) => format!("{}… [truncated, see the raw view]", &s[..i]),
        None => s.to_string(),
    }
}

fn pretty(v: &Value) -> String {
    match v {
        Value::String(s) => cut(s),
        other => cut(&serde_json::to_string_pretty(other).unwrap_or_default()),
    }
}

/// Text of a `tool_result` content: a string, or an array of `{text}` blocks.
fn result_text(v: &Value) -> String {
    match v {
        Value::Array(a) => a
            .iter()
            .map(|b| {
                b.get("text")
                    .and_then(Value::as_str)
                    .map_or_else(|| pretty(b), cut)
            })
            .collect::<Vec<_>>()
            .join("\n"),
        other => pretty(other),
    }
}

/// Turn one Transcript line into items. Total: anything unrecognised is `Raw`.
/// Understands Claude `stream-json` and Codex `--json`.
pub fn parse_line(line: &str) -> Vec<Item> {
    let raw = || vec![Item::Raw(line.to_string())];
    let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(line) else {
        return raw();
    };
    let Some(kind) = obj.get("type").and_then(Value::as_str) else {
        return raw();
    };
    let items: Option<Vec<Item>> = match kind {
        "system" => Some(vec![Item::Note(format!(
            "system {}",
            obj.get("subtype").and_then(Value::as_str).unwrap_or("")
        ))]),
        "rate_limit_event" => Some(vec![Item::Note("rate limit event".into())]),
        "assistant" | "user" => claude_message(kind, &obj),
        "result" => Some(vec![Item::Note(format!(
            "result{}: {}",
            if obj.get("is_error").and_then(Value::as_bool) == Some(true) {
                " (error)"
            } else {
                ""
            },
            obj.get("result")
                .and_then(Value::as_str)
                .map_or_else(String::new, cut)
        ))]),
        "thread.started" | "turn.started" | "turn.completed" => Some(vec![Item::Note(kind.into())]),
        "item.completed" => obj.get("item").and_then(codex_item),
        _ => None,
    };
    match items {
        Some(v) if !v.is_empty() => v,
        Some(_) => vec![Item::Note(format!("{kind} (empty)"))],
        None => raw(),
    }
}

fn claude_message(kind: &str, obj: &serde_json::Map<String, Value>) -> Option<Vec<Item>> {
    let content = obj.get("message")?.as_object()?.get("content")?;
    let role = if kind == "assistant" {
        Role::Assistant
    } else {
        Role::User
    };
    let blocks = match content {
        Value::String(s) => return Some(vec![Item::Text { role, text: cut(s) }]),
        Value::Array(a) => a,
        _ => return None,
    };
    let mut out = Vec::new();
    for b in blocks {
        let field = |k: &str| b.get(k).and_then(Value::as_str);
        match field("type")? {
            "text" => out.push(Item::Text {
                role,
                text: cut(field("text").unwrap_or("")),
            }),
            "thinking" => {
                if let Some(t) = field("thinking").filter(|t| !t.is_empty()) {
                    out.push(Item::Thinking(cut(t)));
                }
            }
            "tool_use" => out.push(Item::ToolCall {
                name: field("name").unwrap_or("tool").to_string(),
                input: b.get("input").map_or_else(String::new, pretty),
            }),
            "tool_result" => out.push(Item::ToolResult {
                text: b.get("content").map_or_else(String::new, result_text),
                is_error: b.get("is_error").and_then(Value::as_bool) == Some(true),
            }),
            _ => return None,
        }
    }
    Some(out)
}

fn codex_item(item: &Value) -> Option<Vec<Item>> {
    let field = |k: &str| item.get(k).and_then(Value::as_str);
    Some(match field("type")? {
        "agent_message" => vec![Item::Text {
            role: Role::Assistant,
            text: cut(field("text")?),
        }],
        "error" => vec![Item::ToolResult {
            text: cut(field("message").unwrap_or("")),
            is_error: true,
        }],
        "command_execution" => {
            let mut v = vec![Item::ToolCall {
                name: "command".into(),
                input: cut(field("command").unwrap_or("")),
            }];
            if let Some(out) = field("aggregated_output") {
                v.push(Item::ToolResult {
                    text: cut(out),
                    is_error: false,
                });
            }
            v
        }
        other => vec![Item::ToolCall {
            name: other.to_string(),
            input: pretty(item),
        }],
    })
}

fn render_item(item: &Item) -> Markup {
    match item {
        Item::Text { role, text } => {
            let (class, who) = match role {
                Role::Assistant => ("tx-assistant", "assistant"),
                Role::User => ("tx-user", "user"),
            };
            html! { div class={"tx " (class)} { span.who { (who) } pre { (text) } } }
        }
        Item::Thinking(t) => {
            html! { div class="tx tx-thinking" { span.who { "thinking" } pre { (t) } } }
        }
        Item::ToolCall { name, input } => {
            html! { div class="tx tx-tool" { span.who { "tool call " code { (name) } } pre { (input) } } }
        }
        Item::ToolResult { text, is_error } => html! {
            div class={"tx tx-result" @if *is_error { " tx-error" }} {
                span.who { @if *is_error { "error" } @else { "tool result" } }
                pre { (text) }
            }
        },
        Item::Note(n) => html! { div class="tx tx-note" { (n) } },
        Item::Raw(l) => html! { pre class="tx tx-raw" { (l) } },
    }
}

/// One fragment per Transcript line; every dynamic string is escaped by maud.
pub fn render_line(line: &str) -> Markup {
    html! { @for item in parse_line(line) { (render_item(&item)) } }
}

/// The only place a Transcript filename is built. `None` means 404 with no
/// filesystem access.
pub fn resolve(snap: &RunSnapshot, inv: &str) -> Option<PathBuf> {
    let token = !inv.is_empty()
        && inv.len() <= 64
        && inv
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !token {
        return None;
    }
    // The engine records `LlmInvoked` only after the provider exits, so a live
    // Transcript is reachable by the UUID form the engine mints.
    let id = if snap.view.invocations.iter().any(|i| i.invocation_id == inv) {
        inv.to_string()
    } else {
        parse_run_id(inv)?
    };
    Some(RunDir::from_path(&snap.entry.run_dir).transcript(&id))
}

#[derive(Debug, Deserialize, Default)]
pub struct Params {
    follow: Option<String>,
    raw: Option<String>,
    from: Option<u64>,
}

fn flag(v: &Option<String>) -> bool {
    matches!(v.as_deref(), Some("1" | "true"))
}

fn terminal(state: &AppState, run_id: &str) -> bool {
    state.snapshot(run_id).is_none_or(|s| {
        matches!(
            s.view.status,
            ViewStatus::Completed | ViewStatus::Failed | ViewStatus::Stopped
        )
    })
}

/// Complete (newline-terminated) lines of `bytes`, with the end offset of each,
/// starting at `base`. The second value is the bytes left over (a torn last line).
fn split_lines(bytes: &[u8], base: u64) -> (Vec<(u64, String)>, &[u8]) {
    let mut out = Vec::new();
    let mut start = 0;
    while let Some(p) = bytes[start..].iter().position(|&b| b == b'\n') {
        let end = start + p;
        let line = String::from_utf8_lossy(&bytes[start..end]);
        out.push((
            base + end as u64 + 1,
            line.trim_end_matches('\r').to_string(),
        ));
        start = end + 1;
    }
    (out, &bytes[start..])
}

/// Reads complete lines appended to a file. A torn last line waits until finished.
struct LineTail {
    path: PathBuf,
    offset: u64,
}

impl LineTail {
    async fn poll(&mut self) -> Vec<(u64, String)> {
        let Ok(mut f) = tokio::fs::File::open(&self.path).await else {
            return Vec::new();
        };
        if f.seek(std::io::SeekFrom::Start(self.offset)).await.is_err() {
            return Vec::new();
        }
        let mut buf = Vec::new();
        if f.take(MAX_PAGE_BYTES).read_to_end(&mut buf).await.is_err() {
            return Vec::new();
        }
        let (lines, rest) = split_lines(&buf, self.offset);
        let mut lines = lines;
        if let Some(&(end, _)) = lines.last() {
            self.offset = end;
        } else if rest.len() as u64 >= MAX_PAGE_BYTES {
            // A single line longer than the cap: emit it rather than stall.
            self.offset += rest.len() as u64;
            lines.push((self.offset, String::from_utf8_lossy(rest).into_owned()));
        }
        lines
    }
}

pub async fn handler(
    State(state): State<AppState>,
    Path((id, inv)): Path<(String, String)>,
    Query(q): Query<Params>,
    headers: HeaderMap,
) -> Response {
    let Some(run_id) = parse_run_id(&id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(snap) = state.snapshot(&run_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(path) = resolve(&snap, &inv) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(meta) = tokio::fs::metadata(&path).await else {
        return (StatusCode::NOT_FOUND, "Transcript file not found\n").into_response();
    };
    if !meta.is_file() {
        return StatusCode::NOT_FOUND.into_response();
    }
    if flag(&q.follow) {
        let from = headers
            .get("last-event-id")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .or(q.from)
            .unwrap_or(0);
        return follow(state, run_id, path, from);
    }
    if flag(&q.raw) {
        return match tokio::fs::read(&path).await {
            Ok(b) => ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], b).into_response(),
            Err(_) => (StatusCode::NOT_FOUND, "Transcript file not found\n").into_response(),
        };
    }
    let mut buf = Vec::new();
    let read = async {
        let f = tokio::fs::File::open(&path).await?;
        f.take(MAX_PAGE_BYTES).read_to_end(&mut buf).await
    };
    if read.await.is_err() {
        return (StatusCode::NOT_FOUND, "Transcript file not found\n").into_response();
    }
    let live = !terminal(&state, &run_id);
    let too_big = meta.len() > MAX_PAGE_BYTES;
    Html(page(&snap, &inv, &buf, live, too_big).into_string()).into_response()
}

fn follow(state: AppState, run_id: String, path: PathBuf, from: u64) -> Response {
    let (tx, out) = mpsc::channel::<Result<Event, Infallible>>(64);
    tokio::spawn(async move {
        let mut tail = LineTail { path, offset: from };
        loop {
            let done = terminal(&state, &run_id);
            let lines = tail.poll().await;
            let idle = lines.is_empty();
            for (end, line) in lines {
                let ev = Event::default()
                    .event("line")
                    .id(end.to_string())
                    .data(render_line(&line).into_string());
                if tx.send(Ok(ev)).await.is_err() {
                    return;
                }
            }
            if idle {
                if done {
                    let _ = tx.send(Ok(Event::default().event("end").data("end"))).await;
                    return;
                }
                tokio::time::sleep(POLL).await;
                if tx.is_closed() {
                    return;
                }
            }
        }
    });
    Sse::new(ReceiverStream::new(out))
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

const FOLLOW_SCRIPT: &str = "(function(){var b=document.getElementById('tx');\
if(!b||b.dataset.live!=='1')return;\
var es=new EventSource('?follow=1&from='+b.dataset.offset);\
es.addEventListener('line',function(e){b.insertAdjacentHTML('beforeend',e.data);});\
es.addEventListener('end',function(){es.close();});})();";

fn page(snap: &RunSnapshot, inv: &str, bytes: &[u8], live: bool, too_big: bool) -> Markup {
    let (lines, rest) = split_lines(bytes, 0);
    let offset = lines.last().map_or(0, |l| l.0);
    let run_id = &snap.entry.run_id;
    let row = snap
        .view
        .invocations
        .iter()
        .find(|i| i.invocation_id == inv);
    // A torn tail is shown only when nothing will complete it.
    let tail = (!live && !rest.is_empty()).then(|| String::from_utf8_lossy(rest).into_owned());
    html! {
        (DOCTYPE)
        html {
            head {
                meta charset="utf-8";
                title { "Transcript " (inv) }
                link rel="stylesheet" href="/assets/monitor.css";
            }
            body {
                p { a href={"/runs/" (run_id)} { "Run " (run_id) } }
                h1 { "Transcript " code { (inv) } }
                p {
                    @if let Some(r) = row {
                        "Model Invocation: " (r.provider) " " (r.model_actual.as_deref().or(r.model_requested.as_deref()).unwrap_or("")) " · "
                    }
                    "Rendered · " a href="?raw=1" { "Raw" }
                    @if live { " · following" }
                }
                @if too_big { p.notice { "Transcript is large; only the first 8 MiB are rendered. See the raw view." } }
                div #tx data-live=(if live { "1" } else { "0" }) data-offset=(offset) {
                    @for (_, line) in &lines { (render_line(line)) }
                    @if let Some(t) = &tail { (render_line(t)) }
                }
                @if live { script { (PreEscaped(FOLLOW_SCRIPT)) } }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projection::RunView;
    use attractor_journal::IndexEntry;
    use chrono::Utc;

    fn snap(dir: &str) -> RunSnapshot {
        RunSnapshot {
            entry: IndexEntry::new("rid", Utc::now(), "/w", "p.dot", std::path::Path::new(dir)),
            view: RunView::default(),
            missing: false,
            answers_sent: Default::default(),
        }
    }

    #[test]
    fn claude_assistant_blocks_in_order() {
        let l = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":""},{"type":"text","text":"hi"},{"type":"tool_use","id":"t","name":"Bash","input":{"command":"ls"}}]}}"#;
        let items = parse_line(l);
        assert_eq!(items.len(), 2, "{items:?}");
        assert_eq!(
            items[0],
            Item::Text {
                role: Role::Assistant,
                text: "hi".into()
            }
        );
        assert!(
            matches!(&items[1], Item::ToolCall { name, input } if name == "Bash" && input.contains("ls"))
        );
    }

    #[test]
    fn claude_tool_result_and_codex_items() {
        let l = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t","content":"out","is_error":true}]}}"#;
        assert_eq!(
            parse_line(l),
            vec![Item::ToolResult {
                text: "out".into(),
                is_error: true
            }]
        );
        let l = r#"{"type":"item.completed","item":{"type":"agent_message","text":"OK"}}"#;
        assert_eq!(
            parse_line(l),
            vec![Item::Text {
                role: Role::Assistant,
                text: "OK".into()
            }]
        );
        let l = r#"{"type":"item.completed","item":{"type":"command_execution","command":"ls","aggregated_output":"a"}}"#;
        assert_eq!(parse_line(l).len(), 2);
    }

    #[test]
    fn garbage_is_raw_and_never_panics() {
        let deep = "[".repeat(100_000);
        let huge = "x".repeat(1 << 20);
        for g in [
            "",
            "not json",
            "{",
            "42",
            "null",
            "[]",
            "{\"type\":5}",
            "{\"type\":\"assistant\",\"message\":42}",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"weird\"}]}}",
            "{\"type\":\"gemini-thing\"}",
            "\u{0}\u{1}",
            deep.as_str(),
            huge.as_str(),
        ] {
            let items = parse_line(g);
            assert!(!items.is_empty());
            assert!(matches!(items[0], Item::Raw(_)), "{g:.30}");
        }
        // Empty content still yields something to show.
        assert!(!parse_line(r#"{"type":"assistant","message":{"content":[]}}"#).is_empty());
    }

    #[test]
    fn long_items_are_truncated_and_html_is_escaped() {
        let long = "a".repeat(MAX_ITEM_CHARS + 10);
        let l = format!(
            r#"{{"type":"item.completed","item":{{"type":"agent_message","text":"{long}"}}}}"#
        );
        let Item::Text { text, .. } = &parse_line(&l)[0] else {
            panic!()
        };
        assert!(text.contains("truncated") && text.len() < long.len() + 60);
        let out = render_line("<script>alert(1)</script>").into_string();
        assert!(!out.contains("<script>") && out.contains("&lt;script&gt;"));
    }

    #[test]
    fn resolve_never_escapes_the_transcripts_folder() {
        let s = snap("/runs/x");
        let base = RunDir::from_path("/runs/x").transcripts_dir();
        for bad in [
            "..",
            "../..",
            "../../etc/passwd",
            "..%2f",
            "a/b",
            "a\\b",
            "a.jsonl",
            "",
            "x\0",
            ".",
            "secret/../../x",
            &"a".repeat(65),
        ] {
            assert_eq!(resolve(&s, bad), None, "{bad}");
        }
        // Not recorded and not a UUID.
        assert_eq!(resolve(&s, "secret"), None);
        let uuid = "0192a3b4-c5d6-7e8f-9a0b-1c2d3e4f5a6b";
        let p = resolve(&s, uuid).unwrap();
        assert_eq!(p, base.join(format!("{uuid}.jsonl")));
        assert!(p.starts_with(&base));
    }

    #[test]
    fn split_lines_holds_back_a_torn_line() {
        let (l, rest) = split_lines(b"a\r\nb\nc", 10);
        assert_eq!(l, vec![(13, "a".to_string()), (15, "b".to_string())]);
        assert_eq!(rest, b"c");
    }
}
