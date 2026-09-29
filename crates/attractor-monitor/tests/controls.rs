//! Run controls over HTTP with a fake `pas` (attractor-ino.38).

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use attractor_journal::{
    append_entry_at, new_run_id, write_run_meta, AttemptEndReason, EventData, IndexEntry,
    JournalWriter, RunMeta,
};
use attractor_monitor::state::AppState;
use attractor_monitor::{bind, serve_on_with, watcher};
use chrono::Utc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

struct Env {
    tmp: tempfile::TempDir,
    addr: SocketAddr,
    token: String,
    _stop: tokio::sync::oneshot::Sender<()>,
}

impl Env {
    fn root(&self) -> &Path {
        self.tmp.path()
    }
    fn log(&self) -> PathBuf {
        self.root().join("calls.log")
    }
    fn calls(&self) -> String {
        std::fs::read_to_string(self.log()).unwrap_or_default()
    }
}

/// `body` is the shell run after the call is recorded (one line: cwd, args).
async fn env(body: &str) -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let fake = tmp.path().join("fake-pas");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\necho \"$(pwd) $*\" >> '{}'\n{body}\n",
            tmp.path().join("calls.log").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let index = tmp.path().join("runs.jsonl");
    let state = AppState::new(&index);
    state.set_pas_exe(&fake);
    let token = state.csrf_token().as_str().to_string();
    watcher::spawn(state.clone(), index, Duration::from_millis(100));
    let l = bind(0).await.unwrap();
    let addr = l.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(serve_on_with(l, state, async {
        let _ = rx.await;
    }));
    Env {
        tmp,
        addr,
        token,
        _stop: tx,
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Running,
    Stopped,
    Completed,
    Failed,
}

/// A Run folder under `<root>/logs/runs/<id>`; returns its id.
fn add_run(e: &Env, kind: Kind, argv: &[&str]) -> String {
    let id = new_run_id();
    let logs = e.root().join("logs");
    let dir = logs.join("runs").join(&id);
    std::fs::create_dir_all(&dir).unwrap();
    let workdir = e.root().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    write_run_meta(
        &dir,
        &RunMeta {
            v: RunMeta::VERSION,
            run_id: id.clone(),
            pipeline_path: e.root().join("p.dot"),
            pipeline_name: "pipe".into(),
            workdir: workdir.clone(),
            git_worktree: None,
            logs_dir: logs.clone(),
            started_at: Utc::now(),
            argv: argv.iter().map(|s| s.to_string()).collect(),
            pas_version: "1".into(),
            epic_id: None,
        },
    )
    .unwrap();
    let w = JournalWriter::open(&dir, &id, 0).unwrap();
    w.append(EventData::RunStarted {
        pipeline_name: "pipe".into(),
        pipeline_path: "p.dot".into(),
        workdir: workdir.to_string_lossy().into(),
        epic_id: None,
        max_budget_usd: None,
        max_steps: None,
        shared_workdir: false,
    })
    .unwrap();
    w.append(EventData::AttemptStarted {
        attempt: 0,
        pid: std::process::id(),
        pas_version: "1".into(),
        argv: vec!["pas".into()],
        git_head: None,
        resumed_from_node: None,
    })
    .unwrap();
    let end = match kind {
        Kind::Running => None,
        Kind::Stopped => Some(AttemptEndReason::Stopped),
        Kind::Completed => Some(AttemptEndReason::Completed),
        Kind::Failed => Some(AttemptEndReason::Failed),
    };
    if let Some(reason) = end {
        w.append(EventData::AttemptEnded {
            attempt: 1,
            reason,
            message: None,
        })
        .unwrap();
    }
    let entry = IndexEntry::new(&id, Utc::now(), &workdir, e.root().join("p.dot"), &dir);
    append_entry_at(&e.root().join("runs.jsonl"), &entry).unwrap();
    id
}

async fn request(addr: SocketAddr, method: &str, path: &str, token: Option<&str>) -> (u16, String) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut req = format!("{method} {path} HTTP/1.0\r\nHost: {addr}\r\nContent-Length: 0\r\n");
    if let Some(t) = token {
        req.push_str(&format!("X-CSRF-Token: {t}\r\n"));
    }
    req.push_str("\r\n");
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

async fn post(e: &Env, id: &str, action: &str) -> (u16, String) {
    request(
        e.addr,
        "POST",
        &format!("/runs/{id}/{action}"),
        Some(&e.token),
    )
    .await
}

