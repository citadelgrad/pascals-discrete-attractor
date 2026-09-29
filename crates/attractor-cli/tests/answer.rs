#![cfg(unix)]
//! `pas answer` (spec File Change 12, C1, C3, C6) against the real binary and
//! a real waiting Human Gate.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const PIPELINE: &str = r#"digraph Gate {
    start [shape="Mdiamond"]
    gate  [shape="hexagon", prompt="Ship it?"]
    no    [shape="diamond"]
    done  [shape="Msquare"]
    start -> gate
    gate -> done [label="approve"]
    gate -> no   [label="reject"]
    no -> done
}"#;

fn pas() -> &'static str {
    env!("CARGO_BIN_EXE_pas")
}

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("p.dot"), PIPELINE).unwrap();
        Self { dir }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn run_command(&self) -> Command {
        let mut c = Command::new(pas());
        c.args(["run"])
            .arg(self.path().join("p.dot"))
            .arg("--workdir")
            .arg(self.path())
            .arg("--logs")
            .arg(self.path().join("logs"))
            .env("PAS_STATE_DIR", self.path().join("state"))
            .env_remove("PAS_HEARTBEAT_INTERVAL_MS")
            .current_dir(self.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        c
    }

    fn spawn(&self) -> Child {
        self.run_command().spawn().unwrap()
    }

    fn run_dir(&self) -> Option<PathBuf> {
        fs::read_dir(self.path().join("logs").join("runs"))
            .ok()?
            .next()
            .map(|e| e.unwrap().path())
    }

    fn run_id(&self) -> String {
        self.run_dir()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    fn events(&self, ty: &str) -> Vec<Value> {
        let Some(dir) = self.run_dir() else {
            return vec![];
        };
        let Ok(text) = fs::read_to_string(dir.join("events.jsonl")) else {
            return vec![];
        };
        text.lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .filter(|e| e["type"] == ty)
            .collect()
    }

    fn wait_for_request(&self, child: &mut Child) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.events("HumanInputRequested").is_empty() {
            assert!(child.try_wait().unwrap().is_none(), "pas exited early");
            assert!(Instant::now() < deadline, "no HumanInputRequested");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn answer(&self, args: &[&str]) -> Output {
        Command::new(pas())
            .arg("answer")
            .args(args)
            .env("PAS_STATE_DIR", self.path().join("state"))
            .current_dir(self.path())
            .output()
            .unwrap()
    }

    fn answer_file(&self) -> PathBuf {
        self.run_dir()
            .unwrap()
            .join("answers")
            .join("q-gate-1.json")
    }

    fn answer_files(&self) -> usize {
        fs::read_dir(self.run_dir().unwrap().join("answers"))
            .map(|d| d.count())
            .unwrap_or(0)
    }
}

fn finish(child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "{status:?}");
            return;
        }
        assert!(Instant::now() < deadline, "pas did not finish");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn json_line(out: &Output) -> Value {
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(text.lines().count(), 1, "one line: {text:?}");
    serde_json::from_str(&text).unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

// AC1: the answer continues the Run; the journal shows source cli.
#[test]
fn answer_continues_the_run_with_source_cli() {
    let fx = Fixture::new();
    let mut child = fx.spawn();
    fx.wait_for_request(&mut child);
    let out = fx.answer(&[&fx.run_id(), "q-gate-1", "approve"]);
    assert!(out.status.success(), "{}", stderr(&out));
    finish(&mut child);
    let answered = fx.events("HumanInputAnswered");
    assert_eq!(answered.len(), 1);
    assert_eq!(answered[0]["data"]["choice"], "approve");
    assert_eq!(answered[0]["data"]["source"], "cli");
    let edge = &fx.events("EdgeSelected")[1];
    assert_eq!(edge["data"]["to_node"], "done");
}

// AC2: --source monitor.
#[test]
fn source_monitor_is_journaled() {
    let fx = Fixture::new();
    let mut child = fx.spawn();
    fx.wait_for_request(&mut child);
    let out = fx.answer(&[&fx.run_id(), "q-gate-1", "reject", "--source", "monitor"]);
    assert!(out.status.success(), "{}", stderr(&out));
    finish(&mut child);
    assert_eq!(
        fx.events("HumanInputAnswered")[0]["data"]["source"],
        "monitor"
    );
    let file: Value = serde_json::from_str(&fs::read_to_string(fx.answer_file()).unwrap()).unwrap();
    assert_eq!(file["source"], "monitor");
    // `terminal` is not a CLI source.
    let out = fx.answer(&[&fx.run_id(), "q-gate-1", "reject", "--source", "terminal"]);
    assert!(!out.status.success());
}

// AC3: a second answer exits 7 and leaves the file byte-for-byte alone.
#[test]
fn second_answer_exits_7_and_keeps_the_file() {
    let fx = Fixture::new();
    let mut child = fx.spawn();
    fx.wait_for_request(&mut child);
    let id = fx.run_id();
    assert!(fx.answer(&[&id, "q-gate-1", "approve"]).status.success());
    let bytes = fs::read(fx.answer_file()).unwrap();
    let mtime = fs::metadata(fx.answer_file()).unwrap().modified().unwrap();
    let out = fx.answer(&[&id, "q-gate-1", "reject"]);
    assert_eq!(out.status.code(), Some(7), "{}", stderr(&out));
    assert!(stderr(&out).contains("already answered"));
    assert_eq!(fs::read(fx.answer_file()).unwrap(), bytes);
    assert_eq!(
        fs::metadata(fx.answer_file()).unwrap().modified().unwrap(),
        mtime
    );
    // Once the Run consumed it, the journal says answered too.
    finish(&mut child);
    let out = fx.answer(&[&id, "q-gate-1", "reject"]);
    assert_eq!(out.status.code(), Some(7));
    assert_eq!(fx.answer_files(), 1);
}

// AC4: a choice not offered exits non-zero and creates no file.
#[test]
fn unoffered_choice_fails_and_creates_no_file() {
    let fx = Fixture::new();
    let mut child = fx.spawn();
    fx.wait_for_request(&mut child);
    let id = fx.run_id();
    for bad in ["maybe", "1"] {
        let out = fx.answer(&[&id, "q-gate-1", bad]);
        assert_eq!(out.status.code(), Some(1));
        assert!(stderr(&out).contains("approve, reject"), "{}", stderr(&out));
    }
    assert_eq!(fx.answer_files(), 0);
    assert!(child.try_wait().unwrap().is_none(), "still waiting");
    assert!(fx.answer(&[&id, "q-gate-1", "approve"]).status.success());
    finish(&mut child);
}

// AC5: unknown ids are named; unsafe ids create nothing.
#[test]
fn unknown_ids_exit_nonzero_naming_the_id() {
    let fx = Fixture::new();
    let mut child = fx.spawn();
    fx.wait_for_request(&mut child);
    let id = fx.run_id();
    let random = "0192a000-0000-7000-8000-0000000000ff";
    for (run, q, named) in [
        (random, "q-gate-1", random),
        ("not-a-uuid", "q-gate-1", "not-a-uuid"),
        (id.as_str(), "q-nope-9", "q-nope-9"),
        (id.as_str(), "../x", "../x"),
    ] {
        let out = fx.answer(&[run, q, "approve"]);
        assert_eq!(out.status.code(), Some(1), "{run} {q}");
        assert!(stderr(&out).contains(named), "{}", stderr(&out));
    }
    assert_eq!(fx.answer_files(), 0);
    assert!(!fx.run_dir().unwrap().join("x.json").exists());
    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn no_index_is_an_unknown_run() {
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(pas())
        .args([
            "answer",
            "0192a000-0000-7000-8000-000000000001",
            "q",
            "a",
            "--json",
        ])
        .env("PAS_STATE_DIR", tmp.path().join("none"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(json_line(&out)["error"]["code"], "unknown_run");
}

#[test]
fn removed_run_folder_is_reported_missing() {
    let fx = Fixture::new();
    let mut child = fx.spawn();
    fx.wait_for_request(&mut child);
    let id = fx.run_id();
    child.kill().unwrap();
    child.wait().unwrap();
    fs::remove_dir_all(fx.run_dir().unwrap()).unwrap();
    let out = fx.answer(&[&id, "q-gate-1", "approve"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains(&id) && stderr(&out).contains("missing"));
}

// AC6: --json prints one object with ok and run_id, success and failure.
#[test]
fn json_output_for_success_and_each_failure() {
    let fx = Fixture::new();
    let mut child = fx.spawn();
    fx.wait_for_request(&mut child);
    let id = fx.run_id();
    let random = "0192a000-0000-7000-8000-0000000000ff";
    let cases = [
        (random, "q-gate-1", "approve", 1, "unknown_run"),
        (id.as_str(), "q-nope-9", "approve", 1, "unknown_question"),
        (id.as_str(), "q-gate-1", "maybe", 1, "invalid_choice"),
    ];
    for (run, q, choice, code, error) in cases {
        let out = fx.answer(&[run, q, choice, "--json"]);
        assert_eq!(out.status.code(), Some(code));
        let v = json_line(&out);
        assert_eq!((v["v"].clone(), v["ok"].clone()), (1.into(), false.into()));
        assert_eq!(v["run_id"], run);
        assert_eq!(v["error"]["code"], error);
        assert!(v["error"]["message"].is_string());
    }
    let out = fx.answer(&[&id, "q-gate-1", "approve", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    let v = json_line(&out);
    let mut keys: Vec<&String> = v.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "answer_path",
            "choice",
            "ok",
            "question_id",
            "run_id",
            "source",
            "v"
        ]
    );
    assert_eq!(
        (v["ok"].clone(), v["run_id"].clone()),
        (true.into(), id.clone().into())
    );
    assert_eq!(v["source"], "cli");
    assert_eq!(
        fs::canonicalize(v["answer_path"].as_str().unwrap()).unwrap(),
        fs::canonicalize(fx.answer_file()).unwrap()
    );
    let out = fx.answer(&[&id, "q-gate-1", "reject", "--json"]);
    assert_eq!(out.status.code(), Some(7));
    let v = json_line(&out);
    assert_eq!(
        (v["ok"].clone(), v["run_id"].clone()),
        (false.into(), id.into())
    );
    assert_eq!(v["error"]["code"], "already_answered");
    finish(&mut child);
}

// A Run killed at the gate keeps the answer; resuming consumes it.
#[test]
fn answer_written_before_a_resume_is_used() {
    let fx = Fixture::new();
    let mut first = fx.spawn();
    fx.wait_for_request(&mut first);
    let id = fx.run_id();
    first.kill().unwrap();
    first.wait().unwrap();
    assert!(fx.answer(&[&id, "q-gate-1", "approve"]).status.success());
    let mut second = fx.spawn();
    finish(&mut second);
    assert_eq!(fx.events("HumanInputAnswered")[0]["data"]["source"], "cli");
}
