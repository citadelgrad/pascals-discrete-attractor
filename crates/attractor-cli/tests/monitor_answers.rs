#![cfg(all(unix, feature = "monitor"))]
//! Monitor Human Gate answer buttons against the real `pas` binary
//! (attractor-ino.39): the page lists the question and one button per choice,
//! a click answers through `pas answer --source monitor`, and a gate that was
//! already answered from the terminal is left alone.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const PIPELINE: &str = r#"digraph Gate {
    start [shape="Mdiamond"]
    gate  [shape="hexagon", prompt="Ship it?"]
    no    [shape="parallelogram", tool_command="touch went-reject"]
    yes   [shape="parallelogram", tool_command="touch went-approve"]
    done  [shape="Msquare"]
    start -> gate
    gate -> yes [label="approve"]
    gate -> no  [label="reject"]
    yes -> done
    no -> done
}"#;

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

struct Fx {
    dir: tempfile::TempDir,
    port: u16,
    _monitor: Kill,
    _run: Kill,
    id: String,
}

impl Fx {
    /// A Monitor and a `pas run` waiting at the gate.
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("p.dot"), PIPELINE).unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let state = dir.path().join("state");
        let monitor = Command::new(env!("CARGO_BIN_EXE_pas"))
            .args(["monitor", "--port", &port.to_string()])
            .env("PAS_STATE_DIR", &state)
            .current_dir(dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let run = Command::new(env!("CARGO_BIN_EXE_pas"))
            .args([
                "run",
                "p.dot",
                "--workdir",
                dir.path().to_str().unwrap(),
                "--logs",
                dir.path().join("logs").to_str().unwrap(),
            ])
            .env("PAS_STATE_DIR", &state)
            .current_dir(dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        wait_for("the Monitor", Duration::from_secs(10), || {
            TcpStream::connect(("127.0.0.1", port)).is_ok()
        });
        let mut fx = Self {
            dir,
            port,
            _monitor: Kill(monitor),
            _run: Kill(run),
            id: String::new(),
        };
        wait_for("the gate", Duration::from_secs(30), || {
            fx.run_dir().is_some()
                && fx
                    .events()
                    .iter()
                    .any(|e| e["type"] == "HumanInputRequested")
        });
        fx.id = fx
            .run_dir()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        fx
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn run_dir(&self) -> Option<PathBuf> {
        fs::read_dir(self.path().join("logs/runs"))
            .ok()?
            .next()
            .map(|e| e.unwrap().path())
    }

    fn events(&self) -> Vec<Value> {
        let Some(d) = self.run_dir() else {
            return vec![];
        };
        fs::read_to_string(d.join("events.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    fn of_type(&self, t: &str) -> Vec<Value> {
        self.events()
            .into_iter()
            .filter(|e| e["type"] == t)
            .collect()
    }

    fn qid(&self) -> String {
        let e = &self.of_type("HumanInputRequested")[0];
        e["data"]["question_id"]
            .as_str()
            .or_else(|| e["question_id"].as_str())
            .unwrap()
            .to_string()
    }

    fn http(&self, method: &str, path: &str, token: Option<&str>, form: &str) -> (u16, String) {
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        let mut req = format!(
            "{method} {path} HTTP/1.0\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n",
            self.port,
            form.len()
        );
        if let Some(t) = token {
            req.push_str(&format!("X-CSRF-Token: {t}\r\n"));
        }
        req.push_str("\r\n");
        req.push_str(form);
        s.write_all(req.as_bytes()).unwrap();
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).unwrap();
        let text = String::from_utf8_lossy(&buf).into_owned();
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        (
            head.split(' ').nth(1).unwrap().parse().unwrap(),
            body.to_string(),
        )
    }

    /// The Run page once it shows the gate; returns (page, CSRF token).
    fn page(&self) -> (String, String) {
        let mut page = String::new();
        wait_for("the gate on the Run page", Duration::from_secs(15), || {
            let (code, body) = self.http("GET", &format!("/runs/{}", self.id), None, "");
            page = body;
            code == 200 && page.contains("data-choice")
        });
        let marker = "X-CSRF-Token&quot;:&quot;";
        let rest = &page[page.find(marker).expect("token in page") + marker.len()..];
        let token = rest.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
        (page, token)
    }

    fn answer(&self, token: &str, choice: &str) -> (u16, String) {
        self.http(
            "POST",
            &format!("/runs/{}/answers/{}", self.id, self.qid()),
            Some(token),
            &format!("choice={choice}"),
        )
    }

    fn answer_file(&self) -> PathBuf {
        self.run_dir()
            .unwrap()
            .join("answers")
            .join(format!("{}.json", self.qid()))
    }
}

#[test]
fn click_answers_the_gate_as_monitor_and_the_buttons_disappear() {
    let fx = Fx::new();
    let (page, token) = fx.page();
    assert!(page.contains("Ship it?"), "{page}");
    assert_eq!(page.matches("data-choice=").count(), 2, "{page}");
    assert!(page.contains(r#"data-choice="approve""#) && page.contains(r#"data-choice="reject""#));

    let (code, body) = fx.answer(&token, "approve");
    assert_eq!(code, 200, "{body}");
    let clicked = Instant::now();
    wait_for("the buttons to go", Duration::from_secs(5), || {
        let (_, s) = fx.http("GET", &format!("/runs/{}/summary", fx.id), None, "");
        !s.contains("data-choice")
    });
    assert!(
        clicked.elapsed() < Duration::from_secs(2),
        "{:?}",
        clicked.elapsed()
    );

    wait_for("the Run to continue", Duration::from_secs(30), || {
        !fx.of_type("AttemptEnded").is_empty()
    });
    let answered = fx.of_type("HumanInputAnswered");
    assert_eq!(answered.len(), 1, "{answered:?}");
    let data = answered[0].get("data").unwrap_or(&answered[0]);
    assert_eq!(data["choice"], "approve");
    assert_eq!(data["source"], "monitor");
    assert!(fx.path().join("went-approve").exists());
    assert!(!fx.path().join("went-reject").exists());
}

#[test]
fn gate_answered_from_the_terminal_is_reported_and_left_unchanged() {
    let fx = Fx::new();
    let (_, token) = fx.page();
    let out = Command::new(env!("CARGO_BIN_EXE_pas"))
        .args(["answer", &fx.id, &fx.qid(), "reject", "--source", "cli"])
        .env("PAS_STATE_DIR", fx.path().join("state"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let before = fs::read(fx.answer_file()).unwrap();

    let (code, body) = fx.answer(&token, "approve");
    assert_eq!(code, 409, "{body}");
    assert!(body.contains("already answered"), "{body}");

    wait_for("the Run to continue", Duration::from_secs(30), || {
        !fx.of_type("AttemptEnded").is_empty()
    });
    assert_eq!(fs::read(fx.answer_file()).unwrap(), before);
    let answered = fx.of_type("HumanInputAnswered");
    assert_eq!(answered.len(), 1, "{answered:?}");
    let data = answered[0].get("data").unwrap_or(&answered[0]);
    assert_eq!(data["choice"], "reject");
    assert_eq!(data["source"], "cli");
    assert!(fx.path().join("went-reject").exists());
    assert!(!fx.path().join("went-approve").exists());
}
