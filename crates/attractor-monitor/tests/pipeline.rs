//! Pipeline check and Launch over HTTP with a fake `pas` (attractor-ino.42).

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use attractor_monitor::state::AppState;
use attractor_monitor::{bind, serve_on_with};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const PID: &str = "0123456789abcdef0123456789abcdef";
const VALID: &str = r#"{"v":1,"ok":true,"valid":true,"diagnostics":[]}"#;

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
    fn repo(&self) -> PathBuf {
        self.root().join("repo").canonicalize().unwrap()
    }
    fn plan(&self) -> PathBuf {
        self.root().join("plans").join(PID)
    }
    fn calls(&self) -> String {
        std::fs::read_to_string(self.root().join("calls.log")).unwrap_or_default()
    }
    fn calls_of(&self, cmd: &str) -> Vec<String> {
        let marker = format!(" {cmd} ");
        self.calls()
            .lines()
            .filter(|l| l.contains(&marker))
            .map(String::from)
            .collect()
    }
    fn set(&self, name: &str, text: &str) {
        std::fs::write(self.root().join(name), text).unwrap();
    }
    fn dot(&self, name: &str) -> PathBuf {
        self.repo().join("pipelines").join(format!("{name}.dot"))
    }
}

async fn env_kind(kind: &str, epic: Option<&str>) -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("repo/.git")).unwrap();
    let repo = root.join("repo").canonicalize().unwrap();
    let plan = root.join("plans").join(PID);
    std::fs::create_dir_all(plan.join("docs")).unwrap();
    std::fs::write(plan.join("docs/01-prd.md"), "prd").unwrap();
    std::fs::write(plan.join("docs/02-spec.md"), "spec").unwrap();
    std::fs::write(
        plan.join("plan.json"),
        json!({"v":1,"id":PID,"files":["01-prd.md","02-spec.md"],"repo":repo,
               "kind":kind,"mode":"reviewed"})
        .to_string(),
    )
    .unwrap();
    if let Some(e) = epic {
        std::fs::write(
            plan.join("result.json"),
            json!({"v":1,"epic_id":e,"task_ids":["t-1"]}).to_string(),
        )
        .unwrap();
    }
    // `validate` answers from the file: BROKEN makes it invalid (exit 1, JSON on stdout).
    let fake = root.join("fake-pas");
    std::fs::write(
        &fake,
        format!(
            r#"#!/bin/sh
d='{d}'
echo "$(pwd) $*" >> "$d/calls.log"
case "$1" in
scaffold|generate)
  out=""; prev=""
  for a in "$@"; do [ "$prev" = "--output" ] && out="$a"; prev="$a"; done
  if [ -f "$d/build.dot" ]; then mkdir -p "$(dirname "$out")"; cp "$d/build.dot" "$out"; fi
  cat "$d/build.out"; exit "$(cat "$d/build.code")";;
validate)
  if grep -q BROKEN "$2"; then
    echo '{{"v":1,"ok":true,"valid":false,"diagnostics":[{{"severity":"error","node_id":"n<1>","message":"missing <exit> node","fix":"add an exit"}},{{"severity":"warning","message":"slow"}}]}}'
    exit 1
  fi
  cat "$d/validate.out"; exit 0;;
run)
  rid=""; prev=""
  for a in "$@"; do [ "$prev" = "--run-id" ] && rid="$a"; prev="$a"; done
  code="$(cat "$d/run.code")"
  if [ "$code" != 0 ]; then cat "$d/run.out"; exit "$code"; fi
  echo "{{\"v\":1,\"run_id\":\"$rid\",\"started_at\":\"2026-01-01T00:00:00Z\",\"workdir\":\"/w\",\"pipeline_path\":\"/p.dot\",\"run_dir\":\"/nonexistent\"}}" >> "$d/runs.jsonl"
  exit 0;;
esac
exit 2
"#,
            d = root.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let state = AppState::new(root.join("runs.jsonl"));
    state.set_pas_exe(&fake);
    let token = state.csrf_token().as_str().to_string();
    let l = bind(0).await.unwrap();
    let addr = l.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(serve_on_with(l, state, async {
        let _ = rx.await;
    }));
    let e = Env {
        tmp,
        addr,
        token,
        _stop: tx,
    };
    e.set("build.dot", "digraph g { start -> done }\n");
    e.set("build.out", r#"{"v":1,"ok":true,"pipeline_path":"x"}"#);
    e.set("build.code", "0");
    e.set("validate.out", VALID);
    e.set("run.code", "0");
    e.set("run.out", "");
    e
}

