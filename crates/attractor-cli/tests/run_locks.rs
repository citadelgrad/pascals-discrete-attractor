#![cfg(unix)]
//! `pas run` Pipeline lock and Worktree lock (spec C5), observed through the
//! real binary in temporary git repositories.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

/// Test hook that shortens the 30 s Heartbeat interval.
const HEARTBEAT_ENV: &str = "PAS_HEARTBEAT_INTERVAL_MS";

/// Blocks in a stage until a `go` file appears in the workdir. The retry
/// lets a resume re-run the stage an interrupted Attempt was in.
const WAITS: &str = r#"digraph Waits {
    start [shape="Mdiamond"]
    wait [shape="parallelogram", timeout="120s", max_retries=2, tool_command="while [ ! -f go ]; do sleep 0.05; done"]
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

fn pas() -> &'static str {
    env!("CARGO_BIN_EXE_pas")
}

/// A scratch folder: `repo/` (a git repository when asked), Pipeline files,
/// logs, and a private Run Index.
struct Scratch {
    dir: tempfile::TempDir,
}

impl Scratch {
    fn new() -> Self {
        let scratch = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        fs::create_dir_all(scratch.repo()).unwrap();
        scratch
    }

    /// A scratch folder whose `repo/` is a git repository with one commit.
    fn git() -> Self {
        let scratch = Self::new();
        git(&scratch.repo(), &["init", "-q"]);
        git(
            &scratch.repo(),
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
        scratch
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn repo(&self) -> PathBuf {
        self.path().join("repo")
    }

    /// Write `<name>.dot` outside the repository.
    fn pipeline(&self, name: &str, source: &str) -> PathBuf {
        let path = self.path().join(format!("{name}.dot"));
        fs::write(&path, source).unwrap();
        path
    }

    fn logs(&self, name: &str) -> PathBuf {
        self.path().join("logs").join(name)
    }

    /// `pas run <name>.dot --workdir <workdir> --logs logs/<name> <extra>`.
    fn command(&self, name: &str, workdir: &Path, extra: &[&str]) -> Command {
        let mut command = pas_command(self.path());
        command
            .arg("run")
            .arg(self.path().join(format!("{name}.dot")))
            .arg("--workdir")
            .arg(workdir)
            .arg("--logs")
            .arg(self.logs(name))
            .args(extra);
        command
    }

    fn run(&self, name: &str, workdir: &Path, extra: &[&str]) -> Output {
        self.command(name, workdir, extra).output().unwrap()
    }

    /// Start `name` (a [`WAITS`] Pipeline) in the background and wait until
    /// it is blocked in its `wait` stage, with the checkpoint saying so.
    fn hold(&self, name: &str, workdir: &Path, extra: &[&str]) -> Holder {
        let child = self
            .command(name, workdir, extra)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut holder = Holder {
            child,
            run_id: String::new(),
            run_dir: PathBuf::new(),
        };
        let runs = self.logs(name).join("runs");
        wait_for(
            "the holder's AttemptStarted",
            Duration::from_secs(20),
            || {
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
                holder.run_dir = run_dir;
                true
            },
        );
        let checkpoint = self.logs(name).join("checkpoint.json");
        wait_for("the holder's wait stage", Duration::from_secs(20), || {
            fs::read_to_string(&checkpoint)
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .is_some_and(|cp| cp["active_node_id"] == "wait")
        });
        holder
    }

    fn index_lines(&self) -> usize {
        fs::read_to_string(self.path().join("state").join("runs.jsonl"))
            .map(|text| text.lines().count())
            .unwrap_or(0)
    }
}

/// A `pas run` in the background. Killed on drop if still running.
struct Holder {
    child: Child,
    run_id: String,
    run_dir: PathBuf,
}

impl Holder {
    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn wait(&mut self, timeout: Duration) -> std::process::ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
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

/// `pas` with a private Run Index and no inherited git or Heartbeat overrides.
fn pas_command(scratch: &Path) -> Command {
    let mut command = Command::new(pas());
    command
        .env("PAS_STATE_DIR", scratch.join("state"))
        .env_remove(HEARTBEAT_ENV)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        // Never find a repository above the scratch folder.
        .env("GIT_CEILING_DIRECTORIES", scratch.parent().unwrap())
        .current_dir(scratch);
    command
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

/// The journal's events; a line still being written is skipped.
fn read_events(run_dir: &Path) -> Vec<Value> {
    fs::read_to_string(run_dir.join("events.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn of_type(events: &[Value], ty: &str) -> Vec<Value> {
    events.iter().filter(|e| e["type"] == ty).cloned().collect()
}

fn run_started(run_dir: &Path) -> Value {
    of_type(&read_events(run_dir), "RunStarted")
        .pop()
        .expect("RunStarted")
}

fn only_run_dir(logs: &Path) -> PathBuf {
    let dirs = run_dirs(&logs.join("runs"));
    assert_eq!(dirs.len(), 1, "expected one Run folder, got {dirs:?}");
    dirs.into_iter().next().unwrap()
}

fn lock_contents(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "pas run failed ({:?})\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        stderr(output)
    );
}

fn assert_exit(output: &Output, code: i32) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        stderr(output)
    );
}

fn find_files(dir: &Path, name: &str, found: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            find_files(&path, name, found);
        } else if entry.file_name() == name {
            found.push(path);
        }
    }
}

// AC1: a second `pas run` of an active Pipeline exits 5 naming the active
// PID and run_id, and leaves no trace: no Run folder, no Index line, the
// holder's checkpoint untouched (even with --fresh).
#[test]
fn second_run_of_active_pipeline_exits_5() {
    let s = Scratch::git();
    s.pipeline("a", WAITS);
    let holder = s.hold("a", &s.repo(), &[]);
    let checkpoint = s.logs("a").join("checkpoint.json");
    let checkpoint_before = fs::read(&checkpoint).ok();

    for extra in [&[][..], &["--fresh"][..], &["--allow-shared-workdir"][..]] {
        let output = s.run("a", &s.repo(), extra);
        assert_exit(&output, 5);
        let err = stderr(&output);
        assert!(
            err.contains(&format!(
                "error: pipeline already running (pid {}, run {})",
                holder.pid(),
                holder.run_id
            )),
            "{extra:?}: {err}"
        );
        assert_eq!(run_dirs(&s.logs("a").join("runs")).len(), 1, "{extra:?}");
        assert_eq!(s.index_lines(), 1, "{extra:?}");
        assert_eq!(fs::read(&checkpoint).ok(), checkpoint_before, "{extra:?}");
    }
}

// AC1 with --json: one C6 error object on stdout, exit 5.
#[test]
fn second_run_json_reports_pipeline_locked() {
    let s = Scratch::new();
    s.pipeline("a", WAITS);
    let holder = s.hold("a", &s.repo(), &[]);

    let output = s.run("a", &s.repo(), &["--json"]);
    assert_exit(&output, 5);
    let stdout = String::from_utf8(output.stdout.clone()).unwrap();
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    let line: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(line["v"], 1);
    assert_eq!(line["ok"], false);
    assert_eq!(line["error"]["code"], "pipeline_locked");
    let message = line["error"]["message"].as_str().unwrap();
    assert!(message.contains(&holder.pid().to_string()), "{message}");
    assert!(message.contains(&holder.run_id), "{message}");
}

// C5: `run.lock` holds the holder's PID and run_id while it runs.
#[test]
fn lock_files_contain_pid_and_run_id() {
    let s = Scratch::git();
    s.pipeline("a", WAITS);
    let holder = s.hold("a", &s.repo(), &[]);

    for path in [
        s.logs("a").join("run.lock"),
        s.repo().join(".git").join("pas-run.lock"),
    ] {
        let contents = lock_contents(&path);
        assert_eq!(contents["pid"], holder.pid(), "{}", path.display());
        assert_eq!(contents["run_id"], holder.run_id, "{}", path.display());
    }
}

// AC2: a different Pipeline in the same worktree exits 6 naming the other
// Run, and creates no Run folder or Index line.
#[test]
fn other_pipeline_in_same_worktree_exits_6() {
    let s = Scratch::git();
    s.pipeline("a", WAITS);
    s.pipeline("b", QUICK);
    let holder = s.hold("a", &s.repo(), &[]);

    let output = s.run("b", &s.repo(), &[]);
    assert_exit(&output, 6);
    let err = stderr(&output);
    assert!(
        err.contains(&format!("pid {}, run {}", holder.pid(), holder.run_id)),
        "{err}"
    );
    assert!(err.contains("--allow-shared-workdir"), "{err}");
    assert!(!s.logs("b").join("runs").exists());
    assert_eq!(s.index_lines(), 1);

    // B released its own Pipeline lock on the way out.
    let b_lock = s.logs("b").join("run.lock");
    assert!(RunLockProbe::is_free(&b_lock));

    // The same refusal in --json.
    let output = s.run("b", &s.repo(), &["--json"]);
    assert_exit(&output, 6);
    let line: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(line["error"]["code"], "worktree_locked");
}

// AC2 from a subdirectory: the lock is per worktree, not per workdir path.
#[test]
fn subdirectory_of_busy_worktree_exits_6() {
    let s = Scratch::git();
    s.pipeline("a", WAITS);
    s.pipeline("b", QUICK);
    let sub = s.repo().join("sub");
    fs::create_dir_all(&sub).unwrap();
    let _holder = s.hold("a", &s.repo(), &[]);

    assert_exit(&s.run("b", &sub, &[]), 6);
}

// AC3: --allow-shared-workdir starts the second Pipeline and records
// shared_workdir = true; the holder's RunStarted says false.
#[test]
fn allow_shared_workdir_starts_and_records_it() {
    let s = Scratch::git();
    s.pipeline("a", WAITS);
    s.pipeline("b", QUICK);
    let holder = s.hold("a", &s.repo(), &[]);

    let output = s.run("b", &s.repo(), &["--allow-shared-workdir"]);
    assert_success(&output);
    assert!(
        stderr(&output).contains("sharing this git worktree"),
        "{}",
        stderr(&output)
    );
    let b = only_run_dir(&s.logs("b"));
    assert_eq!(run_started(&b)["data"]["shared_workdir"], true);
    assert_eq!(
        run_started(&holder.run_dir)["data"]["shared_workdir"],
        false
    );
    // The holder still owns the Worktree lock.
    assert_eq!(
        lock_contents(&s.repo().join(".git").join("pas-run.lock"))["pid"],
        holder.pid()
    );
}

// AC3 boundary: the flag alone, with no other Run, records false.
#[test]
fn allow_shared_workdir_without_contention_records_false() {
    let s = Scratch::git();
    s.pipeline("b", QUICK);
    assert_success(&s.run("b", &s.repo(), &["--allow-shared-workdir"]));
    let b = only_run_dir(&s.logs("b"));
    assert_eq!(run_started(&b)["data"]["shared_workdir"], false);
    assert!(s.repo().join(".git").join("pas-run.lock").is_file());
}

// AC4: two worktrees of one repository each have their own Worktree lock.
#[test]
fn pipelines_in_two_worktrees_run_concurrently() {
    let s = Scratch::git();
    let wt2 = s.path().join("wt2");
    git(
        &s.repo(),
        &["worktree", "add", "-q", "--detach", wt2.to_str().unwrap()],
    );
    s.pipeline("a", WAITS);
    s.pipeline("b", QUICK);
    let holder = s.hold("a", &s.repo(), &[]);

    let output = s.run("b", &wt2, &[]);
    assert_success(&output);
    let b = only_run_dir(&s.logs("b"));
    assert_eq!(run_started(&b)["data"]["shared_workdir"], false);

    let main_lock = s.repo().join(".git").join("pas-run.lock");
    let wt2_lock = s
        .repo()
        .join(".git")
        .join("worktrees")
        .join("wt2")
        .join("pas-run.lock");
    assert_eq!(lock_contents(&main_lock)["run_id"], holder.run_id);
    let b_run_id = run_started(&b)["run_id"].clone();
    assert_eq!(lock_contents(&wt2_lock)["run_id"], b_run_id);
}

// AC5: after `kill -9` both locks are free with no cleanup: another
// Pipeline takes the Worktree lock, and the killed Pipeline resumes.
#[test]
fn kill_9_releases_both_locks() {
    let s = Scratch::git();
    s.pipeline("a", WAITS);
    s.pipeline("b", QUICK);
    let mut holder = s.hold("a", &s.repo(), &[]);

    let status = Command::new("kill")
        .args(["-KILL", &holder.pid().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    let exit = holder.wait(Duration::from_secs(10));
    assert_eq!(exit.code(), None, "killed by a signal: {exit:?}");
    let events = read_events(&holder.run_dir);
    assert!(of_type(&events, "AttemptEnded").is_empty(), "{events:?}");
    // The lock files are still there, naming the dead holder.
    assert_eq!(
        lock_contents(&s.logs("a").join("run.lock"))["pid"],
        holder.pid()
    );

    assert_success(&s.run("b", &s.repo(), &[]));

    fs::write(s.repo().join("go"), "").unwrap();
    let output = s.run("a", &s.repo(), &[]);
    assert_success(&output);
    let events = read_events(&holder.run_dir);
    let ended = of_type(&events, "AttemptEnded");
    assert_eq!(ended.len(), 1, "{events:?}");
    assert_eq!(ended[0]["data"]["attempt"], 2);
    assert_eq!(ended[0]["data"]["reason"], "completed");
}

// AC5 for SIGTERM: the stopped Attempt releases both locks.
#[test]
fn sigterm_releases_both_locks() {
    let s = Scratch::git();
    s.pipeline("a", WAITS);
    s.pipeline("b", QUICK);
    let mut holder = s.hold("a", &s.repo(), &[]);

    let status = Command::new("kill")
        .args(["-TERM", &holder.pid().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(holder.wait(Duration::from_secs(15)).code(), Some(143));

    assert_success(&s.run("b", &s.repo(), &[]));
    fs::write(s.repo().join("go"), "").unwrap();
    assert_success(&s.run("a", &s.repo(), &[]));
}

// AC6: outside a git repository only the Pipeline lock is taken.
#[test]
fn non_git_folder_takes_only_pipeline_lock() {
    let s = Scratch::new();
    s.pipeline("a", WAITS);
    s.pipeline("b", QUICK);
    let holder = s.hold("a", &s.repo(), &[]);

    let lock = lock_contents(&s.logs("a").join("run.lock"));
    assert_eq!(lock["pid"], holder.pid());
    assert_eq!(lock["run_id"], holder.run_id);

    // Another Pipeline in the same folder is not refused.
    let output = s.run("b", &s.repo(), &[]);
    assert_success(&output);
    let b = only_run_dir(&s.logs("b"));
    assert_eq!(run_started(&b)["data"]["shared_workdir"], false);

    let mut found = Vec::new();
    find_files(s.path(), "pas-run.lock", &mut found);
    assert!(found.is_empty(), "{found:?}");
}

// AC6 boundary: `git` missing from PATH is treated like no repository.
#[test]
fn git_missing_takes_only_pipeline_lock() {
    let s = Scratch::git();
    s.pipeline("b", QUICK);
    let bin = s.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    for tool in ["sh", "true"] {
        let target = ["/bin", "/usr/bin"]
            .iter()
            .map(|dir| Path::new(dir).join(tool))
            .find(|p| p.exists())
            .unwrap();
        std::os::unix::fs::symlink(target, bin.join(tool)).unwrap();
    }

    let output = s
        .command("b", &s.repo(), &[])
        .env("PATH", &bin)
        .output()
        .unwrap();
    assert_success(&output);
    assert!(s.logs("b").join("run.lock").is_file());
    assert!(!s.repo().join(".git").join("pas-run.lock").exists());
}

// C5 for directory Runs: each Pipeline takes and releases its own locks, so
// two Pipelines in one worktree run one after the other; a busy worktree
// refuses the whole directory Run with exit 6.
#[test]
fn directory_run_locks_each_pipeline() {
    let s = Scratch::git();
    let batch = s.path().join("batch");
    fs::create_dir_all(&batch).unwrap();
    fs::write(batch.join("01-a.dot"), QUICK).unwrap();
    fs::write(batch.join("02-b.dot"), QUICK).unwrap();
    let dir_run = || {
        let mut command = pas_command(s.path());
        command
            .arg("run")
            .arg(&batch)
            .arg("--workdir")
            .arg(s.repo());
        command.output().unwrap()
    };

    s.pipeline("w", WAITS);
    let holder = s.hold("w", &s.repo(), &[]);
    let output = dir_run();
    assert_exit(&output, 6);
    assert!(
        stderr(&output).contains(&holder.run_id),
        "{}",
        stderr(&output)
    );
    drop(holder);

    let output = dir_run();
    assert_success(&output);
    let mut run_starts = Vec::new();
    find_files(&s.path().join(".pas"), "events.jsonl", &mut run_starts);
    assert_eq!(run_starts.len(), 2, "{run_starts:?}");
    for events in run_starts {
        let started = run_started(events.parent().unwrap());
        assert_eq!(started["data"]["shared_workdir"], false);
    }
}

// The new flag is documented in `pas run --help`.
#[test]
fn help_lists_allow_shared_workdir() {
    let output = Command::new(pas())
        .args(["run", "--help"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&output.stdout).contains("--allow-shared-workdir"));
}

/// Checks a lock from outside `pas`: `flock` is per open file, so a lock
/// taken here conflicts with any other holder.
struct RunLockProbe;

impl RunLockProbe {
    fn is_free(path: &Path) -> bool {
        let file = fs::OpenOptions::new().read(true).open(path).unwrap();
        let free = file.try_lock().is_ok();
        let _ = file.unlock();
        free
    }
}
