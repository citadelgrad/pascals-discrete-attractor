//! Transcript view over HTTP (attractor-ino.35).

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

const UUID: &str = "0192a3b4-c5d6-7e8f-9a0b-1c2d3e4f5a6b";

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn transcripts(e: &Env, id: &str) -> PathBuf {
    let d = e.root.join(id).join("transcripts");
    std::fs::create_dir_all(&d).unwrap();
    d
}

async fn run_ready(addr: SocketAddr, id: &str) {
    wait_body(
        addr,
        &format!("/runs/{id}/summary"),
        "Attempts",
        Duration::from_secs(5),
    )
    .await
    .expect("run loaded");
}

fn positions(body: &str, wants: &[&str]) {
    let mut last = 0;
    for w in wants {
        let p = body
            .find(w)
            .unwrap_or_else(|| panic!("missing {w}: {body}"));
        assert!(p > last, "{w} out of order");
        last = p;
    }
}

// AC 1
#[tokio::test]
async fn finished_transcript_renders_every_message_and_tool_call_in_order() {
    let e = env();
    let (id, w) = new_run(&e, "/r/a", std::process::id());
    w.append(llm(UUID, "n")).unwrap();
    let dir = transcripts(&e, &id);
    std::fs::write(
        dir.join(format!("{UUID}.jsonl")),
        fixture("transcript_claude.jsonl"),
    )
    .unwrap();
    std::fs::write(dir.join("codex-1.jsonl"), fixture("transcript_codex.jsonl")).unwrap();
    let (addr, _stop) = serve(&e).await;
    run_ready(addr, &id).await;

    let (code, body) = get(addr, &format!("/runs/{id}/transcripts/{UUID}")).await;
    assert_eq!(code, 200);
    positions(
        &body,
        &[
            "pondering-alpha",
            "message-bravo",
            "ToolCharlie",
            "echo delta",
            "result-echo",
            "message-foxtrot",
            "done-golf",
        ],
    );
    assert!(body.contains("?raw=1"));

    // Codex, recorded by the journal.
    w.append(llm("codex-1", "n")).unwrap();
    wait_body(
        addr,
        &format!("/runs/{id}/summary"),
        "codex-1",
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    let (code, body) = get(addr, &format!("/runs/{id}/transcripts/codex-1")).await;
    assert_eq!(code, 200);
    positions(&body, &["cmd-hotel", "out-india", "message-juliet"]);
}

// pi Transcripts render as messages, tool calls and results (attractor-9bg.12).
#[tokio::test]
async fn pi_transcript_page_renders_messages_tool_calls_and_results() {
    let e = env();
    let (id, w) = new_run(&e, "/r/pi", std::process::id());
    w.append(llm("pi-1", "n")).unwrap();
    let pi = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../attractor-pipeline/tests/fixtures/providers/pi-1.0.4.jsonl"),
    )
    .unwrap();
    std::fs::write(transcripts(&e, &id).join("pi-1.jsonl"), pi).unwrap();
    let (addr, _stop) = serve(&e).await;
    run_ready(addr, &id).await;

    let (code, body) = get(addr, &format!("/runs/{id}/transcripts/pi-1")).await;
    assert_eq!(code, 200);
    positions(
        &body,
        &["pi session", "echo ok", "tool result", "<pre>done</pre>"],
    );
    assert!(
        body.contains("usage: ") && body.contains("0.00425"),
        "{body}"
    );
    assert!(!body.contains("tx-raw"), "{body}");
}

// AC 2
async fn read_until(s: &mut TcpStream, want: &str, within: Duration) -> (String, Duration) {
    let start = Instant::now();
    let mut acc = String::new();
    let mut buf = [0u8; 4096];
    while start.elapsed() < within {
        if let Ok(Ok(n)) = tokio::time::timeout(Duration::from_millis(100), s.read(&mut buf)).await
        {
            if n == 0 {
                break;
            }
            acc.push_str(&String::from_utf8_lossy(&buf[..n]));
            if acc.contains(want) {
                return (acc, start.elapsed());
            }
        }
    }
    panic!("{want} not seen within {within:?}: {acc}");
}

async fn open_follow(addr: SocketAddr, path: &str) -> TcpStream {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        addr.port()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    s
}