async fn env() -> Env {
    env_kind("epic_pipeline", Some("epic-1")).await
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

async fn send(
    e: &Env,
    method: &str,
    path: &str,
    form: &[(&str, &str)],
    token: bool,
) -> (u16, String) {
    send_host(e, method, path, form, token, &e.addr.to_string()).await
}

async fn send_host(
    e: &Env,
    method: &str,
    path: &str,
    form: &[(&str, &str)],
    token: bool,
    host: &str,
) -> (u16, String) {
    let body: String = form
        .iter()
        .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
        .collect::<Vec<_>>()
        .join("&");
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n",
        body.len()
    );
    if token {
        head.push_str(&format!("X-CSRF-Token: {}\r\n", e.token));
    }
    head.push_str("\r\n");
    let mut s = TcpStream::connect(e.addr).await.unwrap();
    s.write_all(head.as_bytes()).await.unwrap();
    let _ = s.write_all(body.as_bytes()).await;
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf).await;
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status = text.split(' ').nth(1).unwrap().parse().unwrap();
    (status, text)
}

async fn build(e: &Env) -> (u16, String) {
    send(e, "POST", &format!("/plans/{PID}/pipeline"), &[], true).await
}
async fn save(e: &Env, dot: &str) -> (u16, String) {
    send(
        e,
        "PUT",
        &format!("/plans/{PID}/pipeline"),
        &[("dot", dot)],
        true,
    )
    .await
}
async fn launch(e: &Env, form: &[(&str, &str)]) -> (u16, String) {
    send(e, "POST", &format!("/plans/{PID}/launch"), form, true).await
}

fn launch_form(e: &Env) -> Vec<(String, String)> {
    vec![
        ("workdir".into(), e.repo().display().to_string()),
        ("max_budget_usd".into(), "12.5".into()),
        ("max_steps".into(), "40".into()),
    ]
}

async fn launch_with(e: &Env, extra: &[(&str, &str)]) -> (u16, String) {
    let base = launch_form(e);
    let mut form: Vec<(&str, &str)> = base.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    form.extend_from_slice(extra);
    launch(e, &form).await
}

fn button_disabled(body: &str) -> bool {
    let at = body.find("id=\"launch-button\"").expect("launch button");
    let tag_end = body[at..].find('>').unwrap();
    body[at..at + tag_end]
        .split_whitespace()
        .any(|t| t == "disabled")
}

// ---- AC1: Epic mode writes <repo>/pipelines/<epic-id>.dot and shows the graph

#[tokio::test]
async fn epic_mode_builds_with_scaffold_and_shows_the_graph() {
    let e = env().await;
    let (st, body) = build(&e).await;
    assert_eq!(st, 200, "{body}");
    let dot = e.dot("epic-1");
    assert_eq!(
        std::fs::read_to_string(&dot).unwrap(),
        "digraph g { start -> done }\n"
    );
    assert_eq!(
        e.calls_of("scaffold"),
        [format!(
            "{} scaffold epic-1 --output {} --json",
            e.repo().display(),
            dot.display()
        )]
    );
    assert!(body.contains("id=\"dot-src\""), "{body}");
    assert!(
        body.contains("start -&gt; done"),
        "textarea holds the DOT: {body}"
    );
    assert!(body.contains("digraph g"), "{body}");
    assert!(body.contains("viz-standalone.js"), "{body}");
    assert!(!button_disabled(&body), "valid Pipeline enables Launch");
    assert!(body.contains("Pipeline is valid."));
}

#[tokio::test]
async fn epic_mode_without_an_epic_is_refused_without_spawning() {
    let e = env_kind("epic_pipeline", None).await;
    let (st, body) = build(&e).await;
    assert_eq!(st, 409, "{body}");
    assert!(body.contains("create the Epic first"), "{body}");
    assert_eq!(e.calls(), "");
}

#[tokio::test]
async fn pipeline_only_uses_generate_with_every_doc_in_order() {
    let e = env_kind("pipeline", None).await;
    let (st, body) = build(&e).await;
    assert_eq!(st, 200, "{body}");
    let dot = e.dot("spec");
    assert!(dot.is_file());
    let want = format!(
        "{} generate --plan {p}/docs/01-prd.md --plan {p}/docs/02-spec.md --output {} --json",
        e.repo().display(),
        dot.display(),
        p = e.plan().display()
    );
    assert_eq!(e.calls_of("generate"), [want]);
}

