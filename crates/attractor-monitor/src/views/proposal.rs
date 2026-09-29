//! The Proposal editor and Create Epic. The Monitor only spawns
//! `pas decompose` (never `bd`) with the Plan's repository as its working
//! directory; the files it writes are `proposal.json` and `result.json`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use axum::extract::{Form, Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use maud::{html, Markup};
use serde_json::Value;

use crate::plans::{self, OutputKind, PlanMeta};
use crate::proposal::{self, Proposal};
use crate::spawn;
use crate::state::AppState;

type Pairs = Vec<(String, String)>;

fn reply(status: StatusCode, m: Markup) -> Response {
    (status, Html(m.into_string())).into_response()
}

fn notice_err(msg: &str) -> Markup {
    html! { p.notice.err { (msg) } }
}

fn tail(s: &str) -> &str {
    let mut from = s.len().saturating_sub(4096);
    while !s.is_char_boundary(from) {
        from += 1;
    }
    s[from..].trim()
}

/// The editor: Epic fields, one fieldset per Task, and a dependency summary.
fn editor(pid: &str, p: &Proposal, error: Option<&str>) -> Markup {
    let put = format!("/plans/{pid}/proposal");
    let post = format!("/plans/{pid}/epic");
    html! {
        @if let Some(e) = error { (notice_err(e)) }
        form #proposal-form hx-put=(put) hx-target="#proposal" {
            fieldset {
                legend { "Epic" }
                p { label { "Title " input type="text" name="epic_title" value=(p.epic.title) size="60"; } }
                p { label { "Description " br; textarea name="epic_description" rows="4" cols="70" { (p.epic.description) } } }
            }
            @for (i, t) in p.tasks.iter().enumerate() {
                fieldset .task {
                    legend { "Task " (i + 1) }
                    p { label { "Title " input type="text" name=(format!("t{i}_title")) value=(t.title) size="60"; } }
                    p {
                        label { "Type " input type="text" name=(format!("t{i}_type")) value=(t.r#type) size="10"; }
                        " "
                        label { "Priority " input type="text" name=(format!("t{i}_priority")) value=(t.priority) size="4"; }
                    }
                    p { label { "Description " br; textarea name=(format!("t{i}_description")) rows="3" cols="70" { (t.description) } } }
                    p { label { "Acceptance " br; textarea name=(format!("t{i}_acceptance")) rows="2" cols="70" { (t.acceptance.as_deref().unwrap_or("")) } } }
                    p { label { "Design " br; textarea name=(format!("t{i}_design")) rows="2" cols="70" { (t.design.as_deref().unwrap_or("")) } } }
                    p { label { "Notes " br; textarea name=(format!("t{i}_notes")) rows="2" cols="70" { (t.notes.as_deref().unwrap_or("")) } } }
                    @let others: Vec<usize> = (0..p.tasks.len()).filter(|&m| m != i).collect();
                    @if !others.is_empty() {
                        p {
                            "Blocked by: "
                            @for m in others {
                                label {
                                    input type="checkbox" name="dep" value=(format!("{i}:{m}"))
                                        checked[p.dependencies.iter().any(|d| d.blocked == i && d.blocker == m)];
                                    " Task " (m + 1) " "
                                }
                            }
                        }
                    }
                    p { button type="submit" name="remove" value=(i) { "Remove Task " (i + 1) } }
                }
            }
            p { button type="submit" { "Save" } }
            p { button type="button" hx-post=(post) hx-target="#proposal" hx-disabled-elt="this" { "Create Epic" } }
        }
        h3 { "Dependencies" }
        @if p.dependencies.is_empty() {
            p { "None." }
        } @else {
            ul #dependencies {
                @for d in &p.dependencies {
                    @if d.blocked < p.tasks.len() && d.blocker < p.tasks.len() {
                        li {
                            "Task " (d.blocked + 1) " \"" (p.tasks[d.blocked].title) "\" blocked by Task "
                            (d.blocker + 1) " \"" (p.tasks[d.blocker].title) "\""
                        }
                    }
                }
            }
        }
    }
}

fn created(pid: &str, epic_id: &str, task_ids: &[String]) -> Markup {
    html! {
        p.notice.ok {
            "Epic " code { (epic_id) } " created with " (task_ids.len()) " Task(s): "
            @for (i, t) in task_ids.iter().enumerate() {
                @if i > 0 { ", " }
                code { (t) }
            }
        }
        (super::pipeline::build_button(pid, false))
    }
}