#[tokio::test]
async fn follow_shows_appended_lines_of_an_active_transcript_within_2s() {
    let e = env();
    // Active: no LlmInvoked yet, UUID-named file.
    let (id, _w) = new_run(&e, "/r/a", std::process::id());
    let file = transcripts(&e, &id).join(format!("{UUID}.jsonl"));
    std::fs::write(&file, "").unwrap();
    let (addr, _stop) = serve(&e).await;
    run_ready(addr, &id).await;

    let (code, page) = get(addr, &format!("/runs/{id}/transcripts/{UUID}")).await;
    assert_eq!(code, 200);
    assert!(
        page.contains(r#"data-live="1""#) && page.contains("EventSource"),
        "{page}"
    );

    let mut s = open_follow(
        addr,
        &format!("/runs/{id}/transcripts/{UUID}?follow=1&from=0"),
    )
    .await;
    let (head, _) = read_until(&mut s, "text/event-stream", Duration::from_secs(2)).await;
    assert!(head.starts_with("HTTP/1.1 200"));

    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&file)
        .unwrap();
    // A torn write is completed later; only the whole line may appear.
    f.write_all(br#"{"type":"assistant","message":{"content":[{"type":"text","text":"live-"#)
        .unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    f.write_all(b"lima\"}]}}\n").unwrap();
    let (frames, took) = read_until(&mut s, "live-lima", Duration::from_secs(2)).await;
    assert!(
        frames.contains("event: line") && frames.contains("id: "),
        "{frames}"
    );
    assert!(took < Duration::from_secs(2));
}

#[tokio::test]
async fn follow_works_for_a_recorded_invocation_and_resumes_from_last_event_id() {
    let e = env();
    let (id, w) = new_run(&e, "/r/a", std::process::id());
    w.append(llm("inv-1", "n")).unwrap();
    let file = transcripts(&e, &id).join("inv-1.jsonl");
    let l1 = "{\"type\":\"turn.started\"}\n";
    std::fs::write(&file, format!("{l1}{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":\"second-mike\"}}}}\n")).unwrap();
    let (addr, _stop) = serve(&e).await;
    wait_body(
        addr,
        &format!("/runs/{id}/summary"),
        "inv-1",
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    let mut s = open_follow(
        addr,
        &format!("/runs/{id}/transcripts/inv-1?follow=1&from={}", l1.len()),
    )
    .await;
    let (frames, _) = read_until(&mut s, "second-mike", Duration::from_secs(2)).await;
    assert!(!frames.contains("turn.started"), "{frames}");
}

// AC 3
#[tokio::test]
async fn unparseable_lines_are_shown_raw_and_the_page_survives() {
    let e = env();
    let (id, w) = new_run(&e, "/r/a", std::process::id());
    w.append(llm("inv-1", "n")).unwrap();
    std::fs::write(
        transcripts(&e, &id).join("inv-1.jsonl"),
        fixture("transcript_bad.jsonl"),
    )
    .unwrap();
    let (addr, _stop) = serve(&e).await;
    wait_body(
        addr,
        &format!("/runs/{id}/summary"),
        "inv-1",
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    let (code, body) = get(addr, &format!("/runs/{id}/transcripts/inv-1")).await;
    assert_eq!(code, 200);
    assert!(body.contains("tx-raw"));
    assert!(body.contains("not json &lt;b&gt;bold&lt;/b&gt;"), "{body}");
    assert!(!body.contains("<b>bold</b>"));
    assert!(
        body.contains(r#"{&quot;type&quot;:&quot;assistant&quot;,&quot;message&quot;:42}"#),
        "{body}"
    );
    positions(&body, &["not json", "after-kilo"]);
}

// AC 4
#[tokio::test]
async fn nonexistent_invocation_is_404() {
    let e = env();
    let (id, w) = new_run(&e, "/r/a", std::process::id());
    w.append(llm("inv-1", "n")).unwrap(); // recorded, but no file
    let (addr, _stop) = serve(&e).await;
    wait_body(
        addr,
        &format!("/runs/{id}/summary"),
        "inv-1",
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    for inv in ["inv-1", "unknown", UUID] {
        for q in ["", "?raw=1", "?follow=1"] {
            let (code, _) = get(addr, &format!("/runs/{id}/transcripts/{inv}{q}")).await;
            assert_eq!(code, 404, "{inv}{q}");
        }
    }
    let (code, _) = get(addr, &format!("/runs/{UUID}/transcripts/inv-1")).await;
    assert_eq!(code, 404);
    let (code, _) = get_with_host(
        addr,
        &format!("/runs/{id}/transcripts/inv-1"),
        "evil.example",
    )
    .await;
    assert_eq!(code, 403);
}

// AC 5
#[tokio::test]
async fn path_traversal_is_404_and_reads_no_outside_file() {
    let e = env();
    let (id, w) = new_run(&e, "/r/a", std::process::id());
    w.append(llm("inv-1", "n")).unwrap();
    let dir = transcripts(&e, &id);
    std::fs::write(dir.join("inv-1.jsonl"), "{}\n").unwrap();
    std::fs::write(dir.join("secret.jsonl"), "secret-marker").unwrap();
    std::fs::write(e.root.join("outside.txt"), "outside-marker").unwrap();
    std::fs::write(e.root.join("outside.jsonl"), "outside-marker").unwrap();
    let (addr, _stop) = serve(&e).await;
    wait_body(
        addr,
        &format!("/runs/{id}/summary"),
        "inv-1",
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    for bad in [
        "..%2F..%2Fetc%2Fpasswd",
        "..%2f..%2foutside",
        "..%2foutside.txt",
        "%2e%2e",
        "..",
        "inv-1.jsonl",
        "secret",
        "..%5Coutside",
        "inv-1%00",
        "%2Fetc%2Fpasswd",
    ] {
        for q in ["", "?raw=1", "?follow=1"] {
            let (code, body) = get(addr, &format!("/runs/{id}/transcripts/{bad}{q}")).await;
            assert_eq!(code, 404, "{bad}{q}");
            assert!(
                !body.contains("marker") && !body.contains("root:"),
                "{bad}{q}"
            );
        }
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
