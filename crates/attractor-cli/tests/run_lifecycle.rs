#![cfg(unix)]
//! `pas run` Run lifecycle (spec C1, C2, C3, C4, C6): Run folder, run.json,
//! Run Index entry, and Attempt lifecycle Events, observed through the real
//! binary.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const RUN_ID: &str = "01920000-0000-7000-8000-00000000abcd";
/// Test hook that shortens the 30 s Heartbeat interval.
const HEARTBEAT_ENV: &str = "PAS_HEARTBEAT_INTERVAL_MS";

fn pas() -> &'static str {
    env!("CARGO_BIN_EXE_pas")
}

/// A scratch folder with a Pipeline file, a logs folder, and a state folder
/// holding the Run Index.
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

    fn pipeline(&self) -> PathBuf {
        self.path().join("p.dot")
    }

    fn logs(&self) -> PathBuf {
        self.path().join("logs")
    }

    fn runs(&self) -> PathBuf {
        self.logs().join("runs")
    }

    fn index(&self) -> PathBuf {
        self.path().join("state").join("runs.jsonl")
    }

    /// `pas run <p.dot> --workdir <dir> --logs <logs> <extra...>`.
    fn args(&self, extra: &[&str]) -> Vec<String> {
        let mut args: Vec<String> = vec![
            "run".into(),
            self.pipeline().display().to_string(),
            "--workdir".into(),
            self.path().display().to_string(),
            "--logs".into(),
            self.logs().display().to_string(),
        ];
        args.extend(extra.iter().map(|s| s.to_string()));
        args
    }

    fn command(&self, extra: &[&str]) -> Command {
        let mut command = Command::new(pas());
        command
            .args(self.args(extra))
            .env("PAS_STATE_DIR", self.path().join("state"))
            .env_remove(HEARTBEAT_ENV)
            .current_dir(self.path());
        command
    }

    fn run(&self, extra: &[&str]) -> Output {
        self.command(extra).output().unwrap()
    }

    fn run_dirs(&self) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(self.runs()) else {
            return vec![];
        };
        let mut dirs: Vec<PathBuf> = entries.map(|e| e.unwrap().path()).collect();
        dirs.sort();
        dirs
    }

    fn only_run_dir(&self) -> PathBuf {
        let dirs = self.run_dirs();
        assert_eq!(dirs.len(), 1, "expected one Run folder, got {dirs:?}");
        dirs.into_iter().next().unwrap()
    }

    fn index_lines(&self) -> Vec<Value> {
        match fs::read_to_string(self.index()) {
            Ok(text) => text
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect(),
            Err(_) => vec![],
        }
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

fn of_type<'a>(events: &'a [Value], ty: &str) -> Vec<&'a Value> {
    events.iter().filter(|e| e["type"] == ty).collect()
}

fn run_json(run_dir: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(run_dir.join("run.json")).unwrap()).unwrap()
}

fn dir_name(path: &Path) -> String {
    path.file_name().unwrap().to_string_lossy().into_owned()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "pas run failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_failure(output: &Output) {
    assert!(
        !output.status.success(),
        "pas run unexpectedly succeeded\nstdout:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

/// Three stages between start and exit; runs in `--dry-run`.
const THREE_STAGES: &str = r#"digraph Three {
    node [llm_provider="claude"]
    start [shape="Mdiamond"]
    a [shape="box", prompt="a"]
    b [shape="box", prompt="b"]
    c [shape="box", prompt="c"]
    done [shape="Msquare"]
    start -> a -> b -> c -> done
}"#;

/// Fails at the exit's goal gate and keeps its checkpoint, so it can resume.
const FAILS_AT_GATE: &str = r#"digraph Gate {
    start [shape="Mdiamond"]
    first [shape="parallelogram", tool_command="echo first >> trail"]
    check [shape="parallelogram", goal_gate=true, tool_command="test -f gate-open"]
    done [shape="Msquare"]
    start -> first -> check -> done
}"#;

