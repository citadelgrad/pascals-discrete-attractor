//! Run controls: Stop, Kill, Resume, Run again (PRD FR-9). Every action goes
//! through the `pas` binary; the Monitor never writes a journal, a checkpoint
//! or `control/stop` itself (ADR 0001, ADR 0002).

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use attractor_journal::{
    new_run_id, parse_run_id, read_run_meta, PipelineDir, RunDir, RunMeta, RunStatus,
};
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::Form;
use chrono::Utc;
use maud::{html, Markup};

use crate::spawn;
use crate::state::AppState;
use crate::views::run::current_status;

/// How long a freshly started `pas run` has to fail (lock held, bad argv)
/// before the Monitor reports it as started.
const STARTUP_WINDOW: Duration = Duration::from_secs(2);
/// Bytes of `console.log` shown when a start fails.
const TAIL_BYTES: u64 = 4096;
/// Kill escalates to SIGKILL after this long, inside the 5 s the Run has to end.
const KILL_GRACE: &str = "3s";

/// Which actions a Run in a given status offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Allowed {
    pub stop: bool,
    pub kill: bool,
    pub resume: bool,
    pub rerun: bool,
}

pub fn allowed(status: RunStatus) -> Allowed {
    use RunStatus::*;
    Allowed {
        stop: status == Running,
        kill: status == Running,
        resume: matches!(status, Stopped | Failed | Crashed),
        rerun: matches!(status, Completed | Stopped | Failed | Crashed),
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Reissue<'a> {
    /// Same Run, no `--fresh`: continues from the checkpoint as a new Attempt.
    Resume,
    /// A new Run with `--fresh` and the given id.
    RunAgain { new_run_id: &'a str },
}

#[derive(Debug, PartialEq, Eq)]
pub enum ControlError {
    /// The Run was not started by `pas run`, so its argv cannot be replayed.
    NotARunCommand,
}

impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotARunCommand => f.write_str(
                "this Run was not started with `pas run`, so it cannot be re-issued from the Monitor",
            ),
        }
    }
}

/// Flags of `pas run` that take no value.
const BOOL_FLAGS: &[&str] = &["--dry-run", "--fresh", "--json", "--allow-shared-workdir"];
/// Flags the Monitor sets itself.
const OWNED_FLAGS: &[&str] = &["--fresh", "--run-id", "--logs", "-l", "--json"];

fn flag_name(tok: &str) -> &str {
    tok.split_once('=').map_or(tok, |(k, _)| k)
}

/// The args for `pas` that replay `meta.argv`: `argv[0]` is dropped, the
/// Pipeline path is the absolute one from `run.json`, and `--run-id`/`--logs`
/// are set explicitly (`--fresh` only for Run again).
pub fn reissue_args(meta: &RunMeta, mode: Reissue) -> Result<Vec<OsString>, ControlError> {
    if meta.argv.get(1).map(String::as_str) != Some("run") {
        return Err(ControlError::NotARunCommand);
    }
    let mut out: Vec<OsString> = vec!["run".into(), meta.pipeline_path.clone().into_os_string()];
    let mut positional_seen = false;
    let mut it = meta.argv[2..].iter();
    while let Some(tok) = it.next() {
        if !tok.starts_with('-') || tok == "-" {
            if positional_seen {
                out.push(tok.into());
            }
            positional_seen = true;
            continue;
        }
        let name = flag_name(tok);
        let owned =
            OWNED_FLAGS.contains(&name) || (name.starts_with("-l") && !name.starts_with("--"));
        let takes_value = !tok.contains('=')
            && !BOOL_FLAGS.contains(&name)
            && !(tok.starts_with("-l") && tok.len() > 2 && !tok.starts_with("--"));
        let value = if takes_value { it.next() } else { None };
        if owned {
            continue;
        }
        out.push(tok.into());
        if let Some(v) = value {
            out.push(v.into());
        }
    }
    let run_id = match mode {
        Reissue::Resume => meta.run_id.as_str(),
        Reissue::RunAgain { new_run_id } => {
            out.push("--fresh".into());
            new_run_id
        }
    };
    out.push("--run-id".into());
    out.push(run_id.into());
    out.push("--logs".into());
    out.push(meta.logs_dir.clone().into_os_string());
    Ok(out)
}

fn notice(class: &str, text: &str) -> Markup {
    html! { p class=(class) role="status" { (text) } }
}

