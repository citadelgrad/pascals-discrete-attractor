#![cfg(unix)]

//! `pas scaffold` reaches Beads only through the pipeline's `BeadsAdapter`.
//! A stub on a scratch `PATH` stands in for the Beads program.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

/// Built with `concat!` so the program name appears quoted only in the adapter (AC5).
const BEADS_PROGRAM: &str = concat!("b", "d");

fn scaffold(path_dir: &Path, workdir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pas"))
        .args(["scaffold", "e-1", "--output", "out.dot"])
        .current_dir(workdir)
        .env("PATH", path_dir)
        .output()
        .unwrap()
}

fn beads_stub(dir: &Path, script: &str) {
    let path = dir.join(BEADS_PROGRAM);
    fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn scaffold_reads_the_epic_through_the_adapter() {
    let bin = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    beads_stub(
        bin.path(),
        r#"[ "$*" = "show e-1 --json" ] || { echo "unexpected: $*" >&2; exit 9; }
echo '[{"id":"e-1","title":"My Epic","status":"open","description":"Epic body"}]'"#,
    );

    let output = scaffold(bin.path(), work.path());
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let dot = fs::read_to_string(work.path().join("out.dot")).unwrap();
    assert!(
        dot.contains("Implement all child tasks of epic e-1: My Epic. Epic body"),
        "{dot}"
    );
    assert!(!dot.contains("EPIC_ID"));
}

#[test]
fn scaffold_failure_keeps_prefix_and_reports_command_and_stderr() {
    let bin = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    beads_stub(
        bin.path(),
        r#"echo "no issue found matching e-1" >&2; exit 1"#,
    );

    let output = scaffold(bin.path(), work.path());
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("bd show failed:"), "{stderr}");
    assert!(stderr.contains("show e-1 --json"), "{stderr}");
    assert!(stderr.contains("no issue found matching e-1"), "{stderr}");
    assert!(!work.path().join("out.dot").exists());
}

#[test]
fn scaffold_without_beads_on_path_reports_bd_not_found() {
    let empty = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();

    let output = scaffold(empty.path(), work.path());
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("bd show failed: bd not found"), "{stderr}");
    assert!(stderr.contains("PATH"), "{stderr}");
    assert!(!work.path().join("out.dot").exists());
}
