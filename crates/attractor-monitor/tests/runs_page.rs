//! Runs page over HTTP (attractor-ino.33).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use attractor_journal::{
    append_entry_at, new_run_id, AttemptEndReason, EventData, IndexEntry, JournalWriter,
};
use attractor_monitor::state::AppState;
use attractor_monitor::{bind, serve_on_with, watcher};
use chrono::Utc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

struct Env {
    _tmp: tempfile::TempDir,
    index: PathBuf,
    root: PathBuf,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    Env {
        index: root.join("runs.jsonl"),
        root,
        _tmp: tmp,
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

fn attempt_started(pid: u32) -> EventData {
    EventData::AttemptStarted {
        attempt: 0,
        pid,
        pas_version: "1".into(),
        argv: vec!["pas".into()],
        git_head: None,
        resumed_from_node: None,
    }
}

fn new_run(e: &Env, workdir: &str, pid: u32) -> (String, JournalWriter) {
    let id = new_run_id();
    let dir = e.root.join(&id);
    std::fs::create_dir_all(&dir).unwrap();
    let w = JournalWriter::open(&dir, &id, 0).unwrap();
    w.append(started(workdir)).unwrap();
    w.append(attempt_started(pid)).unwrap();
    let entry = IndexEntry::new(&id, Utc::now(), workdir, "p.dot", &dir);
    append_entry_at(&e.index, &entry).unwrap();
    (id, w)
}

/// A journal whose only Events are 10 minutes old.
fn new_stale_run(e: &Env, workdir: &str, pid: u32) -> String {
    let id = new_run_id();
    let dir = e.root.join(&id);
    std::fs::create_dir_all(&dir).unwrap();
    let old = (Utc::now() - chrono::Duration::minutes(10)).to_rfc3339();
    let mut lines = String::new();
    for (seq, data) in [(1, started(workdir)), (2, attempt_started(pid))] {
        let ev = attractor_journal::JournalEvent::new(seq, Utc::now(), &id, 0, data);
        let mut v = serde_json::to_value(&ev).unwrap();
        v["ts"] = serde_json::Value::String(old.clone());
        lines.push_str(&format!("{v}\n"));
    }
    std::fs::write(dir.join("events.jsonl"), lines).unwrap();
    let entry = IndexEntry::new(&id, Utc::now(), workdir, "p.dot", &dir);
    append_entry_at(&e.index, &entry).unwrap();
    id
}

async fn serve(e: &Env) -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let state = AppState::new(&e.index);
    watcher::spawn(state.clone(), e.index.clone(), Duration::from_millis(100));
    let l = bind(0).await.unwrap();
    let addr = l.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(serve_on_with(l, state, async {
        let _ = rx.await;
    }));
    (addr, tx)
}

async fn get_with_host(addr: SocketAddr, path: &str, host: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let req = format!("GET {path} HTTP/1.0\r\nHost: {host}\r\n\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    (
        head.split(' ').nth(1).unwrap().parse().unwrap(),
        body.to_string(),
    )
}

async fn get(addr: SocketAddr, path: &str) -> (u16, String) {
    get_with_host(addr, path, &format!("127.0.0.1:{}", addr.port())).await
}

async fn wait_body(addr: SocketAddr, path: &str, want: &str, within: Duration) -> Option<Duration> {
    let start = Instant::now();
    while start.elapsed() < within {
        let (_, body) = get(addr, path).await;
        if body.contains(want) {
            return Some(start.elapsed());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
}

// --- AC 5 -------------------------------------------------------------

#[tokio::test]
async fn status_change_visible_within_5s_without_reload() {
    let e = env();
    let (_id, w) = new_run(&e, "/r/a", std::process::id());
    let (addr, _stop) = serve(&e).await;

    // What the open page polls (hx-get="/runs" every 2 s).
    assert!(wait_body(
        addr,
        "/runs",
        r#"data-status="running""#,
        Duration::from_secs(5)
    )
    .await
    .is_some());
    w.append(EventData::AttemptEnded {
        attempt: 0,
        reason: AttemptEndReason::Completed,
        message: None,
    })
    .unwrap();
    let took = wait_body(
        addr,
        "/runs",
        r#"data-status="completed""#,
        Duration::from_secs(5),
    )
    .await;
    assert!(took.is_some(), "status change not visible within 5 s");
}

#[tokio::test]
async fn page_polls_the_table_fragment() {
    let e = env();
    let (addr, _stop) = serve(&e).await;
    let (code, body) = get(addr, "/").await;
    assert_eq!(code, 200);
    for want in [
        r#"hx-get="/runs""#,
        r#"hx-trigger="every 2s""#,
        r##"hx-include="#filters""##,
        r#"src="/assets/htmx.min.js""#,
        "pas run",
    ] {
        assert!(body.contains(want), "missing {want}: {body}");
    }
}

// --- AC 1/3/4 over HTTP ---------------------------------------------------

#[tokio::test]
async fn repo_and_status_filters_over_http() {
    let e = env();
    let (_a, _wa) = new_run(&e, "/r/a", std::process::id());
    let (_b, wb) = new_run(&e, "/r/b", std::process::id());
    wb.append(EventData::AttemptEnded {
        attempt: 0,
        reason: AttemptEndReason::Completed,
        message: None,
    })
    .unwrap();
    // A child that has exited: its PID is gone.
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let dead = child.id();
    child.wait().unwrap();
    let _c = new_stale_run(&e, "/r/c", dead);
    let (addr, _stop) = serve(&e).await;

    wait_body(addr, "/runs", "/r/c", Duration::from_secs(5))
        .await
        .expect("runs loaded");
    wait_body(addr, "/runs", "/r/b", Duration::from_secs(5))
        .await
        .unwrap();
    wait_body(
        addr,
        "/runs",
        r#"data-status="completed""#,
        Duration::from_secs(5),
    )
    .await
    .unwrap();

    let (code, body) = get(addr, "/?repo=/r/a").await;
    assert_eq!(code, 200);
    assert!(
        body.contains("<td class=\"repository\">/r/a</td>"),
        "{body}"
    );
    assert!(!body.contains("<td class=\"repository\">/r/b</td>"));
    assert!(!body.contains("<td class=\"repository\">/r/c</td>"));

    let (_, body) = get(addr, "/runs?status=crashed").await;
    assert!(body.contains("/r/c"), "{body}");
    assert!(!body.contains("/r/a<") && !body.contains("/r/b<"), "{body}");

    let (_, body) = get(addr, "/runs?status=failed").await;
    assert!(body.contains("No Runs match"));
}

// --- security ---------------------------------------------------------

#[tokio::test]
async fn foreign_host_is_403_on_runs_fragment() {
    let e = env();
    let (addr, _stop) = serve(&e).await;
    let (code, _) = get_with_host(addr, "/runs", "evil.example").await;
    assert_eq!(code, 403);
    let (code, _) = get_with_host(addr, "/", "evil.example").await;
    assert_eq!(code, 403);
}