fn reply(code: StatusCode, class: &str, text: &str) -> Response {
    (code, Html(notice(class, text).into_string())).into_response()
}

fn conflict(text: &str) -> Response {
    reply(StatusCode::CONFLICT, "notice error", text)
}

fn failed(text: &str) -> Response {
    reply(StatusCode::BAD_GATEWAY, "notice error", text)
}

/// The message of a failed `pas ... --json` (C6), else stderr.
fn failure_message(out: &spawn::PasOutput) -> String {
    let from_json = serde_json::from_str::<serde_json::Value>(out.stdout.trim())
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(String::from));
    from_json
        .or_else(|| Some(out.stderr.trim().to_string()).filter(|s| !s.is_empty()))
        .unwrap_or_else(|| format!("pas exited with code {}", out.code.unwrap_or(-1)))
}

fn tail(path: &Path) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return String::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = f.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)));
    let mut buf = Vec::new();
    let _ = f.take(TAIL_BYTES).read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).trim().to_string()
}

/// A Run the action may target: known to the Index and in a status that allows it.
struct Target {
    id: String,
    run_dir: RunDir,
}

#[allow(clippy::result_large_err)] // the Err is the HTTP reply itself
fn target(
    state: &AppState,
    raw: &str,
    what: &str,
    ok: impl Fn(Allowed) -> bool,
) -> Result<Target, Response> {
    let snap = parse_run_id(raw)
        .and_then(|id| state.snapshot(&id))
        .ok_or_else(|| StatusCode::NOT_FOUND.into_response())?;
    let status = current_status(&snap, Utc::now());
    if !ok(allowed(status)) {
        return Err(conflict(&format!(
            "Cannot {what}: the Run is {}.",
            status.as_str()
        )));
    }
    Ok(Target {
        id: snap.entry.run_id.clone(),
        run_dir: RunDir::from_path(&snap.entry.run_dir),
    })
}

async fn run_pas_command(state: &AppState, args: Vec<OsString>, done: &str) -> Response {
    let exe = match state.pas_exe() {
        Ok(e) => e,
        Err(e) => return failed(&format!("Cannot find the pas executable: {e}")),
    };
    match spawn::run_at(&exe, &args).await {
        Ok(out) if out.code == Some(0) => reply(StatusCode::OK, "notice", done),
        Ok(out) => failed(&failure_message(&out)),
        Err(e) => failed(&format!("Cannot run pas: {e}")),
    }
}

pub async fn stop(State(state): State<AppState>, UrlPath(id): UrlPath<String>) -> Response {
    let t = match target(&state, &id, "stop", |a| a.stop) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let args = ["stop", &t.id, "--source", "monitor", "--json"].map(OsString::from);
    run_pas_command(
        &state,
        args.to_vec(),
        "Stop requested: the Run ends after the current stage.",
    )
    .await
}

pub async fn kill(State(state): State<AppState>, UrlPath(id): UrlPath<String>) -> Response {
    let t = match target(&state, &id, "kill", |a| a.kill) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let args = ["kill", &t.id, "--grace", KILL_GRACE, "--json"].map(OsString::from);
    run_pas_command(&state, args.to_vec(), "Killed.").await
}

pub async fn resume(State(state): State<AppState>, UrlPath(id): UrlPath<String>) -> Response {
    let t = match target(&state, &id, "resume", |a| a.resume) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let meta = match read_run_meta(t.run_dir.path()) {
        Ok(m) => m,
        Err(e) => return failed(&format!("Cannot read run.json: {e}")),
    };
    let args = match reissue_args(&meta, Reissue::Resume) {
        Ok(a) => a,
        Err(e) => return conflict(&e.to_string()),
    };
    start(&state, &meta, args, &t.run_dir, "Resumed as a new Attempt.").await
}

pub async fn rerun(State(state): State<AppState>, UrlPath(id): UrlPath<String>) -> Response {
    let t = match target(&state, &id, "run again", |a| a.rerun) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let meta = match read_run_meta(t.run_dir.path()) {
        Ok(m) => m,
        Err(e) => return failed(&format!("Cannot read run.json: {e}")),
    };
    let new_id = new_run_id();
    let args = match reissue_args(
        &meta,
        Reissue::RunAgain {
            new_run_id: &new_id,
        },
    ) {
        Ok(a) => a,
        Err(e) => return conflict(&e.to_string()),
    };
    let new_dir = match PipelineDir::new(&meta.logs_dir).run(&new_id) {
        Ok(d) => d,
        Err(e) => return failed(&format!("Cannot place the new Run: {e}")),
    };
    let done = format!("Started a new Run {new_id}.");
    let mut resp = start(&state, &meta, args, &new_dir, &done).await;
    if resp.status().is_success() {
        // Point at the new Run; it shows up once the watcher reads the Index.
        let body = html! {
            p.notice role="status" {
                "Started a new Run: " a href=(format!("/runs/{new_id}")) { (new_id) }
            }
        };
        resp = Html(body.into_string()).into_response();
    }
    resp
}

