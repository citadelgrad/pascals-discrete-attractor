//! Run-folder layout (spec C1) and Run IDs (spec C2).
//!
//! ```text
//! <pipeline folder>/
//!   checkpoint.json
//!   run.lock
//!   runs/<run-id>/
//!     run.json  events.jsonl  console.log
//!     transcripts/<invocation-id>.jsonl
//!     answers/<question-id>.json
//!     control/stop
//! ```

use std::io;
use std::path::{Path, PathBuf};

use uuid::Uuid;

pub const RUNS_DIR: &str = "runs";
pub const RUN_JSON: &str = "run.json";
pub const EVENTS_FILE: &str = "events.jsonl";
pub const CONSOLE_LOG: &str = "console.log";
pub const TRANSCRIPTS_DIR: &str = "transcripts";
pub const ANSWERS_DIR: &str = "answers";
pub const CONTROL_DIR: &str = "control";
pub const STOP_FILE: &str = "stop";
pub const RUN_LOCK: &str = "run.lock";
pub const CHECKPOINT_FILE: &str = "checkpoint.json";

/// Mint a new Run ID: a UUID v7, lowercase and hyphenated.
pub fn new_run_id() -> String {
    Uuid::now_v7().hyphenated().to_string()
}

/// Validate a Run ID and normalize it to lowercase hyphenated form.
///
/// Returns `None` for anything that is not a UUID, so a Run ID taken from the
/// command line or a request can never escape the `runs/` folder.
pub fn parse_run_id(s: &str) -> Option<String> {
    Uuid::try_parse(s).ok().map(|u| u.hyphenated().to_string())
}

/// The stable Pipeline folder (`.pas/logs/<stem>-<hash>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineDir(PathBuf);

impl PipelineDir {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn runs_dir(&self) -> PathBuf {
        self.0.join(RUNS_DIR)
    }

    pub fn run_lock(&self) -> PathBuf {
        self.0.join(RUN_LOCK)
    }

    pub fn checkpoint(&self) -> PathBuf {
        self.0.join(CHECKPOINT_FILE)
    }

    /// The folder of one Run. Fails with `InvalidInput` unless `run_id` is a UUID.
    pub fn run(&self, run_id: &str) -> io::Result<RunDir> {
        let id = parse_run_id(run_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid run id: {run_id:?}"),
            )
        })?;
        Ok(RunDir(self.runs_dir().join(id)))
    }
}

/// The folder of one Run (`<pipeline folder>/runs/<run-id>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunDir(PathBuf);

impl RunDir {
    /// Wrap an existing Run folder path, e.g. a `run_dir` from the Run Index.
    pub fn from_path(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn run_json(&self) -> PathBuf {
        self.0.join(RUN_JSON)
    }

    pub fn events(&self) -> PathBuf {
        self.0.join(EVENTS_FILE)
    }

    pub fn console_log(&self) -> PathBuf {
        self.0.join(CONSOLE_LOG)
    }

    pub fn transcripts_dir(&self) -> PathBuf {
        self.0.join(TRANSCRIPTS_DIR)
    }

    pub fn transcript(&self, invocation_id: &str) -> PathBuf {
        self.transcripts_dir()
            .join(format!("{invocation_id}.jsonl"))
    }

    pub fn answers_dir(&self) -> PathBuf {
        self.0.join(ANSWERS_DIR)
    }

    pub fn answer(&self, question_id: &str) -> PathBuf {
        self.answers_dir().join(format!("{question_id}.json"))
    }

    pub fn control_stop(&self) -> PathBuf {
        self.0.join(CONTROL_DIR).join(STOP_FILE)
    }

    /// Create the Run folder and its `transcripts`, `answers`, and `control` sub-folders.
    pub fn create_all(&self) -> io::Result<()> {
        for dir in [
            self.0.clone(),
            self.transcripts_dir(),
            self.answers_dir(),
            self.0.join(CONTROL_DIR),
        ] {
            std::fs::create_dir_all(dir)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_run_id_is_lowercase_v7() {
        let id = new_run_id();
        assert_eq!(id, id.to_lowercase());
        assert_eq!(id.len(), 36);
        let u = Uuid::parse_str(&id).unwrap();
        assert_eq!(u.get_version_num(), 7);
        assert_eq!(parse_run_id(&id).as_deref(), Some(id.as_str()));
    }

    #[test]
    fn parse_run_id_rejects_non_uuids_and_normalizes_case() {
        assert_eq!(parse_run_id("../x"), None);
        assert_eq!(parse_run_id(""), None);
        assert_eq!(parse_run_id("0192...."), None);
        assert_eq!(
            parse_run_id("0192A3B4-C5D6-7E8F-9A0B-1C2D3E4F5A6B").as_deref(),
            Some("0192a3b4-c5d6-7e8f-9a0b-1c2d3e4f5a6b")
        );
    }

    #[test]
    fn paths_match_c1_layout() {
        let p = PipelineDir::new("/r/.pas/logs/x-1a2b3c4d");
        assert_eq!(
            p.checkpoint(),
            Path::new("/r/.pas/logs/x-1a2b3c4d/checkpoint.json")
        );
        assert_eq!(p.run_lock(), Path::new("/r/.pas/logs/x-1a2b3c4d/run.lock"));
        let id = "0192a3b4-c5d6-7e8f-9a0b-1c2d3e4f5a6b";
        let r = p.run(id).unwrap();
        let root = format!("/r/.pas/logs/x-1a2b3c4d/runs/{id}");
        assert_eq!(r.path(), Path::new(&root));
        assert_eq!(r.run_json(), Path::new(&format!("{root}/run.json")));
        assert_eq!(r.events(), Path::new(&format!("{root}/events.jsonl")));
        assert_eq!(r.console_log(), Path::new(&format!("{root}/console.log")));
        assert_eq!(
            r.transcript("inv1"),
            Path::new(&format!("{root}/transcripts/inv1.jsonl"))
        );
        assert_eq!(
            r.answer("q1"),
            Path::new(&format!("{root}/answers/q1.json"))
        );
        assert_eq!(r.control_stop(), Path::new(&format!("{root}/control/stop")));
        assert_eq!(
            p.run("../../etc").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn create_all_makes_sub_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let r = PipelineDir::new(tmp.path()).run(&new_run_id()).unwrap();
        r.create_all().unwrap();
        assert!(r.transcripts_dir().is_dir());
        assert!(r.answers_dir().is_dir());
        assert!(r.control_stop().parent().unwrap().is_dir());
        r.create_all().unwrap();
    }
}
