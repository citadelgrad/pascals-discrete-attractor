#![cfg(unix)]

//! `pas generate --plan / --json` (spec C6). A stub `claude` on a scratch
//! `PATH` logs its stdin (the prompt) and answers with a tiny Pipeline.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

const DOT: &str = r#"digraph G { start [shape="Mdiamond"] work [shape="box" llm_provider="claude" timeout="300s" prompt="x"] done [shape="Msquare"] start -> work -> done }"#;

/// `claude` copies stdin to claude.log and prints `result` as the model answer.
fn claude_stub(dir: &Path, result: &str) {
    let answer = dir.join("answer.json");
    fs::write(&answer, serde_json::json!({ "result": result }).to_string()).unwrap();
    let path = dir.join("claude");
    fs::write(
        &path,
        format!(
            "#!/bin/sh\ncat > {}\ncat {}\n",
            dir.join("claude.log").display(),
            answer.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn pas(args: &[&str], bin: &Path, work: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pas"))
        .args(args)
        .current_dir(work)
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .output()
        .unwrap()
}

fn json_line(output: &Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "stdout: {stdout}");
    serde_json::from_str(lines[0]).unwrap()
}

/// A scratch dir with the stub and two Plan documents.
fn setup(result: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    claude_stub(dir.path(), result);
    fs::write(dir.path().join("a.md"), "ALPHA content").unwrap();
    fs::write(dir.path().join("b.md"), "BETA content").unwrap();
    dir
}

#[test]
fn plan_json_writes_pipeline_and_prints_path() {
    let dir = setup(DOT);
    let out = pas(
        &[
            "generate",
            "--plan",
            "a.md",
            "--plan",
            "b.md",
            "--json",
            "-o",
            "out/p.dot",
        ],
        dir.path(),
        dir.path(),
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = json_line(&out);
    assert_eq!(v["v"], 1);
    assert_eq!(v["ok"], true);
    let expected = fs::canonicalize(dir.path().join("out/p.dot")).unwrap();
    assert_eq!(v["pipeline_path"], expected.to_str().unwrap());
    assert!(fs::read_to_string(expected).unwrap().contains("digraph"));
}

#[test]
fn plan_default_output_uses_last_file_stem() {
    let dir = setup(DOT);
    let out = pas(
        &["generate", "--plan", "a.md", "--plan", "b.md", "--json"],
        dir.path(),
        dir.path(),
    );
    assert!(out.status.success());
    assert!(dir.path().join("pipelines/b.dot").exists());
}

#[test]
fn plan_prompt_contains_documents_in_order_with_headings() {
    let dir = setup(DOT);
    let out = pas(
        &[
            "generate", "--plan", "a.md", "--plan", "b.md", "--json", "-o", "p.dot",
        ],
        dir.path(),
        dir.path(),
    );
    assert!(out.status.success());
    let prompt = fs::read_to_string(dir.path().join("claude.log")).unwrap();
    let h1 = prompt.find("# Plan document 1 of 2: a.md").unwrap();
    let a = prompt.find("ALPHA content").unwrap();
    let h2 = prompt.find("# Plan document 2 of 2: b.md").unwrap();
    let b = prompt.find("BETA content").unwrap();
    assert!(h1 < a && a < h2 && h2 < b);
}

#[test]
fn plan_conflicts_are_usage_errors() {
    for extra in [["--spec", "b.md"], ["--prd", "b.md"], ["b.md", "--json"]] {
        let dir = setup(DOT);
        let mut args = vec!["generate", "--plan", "a.md"];
        args.extend(extra);
        let out = pas(&args, dir.path(), dir.path());
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty());
        assert!(String::from_utf8_lossy(&out.stderr).contains("cannot be used with"));
        assert!(!dir.path().join("claude.log").exists());
    }
}

#[test]
fn json_stdout_is_one_object_and_stderr_has_no_spinner() {
    let dir = setup(DOT);
    let out = pas(
        &["generate", "--plan", "a.md", "--json", "-o", "p.dot"],
        dir.path(),
        dir.path(),
    );
    assert!(out.status.success());
    json_line(&out);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("Generating pipeline"), "{stderr}");
    assert!(!stderr.contains('⠋'), "{stderr}");
}

#[test]
fn json_failures_are_one_error_object() {
    // Unsupported Plan file type.
    let dir = setup(DOT);
    fs::write(dir.path().join("c.pdf"), "x").unwrap();
    let out = pas(
        &["generate", "--plan", "c.pdf", "--json"],
        dir.path(),
        dir.path(),
    );
    assert_eq!(out.status.code(), Some(1));
    let v = json_line(&out);
    assert_eq!((v["v"].clone(), v["ok"].clone()), (1.into(), false.into()));
    assert_eq!(v["error"]["code"], "plan_input");
    assert!(!dir.path().join("claude.log").exists());

    // Model answers without a digraph.
    let dir = setup("sorry, no graph");
    let out = pas(
        &["generate", "--plan", "a.md", "--json"],
        dir.path(),
        dir.path(),
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(json_line(&out)["error"]["code"], "invalid_dot");
}

#[test]
fn spec_only_human_output_is_unchanged() {
    let dir = setup(DOT);
    let out = pas(&["generate", "a.md", "-o", "p.dot"], dir.path(), dir.path());
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    for needle in [
        "Pipeline generated",
        "  Spec: a.md",
        "  Nodes: 3",
        "Validation: PASSED",
    ] {
        assert!(stdout.contains(needle), "{stdout}");
    }
}
