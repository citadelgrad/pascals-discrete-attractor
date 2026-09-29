//! New Plan page over HTTP (attractor-ino.40).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use attractor_monitor::state::AppState;
use attractor_monitor::{bind, serve_on_with};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

struct Env {
    tmp: tempfile::TempDir,
    addr: SocketAddr,
    token: String,
    _stop: tokio::sync::oneshot::Sender<()>,
}

impl Env {
    fn plans(&self) -> PathBuf {
        self.tmp.path().join("plans")
    }
    fn repo(&self) -> String {
        let r = self.tmp.path().join("repo");
        std::fs::create_dir_all(r.join(".git")).unwrap();
        r.to_string_lossy().into_owned()
    }
    /// No Plan folder (nor temp folder) exists.
    fn no_plans(&self) -> bool {
        !self.plans().exists() || std::fs::read_dir(self.plans()).unwrap().next().is_none()
    }
    fn only_plan(&self) -> PathBuf {
        let mut it = std::fs::read_dir(self.plans()).unwrap();
        let p = it.next().unwrap().unwrap().path();
        assert!(it.next().is_none());
        p
    }
}

async fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let state = AppState::new(tmp.path().join("runs.jsonl"));
    let token = state.csrf_token().as_str().to_string();
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

async fn raw(addr: SocketAddr, head: String, body: &[u8]) -> (u16, String) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(head.as_bytes()).await.unwrap();
    // The server may answer (e.g. 403 from the CSRF guard) and close without
    // reading the body, so the body write and the final read can fail with a
    // broken pipe or connection reset once the response has been sent.
    let _ = s.write_all(body).await;
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf).await;
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status = text.split(' ').nth(1).unwrap().parse().unwrap();
    (status, text)
}

async fn get(e: &Env, path: &str) -> (u16, String) {
    raw(
        e.addr,
        format!(
            "GET {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            e.addr
        ),
        b"",
    )
    .await
}

const B: &str = "XBOUNDARYX";

struct Form {
    files: Vec<(String, Vec<u8>)>,
    repo: String,
    kind: &'static str,
    mode: &'static str,
}

fn form(e: &Env, files: Vec<(&str, Vec<u8>)>) -> Form {
    Form {
        files: files.into_iter().map(|(n, b)| (n.to_string(), b)).collect(),
        repo: e.repo(),
        kind: "epic_pipeline",
        mode: "one_click",
    }
}

fn body(f: &Form) -> Vec<u8> {
    let mut b = Vec::new();
    for (n, bytes) in &f.files {
        b.extend(format!("--{B}\r\nContent-Disposition: form-data; name=\"files\"; filename=\"{n}\"\r\nContent-Type: text/plain\r\n\r\n").as_bytes());
        b.extend(bytes);
        b.extend(b"\r\n");
    }
    for (k, v) in [
        ("repo", &f.repo),
        ("kind", &f.kind.to_string()),
        ("mode", &f.mode.to_string()),
    ] {
        b.extend(
            format!("--{B}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n")
                .as_bytes(),
        );
    }
    b.extend(format!("--{B}--\r\n").as_bytes());
    b
}

async fn post_with(e: &Env, f: &Form, token: Option<&str>) -> (u16, String) {
    let b = body(f);
    let mut head = format!(
        "POST /plans/new HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Type: multipart/form-data; boundary={B}\r\nContent-Length: {}\r\n",
        e.addr,
        b.len()
    );
    if let Some(t) = token {
        head.push_str(&format!("X-CSRF-Token: {t}\r\n"));
    }
    head.push_str("\r\n");
    raw(e.addr, head, &b).await
}

async fn post(e: &Env, f: &Form) -> (u16, String) {
    post_with(e, f, Some(&e.token.clone())).await
}

fn plan_json(dir: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(dir.join("plan.json")).unwrap()).unwrap()
}

#[tokio::test]
async fn three_reordered_files_are_stored_in_order() {
    let e = env().await;
    let f = form(
        &e,
        vec![
            ("c.md", b"C".to_vec()),
            ("a.md", b"A".to_vec()),
            ("b.txt", b"B".to_vec()),
        ],
    );
    let (status, text) = post(&e, &f).await;
    assert_eq!(status, 200, "{text}");
    let dir = e.only_plan();
    assert_eq!(
        std::fs::read_to_string(dir.join("docs/01-c.md")).unwrap(),
        "C"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("docs/02-a.md")).unwrap(),
        "A"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("docs/03-b.txt")).unwrap(),
        "B"
    );
    assert!(
        text.contains("Generate Proposal") && text.contains("/proposal"),
        "Epic + Pipeline Plans offer Generate: {text}"
    );
    let j = plan_json(&dir);
    assert_eq!(j["v"], 1);
    assert_eq!(
        j["files"],
        serde_json::json!(["01-c.md", "02-a.md", "03-b.txt"])
    );
    assert_eq!(j["kind"], "epic_pipeline");
    assert_eq!(j["mode"], "one_click");
    assert!(Path::new(j["repo"].as_str().unwrap()).is_absolute());
}

#[tokio::test]
async fn pdf_is_rejected_by_name_and_no_plan_exists() {
    let e = env().await;
    let f = form(
        &e,
        vec![("a.md", b"x".to_vec()), ("notes.pdf", b"x".to_vec())],
    );
    let (status, text) = post(&e, &f).await;
    assert_eq!(status, 400);
    assert!(text.contains("notes.pdf"), "{text}");
    assert!(e.no_plans());
}