async fn page(e: &Env, id: &str) -> String {
    for _ in 0..100 {
        let (code, body) = request(e.addr, "GET", &format!("/runs/{id}"), None).await;
        if code == 200 {
            return body;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("run page never loaded");
}

fn has(html: &str, action: &str) -> bool {
    html.contains(&format!(r#"data-action="{action}""#))
}

const RUN_ARGV: &[&str] = &["pas", "run", "rel.dot", "--max-steps", "9", "--json"];

#[tokio::test]
async fn stop_calls_pas_stop_as_monitor() {
    let e = env("exit 0").await;
    let id = add_run(&e, Kind::Running, RUN_ARGV);
    page(&e, &id).await;
    let (code, body) = post(&e, &id, "stop").await;
    assert_eq!(code, 200, "{body}");
    assert!(e
        .calls()
        .contains(&format!("stop {id} --source monitor --json")));
}

#[tokio::test]
async fn stop_error_message_from_json_is_shown() {
    let e = env(r#"echo '{"v":1,"ok":false,"error":{"code":"not_active","message":"run is not active"}}'; exit 1"#).await;
    let id = add_run(&e, Kind::Running, RUN_ARGV);
    page(&e, &id).await;
    let (code, body) = post(&e, &id, "stop").await;
    assert_eq!(code, 502);
    assert!(body.contains("run is not active"), "{body}");
}

#[tokio::test]
async fn kill_calls_pas_kill_with_short_grace_and_shows_failures() {
    let e = env("exit 0").await;
    let id = add_run(&e, Kind::Running, RUN_ARGV);
    page(&e, &id).await;
    assert_eq!(post(&e, &id, "kill").await.0, 200);
    assert!(e.calls().contains(&format!("kill {id} --grace 3s --json")));

    let e = env(r#"echo '{"error":{"code":"pid_not_lock_holder","message":"pid 7 does not hold the lock"}}'; exit 1"#).await;
    let id = add_run(&e, Kind::Running, RUN_ARGV);
    page(&e, &id).await;
    let (code, body) = post(&e, &id, "kill").await;
    assert_eq!(code, 502);
    assert!(body.contains("pid 7 does not hold the lock"), "{body}");
}

#[tokio::test]
async fn resume_reissues_argv_with_same_run_id_no_fresh_in_workdir() {
    let e = env("exit 0").await;
    let id = add_run(&e, Kind::Stopped, RUN_ARGV);
    page(&e, &id).await;
    let (code, body) = post(&e, &id, "resume").await;
    assert_eq!(code, 200, "{body}");
    let calls = e.calls();
    let logs = e.root().join("logs");
    let want = format!(
        "run {} --max-steps 9 --run-id {id} --logs {}",
        e.root().join("p.dot").display(),
        logs.display()
    );
    assert!(calls.contains(&want), "{calls}");
    assert!(!calls.contains("--fresh"), "{calls}");
    let workdir = std::fs::canonicalize(e.root().join("work")).unwrap();
    assert!(calls.starts_with(&workdir.display().to_string()), "{calls}");
}

#[tokio::test]
async fn resume_output_goes_to_the_same_runs_console_log() {
    let e = env("echo hello-from-child").await;
    let id = add_run(&e, Kind::Stopped, RUN_ARGV);
    page(&e, &id).await;
    assert_eq!(post(&e, &id, "resume").await.0, 200);
    let log = e.root().join("logs/runs").join(&id).join("console.log");
    for _ in 0..50 {
        if std::fs::read_to_string(&log).is_ok_and(|s| s.contains("hello-from-child")) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("console.log not written");
}

#[tokio::test]
async fn run_again_uses_fresh_and_a_new_run_id() {
    let e = env("exit 0").await;
    let id = add_run(&e, Kind::Completed, RUN_ARGV);
    page(&e, &id).await;
    let (code, body) = post(&e, &id, "rerun").await;
    assert_eq!(code, 200, "{body}");
    let calls = e.calls();
    assert!(calls.contains(" --fresh --run-id "), "{calls}");
    let new_id = calls
        .split("--run-id ")
        .nth(1)
        .unwrap()
        .split(' ')
        .next()
        .unwrap();
    assert_ne!(new_id, id);
    assert!(attractor_journal::parse_run_id(new_id).is_some());
    assert!(body.contains(&format!("/runs/{new_id}")), "{body}");
    assert!(e
        .root()
        .join("logs/runs")
        .join(new_id)
        .join("console.log")
        .exists());
}

#[tokio::test]
async fn buttons_follow_status() {
    let e = env("exit 0").await;
    let cases = [
        (Kind::Running, ["stop", "kill"].as_slice()),
        (Kind::Completed, ["rerun"].as_slice()),
        (Kind::Stopped, ["resume", "rerun"].as_slice()),
        (Kind::Failed, ["resume", "rerun"].as_slice()),
    ];
    for (kind, shown) in cases {
        let id = add_run(&e, kind, RUN_ARGV);
        let html = page(&e, &id).await;
        for action in ["stop", "kill", "resume", "rerun"] {
            assert_eq!(has(&html, action), shown.contains(&action), "{action}");
        }
        // The 2 s refresh carries the same buttons.
        let (_, summary) = request(e.addr, "GET", &format!("/runs/{id}/summary"), None).await;
        for action in ["stop", "kill", "resume", "rerun"] {
            assert_eq!(
                has(&summary, action),
                shown.contains(&action),
                "summary {action}"
            );
        }
    }
}

#[tokio::test]
async fn page_carries_the_csrf_token_for_buttons() {
    let e = env("exit 0").await;
    let id = add_run(&e, Kind::Running, RUN_ARGV);
    let html = page(&e, &id).await;
    assert!(html.contains(&e.token), "token missing from the page");
}

#[tokio::test]
async fn actions_in_invalid_states_are_409_and_run_nothing() {
    let e = env("exit 0").await;
    let running = add_run(&e, Kind::Running, RUN_ARGV);
    let done = add_run(&e, Kind::Completed, RUN_ARGV);
    page(&e, &running).await;
    page(&e, &done).await;
    for (id, action) in [
        (&done, "stop"),
        (&done, "kill"),
        (&done, "resume"),
        (&running, "resume"),
        (&running, "rerun"),
    ] {
        assert_eq!(post(&e, id, action).await.0, 409, "{action}");
    }
    assert_eq!(e.calls(), "");
}

#[tokio::test]
async fn unknown_and_invalid_ids_are_404() {
    let e = env("exit 0").await;
    for id in [new_run_id(), "..".to_string(), "not-a-uuid".to_string()] {
        for action in ["stop", "kill", "resume", "rerun"] {
            let (code, _) = post(&e, &id, action).await;
            assert_eq!(code, 404, "{id} {action}");
        }
    }
    assert_eq!(e.calls(), "");
}

#[tokio::test]
async fn resume_lock_error_shows_pid_and_run_id() {
    let e =
        env("echo 'error: pipeline already running (pid 4242, run RID-1234)' >&2; exit 5").await;
    let id = add_run(&e, Kind::Stopped, RUN_ARGV);
    page(&e, &id).await;
    let (code, body) = post(&e, &id, "resume").await;
    assert_eq!(code, 502);
    assert!(body.contains("4242") && body.contains("RID-1234"), "{body}");
}

#[tokio::test]
async fn slow_child_is_reported_started_and_quick_success_is_not_an_error() {
    let e = env("sleep 5").await;
    let id = add_run(&e, Kind::Stopped, RUN_ARGV);
    page(&e, &id).await;
    assert_eq!(post(&e, &id, "resume").await.0, 200);
    let e = env("exit 0").await;
    let id = add_run(&e, Kind::Stopped, RUN_ARGV);
    page(&e, &id).await;
    assert_eq!(post(&e, &id, "resume").await.0, 200);
}

#[tokio::test]
async fn runs_not_started_by_pas_run_cannot_be_reissued() {
    let e = env("exit 0").await;
    let id = add_run(&e, Kind::Stopped, &["pas", "launch", "x"]);
    page(&e, &id).await;
    assert_eq!(post(&e, &id, "resume").await.0, 409);
    assert_eq!(post(&e, &id, "rerun").await.0, 409);
    assert_eq!(e.calls(), "");
}

#[tokio::test]
async fn all_four_routes_are_403_without_the_token_and_run_nothing() {
    let e = env("exit 0").await;
    let id = add_run(&e, Kind::Running, RUN_ARGV);
    page(&e, &id).await;
    for action in ["stop", "kill", "resume", "rerun"] {
        for tok in [None, Some("wrong")] {
            let (code, _) = request(e.addr, "POST", &format!("/runs/{id}/{action}"), tok).await;
            assert_eq!(code, 403, "{action}");
        }
    }
    assert_eq!(e.calls(), "");
}
