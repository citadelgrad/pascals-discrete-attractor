#![cfg(all(unix, feature = "monitor"))]
//! Monitor one-click mode against the real `pas` binary (attractor-ino.43).
//! Stubs on `PATH` stand in for the model (`claude`) and the Beads program.

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

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn stub(dir: &Path, name: &str, script: &str) {
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
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
    /// `claude_out` is what the stubbed model prints; `None` makes it fail.
    fn new(claude_out: Option<&str>) -> Self {
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
        let log = dir.path().join("bd.log");
        stub(
            &bin,
            BEADS_PROGRAM,
            &format!("echo \"$*\" >> {}\necho '[]'", log.display()),
        );
        match claude_out {
            Some(dot) => {
                let answer = dir.path().join("answer.json");
                fs::write(&answer, serde_json::json!({ "result": dot }).to_string()).unwrap();
                stub(&bin, "claude", &format!("cat {}", answer.display()));
            }
            None => stub(&bin, "claude", "echo boom >&2; exit 1"),
        }
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
        let deadline = Instant::now() + Duration::from_secs(10);
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "Monitor did not start");
            std::thread::sleep(Duration::from_millis(50));
        }
        Self {
            dir,
            port,
            _monitor: Kill(monitor),
        }
    }

    fn repo(&self) -> PathBuf {
        self.dir.path().join("repo").canonicalize().unwrap()
    }

    fn bd_log(&self) -> Vec<String> {
        fs::read_to_string(self.dir.path().join("bd.log"))
            .map(|s| s.lines().map(String::from).collect())
            .unwrap_or_default()
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        head_extra: &str,
        body: &[u8],
    ) -> (u16, String, String) {
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        let head = format!(
            "{method} {path} HTTP/1.0\r\nHost: 127.0.0.1:{}\r\nContent-Length: {}\r\n{head_extra}\r\n",
            self.port,
            body.len()
        );
        s.write_all(head.as_bytes()).unwrap();
        s.write_all(body).unwrap();
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
        let (_, _, page) = self.request("GET", "/plans/new", "", b"");
        let marker = "X-CSRF-Token&quot;:&quot;";
        let rest = &page[page.find(marker).expect("token in page") + marker.len()..];
        rest.chars().take_while(|c| c.is_ascii_hexdigit()).collect()
    }

    /// Upload three files as a one-click Plan of `kind`; returns (token, id).
    fn plan(&self, kind: &str) -> (String, String) {
        let token = self.token();
        let b = "XBX";
        let mut body = String::new();
        for n in ["a.md", "b.md", "c.md"] {
            body.push_str(&format!("--{b}\r\nContent-Disposition: form-data; name=\"files\"; filename=\"{n}\"\r\nContent-Type: text/plain\r\n\r\n# {n}\r\n"));
        }
        let repo = self.repo();
        for (k, v) in [
            ("repo", repo.to_str().unwrap()),
            ("kind", kind),
            ("mode", "one_click"),
        ] {
            body.push_str(&format!(
                "--{b}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n"
            ));
        }
        body.push_str(&format!("--{b}--\r\n"));
        let extra =
            format!("Content-Type: multipart/form-data; boundary={b}\r\nX-CSRF-Token: {token}\r\n");
        let (code, _, page) = self.request("POST", "/plans/new", &extra, body.as_bytes());
        assert_eq!(code, 200, "{page}");
        assert!(page.contains("/one-click"), "{page}");
        let rest = &page[page.find("<code>").unwrap() + 6..];
        (token, rest[..32].to_string())
    }

    fn one_click(&self, token: &str, pid: &str) -> (u16, String, String) {
        self.request(
            "POST",
            &format!("/plans/{pid}/one-click"),
            &format!("X-CSRF-Token: {token}\r\n"),
            b"",
        )
    }

    /// The Run folders of every Pipeline under the repository's `pipelines/`.
    fn run_dirs(&self) -> Vec<PathBuf> {
        let repo = self.repo();
        let Ok(rd) = fs::read_dir(repo.join("pipelines")) else {
            return vec![];
        };
        rd.flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "dot"))
            .flat_map(|e| {
                let runs = attractor_monitor::pipeline::logs_dir(&repo, &e.path()).join("runs");
                fs::read_dir(runs)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|d| d.path())
                    .collect::<Vec<_>>()
            })
            .collect()
    }
}

fn wait_for(what: &str, within: Duration, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn redirect_id(head: &str) -> String {
    let h = head.to_lowercase();
    let at =
        h.find("hx-redirect: /runs/").expect("HX-Redirect header") + "hx-redirect: /runs/".len();
    head[at..].lines().next().unwrap().trim().to_string()
}

fn run_started(run_dir: &Path) -> Value {
    fs::read_to_string(run_dir.join("events.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|e| e["type"] == "RunStarted")
        .expect("RunStarted")
}

#[test]
fn pipeline_only_one_click_reaches_a_run_with_default_limits_and_calls_no_bd() {
    let fx = Fx::new(Some(QUICK));
    let (token, pid) = fx.plan("pipeline");
    let (code, head, body) = fx.one_click(&token, &pid);
    assert_eq!(code, 200, "{body}");
    let id = redirect_id(&head);
    let (code, _, _) = fx.request("GET", &format!("/runs/{id}"), "", b"");
    assert_eq!(code, 200, "the Run page answers at once");
    let dirs = fx.run_dirs();
    let run_dir = dirs
        .iter()
        .find(|d| d.file_name().is_some_and(|n| n.to_string_lossy() == id))
        .unwrap_or_else(|| panic!("Run folder for {id} in {dirs:?}"));
    wait_for("the Run's events", Duration::from_secs(20), || {
        run_dir.join("events.jsonl").is_file()
    });
    let started = run_started(run_dir);
    assert_eq!(started["data"]["max_budget_usd"], 200.0);
    assert_eq!(started["data"]["max_steps"], 200);
    assert!(fx.bd_log().is_empty(), "no bd command: {:?}", fx.bd_log());
}

#[test]
fn a_pipeline_pas_rejects_stops_at_the_pipeline_step_and_starts_no_run() {
    // The real `pas generate` refuses a Pipeline with no exit node; the flow
    // stops on that step's page. Diagnostics for a written-but-invalid file
    // are covered over HTTP in the monitor crate.
    let fx = Fx::new(Some(NO_EXIT));
    let (token, pid) = fx.plan("pipeline");
    let (code, head, body) = fx.one_click(&token, &pid);
    assert_eq!(code, 502, "{body}");
    assert!(!head.to_lowercase().contains("hx-redirect"), "{head}");
    assert!(body.contains("<div id=\"pipeline\">"), "{body}");
    assert!(body.contains("no canonical exit node"), "{body}");
    assert!(fx.run_dirs().is_empty(), "no Run was started");
    assert!(fx.bd_log().is_empty());
}

#[test]
fn a_failed_decompose_stops_at_the_proposal_and_creates_no_epic() {
    let fx = Fx::new(None);
    let (token, pid) = fx.plan("epic_pipeline");
    let (code, head, body) = fx.one_click(&token, &pid);
    assert_eq!(code, 502, "{body}");
    assert!(!head.to_lowercase().contains("hx-redirect"), "{head}");
    assert!(body.contains("<div id=\"proposal\">"), "{body}");
    assert!(body.contains("notice err"), "{body}");
    assert!(fx.bd_log().is_empty(), "no Epic: {:?}", fx.bd_log());
    assert!(!fx
        .dir
        .path()
        .join("state/plans")
        .join(&pid)
        .join("result.json")
        .exists());
}
