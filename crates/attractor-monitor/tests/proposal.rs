//! Proposal editor and Create Epic over HTTP with a fake `pas` (attractor-ino.41).

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use attractor_monitor::state::AppState;
use attractor_monitor::{bind, serve_on_with};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const PID: &str = "0123456789abcdef0123456789abcdef";

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
    fn set(&self, name: &str, text: &str) {
        std::fs::write(self.root().join(name), text).unwrap();
    }
    fn stored(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.plan().join("proposal.json")).unwrap()).unwrap()
    }
    fn seen(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.root().join("seen.json")).unwrap()).unwrap()
    }
}

fn proposal() -> Value {
    json!({"v":1,"epic":{"title":"Epic title","description":"Epic body"},
        "tasks":[
            {"title":"Alpha task","type":"task","priority":"P2","description":"a","acceptance":"acc a"},
            {"title":"Beta task","type":"task","priority":"P1","description":"b"},
            {"title":"Gamma task","type":"feature","priority":"P2","description":"c"}],
        "dependencies":[{"blocked":1,"blocker":0},{"blocked":2,"blocker":1}]})
}

fn ok_gen() -> String {
    json!({"v":1,"ok":true,"proposal":proposal()}).to_string()
}

async fn env_kind(kind: &str) -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("repo/.git")).unwrap();
    let repo = root.join("repo").canonicalize().unwrap();
    let plan = root.join("plans").join(PID);
    std::fs::create_dir_all(plan.join("docs")).unwrap();
    std::fs::write(plan.join("docs/01-b.md"), "second file, first place").unwrap();
    std::fs::write(plan.join("docs/02-a.md"), "first file, second place").unwrap();
    std::fs::write(
        plan.join("plan.json"),
        json!({"v":1,"id":PID,"files":["01-b.md","02-a.md"],"repo":repo,
               "kind":kind,"mode":"reviewed"})
        .to_string(),
    )
    .unwrap();
    let fake = root.join("fake-pas");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\necho \"$(pwd) $*\" >> '{d}/calls.log'\n\
             case \"$*\" in\n\
             *--dry-run*) cat '{d}/gen.out'; exit \"$(cat '{d}/gen.code')\";;\n\
             *--from-proposal*) cp \"$3\" '{d}/seen.json'; cat '{d}/create.out'; exit \"$(cat '{d}/create.code')\";;\n\
             esac\nexit 2\n",
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
    e.set("gen.out", &ok_gen());
    e.set("gen.code", "0");
    e.set(
        "create.out",
        r#"{"v":1,"ok":true,"epic_id":"epic-1","task_ids":["t-1","t-2"]}"#,
    );
    e.set("create.code", "0");
    e
}

