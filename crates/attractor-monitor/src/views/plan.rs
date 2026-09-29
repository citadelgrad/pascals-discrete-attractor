//! The New Plan page: upload ordered files, choose the target repository,
//! output kind and mode. Storage is `plans.rs`; this only reads the form.

use axum::extract::multipart::Multipart;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use maud::{html, Markup, PreEscaped, DOCTYPE};

use crate::plans::{self, Mode, OutputKind, PlanError, PlanMeta, PlanRequest, Upload};
use crate::state::AppState;

/// Shows 400 replies, and lets the user reorder the chosen files. The file
/// input is rebuilt from a `DataTransfer`, so the multipart field order is
/// the chosen order.
const PLAN_SCRIPT: &str = r#"document.body.addEventListener('htmx:beforeSwap',function(e){if([400,404,409,502].indexOf(e.detail.xhr.status)>=0){e.detail.shouldSwap=true;e.detail.isError=false;}});
(function(){var inp=document.getElementById('plan-files'),box=document.getElementById('file-order');
function draw(){box.innerHTML='';Array.from(inp.files).forEach(function(f,i){var li=document.createElement('li');li.textContent=f.name+' ';
[['Up',-1],['Down',1]].forEach(function(b){var btn=document.createElement('button');btn.type='button';btn.textContent=b[0];btn.onclick=function(){move(i,b[1]);};li.appendChild(btn);});box.appendChild(li);});}
function move(i,d){var a=Array.from(inp.files),j=i+d;if(j<0||j>=a.length)return;var t=a[i];a[i]=a[j];a[j]=t;var dt=new DataTransfer();a.forEach(function(f){dt.items.add(f);});inp.files=dt.files;draw();}
inp.addEventListener('change',draw);})();"#;

pub fn page(csrf: &str) -> Markup {
    html! {
        (DOCTYPE)
        html {
            head {
                meta charset="utf-8";
                title { "PAS Monitor: New Plan" }
                link rel="stylesheet" href="/assets/monitor.css";
                script src="/assets/htmx.min.js" {}
            }
            body hx-headers=(serde_json::json!({ "X-CSRF-Token": csrf }).to_string()) {
                p { a href="/" { "All Runs" } }
                h1 { "New Plan" }
                form #plan-form hx-post="/plans/new" hx-encoding="multipart/form-data" hx-target="#plan-result" {
                    p {
                        label { "Plan files (.md or .txt, up to " (plans::MAX_FILES) " files, 1 MiB each) "
                            input #plan-files type="file" name="files" multiple accept=".md,.txt";
                        }
                    }
                    ol #file-order {}
                    p {
                        label { "Target repository (path) "
                            input type="text" name="repo" required size="60";
                        }
                    }
                    fieldset {
                        legend { "Output" }
                        label { input type="radio" name="kind" value="pipeline" checked; " Pipeline only" }
                        " "
                        label { input type="radio" name="kind" value="epic_pipeline"; " Epic + Pipeline" }
                    }
                    fieldset {
                        legend { "Mode" }
                        label { input type="radio" name="mode" value="reviewed" checked; " Reviewed" }
                        " "
                        label { input type="radio" name="mode" value="one_click"; " One-click" }
                    }
                    p { button type="submit" { "Create Plan" } }
                }
                div #plan-result {}
                script { (PreEscaped(PLAN_SCRIPT)) }
            }
        }
    }
}

fn created(m: &PlanMeta) -> Markup {
    let kind = match m.kind {
        OutputKind::PipelineOnly => "Pipeline only",
        OutputKind::EpicAndPipeline => "Epic + Pipeline",
    };
    let mode = match m.mode {
        Mode::Reviewed => "Reviewed",
        Mode::OneClick => "One-click",
    };
    html! {
        p.notice.ok {
            "Plan " code { (m.id) } " created: " (m.files.len()) " file(s), "
            (m.repo.display()) ", " (kind) ", " (mode)
        }
        @if m.mode == Mode::OneClick {
            div #one-click hx-post=(format!("/plans/{}/one-click", m.id)) hx-trigger="load"
                hx-target="#one-click" hx-disabled-elt="this" {
                p { "Working: Proposal, Epic, Pipeline, check and Launch run without pausing." }
            }
        } @else if m.kind == OutputKind::EpicAndPipeline {
            p {
                button type="button" hx-post=(format!("/plans/{}/proposal", m.id))
                    hx-target="#proposal" hx-disabled-elt="this" { "Generate Proposal" }
            }
            div #proposal {}
        } @else {
            (super::pipeline::build_button(&m.id, false))
        }
    }
}

fn rejected(msg: &str) -> Markup {
    html! { p.notice.err { (msg) } }
}

fn bad(msg: &str) -> Response {
    (StatusCode::BAD_REQUEST, Html(rejected(msg).into_string())).into_response()
}

pub async fn page_handler(State(state): State<AppState>) -> Html<String> {
    Html(page(state.csrf_token().as_str()).into_string())
}

/// Read the form. `Err` is a user-facing message for a 400.
async fn read_form(mut mp: Multipart) -> Result<PlanRequest, PlanError> {
    let mut files = Vec::new();
    let (mut repo, mut kind, mut mode) = (String::new(), None, None);
    let malformed = || PlanError::BadForm("malformed form".into());
    while let Some(mut field) = mp.next_field().await.map_err(|_| malformed())? {
        let name = field.name().unwrap_or("").to_string();
        if name == "files" {
            let fname = field.file_name().unwrap_or("").to_string();
            // An empty file input still submits one nameless empty part.
            let mut bytes = Vec::new();
            while let Some(chunk) = field.chunk().await.map_err(|_| malformed())? {
                if bytes.len() + chunk.len() > plans::MAX_FILE_BYTES {
                    return Err(PlanError::TooLarge(fname));
                }
                bytes.extend_from_slice(&chunk);
            }
            if fname.is_empty() && bytes.is_empty() {
                continue;
            }
            files.push(Upload { name: fname, bytes });
        } else {
            let text = field.text().await.map_err(|_| malformed())?;
            match name.as_str() {
                "repo" => repo = text.trim().to_string(),
                "kind" => {
                    kind = Some(match text.as_str() {
                        "pipeline" => OutputKind::PipelineOnly,
                        "epic_pipeline" => OutputKind::EpicAndPipeline,
                        _ => return Err(PlanError::BadForm("unknown output kind".into())),
                    })
                }
                "mode" => {
                    mode = Some(match text.as_str() {
                        "reviewed" => Mode::Reviewed,
                        "one_click" => Mode::OneClick,
                        _ => return Err(PlanError::BadForm("unknown mode".into())),
                    })
                }
                _ => {}
            }
        }
    }
    Ok(PlanRequest {
        files,
        repo,
        kind: kind.unwrap_or(OutputKind::PipelineOnly),
        mode: mode.unwrap_or(Mode::Reviewed),
    })
}

pub async fn create_handler(State(state): State<AppState>, mp: Multipart) -> Response {
    let req = match read_form(mp).await {
        Ok(r) => r,
        Err(e) => return bad(&e.to_string()),
    };
    let root = plans::plans_root(state.index_path());
    match tokio::task::spawn_blocking(move || plans::create(&root, req)).await {
        Ok(Ok(meta)) => Html(created(&meta).into_string()).into_response(),
        Ok(Err(e)) if e.is_client_error() => bad(&e.to_string()),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html(rejected("cannot store the Plan").into_string()),
        )
            .into_response(),
    }
}