// The Err is the ready reply, returned as is by the handlers.
#[allow(clippy::result_large_err)]
fn load(state: &AppState, pid: &str) -> Result<(PathBuf, PlanMeta), Response> {
    let root = plans::plans_root(state.index_path());
    plans::load_meta(&root, pid)
        .ok_or_else(|| reply(StatusCode::NOT_FOUND, notice_err("no such Plan")))
}

/// Parse `pas decompose --json` output. The JSON is on stdout even when the
/// exit code is 1, so it is parsed whatever the code. `Err` is a message.
fn parse_decompose(out: std::io::Result<spawn::PasOutput>) -> Result<Value, String> {
    let out = out.map_err(|e| format!("cannot run pas decompose: {e}"))?;
    let v: Option<Value> = serde_json::from_str(out.stdout.trim()).ok();
    match v {
        Some(v) if v["ok"] == Value::Bool(true) && out.code == Some(0) => Ok(v),
        Some(v) if v["ok"] == Value::Bool(false) => {
            let msg = v["error"]["message"].as_str().unwrap_or("decompose failed");
            Err(match v["error"]["code"].as_str() {
                Some(code) => format!("{msg} ({code})"),
                None => msg.to_string(),
            })
        }
        _ => Err(format!(
            "pas decompose produced no usable result (exit {:?}): {}",
            out.code,
            tail(&out.stderr)
        )),
    }
}

async fn decompose(state: &AppState, args: Vec<OsString>, repo: &Path) -> Result<Value, String> {
    let exe = state
        .pas_exe()
        .map_err(|e| format!("cannot find the pas executable: {e}"))?;
    parse_decompose(spawn::run_at_in(&exe, &args, Some(repo)).await)
}

fn pid_of(dir: &Path) -> String {
    dir.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `POST /plans/{pid}/proposal`: run Generate and store the Proposal.
pub async fn generate(State(state): State<AppState>, UrlPath(pid): UrlPath<String>) -> Response {
    let (dir, meta) = match load(&state, &pid) {
        Ok(x) => x,
        Err(r) => return r,
    };
    if meta.kind != OutputKind::EpicAndPipeline {
        return reply(
            StatusCode::CONFLICT,
            notice_err("this Plan produces a Pipeline only, so it has no Proposal"),
        );
    }
    let mut args: Vec<OsString> = vec!["decompose".into()];
    for f in plans::docs_in_order(&dir, &meta) {
        args.push("--plan".into());
        args.push(f.into_os_string());
    }
    args.extend(["--dry-run".into(), "--json".into()]);
    let v = match decompose(&state, args, &meta.repo).await {
        Ok(v) => v,
        Err(m) => return reply(StatusCode::BAD_GATEWAY, notice_err(&m)),
    };
    let p: Proposal = match serde_json::from_value(v["proposal"].clone()) {
        Ok(p) => p,
        Err(e) => {
            return reply(
                StatusCode::BAD_GATEWAY,
                notice_err(&format!("pas decompose returned a malformed Proposal: {e}")),
            )
        }
    };
    // Stored even when it has a cycle, so the model's output is not lost;
    // Create Epic refuses it until it is fixed.
    if let Err(e) = proposal::write_atomic(&dir.join("proposal.json"), &p) {
        return reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            notice_err(&format!("cannot store the Proposal: {e}")),
        );
    }
    let warn = p.validate().err();
    reply(StatusCode::OK, editor(&pid, &p, warn.as_deref()))
}

