#![cfg(unix)]

//! Contract tests for every `--json` payload (spec C6). Each command has a
//! golden success and failure payload under `tests/golden/json/`. Volatile
//! values are normalised by field name, the result is compared with the
//! golden file, and stdout must be exactly one JSON object.
//! `UPDATE_GOLDEN=1 cargo test -p attractor-cli --test json_contract`
//! rewrites the golden files.

mod common;

use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

/// Built with `concat!` so the program name appears quoted only in the adapter.
const BEADS_PROGRAM: &str = concat!("b", "d");

/// Every C6 payload kind; each needs a success and a failure golden file.
const KINDS: [&str; 10] = [
    "decompose_proposal",
    "decompose_create",
    "scaffold",
    "generate",
    "validate",
    "run",
    "answer",
    "stop",
    "kill",
    "runs",
];

const PROPOSAL: &str = r#"{"v":1,"epic":{"title":"Epic","description":"Epic body"},"tasks":[{"title":"A","type":"task","priority":"P2","description":"a"},{"title":"B","type":"task","priority":"P1","description":"b"}],"dependencies":[{"blocked":1,"blocker":0}]}"#;

const DOT: &str = r#"digraph G { start [shape="Mdiamond"] work [shape="box" llm_provider="claude" timeout="300s" prompt="x"] done [shape="Msquare"] start -> work -> done }"#;

const WAITS: &str = concat!(
    r#"digraph Waits {
    start [shape="Mdiamond"]
    wait [shape="parallelogram", timeout="120s", tool_command=""#,
    common::wait_for_go!(),
    r#""]
    done [shape="Msquare"]
    start -> wait -> done
}"#
);

const QUICK: &str = r#"digraph Quick {
    start [shape="Mdiamond"]
    step [shape="parallelogram", tool_command="true"]
    done [shape="Msquare"]
    start -> step -> done
}"#;

const GATE: &str = r#"digraph Gate {
    start [shape="Mdiamond"]
    gate  [shape="hexagon", prompt="Ship it?"]
    done  [shape="Msquare"]
    start -> gate
    gate -> done [label="approve"]
}"#;

// ---------------------------------------------------------------- goldens

fn golden_path(kind: &str, outcome: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/json")
        .join(format!("{kind}.{outcome}.json"))
}

/// Replace volatile values, keyed on field name, with placeholders.
fn normalise(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, v)| {
                    let placeholder = match key.as_str() {
                        "run_id" => Some("<RUN_ID>"),
                        "pid" => Some("<PID>"),
                        "pipeline_path" | "run_dir" | "answer_path" | "stop_path" | "path"
                        | "logs_dir" | "workdir" | "pipeline" => Some("<PATH>"),
                        "message" => Some("<MESSAGE>"),
                        k if k.ends_with("_at") || k == "ts" || k == "timestamp" => Some("<TS>"),
                        _ => None,
                    };
                    let v = match placeholder {
                        Some(p) if !v.is_null() => Value::String(p.into()),
                        _ => normalise(v),
                    };
                    (key.clone(), v)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(normalise).collect()),
        other => other.clone(),
    }
}

/// Compare a normalised payload with its golden file (exact keys, types, values).
fn compare(golden: &Value, actual: &Value) -> Result<(), String> {
    if golden == actual {
        Ok(())
    } else {
        Err(format!("golden: {golden:#}\nactual: {actual:#}"))
    }
}

/// Parse `stdout`'s first line as one JSON object; `whole` also requires
/// that nothing else is on stdout.
fn one_object(stdout: &str, whole: bool) -> Value {
    let lines: Vec<&str> = stdout.lines().collect();
    if whole {
        assert_eq!(lines.len(), 1, "stdout must be one line: {stdout:?}");
    }
    let first = lines.first().unwrap_or_else(|| panic!("empty stdout"));
    let value: Value = serde_json::from_str(first).unwrap();
    assert!(value.is_object(), "not an object: {value}");
    value
}

