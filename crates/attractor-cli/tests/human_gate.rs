#![cfg(unix)]
//! Human Gates in `pas run` (spec File Change 9, C1, C3): a `wait.human`
//! stage journals `HumanInputRequested`, waits for the terminal (only when
//! stdin is a TTY) or `answers/<question-id>.json`, and journals
//! `HumanInputAnswered` with the answer's source. Observed through the real
//! binary.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use attractor_journal::{write_answer, AnswerFile, AnswerSource, RunDir};
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

/// Longer than two answer-file polls (1 s each).
const TWO_POLLS: Duration = Duration::from_millis(2500);

fn pas() -> &'static str {
    env!("CARGO_BIN_EXE_pas")
}

/// A scratch folder with the Pipeline, a logs folder, and a state folder.
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

    fn args(&self) -> Vec<String> {
        let path = |p: PathBuf| p.display().to_string();
        vec![
            "run".into(),
            path(self.path().join("p.dot")),
            "--workdir".into(),
            path(self.path().to_path_buf()),
            "--logs".into(),
            path(self.path().join("logs")),
        ]
    }

    fn command(&self) -> Command {
        let mut command = Command::new(pas());
        command
            .args(self.args())
            .env("PAS_STATE_DIR", self.path().join("state"))
            .env_remove("PAS_HEARTBEAT_INTERVAL_MS")
            .current_dir(self.path())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        command
    }

    fn run_dir(&self) -> Option<PathBuf> {
        let mut dirs: Vec<PathBuf> = fs::read_dir(self.path().join("logs").join("runs"))
            .ok()?
            .map(|e| e.unwrap().path())
            .collect();
        assert!(dirs.len() <= 1, "one Run folder, got {dirs:?}");
        dirs.pop()
    }

    fn events(&self) -> Vec<Value> {
        let Some(run_dir) = self.run_dir() else {
            return vec![];
        };
        let Ok(text) = fs::read_to_string(run_dir.join("events.jsonl")) else {
            return vec![];
        };
        text.lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn of_type(&self, ty: &str) -> Vec<Value> {
        self.events()
            .into_iter()
            .filter(|e| e["type"] == ty)
            .collect()
    }

    /// Wait for the `count`th `HumanInputRequested` and return it.
    fn wait_for_request(&self, child: &mut Child, count: usize) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let requests = self.of_type("HumanInputRequested");
            if requests.len() >= count {
                return requests[count - 1].clone();
            }
            if let Some(status) = child.try_wait().unwrap() {
                panic!("pas exited before asking: {status:?}\n{}", stderr(child));
            }
            if Instant::now() > deadline {
                child.kill().unwrap();
                panic!("no HumanInputRequested #{count}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn answer(&self, question_id: &str, choice: &str, source: AnswerSource) {
        let run = RunDir::from_path(self.run_dir().unwrap());
        let file = AnswerFile::new(question_id, choice, source);
        assert!(write_answer(&run, &file).unwrap(), "first answer");
    }

    /// The gate is still waiting: the process runs and nothing answered.
    fn assert_waiting(&self, child: &mut Child) {
        assert!(child.try_wait().unwrap().is_none(), "pas stopped waiting");
        assert!(self.of_type("HumanInputAnswered").is_empty());
        assert!(!self
            .of_type("StageCompleted")
            .iter()
            .any(|e| e["data"]["node_id"] == "gate"));
    }
}

fn stderr(child: &mut Child) -> String {
    let mut text = String::new();
    if let Some(mut err) = child.stderr.take() {
        std::io::Read::read_to_string(&mut err, &mut text).unwrap();
    }
    text
}

fn wait_exit(child: &mut Child) -> ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("pas did not finish");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn assert_exit_ok(child: &mut Child) {
    let status = wait_exit(child);
    assert!(status.success(), "{status:?}\n{}", stderr(child));
}

fn ts(event: &Value) -> chrono::DateTime<chrono::Utc> {
    event["ts"].as_str().unwrap().parse().unwrap()
}

fn assert_request(request: &Value) {
    assert_eq!(request["data"]["question_id"], "q-gate-1", "{request}");
    assert_eq!(request["data"]["node_id"], "gate");
    assert_eq!(request["data"]["text"], "Ship it?");
    assert_eq!(
        request["data"]["choices"],
        serde_json::json!(["approve", "reject"])
    );
}

/// The one `HumanInputAnswered`, after checking the gate routed by its choice.
fn assert_answered(fx: &Fixture, choice: &str, source: &str) -> Value {
    let answers = fx.of_type("HumanInputAnswered");
    assert_eq!(answers.len(), 1, "{answers:?}");
    let answer = answers[0].clone();
    assert_eq!(answer["data"]["question_id"], "q-gate-1");
    assert_eq!(answer["data"]["choice"], choice);
    assert_eq!(answer["data"]["source"], source);
    let edge = fx
        .of_type("EdgeSelected")
        .into_iter()
        .find(|e| e["data"]["from_node"] == "gate")
        .expect("an edge out of the gate");
    assert_eq!(edge["data"]["edge_label"], choice);
    let to = if choice == "approve" { "done" } else { "no" };
    assert_eq!(edge["data"]["to_node"], to);
    answer
}

// AC1 + AC2: the request is journaled before the gate waits; an answer file
// continues the stage within 2 s, routed by its label, with its source.
#[test]
fn answer_file_continues_the_gate_with_its_source() {
    let fx = Fixture::new();
    let mut child = fx.command().stdin(Stdio::null()).spawn().unwrap();

    let request = fx.wait_for_request(&mut child, 1);
    assert_request(&request);
    let events = fx.events();
    assert_eq!(
        events.last().unwrap()["type"],
        "HumanInputRequested",
        "nothing after the request while waiting"
    );
    std::thread::sleep(Duration::from_millis(500));
    fx.assert_waiting(&mut child);

    let written = chrono::Utc::now();
    fx.answer("q-gate-1", "reject", AnswerSource::Monitor);
    assert_exit_ok(&mut child);

    let answer = assert_answered(&fx, "reject", "monitor");
    let waited = ts(&answer) - written;
    assert!(
        waited < chrono::Duration::seconds(2),
        "answered after {waited}"
    );
    assert!(ts(&answer) > ts(&request));
}

// AC4: with stdin not a TTY nothing is read from it (typed answers do not
// count), the Run says how to answer, and the answer file still works.
#[test]
fn piped_stdin_is_not_read_and_the_answer_file_still_works() {
    let fx = Fixture::new();
    let mut child = fx.command().stdin(Stdio::piped()).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"1\napprove\n2\n").unwrap();
    stdin.flush().unwrap();

    fx.wait_for_request(&mut child, 1);
    std::thread::sleep(TWO_POLLS);
    fx.assert_waiting(&mut child);

    fx.answer("q-gate-1", "reject", AnswerSource::Cli);
    assert_exit_ok(&mut child);
    drop(stdin);
    assert_answered(&fx, "reject", "cli");

    let err = stderr(&mut child);
    let path = fx.run_dir().unwrap().join("answers").join("q-gate-1.json");
    assert!(err.contains("Human Gate q-gate-1"), "{err}");
    assert!(err.contains(&path.display().to_string()), "{err}");
}

