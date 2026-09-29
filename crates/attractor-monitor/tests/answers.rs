//! Human Gate answer buttons over HTTP with a fake `pas` (attractor-ino.39).

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use attractor_journal::{
    append_entry_at, new_run_id, write_run_meta, AnswerSource, AttemptEndReason, EventData,
    IndexEntry, JournalWriter, RunMeta,
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

const QID: &str = "q-gate-1";

/// A Run folder with a pending gate (`ended` adds an AttemptEnded); returns
/// its id and folder.
fn add_run(e: &Env, ended: Option<AttemptEndReason>, choices: &[&str]) -> (String, PathBuf) {
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
            argv: vec!["pas".into(), "run".into(), "p.dot".into()],
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
    w.append(EventData::HumanInputRequested {
        question_id: QID.into(),
        node_id: "gate".into(),
        text: "Ship it?".into(),
        choices: choices.iter().map(|s| s.to_string()).collect(),
        default: Some(choices[0].into()),
    })
    .unwrap();
    if let Some(reason) = ended {
        w.append(EventData::AttemptEnded {
            attempt: 1,
            reason,
            message: None,
        })
        .unwrap();
    }
    let entry = IndexEntry::new(&id, Utc::now(), &workdir, e.root().join("p.dot"), &dir);
    append_entry_at(&e.root().join("runs.jsonl"), &entry).unwrap();
    (id, dir)
}

fn journal_append(dir: &Path, id: &str, data: EventData) {
    JournalWriter::open(dir, id, 1)
        .unwrap()
        .append(data)
        .unwrap();
}

async fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    form: &str,
) -> (u16, String) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut req = format!(
        "{method} {path} HTTP/1.0\r\nHost: {addr}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n",
        form.len()
    );
    if let Some(t) = token {
        req.push_str(&format!("X-CSRF-Token: {t}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(form);
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

async fn answer(e: &Env, id: &str, qid: &str, form: &str) -> (u16, String) {
    request(
        e.addr,
        "POST",
        &format!("/runs/{id}/answers/{qid}"),
        Some(&e.token),
        form,
    )
    .await
}

async fn get(e: &Env, path: &str) -> (u16, String) {
    request(e.addr, "GET", path, None, "").await
}

/// The Run page once the watcher has folded the journal far enough to show `needle`.
async fn page_with(e: &Env, id: &str, needle: &str) -> String {
    for _ in 0..100 {
        let (code, body) = get(e, &format!("/runs/{id}")).await;
        if code == 200 && body.contains(needle) {
            return body;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("run page never showed {needle}");
}

async fn page(e: &Env, id: &str) -> String {
    page_with(e, id, "data-question").await
}

fn buttons(html: &str) -> usize {
    html.matches("data-choice=").count()
}

#[tokio::test]
async fn pending_gate_shows_question_and_one_button_per_choice() {
    let e = env("exit 0").await;
    let (id, _) = add_run(&e, None, &["approve", "reject", "-odd"]);
    let html = page(&e, &id).await;
    assert!(html.contains("Ship it?"));
    assert_eq!(buttons(&html), 3, "{html}");
    assert!(html.contains(&format!(r#"hx-post="/runs/{id}/answers/{QID}""#)));
}

#[tokio::test]
async fn click_runs_pas_answer_as_monitor() {
    let e = env(r#"echo '{"ok":true}'"#).await;
    let (id, _) = add_run(&e, None, &["approve", "reject"]);
    page(&e, &id).await;
    let (code, body) = answer(&e, &id, QID, "choice=approve").await;
    assert_eq!(code, 200, "{body}");
    let calls = e.calls();
    assert_eq!(calls.lines().count(), 1, "{calls}");
    assert!(
        calls.trim_end().ends_with(&format!(
            "answer --source monitor --json -- {id} {QID} approve"
        )),
        "{calls}"
    );
}

#[tokio::test]
async fn choice_starting_with_a_dash_goes_after_the_separator() {
    let e = env("exit 0").await;
    let (id, _) = add_run(&e, None, &["--force", "no"]);
    page(&e, &id).await;
    let (code, body) = answer(&e, &id, QID, "choice=--force").await;
    assert_eq!(code, 200, "{body}");
    assert!(e.calls().contains(&format!("-- {id} {QID} --force")));
}

#[tokio::test]
async fn unoffered_or_missing_choice_is_refused_without_calling_pas() {
    let e = env("exit 0").await;
    let (id, _) = add_run(&e, None, &["approve", "reject"]);
    page(&e, &id).await;
    let (code, _) = answer(&e, &id, QID, "choice=merge").await;
    assert_eq!(code, 400);
    let (code, _) = answer(&e, &id, QID, "").await;
    assert!((400..500).contains(&code), "{code}");
    assert_eq!(e.calls(), "");
}

#[tokio::test]
async fn already_answered_from_the_terminal_says_so_and_changes_nothing() {
    let e = env(
        r#"echo '{"v":1,"ok":false,"error":{"code":"already_answered","message":"x"}}'; exit 7"#,
    )
    .await;
    let (id, _) = add_run(&e, None, &["approve", "reject"]);
    page(&e, &id).await;
    let (code, body) = answer(&e, &id, QID, "choice=approve").await;
    assert_eq!(code, 409, "{body}");
    assert!(body.contains("already answered"), "{body}");
    // The gate is still pending in the journal and no answer was remembered.
    let (_, summary) = get(&e, &format!("/runs/{id}/summary")).await;
    assert_eq!(buttons(&summary), 2, "{summary}");
}

#[tokio::test]
async fn gate_answered_in_the_journal_is_refused_without_calling_pas() {
    let e = env("exit 0").await;
    let (id, dir) = add_run(&e, None, &["approve", "reject"]);
    page(&e, &id).await;
    journal_append(
        &dir,
        &id,
        EventData::HumanInputAnswered {
            question_id: QID.into(),
            choice: "reject".into(),
            source: AnswerSource::Terminal,
        },
    );
    tokio::time::sleep(Duration::from_millis(600)).await;
    let (code, body) = answer(&e, &id, QID, "choice=approve").await;
    assert_eq!(code, 409, "{body}");
    assert!(body.contains("already answered"), "{body}");
    assert_eq!(e.calls(), "");
}

#[tokio::test]
async fn buttons_are_gone_right_after_a_successful_answer() {
    let e = env("exit 0").await;
    let (id, dir) = add_run(&e, None, &["approve", "reject"]);
    page(&e, &id).await;
    let (code, _) = answer(&e, &id, QID, "choice=approve").await;
    assert_eq!(code, 200);
    // The journal shows no answer yet, still no buttons.
    let (_, summary) = get(&e, &format!("/runs/{id}/summary")).await;
    assert_eq!(buttons(&summary), 0, "{summary}");
    assert!(summary.contains("Answered approve"), "{summary}");
    // A second click is refused without another call.
    let (code, _) = answer(&e, &id, QID, "choice=reject").await;
    assert_eq!(code, 409);
    assert_eq!(e.calls().lines().count(), 1);
    // Once the journal records it the gate block is gone entirely.
    journal_append(
        &dir,
        &id,
        EventData::HumanInputAnswered {
            question_id: QID.into(),
            choice: "approve".into(),
            source: AnswerSource::Monitor,
        },
    );
    tokio::time::sleep(Duration::from_millis(600)).await;
    let (_, summary) = get(&e, &format!("/runs/{id}/summary")).await;
    assert!(!summary.contains("class=\"gate\""), "{summary}");
    assert!(!summary.contains("Answered approve"), "{summary}");
}

#[tokio::test]
async fn pas_failure_is_reported_and_buttons_stay() {
    let e = env(r#"echo '{"ok":false,"error":{"code":"io_error","message":"disk full"}}'; exit 1"#)
        .await;
    let (id, _) = add_run(&e, None, &["approve", "reject"]);
    page(&e, &id).await;
    let (code, body) = answer(&e, &id, QID, "choice=approve").await;
    assert_eq!(code, 502);
    assert!(body.contains("disk full"));
    let (_, summary) = get(&e, &format!("/runs/{id}/summary")).await;
    assert_eq!(buttons(&summary), 2);
}

#[tokio::test]
async fn ended_runs_show_no_buttons_and_refuse_answers() {
    let e = env("exit 0").await;
    for reason in [
        AttemptEndReason::Stopped,
        AttemptEndReason::Completed,
        AttemptEndReason::Failed,
    ] {
        let (id, _) = add_run(&e, Some(reason), &["approve", "reject"]);
        let status = format!("data-status=\"{}\"", format!("{reason:?}").to_lowercase());
        let html = page_with(&e, &id, &status).await;
        assert_eq!(buttons(&html), 0, "{html}");
        let (code, _) = answer(&e, &id, QID, "choice=approve").await;
        assert!(code == 404 || code == 409, "{code}");
    }
    assert_eq!(e.calls(), "");
}

#[tokio::test]
async fn guards_csrf_and_unknown_targets() {
    let e = env("exit 0").await;
    let (id, _) = add_run(&e, None, &["approve", "reject"]);
    page(&e, &id).await;
    let path = format!("/runs/{id}/answers/{QID}");
    let (code, _) = request(e.addr, "POST", &path, None, "choice=approve").await;
    assert_eq!(code, 403);
    let (code, _) = answer(&e, "nope", QID, "choice=approve").await;
    assert_eq!(code, 404);
    let (code, _) = answer(&e, &new_run_id(), QID, "choice=approve").await;
    assert_eq!(code, 404);
    let (code, _) = answer(&e, &id, "other-q", "choice=approve").await;
    assert_eq!(code, 404);
    let (code, _) = answer(&e, &id, "..%2Fx", "choice=approve").await;
    assert_eq!(code, 404);
    assert_eq!(e.calls(), "");
}