fn assert_contiguous_seq(events: &[Value]) {
    let seqs: Vec<u64> = events.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
    let expected: Vec<u64> = (1..=seqs.len() as u64).collect();
    assert_eq!(seqs, expected, "seq must run 1..n without gaps");
}

// AC: fresh run creates run.json + events.jsonl and one Index line; the
// run_id is a UUID v7; AttemptEnded is `completed`.
#[test]
fn fresh_run_creates_run_folder_journal_and_one_index_entry() {
    let fx = Fixture::new(THREE_STAGES);
    let output = fx.run(&["--dry-run"]);
    assert_success(&output);

    let run_dir = fx.only_run_dir();
    let run_id = dir_name(&run_dir);
    let uuid = uuid::Uuid::parse_str(&run_id).unwrap();
    assert_eq!(
        uuid.get_version_num(),
        7,
        "Run ID must be UUID v7: {run_id}"
    );
    assert_eq!(
        run_id,
        uuid.hyphenated().to_string(),
        "lowercase hyphenated"
    );

    let meta = run_json(&run_dir);
    assert_eq!(meta["v"], 1);
    assert_eq!(meta["run_id"], run_id.as_str());
    assert_eq!(meta["pipeline_name"], "Three");
    for key in ["pipeline_path", "workdir", "logs_dir"] {
        assert!(
            Path::new(meta[key].as_str().unwrap()).is_absolute(),
            "{key} must be absolute: {meta}"
        );
    }

    let index = fx.index_lines();
    assert_eq!(index.len(), 1, "exactly one Index line: {index:?}");
    assert_eq!(index[0]["run_id"], run_id.as_str());
    let indexed_dir = PathBuf::from(index[0]["run_dir"].as_str().unwrap());
    assert!(indexed_dir.is_absolute());
    assert_eq!(
        fs::canonicalize(&indexed_dir).unwrap(),
        fs::canonicalize(&run_dir).unwrap()
    );

    let events = events(&run_dir);
    let types = types(&events);
    assert_eq!(
        &types[..3],
        ["RunStarted", "AttemptStarted", "PipelineStarted"]
    );
    assert_eq!(types.last(), Some(&"AttemptEnded"));
    assert!(types.contains(&"PipelineCompleted"));
    assert_contiguous_seq(&events);
    assert!(events
        .iter()
        .all(|e| e["run_id"] == run_id.as_str() && e["attempt"] == 1));
    assert_eq!(events[1]["data"]["attempt"], 1);
    assert_eq!(events.last().unwrap()["data"]["reason"], "completed");
    assert!(
        !fx.logs().join("checkpoint.json").exists(),
        "a completed Run clears its checkpoint"
    );

    // Without --json, stdout is still the human report.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Pipeline completed"), "{stdout}");
    assert!(stdout.contains(&format!("Run: {run_id}")), "{stdout}");
}

