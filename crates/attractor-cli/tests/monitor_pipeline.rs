#![cfg(all(unix, feature = "monitor"))]
//! Monitor Pipeline check and Launch against the real `pas` binary
//! (attractor-ino.42). A stub on `PATH` stands in for the Beads program.

mod common;

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

/// Built with `concat!` so the program name appears quoted only in the stub.
const BEADS_PROGRAM: &str = concat!("b", "d");

const QUICK: &str = r#"digraph Quick {
    start [shape="Mdiamond"]
    step [shape="parallelogram", tool_command="true"]
    done [shape="Msquare"]
    start -> step -> done
}"#;

/// No exit node: `pas validate` rejects it.
const NO_EXIT: &str = r#"digraph NoExit {
    start [shape="Mdiamond"]
    step [shape="parallelogram", tool_command="true"]
    start -> step
}"#;

const WAITS: &str = concat!(
    r#"digraph Waits {
    start [shape="Mdiamond"]
    wait [shape="parallelogram", timeout="120s", max_retries=2, tool_command=""#,
    common::wait_for_go!(),
    r#""]
    done [shape="Msquare"]
    start -> wait -> done
}"#
);

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_for(what: &str, within: Duration, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

struct Fx {
    dir: tempfile::TempDir,
    port: u16,
    _monitor: Kill,
}

impl Fx {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        git(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
        );
        let stub = bin.join(BEADS_PROGRAM);
        fs::write(
            &stub,
            "#!/bin/sh\n[ \"$*\" = \"show e-1 --json\" ] || { echo \"no issue found matching $2\" >&2; exit 1; }\n\
             echo '[{\"id\":\"e-1\",\"title\":\"My Epic\",\"status\":\"open\",\"description\":\"Epic body\"}]'\n",
        )
        .unwrap();
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let monitor = Command::new(env!("CARGO_BIN_EXE_pas"))
            .args(["monitor", "--port", &port.to_string()])
            .env("PAS_STATE_DIR", dir.path().join("state"))
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .current_dir(dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        wait_for("the Monitor", Duration::from_secs(10), || {
            TcpStream::connect(("127.0.0.1", port)).is_ok()
        });
        Self {
            dir,
            port,
            _monitor: Kill(monitor),
        }
    }

    fn repo(&self) -> PathBuf {
        self.dir.path().join("repo").canonicalize().unwrap()
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        body: &str,
        token: Option<&str>,
    ) -> (u16, String, String) {
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        let mut head = format!(
            "{method} {path} HTTP/1.0\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n",
            self.port,
            body.len()
        );
        if let Some(t) = token {
            head.push_str(&format!("X-CSRF-Token: {t}\r\n"));
        }
        head.push_str("\r\n");
        s.write_all(head.as_bytes()).unwrap();
        s.write_all(body.as_bytes()).unwrap();
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).unwrap();
        let text = String::from_utf8_lossy(&buf).into_owned();
        let (h, b) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        (
            h.split(' ').nth(1).unwrap().parse().unwrap(),
            h.to_string(),
            b.to_string(),
        )
    }

    fn token(&self) -> String {
        let (_, _, page) = self.request("GET", "/plans/new", "", None);
        let marker = "X-CSRF-Token&quot;:&quot;";
        let rest = &page[page.find(marker).expect("token in page") + marker.len()..];
        rest.chars().take_while(|c| c.is_ascii_hexdigit()).collect()
    }

    /// Upload one file as a Plan of `kind`; returns (token, plan id).
    fn plan(&self, kind: &str) -> (String, String) {
        let token = self.token();
        let b = "XBX";
        let mut body = format!("--{b}\r\nContent-Disposition: form-data; name=\"files\"; filename=\"one.md\"\r\nContent-Type: text/plain\r\n\r\n# One\r\n");
        let repo = self.repo();
        for (k, v) in [
            ("repo", repo.to_str().unwrap()),
            ("kind", kind),
            ("mode", "reviewed"),
        ] {
            body.push_str(&format!(
                "--{b}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n"
            ));
        }
        body.push_str(&format!("--{b}--\r\n"));
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        let head = format!(
            "POST /plans/new HTTP/1.0\r\nHost: 127.0.0.1:{}\r\nContent-Type: multipart/form-data; boundary={b}\r\nContent-Length: {}\r\nX-CSRF-Token: {token}\r\n\r\n",
            self.port,
            body.len()
        );
        s.write_all(head.as_bytes()).unwrap();
        s.write_all(body.as_bytes()).unwrap();
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).unwrap();
        let page = String::from_utf8_lossy(&buf).into_owned();
        assert!(page.starts_with("HTTP/1.0 200"), "{page}");
        let rest = &page[page.find("<code>").unwrap() + 6..];
        (token, rest[..32].to_string())
    }

    /// Pretend Create Epic ran: the Plan's `result.json`.
    fn set_epic(&self, pid: &str, epic: &str) {
        let f = self
            .dir
            .path()
            .join("state/plans")
            .join(pid)
            .join("result.json");
        fs::write(f, format!(r#"{{"v":1,"epic_id":"{epic}","task_ids":[]}}"#)).unwrap();
    }

    fn form(
        &self,
        method: &str,
        path: &str,
        token: &str,
        pairs: &[(&str, &str)],
    ) -> (u16, String, String) {
        let enc = |s: &str| -> String {
            s.bytes()
                .map(|b| match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                        (b as char).to_string()
                    }
                    _ => format!("%{b:02X}"),
                })
                .collect()
        };
        let body: Vec<String> = pairs
            .iter()
            .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
            .collect();
        self.request(method, path, &body.join("&"), Some(token))
    }
}

