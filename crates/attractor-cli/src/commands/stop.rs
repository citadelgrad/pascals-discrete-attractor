//! `pas stop <run-id> [--source cli|monitor] [--json]`: ask an active Run to
//! stop after its current stage by creating `control/stop` (spec File Change
//! 10 and 12, C1, C6).
//!
//! The Run notices the file between stages, journals `StopRequested` and
//! `AttemptEnded{stopped}` itself, and removes the file at its next Attempt.
//! This command never writes the journal and takes no Run lock.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use attractor_journal::{parse_run_id, read_index_at, run_status, RunDir, RunStatus};
use serde::Serialize;

use super::answer::AnswerSourceArg;
use super::run::RunRefused;
use super::runs::pid_alive;

#[derive(Debug)]
enum StopError {
    UnknownRun(String),
    RunMissing { run_id: String, run_dir: PathBuf },
    NotActive { run_id: String, status: RunStatus },
    Io(String),
}

impl StopError {
    /// Stable `error.code` of the `--json` failure object.
    fn code(&self) -> &'static str {
        match self {
            Self::UnknownRun(_) => "unknown_run",
            Self::RunMissing { .. } => "run_missing",
            Self::NotActive { .. } => "not_active",
            Self::Io(_) => "io_error",
        }
    }
}

impl std::fmt::Display for StopError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownRun(id) => write!(f, "unknown run-id {id}: not in the Run Index"),
            Self::RunMissing { run_id, run_dir } => write!(
                f,
                "run {run_id} is missing: its folder {} no longer exists",
                run_dir.display()
            ),
            Self::NotActive { run_id, status } => {
                write!(f, "run {run_id} is not active (status: {status})")
            }
            Self::Io(message) => f.write_str(message),
        }
    }
}

/// A recorded stop request.
#[derive(Debug, Serialize)]
struct Stopped {
    v: u32,
    ok: bool,
    run_id: String,
    stop_path: PathBuf,
    already_requested: bool,
}

/// Request a stop of Run `run_id` found through the Index at `index`
/// (`None`: no state folder, so an empty Index). `is_alive` probes PIDs.
fn stop(
    index: Option<&Path>,
    run_id: &str,
    source: &str,
    now: chrono::DateTime<chrono::Utc>,
    is_alive: impl Fn(u32) -> bool,
) -> Result<Stopped, StopError> {
    let unknown_run = || StopError::UnknownRun(run_id.to_string());
    let run_id = parse_run_id(run_id).ok_or_else(unknown_run)?;
    let entries = match index {
        Some(path) => read_index_at(path)
            .map_err(|e| StopError::Io(format!("cannot read the Run Index: {e}")))?,
        None => Vec::new(),
    };
    let entry = entries
        .iter()
        .rev()
        .find(|e| parse_run_id(&e.run_id).as_deref() == Some(run_id.as_str()))
        .ok_or_else(unknown_run)?;
    let (status, _) = run_status(entry, now, is_alive);
    match status {
        RunStatus::Running => {}
        RunStatus::Missing => {
            return Err(StopError::RunMissing {
                run_id,
                run_dir: entry.run_dir.clone(),
            })
        }
        status => return Err(StopError::NotActive { run_id, status }),
    }
    let run_dir = RunDir::from_path(&entry.run_dir);
    let path = run_dir.control_stop();
    let io_error = |e: io::Error| StopError::Io(format!("cannot write {}: {e}", path.display()));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io_error)?;
    }
    let content = serde_json::json!({
        "v": 1,
        "source": source,
        "requested_at": now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    });
    // create_new: the first request wins, a repeat is a no-op success.
    let already_requested = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            file.write_all(content.to_string().as_bytes())
                .and_then(|()| file.sync_all())
                .map_err(io_error)?;
            false
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => true,
        Err(e) => return Err(io_error(e)),
    };
    Ok(Stopped {
        v: 1,
        ok: true,
        run_id,
        stop_path: path,
        already_requested,
    })
}