// AC: resuming appends to the same journal, attempt + 1, seq continues, no
// Index line, run.json unchanged. Also AC: AttemptEnded `failed`.
#[test]
fn resume_appends_attempt_to_same_journal() {
    let fx = Fixture::new(FAILS_AT_GATE);
    let first = fx.run(&[]);
    assert_failure(&first);

    let run_dir = fx.only_run_dir();
    let run_id = dir_name(&run_dir);
    let attempt1 = events(&run_dir);
    let last1 = attempt1.last().unwrap();
    assert_eq!(last1["type"], "AttemptEnded");
    assert_eq!(last1["data"]["reason"], "failed");
    assert!(last1["data"]["message"]
        .as_str()
        .unwrap()
        .contains("Goal gate unsatisfied"));
    let checkpoint: Value =
        serde_json::from_str(&fs::read_to_string(fx.logs().join("checkpoint.json")).unwrap())
            .unwrap();
    assert_eq!(
        checkpoint["run_id"],
        run_id.as_str(),
        "checkpoint names the Run"
    );
    let meta_before = fs::read(run_dir.join("run.json")).unwrap();

    let second = fx.run(&[]);
    assert_failure(&second);

    assert_eq!(
        fx.only_run_dir(),
        run_dir,
        "resume must not create a folder"
    );
    assert_eq!(fs::read(run_dir.join("run.json")).unwrap(), meta_before);
    assert_eq!(fx.index_lines().len(), 1, "resume adds no Index line");

    let all = events(&run_dir);
    assert_eq!(
        &all[..attempt1.len()],
        &attempt1[..],
        "earlier lines unchanged"
    );
    let attempt2 = &all[attempt1.len()..];
    assert_eq!(attempt2[0]["type"], "AttemptStarted");
    assert_eq!(attempt2[0]["attempt"], 2);
    assert_eq!(attempt2[0]["data"]["attempt"], 2);
    assert!(attempt2[0]["data"]["resumed_from_node"].is_string());
    assert_eq!(
        attempt2[0]["seq"].as_u64().unwrap(),
        last1["seq"].as_u64().unwrap() + 1
    );
    assert_contiguous_seq(&all);
    assert_eq!(of_type(&all, "RunStarted").len(), 1, "RunStarted only once");
    assert!(attempt2
        .iter()
        .all(|e| e["attempt"] == 2 && e["run_id"] == run_id.as_str()));
    assert_eq!(attempt2.last().unwrap()["type"], "AttemptEnded");
    assert_eq!(attempt2.last().unwrap()["data"]["attempt"], 2);
}

// AC: --fresh creates a second Run folder and leaves the first unchanged.
#[test]
fn fresh_flag_starts_new_run_and_keeps_old_folder() {
    let fx = Fixture::new(FAILS_AT_GATE);
    assert_failure(&fx.run(&[]));
    let first_dir = fx.only_run_dir();
    let snapshot: Vec<(String, Vec<u8>)> = ["run.json", "events.jsonl"]
        .iter()
        .map(|f| (f.to_string(), fs::read(first_dir.join(f)).unwrap()))
        .collect();

    assert_failure(&fx.run(&["--fresh"]));

    let dirs = fx.run_dirs();
    assert_eq!(dirs.len(), 2, "{dirs:?}");
    let second_dir = dirs.iter().find(|d| **d != first_dir).unwrap();
    assert_ne!(dir_name(second_dir), dir_name(&first_dir));
    for (file, bytes) in snapshot {
        assert_eq!(
            fs::read(first_dir.join(&file)).unwrap(),
            bytes,
            "{file} changed"
        );
    }
    let index = fx.index_lines();
    assert_eq!(index.len(), 2);
    assert_eq!(index[1]["run_id"], dir_name(second_dir).as_str());
    let second = events(second_dir);
    assert_eq!(types(&second)[..2], ["RunStarted", "AttemptStarted"]);
    assert_eq!(second[1]["data"]["attempt"], 1);
}

// AC: --run-id <uuid> names the Run.
#[test]
fn run_id_flag_names_the_run() {
    let fx = Fixture::new(THREE_STAGES);
    // Upper case is accepted and normalized.
    assert_success(&fx.run(&["--dry-run", "--run-id", &RUN_ID.to_uppercase()]));

    let run_dir = fx.only_run_dir();
    assert_eq!(dir_name(&run_dir), RUN_ID);
    assert_eq!(run_json(&run_dir)["run_id"], RUN_ID);
    assert_eq!(fx.index_lines()[0]["run_id"], RUN_ID);
    assert!(events(&run_dir).iter().all(|e| e["run_id"] == RUN_ID));

    // A second new Run with the same ID is refused and changes nothing.
    let journal = fs::read(run_dir.join("events.jsonl")).unwrap();
    let again = fx.run(&["--dry-run", "--run-id", RUN_ID]);
    assert_failure(&again);
    assert!(String::from_utf8_lossy(&again.stderr).contains("already exists"));
    assert_eq!(fs::read(run_dir.join("events.jsonl")).unwrap(), journal);
    assert_eq!(fx.index_lines().len(), 1);
}