#[tokio::test]
async fn invalid_pipeline_still_shows_the_editor_and_diagnostics() {
    let e = env().await;
    e.set("build.dot", "digraph BROKEN {}");
    e.set(
        "build.out",
        r#"{"v":1,"ok":false,"error":{"code":"invalid_pipeline","message":"written but has 1 validation error(s)"}}"#,
    );
    e.set("build.code", "1");
    let (st, body) = build(&e).await;
    assert_eq!(st, 200, "{body}");
    assert!(
        body.contains("written but has 1 validation error(s)"),
        "{body}"
    );
    assert!(body.contains("id=\"dot-text\""), "{body}");
    assert!(body.contains("Pipeline is not valid."), "{body}");
    assert!(button_disabled(&body));
}

#[tokio::test]
async fn other_build_failures_show_only_the_message() {
    let e = env().await;
    e.set("build.dot", "irrelevant");
    e.set(
        "build.out",
        r#"{"v":1,"ok":false,"error":{"code":"bd_failed","message":"epic not found"}}"#,
    );
    e.set("build.code", "1");
    let (st, body) = build(&e).await;
    assert_eq!(st, 502, "{body}");
    assert!(body.contains("epic not found (bd_failed)"), "{body}");
    assert!(!body.contains("id=\"dot-text\""), "{body}");
    assert!(e.calls_of("validate").is_empty());
}

#[tokio::test]
async fn a_build_in_progress_blocks_a_second_one() {
    let e = env().await;
    std::fs::write(e.plan().join("pipeline.pending"), "").unwrap();
    let (st, body) = build(&e).await;
    assert_eq!(st, 409, "{body}");
    assert!(body.contains("already being built"));
    assert_eq!(e.calls(), "");
    // The marker of a finished build is gone.
    std::fs::remove_file(e.plan().join("pipeline.pending")).unwrap();
    assert_eq!(build(&e).await.0, 200);
    assert!(!e.plan().join("pipeline.pending").exists());
}

#[tokio::test]
async fn the_saved_pipeline_can_be_reloaded() {
    let e = env().await;
    let path = format!("/plans/{PID}/pipeline");
    assert_eq!(send(&e, "GET", &path, &[], false).await.0, 409);
    build(&e).await;
    let (st, body) = send(&e, "GET", &path, &[], false).await;
    assert_eq!(st, 200);
    assert!(body.contains("digraph g"));
}

// ---- AC2: diagnostics with node ids; Launch disabled while valid=false

#[tokio::test]
async fn diagnostics_list_node_ids_and_disable_launch() {
    let e = env().await;
    build(&e).await;
    let (st, body) = save(&e, "digraph BROKEN {}").await;
    assert_eq!(st, 200, "{body}");
    assert!(body.contains("Pipeline is not valid."));
    // Hostile text is escaped.
    assert!(body.contains("<code>n&lt;1&gt;</code>"), "{body}");
    assert!(body.contains("missing &lt;exit&gt; node"), "{body}");
    assert!(body.contains("Fix: add an exit"), "{body}");
    assert!(body.contains("<strong>warning</strong>"), "{body}");
    assert!(!body.contains("<exit>"));
    assert!(button_disabled(&body));
}

#[tokio::test]
async fn launch_is_refused_server_side_while_invalid() {
    let e = env().await;
    build(&e).await;
    std::fs::write(e.dot("epic-1"), "digraph BROKEN {}").unwrap();
    let (st, body) = launch_with(&e, &[]).await;
    assert_eq!(st, 409, "{body}");
    assert!(body.contains("not valid"), "{body}");
    assert!(!body.contains("hx-redirect"));
    assert!(e.calls_of("run").is_empty(), "{}", e.calls());
}

#[tokio::test]
async fn a_pipeline_that_cannot_be_checked_disables_launch() {
    let e = env().await;
    build(&e).await;
    e.set("validate.out", "garbage");
    let (st, body) = send(&e, "GET", &format!("/plans/{PID}/pipeline"), &[], false).await;
    assert_eq!(st, 200);
    assert!(body.contains("Could not check the Pipeline"), "{body}");
    assert!(button_disabled(&body));
}

// ---- AC3: saving an edited DOT re-validates

