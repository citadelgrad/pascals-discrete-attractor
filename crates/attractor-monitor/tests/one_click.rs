//! One-click mode over HTTP with a fake `pas` (attractor-ino.43).

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use attractor_monitor::state::AppState;
use attractor_monitor::{bind, serve_on_with};
use serde_json::{json, Value};
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
    fn plan(&self) -> PathBuf {
        self.root().join("plans").join(PID)
    }
    fn repo(&self) -> PathBuf {
        self.root().join("repo").canonicalize().unwrap()
    }
    fn calls(&self) -> String {
        std::fs::read_to_string(self.root().join("calls.log")).unwrap_or_default()
    }
    /// The subcommand of every `pas` call, in order.
    fn verbs(&self) -> Vec<String> {
        self.calls()
            .lines()
            .map(|l| l.split(' ').nth(1).unwrap_or("").to_string())
            .collect()
    }
    fn calls_of(&self, verb: &str) -> Vec<String> {
        let m = format!(" {verb} ");
        self.calls()
            .lines()
            .filter(|l| l.contains(&m))
            .map(String::from)
            .collect()
    }
    fn set(&self, name: &str, text: &str) {
        std::fs::write(self.root().join(name), text).unwrap();
    }
    fn runs(&self) -> String {
        std::fs::read_to_string(self.root().join("runs.jsonl")).unwrap_or_default()
    }
}

fn proposal() -> Value {
    json!({"v":1,"epic":{"title":"Epic title","description":"Epic body"},
        "tasks":[
            {"title":"Alpha","type":"task","priority":"P2","description":"a"},
            {"title":"Beta","type":"task","priority":"P1","description":"b"}],
        "dependencies":[{"blocked":1,"blocker":0}]})
}

async fn env_with(kind: &str, mode: &str) -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("repo/.git")).unwrap();
    let repo = root.join("repo").canonicalize().unwrap();
    let plan = root.join("plans").join(PID);
    std::fs::create_dir_all(plan.join("docs")).unwrap();
    for n in ["01-a.md", "02-b.md", "03-c.md"] {
        std::fs::write(plan.join("docs").join(n), n).unwrap();
    }
    std::fs::write(
        plan.join("plan.json"),
        json!({"v":1,"id":PID,"files":["01-a.md","02-b.md","03-c.md"],"repo":repo,
               "kind":kind,"mode":mode})
        .to_string(),
    )
    .unwrap();
    let fake = root.join("fake-pas");
    std::fs::write(
        &fake,
        format!(
            r#"#!/bin/sh
d='{d}'
echo "$(pwd) $*" >> "$d/calls.log"
case "$1" in
decompose)
  case "$*" in
  *--dry-run*) cat "$d/gen.out"; exit "$(cat "$d/gen.code")";;
  *) cat "$d/create.out"; exit "$(cat "$d/create.code")";;
  esac;;
scaffold|generate)
  out=""; prev=""
  for a in "$@"; do [ "$prev" = "--output" ] && out="$a"; prev="$a"; done
  mkdir -p "$(dirname "$out")"; cp "$d/build.dot" "$out"
  cat "$d/build.out"; exit "$(cat "$d/build.code")";;
validate)
  if grep -q BROKEN "$2"; then
    echo '{{"v":1,"ok":true,"valid":false,"diagnostics":[{{"severity":"error","node_id":"n1","message":"missing exit"}}]}}'
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
    e.set(
        "gen.out",
        &json!({"v":1,"ok":true,"proposal":proposal()}).to_string(),
    );
    e.set("gen.code", "0");
    e.set(
        "create.out",
        r#"{"v":1,"ok":true,"epic_id":"epic-1","task_ids":["t-1","t-2"]}"#,
    );
    e.set("create.code", "0");
    e.set("build.dot", "digraph g { start -> done }\n");
    e.set("build.out", r#"{"v":1,"ok":true}"#);
    e.set("build.code", "0");
    e.set("validate.out", VALID);
    e.set("run.code", "0");
    e.set("run.out", "");
    e
}