// AC: an invalid --run-id exits non-zero before creating any folder (and
// before touching the checkpoint, even with --fresh).
#[test]
fn invalid_run_id_fails_before_creating_anything() {
    for bad in ["nope", "../escape", "", "01920000-0000-7000-8000"] {
        for json in [false, true] {
            let fx = Fixture::new(THREE_STAGES);
            fs::create_dir_all(fx.logs()).unwrap();
            let sentinel = fx.logs().join("checkpoint.json");
            fs::write(&sentinel, "sentinel").unwrap();

            let mut extra = vec!["--dry-run", "--fresh", "--run-id", bad];
            if json {
                extra.push("--json");
            }
            let output = fx.run(&extra);

            assert_failure(&output);
            assert!(!fx.runs().exists(), "no runs/ for {bad:?}");
            assert!(!fx.path().join("escape").exists());
            assert!(!fx.index().exists(), "no Index for {bad:?}");
            assert_eq!(fs::read_to_string(&sentinel).unwrap(), "sentinel");
            if json {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let first: Value = serde_json::from_str(stdout.lines().next().unwrap()).unwrap();
                assert_eq!(first["v"], 1);
                assert_eq!(first["ok"], false);
                assert_eq!(first["error"]["code"], "invalid_run_id");
                assert!(first["error"]["message"].is_string());
            }
        }
    }
}

// A --run-id that differs from the checkpoint's Run is refused.
#[test]
fn run_id_conflicting_with_checkpoint_is_rejected() {
    let fx = Fixture::new(FAILS_AT_GATE);
    assert_failure(&fx.run(&[]));
    let run_dir = fx.only_run_dir();
    let journal = fs::read(run_dir.join("events.jsonl")).unwrap();

    let output = fx.run(&["--run-id", RUN_ID, "--json"]);
    assert_failure(&output);
    let first: Value = serde_json::from_str(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(first["error"]["code"], "run_id_mismatch");
    assert_eq!(fx.run_dirs(), vec![run_dir.clone()]);
    assert_eq!(fs::read(run_dir.join("events.jsonl")).unwrap(), journal);

    // The checkpoint's own ID resumes it.
    let own = dir_name(&run_dir);
    assert_failure(&fx.run(&["--run-id", &own]));
    assert_eq!(fx.run_dirs(), vec![run_dir.clone()]);
    assert_eq!(of_type(&events(&run_dir), "AttemptStarted").len(), 2);
}

// AC: AttemptEnded `max_steps`.
#[test]
fn max_steps_ends_attempt_with_max_steps() {
    let fx = Fixture::new(THREE_STAGES);
    assert_failure(&fx.run(&["--dry-run", "--max-steps", "1"]));
    let events = events(&fx.only_run_dir());
    let last = events.last().unwrap();
    assert_eq!(last["type"], "AttemptEnded");
    assert_eq!(last["data"]["reason"], "max_steps");
    assert_eq!(
        last["data"]["message"],
        "Pipeline exceeded maximum step count (1). Use --max-steps to increase."
    );
}

// AC: AttemptEnded `budget_exhausted`. A `claude` shim reports $0.05 per
// stage against a $0.01 budget, so the second stage trips the limit.
#[test]
fn budget_exceeded_ends_attempt_with_budget_exhausted() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fixture::new(
        r#"digraph Costly {
            node [llm_provider="claude"]
            start [shape="Mdiamond"]
            a [shape="box", prompt="a"]
            b [shape="box", prompt="b"]
            done [shape="Msquare"]
            start -> a -> b -> done
        }"#,
    );
    let shims = fx.path().join("bin");
    fs::create_dir_all(&shims).unwrap();
    let claude = shims.join("claude");
    fs::write(
        &claude,
        "#!/bin/sh\nprintf '%s\\n' '{\"result\":\"ok\",\"is_error\":false,\"subtype\":\"\",\"total_cost_usd\":0.05,\"num_turns\":1}'\n",
    )
    .unwrap();
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();

    let output = fx
        .command(&["--max-budget-usd", "0.01"])
        .env("PATH", format!("{}:/bin:/usr/bin", shims.display()))
        .output()
        .unwrap();
    assert_failure(&output);
    let events = events(&fx.only_run_dir());
    let last = events.last().unwrap();
    assert_eq!(last["type"], "AttemptEnded", "{:?}", types(&events));
    assert_eq!(last["data"]["reason"], "budget_exhausted", "{last}");
    assert!(last["data"]["message"]
        .as_str()
        .unwrap()
        .contains("exceeded budget"));
}

