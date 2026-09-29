//! Watcher and SSE tests (attractor-ino.32).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use attractor_journal::{
    append_entry_at, new_run_id, EventData, IndexEntry, JournalWriter, RunDir,
};
use attractor_monitor::projection::{fold, ViewStatus};
use attractor_monitor::state::AppState;
use attractor_monitor::{bind, serve_on_with, watcher};
use chrono::Utc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const POLL: Duration = Duration::from_millis(100);

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

fn started() -> EventData {
    EventData::RunStarted {
        pipeline_name: "p".into(),
        pipeline_path: "p.dot".into(),
        workdir: "/w".into(),
        epic_id: None,
        max_budget_usd: None,
        max_steps: None,
        shared_workdir: false,
    }
}

fn attempt_started() -> EventData {
    EventData::AttemptStarted {
        attempt: 0,
        pid: std::process::id(),
        pas_version: "1".into(),
        argv: vec!["pas".into()],
        git_head: None,
        resumed_from_node: None,
    }
}

fn stage(n: &str) -> EventData {
    EventData::StageStarted {
        node_id: n.into(),
        handler_type: "codergen".into(),
    }
}

/// Create a Run folder and journal writer; optionally register it in the Index.
fn new_run(e: &Env, register: bool) -> (String, PathBuf, JournalWriter) {
    let id = new_run_id();
    let dir = e.root.join(&id);
    std::fs::create_dir_all(&dir).unwrap();
    let w = JournalWriter::open(&dir, &id, 0).unwrap();
    if register {
        register_run(e, &id, &dir);
    }
    (id, dir, w)
}

fn register_run(e: &Env, id: &str, dir: &Path) {
    let entry = IndexEntry::new(id, Utc::now(), "/w", "p.dot", dir);
    append_entry_at(&e.index, &entry).unwrap();
}