// AC5: an answer file whose choice is not offered is ignored (moved aside)
// and the gate keeps waiting for a valid one.
#[test]
fn answer_file_with_unknown_choice_is_ignored() {
    let fx = Fixture::new();
    let mut child = fx.command().stdin(Stdio::null()).spawn().unwrap();
    fx.wait_for_request(&mut child, 1);

    fx.answer("q-gate-1", "maybe", AnswerSource::Cli);
    std::thread::sleep(TWO_POLLS);
    fx.assert_waiting(&mut child);
    let answers = fx.run_dir().unwrap().join("answers");
    assert!(!answers.join("q-gate-1.json").exists());
    let rejected = fs::read_to_string(answers.join("q-gate-1.json.rejected")).unwrap();
    assert!(rejected.contains("maybe"), "{rejected}");

    fx.answer("q-gate-1", "approve", AnswerSource::Cli);
    assert_exit_ok(&mut child);
    assert_answered(&fx, "approve", "cli");
    let err = stderr(&mut child);
    assert!(err.contains("Ignoring answer file"), "{err}");
}

// AC6: after a kill during the wait, the resumed Run asks again with the
// same question_id, in the same Run.
#[test]
fn resume_after_kill_asks_the_same_question_again() {
    let fx = Fixture::new();
    let mut first = fx.command().stdin(Stdio::null()).spawn().unwrap();
    let asked = fx.wait_for_request(&mut first, 1);
    assert_eq!(asked["attempt"], 1);
    first.kill().unwrap(); // SIGKILL: no clean shutdown
    first.wait().unwrap();

    let mut second = fx.command().stdin(Stdio::null()).spawn().unwrap();
    let again = fx.wait_for_request(&mut second, 2);
    assert_eq!(again["attempt"], 2);
    assert_eq!(again["data"]["question_id"], asked["data"]["question_id"]);
    assert_request(&again);
    assert_eq!(fx.of_type("RunStarted").len(), 1, "same Run");

    fx.answer("q-gate-1", "approve", AnswerSource::Cli);
    assert_exit_ok(&mut second);
    assert_answered(&fx, "approve", "cli");
    assert_eq!(fx.of_type("HumanInputRequested").len(), 2);
}

