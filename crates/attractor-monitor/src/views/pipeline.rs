//! The Pipeline check and Launch form. The Monitor only spawns `pas`
//! (`scaffold`, `generate`, `validate`, `run`) with the Plan's repository as
//! its working directory. The DOT path is derived from the stored Plan, never
//! taken from the browser; the browser sends DOT text and Launch values.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use attractor_journal::{new_run_id, read_index_at, PipelineDir};
use axum::extract::{DefaultBodyLimit, Form, Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::MethodRouter;
use maud::{html, Markup};
use serde_json::Value;

use crate::pipeline::{self, Check, LaunchForm};
use crate::plans::{self, OutputKind, PlanMeta};
use crate::spawn;
use crate::state::AppState;
use crate::views::proposal::{read_result, Pending};
use crate::views::run::{dot_graph, load_graph, GraphSource};

type Pairs = Vec<(String, String)>;

/// How long Launch waits for the Run to show up in the Run Index before it
/// redirects anyway.
const LAUNCH_WAIT: Duration = Duration::from_secs(5);
const LAUNCH_POLL: Duration = Duration::from_millis(100);
/// The saved DOT text is at most 1 MiB, and percent-encoding can triple it.
pub const SAVE_BODY_LIMIT: usize = 3 * pipeline::MAX_DOT_BYTES + (64 << 10);

pub fn body_limit<S: Clone + Send + Sync + 'static>(m: MethodRouter<S>) -> MethodRouter<S> {
    m.layer(DefaultBodyLimit::max(SAVE_BODY_LIMIT))
}

fn reply(status: StatusCode, m: Markup) -> Response {
    (status, Html(m.into_string())).into_response()
}

fn notice_err(msg: &str) -> Markup {
    html! { p.notice.err { (msg) } }
}

/// The Build button and the place its result goes.
pub fn build_button(pid: &str, exists: bool) -> Markup {
    html! {
        p {
            button type="button" hx-post=(format!("/plans/{pid}/pipeline")) hx-target="#pipeline"
                hx-disabled-elt="this"
                hx-confirm=[exists.then_some("Rebuilding overwrites the Pipeline file, including your edits. Continue?")] {
                @if exists { "Rebuild Pipeline (overwrites)" } @else { "Build Pipeline" }
            }
        }
        div #pipeline {}
    }
}

/// Launch values as typed, kept across a refused Launch.
#[derive(Debug, Clone)]
pub(crate) struct Values {
    pub(crate) workdir: String,
    pub(crate) budget: String,
    pub(crate) steps: String,
    shared: bool,
}

impl Values {
    pub(crate) fn defaults(repo: &Path) -> Self {
        Self {
            workdir: repo.display().to_string(),
            budget: pipeline::DEFAULT_MAX_BUDGET_USD.to_string(),
            steps: pipeline::DEFAULT_MAX_STEPS.to_string(),
            shared: false,
        }
    }

    fn from_form(form: &Pairs, repo: &Path) -> Self {
        let get = |k: &str| {
            form.iter()
                .rev()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
        };
        let d = Self::defaults(repo);
        Self {
            workdir: get("workdir").unwrap_or(d.workdir),
            budget: get("max_budget_usd").unwrap_or(d.budget),
            steps: get("max_steps").unwrap_or(d.steps),
            shared: get("allow_shared").is_some(),
        }
    }
}