#[tokio::test]
async fn saving_writes_the_text_and_revalidates() {
    let e = env().await;
    build(&e).await;
    let (_, body) = save(&e, "digraph BROKEN {}").await;
    assert!(body.contains("Pipeline is not valid."));
    assert_eq!(
        std::fs::read_to_string(e.dot("epic-1")).unwrap(),
        "digraph BROKEN {}"
    );
    let fixed = "digraph fixed { a -> b }\n";
    let (st, body) = save(&e, fixed).await;
    assert_eq!(st, 200, "{body}");
    assert_eq!(std::fs::read_to_string(e.dot("epic-1")).unwrap(), fixed);
    assert!(body.contains("Pipeline is valid."), "{body}");
    assert!(!body.contains("missing &lt;exit&gt;"));
    assert!(!button_disabled(&body));
    // build (validate) + two saves = three validations.
    assert_eq!(e.calls_of("validate").len(), 3);
    let leftovers: Vec<_> = std::fs::read_dir(e.repo().join("pipelines"))
        .unwrap()
        .map(|d| d.unwrap().file_name())
        .collect();
    assert_eq!(leftovers.len(), 1, "no temp file left: {leftovers:?}");
}

#[tokio::test]
async fn save_rejects_empty_and_oversized_text_and_a_missing_pipeline() {
    let e = env().await;
    let (st, body) = save(&e, "digraph {}").await;
    assert_eq!(st, 409, "{body}");
    assert!(body.contains("build it first"));
    build(&e).await;
    let before = std::fs::read_to_string(e.dot("epic-1")).unwrap();
    for text in ["", "  \n "] {
        assert_eq!(save(&e, text).await.0, 400);
    }
    let big = "a".repeat(1024 * 1024 + 1);
    let (st, body) = save(&e, &big).await;
    assert_eq!(st, 400, "{}", &body[..body.len().min(300)]);
    assert_eq!(std::fs::read_to_string(e.dot("epic-1")).unwrap(), before);
    // Exactly 1 MiB is accepted.
    let edge = "a".repeat(1024 * 1024);
    assert_eq!(save(&e, &edge).await.0, 200);
}

// ---- AC4: Launch passes budget and steps and redirects

#[tokio::test]
async fn launch_starts_a_run_with_budget_and_steps_and_redirects() {
    let e = env().await;
    build(&e).await;
    let (st, body) = launch_with(&e, &[]).await;
    assert_eq!(st, 200, "{body}");
    let call = e.calls_of("run");
    assert_eq!(call.len(), 1, "{}", e.calls());
    let call = &call[0];
    let dot = e.dot("epic-1");
    assert!(
        call.starts_with(&format!(
            "{} run {} --run-id ",
            e.repo().display(),
            dot.display()
        )),
        "{call}"
    );
    for want in [
        "--fresh",
        "--json",
        &format!("--workdir {}", e.repo().display()),
        "--max-budget-usd 12.5",
        "--max-steps 40",
    ] {
        assert!(call.contains(want), "{want} in {call}");
    }
    assert!(!call.contains("--allow-shared-workdir"));
    let id = call
        .split("--run-id ")
        .nth(1)
        .unwrap()
        .split(' ')
        .next()
        .unwrap();
    let uuid = attractor_journal::parse_run_id(id).expect("a valid run id");
    assert_eq!(uuid, id);
    let head = body.to_lowercase();
    assert!(head.contains(&format!("hx-redirect: /runs/{id}")), "{body}");
    // The Run page exists at once, before any watcher poll.
    let (st, page) = send(&e, "GET", &format!("/runs/{id}"), &[], false).await;
    assert_eq!(st, 200, "{}", &page[..page.len().min(400)]);
}

#[tokio::test]
async fn bad_launch_values_are_refused_without_spawning() {
    let e = env().await;
    build(&e).await;
    let wd = e.repo().display().to_string();
    let cases: Vec<Vec<(&str, &str)>> = vec![
        vec![
            ("workdir", "relative"),
            ("max_budget_usd", "1"),
            ("max_steps", "1"),
        ],
        vec![
            ("workdir", &wd),
            ("max_budget_usd", "0"),
            ("max_steps", "1"),
        ],
        vec![
            ("workdir", &wd),
            ("max_budget_usd", "NaN"),
            ("max_steps", "1"),
        ],
        vec![
            ("workdir", &wd),
            ("max_budget_usd", "1"),
            ("max_steps", "0"),
        ],
        vec![
            ("workdir", &wd),
            ("max_budget_usd", "1"),
            ("max_steps", "2.5"),
        ],
        vec![],
    ];
    for form in cases {
        let (st, body) = launch(&e, &form).await;
        assert_eq!(st, 400, "{form:?}: {body}");
    }
    assert!(e.calls_of("run").is_empty());
    // The typed values stay on screen.
    let (st, body) = launch(
        &e,
        &[
            ("workdir", &wd),
            ("max_budget_usd", "0"),
            ("max_steps", "77"),
        ],
    )
    .await;
    assert_eq!(st, 400);
    assert!(body.contains("value=\"77\""), "{body}");
}

