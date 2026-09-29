#![cfg(unix)]
//! `pas kill` (spec File Change 12, C1, C3, C5, C6), observed through the
//! real binary.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use attractor_journal::{append_entry_at, IndexEntry, PipelineDir, INDEX_FILE};
use serde_json::Value;
use std::os::unix::process::CommandExt;

mod common;

fn pas() -> &'static str {
    env!("CARGO_BIN_EXE_pas")
}

const RUN: &str = "0192a000-0000-7000-8000-000000000001";

fn wait_for(what: &str, ready: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 only probes.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

fn gone(pid: u32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive(pid) {
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

/// Spawn `command` in its own process group; a thread reaps it, so it does
/// not linger as a zombie after it is killed.
fn spawn_leader(mut command: Command) -> u32 {
    command
        .process_group(0)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child: Child = command.spawn().unwrap();
    let pid = child.id();
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    pid
}

fn read_pid(path: &Path) -> u32 {
    wait_for("a pid file", || {
        fs::read_to_string(path).is_ok_and(|s| s.trim().parse::<u32>().is_ok())
    });
    fs::read_to_string(path).unwrap().trim().parse().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn json(output: &Output) -> Value {
    serde_json::from_str(stdout(output).trim()).unwrap()
}

fn sentinel() -> u32 {
    let mut command = Command::new("sleep");
    command.arg("300");
    spawn_leader(command)
}

/// A hand-built Run: Index entry, journal, and Pipeline lock file.
struct Synthetic {
    dir: tempfile::TempDir,
}

impl Synthetic {
    /// `journal_tail` is appended after `AttemptStarted`; `heartbeat_pid` is
    /// the PID of the Heartbeat; `ts` its timestamp.
    fn new(heartbeat_pid: u32, journal_tail: &[&str], ts: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let pipeline = PipelineDir::new(dir.path().join("pipe"));
        let run_dir = pipeline.run(RUN).unwrap();
        run_dir.create_all().unwrap();
        let line = |seq: u32, ty: &str, data: String| {
            format!(
                r#"{{"v":1,"seq":{seq},"ts":"{ts}","run_id":"{RUN}","attempt":1,"type":"{ty}","data":{data}}}"#
            )
        };
        let mut lines = vec![
            line(
                1,
                "AttemptStarted",
                format!(
                    r#"{{"attempt":1,"pid":{heartbeat_pid},"argv":[],"pas_version":"0","resumed_from_node":null}}"#
                ),
            ),
            line(2, "Heartbeat", format!(r#"{{"pid":{heartbeat_pid}}}"#)),
        ];
        for (i, reason) in journal_tail.iter().enumerate() {
            lines.push(line(
                3 + i as u32,
                "AttemptEnded",
                format!(r#"{{"attempt":1,"reason":"{reason}"}}"#),
            ));
        }
        fs::write(run_dir.events(), lines.join("\n") + "\n").unwrap();
        let state = dir.path().join("state");
        fs::create_dir_all(&state).unwrap();
        let entry = IndexEntry::new(
            RUN,
            chrono::Utc::now(),
            "/w",
            "/p.dot",
            run_dir.path().to_path_buf(),
        );
        append_entry_at(&state.join(INDEX_FILE), &entry).unwrap();
        Self { dir }
    }

    fn fresh(heartbeat_pid: u32, journal_tail: &[&str]) -> Self {
        let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        Self::new(heartbeat_pid, journal_tail, &ts)
    }

    fn lock_path(&self) -> PathBuf {
        self.dir.path().join("pipe").join("run.lock")
    }

    /// Write the lock file's contents and hold the `flock` in this test.
    fn hold_lock(&self, pid: u32, run_id: &str) -> File {
        fs::write(
            self.lock_path(),
            format!(r#"{{"pid":{pid},"run_id":"{run_id}"}}"#),
        )
        .unwrap();
        let file = File::open(self.lock_path()).unwrap();
        file.lock().unwrap();
        file
    }

    fn kill(&self, args: &[&str]) -> Output {
        Command::new(pas())
            .arg("kill")
            .arg(RUN)
            .args(args)
            .env("PAS_STATE_DIR", self.dir.path().join("state"))
            .output()
            .unwrap()
    }
}

// AC3: a Heartbeat PID that does not hold the lock gets no signal.
#[test]
fn pid_not_holding_the_lock_is_refused_without_a_signal() {
    let victim = sentinel();

    // Nobody holds the lock.
    let run = Synthetic::fresh(victim, &[]);
    fs::write(
        run.lock_path(),
        format!(r#"{{"pid":{victim},"run_id":"{RUN}"}}"#),
    )
    .unwrap();
    let out = run.kill(&["--json"]);
    assert_eq!(out.status.code(), Some(1));
    let v = json(&out);
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "pid_not_lock_holder");
    assert!(alive(victim), "no signal may be sent");

    // The lock is held, but the file names another PID.
    let run = Synthetic::fresh(victim, &[]);
    let _lock = run.hold_lock(std::process::id(), RUN);
    let out = run.kill(&["--json"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(json(&out)["error"]["code"], "pid_not_lock_holder");
    assert!(alive(victim));

    // The lock is held by that PID, but for another Run.
    let run = Synthetic::fresh(victim, &[]);
    let _lock = run.hold_lock(victim, "0192a000-0000-7000-8000-0000000000ff");
    let out = run.kill(&[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(alive(victim));

    // No lock file at all.
    let run = Synthetic::fresh(victim, &[]);
    let out = run.kill(&[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(!run.lock_path().exists(), "the probe must create nothing");
    assert!(alive(victim));
}

// AC4: a Run that is not active exits non-zero and sends no signal.
#[test]
fn kill_on_a_run_that_is_not_active_fails_without_a_signal() {
    let victim = sentinel();
    for reason in ["completed", "failed", "stopped"] {
        let run = Synthetic::fresh(victim, &[reason]);
        let _lock = run.hold_lock(victim, RUN);
        let out = run.kill(&["--json"]);
        assert_eq!(out.status.code(), Some(1), "{reason}");
        let v = json(&out);
        assert_eq!(v["error"]["code"], "not_active");
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not active"));
        assert!(alive(victim), "{reason}: no signal may be sent");
    }

    // Crashed: old journal and a PID that no longer exists.
    let mut done = Command::new("true").spawn().unwrap();
    done.wait().unwrap();
    let run = Synthetic::new(done.id(), &[], "2026-09-24T10:00:00.000Z");
    let out = run.kill(&[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not active"));

    // Unknown Run ID.
    let out = Command::new(pas())
        .args(["kill", "0192a000-0000-7000-8000-0000000000ff"])
        .env("PAS_STATE_DIR", run.dir.path().join("state"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(alive(victim));
}

#[test]
fn an_invalid_grace_fails_before_any_signal() {
    let victim = sentinel();
    let run = Synthetic::fresh(victim, &[]);
    let _lock = run.hold_lock(victim, RUN);
    let out = run.kill(&["--grace", "soon", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(json(&out)["error"]["code"], "invalid_grace");
    assert!(alive(victim));
}

// AC2 (SIGTERM honoured): ends well before the grace period.
#[test]
fn sigterm_honouring_process_ends_before_grace() {
    let mut command = Command::new("sleep");
    command.arg("300");
    let pid = spawn_leader(command);
    let run = Synthetic::fresh(pid, &[]);
    let _lock = run.hold_lock(pid, RUN);
    let started = Instant::now();
    let out = run.kill(&["--grace", "20s", "--json"]);
    assert!(out.status.success(), "{}", stdout(&out));
    assert!(started.elapsed() < Duration::from_secs(10));
    let v = json(&out);
    assert_eq!((v["v"].clone(), v["ok"].clone()), (1.into(), true.into()));
    assert_eq!(v["run_id"], RUN);
    assert_eq!(v["pid"], pid);
    assert_eq!(v["signal"], "SIGTERM");
    assert!(gone(pid));
}

// AC2: a process that ignores SIGTERM is killed after the grace period ± 1 s.
#[test]
fn sigterm_ignoring_run_is_killed_after_grace() {
    let mut command = Command::new("sh");
    command.args(["-c", "trap '' TERM; while :; do sleep 1; done"]);
    let pid = spawn_leader(command);
    let run = Synthetic::fresh(pid, &[]);
    let _lock = run.hold_lock(pid, RUN);
    std::thread::sleep(Duration::from_millis(300)); // let the trap install
    let started = Instant::now();
    let out = run.kill(&["--grace", "2s", "--json"]);
    let elapsed = started.elapsed();
    assert!(out.status.success(), "{}", stdout(&out));
    assert!(
        elapsed >= Duration::from_millis(1900) && elapsed <= Duration::from_millis(3000),
        "elapsed {elapsed:?}"
    );
    assert_eq!(json(&out)["signal"], "SIGKILL");
    assert!(gone(pid));
}

// AC1 (escalation): provider children in their own group die with the Run.
#[test]
fn kill_escalation_also_ends_provider_children() {
    let dir = tempfile::tempdir().unwrap();
    let child_file = dir.path().join("child.pid");
    let script = format!(
        "import signal,subprocess,time\n\
         signal.signal(signal.SIGTERM, signal.SIG_IGN)\n\
         p = subprocess.Popen(['sleep','300'], start_new_session=True)\n\
         open({:?}, 'w').write(str(p.pid))\n\
         time.sleep(300)\n",
        child_file.to_str().unwrap()
    );
    let mut command = Command::new("python3");
    command.args(["-c", &script]);
    let pid = spawn_leader(command);
    let child = read_pid(&child_file);
    let run = Synthetic::fresh(pid, &[]);
    let _lock = run.hold_lock(pid, RUN);
    std::thread::sleep(Duration::from_millis(300));
    let out = run.kill(&["--grace", "1s", "--json"]);
    assert!(out.status.success(), "{}", stdout(&out));
    let v = json(&out);
    assert_eq!(v["signal"], "SIGKILL");
    assert!(v["children_killed"].as_u64().unwrap() >= 1);
    assert!(gone(pid));
    assert!(gone(child), "the provider child must not survive");
}

const BLOCKING: &str = concat!(
    "digraph Kill {\n",
    "    start [shape=\"Mdiamond\"]\n",
    "    block [shape=\"parallelogram\", tool_command=\"echo $$ > stage.pid; ",
    common::wait_for_go!(),
    "\"]\n",
    "    done [shape=\"Msquare\"]\n",
    "    start -> block -> done\n",
    "}\n"
);

// AC1: a real `pas run` and its stage child both end.
#[test]
fn kill_ends_the_run_and_its_provider_children() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("p.dot"), BLOCKING).unwrap();
    let state = dir.path().join("state");
    let logs = dir.path().join("logs");
    let mut command = Command::new(pas());
    command
        .args([
            "run",
            dir.path().join("p.dot").to_str().unwrap(),
            "--workdir",
            dir.path().to_str().unwrap(),
            "--logs",
            logs.to_str().unwrap(),
        ])
        .env("PAS_STATE_DIR", &state)
        .env("PAS_HEARTBEAT_INTERVAL_MS", "100")
        .current_dir(dir.path());
    let run_pid = spawn_leader(command);
    let stage_pid = read_pid(&dir.path().join("stage.pid"));
    let run_dir = fs::read_dir(logs.join("runs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let run_id = run_dir.file_name().unwrap().to_string_lossy().into_owned();
    // Wait for a Heartbeat, so the last PID comes from the journal.
    wait_for("a Heartbeat", || {
        fs::read_to_string(run_dir.join("events.jsonl")).is_ok_and(|s| s.contains("Heartbeat"))
    });

    let out = Command::new(pas())
        .args(["kill", &run_id, "--json"])
        .env("PAS_STATE_DIR", &state)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stdout(&out));
    assert_eq!(json(&out)["pid"], run_pid);
    assert!(gone(run_pid), "pas run must end");
    assert!(gone(stage_pid), "the provider child must end");

    let journal = fs::read_to_string(run_dir.join("events.jsonl")).unwrap();
    let last: Value = serde_json::from_str(journal.lines().last().unwrap()).unwrap();
    assert_eq!(last["type"], "AttemptEnded");
    assert_eq!(last["data"]["reason"], "stopped");

    // The Run is no longer active.
    let again = Command::new(pas())
        .args(["kill", &run_id])
        .env("PAS_STATE_DIR", &state)
        .output()
        .unwrap();
    assert_eq!(again.status.code(), Some(1));
}
