#![cfg(unix)]

//! `pas decompose --plan / --from-proposal / --json` (spec C6). Stubs on a
//! scratch `PATH` stand in for the Beads program and for `claude`; each logs
//! its argv so tests can assert which calls were (not) made.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

/// Built with `concat!` so the program name appears quoted only in the adapter.
const BEADS_PROGRAM: &str = concat!("b", "d");

const PROPOSAL: &str = r#"{"v":1,"epic":{"title":"Epic","description":"Epic body"},"tasks":[{"title":"A","type":"task","priority":"P2","description":"a"},{"title":"B","type":"task","priority":"P1","description":"b","acceptance":"acc"},{"title":"C","type":"task","priority":"P2","description":"c"}],"dependencies":[{"blocked":1,"blocker":0},{"blocked":2,"blocker":1}]}"#;

fn stub(dir: &Path, name: &str, script: &str) {
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// `bd` logs argv to bd.log; `create` prints sequential ids (id-1, id-2, ...).
fn beads_stub(dir: &Path) {
    let log = dir.join("bd.log");
    let counter = dir.join("bd.count");
    stub(
        dir,
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
}

/// `claude` logs its prompt to claude.log and answers with `proposal`.
fn claude_stub(dir: &Path, proposal: &str) {
    let log = dir.join("claude.log");
    let answer = dir.join("answer.json");
    fs::write(
        &answer,
        serde_json::json!({ "result": proposal }).to_string(),
    )
    .unwrap();
    stub(
        dir,
        "claude",
        &format!("echo \"$2\" >> {}\ncat {}", log.display(), answer.display()),
    );
}

fn log_lines(dir: &Path, name: &str) -> Vec<String> {
    fs::read_to_string(dir.join(name))
        .map(|s| s.lines().map(String::from).collect())
        .unwrap_or_default()
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

fn setup() -> (tempfile::TempDir, tempfile::TempDir) {
    let bin = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    beads_stub(bin.path());
    claude_stub(bin.path(), PROPOSAL);
    (bin, work)
}

/// AC1: --plan a.md --plan b.txt --dry-run --json prints the v1 Proposal, no bd call.
#[test]
fn plan_dry_run_json_prints_v1_proposal_without_bd() {
    let (bin, work) = setup();
    fs::write(work.path().join("a.md"), "# First").unwrap();
    fs::write(work.path().join("b.txt"), "Second").unwrap();

    let output = pas(
        &[
            "decompose",
            "--plan",
            "a.md",
            "--plan",
            "b.txt",
            "--dry-run",
            "--json",
        ],
        bin.path(),
        work.path(),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let out = json_line(&output);
    assert_eq!(out["v"], 1);
    assert_eq!(out["ok"], true);
    assert_eq!(out["proposal"]["v"], 1);
    assert_eq!(out["proposal"]["epic"]["title"], "Epic");
    assert_eq!(out["proposal"]["tasks"].as_array().unwrap().len(), 3);
    assert_eq!(out["proposal"]["dependencies"][1]["blocker"], 1);
    assert!(log_lines(bin.path(), "bd.log").is_empty());

    let prompt = fs::read_to_string(bin.path().join("claude.log")).unwrap();
    let first = prompt.find("# Plan document 1 of 2: a.md").expect(&prompt);
    let second = prompt.find("# Plan document 2 of 2: b.txt").expect(&prompt);
    assert!(first < second);
}

/// AC2: --from-proposal creates exactly the Proposal, no LLM call.
#[test]
fn from_proposal_creates_exactly_the_proposal() {
    let (bin, work) = setup();
    fs::write(work.path().join("p.json"), PROPOSAL).unwrap();

    let output = pas(
        &["decompose", "--from-proposal", "p.json", "--json"],
        bin.path(),
        work.path(),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let out = json_line(&output);
    assert_eq!(out["v"], 1);
    assert_eq!(out["ok"], true);
    assert_eq!(out["epic_id"], "id-1");
    assert_eq!(out["task_ids"], serde_json::json!(["id-2", "id-3", "id-4"]));

    let calls = log_lines(bin.path(), "bd.log");
    let creates: Vec<&String> = calls.iter().filter(|c| c.starts_with("create")).collect();
    assert_eq!(creates.len(), 4, "{calls:?}");
    assert!(creates[0].contains("--type epic"));
    assert!(creates[1].contains("--title A"));
    assert!(creates[2].contains("--title B") && creates[2].contains("--acceptance acc"));
    assert!(creates[3].contains("--title C"));
    let deps: Vec<&str> = calls
        .iter()
        .filter(|c| c.starts_with("dep add"))
        .map(String::as_str)
        .collect();
    assert_eq!(
        deps,
        [
            "dep add id-1 id-2 --json",
            "dep add id-1 id-3 --json",
            "dep add id-1 id-4 --json",
            "dep add id-3 id-2 --json",
            "dep add id-4 id-3 --json",
        ]
    );
    assert!(log_lines(bin.path(), "claude.log").is_empty());
}

/// AC3: a dependency on a missing Task index is rejected before any bd call.
#[test]
fn proposal_with_missing_task_index_is_rejected_before_bd() {
    let (bin, work) = setup();
    let bad = r#"{"v":1,"epic":{"title":"E","description":"d"},"tasks":[{"title":"A","description":"a"},{"title":"B","description":"b"}],"dependencies":[{"blocked":0,"blocker":5}]}"#;
    fs::write(work.path().join("p.json"), bad).unwrap();

    let output = pas(
        &["decompose", "--from-proposal", "p.json", "--json"],
        bin.path(),
        work.path(),
    );
    assert!(!output.status.success());
    let out = json_line(&output);
    assert_eq!(out["ok"], false);
    assert_eq!(out["error"]["code"], "invalid_proposal");
    assert!(out["error"]["message"]
        .as_str()
        .unwrap()
        .contains("blocker=5"));
    assert!(log_lines(bin.path(), "bd.log").is_empty());
}

#[test]
fn malformed_proposal_is_rejected_before_bd() {
    let (bin, work) = setup();
    fs::write(work.path().join("p.json"), "{not json").unwrap();
    let output = pas(
        &["decompose", "--from-proposal", "p.json", "--json"],
        bin.path(),
        work.path(),
    );
    assert!(!output.status.success());
    assert_eq!(json_line(&output)["error"]["code"], "invalid_proposal");
    assert!(log_lines(bin.path(), "bd.log").is_empty());
}

#[test]
fn plan_with_unsupported_extension_names_the_file() {
    let (bin, work) = setup();
    fs::write(work.path().join("notes.pdf"), "x").unwrap();
    let output = pas(
        &["decompose", "--plan", "notes.pdf", "--dry-run", "--json"],
        bin.path(),
        work.path(),
    );
    assert!(!output.status.success());
    let out = json_line(&output);
    assert_eq!(out["error"]["code"], "plan_input");
    assert!(out["error"]["message"]
        .as_str()
        .unwrap()
        .contains("notes.pdf"));
    assert!(log_lines(bin.path(), "claude.log").is_empty());
}

/// AC4: spec_path with --plan (and other bad combinations) is a usage error.
#[test]
fn spec_path_and_plan_conflict_is_a_usage_error() {
    let (bin, work) = setup();
    for args in [
        &["decompose", "spec.md", "--plan", "a.md"][..],
        &["decompose"][..],
        &["decompose", "--from-proposal", "p.json", "--plan", "a.md"][..],
        &["decompose", "--from-proposal", "p.json", "--dry-run"][..],
    ] {
        let output = pas(args, bin.path(), work.path());
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty());
    }
    let output = pas(
        &["decompose", "spec.md", "--plan", "a.md"],
        bin.path(),
        work.path(),
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot be used with"));
    assert!(log_lines(bin.path(), "bd.log").is_empty());
    assert!(log_lines(bin.path(), "claude.log").is_empty());
}

/// AC5: `decompose <spec>` keeps its human output and bd call sequence.
#[test]
fn spec_path_without_flags_is_unchanged() {
    let (bin, work) = setup();
    fs::write(work.path().join("spec.md"), "# Spec\n").unwrap();

    let output = pas(&["decompose", "spec.md"], bin.path(), work.path());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("✓ Decomposition complete"), "{stdout}");
    assert!(stdout.contains("Epic ID: id-1"), "{stdout}");
    assert!(stdout.contains("Tasks created: 3"), "{stdout}");
    assert!(stdout.contains("Dependencies: 2"), "{stdout}");

    let calls = log_lines(bin.path(), "bd.log");
    assert_eq!(calls.iter().filter(|c| c.starts_with("create")).count(), 4);
    assert_eq!(calls.iter().filter(|c| c.starts_with("dep add")).count(), 5);
}

#[test]
fn spec_path_dry_run_prints_human_text_without_bd_creates() {
    let (bin, work) = setup();
    fs::write(work.path().join("spec.md"), "# Spec\n").unwrap();
    let output = pas(
        &["decompose", "spec.md", "--dry-run"],
        bin.path(),
        work.path(),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Decomposition (dry run):"), "{stdout}");
    assert!(stdout.contains("Tasks (3):"), "{stdout}");
    assert!(!log_lines(bin.path(), "bd.log")
        .iter()
        .any(|c| c.starts_with("create")));
}