fn disabled(page: &str) -> bool {
    let at = page.find("id=\"launch-button\"").expect("launch button");
    let end = page[at..].find('>').unwrap();
    page[at..at + end]
        .split_whitespace()
        .any(|t| t == "disabled")
}

fn redirect_id(head: &str) -> String {
    let h = head.to_lowercase();
    let at =
        h.find("hx-redirect: /runs/").expect("HX-Redirect header") + "hx-redirect: /runs/".len();
    head[at..].lines().next().unwrap().trim().to_string()
}

fn events(run_dir: &Path) -> Vec<Value> {
    fs::read_to_string(run_dir.join("events.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn run_started(run_dir: &Path) -> Value {
    events(run_dir)
        .into_iter()
        .find(|e| e["type"] == "RunStarted")
        .expect("RunStarted")
}

/// The Run folder where the Monitor's `logs_dir` says it is.
fn run_dir_of(repo: &Path, dot: &Path, id: &str) -> PathBuf {
    attractor_monitor::pipeline::logs_dir(repo, dot)
        .join("runs")
        .join(id)
}

#[test]
fn defaults_match_pas_run_help() {
    let out = Command::new(env!("CARGO_BIN_EXE_pas"))
        .args(["run", "--help"])
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&out.stdout).replace('\n', " ");
    let b = attractor_monitor::pipeline::DEFAULT_MAX_BUDGET_USD;
    let s = attractor_monitor::pipeline::DEFAULT_MAX_STEPS;
    assert!(help.contains(&format!("Defaults to ${b}")), "{help}");
    assert!(help.contains(&format!("Default: {s}")), "{help}");
}

#[test]
fn epic_build_edit_check_and_launch() {
    let fx = Fx::new();
    let (token, pid) = fx.plan("epic_pipeline");
    let base = format!("/plans/{pid}");

    // No Epic yet.
    let (code, _, _) = fx.form("POST", &format!("{base}/pipeline"), &token, &[]);
    assert_eq!(code, 409);

    fx.set_epic(&pid, "e-1");
    let (code, _, page) = fx.form("POST", &format!("{base}/pipeline"), &token, &[]);
    assert_eq!(code, 200, "{page}");
    let dot = fx.repo().join("pipelines/e-1.dot");
    assert!(dot.is_file());
    assert!(page.contains("id=\"dot-src\""), "graph shown: {page}");
    assert!(
        page.contains("My Epic") || page.contains("digraph"),
        "{page}"
    );
    assert!(page.contains("Pipeline is valid."), "{page}");
    assert!(!disabled(&page));

    // A broken edit lists the real validator's diagnostics and disables Launch.
    let (code, _, page) = fx.form(
        "PUT",
        &format!("{base}/pipeline"),
        &token,
        &[("dot", NO_EXIT)],
    );
    assert_eq!(code, 200, "{page}");
    assert_eq!(fs::read_to_string(&dot).unwrap(), NO_EXIT);
    assert!(page.contains("Pipeline is not valid."), "{page}");
    assert!(page.contains("id=\"diagnostics\""), "{page}");
    assert!(disabled(&page));
    let (code, _, _) = fx.form(
        "POST",
        &format!("{base}/launch"),
        &token,
        &[
            ("workdir", fx.repo().to_str().unwrap()),
            ("max_budget_usd", "1"),
            ("max_steps", "5"),
        ],
    );
    assert_eq!(code, 409, "an invalid Pipeline is never launched");

    // Fixed: Launch is enabled and starts a Run with the given limits.
    let (_, _, page) = fx.form(
        "PUT",
        &format!("{base}/pipeline"),
        &token,
        &[("dot", QUICK)],
    );
    assert!(page.contains("Pipeline is valid."), "{page}");
    assert!(!disabled(&page));
    let (code, head, body) = fx.form(
        "POST",
        &format!("{base}/launch"),
        &token,
        &[
            ("workdir", fx.repo().to_str().unwrap()),
            ("max_budget_usd", "12.5"),
            ("max_steps", "40"),
        ],
    );
    assert_eq!(code, 200, "{body}");
    let id = redirect_id(&head);
    // The redirect target serves at once, and the Run folder is where the
    // Monitor computed it (this also guards the copied hash).
    let (code, _, _) = fx.request("GET", &format!("/runs/{id}"), "", None);
    assert_eq!(code, 200);
    let run_dir = run_dir_of(&fx.repo(), &dot, &id);
    wait_for("the Run's events", Duration::from_secs(20), || {
        run_dir.join("events.jsonl").is_file()
    });
    let started = run_started(&run_dir);
    assert_eq!(started["data"]["max_budget_usd"], 12.5);
    assert_eq!(started["data"]["max_steps"], 40);
    assert_eq!(started["data"]["shared_workdir"], false);
    assert!(run_dir.join("console.log").is_file());
}

#[test]
fn pipeline_only_plan_builds_are_reported_when_generate_cannot_run() {
    // No `claude` on PATH: the failure is shown as a message, not a page.
    let fx = Fx::new();
    let (token, pid) = fx.plan("pipeline");
    let (code, _, page) = fx.form("POST", &format!("/plans/{pid}/pipeline"), &token, &[]);
    assert_eq!(code, 502, "{page}");
    assert!(page.contains("notice err"), "{page}");
    assert!(!page.contains("id=\"launch-button\""));
}

#[test]
fn worktree_lock_is_shown_unless_shared_workdir_is_checked() {
    let fx = Fx::new();
    let repo = fx.repo();
    // A holder Run in the same worktree, started like a user would.
    let waits = fx.dir.path().join("waits.dot");
    fs::write(&waits, WAITS).unwrap();
    let holder_logs = fx.dir.path().join("holder-logs");
    let holder = Command::new(env!("CARGO_BIN_EXE_pas"))
        .args(["run"])
        .arg(&waits)
        .arg("--workdir")
        .arg(&repo)
        .arg("--logs")
        .arg(&holder_logs)
        .env("PAS_STATE_DIR", fx.dir.path().join("state"))
        .current_dir(fx.dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _holder = Kill(holder);
    wait_for("the holder's Attempt", Duration::from_secs(20), || {
        fs::read_dir(holder_logs.join("runs"))
            .ok()
            .and_then(|mut d| d.next())
            .map(|d| {
                events(&d.unwrap().path())
                    .iter()
                    .any(|e| e["type"] == "AttemptStarted")
            })
            .unwrap_or(false)
    });

    let (token, pid) = fx.plan("epic_pipeline");
    fx.set_epic(&pid, "e-1");
    let base = format!("/plans/{pid}");
    fx.form("POST", &format!("{base}/pipeline"), &token, &[]);
    fx.form(
        "PUT",
        &format!("{base}/pipeline"),
        &token,
        &[("dot", QUICK)],
    );
    let dot = repo.join("pipelines/e-1.dot");
    let form = [
        ("workdir", repo.to_str().unwrap()),
        ("max_budget_usd", "5"),
        ("max_steps", "10"),
    ];

    let (code, head, page) = fx.form("POST", &format!("{base}/launch"), &token, &form);
    assert_eq!(code, 409, "{page}");
    assert!(
        page.contains("another Run is active in this git worktree"),
        "{page}"
    );
    assert!(!head.to_lowercase().contains("hx-redirect"));
    assert!(page.contains("value=\"10\""), "the form keeps its values");

    let mut shared = form.to_vec();
    shared.push(("allow_shared", "on"));
    let (code, head, page) = fx.form("POST", &format!("{base}/launch"), &token, &shared);
    assert_eq!(code, 200, "{page}");
    let id = redirect_id(&head);
    let run_dir = run_dir_of(&repo, &dot, &id);
    wait_for("the shared Run's events", Duration::from_secs(20), || {
        run_dir.join("events.jsonl").is_file()
    });
    assert_eq!(run_started(&run_dir)["data"]["shared_workdir"], true);
}
