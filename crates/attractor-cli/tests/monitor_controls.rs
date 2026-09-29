#![cfg(all(unix, feature = "monitor"))]
//! Monitor Run controls against the real `pas` binary (attractor-ino.38):
//! Stop, Kill, Resume, Run again and the lock error, driven over HTTP with the
//! CSRF token scraped from the served Run page.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const PIPELINE: &str = r#"digraph Controls {
    start [shape="Mdiamond"]
    slow [shape="parallelogram", tool_command="echo run >> slow.count; touch started; sleep 2; touch finished"]
    next [shape="parallelogram", tool_command="echo run >> next.count"]
    done [shape="Msquare"]
    start -> slow -> next -> done
}"#;

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Fx {
    dir: tempfile::TempDir,
    port: u16,
    _monitor: Kill,
}

fn wait_for(what: &str, within: Duration, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

impl Fx {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("p.dot"), PIPELINE).unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let monitor = Command::new(env!("CARGO_BIN_EXE_pas"))
            .args(["monitor", "--port", &port.to_string()])
            .env("PAS_STATE_DIR", dir.path().join("state"))
            .current_dir(dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let fx = Self {
            dir,
            port,
            _monitor: Kill(monitor),
        };
        wait_for("the Monitor", Duration::from_secs(10), || {
            TcpStream::connect(("127.0.0.1", port)).is_ok()
        });
        fx
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn logs(&self) -> PathBuf {
        self.path().join("logs")
    }

    fn run_dirs(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = fs::read_dir(self.logs().join("runs"))
            .map(|d| d.map(|e| e.unwrap().path()).collect())
            .unwrap_or_default();
        v.sort();
        v
    }

    /// Start `pas run` like a user would and wait until `slow` executes.
    fn start_run(&self) -> (std::sync::mpsc::Receiver<Option<i32>>, String) {
        let mut child = Command::new(env!("CARGO_BIN_EXE_pas"))
            .args([
                "run",
                "p.dot",
                "--workdir",
                self.path().to_str().unwrap(),
                "--logs",
                self.logs().to_str().unwrap(),
            ])
            .env("PAS_STATE_DIR", self.path().join("state"))
            .current_dir(self.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        // Reap the child as soon as it ends, as a shell would, so `pas kill`
        // does not see a zombie as a live process.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait().ok().and_then(|s| s.code()));
        });
        wait_for("slow to start", Duration::from_secs(30), || {
            self.path().join("started").exists()
        });
        let id = self.run_dirs()[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        (rx, id)
    }

    fn http(&self, method: &str, path: &str, token: Option<&str>) -> (u16, String) {
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        let mut req = format!(
            "{method} {path} HTTP/1.0\r\nHost: 127.0.0.1:{}\r\nContent-Length: 0\r\n",
            self.port
        );
        if let Some(t) = token {
            req.push_str(&format!("X-CSRF-Token: {t}\r\n"));
        }
        req.push_str("\r\n");
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

    /// Wait for the Monitor to know the Run and return the CSRF token from its page.
    fn token_for(&self, id: &str) -> String {
        let mut page = String::new();
        wait_for("the Run page", Duration::from_secs(10), || {
            let (code, body) = self.http("GET", &format!("/runs/{id}"), None);
            page = body;
            code == 200
        });
        let marker = "X-CSRF-Token&quot;:&quot;";
        let rest = &page[page.find(marker).expect("token in page") + marker.len()..];
        rest.chars().take_while(|c| c.is_ascii_hexdigit()).collect()
    }

    fn post(&self, id: &str, action: &str, token: &str) -> (u16, String) {
        self.http("POST", &format!("/runs/{id}/{action}"), Some(token))
    }

    fn shows(&self, id: &str, statuses: &[&str]) -> bool {
        let (_, body) = self.http("GET", &format!("/runs/{id}/summary"), None);
        statuses
            .iter()
            .any(|s| body.contains(&format!(r#"data-status="{s}""#)))
    }

    fn wait_status(&self, id: &str, statuses: &[&str], within: Duration) -> Duration {
        let start = Instant::now();
        wait_for(&format!("status {statuses:?}"), within, || {
            self.shows(id, statuses)
        });
        start.elapsed()
    }
}

fn events(run_dir: &Path) -> Vec<Value> {
    fs::read_to_string(run_dir.join("events.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn has_event(run_dir: &Path, ty: &str) -> bool {
    events(run_dir).iter().any(|e| e["type"] == ty)
}

fn wait_exit(child: std::sync::mpsc::Receiver<Option<i32>>) -> Option<i32> {
    child
        .recv_timeout(Duration::from_secs(30))
        .expect("pas run did not exit")
}

#[test]
fn stop_then_resume_then_run_again() {
    let fx = Fx::new();
    let (child, id) = fx.start_run();
    let token = fx.token_for(&id);

    // Stop: the stage finishes, then the Attempt ends stopped.
    let (code, body) = fx.post(&id, "stop", &token);
    assert_eq!(code, 200, "{body}");
    assert_eq!(wait_exit(child), Some(0));
    let run_dir = fx.run_dirs()[0].clone();
    let all = events(&run_dir);
    let stop = all.iter().find(|e| e["type"] == "StopRequested").unwrap();
    assert_eq!(stop["data"]["source"], "monitor");
    let last = all.last().unwrap();
    assert_eq!(last["type"], "AttemptEnded");
    assert_eq!(last["data"]["reason"], "stopped");
    assert!(fx.path().join("finished").exists());
    assert!(!fx.path().join("next.count").exists());
    fx.wait_status(&id, &["stopped"], Duration::from_secs(10));

    // Resume: a second Attempt of the same Run.
    let (code, body) = fx.post(&id, "resume", &token);
    assert_eq!(code, 200, "{body}");
    wait_for("the resumed Run to finish", Duration::from_secs(30), || {
        has_event(&run_dir, "PipelineCompleted")
    });
    let all = events(&run_dir);
    let attempts: Vec<u64> = all
        .iter()
        .filter(|e| e["type"] == "AttemptStarted")
        .map(|e| e["data"]["attempt"].as_u64().unwrap())
        .collect();
    assert_eq!(attempts.len(), 2, "{attempts:?}");
    assert!(all.iter().all(|e| e["run_id"] == id.as_str()));
    let seqs: Vec<u64> = all.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
    assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>());
    assert_eq!(fx.run_dirs().len(), 1, "resume must not create a new Run");
    fx.wait_status(&id, &["completed"], Duration::from_secs(10));

    // Resume is refused for a completed Run; Run again starts a new one.
    assert_eq!(fx.post(&id, "resume", &token).0, 409);
    let (code, body) = fx.post(&id, "rerun", &token);
    assert_eq!(code, 200, "{body}");
    wait_for("a second Run", Duration::from_secs(30), || {
        fx.run_dirs().len() == 2
    });
    let dirs = fx.run_dirs();
    let new_dir = dirs.iter().find(|d| !d.ends_with(&id)).unwrap();
    let new_id = new_dir.file_name().unwrap().to_string_lossy().into_owned();
    assert_ne!(new_id, id);
    assert!(body.contains(&format!("/runs/{new_id}")), "{body}");
    assert!(events(&run_dir).iter().all(|e| e["run_id"] == id.as_str()));
    wait_for("the new Run to finish", Duration::from_secs(30), || {
        has_event(new_dir, "PipelineCompleted")
    });
}

#[test]
fn kill_ends_the_run_within_five_seconds() {
    let fx = Fx::new();
    let (child, id) = fx.start_run();
    let token = fx.token_for(&id);
    let start = Instant::now();
    let (code, body) = fx.post(&id, "kill", &token);
    assert_eq!(code, 200, "{body}");
    fx.wait_status(&id, &["stopped", "crashed"], Duration::from_secs(5));
    assert!(start.elapsed() < Duration::from_secs(5));
    let _ = wait_exit(child);
    assert_eq!(
        fx.post(&id, "kill", &token).0,
        409,
        "finished Runs cannot be killed"
    );
}

#[test]
fn resume_while_the_lock_is_held_shows_pid_and_run_id() {
    let fx = Fx::new();
    let (child, id) = fx.start_run();
    let token = fx.token_for(&id);
    assert_eq!(fx.post(&id, "stop", &token).0, 200);
    assert_eq!(wait_exit(child), Some(0));
    fx.wait_status(&id, &["stopped"], Duration::from_secs(10));

    // Another Attempt holds the Pipeline lock.
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(fx.logs().join("run.lock"))
        .unwrap();
    lock.try_lock().unwrap();
    let pid = std::process::id();
    fs::write(
        fx.logs().join("run.lock"),
        format!("{{\"pid\":{pid},\"run_id\":\"{id}\"}}\n"),
    )
    .unwrap();

    let (code, body) = fx.post(&id, "resume", &token);
    assert_eq!(code, 502, "{body}");
    assert!(body.contains(&format!("pid {pid}")), "{body}");
    assert!(body.contains(&id), "{body}");
    drop(lock);
}