/// Start `pas run` detached, with its output in `run_dir`'s `console.log`. A
/// child that exits non-zero inside [`STARTUP_WINDOW`] (lock held, bad argv)
/// is reported with the tail of that log.
async fn start(
    state: &AppState,
    meta: &RunMeta,
    args: Vec<OsString>,
    run_dir: &RunDir,
    done: &str,
) -> Response {
    let exe = match state.pas_exe() {
        Ok(e) => e,
        Err(e) => return failed(&format!("Cannot find the pas executable: {e}")),
    };
    let log = run_dir.console_log();
    let before = std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0);
    let (_pid, exited) = match spawn::spawn_detached_watch(&exe, &args, &log, Some(&meta.workdir)) {
        Ok(v) => v,
        Err(e) => return failed(&format!("Cannot start pas: {e}")),
    };
    match tokio::time::timeout(STARTUP_WINDOW, exited).await {
        Ok(Ok(Some(0))) | Err(_) | Ok(Err(_)) => reply(StatusCode::OK, "notice", done),
        Ok(Ok(code)) => {
            let text = tail(&log);
            let text = if std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0) > before {
                text
            } else {
                String::new()
            };
            let code = code.map_or("a signal".to_string(), |c| format!("code {c}"));
            failed(&if text.is_empty() {
                format!("pas exited with {code} at start.")
            } else {
                format!("pas exited with {code} at start: {text}")
            })
        }
    }
}

#[derive(serde::Deserialize)]
pub struct AnswerForm {
    choice: String,
}

/// Exit code of `pas answer` when the gate already has an answer (C6).
const EXIT_ALREADY_ANSWERED: i32 = 7;

/// Answer the pending Human Gate `qid` through `pas answer --source monitor`.
pub async fn answer(
    State(state): State<AppState>,
    UrlPath((id, qid)): UrlPath<(String, String)>,
    Form(form): Form<AnswerForm>,
) -> Response {
    let Some(snap) = parse_run_id(&id).and_then(|id| state.snapshot(&id)) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !attractor_journal::is_valid_question_id(&qid) {
        return StatusCode::NOT_FOUND.into_response();
    }
    if let Some(a) = snap.view.answered.iter().find(|a| a.question_id == qid) {
        return already_answered(Some(&format!("{:?}", a.source).to_lowercase()));
    }
    let Some(gate) = snap.view.gate.as_ref().filter(|g| g.question_id == qid) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let status = current_status(&snap, Utc::now());
    if status != RunStatus::Running {
        return conflict(&format!("Cannot answer: the Run is {}.", status.as_str()));
    }
    if snap.answers_sent.contains_key(&qid) {
        return already_answered(Some("monitor"));
    }
    if !gate.choices.contains(&form.choice) {
        return reply(
            StatusCode::BAD_REQUEST,
            "notice error",
            "That choice is not offered by this gate.",
        );
    }
    let exe = match state.pas_exe() {
        Ok(e) => e,
        Err(e) => return failed(&format!("Cannot find the pas executable: {e}")),
    };
    let run_id = snap.entry.run_id.clone();
    let mut args: Vec<OsString> = ["answer", "--source", "monitor", "--json", "--"]
        .map(OsString::from)
        .to_vec();
    args.extend([&run_id, &qid, &form.choice].map(OsString::from));
    match spawn::run_at(&exe, &args).await {
        Ok(out) if out.code == Some(0) => {
            state.mark_answer_sent(&run_id, &qid, &form.choice);
            reply(
                StatusCode::OK,
                "notice",
                &format!(
                    "Answered {}. Waiting for the Run to record it.",
                    form.choice
                ),
            )
        }
        Ok(out) if out.code == Some(EXIT_ALREADY_ANSWERED) => already_answered(None),
        Ok(out) => failed(&failure_message(&out)),
        Err(e) => failed(&format!("Cannot run pas: {e}")),
    }
}