async fn wait_for<T>(mut f: impl FnMut() -> Option<T>, within: Duration) -> Option<T> {
    let end = Instant::now() + within;
    while Instant::now() < end {
        if let Some(v) = f() {
            return Some(v);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    None
}

async fn serve(state: AppState) -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let l = bind(0).await.unwrap();
    let addr = l.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(serve_on_with(l, state, async {
        let _ = rx.await;
    }));
    (addr, tx)
}

#[derive(Debug)]
struct Frame {
    event: String,
    id: Option<u64>,
    data: String,
}

struct Client {
    stream: TcpStream,
    buf: String,
    status: u16,
}

impl Client {
    async fn connect(addr: SocketAddr, path: &str, extra: &[&str]) -> Client {
        let mut s = TcpStream::connect(addr).await.unwrap();
        // HTTP/1.0 so the response is not chunk-framed.
        let mut req = format!("GET {path} HTTP/1.0\r\nHost: 127.0.0.1:{}\r\n", addr.port());
        for h in extra {
            req.push_str(h);
            req.push_str("\r\n");
        }
        req.push_str("\r\n");
        s.write_all(req.as_bytes()).await.unwrap();
        let mut c = Client {
            stream: s,
            buf: String::new(),
            status: 0,
        };
        while !c.buf.contains("\r\n\r\n") {
            assert!(c.fill(Duration::from_secs(5)).await, "no response head");
        }
        let (head, rest) = c.buf.split_once("\r\n\r\n").unwrap();
        c.status = head.split(' ').nth(1).unwrap().parse().unwrap();
        c.buf = rest.to_string();
        c
    }

    async fn fill(&mut self, within: Duration) -> bool {
        let mut chunk = [0u8; 4096];
        match tokio::time::timeout(within, self.stream.read(&mut chunk)).await {
            Ok(Ok(n)) if n > 0 => {
                self.buf.push_str(&String::from_utf8_lossy(&chunk[..n]));
                true
            }
            _ => false,
        }
    }

    async fn frame(&mut self, within: Duration) -> Option<Frame> {
        let end = Instant::now() + within;
        loop {
            while let Some(i) = self.buf.find("\n\n") {
                let raw: String = self.buf.drain(..i + 2).collect();
                if raw.trim_start().starts_with(':') {
                    continue; // keep-alive comment
                }
                let mut f = Frame {
                    event: String::new(),
                    id: None,
                    data: String::new(),
                };
                for line in raw.lines() {
                    if let Some(v) = line.strip_prefix("event:") {
                        f.event = v.trim().into();
                    } else if let Some(v) = line.strip_prefix("id:") {
                        f.id = v.trim().parse().ok();
                    } else if let Some(v) = line.strip_prefix("data:") {
                        f.data = v.trim().into();
                    }
                }
                return Some(f);
            }
            let left = end.checked_duration_since(Instant::now())?;
            if !self.fill(left).await {
                return None;
            }
        }
    }

    /// Next `journal` frame's seq, skipping `projection` frames.
    async fn next_journal(&mut self, within: Duration) -> Option<u64> {
        let end = Instant::now() + within;
        loop {
            let left = end.checked_duration_since(Instant::now())?;
            let f = self.frame(left).await?;
            if f.event == "journal" {
                return f.id;
            }
        }
    }
}

// --- AC 1 -------------------------------------------------------------

#[tokio::test]
async fn new_run_appears_within_5s() {
    let e = env();
    let state = AppState::new(&e.index);
    let w = watcher::spawn(state.clone(), e.index.clone(), POLL);
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(state.list().is_empty());

    let t0 = Instant::now();
    let (id, _dir, jw) = new_run(&e, true);
    jw.append(started()).unwrap();
    let snap = wait_for(
        || {
            state
                .snapshot(&id)
                .filter(|s| s.view.status == ViewStatus::Running)
        },
        Duration::from_secs(5),
    )
    .await
    .expect("run visible");
    assert!(t0.elapsed() < Duration::from_secs(5));
    assert_eq!(snap.view.pipeline_name.as_deref(), Some("p"));
    w.abort();
}

#[tokio::test]
async fn missing_run_dir_is_kept_as_missing() {
    let e = env();
    let id = new_run_id();
    register_run(&e, &id, &e.root.join("gone"));
    let state = AppState::new(&e.index);
    let w = watcher::spawn(state.clone(), e.index.clone(), POLL);
    let snap = wait_for(|| state.snapshot(&id), Duration::from_secs(2))
        .await
        .expect("listed");
    assert!(snap.missing);
    assert_eq!(snap.view.last_seq, 0);
    w.abort();
}

#[tokio::test]
async fn ended_run_resumed_is_refreshed() {
    let e = env();
    let (id, dir, jw) = new_run(&e, true);
    jw.append(started()).unwrap();
    jw.append(attempt_started()).unwrap();
    jw.append(EventData::AttemptEnded {
        attempt: 0,
        reason: attempt_journal_reason(),
        message: None,
    })
    .unwrap();
    let state = AppState::new(&e.index);
    let w = watcher::spawn(state.clone(), e.index.clone(), POLL);
    wait_for(
        || {
            state
                .snapshot(&id)
                .filter(|s| s.view.status == ViewStatus::Stopped)
        },
        Duration::from_secs(3),
    )
    .await
    .expect("ended run loaded");

    let jw2 = JournalWriter::open(&dir, &id, 1).unwrap();
    jw2.append(EventData::AttemptStarted {
        attempt: 1,
        pid: std::process::id(),
        pas_version: "1".into(),
        argv: vec![],
        git_head: None,
        resumed_from_node: None,
    })
    .unwrap();
    wait_for(
        || {
            state
                .snapshot(&id)
                .filter(|s| s.view.attempts.len() == 2 && s.view.status == ViewStatus::Running)
        },
        Duration::from_secs(3),
    )
    .await
    .expect("resume picked up");
    w.abort();
}

fn attempt_journal_reason() -> attractor_journal::AttemptEndReason {
    attractor_journal::AttemptEndReason::Stopped
}

// --- AC 2 -------------------------------------------------------------

#[tokio::test]
async fn sse_delivers_new_event_within_2s() {
    let e = env();
    let (id, _dir, jw) = new_run(&e, true);
    jw.append(started()).unwrap();
    let state = AppState::new(&e.index);
    let w = watcher::spawn(state.clone(), e.index.clone(), POLL);
    let (addr, _stop) = serve(state.clone()).await;
    wait_for(|| state.snapshot(&id), Duration::from_secs(3)).await;

    let mut c = Client::connect(addr, &format!("/runs/{id}/events"), &[]).await;
    assert_eq!(c.status, 200);
    assert_eq!(c.next_journal(Duration::from_secs(2)).await, Some(1));

    for i in 0..3u64 {
        let t0 = Instant::now();
        let ev = jw.append(stage(&format!("n{i}"))).unwrap();
        let got = c.next_journal(Duration::from_secs(2)).await;
        assert_eq!(got, Some(ev.seq), "event {i} not delivered");
        assert!(t0.elapsed() < Duration::from_secs(2));
    }
    // A projection frame follows the journal frame.
    let f = c.frame(Duration::from_secs(2)).await.unwrap();
    assert_eq!(f.event, "projection");
    assert!(f.id.is_none());
    assert!(f.data.contains("\"last_seq\":4"), "{}", f.data);
    w.abort();
}

// --- AC 3 -------------------------------------------------------------

#[tokio::test]
async fn restart_rebuilds_identical_view() {
    let e = env();
    let (id, dir, jw) = new_run(&e, true);
    jw.append(started()).unwrap();
    jw.append(attempt_started()).unwrap();
    jw.append(stage("a")).unwrap();

    let a = AppState::new(&e.index);
    let wa = watcher::spawn(a.clone(), e.index.clone(), POLL);
    wait_for(
        || a.snapshot(&id).filter(|s| s.view.last_seq == 3),
        Duration::from_secs(3),
    )
    .await
    .unwrap();
    jw.append(stage("b")).unwrap();
    jw.append(stage("c")).unwrap();
    let before = wait_for(
        || a.snapshot(&id).filter(|s| s.view.last_seq == 5),
        Duration::from_secs(3),
    )
    .await
    .unwrap();
    wa.abort();

    let b = AppState::new(&e.index);
    let wb = watcher::spawn(b.clone(), e.index.clone(), POLL);
    let after = wait_for(
        || b.snapshot(&id).filter(|s| s.view.last_seq == 5),
        Duration::from_secs(3),
    )
    .await
    .unwrap();
    assert_eq!(before.view, after.view);

    // ... and equals prefix + new events folded incrementally.
    let all = attractor_journal::read_all(RunDir::from_path(&dir).events()).unwrap();
    let mut prefix = fold(&all[..3]);
    for ev in &all[3..] {
        prefix.apply(ev);
    }
    assert_eq!(prefix, after.view);
    wb.abort();
}

// --- AC 4 -------------------------------------------------------------

#[tokio::test]
async fn reconnect_gets_only_higher_seq() {
    let e = env();
    let (id, _dir, jw) = new_run(&e, true);
    jw.append(started()).unwrap();
    jw.append(attempt_started()).unwrap();
    let state = AppState::new(&e.index);
    let w = watcher::spawn(state.clone(), e.index.clone(), POLL);
    let (addr, _stop) = serve(state.clone()).await;
    wait_for(|| state.snapshot(&id), Duration::from_secs(3)).await;

    let path = format!("/runs/{id}/events");
    let mut c = Client::connect(addr, &path, &[]).await;
    assert_eq!(c.next_journal(Duration::from_secs(2)).await, Some(1));
    assert_eq!(c.next_journal(Duration::from_secs(2)).await, Some(2));
    drop(c);

    jw.append(stage("a")).unwrap();
    jw.append(stage("b")).unwrap();

    let mut c = Client::connect(addr, &path, &["Last-Event-ID: 2"]).await;
    assert_eq!(c.next_journal(Duration::from_secs(2)).await, Some(3));
    assert_eq!(c.next_journal(Duration::from_secs(2)).await, Some(4));
    jw.append(stage("c")).unwrap();
    assert_eq!(c.next_journal(Duration::from_secs(2)).await, Some(5));
    w.abort();
}

#[tokio::test]
async fn garbage_last_event_id_replays_all() {
    let e = env();
    let (id, _dir, jw) = new_run(&e, true);
    jw.append(started()).unwrap();
    jw.append(attempt_started()).unwrap();
    let state = AppState::new(&e.index);
    let (addr, _stop) = serve(state.clone()).await; // no watcher: on-miss rescan finds it
    let mut c = Client::connect(
        addr,
        &format!("/runs/{id}/events"),
        &["Last-Event-ID: banana"],
    )
    .await;
    assert_eq!(c.status, 200);
    assert_eq!(c.next_journal(Duration::from_secs(2)).await, Some(1));
    assert_eq!(c.next_journal(Duration::from_secs(2)).await, Some(2));
}

#[tokio::test]
async fn lagged_receiver_recovers_from_file() {
    let e = env();
    let (id, _dir, jw) = new_run(&e, true);
    jw.append(started()).unwrap();
    let state = AppState::with_capacity(&e.index, 2);
    let w = watcher::spawn(state.clone(), e.index.clone(), POLL);
    let (addr, _stop) = serve(state.clone()).await;
    wait_for(|| state.snapshot(&id), Duration::from_secs(3)).await;

    let mut c = Client::connect(addr, &format!("/runs/{id}/events"), &[]).await;
    assert_eq!(c.next_journal(Duration::from_secs(2)).await, Some(1));
    // Burst well beyond the broadcast capacity without reading.
    for i in 0..40 {
        jw.append(stage(&format!("n{i}"))).unwrap();
    }
    for want in 2..=41u64 {
        assert_eq!(
            c.next_journal(Duration::from_secs(3)).await,
            Some(want),
            "gap or duplicate"
        );
    }
    w.abort();
}

// --- AC 5 -------------------------------------------------------------

#[tokio::test]
async fn unknown_run_events_is_404() {
    let e = env();
    let state = AppState::new(&e.index);
    let (addr, _stop) = serve(state).await;
    for path in [
        format!("/runs/{}/events", new_run_id()),
        "/runs/not-a-run/events".to_string(),
        "/runs/..%2F..%2Fetc/events".to_string(),
    ] {
        let c = Client::connect(addr, &path, &[]).await;
        assert_eq!(c.status, 404, "{path}");
    }
}

#[tokio::test]
async fn run_with_missing_folder_streams_empty() {
    let e = env();
    let id = new_run_id();
    register_run(&e, &id, &e.root.join("gone"));
    let state = AppState::new(&e.index);
    let (addr, _stop) = serve(state).await;
    // Known to the Index but its folder is gone: the stream is empty, not an error.
    let c = Client::connect(addr, &format!("/runs/{id}/events"), &[]).await;
    assert_eq!(c.status, 200);
}

#[tokio::test]
async fn foreign_host_is_still_403() {
    let e = env();
    let state = AppState::new(&e.index);
    let (addr, _stop) = serve(state).await;
    let mut s = TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "GET /runs/{}/events HTTP/1.0\r\nHost: evil.example\r\n\r\n",
        new_run_id()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    assert!(
        out.starts_with("HTTP/1.0 403") || out.starts_with("HTTP/1.1 403"),
        "{out}"
    );
}