fn assert_golden(kind: &str, outcome: &str, payload: &Value) {
    let actual = normalise(payload);
    assert_eq!(actual["v"], 1, "{kind}.{outcome}");
    assert_eq!(actual["ok"], outcome == "success", "{kind}.{outcome}");
    let path = golden_path(kind, outcome);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, serde_json::to_string_pretty(&actual).unwrap() + "\n").unwrap();
    }
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing golden {}: {e}", path.display()));
    let golden: Value = serde_json::from_str(&text).unwrap();
    if let Err(diff) = compare(&golden, &actual) {
        panic!("{kind}.{outcome} deviates from {}\n{diff}", path.display());
    }
}

/// Check a finished command's stdout and exit status, then its golden file.
fn check(kind: &str, outcome: &str, output: &Output) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.success(),
        outcome == "success",
        "{kind}.{outcome} exit: {:?}\nstdout: {stdout}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_golden(kind, outcome, &one_object(&stdout, true));
}

// ---------------------------------------------------------------- fixtures

fn stub(dir: &Path, name: &str, script: &str) {
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// `bd` stub: `create` prints sequential ids, `show e-1` prints an Epic.
fn beads_stub(dir: &Path) {
    let counter = dir.join("bd.count");
    stub(
        dir,
        BEADS_PROGRAM,
        &format!(
            r#"if [ "$1" = "create" ]; then
  n=$(cat {c} 2>/dev/null || echo 0); n=$((n+1)); echo $n > {c}
  echo "{{\"id\":\"id-$n\"}}"
elif [ "$*" = "show e-1 --json" ]; then
  echo '[{{"id":"e-1","title":"My Epic","status":"open","description":"Epic body"}}]'
elif [ "$1" = "show" ]; then
  echo "no issue found matching $2" >&2; exit 1
else
  echo '[]'
fi"#,
            c = counter.display()
        ),
    );
}

fn claude_stub(dir: &Path, result: &str) {
    let answer = dir.join("answer.json");
    fs::write(&answer, json!({ "result": result }).to_string()).unwrap();
    stub(
        dir,
        "claude",
        &format!("cat > /dev/null\ncat {}", answer.display()),
    );
}

/// A scratch folder with stubs on `PATH`, a workdir and a private state folder.
struct Scratch {
    dir: tempfile::TempDir,
}

impl Scratch {
    fn new() -> Self {
        let scratch = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        fs::create_dir_all(scratch.bin()).unwrap();
        beads_stub(&scratch.bin());
        claude_stub(&scratch.bin(), DOT);
        scratch
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn bin(&self) -> PathBuf {
        self.path().join("bin")
    }

    fn state(&self) -> PathBuf {
        self.path().join("state")
    }

    fn write(&self, name: &str, text: &str) {
        fs::write(self.path().join(name), text).unwrap();
    }

    fn pas(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pas"));
        command
            .args(args)
            .current_dir(self.path())
            .env("PATH", format!("{}:/usr/bin:/bin", self.bin().display()))
            .env("PAS_STATE_DIR", self.state())
            .env("PAS_HEARTBEAT_INTERVAL_MS", "100")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env("GIT_CEILING_DIRECTORIES", self.path().parent().unwrap());
        command
    }

    fn output(&self, args: &[&str]) -> Output {
        self.pas(args).output().unwrap()
    }

    /// Start `pas run <pipeline> --json`; returns the child and its first
    /// stdout line (the C6 payload).
    fn start(&self, pipeline: &str) -> (Active, Value, String) {
        let logs = self.path().join("logs");
        let mut command = self.pas(&[
            "run",
            self.path().join(pipeline).to_str().unwrap(),
            "--workdir",
            self.path().to_str().unwrap(),
            "--logs",
            logs.to_str().unwrap(),
            "--json",
        ]);
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        let payload = one_object(&line, true);
        let run_id = payload["run_id"].as_str().unwrap().to_string();
        let active = Active {
            child: Some(child),
            _stdout: stdout,
            go: self.path().join("go"),
            events: logs.join("runs").join(&run_id).join("events.jsonl"),
        };
        (active, payload, run_id)
    }
}

/// A live `pas run`; killed and reaped on drop so no test leaks a child.
struct Active {
    child: Option<Child>,
    _stdout: BufReader<std::process::ChildStdout>,
    go: PathBuf,
    events: PathBuf,
}

impl Active {
    fn wait_for_event(&self, ty: &str) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !fs::read_to_string(&self.events).is_ok_and(|s| s.contains(ty)) {
            assert!(Instant::now() < deadline, "timed out waiting for {ty}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn finish(&mut self) {
        let _ = fs::write(&self.go, "");
        let deadline = Instant::now() + Duration::from_secs(30);
        let child = self.child.as_mut().unwrap();
        while child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "pas run did not finish");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Active {
    fn drop(&mut self) {
        let _ = fs::write(&self.go, "");
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

// ------------------------------------------------------------------ tests

#[test]
fn decompose_proposal_contract() {
    let s = Scratch::new();
    claude_stub(&s.bin(), PROPOSAL);
    s.write("a.md", "# First");
    let args = |plan: &'static str| ["decompose", "--plan", plan, "--dry-run", "--json"];
    check("decompose_proposal", "success", &s.output(&args("a.md")));
    check("decompose_proposal", "failure", &s.output(&args("a.bin")));
}

#[test]
fn decompose_create_contract() {
    let s = Scratch::new();
    s.write("p.json", PROPOSAL);
    s.write("bad.json", "{ nope");
    let args = |file: &'static str| ["decompose", "--from-proposal", file, "--json"];
    check("decompose_create", "success", &s.output(&args("p.json")));
    check("decompose_create", "failure", &s.output(&args("bad.json")));
}

#[test]
fn scaffold_contract() {
    let s = Scratch::new();
    let out = |epic: &str| s.output(&["scaffold", epic, "--output", "out.dot", "--json"]);
    check("scaffold", "success", &out("e-1"));
    check("scaffold", "failure", &out("e-404"));
}

#[test]
fn generate_contract() {
    let s = Scratch::new();
    s.write("a.md", "ALPHA");
    s.write("a.bin", "ALPHA");
    let out = |plan: &str| s.output(&["generate", "--plan", plan, "--json", "-o", "gen.dot"]);
    check("generate", "success", &out("a.md"));
    check("generate", "failure", &out("a.bin"));
}

#[test]
fn validate_contract() {
    let s = Scratch::new();
    s.write("ok.dot", QUICK);
    let out = |file: &str| s.output(&["validate", file, "--json"]);
    check("validate", "success", &out("ok.dot"));
    check("validate", "failure", &out("missing.dot"));
}

#[test]
fn run_contract() {
    let s = Scratch::new();
    s.write("waits.dot", WAITS);
    let (mut active, payload, _) = s.start("waits.dot");
    assert_golden("run", "success", &payload);
    active.finish();

    s.write("broken.dot", "digraph {");
    let out = s.output(&["run", "broken.dot", "--json", "--logs", "logs2"]);
    check("run", "failure", &out);
}

#[test]
fn answer_contract() {
    let s = Scratch::new();
    s.write("gate.dot", GATE);
    let (mut active, _, run_id) = s.start("gate.dot");
    active.wait_for_event("HumanInputRequested");
    let out = s.output(&["answer", &run_id, "q-gate-1", "approve", "--json"]);
    check("answer", "success", &out);
    active.finish();
    let out = s.output(&["answer", &run_id, "q-gate-1", "approve", "--json"]);
    check("answer", "failure", &out);
}

#[test]
fn stop_contract() {
    let s = Scratch::new();
    s.write("waits.dot", WAITS);
    let (mut active, _, run_id) = s.start("waits.dot");
    active.wait_for_event("AttemptStarted");
    check("stop", "success", &s.output(&["stop", &run_id, "--json"]));
    active.finish();
    let out = s.output(&["stop", "0192a000-0000-7000-8000-0000000000ff", "--json"]);
    check("stop", "failure", &out);
}

#[test]
fn kill_contract() {
    let s = Scratch::new();
    s.write("waits.dot", WAITS);
    let (mut active, _, run_id) = s.start("waits.dot");
    active.wait_for_event("Heartbeat");
    // `kill` checks liveness by PID, so the child must be reaped as it exits.
    let mut child = active.child.take().unwrap();
    let reaper = std::thread::spawn(move || child.wait());
    let out = s.output(&["kill", &run_id, "--grace", "5s", "--json"]);
    check("kill", "success", &out);
    reaper.join().unwrap().unwrap();
    let out = s.output(&["kill", "0192a000-0000-7000-8000-0000000000ff", "--json"]);
    check("kill", "failure", &out);
}

#[test]
fn runs_contract() {
    let s = Scratch::new();
    s.write("quick.dot", QUICK);
    let run = s.output(&[
        "run",
        "quick.dot",
        "--workdir",
        s.path().to_str().unwrap(),
        "--logs",
        s.path().join("logs").to_str().unwrap(),
    ]);
    assert!(run.status.success());
    let out = s.output(&["runs", "--json"]);
    let payload = one_object(&String::from_utf8_lossy(&out.stdout), true);
    assert_eq!(payload["runs"].as_array().unwrap().len(), 1);
    check("runs", "success", &out);

    let broken = tempfile::tempdir().unwrap();
    fs::create_dir_all(broken.path().join("runs.jsonl")).unwrap();
    let out = s
        .pas(&["runs", "--json"])
        .env("PAS_STATE_DIR", broken.path())
        .output()
        .unwrap();
    check("runs", "failure", &out);
}

// -------------------------------------------------------- self-checks

/// AC1: every C6 command has both golden files, each a v1 object.
#[test]
fn every_command_has_success_and_failure_goldens() {
    for kind in KINDS {
        for outcome in ["success", "failure"] {
            let path = golden_path(kind, outcome);
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("missing golden {}: {e}", path.display()));
            let golden: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(golden["v"], 1, "{}", path.display());
            assert_eq!(golden["ok"], outcome == "success", "{}", path.display());
        }
    }
}

/// AC2 helper: more than one stdout line, or a non-object, is rejected.
#[test]
fn stdout_must_be_exactly_one_object() {
    for bad in ["{\"v\":1}\n{\"v\":1}\n", "[1]\n", "1\n"] {
        assert!(
            std::panic::catch_unwind(|| one_object(bad, true)).is_err(),
            "{bad}"
        );
    }
    assert!(one_object("{\"v\":1}\n", true).is_object());
}

/// AC3: renaming or removing a field, or changing its type, fails the comparison.
#[test]
fn mutated_payload_fails_the_comparison() {
    let golden: Value =
        serde_json::from_str(&fs::read_to_string(golden_path("answer", "success")).unwrap())
            .unwrap();
    assert!(compare(&golden, &golden).is_ok());
    let object = |v: &Value| -> Map<String, Value> { v.as_object().unwrap().clone() };

    let mut renamed = object(&golden);
    let choice = renamed.remove("choice").unwrap();
    renamed.insert("picked".into(), choice);
    assert!(compare(&golden, &Value::Object(renamed)).is_err());

    let mut removed = object(&golden);
    removed.remove("source");
    assert!(compare(&golden, &Value::Object(removed)).is_err());

    let mut retyped = object(&golden);
    retyped.insert("ok".into(), json!("true"));
    assert!(compare(&golden, &Value::Object(retyped)).is_err());

    let mut extra = object(&golden);
    extra.insert("surprise".into(), json!(1));
    assert!(compare(&golden, &Value::Object(extra)).is_err());
}
