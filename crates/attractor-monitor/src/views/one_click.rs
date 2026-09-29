//! One-click mode: the reviewed steps run in order without pausing. Proposal,
//! Epic, Pipeline, check and Launch are the same handlers the reviewed flow
//! uses; the flow stops at the first step that fails and shows that step's
//! page, so the person can carry on from there in reviewed style.

use axum::extract::{Form, Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use maud::{html, Markup};

use crate::plans::{Mode, OutputKind};
use crate::state::AppState;
use crate::views::pipeline::{self, Built, Values};
use crate::views::proposal::{self, read_result, Pending};

fn refuse(status: StatusCode, msg: &str) -> Response {
    (status, Html(html! { p.notice.err { (msg) } }.into_string())).into_response()
}

/// A failed step's reply inside the container its own buttons target. A
/// reply with `HX-Redirect` (a started Run) passes through unchanged.
async fn stopped(resp: Response, container: &str, extra: Option<Markup>) -> Response {
    if resp.headers().contains_key("HX-Redirect") {
        return resp;
    }
    let (parts, body) = resp.into_parts();
    let bytes = axum::body::to_bytes(body, 8 << 20)
        .await
        .unwrap_or_default();
    let inner = String::from_utf8_lossy(&bytes).into_owned();
    let extra = extra.map(|m| m.into_string()).unwrap_or_default();
    let html = format!(r#"<div id="{container}">{inner}{extra}</div>"#);
    (parts.status, Html(html)).into_response()
}

/// `POST /plans/{pid}/one-click`.
pub async fn run(State(state): State<AppState>, UrlPath(pid): UrlPath<String>) -> Response {
    let c = match pipeline::ctx(&state, &pid) {
        Ok(c) => c,
        Err(r) => return r,
    };
    if c.meta.mode != Mode::OneClick {
        return refuse(
            StatusCode::CONFLICT,
            "this Plan is reviewed: use its buttons to step through it",
        );
    }
    let marker = c.dir.join("one-click.pending");
    if std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
        .is_err()
    {
        return refuse(
            StatusCode::CONFLICT,
            "this Plan's one-click run is already in progress",
        );
    }
    let _pending = Pending(marker);
    // A retry after a later step failed reuses the Epic; it never makes a second.
    if c.meta.kind == OutputKind::EpicAndPipeline && read_result(&c.dir).is_none() {
        let r = proposal::generate(State(state.clone()), UrlPath(pid.clone())).await;
        if r.status() != StatusCode::OK {
            return stopped(r, "proposal", None).await;
        }
        let r =
            proposal::create_epic(State(state.clone()), UrlPath(pid.clone()), Form(vec![])).await;
        if r.status() != StatusCode::OK {
            return stopped(r, "proposal", None).await;
        }
    }
    // The Pipeline path depends on the Epic id, so read the Plan again.
    let c = match pipeline::ctx(&state, &pid) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let values = Values::defaults(&c.meta.repo);
    match pipeline::build_step(&state, &c).await {
        Built::Failed(r) => {
            let again = pipeline::build_button_inner(&pid);
            return stopped(r, "pipeline", Some(again)).await;
        }
        Built::Ready(chk, msg) => {
            if !chk.is_valid() || msg.is_some() {
                let r = c.render(StatusCode::OK, &chk, &values, msg.as_deref());
                return stopped(r, "pipeline", None).await;
            }
        }
    }
    let form = vec![
        ("workdir".to_string(), values.workdir.clone()),
        ("max_budget_usd".to_string(), values.budget.clone()),
        ("max_steps".to_string(), values.steps.clone()),
    ];
    let r = pipeline::launch_step(&state, &c, &form).await;
    stopped(r, "pipeline", None).await
}
