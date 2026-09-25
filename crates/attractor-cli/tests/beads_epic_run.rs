#![cfg(unix)]
//! End-to-end epic Run against a temporary Beads workspace (spec Testing
//! Strategy, T3-6): the real `pas run` picks each Task with `beads.select`,
//! commits with a tool node, pushes to a local bare remote, and closes the
//! Task with `beads.close require_upstream=true`.
//!
//! The Pipeline has no model nodes, so the Run costs nothing and needs no
//! provider. `pas run --dry-run` cannot be used: it skips tool nodes and
//! never claims or closes Tasks.
//!
//! Every path the Run touches lives in one temp folder outside the
//! repository, and `BEADS_DIR` points `bd` at a workspace created there, so
//! the repository's own `.beads` is never read or changed. The test skips
//! with a notice when `bd` is not on `PATH`, and fails instead when
//! `PAS_REQUIRE_BD=1`.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use attractor_pipeline::beads_adapter::bd_on_path;
use attractor_pipeline::{BeadsAdapter, NewIssue};
use serde_json::Value;

/// Prefix of the temporary workspace's IDs; the repository uses `attractor-`.
const PREFIX: &str = "pe";

fn pas() -> &'static str {
    env!("CARGO_BIN_EXE_pas")
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// Whether the test can run: `bd` on `PATH` and no `BEADS_DB` override.
fn beads_available() -> bool {
    if !bd_on_path(std::env::var_os("PATH").as_deref()) {
        assert!(
            std::env::var("PAS_REQUIRE_BD").as_deref() != Ok("1"),
            "PAS_REQUIRE_BD=1 but bd is not on PATH"
        );
        eprintln!("skipping: bd not on PATH");
        return false;
    }
    if std::env::var_os("BEADS_DB").is_some() {
        eprintln!("skipping: BEADS_DB is set and would override the test workspace");
        return false;
    }
    true
}

/// `git -C <dir> <args>` isolated from any outer repository's variables.
fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn is_ancestor(repo: &Path, commit: &str, of: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["merge-base", "--is-ancestor", commit, of])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .status()
        .unwrap()
        .success()
}

/// A model-free Pipeline with the same Task loop as `templates/epic-runner.dot`.
fn pipeline(epic: &str) -> String {
    format!(
        r#"digraph EpicE2E {{
    graph [goal="Close every Task of the Epic"]
    start [shape="Mdiamond"]
    done  [shape="Msquare"]
    pick_task  [shape="diamond", type="beads.select", epic="{epic}"]
    blocked    [shape="diamond", label="Blocked"]
    work       [shape="parallelogram", tool_command="git commit -q --allow-empty -m work"]
    publish    [shape="parallelogram", tool_command="git push -q"]
    close_task [shape="box", type="beads.close", require_upstream=true]
    start -> pick_task
    pick_task -> work    [label="MORE", condition="preferred_label=MORE"]
    pick_task -> done    [label="DONE", condition="preferred_label=DONE"]
    pick_task -> blocked [label="BLOCKED", condition="preferred_label=BLOCKED"]
    blocked -> done
    work -> publish
    publish -> close_task
    close_task -> pick_task [label="CLOSED", condition="outcome=success", loop_restart=true]
    close_task -> publish   [label="UNPUBLISHED", condition="outcome=fail"]
}}
"#
    )
}