/// `script` runs `pas` on a pseudo-terminal, so stdin is a TTY. The
/// command line differs between BSD and util-linux `script`.
fn command_on_a_tty(fx: &Fixture) -> Option<Command> {
    let quoted: Vec<String> = std::iter::once(pas().to_string())
        .chain(fx.args())
        .map(|a| format!("'{}'", a.replace('\'', r"'\''")))
        .collect();
    let mut command = Command::new("script");
    if cfg!(target_os = "linux") {
        command.args(["-qec", &quoted.join(" "), "/dev/null"]);
    } else if cfg!(target_os = "macos") {
        command.args(["-q", "/dev/null", pas()]).args(fx.args());
    } else {
        return None;
    }
    let available = Command::new("script")
        .args(if cfg!(target_os = "linux") {
            vec!["-qec", "true", "/dev/null"]
        } else {
            vec!["-q", "/dev/null", "true"]
        })
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !available {
        return None;
    }
    command
        .env("PAS_STATE_DIR", fx.path().join("state"))
        .env_remove("PAS_HEARTBEAT_INTERVAL_MS")
        .current_dir(fx.path())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    Some(command)
}

// AC3: with a TTY, a terminal answer continues the stage and is sourced
// `terminal`; it is also recorded in the answer file.
#[test]
fn terminal_answer_on_a_tty_is_sourced_terminal() {
    let fx = Fixture::new();
    let Some(mut command) = command_on_a_tty(&fx) else {
        eprintln!("skipping: `script` cannot run a command on a pseudo-terminal here");
        return;
    };
    let mut child = command.stdin(Stdio::piped()).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    fx.wait_for_request(&mut child, 1);

    stdin.write_all(b"2\n").unwrap();
    stdin.flush().unwrap();
    assert_exit_ok(&mut child);
    drop(stdin);

    assert_answered(&fx, "reject", "terminal");
    let run = RunDir::from_path(fx.run_dir().unwrap());
    let file = attractor_journal::read_answer(&run, "q-gate-1")
        .unwrap()
        .unwrap();
    assert_eq!(file.source, AnswerSource::Terminal);
    assert_eq!(file.choice, "reject");
}