async fn env() -> Env {
    env_kind("epic_pipeline").await
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
    let body: String = form
        .iter()
        .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
        .collect::<Vec<_>>()
        .join("&");
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n",
        e.addr,
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

async fn generate(e: &Env) -> (u16, String) {
    send(e, "POST", &format!("/plans/{PID}/proposal"), &[], true).await
}
async fn save(e: &Env, form: &[(&str, &str)]) -> (u16, String) {
    send(e, "PUT", &format!("/plans/{PID}/proposal"), form, true).await
}
async fn create(e: &Env, form: &[(&str, &str)]) -> (u16, String) {
    send(e, "POST", &format!("/plans/{PID}/epic"), form, true).await
}

#[tokio::test]
async fn generate_shows_epic_tasks_and_dependencies() {
    let e = env().await;
    let (st, body) = generate(&e).await;
    assert_eq!(st, 200, "{body}");
    for t in [
        "Epic title",
        "Epic body",
        "Alpha task",
        "Beta task",
        "Gamma task",
    ] {
        assert!(body.contains(t), "missing {t}: {body}");
    }
    assert!(
        body.contains("Task 2 &quot;Beta task&quot; blocked by Task 1 &quot;Alpha task&quot;"),
        "{body}"
    );
    assert!(
        body.contains("Task 3 &quot;Gamma task&quot; blocked by Task 2 &quot;Beta task&quot;"),
        "{body}"
    );
    assert_eq!(e.stored(), proposal());
    // Files in stored order, run in the Plan's repository.
    let plan = e.plan();
    let want = format!(
        "{} decompose --plan {p}/docs/01-b.md --plan {p}/docs/02-a.md --dry-run --json",
        e.repo().display(),
        p = plan.display()
    );
    assert_eq!(e.calls().trim(), want);
}

#[tokio::test]
async fn saved_proposal_can_be_reloaded() {
    let e = env().await;
    let (st, _) = send(&e, "GET", &format!("/plans/{PID}/proposal"), &[], false).await;
    assert_eq!(st, 404);
    generate(&e).await;
    let (st, body) = send(&e, "GET", &format!("/plans/{PID}/proposal"), &[], false).await;
    assert_eq!(st, 200);
    assert!(body.contains("Alpha task"));
}

fn edit_form() -> Vec<(&'static str, &'static str)> {
    vec![
        ("epic_title", "Epic title"),
        ("epic_description", "Epic body"),
        ("t0_title", "Alpha task"),
        ("t1_title", "Beta task"),
        ("t2_title", "Gamma renamed"),
        ("dep", "1:0"),
        ("dep", "2:1"),
    ]
}

#[tokio::test]
async fn remove_and_edit_then_create_epic() {
    let e = env().await;
    generate(&e).await;
    let mut form = edit_form();
    form.push(("remove", "1"));
    let (st, body) = save(&e, &form).await;
    assert_eq!(st, 200, "{body}");
    let (st, body) = create(&e, &[]).await;
    assert_eq!(st, 200, "{body}");
    assert!(body.contains("epic-1") && body.contains("t-2"), "{body}");
    let seen = e.seen();
    let titles: Vec<&str> = seen["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, ["Alpha task", "Gamma renamed"]);
    // Optional fields the form did not show survive the edit.
    assert_eq!(seen["tasks"][0]["acceptance"], "acc a");
    let result: Value =
        serde_json::from_slice(&std::fs::read(e.plan().join("result.json")).unwrap()).unwrap();
    assert_eq!(
        result,
        json!({"v":1,"epic_id":"epic-1","task_ids":["t-1","t-2"]})
    );
    let calls = e.calls();
    assert!(
        calls
            .lines()
            .last()
            .unwrap()
            .starts_with(&e.repo().display().to_string()),
        "{calls}"
    );
    assert!(calls.contains("decompose --from-proposal"));
}

#[tokio::test]
async fn create_saves_the_form_on_screen_first() {
    let e = env().await;
    generate(&e).await;
    let mut form = edit_form();
    form.push(("remove", "0"));
    let (st, body) = create(&e, &form).await;
    assert_eq!(st, 200, "{body}");
    let seen = e.seen();
    assert_eq!(seen["tasks"].as_array().unwrap().len(), 2);
    assert_eq!(seen["tasks"][1]["title"], "Gamma renamed");
    assert_eq!(seen["dependencies"], json!([{"blocked":1,"blocker":0}]));
}

#[tokio::test]
async fn removing_a_task_removes_its_dependencies_from_the_saved_file() {
    let e = env().await;
    generate(&e).await;
    let mut form = edit_form();
    form.push(("remove", "1"));
    let (st, _) = save(&e, &form).await;
    assert_eq!(st, 200);
    // Both dependencies mentioned Task 2 (index 1): none remain.
    assert_eq!(e.stored()["dependencies"], json!([]));
    assert_eq!(e.stored()["tasks"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn cycle_is_rejected_on_save_naming_tasks() {
    let e = env().await;
    generate(&e).await;
    let before = std::fs::read(e.plan().join("proposal.json")).unwrap();
    let mut form = edit_form();
    form.push(("dep", "0:2"));
    let (st, body) = save(&e, &form).await;
    assert_eq!(st, 400, "{body}");
    for t in [
        "Task 1 &quot;Alpha task&quot;",
        "Task 2 &quot;Beta task&quot;",
        "Task 3 &quot;Gamma renamed&quot;",
    ] {
        assert!(body.contains(t), "missing {t}: {body}");
    }
    assert!(body.contains("cycle"));
    assert_eq!(
        std::fs::read(e.plan().join("proposal.json")).unwrap(),
        before
    );
}

#[tokio::test]
async fn a_stored_cycle_is_never_created() {
    let e = env().await;
    let mut p = proposal();
    p["dependencies"] = json!([{"blocked":0,"blocker":1},{"blocked":1,"blocker":0}]);
    e.set(
        "gen.out",
        &json!({"v":1,"ok":true,"proposal":p}).to_string(),
    );
    let (st, body) = generate(&e).await;
    assert_eq!(st, 200);
    assert!(body.contains("cycle"), "warning is shown: {body}");
    let calls_before = e.calls();
    let (st, body) = create(&e, &[]).await;
    assert_eq!(st, 400, "{body}");
    assert!(body.contains("cycle"));
    assert_eq!(e.calls(), calls_before, "pas was not called again");
    assert!(!e.plan().join("result.json").exists());
    assert!(!e.plan().join("epic.pending").exists());
}

#[tokio::test]
async fn generate_ok_false_shows_message_and_creates_nothing() {
    let e = env().await;
    e.set(
        "gen.out",
        r#"{"v":1,"ok":false,"error":{"code":"llm_failed","message":"the model said no"}}"#,
    );
    e.set("gen.code", "1");
    let (st, body) = generate(&e).await;
    assert_eq!(st, 502);
    assert!(body.contains("the model said no"), "{body}");
    assert!(!e.plan().join("proposal.json").exists());
    assert!(!e.plan().join("result.json").exists());
}

#[tokio::test]
async fn generate_without_output_shows_a_notice_and_creates_nothing() {
    let e = env().await;
    e.set("gen.out", "");
    e.set("gen.code", "1");
    let (st, body) = generate(&e).await;
    assert_eq!(st, 502);
    assert!(body.contains("no usable result"), "{body}");
    assert!(!e.plan().join("proposal.json").exists());
}

#[tokio::test]
async fn create_ok_false_shows_message_and_writes_no_result() {
    let e = env().await;
    generate(&e).await;
    e.set(
        "create.out",
        r#"{"v":1,"ok":false,"error":{"code":"beads_failed","message":"bd exploded"}}"#,
    );
    e.set("create.code", "1");
    let (st, body) = create(&e, &[]).await;
    assert_eq!(st, 502);
    assert!(body.contains("bd exploded"), "{body}");
    assert!(!e.plan().join("result.json").exists());
    assert!(
        !e.plan().join("epic.pending").exists(),
        "a retry is allowed"
    );
    // The retry works once decompose succeeds.
    e.set(
        "create.out",
        r#"{"v":1,"ok":true,"epic_id":"epic-9","task_ids":[]}"#,
    );
    e.set("create.code", "0");
    let (st, _) = create(&e, &[]).await;
    assert_eq!(st, 200);
}

#[tokio::test]
async fn second_create_does_not_spawn_again() {
    let e = env().await;
    generate(&e).await;
    assert_eq!(create(&e, &[]).await.0, 200);
    let calls = e.calls();
    let (st, body) = create(&e, &[]).await;
    assert_eq!(st, 409);
    assert!(body.contains("epic-1"));
    assert_eq!(e.calls(), calls);
}

#[tokio::test]
async fn a_pending_create_blocks_a_second_one() {
    let e = env().await;
    generate(&e).await;
    std::fs::write(e.plan().join("epic.pending"), "").unwrap();
    let calls = e.calls();
    assert_eq!(create(&e, &[]).await.0, 409);
    assert_eq!(e.calls(), calls);
    assert!(e.plan().join("epic.pending").exists(), "not ours to remove");
}

#[tokio::test]
async fn guards_csrf_bad_ids_and_kind() {
    let e = env().await;
    let (st, _) = send(&e, "POST", &format!("/plans/{PID}/proposal"), &[], false).await;
    assert_eq!(st, 403);
    let (st, _) = send(
        &e,
        "PUT",
        &format!("/plans/{PID}/proposal"),
        &edit_form(),
        false,
    )
    .await;
    assert_eq!(st, 403);
    let (st, _) = send(&e, "POST", &format!("/plans/{PID}/epic"), &[], false).await;
    assert_eq!(st, 403);
    for bad in [
        "..",
        "abc",
        "0123456789ABCDEF0123456789ABCDEF",
        "ffffffffffffffffffffffffffffffff",
    ] {
        let (st, _) = send(&e, "POST", &format!("/plans/{bad}/proposal"), &[], true).await;
        assert_eq!(st, 404, "{bad}");
    }
    let (st, _) = send(&e, "POST", "/plans/..%2F..%2Fx/proposal", &[], true).await;
    assert_eq!(st, 404);
    assert_eq!(e.calls(), "", "no pas was spawned");

    let p = env_kind("pipeline").await;
    let (st, _) = generate(&p).await;
    assert_eq!(st, 409);
    assert_eq!(p.calls(), "");
}

#[tokio::test]
async fn save_and_create_need_a_proposal() {
    let e = env().await;
    assert_eq!(save(&e, &edit_form()).await.0, 409);
    assert_eq!(create(&e, &[]).await.0, 409);
    assert_eq!(e.calls(), "");
    assert!(!e.plan().join("epic.pending").exists());
}

#[tokio::test]
async fn blank_title_and_bad_dependency_are_rejected_on_save() {
    let e = env().await;
    generate(&e).await;
    let mut form = edit_form();
    form[2] = ("t0_title", "  ");
    assert_eq!(save(&e, &form).await.0, 400);
    let mut form = edit_form();
    form.push(("dep", "0:0"));
    assert_eq!(save(&e, &form).await.0, 400);
    let mut form = edit_form();
    form.push(("dep", "9:0"));
    assert_eq!(save(&e, &form).await.0, 400);
    assert_eq!(e.stored(), proposal());
}
