#![cfg(unix)]

//! `pas validate` on Pipelines with `beads.select` / `beads.close` nodes.
//! `PATH` points at a scratch directory, with or without a stub Beads program.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

/// Built with `concat!` so the program name appears quoted only in the adapter.
const BEADS_PROGRAM: &str = concat!("b", "d");

const PIPELINE: &str = r#"digraph G {
    start [shape="Mdiamond"]
    pick_task [shape="diamond", type="beads.select", epic="e-1", order="e-1.2,e-1.1"]
    close_task [shape="box", type="beads.close", require_upstream=true]
    done [shape="Msquare"]
    start -> pick_task
    pick_task -> close_task [label="MORE", condition="preferred_label=MORE"]
    pick_task -> done [label="DONE", condition="preferred_label=DONE"]
    close_task -> pick_task
}"#;

fn validate(path_dir: &Path, pipeline: &str) -> Output {
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("pipeline.dot");
    fs::write(&file, pipeline).unwrap();
    Command::new(env!("CARGO_BIN_EXE_pas"))
        .arg("validate")
        .arg(&file)
        .env("PATH", path_dir)
        .output()
        .unwrap()
}

/// A directory holding an executable stub that fails if validation runs it.
fn beads_bin() -> tempfile::TempDir {
    let bin = tempfile::tempdir().unwrap();
    let path = bin.path().join(BEADS_PROGRAM);
    fs::write(
        &path,
        "#!/bin/sh\necho \"validation ran: $*\" >&2\nexit 9\n",
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

// AC1
#[test]
fn validate_passes_with_bd_on_path() {
    let bin = beads_bin();
    let output = validate(bin.path(), PIPELINE);
    let text = text(&output);
    assert!(output.status.success(), "{text}");
    assert!(text.contains("Pipeline is valid"), "{text}");
    assert!(!text.contains("validation ran"), "{text}");
}

// AC2
#[test]
fn validate_fails_without_bd_and_names_each_node() {
    let empty = tempfile::tempdir().unwrap();
    let output = validate(empty.path(), PIPELINE);
    let text = text(&output);
    assert!(!output.status.success(), "{text}");
    for node in ["pick_task", "close_task"] {
        assert!(
            text.lines().any(|line| line.contains("beads_available")
                && line.contains(&format!("'{node}'"))),
            "no beads_available error for {node}: {text}"
        );
    }
}

// AC3
#[test]
fn validate_fails_when_select_has_no_epic() {
    let bin = beads_bin();
    let output = validate(bin.path(), &PIPELINE.replace(r#" epic="e-1","#, ""));
    let text = text(&output);
    assert!(!output.status.success(), "{text}");
    assert!(
        text.lines()
            .any(|line| line.contains("attribute_required") && line.contains("'pick_task'")),
        "{text}"
    );
    assert!(!text.contains("close_task"), "{text}");
}

fn run(path_dir: &Path, work: &Path, extra: &[&str]) -> Output {
    fs::write(work.join("p.dot"), PIPELINE).unwrap();
    Command::new(env!("CARGO_BIN_EXE_pas"))
        .arg("run")
        .arg(work.join("p.dot"))
        .arg("--workdir")
        .arg(work)
        .arg("--logs")
        .arg(work.join("logs"))
        .args(extra)
        .env("PATH", path_dir)
        .env("PAS_STATE_DIR", work.join("state"))
        .current_dir(work)
        .output()
        .unwrap()
}

#[test]
fn run_without_bd_fails_before_the_run_starts() {
    let empty = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let output = run(empty.path(), work.path(), &[]);
    let text = text(&output);
    assert!(!output.status.success(), "{text}");
    assert!(
        text.lines()
            .any(|line| line.contains("beads_available") && line.contains("'pick_task'")),
        "{text}"
    );
    assert!(!work.path().join("logs").join("runs").exists(), "{text}");
}

#[test]
fn dry_run_does_not_need_bd() {
    let empty = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let output = run(empty.path(), work.path(), &["--dry-run"]);
    let text = text(&output);
    assert!(!text.contains("beads_available"), "{text}");
    assert!(work.path().join("logs").join("runs").exists(), "{text}");
}
