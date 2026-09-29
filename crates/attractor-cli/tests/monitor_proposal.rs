#![cfg(all(unix, feature = "monitor"))]
//! Monitor Proposal editor against the real `pas` binary (attractor-ino.41).
//! Stubs on `PATH` stand in for the Beads program and `claude`; the `bd` log
//! is what `bd children` would show. Feeding the Monitor's saved Proposal to
//! the real `pas decompose --from-proposal` also catches drift between the
//! Monitor's copy of the Proposal shape and the CLI's.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Built with `concat!` so the program name appears quoted only in the stub.
const BEADS_PROGRAM: &str = concat!("b", "d");

const PROPOSAL: &str = r#"{"v":1,"epic":{"title":"Epic","description":"Epic body"},"tasks":[{"title":"A","type":"task","priority":"P2","description":"a"},{"title":"B","type":"task","priority":"P1","description":"b","acceptance":"acc"},{"title":"C","type":"task","priority":"P2","description":"c"}],"dependencies":[{"blocked":1,"blocker":0},{"blocked":2,"blocker":1}]}"#;

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn stub(dir: &Path, name: &str, script: &str) {
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

struct Fx {
    dir: tempfile::TempDir,
    port: u16,
    _monitor: Kill,
}

impl Fx {
    fn new(claude_script: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(dir.path().join("repo/.git")).unwrap();
        let (log, counter) = (dir.path().join("bd.log"), dir.path().join("bd.count"));
        stub(
            &bin,
            BEADS_PROGRAM,
            &format!(
                r#"echo "$*" >> {log}
if [ "$1" = "create" ]; then
  n=$(cat {counter} 2>/dev/null || echo 0); n=$((n+1)); echo $n > {counter}
  echo "{{\"id\":\"id-$n\"}}"
else
  echo '[]'
fi"#,
                log = log.display(),
                counter = counter.display()
            ),
        );
        stub(&bin, "claude", claude_script);
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let monitor = Command::new(env!("CARGO_BIN_EXE_pas"))
            .args(["monitor", "--port", &port.to_string()])
            .env("PAS_STATE_DIR", dir.path().join("state"))
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .current_dir(dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "Monitor did not start");
            std::thread::sleep(Duration::from_millis(50));
        }
        Self {
            dir,
            port,
            _monitor: Kill(monitor),
        }
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        ctype: &str,
        body: &[u8],
        token: Option<&str>,
    ) -> (u16, String) {
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        let mut head = format!(
            "{method} {path} HTTP/1.0\r\nHost: 127.0.0.1:{}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n",
            self.port,
            body.len()
        );
        if let Some(t) = token {
            head.push_str(&format!("X-CSRF-Token: {t}\r\n"));
        }
        head.push_str("\r\n");
        s.write_all(head.as_bytes()).unwrap();
        s.write_all(body).unwrap();
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).unwrap();
        let text = String::from_utf8_lossy(&buf).into_owned();
        let (h, b) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        (h.split(' ').nth(1).unwrap().parse().unwrap(), b.to_string())
    }

    fn token(&self) -> String {
        let (_, page) = self.request("GET", "/plans/new", "text/plain", b"", None);
        let marker = "X-CSRF-Token&quot;:&quot;";
        let rest = &page[page.find(marker).expect("token in page") + marker.len()..];
        rest.chars().take_while(|c| c.is_ascii_hexdigit()).collect()
    }

    /// Upload two files as an Epic + Pipeline Plan; returns (token, plan id).
    fn plan(&self) -> (String, String) {
        let token = self.token();
        let b = "XBX";
        let mut body = String::new();
        for (n, t) in [("one.md", "# One"), ("two.md", "# Two")] {
            body.push_str(&format!("--{b}\r\nContent-Disposition: form-data; name=\"files\"; filename=\"{n}\"\r\nContent-Type: text/plain\r\n\r\n{t}\r\n"));
        }
        let repo = self.dir.path().join("repo");
        for (k, v) in [
            ("repo", repo.to_str().unwrap()),
            ("kind", "epic_pipeline"),
            ("mode", "reviewed"),
        ] {
            body.push_str(&format!(
                "--{b}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n"
            ));
        }
        body.push_str(&format!("--{b}--\r\n"));
        let (code, page) = self.request(
            "POST",
            "/plans/new",
            &format!("multipart/form-data; boundary={b}"),
            body.as_bytes(),
            Some(&token),
        );
        assert_eq!(code, 200, "{page}");
        let rest = &page[page.find("<code>").unwrap() + 6..];
        (token, rest[..32].to_string())
    }

    fn form(&self, method: &str, path: &str, token: &str, body: &str) -> (u16, String) {
        self.request(
            method,
            path,
            "application/x-www-form-urlencoded",
            body.as_bytes(),
            Some(token),
        )
    }

    fn bd_log(&self) -> Vec<String> {
        fs::read_to_string(self.dir.path().join("bd.log"))
            .map(|s| s.lines().map(String::from).collect())
            .unwrap_or_default()
    }
}

