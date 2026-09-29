//! Run page over HTTP (attractor-ino.34).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use attractor_journal::{append_entry_at, new_run_id, EventData, IndexEntry, JournalWriter};
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
    new_run_with(e, workdir, pid, "p.dot")
}

fn new_run_with(e: &Env, workdir: &str, pid: u32, pipeline: &str) -> (String, JournalWriter) {
    let id = new_run_id();
    let dir = e.root.join(&id);
    std::fs::create_dir_all(&dir).unwrap();
    let w = JournalWriter::open(&dir, &id, 0).unwrap();
    w.append(started(workdir)).unwrap();
    w.append(attempt_started(pid)).unwrap();
    let entry = IndexEntry::new(&id, Utc::now(), workdir, pipeline, &dir);
    append_entry_at(&e.index, &entry).unwrap();
    (id, w)
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

fn stage(node: &str, handler: &str) -> EventData {
    EventData::StageStarted {
        node_id: node.into(),
        handler_type: handler.into(),
    }
}

fn llm(id: &str, node: &str) -> EventData {
    EventData::LlmInvoked {
        invocation_id: id.into(),
        node_id: node.into(),
        provider: "anthropic".into(),
        model_requested: None,
        model_actual: Some("opus".into()),
        input_tokens: None,
        output_tokens: None,
        cost_usd: Some(0.5),
        duration_ms: 1,
        transcript: format!("transcripts/{id}.jsonl"),
        status: "ok".into(),
    }
}

async fn ready(addr: SocketAddr, id: &str) {
    wait_body(
        addr,
        &format!("/runs/{id}/summary"),
        "Attempts",
        Duration::from_secs(5),
    )
    .await
    .expect("run loaded");
}

// --- AC 1, 3, 4 over HTTP ---------------------------------------------------

#[tokio::test]
async fn page_shows_bars_tasks_findings_and_sections() {
    let e = env();
    let dot = e.root.join("p.dot");
    std::fs::write(&dot, "digraph g { a -> b }").unwrap();
    let (id, w) = new_run_with(&e, "/r/a", std::process::id(), dot.to_str().unwrap());
    w.append(stage("a", "codergen")).unwrap();
    w.append(EventData::TaskClaimed {
        task_id: "attractor-ino.99".into(),
        title: "Do it".into(),
        epic_id: "E".into(),
        node_id: "a".into(),
    })
    .unwrap();
    w.append(llm("inv-1", "a")).unwrap();
    w.append(EventData::StageFailed {
        node_id: "a".into(),
        error: "boom".into(),
    })
    .unwrap();
    let (addr, _stop) = serve(&e).await;
    wait_body(
        addr,
        &format!("/runs/{id}/summary"),
        "stage_failed",
        Duration::from_secs(5),
    )
    .await
    .expect("events applied");

    let (code, body) = get(addr, &format!("/runs/{id}")).await;
    assert_eq!(code, 200);
    for want in [
        "Budget",
        "no limit",
        "1 steps",
        r#"data-task-id="attractor-ino.99""#,
        "Do it",
        "opus",
        r#"class="finding critical" data-rule="stage_failed""#,
        r#"id="dot-src""#,
        r#"data-current="a""#,
        "&quot;a&quot;",
        r#"id="event-log""#,
        r#"hx-get="/runs/"#,
        r#"hx-trigger="every 2s""#,
        "new EventSource",
    ] {
        assert!(body.contains(want), "missing {want}: {body}");
    }
}

async fn read_until(
    want: &str,
    s: &mut TcpStream,
    seen: &mut String,
    start: Instant,
) -> Option<()> {
    let mut buf = [0u8; 4096];
    while !seen.contains(want) {
        let left = Duration::from_secs(2).checked_sub(start.elapsed())?;
        let n = tokio::time::timeout(left, s.read(&mut buf))
            .await
            .ok()?
            .ok()?;
        if n == 0 {
            return None;
        }
        seen.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    Some(())
}

// --- AC 5 -------------------------------------------------------------------

#[tokio::test]
async fn event_appears_in_log_stream_within_2s() {
    let e = env();
    let (id, w) = new_run(&e, "/r/a", std::process::id());
    let (addr, _stop) = serve(&e).await;
    ready(addr, &id).await;

    let mut s = TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "GET /runs/{id}/events HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        addr.port()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    // Backlog first (RunStarted + AttemptStarted), so the stream is live.
    let mut seen = String::new();
    let start = Instant::now();
    read_until("AttemptStarted", &mut s, &mut seen, start)
        .await
        .expect("backlog");
    w.append(stage("newnode", "codergen")).unwrap();
    let start = Instant::now();
    read_until("newnode", &mut s, &mut seen, start)
        .await
        .expect("new Event not streamed within 2 s");
    assert!(seen.contains("event: journal"));
}

// --- AC 6 -------------------------------------------------------------------

#[tokio::test]
async fn transcript_link_opens_the_invocations_transcript() {
    let e = env();
    let (id, w) = new_run(&e, "/r/a", std::process::id());
    w.append(stage("impl", "codergen")).unwrap();
    w.append(llm("inv-1", "impl")).unwrap();
    let dir = e.root.join(&id).join("transcripts");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("inv-1.jsonl"), "{\"hello\":\"transcript\"}\n").unwrap();
    // A file no invocation refers to must stay unreachable.
    std::fs::write(dir.join("secret.jsonl"), "secret").unwrap();
    std::fs::write(e.root.join("outside.txt"), "outside").unwrap();
    let (addr, _stop) = serve(&e).await;
    wait_body(
        addr,
        &format!("/runs/{id}/summary"),
        "inv-1",
        Duration::from_secs(5),
    )
    .await
    .expect("invocation applied");

    let (_, page) = get(addr, &format!("/runs/{id}")).await;
    let link = format!("/runs/{id}/transcripts/inv-1");
    assert!(page.contains(&format!(r#"href="{link}""#)), "{page}");
    let (code, body) = get(addr, &format!("{link}?raw=1")).await;
    assert_eq!((code, body.trim()), (200, "{\"hello\":\"transcript\"}"));
    let (code, body) = get(addr, &link).await;
    assert_eq!(code, 200);
    assert!(
        body.contains("Transcript") && body.contains("tx-raw"),
        "{body}"
    );

    for bad in [
        "secret",
        "..",
        "..%2foutside.txt",
        "..%2f..%2foutside",
        "inv-1.jsonl",
        "%2e%2e",
    ] {
        let (code, body) = get(addr, &format!("/runs/{id}/transcripts/{bad}")).await;
        assert_eq!(code, 404, "{bad}");
        assert!(!body.contains("secret") && !body.contains("outside"));
    }
    let (code, _) = get(addr, "/runs/not-a-uuid/transcripts/inv-1").await;
    assert_eq!(code, 404);
    let (code, _) = get_with_host(addr, &link, "evil.example").await;
    assert_eq!(code, 403);
}

#[tokio::test]
async fn known_invocation_with_no_transcript_file_is_404() {
    let e = env();
    let (id, w) = new_run(&e, "/r/a", std::process::id());
    w.append(llm("inv-1", "impl")).unwrap();
    let (addr, _stop) = serve(&e).await;
    wait_body(
        addr,
        &format!("/runs/{id}/summary"),
        "inv-1",
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    let (code, _) = get(addr, &format!("/runs/{id}/transcripts/inv-1")).await;
    assert_eq!(code, 404);
}

// --- AC 7 -------------------------------------------------------------------

#[tokio::test]
async fn deleted_pipeline_file_hides_only_the_graph() {
    let e = env();
    let dot = e.root.join("gone.dot");
    let (id, w) = new_run_with(&e, "/r/a", std::process::id(), dot.to_str().unwrap());
    w.append(stage("a", "codergen")).unwrap();
    let (addr, _stop) = serve(&e).await;
    ready(addr, &id).await;

    let (code, body) = get(addr, &format!("/runs/{id}")).await;
    assert_eq!(code, 200);
    assert!(body.contains("Pipeline file not found:"), "{body}");
    for want in [
        "<header>",
        r#"id="attempts""#,
        r#"id="tasks""#,
        r#"id="commits""#,
        r#"id="models""#,
        r#"id="findings""#,
        r#"id="event-log""#,
    ] {
        assert!(body.contains(want), "missing {want}: {body}");
    }
    assert!(!body.contains("viz-standalone") && !body.contains("dot-src"));
}

// --- Findings wiring -----------------------------------------------------------

#[tokio::test]
async fn node_timeout_from_the_pipeline_file_reaches_findings() {
    let e = env();
    let dot = e.root.join("p.dot");
    std::fs::write(&dot, "digraph { n [timeout=1s] }").unwrap();
    let (id, w) = new_run_with(&e, "/r/a", std::process::id(), dot.to_str().unwrap());
    w.append(stage("n", "codergen")).unwrap();
    let (addr, _stop) = serve(&e).await;
    ready(addr, &id).await;
    // Silent for over 1 s with a fresh heartbeat: Stalled by the file's timeout.
    tokio::time::sleep(Duration::from_millis(1300)).await;
    w.append(EventData::Heartbeat {
        pid: std::process::id(),
    })
    .unwrap();
    // A heartbeat is activity, so wait for silence again.
    tokio::time::sleep(Duration::from_millis(1300)).await;
    let took = wait_body(
        addr,
        &format!("/runs/{id}/summary"),
        r#"data-rule="stalled""#,
        Duration::from_secs(3),
    )
    .await;
    assert!(
        took.is_some(),
        "stalled Finding not derived from timeout=1s"
    );
}

// --- routing and security -------------------------------------------------------

#[tokio::test]
async fn unknown_and_invalid_run_ids_are_404() {
    let e = env();
    let (addr, _stop) = serve(&e).await;
    for p in [
        "/runs/not-a-uuid",
        "/runs/0190a1a1-0000-7000-8000-000000000000",
        "/runs/..%2f..%2fetc",
        "/runs/not-a-uuid/summary",
        "/runs/0190a1a1-0000-7000-8000-000000000000/summary",
    ] {
        let (code, _) = get(addr, p).await;
        assert_eq!(code, 404, "{p}");
    }
}

#[tokio::test]
async fn foreign_host_is_403_on_run_page_and_summary() {
    let e = env();
    let (id, _w) = new_run(&e, "/r/a", std::process::id());
    let (addr, _stop) = serve(&e).await;
    ready(addr, &id).await;
    for p in [format!("/runs/{id}"), format!("/runs/{id}/summary")] {
        let (code, _) = get_with_host(addr, &p, "evil.example").await;
        assert_eq!(code, 403, "{p}");
    }
}

#[tokio::test]
async fn runs_table_links_to_the_run_page() {
    let e = env();
    let (id, _w) = new_run(&e, "/r/a", std::process::id());
    let (addr, _stop) = serve(&e).await;
    let link = format!(r#"href="/runs/{id}""#);
    wait_body(addr, "/runs", &link, Duration::from_secs(5))
        .await
        .expect("row links to the Run page");
}

#[tokio::test]
async fn summary_fragment_carries_graph_state_out_of_band() {
    let e = env();
    let (id, w) = new_run(&e, "/r/a", std::process::id());
    w.append(stage("n", "codergen")).unwrap();
    let (addr, _stop) = serve(&e).await;
    let (_, body) = {
        wait_body(
            addr,
            &format!("/runs/{id}/summary"),
            r#"data-current="n""#,
            Duration::from_secs(5),
        )
        .await
        .expect("state updated");
        get(addr, &format!("/runs/{id}/summary")).await
    };
    assert!(
        body.contains(r#"hx-swap-oob="true""#) && body.contains("&quot;n&quot;"),
        "{body}"
    );
}