async fn env() -> Env {
    env_with("epic_pipeline", "one_click").await
}

async fn send_host(e: &Env, method: &str, path: &str, token: bool, host: &str) -> (u16, String) {
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Length: 0\r\n"
    );
    if token {
        head.push_str(&format!("X-CSRF-Token: {}\r\n", e.token));
    }
    head.push_str("\r\n");
    let mut s = TcpStream::connect(e.addr).await.unwrap();
    s.write_all(head.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf).await;
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status = text.split(' ').nth(1).unwrap().parse().unwrap();
    (status, text)
}

async fn one_click_at(e: &Env, pid: &str) -> (u16, String) {
    send_host(
        e,
        "POST",
        &format!("/plans/{pid}/one-click"),
        true,
        &e.addr.to_string(),
    )
    .await
}

async fn one_click(e: &Env) -> (u16, String) {
    one_click_at(e, PID).await
}

fn redirect(text: &str) -> Option<String> {
    text.lines()
        .find_map(|l| {
            l.strip_prefix("hx-redirect: ")
                .or_else(|| l.strip_prefix("HX-Redirect: "))
        })
        .map(|s| s.trim().to_string())
}

// ---- AC1: one click on 3 files ends on the Run page of an active Run

#[tokio::test]
async fn one_click_three_files_reach_an_active_run() {
    let e = env().await;
    let (st, text) = one_click(&e).await;
    assert_eq!(st, 200, "{text}");
    let to = redirect(&text).unwrap_or_else(|| panic!("no HX-Redirect: {text}"));
    let rid = to.strip_prefix("/runs/").expect("redirect to a Run page");
    assert!(e.runs().contains(rid), "the Run is in the Run Index");
    assert_eq!(
        e.verbs(),
        [
            "decompose",
            "decompose",
            "scaffold",
            "validate",
            "validate",
            "run"
        ]
    );
    let p = e.plan().display().to_string();
    let first = e.calls_of("decompose").remove(0);
    assert!(
        first.ends_with(&format!(
            "decompose --plan {p}/docs/01-a.md --plan {p}/docs/02-b.md --plan {p}/docs/03-c.md --dry-run --json"
        )),
        "{first}"
    );
    assert!(e.calls_of("decompose")[1].contains("--from-proposal"));
    let run = e.calls_of("run").remove(0);
    assert!(run.contains("--max-budget-usd 200"), "{run}");
    assert!(run.contains("--max-steps 200"), "{run}");
    assert!(
        run.contains(&format!("--workdir {}", e.repo().display())),
        "{run}"
    );
    assert!(!run.contains("--allow-shared-workdir"), "{run}");
    let result: Value =
        serde_json::from_slice(&std::fs::read(e.plan().join("result.json")).unwrap()).unwrap();
    assert_eq!(result["epic_id"], "epic-1");
    assert!(e.repo().join("pipelines/epic-1.dot").is_file());
    let (st, _) = send_host(&e, "GET", &to, true, &e.addr.to_string()).await;
    assert_eq!(st, 200, "the Run page answers at once");
    assert!(!e.plan().join("one-click.pending").exists());
}

// ---- AC2: validation fails, stop at the Pipeline check, no Run

