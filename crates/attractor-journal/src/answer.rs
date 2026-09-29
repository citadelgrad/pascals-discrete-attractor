//! Human Gate answer files: `runs/<run-id>/answers/<question-id>.json` (spec C1).
//!
//! Whoever answers a Human Gate — the terminal, `pas answer`, or the Monitor
//! (through `pas answer`) — writes one of these files. Only the first writer
//! succeeds, so the file alone decides which answer wins. A complete file the
//! waiting Run cannot use is renamed to `<question-id>.json.rejected`.
//!
//! ```json
//! {"v":1,"question_id":"q-review-1","choice":"approve","source":"cli","answered_at":"2026-09-24T10:00:00Z"}
//! ```

use std::io;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::event::AnswerSource;
use crate::layout::RunDir;

/// Version of an answer file (`v`).
pub const ANSWER_VERSION: u32 = 1;
/// Longest accepted question ID.
pub const MAX_QUESTION_ID_LEN: usize = 128;

/// Contents of `answers/<question-id>.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnswerFile {
    pub v: u32,
    pub question_id: String,
    /// One of the `choices` of the matching `HumanInputRequested`.
    pub choice: String,
    pub source: AnswerSource,
    pub answered_at: DateTime<Utc>,
}

impl AnswerFile {
    /// An answer written now.
    pub fn new(question_id: &str, choice: &str, source: AnswerSource) -> Self {
        Self {
            v: ANSWER_VERSION,
            question_id: question_id.to_string(),
            choice: choice.to_string(),
            source,
            answered_at: Utc::now(),
        }
    }
}

/// True when `id` is 1..=128 characters of `[A-Za-z0-9_-]`, so it names a
/// file inside `answers/` and nothing else.
pub fn is_valid_question_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_QUESTION_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The `n`th question ID for the Human Gate `node_id`: `q-<node>-<n>`, with
/// characters outside `[A-Za-z0-9_-]` replaced by `_`. Always valid.
pub fn question_id_for(node_id: &str, n: u32) -> String {
    let suffix = format!("-{n}");
    let room = MAX_QUESTION_ID_LEN - "q-".len() - suffix.len();
    let slug: String = node_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(room)
        .collect();
    format!("q-{slug}{suffix}")
}

fn checked_path(run_dir: &RunDir, question_id: &str) -> io::Result<PathBuf> {
    if !is_valid_question_id(question_id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid question id: {question_id:?}"),
        ));
    }
    Ok(run_dir.answer(question_id))
}

/// Create `answers/<question-id>.json` unless it already exists.
///
/// `Ok(true)`: this call wrote the answer. `Ok(false)`: an answer file was
/// already there and is left untouched. The file is written to a temp file
/// and hard-linked into place, which fails like `O_CREAT|O_EXCL` when the
/// name exists and never exposes a half-written file.
pub fn write_answer(run_dir: &RunDir, answer: &AnswerFile) -> io::Result<bool> {
    let path = checked_path(run_dir, &answer.question_id)?;
    std::fs::create_dir_all(run_dir.answers_dir())?;
    // Unique per call, so two writers in one process never share a temp file.
    static NEXT_TMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = NEXT_TMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = run_dir.answers_dir().join(format!(
        ".{}.{}.{nanos}.tmp",
        answer.question_id,
        std::process::id()
    ));
    let mut bytes = serde_json::to_vec(answer).map_err(io::Error::other)?;
    bytes.push(b'\n');
    let linked = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        io::Write::write_all(&mut file, &bytes)?;
        file.sync_all()?;
        match std::fs::hard_link(&tmp, &path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(e),
        }
    })();
    let _ = std::fs::remove_file(&tmp);
    linked
}

/// Read `answers/<question-id>.json`.
///
/// `Ok(None)`: no file yet, or a file that ends before its JSON does (still
/// being written by a tool that does not use [`write_answer`]).
/// `Err(InvalidData)`: a complete file that is not an answer.
pub fn read_answer(run_dir: &RunDir, question_id: &str) -> io::Result<Option<AnswerFile>> {
    let path = checked_path(run_dir, question_id)?;
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    match serde_json::from_slice::<AnswerFile>(&bytes) {
        Ok(answer) => Ok(Some(answer)),
        Err(e) if e.is_eof() => Ok(None),
        Err(e) => Err(io::Error::new(io::ErrorKind::InvalidData, e)),
    }
}