fn diagnostics(check: &Check) -> Markup {
    html! {
        @match check {
            Check::Failed(m) => { p.notice.err { "Could not check the Pipeline: " (m) } }
            Check::Checked { valid, diagnostics } => {
                @if *valid { p.notice.ok #validity { "Pipeline is valid." } }
                @else { p.notice.err #validity { "Pipeline is not valid." } }
                @if !diagnostics.is_empty() {
                    ul #diagnostics {
                        @for d in diagnostics {
                            li class=(format!("diag {}", d.severity)) {
                                strong { (d.severity) } " "
                                @if let Some(n) = &d.node_id { code { (n) } ": " }
                                (d.message)
                                @if let Some(f) = &d.fix { " Fix: " (f) }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The Pipeline section: graph, DOT editor, diagnostics and the Launch form.
fn section(
    pid: &str,
    dot_path: &Path,
    check: &Check,
    values: &Values,
    error: Option<&str>,
) -> Markup {
    let source = load_graph(dot_path);
    let text = match &source {
        GraphSource::Dot(t) => t.as_str(),
        GraphSource::Unavailable(_) => "",
    };
    let base = format!("/plans/{pid}");
    html! {
        @if let Some(e) = error { (notice_err(e)) }
        p { "Pipeline file: " code { (dot_path.display()) } }
        (dot_graph(&source))
        form #dot-form hx-put=(format!("{base}/pipeline")) hx-target="#pipeline" {
            p { label { "DOT " br; textarea #dot-text name="dot" rows="18" cols="90" spellcheck="false" { (text) } } }
            p { button type="submit" { "Save and check" } }
        }
        (diagnostics(check))
        (build_button_inner(pid))
        form #launch-form hx-post=(format!("{base}/launch")) hx-target="#pipeline" {
            fieldset {
                legend { "Launch" }
                p { "Launch starts a new Run. To continue a stopped Run, use Resume on its page." }
                p { label { "Working directory " input type="text" name="workdir" value=(values.workdir) size="60"; } }
                p { label { "Budget (USD) " input type="text" name="max_budget_usd" value=(values.budget) size="8"; } }
                p { label { "Max steps " input type="text" name="max_steps" value=(values.steps) size="6"; } }
                p {
                    label {
                        input type="checkbox" name="allow_shared" checked[values.shared];
                        " Run even if another Run is active in this worktree"
                    }
                }
                p { button #launch-button type="submit" disabled[!check.is_valid()] hx-disabled-elt="this" { "Launch" } }
            }
        }
    }
}

pub(crate) fn build_button_inner(pid: &str) -> Markup {
    html! {
        p {
            button type="button" hx-post=(format!("/plans/{pid}/pipeline")) hx-target="#pipeline"
                hx-disabled-elt="this"
                hx-confirm="Rebuilding overwrites the Pipeline file, including your edits. Continue?" {
                "Rebuild Pipeline (overwrites)"
            }
        }
    }
}

pub(crate) struct Ctx {
    pub(crate) pid: String,
    pub(crate) dir: PathBuf,
    pub(crate) meta: PlanMeta,
    /// Where the Pipeline is written; `Err` is why it cannot be named yet.
    dot: Result<PathBuf, String>,
}

#[allow(clippy::result_large_err)] // the Err is the ready reply
pub(crate) fn ctx(state: &AppState, pid: &str) -> Result<Ctx, Response> {
    let root = plans::plans_root(state.index_path());
    let (dir, meta) = plans::load_meta(&root, pid)
        .ok_or_else(|| reply(StatusCode::NOT_FOUND, notice_err("no such Plan")))?;
    let epic = read_result(&dir).map(|(e, _)| e);
    let dot = pipeline::pipeline_path(&meta, epic.as_deref());
    Ok(Ctx {
        pid: pid.to_string(),
        dir,
        meta,
        dot,
    })
}

impl Ctx {
    #[allow(clippy::result_large_err)]
    fn dot(&self) -> Result<&Path, Response> {
        self.dot
            .as_deref()
            .map_err(|m| reply(StatusCode::CONFLICT, notice_err(m)))
    }

    /// The saved Pipeline file, which must exist.
    #[allow(clippy::result_large_err)]
    fn existing_dot(&self) -> Result<&Path, Response> {
        let p = self.dot()?;
        if p.is_file() {
            Ok(p)
        } else {
            Err(reply(
                StatusCode::CONFLICT,
                notice_err("no Pipeline yet: build it first"),
            ))
        }
    }

    pub(crate) fn render(
        &self,
        status: StatusCode,
        check: &Check,
        v: &Values,
        err: Option<&str>,
    ) -> Response {
        match self.dot.as_deref() {
            Ok(p) => reply(status, section(&self.pid, p, check, v, err)),
            Err(m) => reply(StatusCode::CONFLICT, notice_err(m)),
        }
    }
}

/// `pas validate <dot> --json`, its verdict parsed from stdout.
async fn check(state: &AppState, repo: &Path, dot: &Path) -> Check {
    let exe = match state.pas_exe() {
        Ok(e) => e,
        Err(e) => return Check::Failed(format!("cannot find the pas executable: {e}")),
    };
    let args: Vec<OsString> = vec!["validate".into(), dot.into(), "--json".into()];
    match spawn::run_at_in(&exe, &args, Some(repo)).await {
        Ok(o) => pipeline::parse_validate(o.code, &o.stdout, &o.stderr),
        Err(e) => Check::Failed(format!("cannot run pas validate: {e}")),
    }
}

/// `GET /plans/{pid}/pipeline`: the saved Pipeline section, for a reload.
pub async fn show(State(state): State<AppState>, UrlPath(pid): UrlPath<String>) -> Response {
    let c = match ctx(&state, &pid) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let dot = match c.existing_dot() {
        Ok(d) => d.to_path_buf(),
        Err(r) => return r,
    };
    let chk = check(&state, &c.meta.repo, &dot).await;
    c.render(StatusCode::OK, &chk, &Values::defaults(&c.meta.repo), None)
}

/// The message of the failed `pas ... --json` (`ok:false`), if stdout has one.
fn failure(out: &spawn::PasOutput) -> (Option<String>, String) {
    let v: Option<Value> = serde_json::from_str(out.stdout.trim()).ok();
    let code = v
        .as_ref()
        .and_then(|v| v["error"]["code"].as_str().map(String::from));
    let msg = v
        .as_ref()
        .and_then(|v| v["error"]["message"].as_str().map(String::from))
        .or_else(|| Some(out.stderr.trim().to_string()).filter(|s| !s.is_empty()))
        .unwrap_or_else(|| format!("pas exited with code {}", out.code.unwrap_or(-1)));
    (code, msg)
}

/// The result of building the Pipeline file.
pub(crate) enum Built {
    /// The file is written and checked. The message is why `pas` refused it,
    /// when the file is there to be fixed in the editor.
    Ready(Check, Option<String>),
    Failed(Response),
}

/// `POST /plans/{pid}/pipeline`: build the Pipeline with `pas scaffold`
/// (Epic mode) or `pas generate --plan` (Pipeline only).
pub async fn build(State(state): State<AppState>, UrlPath(pid): UrlPath<String>) -> Response {
    let c = match ctx(&state, &pid) {
        Ok(c) => c,
        Err(r) => return r,
    };
    match build_step(&state, &c).await {
        Built::Ready(chk, msg) => c.render(
            StatusCode::OK,
            &chk,
            &Values::defaults(&c.meta.repo),
            msg.as_deref(),
        ),
        Built::Failed(r) => r,
    }
}

pub(crate) async fn build_step(state: &AppState, c: &Ctx) -> Built {
    let dot = match c.dot() {
        Ok(d) => d.to_path_buf(),
        Err(r) => return Built::Failed(r),
    };
    let marker = c.dir.join("pipeline.pending");
    if std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
        .is_err()
    {
        return Built::Failed(reply(
            StatusCode::CONFLICT,
            notice_err("this Plan's Pipeline is already being built"),
        ));
    }
    let _pending = Pending(marker);
    let mut args: Vec<OsString> = Vec::new();
    match c.meta.kind {
        OutputKind::EpicAndPipeline => {
            let epic = read_result(&c.dir).map(|(e, _)| e).unwrap_or_default();
            args.extend(["scaffold".into(), epic.into()]);
        }
        OutputKind::PipelineOnly => {
            args.push("generate".into());
            for f in plans::docs_in_order(&c.dir, &c.meta) {
                args.push("--plan".into());
                args.push(f.into_os_string());
            }
        }
    }
    args.extend(["--output".into(), dot.clone().into(), "--json".into()]);
    let exe = match state.pas_exe() {
        Ok(e) => e,
        Err(e) => {
            return Built::Failed(reply(
                StatusCode::BAD_GATEWAY,
                notice_err(&format!("cannot find the pas executable: {e}")),
            ))
        }
    };
    let out = match spawn::run_at_in(&exe, &args, Some(&c.meta.repo)).await {
        Ok(o) => o,
        Err(e) => {
            return Built::Failed(reply(
                StatusCode::BAD_GATEWAY,
                notice_err(&format!("cannot run pas: {e}")),
            ))
        }
    };
    let ok = out.code == Some(0)
        && serde_json::from_str::<Value>(out.stdout.trim())
            .map(|v| v["ok"] == Value::Bool(true))
            .unwrap_or(false);
    if ok {
        return Built::Ready(check(state, &c.meta.repo, &dot).await, None);
    }
    let (code, msg) = failure(&out);
    // The file is written before it is validated, so it can be fixed in the editor.
    if code.as_deref() == Some("invalid_pipeline") && dot.is_file() {
        return Built::Ready(check(state, &c.meta.repo, &dot).await, Some(msg));
    }
    let shown = match code {
        Some(k) => format!("{msg} ({k})"),
        None => msg,
    };
    Built::Failed(reply(StatusCode::BAD_GATEWAY, notice_err(&shown)))
}

/// `PUT /plans/{pid}/pipeline`: save the edited DOT, then check it again.
pub async fn save(
    State(state): State<AppState>,
    UrlPath(pid): UrlPath<String>,
    Form(form): Form<Pairs>,
) -> Response {
    let c = match ctx(&state, &pid) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let dot = match c.existing_dot() {
        Ok(d) => d.to_path_buf(),
        Err(r) => return r,
    };
    let values = Values::defaults(&c.meta.repo);
    let text = form
        .iter()
        .rev()
        .find(|(n, _)| n == "dot")
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    let bad = |m: &str| {
        let chk = Check::Failed("the file was not changed".into());
        c.render(StatusCode::BAD_REQUEST, &chk, &values, Some(m))
    };
    if text.trim().is_empty() {
        return bad("the DOT text is empty");
    }
    if text.len() > pipeline::MAX_DOT_BYTES {
        return bad("the DOT text is larger than 1 MiB");
    }
    let tmp = dot.with_extension("dot.tmp");
    let text = text.to_string();
    let (t2, d2) = (tmp.clone(), dot.clone());
    let written = tokio::task::spawn_blocking(move || {
        std::fs::write(&t2, text).and_then(|()| std::fs::rename(&t2, &d2))
    })
    .await;
    match written {
        Ok(Ok(())) => {}
        other => {
            let _ = std::fs::remove_file(&tmp);
            let why = match other {
                Ok(Err(e)) => e.to_string(),
                _ => "internal error".into(),
            };
            return reply(
                StatusCode::INTERNAL_SERVER_ERROR,
                notice_err(&format!("cannot save the Pipeline: {why}")),
            );
        }
    }
    let chk = check(&state, &c.meta.repo, &dot).await;
    c.render(StatusCode::OK, &chk, &values, None)
}

/// The error a refused `pas run` printed: its JSON `error.message` line in
/// `console.log`, else the tail of the log.
fn refusal_message(console: &Path) -> String {
    let text = crate::controls::tail(console);
    text.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .find(|v| v["ok"] == Value::Bool(false))
        .and_then(|v| v["error"]["message"].as_str().map(String::from))
        .unwrap_or(text)
}

/// `POST /plans/{pid}/launch`: check the Pipeline again, then start `pas run`.
pub async fn launch(
    State(state): State<AppState>,
    UrlPath(pid): UrlPath<String>,
    Form(form): Form<Pairs>,
) -> Response {
    match ctx(&state, &pid) {
        Ok(c) => launch_step(&state, &c, &form).await,
        Err(r) => r,
    }
}

pub(crate) async fn launch_step(state: &AppState, c: &Ctx, form: &Pairs) -> Response {
    let dot = match c.existing_dot() {
        Ok(d) => d.to_path_buf(),
        Err(r) => return r,
    };
    let values = Values::from_form(form, &c.meta.repo);
    let chk = check(state, &c.meta.repo, &dot).await;
    let f = match LaunchForm::parse(form) {
        Ok(f) => f,
        Err(m) => return c.render(StatusCode::BAD_REQUEST, &chk, &values, Some(&m)),
    };
    if !chk.is_valid() {
        return c.render(
            StatusCode::CONFLICT,
            &chk,
            &values,
            Some("the Pipeline is not valid, so it cannot be launched"),
        );
    }
    let exe = match state.pas_exe() {
        Ok(e) => e,
        Err(e) => {
            return c.render(
                StatusCode::BAD_GATEWAY,
                &chk,
                &values,
                Some(&format!("cannot find the pas executable: {e}")),
            )
        }
    };
    let run_id = new_run_id();
    let logs = pipeline::logs_dir(&c.meta.repo, &dot);
    let console = match PipelineDir::new(&logs).run(&run_id) {
        Ok(d) => d.console_log(),
        Err(e) => {
            return c.render(
                StatusCode::BAD_GATEWAY,
                &chk,
                &values,
                Some(&format!("cannot place the new Run: {e}")),
            )
        }
    };
    let args = pipeline::launch_args(&dot, &run_id, &f);
    let (_pid, mut exited) =
        match spawn::spawn_detached_watch(&exe, &args, &console, Some(&c.meta.repo)) {
            Ok(v) => v,
            Err(e) => {
                return c.render(
                    StatusCode::BAD_GATEWAY,
                    &chk,
                    &values,
                    Some(&format!("cannot start pas: {e}")),
                )
            }
        };
    let deadline = Instant::now() + LAUNCH_WAIT;
    let mut code: Option<Option<i32>> = None;
    loop {
        // Register the Run now, so its page exists before the watcher's next poll.
        let index = state.index_path().to_path_buf();
        let entries = tokio::task::spawn_blocking(move || read_index_at(&index))
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        if let Some(e) = entries.into_iter().find(|e| e.run_id == run_id) {
            state.upsert_entry(e);
            return (
                StatusCode::OK,
                [("HX-Redirect", format!("/runs/{run_id}"))],
                Html(
                    html! { p.notice.ok { "Started Run " a href=(format!("/runs/{run_id}")) { (run_id) } } }
                        .into_string(),
                ),
            )
                .into_response();
        }
        if code.is_none() {
            code = match exited.try_recv() {
                Ok(c) => Some(c),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => None,
                Err(_) => Some(None),
            };
        }
        match code {
            Some(Some(0)) | None => {}
            Some(other) => {
                let mut msg = refusal_message(&console);
                if msg.is_empty() {
                    msg = match other {
                        Some(n) => format!("pas exited with code {n} at start"),
                        None => "pas was stopped by a signal at start".into(),
                    };
                }
                let (status, hint) = match other {
                    Some(6) => (
                        StatusCode::CONFLICT,
                        " Tick \"Run even if another Run is active in this worktree\" to start anyway.",
                    ),
                    Some(5) => (StatusCode::CONFLICT, ""),
                    _ => (StatusCode::BAD_GATEWAY, ""),
                };
                return c.render(status, &chk, &values, Some(&format!("{msg}{hint}")));
            }
        }
        if Instant::now() >= deadline {
            // Still starting: the Run page appears once the Index has it.
            return (
                StatusCode::OK,
                [("HX-Redirect", format!("/runs/{run_id}"))],
                Html(html! { p.notice { "Starting Run " (run_id) } }.into_string()),
            )
                .into_response();
        }
        tokio::time::sleep(LAUNCH_POLL).await;
    }
}
