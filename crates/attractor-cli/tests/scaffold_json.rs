#![cfg(unix)]

//! `pas scaffold` writes a Beads-driven Pipeline that `pas validate` accepts,
//! and `--json` prints the C6 payload. A stub on a scratch `PATH` stands in
//! for the Beads program; every test writes into a tempdir via `--output`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

/// Built with `concat!` so the program name appears quoted only in the adapter.
const BEADS_PROGRAM: &str = concat!("b", "d");

const EPIC_E1: &str = r#"[ "$*" = "show e-1 --json" ] || { echo "no issue found matching $2" >&2; exit 1; }
echo '[{"id":"e-1","title":"My Epic","status":"open","description":"Epic body"}]'"#;

fn beads_stub(dir: &Path, script: &str) {
    let path = dir.join(BEADS_PROGRAM);
    fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn pas(args: &[&str], path_dir: &Path, workdir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pas"))
        .args(args)
        .current_dir(workdir)
        .env("PATH", path_dir)
        .output()
        .unwrap()
}

/// The single JSON object `--json` printed, asserting nothing else is on stdout.
fn json_line(output: &Output) -> serde_json::Map<String, serde_json::Value> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "stdout: {stdout}");
    match serde_json::from_str(lines[0]).unwrap() {
        serde_json::Value::Object(map) => map,
        other => panic!("expected an object, got {other}"),
    }
}

fn keys(map: &serde_json::Map<String, serde_json::Value>) -> Vec<&str> {
    let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
    keys.sort();
    keys
}

/// AC1: the scaffolded file has one beads.select with epic="e-1" and one
/// beads.close, and the real `pas validate` accepts it with bd on PATH.
#[test]
fn scaffold_output_passes_pas_validate() {
    let bin = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    beads_stub(bin.path(), EPIC_E1);

    let output = pas(
        &["scaffold", "e-1", "--output", "out.dot"],
        bin.path(),
        work.path(),
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("✓ Pipeline scaffolded"), "{stdout}");
    assert!(stdout.contains("Validation: PASSED"), "{stdout}");

    let dot = fs::read_to_string(work.path().join("out.dot")).unwrap();
    assert_eq!(dot.matches(r#"type="beads.select""#).count(), 1, "{dot}");
    assert_eq!(dot.matches(r#"type="beads.close""#).count(), 1, "{dot}");
    assert!(dot.contains(r#"epic="e-1""#), "{dot}");

    let validate = pas(&["validate", "out.dot"], bin.path(), work.path());
    let stdout = String::from_utf8_lossy(&validate.stdout);
    assert!(validate.status.success(), "{stdout}");
    assert!(stdout.contains("Pipeline is valid"), "{stdout}");
}

/// AC4: exactly one JSON object with v=1, ok=true and an absolute
/// pipeline_path that exists; no other keys.
#[test]
fn scaffold_json_success_is_one_object_with_existing_path() {
    let bin = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    beads_stub(bin.path(), EPIC_E1);

    let output = pas(
        &["scaffold", "e-1", "--output", "nested/out.dot", "--json"],
        bin.path(),
        work.path(),
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let payload = json_line(&output);
    assert_eq!(keys(&payload), ["ok", "pipeline_path", "v"]);
    assert_eq!(payload["v"], 1);
    assert_eq!(payload["ok"], true);
    let path = Path::new(payload["pipeline_path"].as_str().unwrap());
    assert!(path.is_absolute(), "{}", path.display());
    assert!(path.is_file(), "{}", path.display());
    assert_eq!(
        path,
        fs::canonicalize(work.path().join("nested/out.dot")).unwrap()
    );
}

/// AC5: a missing Epic gives ok=false, error code epic_not_found with bd's
/// message, a non-zero exit, and no Pipeline file.
#[test]
fn scaffold_json_missing_epic_reports_error_code() {
    let bin = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    beads_stub(bin.path(), EPIC_E1);

    let output = pas(
        &["scaffold", "e-404", "--output", "out.dot", "--json"],
        bin.path(),
        work.path(),
    );
    assert!(!output.status.success());
    let payload = json_line(&output);
    assert_eq!(keys(&payload), ["error", "ok", "v"]);
    assert_eq!(payload["v"], 1);
    assert_eq!(payload["ok"], false);
    assert_eq!(payload["error"]["code"], "epic_not_found");
    let message = payload["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("no issue found matching e-404"),
        "{message}"
    );
    assert!(!work.path().join("out.dot").exists());
}

/// Without bd on PATH, `--json` reports bd_not_found and exits non-zero.
#[test]
fn scaffold_json_without_bd_reports_bd_not_found() {
    let empty = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();

    let output = pas(
        &["scaffold", "e-1", "--output", "out.dot", "--json"],
        empty.path(),
        work.path(),
    );
    assert!(!output.status.success());
    let payload = json_line(&output);
    assert_eq!(payload["ok"], false);
    assert_eq!(payload["error"]["code"], "bd_not_found");
    assert!(!work.path().join("out.dot").exists());
}

/// Unreadable bd output is a bd_failed error, not a missing Epic.
#[test]
fn scaffold_json_with_unparseable_bd_output_reports_bd_failed() {
    let bin = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    beads_stub(bin.path(), "echo 'not json'");

    let output = pas(
        &["scaffold", "e-1", "--output", "out.dot", "--json"],
        bin.path(),
        work.path(),
    );
    assert!(!output.status.success());
    let payload = json_line(&output);
    assert_eq!(payload["error"]["code"], "bd_failed");
}

/// A Pipeline that cannot be written is write_failed.
#[test]
fn scaffold_json_unwritable_output_reports_write_failed() {
    let bin = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    beads_stub(bin.path(), EPIC_E1);
    fs::write(work.path().join("file"), "").unwrap();

    let output = pas(
        &["scaffold", "e-1", "--output", "file/out.dot", "--json"],
        bin.path(),
        work.path(),
    );
    assert!(!output.status.success());
    let payload = json_line(&output);
    assert_eq!(payload["error"]["code"], "write_failed");
}