#[tokio::test]
async fn invalid_pipeline_stops_at_the_check_and_starts_no_run() {
    let e = env().await;
    e.set("build.dot", "digraph BROKEN {}");
    let (st, text) = one_click(&e).await;
    assert_eq!(st, 200, "{text}");
    assert!(redirect(&text).is_none(), "{text}");
    assert!(text.contains(r#"<div id="pipeline">"#), "{text}");
    assert!(text.contains("Pipeline is not valid."), "{text}");
    assert!(
        text.contains("<code>n1</code>"),
        "diagnostic with node id: {text}"
    );
    assert!(text.contains("missing exit"), "{text}");
    assert!(text.contains("disabled"), "Launch is disabled: {text}");
    assert!(e.calls_of("run").is_empty());
    assert_eq!(e.runs(), "");
}

#[tokio::test]
async fn a_pipeline_pas_refuses_as_invalid_stops_at_the_check() {
    let e = env().await;
    e.set("build.dot", "digraph BROKEN {}");
    e.set(
        "build.out",
        r#"{"v":1,"ok":false,"error":{"code":"invalid_pipeline","message":"written but has 1 validation error(s)"}}"#,
    );
    e.set("build.code", "1");
    let (st, text) = one_click(&e).await;
    assert_eq!(st, 200, "{text}");
    assert!(
        text.contains("written but has 1 validation error(s)"),
        "{text}"
    );
    assert!(e.calls_of("run").is_empty());
}

#[tokio::test]
async fn a_failed_build_stops_with_a_rebuild_button() {
    let e = env().await;
    e.set(
        "build.out",
        r#"{"v":1,"ok":false,"error":{"code":"bd_failed","message":"epic not found"}}"#,
    );
    e.set("build.code", "1");
    let (st, text) = one_click(&e).await;
    assert_eq!(st, 502, "{text}");
    assert!(text.contains("epic not found (bd_failed)"), "{text}");
    assert!(text.contains(r#"<div id="pipeline">"#), "{text}");
    assert!(text.contains("Rebuild Pipeline"), "{text}");
    assert!(e.calls_of("run").is_empty());
}

// ---- AC3: decompose fails, stop at the Proposal step, no Epic

#[tokio::test]
async fn decompose_failure_stops_at_the_proposal() {
    let e = env().await;
    e.set(
        "gen.out",
        r#"{"v":1,"ok":false,"error":{"code":"model_failed","message":"model said no"}}"#,
    );
    e.set("gen.code", "1");
    let (st, text) = one_click(&e).await;
    assert_eq!(st, 502, "{text}");
    assert!(text.contains(r#"<div id="proposal">"#), "{text}");
    assert!(text.contains("model said no (model_failed)"), "{text}");
    assert_eq!(e.verbs(), ["decompose"]);
    assert!(!e.plan().join("result.json").exists());
    assert!(redirect(&text).is_none());
}

#[tokio::test]
async fn decompose_with_no_output_stops_at_the_proposal() {
    let e = env().await;
    e.set("gen.out", "");
    e.set("gen.code", "1");
    let (st, text) = one_click(&e).await;
    assert_eq!(st, 502, "{text}");
    assert!(text.contains(r#"<div id="proposal">"#), "{text}");
    assert_eq!(e.verbs(), ["decompose"]);
    assert!(!e.plan().join("result.json").exists());
}

#[tokio::test]
async fn a_proposal_with_a_cycle_stops_before_creating_the_epic() {
    let e = env().await;
    let mut p = proposal();
    p["dependencies"] = json!([{"blocked":1,"blocker":0},{"blocked":0,"blocker":1}]);
    e.set(
        "gen.out",
        &json!({"v":1,"ok":true,"proposal":p}).to_string(),
    );
    let (st, text) = one_click(&e).await;
    assert_eq!(st, 400, "{text}");
    assert!(text.contains(r#"<div id="proposal">"#), "{text}");
    assert!(
        text.contains("proposal-form"),
        "the editor is shown: {text}"
    );
    assert_eq!(e.verbs(), ["decompose"], "only the dry run ran");
    assert!(!e.plan().join("result.json").exists());
}

#[tokio::test]
async fn a_failed_epic_creation_stops_at_the_proposal_without_a_result() {
    let e = env().await;
    e.set(
        "create.out",
        r#"{"v":1,"ok":false,"error":{"code":"bd_failed","message":"bd broke"}}"#,
    );
    e.set("create.code", "1");
    let (st, text) = one_click(&e).await;
    assert_eq!(st, 502, "{text}");
    assert!(text.contains(r#"<div id="proposal">"#), "{text}");
    assert!(text.contains("bd broke (bd_failed)"), "{text}");
    assert_eq!(e.verbs(), ["decompose", "decompose"]);
    assert!(!e.plan().join("result.json").exists());
    assert!(!e.plan().join("epic.pending").exists());
}

// ---- AC4: Pipeline-only one-click runs no Epic step, so no bd

#[tokio::test]
async fn pipeline_only_one_click_never_decomposes_or_scaffolds() {
    let e = env_with("pipeline", "one_click").await;
    let (st, text) = one_click(&e).await;
    assert_eq!(st, 200, "{text}");
    assert!(redirect(&text).is_some(), "{text}");
    assert_eq!(e.verbs(), ["generate", "validate", "validate", "run"]);
    assert!(!e.plan().join("result.json").exists());
}

// ---- guards and edge cases

#[tokio::test]
async fn a_reviewed_plan_is_refused_without_spawning() {
    let e = env_with("epic_pipeline", "reviewed").await;
    let (st, text) = one_click(&e).await;
    assert_eq!(st, 409, "{text}");
    assert_eq!(e.calls(), "");
}

#[tokio::test]
async fn retry_after_a_failed_build_reuses_the_epic() {
    let e = env().await;
    e.set(
        "build.out",
        r#"{"v":1,"ok":false,"error":{"code":"bd_failed","message":"boom"}}"#,
    );
    e.set("build.code", "1");
    assert_eq!(one_click(&e).await.0, 502);
    e.set("build.out", r#"{"v":1,"ok":true}"#);
    e.set("build.code", "0");
    let (st, text) = one_click(&e).await;
    assert_eq!(st, 200, "{text}");
    assert!(redirect(&text).is_some(), "{text}");
    let from = e
        .calls_of("decompose")
        .iter()
        .filter(|l| l.contains("--from-proposal"))
        .count();
    assert_eq!(from, 1, "one Epic in total");
    assert_eq!(
        e.calls_of("decompose").len(),
        2,
        "the second attempt starts at Build"
    );
    assert_eq!(e.calls_of("scaffold").len(), 2);
}

#[tokio::test]
async fn a_worktree_lock_stops_with_the_launch_form() {
    let e = env().await;
    e.set("run.code", "6");
    e.set(
        "run.out",
        r#"{"v":1,"ok":false,"error":{"code":"worktree_locked","message":"another Run is active"}}"#,
    );
    let (st, text) = one_click(&e).await;
    assert_eq!(st, 409, "{text}");
    assert!(redirect(&text).is_none());
    assert!(text.contains(r#"<div id="pipeline">"#), "{text}");
    assert!(text.contains("another Run is active"), "{text}");
    assert!(text.contains("Run even if another Run is active"), "{text}");
    assert_eq!(e.runs(), "");
}

#[tokio::test]
async fn a_second_one_click_is_blocked_while_one_is_running() {
    let e = env().await;
    std::fs::write(e.plan().join("one-click.pending"), "").unwrap();
    let (st, text) = one_click(&e).await;
    assert_eq!(st, 409, "{text}");
    assert!(text.contains("already in progress"), "{text}");
    assert_eq!(e.calls(), "");
}

#[tokio::test]
async fn guards_csrf_host_and_bad_ids() {
    let e = env().await;
    let path = format!("/plans/{PID}/one-click");
    let (st, _) = send_host(&e, "POST", &path, false, &e.addr.to_string()).await;
    assert_eq!(st, 403, "no token");
    let (st, _) = send_host(&e, "POST", &path, true, "evil.example").await;
    assert_eq!(st, 403, "foreign Host");
    assert_eq!(e.calls(), "");
    for bad in ["..", "abc", &"A".repeat(32), &"g".repeat(32)] {
        assert_eq!(one_click_at(&e, bad).await.0, 404, "{bad}");
    }
    assert_eq!(e.calls(), "");
}
