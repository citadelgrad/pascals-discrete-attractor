#![cfg(unix)]
//! `pas stop` and the stop check between stages (spec File Change 10 and 12,
//! C1, C3, C6), observed through the real binary.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

fn pas() -> &'static str {
    env!("CARGO_BIN_EXE_pas")
}

/// `slow` takes ~2 s; `next` must not start when a stop arrives during it.
const SLOW_THEN_NEXT: &str = r#"digraph Stoppable {
    start [shape="Mdiamond"]
    slow [shape="parallelogram", tool_command="echo run >> slow.count; touch started; sleep 2; touch finished"]
    next [shape="parallelogram", tool_command="echo run >> next.count"]
    done [shape="Msquare"]
    start -> slow -> next -> done
}"#;

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new(source: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("p.dot"), source).unwrap();
        Self { dir }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn logs(&self) -> PathBuf {
        self.path().join("logs")
    }

    fn pas(&self, args: &[&str]) -> Command {
        let mut command = Command::new(pas());
        command
            .args(args)
            .env("PAS_STATE_DIR", self.path().join("state"))
            .current_dir(self.path());
        command
    }

    fn run_cmd(&self) -> Command {
        let mut command = self.pas(&[
            "run",
            self.path().join("p.dot").to_str().unwrap(),
            "--workdir",
            self.path().to_str().unwrap(),
            "--logs",
            self.logs().to_str().unwrap(),
        ]);
        command.stdout(Stdio::null()).stderr(Stdio::null());
        command
    }

    fn spawn(&self) -> Child {
        self.run_cmd().spawn().unwrap()
    }

    fn run_dir(&self) -> PathBuf {
        let dirs: Vec<PathBuf> = fs::read_dir(self.logs().join("runs"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(dirs.len(), 1, "{dirs:?}");
        dirs.into_iter().next().unwrap()
    }

    fn run_id(&self) -> String {
        self.run_dir().file_name().unwrap().to_string_lossy().into()
    }

    fn stop(&self, extra: &[&str]) -> Output {
        let id = self.run_id();
        let mut args = vec!["stop", id.as_str()];
        args.extend(extra);
        self.pas(&args).output().unwrap()
    }

    /// Start a run, wait until `slow` is executing, and return the child.
    fn start_and_wait_for_slow(&self) -> Child {
        let child = self.spawn();
        wait_for("slow to start", || self.path().join("started").exists());
        child
    }
}

fn wait_for(what: &str, ready: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_exit(mut child: Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("pas run did not exit");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn events(run_dir: &Path) -> Vec<Value> {
    fs::read_to_string(run_dir.join("events.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn types(events: &[Value]) -> Vec<&str> {
    events.iter().map(|e| e["type"].as_str().unwrap()).collect()
}

fn lines(path: PathBuf) -> usize {
    fs::read_to_string(path).map_or(0, |s| s.lines().count())
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Run to a stop: `pas stop` during `slow`, then wait for `pas run` to exit.
fn stopped_run(fx: &Fixture, stop_args: &[&str]) {
    let child = fx.start_and_wait_for_slow();
    let out = fx.stop(stop_args);
    assert!(out.status.success(), "{}", stderr(&out));
    let status = wait_exit(child);
    assert_eq!(status.code(), Some(0), "a stopped Run exits 0");
}

// AC1: the stage finishes, then StopRequested and AttemptEnded{stopped}, exit 0.
#[test]
fn stop_during_a_stage_lets_it_finish_then_ends_stopped() {
    let fx = Fixture::new(SLOW_THEN_NEXT);
    let child = fx.start_and_wait_for_slow();
    let out = fx.stop(&["--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let json: Value = serde_json::from_str(stdout(&out).trim()).unwrap();
    assert_eq!(json["v"], 1);
    assert_eq!(json["ok"], true);
    assert_eq!(json["run_id"], fx.run_id().as_str());
    assert_eq!(json["already_requested"], false);
    assert!(json["stop_path"]
        .as_str()
        .unwrap()
        .ends_with("control/stop"));

    assert_eq!(wait_exit(child).code(), Some(0));
    assert!(fx.path().join("finished").exists(), "slow must finish");
    assert_eq!(lines(fx.path().join("next.count")), 0, "next must not run");

    let all = events(&fx.run_dir());
    let names = types(&all);
    assert!(all
        .iter()
        .any(|e| e["type"] == "StageCompleted" && e["data"]["node_id"] == "slow"));
    let stop = all.iter().find(|e| e["type"] == "StopRequested").unwrap();
    assert_eq!(stop["data"]["source"], "cli");
    let last = all.last().unwrap();
    assert_eq!(last["type"], "AttemptEnded", "{names:?}");
    assert_eq!(last["data"]["reason"], "stopped");
    assert_eq!(last["attempt"], 1);
    assert!(!names.contains(&"PipelineCompleted"));
    assert!(!names.contains(&"PipelineFailed"));
    let seqs: Vec<u64> = all.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
    assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>());

    let runs = fx.pas(&["runs"]).output().unwrap();
    assert!(stdout(&runs).contains("stopped"), "{}", stdout(&runs));
}

// AC2: the checkpoint names the next node; resume does not repeat `slow`.
#[test]
fn stop_checkpoint_resumes_at_the_next_node() {
    let fx = Fixture::new(SLOW_THEN_NEXT);
    stopped_run(&fx, &[]);
    let checkpoint: Value =
        serde_json::from_str(&fs::read_to_string(fx.logs().join("checkpoint.json")).unwrap())
            .unwrap();
    assert_eq!(checkpoint["current_node_id"], "next");
    assert_eq!(checkpoint["run_id"], fx.run_id().as_str());
    assert!(checkpoint["completed_nodes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n == "slow"));

    assert!(fx.run_cmd().status().unwrap().success());
    assert_eq!(lines(fx.path().join("slow.count")), 1, "slow ran once");
    assert_eq!(lines(fx.path().join("next.count")), 1);
    let all = events(&fx.run_dir());
    let second = all
        .iter()
        .find(|e| e["type"] == "AttemptStarted" && e["attempt"] == 2);
    assert_eq!(second.unwrap()["data"]["resumed_from_node"], "next");
}

// AC3: resume removes the stop file and the Run continues to completion.
#[test]
fn resume_removes_the_stop_file_and_continues() {
    let fx = Fixture::new(SLOW_THEN_NEXT);
    stopped_run(&fx, &[]);
    let stop_file = fx.run_dir().join("control").join("stop");
    assert!(stop_file.exists(), "the stop file stays after the stop");

    assert!(fx.run_cmd().status().unwrap().success());
    assert!(!stop_file.exists());
    let all = events(&fx.run_dir());
    let last = all.last().unwrap();
    assert_eq!(last["data"]["reason"], "completed");
    assert_eq!(last["attempt"], 2);
    assert_eq!(all.iter().filter(|e| e["type"] == "RunStarted").count(), 1);
}

// AC3 boundary: a stop file already present when an Attempt starts is stale.
#[test]
fn stale_stop_file_does_not_stop_the_next_attempt() {
    let fx = Fixture::new(SLOW_THEN_NEXT);
    stopped_run(&fx, &[]);
    // Still present; a second stale request is the same thing.
    assert!(fx.run_cmd().status().unwrap().success());
    assert_eq!(lines(fx.path().join("next.count")), 1);
    assert_eq!(
        events(&fx.run_dir()).last().unwrap()["data"]["reason"],
        "completed"
    );
}

// AC4: finished, unknown, and missing Runs are refused.
#[test]
fn stop_on_a_run_that_is_not_active_fails() {
    let fx = Fixture::new(SLOW_THEN_NEXT);
    stopped_run(&fx, &[]);
    assert!(fx.run_cmd().status().unwrap().success());
    let run_dir = fx.run_dir();
    let id = fx.run_id();

    // completed
    let out = fx.pas(&["stop", &id]).output().unwrap();
    assert!(!out.status.success());
    assert!(stderr(&out).contains("not active"), "{}", stderr(&out));
    assert!(!run_dir.join("control").join("stop").exists());

    let out = fx.pas(&["stop", &id, "--json"]).output().unwrap();
    assert!(!out.status.success());
    let json: Value = serde_json::from_str(stdout(&out).trim()).unwrap();
    assert_eq!(json["ok"], false);
    assert_eq!(json["run_id"], id.as_str());
    assert_eq!(json["error"]["code"], "not_active");
    assert!(json["error"]["message"]
        .as_str()
        .unwrap()
        .contains("not active"));

    // unknown
    let other = "0192a000-0000-7000-8000-0000000000ff";
    let out = fx.pas(&["stop", other]).output().unwrap();
    assert!(!out.status.success());
    assert!(stderr(&out).contains(other), "{}", stderr(&out));

    // missing folder
    fs::remove_dir_all(&run_dir).unwrap();
    let out = fx.pas(&["stop", &id, "--json"]).output().unwrap();
    assert!(!out.status.success());
    let json: Value = serde_json::from_str(stdout(&out).trim()).unwrap();
    assert_eq!(json["error"]["code"], "run_missing");
}

// AC4: a stopped Run is not active either.
#[test]
fn stop_on_a_stopped_run_fails() {
    let fx = Fixture::new(SLOW_THEN_NEXT);
    stopped_run(&fx, &[]);
    let out = fx.stop(&[]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("not active"), "{}", stderr(&out));
}

#[test]
fn second_stop_is_idempotent_and_source_is_recorded() {
    let fx = Fixture::new(SLOW_THEN_NEXT);
    let child = fx.start_and_wait_for_slow();
    assert!(fx.stop(&["--source", "monitor"]).status.success());
    let stop_file = fx.run_dir().join("control").join("stop");
    let first = fs::read_to_string(&stop_file).unwrap();
    let again = fx.stop(&["--json"]);
    // The Run may already have ended: only assert the repeat when it is still
    // active. slow sleeps 2 s, so it normally is.
    if again.status.success() {
        let json: Value = serde_json::from_str(stdout(&again).trim()).unwrap();
        assert_eq!(json["already_requested"], true);
        assert_eq!(fs::read_to_string(&stop_file).unwrap(), first);
    }
    assert_eq!(wait_exit(child).code(), Some(0));
    let all = events(&fx.run_dir());
    let stop = all.iter().find(|e| e["type"] == "StopRequested").unwrap();
    assert_eq!(stop["data"]["source"], "monitor");
}

#[test]
fn garbage_stop_file_still_stops_with_source_cli() {
    let fx = Fixture::new(SLOW_THEN_NEXT);
    let child = fx.start_and_wait_for_slow();
    let control = fx.run_dir().join("control");
    fs::create_dir_all(&control).unwrap();
    fs::write(control.join("stop"), "\u{0}not json").unwrap();
    assert_eq!(wait_exit(child).code(), Some(0));
    let all = events(&fx.run_dir());
    let stop = all.iter().find(|e| e["type"] == "StopRequested").unwrap();
    assert_eq!(stop["data"]["source"], "cli");
    assert_eq!(all.last().unwrap()["data"]["reason"], "stopped");
}

// Batch mode must not move on to the next pipeline after a stop.
#[test]
fn batch_stop_does_not_advance_to_the_next_pipeline() {
    let fx = Fixture::new(SLOW_THEN_NEXT);
    let batch = fx.path().join("batch");
    fs::create_dir_all(&batch).unwrap();
    fs::rename(fx.path().join("p.dot"), batch.join("a.dot")).unwrap();
    fs::write(
        batch.join("b.dot"),
        r#"digraph B {
            start [shape="Mdiamond"]
            t [shape="parallelogram", tool_command="touch b-ran"]
            done [shape="Msquare"]
            start -> t -> done
        }"#,
    )
    .unwrap();
    let mut command = fx.pas(&[
        "run",
        batch.to_str().unwrap(),
        "--workdir",
        fx.path().to_str().unwrap(),
    ]);
    command.stdout(Stdio::null()).stderr(Stdio::null());
    let child = command.spawn().unwrap();
    wait_for("slow to start", || fx.path().join("started").exists());
    let state = fx.path().join("state").join("runs.jsonl");
    let index: Value =
        serde_json::from_str(fs::read_to_string(&state).unwrap().lines().last().unwrap()).unwrap();
    let id = index["run_id"].as_str().unwrap().to_string();
    assert!(fx.pas(&["stop", &id]).output().unwrap().status.success());
    assert_eq!(wait_exit(child).code(), Some(0));
    assert!(!fx.path().join("b-ran").exists(), "b must not run");
    let entries = fs::read_to_string(&state).unwrap().lines().count();
    assert_eq!(entries, 1, "the second pipeline has no Run");
}
