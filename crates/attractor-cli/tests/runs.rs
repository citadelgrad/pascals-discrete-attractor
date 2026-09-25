#![cfg(unix)]
//! `pas runs [--active] [--json]` (spec File Change 12, C4, C6), observed
//! through the real binary with a private Run Index.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use attractor_journal::{append_entry_at, EventData, IndexEntry, JournalEvent, RunDir};
use chrono::Utc;
use serde_json::Value;

/// Blocks in a stage until a `go` file appears in the workdir.
const WAITS: &str = r#"digraph Waits {
    start [shape="Mdiamond"]
    wait [shape="parallelogram", timeout="120s", tool_command="while [ ! -f go ]; do sleep 0.05; done"]
    done [shape="Msquare"]
    start -> wait -> done
}"#;

/// Finishes at once.
const QUICK: &str = r#"digraph Quick {
    start [shape="Mdiamond"]
    step [shape="parallelogram", tool_command="true"]
    done [shape="Msquare"]
    start -> step -> done
}"#;

/// A scratch folder with Pipeline files, a workdir, logs, and a private
/// state folder (`state/`, holding the Run Index).
struct Scratch {
    dir: tempfile::TempDir,
}

impl Scratch {
    fn new() -> Self {
        let scratch = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        fs::create_dir_all(scratch.work()).unwrap();
        fs::write(scratch.path().join("waits.dot"), WAITS).unwrap();
        fs::write(scratch.path().join("quick.dot"), QUICK).unwrap();
        scratch
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn work(&self) -> PathBuf {
        self.path().join("work")
    }

    fn index(&self) -> PathBuf {
        self.path().join("state").join("runs.jsonl")
    }

    fn logs(&self, name: &str) -> PathBuf {
        self.path().join("logs").join(name)
    }

    /// `pas <args>` with this scratch folder's state folder.
    fn pas(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pas"));
        command
            .env("PAS_STATE_DIR", self.path().join("state"))
            .env_remove("PAS_HEARTBEAT_INTERVAL_MS")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env("GIT_CEILING_DIRECTORIES", self.path().parent().unwrap())
            .current_dir(self.path());
        command
    }

    fn run_command(&self, name: &str) -> Command {
        let mut command = self.pas();
        command
            .arg("run")
            .arg(self.path().join(format!("{name}.dot")))
            .arg("--workdir")
            .arg(self.work())
            .arg("--logs")
            .arg(self.logs(name));
        command
    }

    /// Run the [`QUICK`] Pipeline to completion; returns its Run folder.
    fn complete(&self, name: &str) -> PathBuf {
        fs::write(self.path().join(format!("{name}.dot")), QUICK).unwrap();
        let output = self.run_command(name).output().unwrap();
        assert_success(&output);
        only_run_dir(&self.logs(name))
    }

    /// Start the [`WAITS`] Pipeline and wait until its Attempt has started.
    fn hold(&self) -> Holder {
        let child = self
            .run_command("waits")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut holder = Holder {
            child,
            run_id: String::new(),
            go: self.work().join("go"),
        };
        let runs = self.logs("waits").join("runs");
        wait_for("AttemptStarted", Duration::from_secs(20), || {
            let Some(run_dir) = run_dirs(&runs).into_iter().next() else {
                return false;
            };
            let Some(started) = read_events(&run_dir)
                .into_iter()
                .find(|e| e["type"] == "AttemptStarted")
            else {
                return false;
            };
            holder.run_id = started["run_id"].as_str().unwrap().to_string();
            true
        });
        holder
    }

    fn runs(&self, extra: &[&str]) -> Output {
        self.pas().arg("runs").args(extra).output().unwrap()
    }

    /// `pas runs --json <extra>`: exit 0 and exactly one JSON object.
    fn runs_json(&self, extra: &[&str]) -> Value {
        let output = self
            .pas()
            .arg("runs")
            .arg("--json")
            .args(extra)
            .output()
            .unwrap();
        assert_success(&output);
        one_json(&output)
    }

    /// `run_id -> status` from `pas runs --json <extra>`.
    fn statuses(&self, extra: &[&str]) -> Vec<(String, String)> {
        let body = self.runs_json(extra);
        assert_eq!(body["v"], 1);
        assert_eq!(body["ok"], true);
        body["runs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r["run_id"].as_str().unwrap().to_string(),
                    r["status"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    /// A Run folder and Index entry written by hand: `RunStarted`,
    /// `AttemptStarted{pid}` and a `Heartbeat{pid}` `age` ago, and no
    /// `AttemptEnded` — what a process killed `age` ago leaves behind.
    fn unended_run(&self, pid: u32, age: chrono::Duration) -> String {
        let run_id = attractor_journal::new_run_id();
        let run_dir = self.path().join("hand").join(&run_id);
        fs::create_dir_all(&run_dir).unwrap();
        let then = Utc::now() - age;
        let started = then - chrono::Duration::seconds(30);
        let events = [
            EventData::RunStarted {
                pipeline_name: "Hand".into(),
                pipeline_path: "/p.dot".into(),
                workdir: "/w".into(),
                epic_id: None,
                max_budget_usd: None,
                max_steps: None,
                shared_workdir: false,
            },
            EventData::AttemptStarted {
                attempt: 1,
                pid,
                pas_version: "0".into(),
                argv: vec![],
                git_head: None,
                resumed_from_node: None,
            },
            EventData::Heartbeat { pid },
        ];
        let mut text = String::new();
        for (i, data) in events.into_iter().enumerate() {
            let ts = if matches!(data, EventData::Heartbeat { .. }) {
                then
            } else {
                started
            };
            let event = JournalEvent::new(i as u64 + 1, ts, &run_id, 1, data);
            text.push_str(&serde_json::to_string(&event).unwrap());
            text.push('\n');
        }
        fs::write(RunDir::from_path(&run_dir).events(), text).unwrap();
        let entry = IndexEntry::new(&run_id, started, "/w", "/p.dot", &run_dir);
        append_entry_at(&self.index(), &entry).unwrap();
        run_id
    }
}

/// A `pas run` of [`WAITS`] in the background. Released and reaped on drop.
struct Holder {
    child: Child,
    run_id: String,
    go: PathBuf,
}

impl Holder {
    fn release(&mut self) {
        fs::write(&self.go, "").unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "holder failed: {status:?}");
                return;
            }
            assert!(Instant::now() < deadline, "holder did not exit");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn wait_for(what: &str, timeout: Duration, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn run_dirs(runs: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(runs) else {
        return vec![];
    };
    let mut dirs: Vec<PathBuf> = entries.map(|e| e.unwrap().path()).collect();
    dirs.sort();
    dirs
}

fn only_run_dir(logs: &Path) -> PathBuf {
    let dirs = run_dirs(&logs.join("runs"));
    assert_eq!(dirs.len(), 1, "expected one Run folder, got {dirs:?}");
    dirs.into_iter().next().unwrap()
}

fn read_events(run_dir: &Path) -> Vec<Value> {
    fs::read_to_string(run_dir.join("events.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn run_id_of(run_dir: &Path) -> String {
    run_dir.file_name().unwrap().to_str().unwrap().to_string()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "pas failed ({:?})\nstdout:\n{}\nstderr:\n{}",
        output.status,
        stdout(output),
        stderr(output)
    );
}

/// Stdout holds exactly one JSON value (and nothing else).
fn one_json(output: &Output) -> Value {
    let text = stdout(output);
    let mut stream = serde_json::Deserializer::from_str(&text).into_iter::<Value>();
    let value = stream
        .next()
        .unwrap_or_else(|| panic!("no JSON on stdout: {text:?}"))
        .unwrap();
    assert!(
        stream.next().is_none(),
        "more than one JSON value: {text:?}"
    );
    assert_eq!(text.lines().count(), 1, "{text:?}");
    value
}

fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
    v.iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
}

/// PID of a process that has already exited and been reaped.
fn dead_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

/// AC: with one completed Run and one active Run, `pas runs` lists both and
/// `pas runs --active` lists only the active one.
#[test]
fn lists_completed_and_active_runs() {
    let s = Scratch::new();
    let done = run_id_of(&s.complete("quick"));
    let mut holder = s.hold();
    let active = holder.run_id.clone();

    assert_eq!(
        s.statuses(&[]),
        pairs(&[(&done, "completed"), (&active, "running")])
    );
    assert_eq!(s.statuses(&["--active"]), pairs(&[(&active, "running")]));

    let output = s.runs(&[]);
    assert_success(&output);
    let text = stdout(&output);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "{text}");
    assert!(lines[0].starts_with("RUN ID"), "{text}");
    assert!(lines[1].starts_with(&done) && lines[1].contains(" completed "));
    assert!(lines[2].starts_with(&active) && lines[2].contains(" running "));

    let output = s.runs(&["--active"]);
    assert_success(&output);
    let text = stdout(&output);
    assert_eq!(text.lines().count(), 2, "{text}");
    assert!(text.contains(&active) && !text.contains(&done), "{text}");

    // Once it finishes, nothing is active.
    holder.release();
    assert_eq!(
        s.statuses(&[]),
        pairs(&[(&done, "completed"), (&active, "completed")])
    );
    assert!(s.statuses(&["--active"]).is_empty());
    assert_eq!(stdout(&s.runs(&["--active"])), "(no active runs)\n");
}

/// AC: no AttemptEnded, last Heartbeat older than 2 minutes, PID not alive
/// ⇒ `crashed`.
#[test]
fn crashed_run_shows_crashed() {
    let s = Scratch::new();
    let crashed = s.unended_run(dead_pid(), chrono::Duration::minutes(10));
    assert_eq!(s.statuses(&[]), pairs(&[(&crashed, "crashed")]));
    assert!(s.statuses(&["--active"]).is_empty());
    let text = stdout(&s.runs(&[]));
    assert!(
        text.contains(&crashed) && text.contains(" crashed "),
        "{text}"
    );
}

/// Each crashed condition on its own is not enough.
#[test]
fn crashed_needs_old_heartbeat_and_dead_pid() {
    let s = Scratch::new();
    // Old Heartbeat, PID alive (this test process): running.
    let hung = s.unended_run(std::process::id(), chrono::Duration::minutes(10));
    // Dead PID, Heartbeat 30 s old: running.
    let recent = s.unended_run(dead_pid(), chrono::Duration::seconds(30));
    assert_eq!(
        s.statuses(&[]),
        pairs(&[(&hung, "running"), (&recent, "running")])
    );
    assert_eq!(s.statuses(&["--active"]).len(), 2);
}

/// A real `kill -9` leaves no AttemptEnded; the Run stays `running` until its
/// last sign of life is 2 minutes old.
#[test]
fn killed_run_is_running_until_two_minutes() {
    let s = Scratch::new();
    let mut holder = s.hold();
    holder.child.kill().unwrap();
    holder.child.wait().unwrap();
    let run_dir = only_run_dir(&s.logs("waits"));
    assert!(read_events(&run_dir)
        .iter()
        .all(|e| e["type"] != "AttemptEnded"));
    assert_eq!(s.statuses(&[]), pairs(&[(&holder.run_id, "running")]));
}

/// AC: an Index entry whose run_dir no longer exists shows `missing` and
/// does not cause an error.
#[test]
fn missing_run_dir_shows_missing() {
    let s = Scratch::new();
    let gone_dir = s.complete("gone");
    let gone = run_id_of(&gone_dir);
    let kept = run_id_of(&s.complete("kept"));
    fs::remove_dir_all(&gone_dir).unwrap();

    assert_eq!(
        s.statuses(&[]),
        pairs(&[(&gone, "missing"), (&kept, "completed")])
    );
    let output = s.runs(&[]);
    assert_success(&output);
    assert!(stdout(&output).contains(" missing "), "{}", stdout(&output));
    assert!(stderr(&output).is_empty(), "{}", stderr(&output));
    assert!(s.statuses(&["--active"]).is_empty());
}

/// AC: `pas runs --json` prints one object `{v:1, ok:true, runs:[...]}` with
/// run_id, workdir, pipeline_path, started_at, and status per Run.
#[test]
fn json_shape() {
    let s = Scratch::new();
    s.complete("quick");
    let body = s.runs_json(&[]);
    assert_eq!(body["v"], 1);
    assert_eq!(body["ok"], true);
    let runs = body["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1);
    let run = &runs[0];

    let index: Value = serde_json::from_str(fs::read_to_string(s.index()).unwrap().trim()).unwrap();
    for key in ["run_id", "workdir", "pipeline_path", "started_at"] {
        assert!(run[key].is_string(), "{key} in {run}");
        assert_eq!(run[key], index[key], "{key}");
    }
    assert_eq!(run["status"], "completed");
    // Envelope keys, then each Run as its Index line with `status` appended.
    let line = fs::read_to_string(s.index()).unwrap();
    let text = stdout(&s.runs(&["--json"]));
    let run_text = format!(
        "{},\"status\":\"completed\"}}",
        line.trim().trim_end_matches('}')
    );
    assert_eq!(
        text.trim(),
        format!("{{\"v\":1,\"ok\":true,\"runs\":[{run_text}]}}")
    );
    // Nothing but the Index entry and its status.
    let mut expected = index.clone();
    expected["status"] = "completed".into();
    assert_eq!(run, &expected);
    // Status is never written back to the Index.
    assert!(index.get("status").is_none());
}

/// AC: with an absent Index, `pas runs --json` prints `{v:1,ok:true,runs:[]}`
/// and exits 0 — and creates nothing.
#[test]
fn json_with_absent_index() {
    let s = Scratch::new();
    let empty = serde_json::json!({"v": 1, "ok": true, "runs": []});
    assert_eq!(s.runs_json(&[]), empty);
    assert_eq!(s.runs_json(&["--active"]), empty);
    assert!(!s.path().join("state").exists());
    let output = s.runs(&[]);
    assert_success(&output);
    assert_eq!(stdout(&output), "(no runs)\n");
}

/// AC: with an empty Index, `pas runs --json` prints `{v:1,ok:true,runs:[]}`
/// and exits 0.
#[test]
fn json_with_empty_index() {
    let s = Scratch::new();
    fs::create_dir_all(s.index().parent().unwrap()).unwrap();
    fs::write(s.index(), "").unwrap();
    let empty = serde_json::json!({"v": 1, "ok": true, "runs": []});
    assert_eq!(s.runs_json(&[]), empty);
    assert_eq!(s.runs_json(&["--active"]), empty);
}

/// No state folder can be resolved at all: no Index, so an empty list.
#[test]
fn json_without_any_state_folder() {
    let s = Scratch::new();
    let output = s
        .pas()
        .args(["runs", "--json"])
        .env_remove("PAS_STATE_DIR")
        .env_remove("XDG_STATE_HOME")
        .env_remove("HOME")
        .output()
        .unwrap();
    assert_success(&output);
    assert_eq!(
        one_json(&output),
        serde_json::json!({"v": 1, "ok": true, "runs": []})
    );
}

/// An Index that exists but cannot be read is an error: one JSON object
/// with `ok:false`, and exit 1.
#[test]
fn unreadable_index_is_an_error() {
    use std::os::unix::fs::PermissionsExt;
    let s = Scratch::new();
    s.complete("quick");
    fs::set_permissions(s.index(), fs::Permissions::from_mode(0o000)).unwrap();
    if fs::File::open(s.index()).is_ok() {
        eprintln!("skipped: running with permission to read a mode-000 file");
        return;
    }
    let output = s.runs(&["--json"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let body = one_json(&output);
    assert_eq!(body["v"], 1);
    assert_eq!(body["ok"], false);
    assert_eq!(body["error"]["code"], "index_unreadable");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("runs.jsonl"));

    let output = s.runs(&[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).is_empty());
    assert!(stderr(&output).contains("cannot read the Run Index"));
}

/// One unreadable journal is a warning; the listing still succeeds.
#[test]
fn corrupt_journal_does_not_fail_listing() {
    let s = Scratch::new();
    let bad_dir = s.complete("bad");
    let bad = run_id_of(&bad_dir);
    let good = run_id_of(&s.complete("good"));
    fs::write(bad_dir.join("events.jsonl"), "not json\n").unwrap();

    let output = s.runs(&["--json"]);
    assert_success(&output);
    let runs = one_json(&output)["runs"].as_array().unwrap().clone();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0]["run_id"], bad.as_str());
    assert_eq!(runs[1]["run_id"], good.as_str());
    assert_eq!(runs[1]["status"], "completed");
    let err = stderr(&output);
    assert!(err.contains("warning:") && err.contains(&bad), "{err}");
}

/// `pas runs` takes no Run lock: listing while a Run holds its locks
/// neither blocks nor disturbs it, and a new `pas run` still starts.
#[test]
fn listing_does_not_take_run_locks() {
    let s = Scratch::new();
    let mut holder = s.hold();
    let started = Instant::now();
    assert_eq!(s.statuses(&["--active"]).len(), 1);
    assert!(started.elapsed() < Duration::from_secs(10));
    let quick = s.complete("quick");
    holder.release();
    assert_eq!(s.statuses(&[]).len(), 2);
    assert!(quick.exists());
}

#[test]
fn help_lists_runs() {
    let s = Scratch::new();
    let output = s.pas().arg("--help").output().unwrap();
    assert_success(&output);
    assert!(stdout(&output).contains("runs"), "{}", stdout(&output));
    let output = s.pas().args(["runs", "--help"]).output().unwrap();
    assert_success(&output);
    let text = stdout(&output);
    assert!(
        text.contains("--active") && text.contains("--json"),
        "{text}"
    );
}