/// `GET /plans/{pid}/proposal`: the saved editor, or the created Epic.
pub async fn show(State(state): State<AppState>, UrlPath(pid): UrlPath<String>) -> Response {
    let (dir, _) = match load(&state, &pid) {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Some((epic, tasks)) = read_result(&dir) {
        return reply(StatusCode::OK, created(&pid, &epic, &tasks));
    }
    match proposal::read(&dir.join("proposal.json")) {
        Ok(p) => {
            let warn = p.validate().err();
            reply(StatusCode::OK, editor(&pid, &p, warn.as_deref()))
        }
        Err(_) => reply(StatusCode::NOT_FOUND, notice_err("no Proposal yet")),
    }
}

pub(crate) fn read_result(dir: &Path) -> Option<(String, Vec<String>)> {
    let v: Value = serde_json::from_slice(&std::fs::read(dir.join("result.json")).ok()?).ok()?;
    let ids = v["task_ids"].as_array()?;
    Some((
        v["epic_id"].as_str()?.to_string(),
        ids.iter()
            .filter_map(|t| t.as_str().map(String::from))
            .collect(),
    ))
}

/// Apply the editor's fields to the saved Proposal and validate. On failure
/// the reply is ready: the submitted values re-rendered (400) with the
/// message, or 409 when there is no Proposal to edit.
#[allow(clippy::result_large_err)]
fn edited(pid: &str, dir: &Path, form: &Pairs) -> Result<Proposal, Response> {
    let Ok(mut p) = proposal::read(&dir.join("proposal.json")) else {
        return Err(reply(
            StatusCode::CONFLICT,
            notice_err("no Proposal yet: generate one first"),
        ));
    };
    let base = p.clone();
    let bad = |p: &Proposal, m: &str| reply(StatusCode::BAD_REQUEST, editor(pid, p, Some(m)));
    if let Err(m) = p.apply_form(form) {
        return Err(bad(&base, &m));
    }
    p.validate().map_err(|m| bad(&p, &m))?;
    Ok(p)
}

/// `PUT /plans/{pid}/proposal`: save the edited Proposal.
pub async fn save(
    State(state): State<AppState>,
    UrlPath(pid): UrlPath<String>,
    Form(form): Form<Pairs>,
) -> Response {
    let (dir, _) = match load(&state, &pid) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let p = match edited(&pid, &dir, &form) {
        Ok(p) => p,
        Err(r) => return r,
    };
    match proposal::write_atomic(&dir.join("proposal.json"), &p) {
        Ok(()) => reply(StatusCode::OK, editor(&pid, &p, None)),
        Err(e) => reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            notice_err(&format!("cannot store the Proposal: {e}")),
        ),
    }
}

/// Removes the in-progress marker however the request ends.
pub(crate) struct Pending(pub(crate) PathBuf);

impl Drop for Pending {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// `POST /plans/{pid}/epic`: save what is on screen, then create the Epic.
pub async fn create_epic(
    State(state): State<AppState>,
    UrlPath(pid): UrlPath<String>,
    Form(form): Form<Pairs>,
) -> Response {
    let (dir, meta) = match load(&state, &pid) {
        Ok(x) => x,
        Err(r) => return r,
    };
    if let Some((epic, tasks)) = read_result(&dir) {
        return reply(StatusCode::CONFLICT, created(&pid, &epic, &tasks));
    }
    let marker = dir.join("epic.pending");
    let opened = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker);
    if opened.is_err() {
        return reply(
            StatusCode::CONFLICT,
            notice_err("this Plan's Epic is already being created"),
        );
    }
    let _pending = Pending(marker);
    let path = dir.join("proposal.json");
    // Without editor fields the saved Proposal is created as it is.
    let p = match edited(&pid, &dir, &form) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if let Err(e) = proposal::write_atomic(&path, &p) {
        return reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            notice_err(&format!("cannot store the Proposal: {e}")),
        );
    }
    let args: Vec<OsString> = vec![
        "decompose".into(),
        "--from-proposal".into(),
        path.into_os_string(),
        "--json".into(),
    ];
    let v = match decompose(&state, args, &meta.repo).await {
        Ok(v) => v,
        Err(m) => return reply(StatusCode::BAD_GATEWAY, editor(&pid, &p, Some(&m))),
    };
    let (Some(epic), Some(tasks)) = (v["epic_id"].as_str(), v["task_ids"].as_array()) else {
        return reply(
            StatusCode::BAD_GATEWAY,
            editor(&pid, &p, Some("pas decompose returned no Epic id")),
        );
    };
    let tasks: Vec<String> = tasks
        .iter()
        .filter_map(|t| t.as_str().map(String::from))
        .collect();
    let result = serde_json::json!({ "v": 1, "epic_id": epic, "task_ids": tasks });
    let tmp = dir.join("result.json.tmp");
    let written = std::fs::write(&tmp, result.to_string())
        .and_then(|()| std::fs::rename(&tmp, dir.join("result.json")));
    if let Err(e) = written {
        // The Epic exists in beads; say so rather than hide it.
        return reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            notice_err(&format!(
                "Epic {epic} was created but result.json could not be written (Plan {}): {e}",
                pid_of(&dir)
            )),
        );
    }
    reply(StatusCode::OK, created(&pid, epic, &tasks))
}