// ---- AC5: lock errors, and the shared-workdir override

#[tokio::test]
async fn worktree_lock_is_shown_with_the_form_kept() {
    let e = env().await;
    build(&e).await;
    let msg = "another Run is active in this git worktree (pid 4242, run 0192abcd); pass --allow-shared-workdir to run anyway";
    e.set(
        "run.out",
        &format!(r#"{{"v":1,"ok":false,"error":{{"code":"worktree_locked","message":"{msg}"}}}}"#),
    );
    e.set("run.code", "6");
    let (st, body) = launch_with(&e, &[]).await;
    assert_eq!(st, 409, "{body}");
    assert!(body.contains("pid 4242, run 0192abcd"), "{body}");
    assert!(body.contains("Run even if another Run is active in this worktree"));
    assert!(body.contains("value=\"12.5\""));
    assert!(body.contains("value=\"40\""));
    assert!(!body.to_lowercase().contains("hx-redirect"));
    assert!(!button_disabled(&body));
}

#[tokio::test]
async fn shared_workdir_checkbox_passes_the_override_and_redirects() {
    let e = env().await;
    build(&e).await;
    let (st, body) = launch_with(&e, &[("allow_shared", "on")]).await;
    assert_eq!(st, 200, "{body}");
    let call = &e.calls_of("run")[0];
    assert!(call.ends_with("--allow-shared-workdir"), "{call}");
    assert!(body.to_lowercase().contains("hx-redirect: /runs/"));
}

#[tokio::test]
async fn pipeline_lock_has_no_override() {
    let e = env().await;
    build(&e).await;
    e.set(
        "run.out",
        r#"{"v":1,"ok":false,"error":{"code":"pipeline_locked","message":"this Pipeline already has an active Run (pid 9)"}}"#,
    );
    e.set("run.code", "5");
    let (st, body) = launch_with(&e, &[("allow_shared", "on")]).await;
    assert_eq!(st, 409, "{body}");
    assert!(
        body.contains("this Pipeline already has an active Run (pid 9)"),
        "{body}"
    );
    assert!(!body.contains("Tick"), "{body}");
    assert!(!body.to_lowercase().contains("hx-redirect"));
}

#[tokio::test]
async fn other_start_failures_are_shown_as_bad_gateway() {
    let e = env().await;
    build(&e).await;
    e.set("run.out", "boom: not json");
    e.set("run.code", "3");
    let (st, body) = launch_with(&e, &[]).await;
    assert_eq!(st, 502, "{body}");
    assert!(body.contains("boom: not json"), "{body}");
}

// ---- Guards

#[tokio::test]
async fn guards_csrf_host_and_bad_ids() {
    let e = env().await;
    let p = format!("/plans/{PID}");
    for (m, path) in [
        ("POST", format!("{p}/pipeline")),
        ("PUT", format!("{p}/pipeline")),
        ("POST", format!("{p}/launch")),
    ] {
        let (st, _) = send(&e, m, &path, &[("dot", "x")], false).await;
        assert_eq!(st, 403, "{m} {path} without token");
        let (st, _) = send_host(&e, m, &path, &[("dot", "x")], true, "evil.example").await;
        assert_eq!(st, 403, "{m} {path} with a foreign Host");
    }
    let (st, _) = send_host(
        &e,
        "GET",
        &format!("{p}/pipeline"),
        &[],
        false,
        "evil.example",
    )
    .await;
    assert_eq!(st, 403);
    assert_eq!(e.calls(), "");
    for bad in ["..", "abc", &"A".repeat(32), &"g".repeat(32)] {
        for (m, tail) in [
            ("POST", "pipeline"),
            ("PUT", "pipeline"),
            ("POST", "launch"),
            ("GET", "pipeline"),
        ] {
            let (st, _) = send(
                &e,
                m,
                &format!("/plans/{bad}/{tail}"),
                &[("dot", "x")],
                true,
            )
            .await;
            assert_eq!(st, 404, "{m} {bad}/{tail}");
        }
    }
    assert_eq!(e.calls(), "");
}

#[tokio::test]
async fn save_and_launch_need_a_built_pipeline() {
    let e = env().await;
    let (st, _) = launch_with(&e, &[]).await;
    assert_eq!(st, 409);
    assert_eq!(e.calls(), "");
}
