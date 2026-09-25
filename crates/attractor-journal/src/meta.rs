//! `run.json`: metadata written once when a Run starts (spec C1).
//!
//! It is never rewritten when the Run is resumed; `argv` is kept so Resume
//! can re-issue the same command.

use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::layout::RUN_JSON;
use crate::RUN_META_VERSION;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunMeta {
    pub v: u32,
    pub run_id: String,
    pub pipeline_path: PathBuf,
    pub pipeline_name: String,
    pub workdir: PathBuf,
    /// `null` when the workdir is not inside a git worktree.
    pub git_worktree: Option<PathBuf>,
    pub logs_dir: PathBuf,
    pub started_at: DateTime<Utc>,
    pub argv: Vec<String>,
    pub pas_version: String,
    pub epic_id: Option<String>,
}

impl RunMeta {
    /// The current `run.json` version.
    pub const VERSION: u32 = RUN_META_VERSION;
}

/// Write `<run_dir>/run.json` atomically (temp file + rename).
pub fn write_run_meta(run_dir: &Path, meta: &RunMeta) -> io::Result<()> {
    std::fs::create_dir_all(run_dir)?;
    let tmp = run_dir.join(format!("{RUN_JSON}.tmp"));
    let mut bytes = serde_json::to_vec_pretty(meta).map_err(io::Error::other)?;
    bytes.push(b'\n');
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, run_dir.join(RUN_JSON))
}

/// Read `<run_dir>/run.json`.
pub fn read_run_meta(run_dir: &Path) -> io::Result<RunMeta> {
    let bytes = std::fs::read(run_dir.join(RUN_JSON))?;
    serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"{ "v": 1, "run_id": "0192...", "pipeline_path": "/abs/pipelines/x.dot", "pipeline_name": "X",
  "workdir": "/abs/repo", "git_worktree": "/abs/repo", "logs_dir": "/abs/repo/.pas/logs/x-1a2b3c4d",
  "started_at": "2026-09-24T10:00:00Z", "argv": ["pas","run","pipelines/x.dot","--max-budget-usd","50"],
  "pas_version": "0.11.0", "epic_id": null }"#;

    #[test]
    fn spec_example_parses() {
        let m: RunMeta = serde_json::from_str(EXAMPLE).unwrap();
        assert_eq!(m.v, RunMeta::VERSION);
        assert_eq!(m.argv.len(), 5);
        assert_eq!(m.epic_id, None);
    }

    #[test]
    fn write_then_read_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m: RunMeta = serde_json::from_str(EXAMPLE).unwrap();
        m.git_worktree = None;
        write_run_meta(tmp.path(), &m).unwrap();
        assert_eq!(read_run_meta(tmp.path()).unwrap(), m);
        assert!(!tmp.path().join("run.json.tmp").exists());
        let text = std::fs::read_to_string(tmp.path().join(RUN_JSON)).unwrap();
        assert!(text.contains("\"git_worktree\": null"), "{text}");
        assert!(text.contains("\"epic_id\": null"), "{text}");
    }

    #[test]
    fn read_missing_or_corrupt() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            read_run_meta(tmp.path()).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        std::fs::write(tmp.path().join(RUN_JSON), "{").unwrap();
        assert_eq!(
            read_run_meta(tmp.path()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