fn claude_ok(dir: &Path) -> String {
    let answer = dir.join("answer.json");
    fs::create_dir_all(dir).unwrap();
    fs::write(
        &answer,
        serde_json::json!({ "result": PROPOSAL }).to_string(),
    )
    .unwrap();
    format!("cat {}", answer.display())
}

#[test]
fn generate_edit_remove_and_create_epic() {
    let scratch = tempfile::tempdir().unwrap();
    let fx = Fx::new(&claude_ok(scratch.path()));
    let (token, id) = fx.plan();

    let (code, page) = fx.form("POST", &format!("/plans/{id}/proposal"), &token, "");
    assert_eq!(code, 200, "{page}");
    for t in ["Epic", "Task 3", "blocked by"] {
        assert!(page.contains(t), "{page}");
    }

    // Remove Task B (index 1), retitle C.
    let (code, page) = fx.form(
        "PUT",
        &format!("/plans/{id}/proposal"),
        &token,
        "epic_title=Epic&t0_title=A&t1_title=B&t2_title=C%20renamed&dep=1%3A0&dep=2%3A1&remove=1",
    );
    assert_eq!(code, 200, "{page}");
    let (code, page) = fx.form("POST", &format!("/plans/{id}/epic"), &token, "");
    assert_eq!(code, 200, "{page}");
    assert!(page.contains("id-1"), "{page}");

    let log = fx.bd_log();
    let creates: Vec<&String> = log.iter().filter(|l| l.starts_with("create")).collect();
    assert_eq!(creates.len(), 3, "Epic plus two Tasks: {log:?}");
    assert!(creates.iter().any(|l| l.contains("C renamed")), "{log:?}");
    assert!(
        !creates
            .iter()
            .any(|l| l.contains("--title B") || l.contains("\"B\"")),
        "{log:?}"
    );
    // The only `dep add` calls link the Epic (id-1) to its Tasks; none links
    // two Tasks, because both dependencies mentioned the removed Task.
    let deps: Vec<&String> = log.iter().filter(|l| l.starts_with("dep add")).collect();
    assert_eq!(deps.len(), 2, "{log:?}");
    assert!(
        deps.iter().all(|l| l.starts_with("dep add id-1 ")),
        "{log:?}"
    );
}

#[test]
fn failing_claude_shows_the_message_and_calls_no_bd() {
    let fx = Fx::new("echo boom >&2; exit 1");
    let (token, id) = fx.plan();
    let (code, page) = fx.form("POST", &format!("/plans/{id}/proposal"), &token, "");
    assert_eq!(code, 502, "{page}");
    assert!(page.contains("notice err"), "{page}");
    assert!(fx.bd_log().is_empty());
    let (code, _) = fx.form("POST", &format!("/plans/{id}/epic"), &token, "");
    assert_eq!(code, 409, "nothing to create");
    assert!(fx.bd_log().is_empty());
}
