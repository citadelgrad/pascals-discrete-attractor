use std::ffi::OsString;
use std::path::Path;
use std::time::{Duration, Instant};

use attractor_monitor::spawn::{pas_exe, run_pas, spawn_detached_at, spawn_run_detached};

fn os(a: &[&str]) -> Vec<OsString> {
    a.iter().map(OsString::from).collect()
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(20);
    while Instant::now() < end {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

#[test]
fn spawn_helper_runs_the_current_executable() {
    assert_eq!(pas_exe().unwrap(), std::env::current_exe().unwrap());
}

#[tokio::test]
async fn run_pas_executes_the_same_binary() {
    let out = run_pas(&os(&["--list", "--format", "terse"]))
        .await
        .unwrap();
    assert_eq!(out.code, Some(0));
    // libtest listing includes this very test, so the child is this binary.
    assert!(
        out.stdout.contains("run_pas_executes_the_same_binary"),
        "{}",
        out.stdout
    );
}

#[test]
fn console_log_receives_stdout_and_stderr_and_appends() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("logs/runs/0190-run/console.log");
    let args = os(&["-c", "echo out; echo err >&2"]);
    spawn_detached_at(Path::new("/bin/sh"), &args, &log).unwrap();
    wait_for("first output", || {
        let t = read(&log);
        t.contains("out") && t.contains("err")
    });
    spawn_detached_at(Path::new("/bin/sh"), &args, &log).unwrap();
    wait_for("second output", || read(&log).matches("out\n").count() == 2);
}

#[test]
fn spawn_run_detached_captures_stdout_and_stderr_of_pas_binary() {
    let dir = tempfile::tempdir().unwrap();
    let ok = dir.path().join("a/console.log");
    spawn_run_detached(&os(&["--list", "--format", "terse"]), &ok).unwrap();
    wait_for("listing", || {
        read(&ok).contains("console_log_receives_stdout")
    });
    let bad = dir.path().join("b/console.log");
    spawn_run_detached(&os(&["--no-such-flag-xyz"]), &bad).unwrap();
    wait_for("usage error", || read(&bad).contains("no-such-flag-xyz"));
}

#[test]
fn spawn_fails_when_executable_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let r = spawn_detached_at(
        Path::new("/nonexistent/pas"),
        &[],
        &dir.path().join("c.log"),
    );
    assert!(r.is_err());
}

#[cfg(unix)]
mod survive {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};

    const ENV: &str = "SPAWN_SURVIVOR_LOG";

    /// Plays the Monitor in a re-executed test binary; a no-op otherwise.
    #[test]
    fn spawn_survivor_parent() {
        let Ok(log) = std::env::var(ENV) else { return };
        let args = os(&["-c", "sleep 2; echo survived"]);
        let pid = spawn_detached_at(Path::new("/bin/sh"), &args, Path::new(&log)).unwrap();
        println!("PID={pid}");
        std::thread::sleep(Duration::from_secs(60));
    }

    #[test]
    fn run_keeps_running_after_monitor_is_killed() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("runs/x/console.log");
        let mut monitor = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "survive::spawn_survivor_parent", "--nocapture"])
            .env(ENV, &log)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(monitor.stdout.take().unwrap()).lines();
        let pid: i32 = loop {
            let l = lines.next().expect("monitor exited early").unwrap();
            if let Some(p) = l.strip_prefix("PID=") {
                break p.parse().unwrap();
            }
        };
        monitor.kill().unwrap(); // SIGKILL
        monitor.wait().unwrap();
        // SAFETY: plain libc queries on a pid.
        unsafe {
            assert_eq!(libc::kill(pid, 0), 0, "child died with the monitor");
            assert_eq!(libc::getsid(pid), pid, "child is not in its own session");
        }
        wait_for("survivor output", || read(&log).contains("survived"));
    }
}
