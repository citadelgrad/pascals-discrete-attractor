//! Run Index append under concurrent processes (spec C4).
//!
//! The parent test re-executes this test binary; each child runs
//! `index_child_append`, which is a no-op unless `CHILD_ENV` is set.

use std::io::BufRead;
use std::path::PathBuf;
use std::process::Command;

use attractor_journal::{append_entry, IndexEntry, INDEX_FILE};

const CHILD_ENV: &str = "PAS_JOURNAL_INDEX_CHILD";
const CHILDREN: usize = 10;
/// Well above PIPE_BUF, so interleaved unlocked writes would tear lines.
const PAYLOAD: usize = 64 * 1024;

#[test]
fn index_child_append() {
    let Ok(n) = std::env::var(CHILD_ENV) else {
        return;
    };
    let run_id = format!("0192a3b4-c5d6-7e8f-9a0b-{:012}", n.parse::<u64>().unwrap());
    let workdir = PathBuf::from(format!("/{}", n.repeat(PAYLOAD / n.len())));
    let entry = IndexEntry::new(
        run_id,
        chrono::Utc::now(),
        workdir,
        PathBuf::from("/abs/pipelines/x.dot"),
        PathBuf::from("/abs/repo/.pas/logs/x/runs"),
    );
    append_entry(&entry).unwrap();
}

#[test]
fn index_append_from_10_processes() {
    let state = tempfile::tempdir().unwrap();
    let exe = std::env::current_exe().unwrap();
    let children: Vec<_> = (0..CHILDREN)
        .map(|n| {
            Command::new(&exe)
                .args([
                    "index_child_append",
                    "--exact",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(CHILD_ENV, n.to_string())
                .env("PAS_STATE_DIR", state.path())
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }

    let bytes = std::fs::read(state.path().join(INDEX_FILE)).unwrap();
    assert!(bytes.ends_with(b"\n"));
    let lines: Vec<String> = bytes.lines().map(Result::unwrap).collect();
    assert_eq!(lines.len(), CHILDREN);
    let mut ids = std::collections::BTreeSet::new();
    for line in &lines {
        let entry: IndexEntry = serde_json::from_str(line).expect("torn Index line");
        assert!(entry.workdir.as_os_str().len() > PAYLOAD / 2);
        ids.insert(entry.run_id);
    }
    assert_eq!(ids.len(), CHILDREN);
}

#[test]
fn index_append_waits_for_the_index_lock() {
    let state = tempfile::tempdir().unwrap();
    let index = state.path().join(INDEX_FILE);
    let held = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&index)
        .unwrap();
    held.lock().unwrap();

    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "index_child_append",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, "7")
        .env("PAS_STATE_DIR", state.path())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(
        child.try_wait().unwrap().is_none(),
        "append finished while another process held the Index lock"
    );
    assert_eq!(std::fs::metadata(&index).unwrap().len(), 0);

    held.unlock().unwrap();
    assert!(child.wait().unwrap().success());
    let text = std::fs::read_to_string(&index).unwrap();
    assert_eq!(text.lines().count(), 1);
    serde_json::from_str::<IndexEntry>(text.trim_end()).unwrap();
}
