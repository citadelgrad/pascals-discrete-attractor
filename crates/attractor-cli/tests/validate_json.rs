//! `pas validate --json` prints the C6 payload; human output is unchanged.

use std::path::Path;
use std::process::{Command, Output};

const VALID: &str = r#"digraph G {
    start [shape="Mdiamond"]
    work [shape="box", llm_provider="codex", prompt="Do work"]
    done [shape="Msquare"]
    start -> work -> done
}"#;

const INVALID: &str = r#"digraph G {
    start [shape="Mdiamond"]
    a [shape="box", prompt="A"]
    b [shape="box", llm_provider="llama", prompt="B"]
    done [shape="Msquare"]
    start -> a -> b -> done
}"#;

const WARNING_ONLY: &str = r#"digraph G {
    start [shape="Mdiamond"]
    review [shape="box", llm_provider="codex"]
    done [shape="Msquare"]
    start -> review -> done
}"#;

fn pas(args: &[&str], file: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pas"))
        .arg("validate")
        .arg(file)
        .args(args)
        .output()
        .unwrap()
}

fn write(dir: &tempfile::TempDir, content: &str) -> std::path::PathBuf {
    let path = dir.path().join("p.dot");
    std::fs::write(&path, content).unwrap();
    path
}

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

#[test]
fn valid_pipeline_prints_valid_true_and_empty_diagnostics() {
    let dir = tempfile::tempdir().unwrap();
    let out = pas(&["--json"], &write(&dir, VALID));
    assert!(out.status.success());
    let json = json_line(&out);
    assert_eq!(json["v"], 1);
    assert_eq!(json["ok"], true);
    assert_eq!(json["valid"], true);
    assert_eq!(json["diagnostics"], serde_json::json!([]));
    assert_eq!(keys(&json), ["diagnostics", "ok", "v", "valid"]);
}

#[test]
fn invalid_pipeline_prints_valid_false_with_one_diagnostic_per_error() {
    let dir = tempfile::tempdir().unwrap();
    let file = write(&dir, INVALID);
    let out = pas(&["--json"], &file);
    assert_eq!(out.status.code(), Some(1));
    let json = json_line(&out);
    assert_eq!(json["ok"], true);
    assert_eq!(json["valid"], false);

    let diagnostics = json["diagnostics"].as_array().unwrap();
    let errors: Vec<_> = diagnostics
        .iter()
        .filter(|d| d["severity"] == "error")
        .collect();
    assert!(errors.len() >= 2, "{diagnostics:?}");
    for d in diagnostics {
        assert!(d["severity"].is_string());
        assert!(!d["message"].as_str().unwrap().is_empty());
    }
    assert!(errors.iter().any(|d| d["node_id"] == "a"));
    assert!(errors.iter().any(|d| d["node_id"] == "b"));
    assert!(errors.iter().any(|d| d["fix"].is_string()));

    // One JSON error per `[ERROR]` line of the human report.
    let human = pas(&[], &file);
    let human_errors = String::from_utf8_lossy(&human.stdout)
        .lines()
        .filter(|l| l.starts_with("[ERROR]"))
        .count();
    assert_eq!(errors.len(), human_errors);
}

#[test]
fn missing_file_prints_ok_false_with_error_code() {
    let dir = tempfile::tempdir().unwrap();
    let out = pas(&["--json"], &dir.path().join("nope.dot"));
    assert_eq!(out.status.code(), Some(1));
    let json = json_line(&out);
    assert_eq!(json["v"], 1);
    assert_eq!(json["ok"], false);
    assert_eq!(json["error"]["code"], "io");
    assert!(!json["error"]["message"].as_str().unwrap().is_empty());
    assert_eq!(keys(&json), ["error", "ok", "v"]);
}

#[test]
fn unparseable_dot_prints_invalid_dot_error() {
    let dir = tempfile::tempdir().unwrap();
    let out = pas(&["--json"], &write(&dir, "this is not dot {{{"));
    assert_eq!(out.status.code(), Some(1));
    let json = json_line(&out);
    assert_eq!(json["ok"], false);
    assert_eq!(json["error"]["code"], "invalid_dot");
    assert!(!json.contains_key("valid"));
}

#[test]
fn warnings_only_pipeline_is_valid_with_diagnostics() {
    let dir = tempfile::tempdir().unwrap();
    let out = pas(&["--json"], &write(&dir, WARNING_ONLY));
    assert!(out.status.success());
    let json = json_line(&out);
    assert_eq!(json["valid"], true);
    let diagnostics = json["diagnostics"].as_array().unwrap();
    assert!(!diagnostics.is_empty());
    assert!(diagnostics.iter().all(|d| d["severity"] != "error"));
    assert!(diagnostics.iter().any(|d| d["severity"] == "warning"));
}

#[test]
fn human_output_unchanged_without_json() {
    let dir = tempfile::tempdir().unwrap();
    let out = pas(&[], &write(&dir, VALID));
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "Pipeline is valid\n");

    let out = pas(&[], &write(&dir, INVALID));
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stdout).contains("[ERROR]"));
}