fn wait_for(what: &str, timeout: Duration, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn process_alive(pid: i32) -> bool {
    // kill(pid, 0) succeeds while the process exists (zombies included).
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

// AC: SIGTERM during a stage writes AttemptEnded{stopped} and exits; the
// stage's child process is killed and the checkpoint stays resumable.
#[test]
fn sigterm_during_stage_writes_stopped_and_exits() {
    let fx = Fixture::new(
        r#"digraph Sleepy {
            start [shape="Mdiamond"]
            nap [shape="parallelogram", timeout="120s", tool_command="echo $$ > nap.pid; exec sleep 60"]
            done [shape="Msquare"]
            start -> nap -> done
        }"#,
    );
    let mut child = fx
        .command(&[])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let pid_file = fx.path().join("nap.pid");
    wait_for("the stage to start", Duration::from_secs(20), || {
        fs::read_to_string(&pid_file).is_ok_and(|s| s.trim().parse::<i32>().is_ok())
    });
    let run_dir = fx.only_run_dir();
    assert!(of_type(&events(&run_dir), "StageStarted")
        .iter()
        .any(|e| e["data"]["node_id"] == "nap"));
    let nap_pid: i32 = fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    let status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());

    let deadline = Instant::now() + Duration::from_secs(15);
    let exit = loop {
        if let Some(exit) = child.try_wait().unwrap() {
            break exit;
        }
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("pas did not exit after SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(!exit.success());
    assert_eq!(exit.code(), Some(143), "{exit:?}");

    let events = events(&run_dir);
    let last = events.last().unwrap();
    assert_eq!(last["type"], "AttemptEnded", "{:?}", types(&events));
    assert_eq!(last["data"]["reason"], "stopped");
    assert_eq!(last["data"]["message"], "SIGTERM");
    assert_eq!(last["data"]["attempt"], 1);
    assert_contiguous_seq(&events);

    wait_for("the stage's child to die", Duration::from_secs(5), || {
        !process_alive(nap_pid)
    });
    assert!(
        fx.logs().join("checkpoint.json").exists(),
        "stopped Run stays resumable"
    );
}

// AC: with --json, stdout's first line is {v, ok, run_id, run_dir} and
// run_dir exists when it is printed; human output goes to stderr.
#[test]
fn json_first_line_names_existing_run_dir() {
    let fx = Fixture::new(
        r#"digraph Slow {
            start [shape="Mdiamond"]
            wait [shape="parallelogram", timeout="60s", tool_command="while [ ! -f go ]; do sleep 0.05; done"]
            done [shape="Msquare"]
            start -> wait -> done
        }"#,
    );
    let mut child = fx
        .command(&["--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    let mut first = String::new();
    stdout.read_line(&mut first).unwrap();
    // The Run is still blocked in its stage: the folder must already exist.
    let line: Value = serde_json::from_str(&first).unwrap();
    let run_dir = PathBuf::from(line["run_dir"].as_str().unwrap());
    let run_dir_existed = run_dir.is_dir();
    fs::write(fx.path().join("go"), "").unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(run_dir_existed, "run_dir must exist when printed");
    assert_eq!(line["v"], 1);
    assert_eq!(line["ok"], true);
    let run_id = line["run_id"].as_str().unwrap();
    assert!(run_dir.is_absolute());
    assert_eq!(dir_name(&run_dir), run_id);
    assert_eq!(fx.index_lines()[0]["run_id"], run_id);
    assert_eq!(fx.index_lines()[0]["run_dir"], line["run_dir"]);

    let mut rest = String::new();
    std::io::Read::read_to_string(&mut stdout, &mut rest).unwrap();
    assert_eq!(rest, "", "--json keeps stdout for the JSON line only");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Pipeline completed"), "{stderr}");
}

// AC: run.json.argv equals the process argv (also in AttemptStarted).
#[test]
fn run_json_records_process_argv() {
    let fx = Fixture::new(THREE_STAGES);
    let args = fx.args(&["--dry-run", "--max-steps", "50", "--max-budget-usd", "7.5"]);
    assert_success(&fx.run(&["--dry-run", "--max-steps", "50", "--max-budget-usd", "7.5"]));

    let run_dir = fx.only_run_dir();
    let mut expected = vec![pas().to_string()];
    expected.extend(args);
    let argv: Vec<String> = serde_json::from_value(run_json(&run_dir)["argv"].clone()).unwrap();
    assert_eq!(argv, expected);
    let started = events(&run_dir)
        .into_iter()
        .find(|e| e["type"] == "AttemptStarted")
        .unwrap();
    assert_eq!(started["data"]["argv"], serde_json::json!(expected));
    assert!(started["data"]["pid"].as_u64().is_some());
}

// Each Pipeline in a directory gets its own Run and Index line; --run-id and
// --json are refused for a directory before anything is created.
#[test]
fn directory_run_creates_one_run_per_pipeline() {
    let fx = Fixture::new(THREE_STAGES);
    let batch = fx.path().join("batch");
    fs::create_dir_all(&batch).unwrap();
    fs::write(batch.join("01-a.dot"), THREE_STAGES).unwrap();
    fs::write(batch.join("02-b.dot"), THREE_STAGES).unwrap();

    let run_dir_cmd = |extra: &[&str]| {
        Command::new(pas())
            .arg("run")
            .arg(&batch)
            .args(["--dry-run"])
            .args(extra)
            .env("PAS_STATE_DIR", fx.path().join("state"))
            .env_remove(HEARTBEAT_ENV)
            .current_dir(fx.path())
            .output()
            .unwrap()
    };

    for extra in [&["--run-id", RUN_ID][..], &["--json"][..]] {
        let output = run_dir_cmd(extra);
        assert_failure(&output);
        assert!(!fx.path().join(".pas").exists(), "{extra:?} created files");
        assert!(!fx.index().exists());
    }

    assert_success(&run_dir_cmd(&[]));
    let index = fx.index_lines();
    assert_eq!(index.len(), 2, "{index:?}");
    assert_ne!(index[0]["run_id"], index[1]["run_id"]);
    for entry in &index {
        let run_dir = PathBuf::from(entry["run_dir"].as_str().unwrap());
        assert!(run_dir.join("run.json").is_file());
        assert_eq!(
            events(&run_dir).last().unwrap()["data"]["reason"],
            "completed"
        );
    }
}

fn ts(event: &Value) -> chrono::DateTime<chrono::FixedOffset> {
    chrono::DateTime::parse_from_rfc3339(event["ts"].as_str().unwrap()).unwrap()
}

/// Every Heartbeat sits between this Attempt's `AttemptStarted` and its
/// `AttemptEnded`, which is the journal's last Event.
fn assert_heartbeats_inside_attempt(events: &[Value]) {
    let started = events
        .iter()
        .position(|e| e["type"] == "AttemptStarted")
        .unwrap();
    let ended = events
        .iter()
        .rposition(|e| e["type"] == "AttemptEnded")
        .unwrap();
    assert_eq!(ended, events.len() - 1, "{:?}", types(events));
    for (i, e) in events.iter().enumerate() {
        if e["type"] == "Heartbeat" {
            assert!(
                i > started && i < ended,
                "Heartbeat at {i}: {:?}",
                types(events)
            );
        }
    }
}

// AC: Heartbeats carry the `pas run` PID, are evenly spaced by the interval,
// and none follows AttemptEnded (completed). Uses a 200 ms interval in place
// of 30 s; `heartbeat_real_30s_interval` checks the real one.
#[test]
fn heartbeats_carry_pid_and_are_evenly_spaced() {
    let fx = Fixture::new(
        r#"digraph Nap {
            start [shape="Mdiamond"]
            nap [shape="parallelogram", timeout="60s", tool_command="sleep 1.1"]
            done [shape="Msquare"]
            start -> nap -> done
        }"#,
    );
    let child = fx
        .command(&[])
        .env(HEARTBEAT_ENV, "200")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    let output = child.wait_with_output().unwrap();
    assert_success(&output);

    let events = events(&fx.only_run_dir());
    let beats = of_type(&events, "Heartbeat");
    assert!(beats.len() >= 4, "{:?}", types(&events));
    let started = of_type(&events, "AttemptStarted")[0];
    for beat in &beats {
        assert_eq!(beat["data"]["pid"], pid);
        assert_eq!(beat["data"]["pid"], started["data"]["pid"]);
        assert_eq!(beat["attempt"], 1);
        assert_eq!(beat["run_id"], started["run_id"]);
    }
    for pair in beats.windows(2) {
        let gap = (ts(pair[1]) - ts(pair[0])).num_milliseconds();
        assert!((100..=300).contains(&gap), "gap {gap} ms: {beats:?}");
    }
    let first = (ts(beats[0]) - ts(started)).num_milliseconds();
    assert!(
        (100..=300).contains(&first),
        "first Heartbeat {first} ms in"
    );
    assert_heartbeats_inside_attempt(&events);
    assert_eq!(events.last().unwrap()["data"]["reason"], "completed");
    assert_contiguous_seq(&events);
}

// AC: no Heartbeat after AttemptEnded{failed}.
#[test]
fn no_heartbeat_after_failed_attempt() {
    let fx = Fixture::new(
        r#"digraph Fails {
            start [shape="Mdiamond"]
            nap [shape="parallelogram", goal_gate=true, timeout="60s", tool_command="sleep 0.6; exit 1"]
            done [shape="Msquare"]
            start -> nap -> done
        }"#,
    );
    assert_failure(&fx.command(&[]).env(HEARTBEAT_ENV, "100").output().unwrap());
    // Anything still running would write within a few intervals.
    std::thread::sleep(Duration::from_millis(400));

    let events = events(&fx.only_run_dir());
    assert!(
        !of_type(&events, "Heartbeat").is_empty(),
        "{:?}",
        types(&events)
    );
    assert_heartbeats_inside_attempt(&events);
    assert_eq!(events.last().unwrap()["data"]["reason"], "failed");
    assert_contiguous_seq(&events);
}

// AC: no Heartbeat after AttemptEnded{stopped} on SIGTERM.
#[test]
fn no_heartbeat_after_sigterm() {
    let fx = Fixture::new(
        r#"digraph Sleepy {
            start [shape="Mdiamond"]
            nap [shape="parallelogram", timeout="120s", tool_command="exec sleep 60"]
            done [shape="Msquare"]
            start -> nap -> done
        }"#,
    );
    let mut child = fx
        .command(&[])
        .env(HEARTBEAT_ENV, "100")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for("two Heartbeats", Duration::from_secs(20), || {
        fx.run_dirs().first().is_some_and(|dir| {
            fs::read_to_string(dir.join("events.jsonl"))
                .is_ok_and(|t| t.matches("\"type\":\"Heartbeat\"").count() >= 2)
        })
    });
    let status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    let deadline = Instant::now() + Duration::from_secs(15);
    let exit = loop {
        if let Some(exit) = child.try_wait().unwrap() {
            break exit;
        }
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("pas did not exit after SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(exit.code(), Some(143), "{exit:?}");

    let run_dir = fx.only_run_dir();
    let journal = fs::read(run_dir.join("events.jsonl")).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(fs::read(run_dir.join("events.jsonl")).unwrap(), journal);
    let events = events(&run_dir);
    assert_heartbeats_inside_attempt(&events);
    assert_eq!(events.last().unwrap()["data"]["reason"], "stopped");
    assert_contiguous_seq(&events);
}

// AC: a Run shorter than 30 s (real interval) has no Heartbeat and completes.
#[test]
fn short_run_has_no_heartbeat() {
    let fx = Fixture::new(THREE_STAGES);
    assert_success(&fx.run(&["--dry-run"]));
    let events = events(&fx.only_run_dir());
    assert!(
        of_type(&events, "Heartbeat").is_empty(),
        "{:?}",
        types(&events)
    );
    let last = events.last().unwrap();
    assert_eq!(last["type"], "AttemptEnded");
    assert_eq!(last["data"]["reason"], "completed");
}

// Each Pipeline of a directory Run stops its Heartbeat before its own
// AttemptEnded; none leaks into the next Pipeline's Run.
#[test]
fn directory_run_stops_heartbeat_per_pipeline() {
    let fx = Fixture::new(THREE_STAGES);
    let batch = fx.path().join("batch");
    fs::create_dir_all(&batch).unwrap();
    let napping = |name: &str| {
        format!(
            r#"digraph {name} {{
                start [shape="Mdiamond"]
                nap [shape="parallelogram", timeout="60s", tool_command="sleep 0.5"]
                done [shape="Msquare"]
                start -> nap -> done
            }}"#
        )
    };
    fs::write(batch.join("01-a.dot"), napping("A")).unwrap();
    fs::write(batch.join("02-b.dot"), napping("B")).unwrap();

    let output = Command::new(pas())
        .arg("run")
        .arg(&batch)
        .env("PAS_STATE_DIR", fx.path().join("state"))
        .env(HEARTBEAT_ENV, "100")
        .current_dir(fx.path())
        .output()
        .unwrap();
    assert_success(&output);
    std::thread::sleep(Duration::from_millis(400));

    let index = fx.index_lines();
    assert_eq!(index.len(), 2, "{index:?}");
    for entry in &index {
        let run_dir = PathBuf::from(entry["run_dir"].as_str().unwrap());
        let events = events(&run_dir);
        assert!(
            !of_type(&events, "Heartbeat").is_empty(),
            "{:?}",
            types(&events)
        );
        assert!(events.iter().all(|e| e["run_id"] == entry["run_id"]));
        assert_heartbeats_inside_attempt(&events);
        assert_eq!(events.last().unwrap()["data"]["reason"], "completed");
        assert_contiguous_seq(&events);
    }
}