/// Move an unusable answer file aside to `<question-id>.json.rejected`, so a
/// valid answer can still be written. Replaces an older rejected file.
pub fn reject_answer(run_dir: &RunDir, question_id: &str) -> io::Result<PathBuf> {
    let path = checked_path(run_dir, question_id)?;
    let rejected = path.with_extension("json.rejected");
    std::fs::rename(&path, &rejected)?;
    Ok(rejected)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_dir() -> (tempfile::TempDir, RunDir) {
        let tmp = tempfile::tempdir().unwrap();
        let run = RunDir::from_path(tmp.path().join("run"));
        run.create_all().unwrap();
        (tmp, run)
    }

    #[test]
    fn question_ids_are_checked() {
        for ok in ["q-review-1", "a", "Q_1-x", &"x".repeat(128)] {
            assert!(is_valid_question_id(ok), "{ok}");
        }
        for bad in [
            "",
            "../x",
            "a/b",
            "a\\b",
            ".",
            "..",
            "q 1",
            "q.json",
            "é",
            &"x".repeat(129),
        ] {
            assert!(!is_valid_question_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn question_id_for_matches_golden_form_and_is_always_valid() {
        assert_eq!(question_id_for("review", 1), "q-review-1");
        assert_eq!(question_id_for("gate", 12), "q-gate-12");
        assert_eq!(question_id_for("a.b/../c d", 2), "q-a_b____c_d-2");
        let long = question_id_for(&"n".repeat(500), u32::MAX);
        assert!(is_valid_question_id(&long), "{long}");
        assert!(long.ends_with(&format!("-{}", u32::MAX)));
        assert!(is_valid_question_id(&question_id_for("", 1)));
    }

    #[test]
    fn first_writer_wins_and_the_file_round_trips() {
        let (_tmp, run) = run_dir();
        let first = AnswerFile::new("q-gate-1", "approve", AnswerSource::Cli);
        assert!(write_answer(&run, &first).unwrap());
        let second = AnswerFile::new("q-gate-1", "reject", AnswerSource::Monitor);
        assert!(!write_answer(&run, &second).unwrap());

        assert_eq!(read_answer(&run, "q-gate-1").unwrap(), Some(first));
        let names: Vec<String> = std::fs::read_dir(run.answers_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["q-gate-1.json".to_string()], "no temp files");
    }

    #[test]
    fn file_format_is_the_documented_one() {
        let (_tmp, run) = run_dir();
        let text = r#"{"v":1,"question_id":"q-review-1","choice":"approve","source":"cli","answered_at":"2026-09-24T10:00:00Z"}"#;
        std::fs::write(run.answer("q-review-1"), text).unwrap();
        let answer = read_answer(&run, "q-review-1").unwrap().unwrap();
        assert_eq!(answer.v, ANSWER_VERSION);
        assert_eq!(answer.choice, "approve");
        assert_eq!(answer.source, AnswerSource::Cli);
        assert_eq!(
            serde_json::to_value(&answer).unwrap(),
            serde_json::from_str::<serde_json::Value>(text).unwrap()
        );
    }

    #[test]
    fn missing_torn_and_invalid_files() {
        let (_tmp, run) = run_dir();
        assert_eq!(read_answer(&run, "q-gate-1").unwrap(), None);

        std::fs::write(run.answer("q-gate-1"), r#"{"v":1,"question_id":"q-ga"#).unwrap();
        assert_eq!(read_answer(&run, "q-gate-1").unwrap(), None, "torn");

        std::fs::write(run.answer("q-gate-1"), "not json\n").unwrap();
        let err = read_answer(&run, "q-gate-1").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        std::fs::write(run.answer("q-gate-1"), r#"{"v":1,"choice":"x"}"#).unwrap();
        let err = read_answer(&run, "q-gate-1").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "missing fields");
    }

    #[test]
    fn rejecting_frees_the_name_for_a_valid_answer() {
        let (_tmp, run) = run_dir();
        std::fs::write(run.answer("q-gate-1"), "garbage").unwrap();
        let rejected = reject_answer(&run, "q-gate-1").unwrap();
        assert_eq!(rejected, run.answers_dir().join("q-gate-1.json.rejected"));
        assert!(rejected.exists());
        assert!(!run.answer("q-gate-1").exists());

        let answer = AnswerFile::new("q-gate-1", "approve", AnswerSource::Monitor);
        assert!(write_answer(&run, &answer).unwrap());
        assert_eq!(read_answer(&run, "q-gate-1").unwrap(), Some(answer));
    }

    #[test]
    fn invalid_question_ids_never_touch_the_file_system() {
        let (tmp, run) = run_dir();
        let answer = AnswerFile::new("../../escape", "x", AnswerSource::Cli);
        let err = write_answer(&run, &answer).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(!tmp.path().join("escape.json").exists());
        assert_eq!(
            read_answer(&run, "a/b").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            reject_answer(&run, "..").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn write_creates_a_missing_answers_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let run = RunDir::from_path(tmp.path().join("run"));
        let answer = AnswerFile::new("q-gate-1", "approve", AnswerSource::Cli);
        assert!(write_answer(&run, &answer).unwrap());
        assert!(run.answer("q-gate-1").exists());
    }
}
