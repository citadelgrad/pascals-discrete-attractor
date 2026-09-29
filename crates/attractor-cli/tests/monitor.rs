#![cfg(feature = "monitor")]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn pas() -> Command {
    Command::new(env!("CARGO_BIN_EXE_pas"))
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn get_status(port: u16, host: &str) -> u16 {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        s,
        "GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out.split(' ').nth(1).unwrap().parse().unwrap()
}

#[test]
fn help_lists_monitor_with_default_port() {
    let out = pas().arg("--help").output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("monitor"));
    let out = pas().args(["monitor", "--help"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("[default: 7777]"), "{text}");
    assert!(text.contains("--open"), "{text}");
}

#[test]
fn listens_on_given_port_and_rejects_foreign_host() {
    let port = free_port();
    let child = pas()
        .args(["monitor", "--port", &port.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _guard = Kill(child);
    let deadline = Instant::now() + Duration::from_secs(10);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(
            Instant::now() < deadline,
            "monitor never listened on {port}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(get_status(port, &format!("127.0.0.1:{port}")), 200);
    assert_eq!(get_status(port, "evil.example"), 403);
}

#[test]
fn port_in_use_exits_nonzero_naming_port() {
    let held = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = held.local_addr().unwrap().port();
    let out = pas()
        .args(["monitor", "--port", &port.to_string()])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains(&port.to_string()), "{err}");
}