fn already_answered(by: Option<&str>) -> Response {
    let by = by.map(|b| format!(" (by {b})")).unwrap_or_default();
    conflict(&format!(
        "This gate is already answered{by}; the Run is unchanged."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn meta(argv: &[&str]) -> RunMeta {
        RunMeta {
            v: RunMeta::VERSION,
            run_id: "0192aaaa-0000-7000-8000-000000000001".into(),
            pipeline_path: "/abs/p.dot".into(),
            pipeline_name: "p".into(),
            workdir: "/w".into(),
            git_worktree: None,
            logs_dir: "/abs/logs/p-1".into(),
            started_at: chrono::Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap(),
            argv: argv.iter().map(|s| s.to_string()).collect(),
            pas_version: "1".into(),
            epic_id: None,
        }
    }

    fn s(v: Vec<OsString>) -> Vec<String> {
        v.into_iter().map(|o| o.into_string().unwrap()).collect()
    }

    #[test]
    fn resume_strips_owned_flags_and_keeps_others() {
        let m = meta(&[
            "pas",
            "run",
            "rel/p.dot",
            "--fresh",
            "--run-id",
            "old",
            "--logs=x",
            "--json",
            "--max-steps",
            "5",
            "--dry-run",
            "-w",
            "sub",
            "--allow-shared-workdir",
        ]);
        let got = s(reissue_args(&m, Reissue::Resume).unwrap());
        assert_eq!(
            got,
            [
                "run",
                "/abs/p.dot",
                "--max-steps",
                "5",
                "--dry-run",
                "-w",
                "sub",
                "--allow-shared-workdir",
                "--run-id",
                &m.run_id,
                "--logs",
                "/abs/logs/p-1"
            ]
        );
        assert!(!got.contains(&"--fresh".to_string()));
    }

    #[test]
    fn short_logs_forms_are_stripped() {
        for argv in [
            &["pas", "run", "p.dot", "-l", "d"][..],
            &["pas", "run", "p.dot", "-ld"][..],
            &["pas", "run", "p.dot", "--logs", "d"][..],
        ] {
            let got = s(reissue_args(&meta(argv), Reissue::Resume).unwrap());
            assert_eq!(got.iter().filter(|a| *a == "--logs").count(), 1, "{got:?}");
            assert!(
                !got.iter().any(|a| a == "d" || a.starts_with("-l")),
                "{got:?}"
            );
        }
    }

    #[test]
    fn flags_before_the_pipeline_are_kept_and_pipeline_is_replaced() {
        let m = meta(&["pas", "run", "--max-steps", "3", "rel.dot"]);
        let got = s(reissue_args(&m, Reissue::Resume).unwrap());
        assert_eq!(&got[..4], ["run", "/abs/p.dot", "--max-steps", "3"]);
        assert!(!got.contains(&"rel.dot".to_string()));
    }

    #[test]
    fn run_again_adds_fresh_and_a_new_id() {
        let m = meta(&["pas", "run", "p.dot"]);
        let new = new_run_id();
        let got = s(reissue_args(&m, Reissue::RunAgain { new_run_id: &new }).unwrap());
        assert!(got.contains(&"--fresh".to_string()));
        let at = got.iter().position(|a| a == "--run-id").unwrap();
        assert_eq!(got[at + 1], new);
        assert_ne!(new, m.run_id);
        assert!(parse_run_id(&new).is_some());
    }

    #[test]
    fn non_run_commands_are_refused() {
        for argv in [&["pas", "launch", "x"][..], &["pas"][..], &[][..]] {
            assert_eq!(
                reissue_args(&meta(argv), Reissue::Resume),
                Err(ControlError::NotARunCommand)
            );
        }
    }

    #[test]
    fn allowed_actions_by_status() {
        use RunStatus::*;
        let a = |stop, kill, resume, rerun| Allowed {
            stop,
            kill,
            resume,
            rerun,
        };
        assert_eq!(allowed(Running), a(true, true, false, false));
        assert_eq!(allowed(Completed), a(false, false, false, true));
        assert_eq!(allowed(Failed), a(false, false, true, true));
        assert_eq!(allowed(Stopped), a(false, false, true, true));
        assert_eq!(allowed(Crashed), a(false, false, true, true));
        assert_eq!(allowed(Missing), a(false, false, false, false));
    }
}