#[tokio::test]
async fn one_mib_boundary() {
    let e = env().await;
    let f = form(&e, vec![("big.md", vec![b'a'; 1024 * 1024 + 1])]);
    let (status, text) = post(&e, &f).await;
    assert_eq!(status, 400);
    assert!(text.contains("big.md"), "{text}");
    assert!(e.no_plans());
    let f = form(&e, vec![("ok.md", vec![b'a'; 1024 * 1024])]);
    assert_eq!(post(&e, &f).await.0, 200);
}

#[tokio::test]
async fn twenty_one_files_are_rejected_twenty_accepted() {
    let e = env().await;
    let many = |n: usize| -> Vec<(String, Vec<u8>)> {
        (0..n)
            .map(|i| (format!("f{i}.md"), b"x".to_vec()))
            .collect()
    };
    let mut f = form(&e, vec![]);
    f.files = many(21);
    let (status, text) = post(&e, &f).await;
    assert_eq!(status, 400);
    assert!(text.contains("20"), "{text}");
    assert!(e.no_plans());
    f.files = many(20);
    assert_eq!(post(&e, &f).await.0, 200);
}

#[tokio::test]
async fn non_git_targets_are_rejected() {
    let e = env().await;
    let plain = e.tmp.path().join("plain");
    std::fs::create_dir(&plain).unwrap();
    let file = e.tmp.path().join("afile");
    std::fs::write(&file, "x").unwrap();
    for bad in [
        plain.to_string_lossy().into_owned(),
        e.tmp.path().join("missing").to_string_lossy().into_owned(),
        file.to_string_lossy().into_owned(),
    ] {
        let mut f = form(&e, vec![("a.md", b"x".to_vec())]);
        f.repo = bad.clone();
        let (status, text) = post(&e, &f).await;
        assert_eq!(status, 400, "{bad}");
        assert!(
            text.contains("not a git repository") && text.contains(&bad),
            "{text}"
        );
    }
    assert!(e.no_plans());
}

#[tokio::test]
async fn uploads_are_under_plans_and_not_served_from_assets() {
    let e = env().await;
    let f = form(&e, vec![("a.md", b"SECRET".to_vec())]);
    assert_eq!(post(&e, &f).await.0, 200);
    let dir = e.only_plan();
    assert!(dir.starts_with(e.tmp.path().join("plans")));
    let id = dir.file_name().unwrap().to_string_lossy().into_owned();
    for p in [
        "/assets/01-a.md".to_string(),
        format!("/assets/plans/{id}/docs/01-a.md"),
        format!("/assets/..%2Fplans%2F{id}%2Fplan.json"),
        format!("/assets/{id}"),
    ] {
        let (status, text) = get(&e, &p).await;
        assert_eq!(status, 404, "{p}");
        assert!(!text.contains("SECRET"));
    }
}

#[tokio::test]
async fn hostile_names_stay_inside_docs() {
    let e = env().await;
    let f = form(
        &e,
        vec![("../../etc/x.md", b"1".to_vec()), ("a/b.md", b"2".to_vec())],
    );
    assert_eq!(post(&e, &f).await.0, 200);
    let dir = e.only_plan();
    assert_eq!(std::fs::read_dir(dir.join("docs")).unwrap().count(), 2);
    assert!(dir.join("docs/01-x.md").exists());
    assert!(!e.tmp.path().join("etc").exists());
}

#[tokio::test]
async fn invalid_utf8_empty_and_unknown_choices_are_400() {
    let e = env().await;
    let (s, t) = post(&e, &form(&e, vec![("x.md", vec![0xff, 0xfe])])).await;
    assert_eq!(s, 400);
    assert!(t.contains("x.md"));
    assert_eq!(post(&e, &form(&e, vec![])).await.0, 400);
    let mut f = form(&e, vec![("a.md", b"x".to_vec())]);
    f.kind = "bogus";
    assert_eq!(post(&e, &f).await.0, 400);
    let mut f = form(&e, vec![("a.md", b"x".to_vec())]);
    f.mode = "bogus";
    assert_eq!(post(&e, &f).await.0, 400);
    assert!(e.no_plans());
}

#[tokio::test]
async fn missing_or_wrong_csrf_token_writes_nothing() {
    let e = env().await;
    let f = form(&e, vec![("a.md", b"x".to_vec())]);
    assert_eq!(post_with(&e, &f, None).await.0, 403);
    assert_eq!(post_with(&e, &f, Some("wrong")).await.0, 403);
    assert!(e.no_plans());
}

#[tokio::test]
async fn form_page_has_inputs_reorder_list_and_csrf_header() {
    let e = env().await;
    let (status, text) = get(&e, "/plans/new").await;
    assert_eq!(status, 200);
    for needle in [
        "type=\"file\"",
        "name=\"files\"",
        "id=\"file-order\"",
        "name=\"repo\"",
        "value=\"epic_pipeline\"",
        "value=\"one_click\"",
        "hx-post=\"/plans/new\"",
        "X-CSRF-Token",
        &e.token,
    ] {
        assert!(text.contains(needle), "missing {needle}");
    }
    let (_, runs) = get(&e, "/").await;
    assert!(runs.contains("/plans/new"));
}