// AC, literally: a 95 s Run with the real 30 s interval has exactly 3
// Heartbeats with the process PID, 30 s ± 2 s apart, none after AttemptEnded.
// Slow; run with `cargo test -p attractor-cli --test run_lifecycle -- --ignored`.
#[test]
#[ignore = "takes 95 s"]
fn heartbeat_real_30s_interval() {
    let fx = Fixture::new(
        r#"digraph Long {
            start [shape="Mdiamond"]
            nap [shape="parallelogram", timeout="300s", tool_command="sleep 95"]
            done [shape="Msquare"]
            start -> nap -> done
        }"#,
    );
    let child = fx
        .command(&[])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    assert_success(&child.wait_with_output().unwrap());

    let events = events(&fx.only_run_dir());
    let started = of_type(&events, "AttemptStarted")[0];
    let ended = events.last().unwrap();
    assert!((ts(ended) - ts(started)).num_seconds() >= 95);
    let beats = of_type(&events, "Heartbeat");
    assert_eq!(beats.len(), 3, "{:?}", types(&events));
    for beat in &beats {
        assert_eq!(beat["data"]["pid"], pid);
    }
    for pair in beats.windows(2) {
        let gap = (ts(pair[1]) - ts(pair[0])).num_milliseconds();
        assert!((28_000..=32_000).contains(&gap), "gap {gap} ms");
    }
    assert_heartbeats_inside_attempt(&events);
    assert_eq!(ended["data"]["reason"], "completed");
    assert_contiguous_seq(&events);
}
