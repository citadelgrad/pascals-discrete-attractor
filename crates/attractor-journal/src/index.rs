//! The machine-wide Run Index, `runs.jsonl` (spec C4).
//!
//! One line per Run, appended when the Run starts, under an exclusive
//! advisory lock on the file itself. The Index never stores a status; status
//! is derived from the Run Journal. Advisory locks are unreliable on network
//! file systems, so the state folder is expected to be local.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::INDEX_VERSION;

pub const INDEX_FILE: &str = "runs.jsonl";

/// One Run Index line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexEntry {
    pub v: u32,
    pub run_id: String,
    pub started_at: DateTime<Utc>,
    pub workdir: PathBuf,
    pub pipeline_path: PathBuf,
    pub run_dir: PathBuf,
}

impl IndexEntry {
    pub fn new(
        run_id: impl Into<String>,
        started_at: DateTime<Utc>,
        workdir: impl Into<PathBuf>,
        pipeline_path: impl Into<PathBuf>,
        run_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            v: INDEX_VERSION,
            run_id: run_id.into(),
            started_at,
            workdir: workdir.into(),
            pipeline_path: pipeline_path.into(),
            run_dir: run_dir.into(),
        }
    }

    /// True when the Run folder no longer exists; such a Run is "missing".
    pub fn is_missing(&self) -> bool {
        !self.run_dir.exists()
    }
}

/// Resolve the PAS state folder from the values of `PAS_STATE_DIR`,
/// `XDG_STATE_HOME`, and `HOME`, in that order of precedence:
/// `$PAS_STATE_DIR`, else `$XDG_STATE_HOME/pas`, else `$HOME/.local/state/pas`.
/// Empty values count as unset.
pub fn resolve_state_dir(
    pas_state_dir: Option<OsString>,
    xdg_state_home: Option<OsString>,
    home: Option<OsString>,
) -> Option<PathBuf> {
    let set = |v: Option<OsString>| v.filter(|v| !v.is_empty()).map(PathBuf::from);
    set(pas_state_dir)
        .or_else(|| set(xdg_state_home).map(|p| p.join("pas")))
        .or_else(|| set(home).map(|p| p.join(".local").join("state").join("pas")))
}

/// The PAS state folder for this process's environment.
pub fn state_dir() -> io::Result<PathBuf> {
    resolve_state_dir(
        std::env::var_os("PAS_STATE_DIR"),
        std::env::var_os("XDG_STATE_HOME"),
        std::env::var_os("HOME"),
    )
    .ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "cannot locate the PAS state folder: set PAS_STATE_DIR, XDG_STATE_HOME, or HOME",
        )
    })
}

/// Path of the Run Index for this process's environment.
pub fn index_path() -> io::Result<PathBuf> {
    Ok(state_dir()?.join(INDEX_FILE))
}

/// Append an entry to the Run Index at [`index_path`].
pub fn append_entry(entry: &IndexEntry) -> io::Result<()> {
    append_entry_at(&index_path()?, entry)
}

/// Append an entry to the Run Index at `path`, as one write under an
/// exclusive lock.
pub fn append_entry_at(path: &Path, entry: &IndexEntry) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut line = serde_json::to_vec(entry).map_err(io::Error::other)?;
    line.push(b'\n');
    let mut file = OpenOptions::new().append(true).create(true).open(path)?;
    file.lock()?;
    let written = file.write_all(&line).and_then(|()| file.sync_data());
    file.unlock()?;
    written
}

/// Read the Run Index at [`index_path`].
pub fn read_index() -> io::Result<Vec<IndexEntry>> {
    read_index_at(&index_path()?)
}

/// Read the Run Index at `path`, in start order. A missing file is an empty
/// Index; lines that do not parse are skipped.
pub fn read_index_at(path: &Path) -> io::Result<Vec<IndexEntry>> {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    file.lock_shared()?;
    let mut bytes = Vec::new();
    let read = file.read_to_end(&mut bytes);
    file.unlock()?;
    read?;
    Ok(bytes
        .split_inclusive(|&b| b == b'\n')
        .filter(|l| l.ends_with(b"\n"))
        .filter_map(|l| serde_json::from_slice(l).ok())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(s: &str) -> Option<OsString> {
        Some(OsString::from(s))
    }

    /// AC: Index path precedence.
    #[test]
    fn state_dir_precedence() {
        assert_eq!(
            resolve_state_dir(os("/pas"), os("/xdg"), os("/home/u")),
            Some(PathBuf::from("/pas"))
        );
        assert_eq!(
            resolve_state_dir(None, os("/xdg"), os("/home/u")),
            Some(PathBuf::from("/xdg/pas"))
        );
        assert_eq!(
            resolve_state_dir(None, None, os("/home/u")),
            Some(PathBuf::from("/home/u/.local/state/pas"))
        );
        assert_eq!(
            resolve_state_dir(os(""), os(""), os("/home/u")),
            Some(PathBuf::from("/home/u/.local/state/pas"))
        );
        assert_eq!(
            resolve_state_dir(os(""), os("/xdg"), None),
            Some(PathBuf::from("/xdg/pas"))
        );
        assert_eq!(resolve_state_dir(None, None, None), None);
        assert_eq!(resolve_state_dir(os(""), os(""), os("")), None);
    }

    #[test]
    fn index_path_is_runs_jsonl_in_state_dir() {
        // Reads the real environment; only the file name is asserted.
        if let Ok(p) = index_path() {
            assert_eq!(p.file_name().unwrap(), INDEX_FILE);
            assert_eq!(p.parent().unwrap(), state_dir().unwrap());
        }
    }

    #[test]
    fn append_and_read_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join(INDEX_FILE);
        let run_dir = tmp.path().join("run");
        std::fs::create_dir(&run_dir).unwrap();
        let a = IndexEntry::new("a", Utc::now(), "/w", "/p.dot", &run_dir);
        let b = IndexEntry::new("b", Utc::now(), "/w", "/p.dot", tmp.path().join("gone"));
        append_entry_at(&path, &a).unwrap();
        append_entry_at(&path, &b).unwrap();
        assert_eq!(read_index_at(&path).unwrap(), vec![a.clone(), b.clone()]);
        assert!(!a.is_missing());
        assert!(b.is_missing());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("{\"v\":1,\"run_id\":\"a\",\"started_at\":"),
            "{text}"
        );
        assert!(!text.contains("status"));
    }

    #[test]
    fn read_missing_index_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(read_index_at(&tmp.path().join(INDEX_FILE))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn read_skips_bad_and_torn_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(INDEX_FILE);
        let good = IndexEntry::new("a", Utc::now(), "/w", "/p.dot", "/r");
        let mut text = serde_json::to_string(&good).unwrap();
        text.push_str("\nnot json\n");
        text.push_str(&serde_json::to_string(&good).unwrap()[..20]);
        std::fs::write(&path, text).unwrap();
        assert_eq!(read_index_at(&path).unwrap(), vec![good]);
    }

    #[test]
    fn spec_example_entry_parses() {
        let line = r#"{"v":1,"run_id":"0192...","started_at":"2026-09-24T10:00:00Z","workdir":"/abs/repo","pipeline_path":"/abs/pipelines/x.dot","run_dir":"/abs/repo/.pas/logs/x-1a2b3c4d/runs/0192..."}"#;
        let e: IndexEntry = serde_json::from_str(line).unwrap();
        assert_eq!(e.workdir, PathBuf::from("/abs/repo"));
    }
}