fn events(run_dir: &Path) -> Vec<Value> {
    fs::read_to_string(run_dir.join("events.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn of_type<'a>(events: &'a [Value], ty: &str) -> Vec<&'a Value> {
    events.iter().filter(|e| e["type"] == ty).collect()
}

fn task_ids(events: &[&Value]) -> Vec<String> {
    events
        .iter()
        .map(|e| e["data"]["task_id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn epic_run_claims_commits_pushes_and_closes_both_tasks() {
    if !beads_available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let beads_home = root.join("beads");
    let beads_dir = beads_home.join(".beads");
    let remote = root.join("remote.git");
    let work = root.join("work");
    let logs = root.join("logs");
    let state = root.join("state");
    let pipeline_path = root.join("p.dot");

    // AC3: everything the Run touches is under the temp root, which is not
    // inside this repository.
    let repo = repo_root();
    assert!(!root.starts_with(&repo), "{root:?} is inside {repo:?}");
    for path in [&beads_dir, &work, &logs, &state, &pipeline_path, &remote] {
        assert!(path.starts_with(&root), "{path:?} escapes {root:?}");
    }

    // Temporary Beads workspace: an Epic with two Tasks, A before B.
    fs::create_dir(&beads_home).unwrap();
    git(&beads_home, &["init", "-q"]);
    let bd = BeadsAdapter::new()
        .in_dir(&beads_home)
        .with_env("BEADS_DIR", &beads_dir)
        .with_env("BEADS_ACTOR", "pas-test");
    bd.init(PREFIX).await.unwrap();
    let issue = |title, issue_type, priority, parent| NewIssue {
        title,
        issue_type,
        priority: Some(priority),
        description: "end-to-end test issue",
        acceptance: None,
        design: None,
        notes: None,
        parent,
    };
    let epic = bd.create(&issue("Epic", "epic", "1", None)).await.unwrap();
    let task_a = bd
        .create(&issue("Task A", "task", "1", Some(&epic)))
        .await
        .unwrap();
    let task_b = bd
        .create(&issue("Task B", "task", "2", Some(&epic)))
        .await
        .unwrap();

    // Work repo on `main`, tracking a local bare remote. Local config keeps
    // the user's signing and hooks away from the tool-node commits.
    git(&root, &["init", "-q", "--bare", "-b", "main", "remote.git"]);
    fs::create_dir(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    for (key, value) in [
        ("user.name", "PAS Test"),
        ("user.email", "pas-test@example.com"),
        ("commit.gpgsign", "false"),
        ("core.hooksPath", "/dev/null"),
    ] {
        git(&work, &["config", key, value]);
    }
    git(&work, &["commit", "-q", "--allow-empty", "-m", "initial"]);
    git(
        &work,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&work, &["push", "-q", "-u", "origin", "main"]);

    fs::write(&pipeline_path, pipeline(&epic)).unwrap();
    let output = Command::new(pas())
        .arg("run")
        .arg(&pipeline_path)
        .arg("--workdir")
        .arg(&work)
        .arg("--logs")
        .arg(&logs)
        .args(["--max-steps", "40"])
        .current_dir(&root)
        .env("BEADS_DIR", &beads_dir)
        .env("BEADS_ACTOR", "pas-test")
        .env("PAS_STATE_DIR", &state)
        .env_remove("PAS_HEARTBEAT_INTERVAL_MS")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "pas run failed ({}):\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let runs: Vec<PathBuf> = fs::read_dir(logs.join("runs"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(runs.len(), 1, "expected exactly one Run folder: {runs:?}");
    let events = events(&runs[0]);

    // AC1: both Tasks are closed in bd, and the last selection returned DONE.
    let children = bd.children(&epic).await.unwrap();
    assert_eq!(children.len(), 2, "children: {children:?}");
    for child in &children {
        assert_eq!(child.status, "closed", "{child:?}");
    }
    let picks: Vec<&Value> = of_type(&events, "EdgeSelected")
        .into_iter()
        .filter(|e| e["data"]["from_node"] == "pick_task")
        .collect();
    let last = picks.last().expect("no edge out of pick_task");
    assert_eq!(last["data"]["edge_label"], "DONE", "{last}");
    assert_eq!(last["data"]["to_node"], "done", "{last}");
    let labels: Vec<&str> = picks
        .iter()
        .map(|e| e["data"]["edge_label"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(labels, ["MORE", "MORE", "DONE"]);
    let selections = of_type(&events, "StageCompleted")
        .into_iter()
        .filter(|e| e["data"]["node_id"] == "pick_task")
        .count();
    assert_eq!(selections, 3);
    assert!(of_type(&events, "TaskSelectionBlocked").is_empty());

    // AC2: 2 TaskClaimed and 2 TaskClosed, in the same order, matching the
    // IDs `bd children` reports.
    let claimed_events = of_type(&events, "TaskClaimed");
    let closed_events = of_type(&events, "TaskClosed");
    let claimed = task_ids(&claimed_events);
    let closed = task_ids(&closed_events);
    assert_eq!(claimed, [task_a.clone(), task_b.clone()]);
    assert_eq!(closed, claimed);
    let child_ids: BTreeSet<String> = children.iter().map(|c| c.id.clone()).collect();
    assert_eq!(claimed.iter().cloned().collect::<BTreeSet<_>>(), child_ids);
    for event in &claimed_events {
        assert_eq!(event["data"]["epic_id"], epic.as_str(), "{event}");
        assert_eq!(event["data"]["node_id"], "pick_task", "{event}");
    }
    // Each Task's Run Commits were pushed to the bare remote's `main`.
    let mut all_commits = BTreeSet::new();
    for event in &closed_events {
        assert_eq!(event["data"]["upstream_verified"], true, "{event}");
        let commits = event["data"]["commits"].as_array().unwrap();
        assert!(!commits.is_empty(), "no Run Commits: {event}");
        for sha in commits {
            let sha = sha.as_str().unwrap();
            assert!(
                all_commits.insert(sha.to_string()),
                "{sha} attributed twice"
            );
            assert!(
                is_ancestor(&remote, sha, "main"),
                "{sha} is not on the remote's main"
            );
        }
    }

    // AC3: every ID the handlers saw came from the temporary workspace.
    for id in child_ids.iter().chain(&claimed).chain(&closed) {
        assert!(id.starts_with(&format!("{PREFIX}-")), "{id}");
    }
    assert!(epic.starts_with(&format!("{PREFIX}-")), "{epic}");
}