/// `pas stop`.
pub fn cmd_stop(run_id: &str, source: AnswerSourceArg, json: bool) -> anyhow::Result<()> {
    let index = attractor_journal::index_path().ok();
    let source = match source {
        AnswerSourceArg::Cli => "cli",
        AnswerSourceArg::Monitor => "monitor",
    };
    match stop(
        index.as_deref(),
        run_id,
        source,
        chrono::Utc::now(),
        pid_alive,
    ) {
        Ok(done) => {
            if json {
                println!("{}", serde_json::to_string(&done)?);
            } else if done.already_requested {
                println!("stop already requested for run {}", done.run_id);
            } else {
                println!(
                    "stop requested for run {}: it ends after its current stage",
                    done.run_id
                );
            }
            Ok(())
        }
        Err(error) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "v": 1,
                        "ok": false,
                        "run_id": run_id,
                        "error": {"code": error.code(), "message": error.to_string()},
                    })
                );
            }
            Err(anyhow::Error::new(RunRefused {
                exit_code: 1,
                message: error.to_string(),
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use attractor_journal::{append_entry_at, IndexEntry, INDEX_FILE};

    const RUN: &str = "0192a000-0000-7000-8000-000000000001";

    fn line(seq: u32, ty: &str, data: &str) -> String {
        format!(
            r#"{{"v":1,"seq":{seq},"ts":"2026-09-24T10:00:00.000Z","run_id":"{RUN}","attempt":1,"type":"{ty}","data":{data}}}"#
        ) + "\n"
    }

    fn started() -> String {
        line(
            1,
            "AttemptStarted",
            r#"{"attempt":1,"pid":1,"argv":[],"pas_version":"0","resumed_from_node":null}"#,
        )
    }

    fn ended(reason: &str) -> String {
        line(
            2,
            "AttemptEnded",
            &format!(r#"{{"attempt":1,"reason":"{reason}"}}"#),
        )
    }

    fn setup(journal: Option<&str>) -> (tempfile::TempDir, PathBuf, RunDir) {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = RunDir::from_path(tmp.path().join("run"));
        if let Some(journal) = journal {
            run_dir.create_all().unwrap();
            std::fs::write(run_dir.events(), journal).unwrap();
        }
        let index = tmp.path().join(INDEX_FILE);
        let entry = IndexEntry::new(
            RUN,
            chrono::Utc::now(),
            "/w",
            "/p.dot",
            run_dir.path().to_path_buf(),
        );
        append_entry_at(&index, &entry).unwrap();
        (tmp, index, run_dir)
    }

    fn go(index: &Path, id: &str) -> Result<Stopped, StopError> {
        stop(Some(index), id, "cli", chrono::Utc::now(), |_| true)
    }

    #[test]
    fn running_run_gets_a_stop_file_with_the_source() {
        let (_tmp, index, run_dir) = setup(Some(&started()));
        let done = stop(Some(&index), RUN, "monitor", chrono::Utc::now(), |_| true).unwrap();
        assert!(!done.already_requested);
        let text = std::fs::read_to_string(run_dir.control_stop()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["source"], "monitor");
        assert!(v["requested_at"].is_string());
    }

    #[test]
    fn second_stop_keeps_the_first_file() {
        let (_tmp, index, run_dir) = setup(Some(&started()));
        go(&index, RUN).unwrap();
        let first = std::fs::read_to_string(run_dir.control_stop()).unwrap();
        let again = stop(Some(&index), RUN, "monitor", chrono::Utc::now(), |_| true).unwrap();
        assert!(again.already_requested);
        assert_eq!(
            std::fs::read_to_string(run_dir.control_stop()).unwrap(),
            first
        );
    }

    #[test]
    fn finished_runs_are_not_active() {
        for reason in ["completed", "failed", "stopped"] {
            let (_tmp, index, run_dir) = setup(Some(&(started() + &ended(reason))));
            let error = go(&index, RUN).unwrap_err();
            assert_eq!(error.code(), "not_active", "{reason}");
            assert!(error.to_string().contains("not active"));
            assert!(!run_dir.control_stop().exists());
        }
    }

    #[test]
    fn crashed_run_is_not_active() {
        let (_tmp, index, run_dir) = setup(Some(&started()));
        // Started long ago, no Heartbeat since, and no process: crashed.
        let error = stop(
            Some(&index),
            RUN,
            "cli",
            chrono::Utc::now() + chrono::Duration::minutes(10),
            |_| false,
        )
        .unwrap_err();
        assert_eq!(error.code(), "not_active");
        assert!(error.to_string().contains("not active"), "{error}");
        assert!(!run_dir.control_stop().exists());
    }

    #[test]
    fn unknown_and_missing_runs() {
        let (_tmp, index, _) = setup(None);
        assert_eq!(go(&index, RUN).unwrap_err().code(), "run_missing");
        let other = "0192a000-0000-7000-8000-0000000000ff";
        let error = go(&index, other).unwrap_err();
        assert_eq!(error.code(), "unknown_run");
        assert!(error.to_string().contains(other));
        assert_eq!(go(&index, "nope").unwrap_err().code(), "unknown_run");
        assert_eq!(
            stop(None, RUN, "cli", chrono::Utc::now(), |_| true)
                .unwrap_err()
                .code(),
            "unknown_run"
        );
    }
}
