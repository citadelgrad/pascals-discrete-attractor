//! `pas answer <run-id> <question-id> <choice> [--source cli|monitor] [--json]`:
//! answer a waiting Human Gate by creating its answer file (spec File
//! Change 12, C1, C3, C6).
//!
//! The Run finds the file and journals `HumanInputAnswered` itself; this
//! command never writes the journal and takes no Run lock, so it can never
//! make a starting `pas run` exit 5 or 6.

use std::io;
use std::path::{Path, PathBuf};

use attractor_journal::{
    is_valid_question_id, parse_run_id, read_all, read_index_at, write_answer, AnswerFile,
    AnswerSource, EventData, RunDir,
};
use serde::Serialize;

use super::run::RunRefused;

/// Exit code of `pas answer` when the question already has an answer.
pub const EXIT_ALREADY_ANSWERED: i32 = 7;

/// Where an answer says it came from; `terminal` belongs to `pas run` itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum AnswerSourceArg {
    Cli,
    Monitor,
}

impl From<AnswerSourceArg> for AnswerSource {
    fn from(source: AnswerSourceArg) -> Self {
        match source {
            AnswerSourceArg::Cli => AnswerSource::Cli,
            AnswerSourceArg::Monitor => AnswerSource::Monitor,
        }
    }
}

#[derive(Debug)]
enum AnswerError {
    UnknownRun(String),
    RunMissing {
        run_id: String,
        run_dir: PathBuf,
    },
    UnknownQuestion {
        run_id: String,
        question_id: String,
    },
    InvalidChoice {
        question_id: String,
        choice: String,
        choices: Vec<String>,
    },
    AlreadyAnswered {
        question_id: String,
    },
    Io(String),
}

impl AnswerError {
    /// Stable `error.code` of the `--json` failure object.
    fn code(&self) -> &'static str {
        match self {
            Self::UnknownRun(_) => "unknown_run",
            Self::RunMissing { .. } => "run_missing",
            Self::UnknownQuestion { .. } => "unknown_question",
            Self::InvalidChoice { .. } => "invalid_choice",
            Self::AlreadyAnswered { .. } => "already_answered",
            Self::Io(_) => "io_error",
        }
    }

    fn exit_code(&self) -> i32 {
        match self {
            Self::AlreadyAnswered { .. } => EXIT_ALREADY_ANSWERED,
            _ => 1,
        }
    }
}

impl std::fmt::Display for AnswerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownRun(id) => write!(f, "unknown run-id {id}: not in the Run Index"),
            Self::RunMissing { run_id, run_dir } => write!(
                f,
                "run {run_id} is missing: its folder {} no longer exists",
                run_dir.display()
            ),
            Self::UnknownQuestion {
                run_id,
                question_id,
            } => write!(
                f,
                "unknown question-id {question_id}: run {run_id} has not asked it"
            ),
            Self::InvalidChoice {
                question_id,
                choice,
                choices,
            } => write!(
                f,
                "choice {choice:?} is not offered by question {question_id}; choices: {}",
                choices.join(", ")
            ),
            Self::AlreadyAnswered { question_id } => {
                write!(f, "question {question_id} is already answered")
            }
            Self::Io(message) => f.write_str(message),
        }
    }
}

/// A recorded answer.
#[derive(Debug, Serialize)]
struct Answered {
    v: u32,
    ok: bool,
    run_id: String,
    question_id: String,
    choice: String,
    source: AnswerSource,
    answer_path: PathBuf,
}

/// Answer `question_id` of Run `run_id` found through the Index at `index`
/// (`None`: no state folder, so an empty Index).
fn answer(
    index: Option<&Path>,
    run_id: &str,
    question_id: &str,
    choice: &str,
    source: AnswerSource,
) -> Result<Answered, AnswerError> {
    let unknown_run = || AnswerError::UnknownRun(run_id.to_string());
    let run_id = parse_run_id(run_id).ok_or_else(unknown_run)?;
    let entries = match index {
        Some(path) => read_index_at(path)
            .map_err(|e| AnswerError::Io(format!("cannot read the Run Index: {e}")))?,
        None => Vec::new(),
    };
    let entry = entries
        .iter()
        .rev()
        .find(|e| parse_run_id(&e.run_id).as_deref() == Some(run_id.as_str()))
        .ok_or_else(unknown_run)?;
    if entry.is_missing() {
        return Err(AnswerError::RunMissing {
            run_id,
            run_dir: entry.run_dir.clone(),
        });
    }
    let unknown_question = || AnswerError::UnknownQuestion {
        run_id: run_id.clone(),
        question_id: question_id.to_string(),
    };
    if !is_valid_question_id(question_id) {
        return Err(unknown_question());
    }
    let run_dir = RunDir::from_path(&entry.run_dir);
    let events = match read_all(run_dir.events()) {
        Ok(events) => events,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(AnswerError::Io(format!("cannot read the Run Journal: {e}"))),
    };
    let mut choices = None;
    let mut answered = false;
    for event in &events {
        match &event.data {
            EventData::HumanInputRequested {
                question_id: q,
                choices: c,
                ..
            } if q == question_id => {
                choices = Some(c.clone());
            }
            EventData::HumanInputAnswered { question_id: q, .. } if q == question_id => {
                answered = true;
            }
            _ => {}
        }
    }
    let choices = choices.ok_or_else(unknown_question)?;
    if !choices.iter().any(|c| c == choice) {
        return Err(AnswerError::InvalidChoice {
            question_id: question_id.to_string(),
            choice: choice.to_string(),
            choices,
        });
    }
    let already = || AnswerError::AlreadyAnswered {
        question_id: question_id.to_string(),
    };
    if answered {
        return Err(already());
    }
    let file = AnswerFile::new(question_id, choice, source);
    match write_answer(&run_dir, &file) {
        Ok(true) => Ok(Answered {
            v: 1,
            ok: true,
            run_id,
            question_id: question_id.to_string(),
            choice: choice.to_string(),
            source,
            answer_path: run_dir.answer(question_id),
        }),
        Ok(false) => Err(already()),
        Err(e) => Err(AnswerError::Io(format!(
            "cannot write the answer file: {e}"
        ))),
    }
}

/// `pas answer`.
pub fn cmd_answer(
    run_id: &str,
    question_id: &str,
    choice: &str,
    source: AnswerSourceArg,
    json: bool,
) -> anyhow::Result<()> {
    let index = attractor_journal::index_path().ok();
    match answer(index.as_deref(), run_id, question_id, choice, source.into()) {
        Ok(done) => {
            if json {
                println!("{}", serde_json::to_string(&done)?);
            } else {
                println!(
                    "answered {} of run {} with {:?} (source {})",
                    done.question_id,
                    done.run_id,
                    done.choice,
                    serde_json::to_value(done.source)?.as_str().unwrap_or("")
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
                exit_code: error.exit_code(),
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

    fn asked() -> String {
        line(
            1,
            "HumanInputRequested",
            r#"{"question_id":"q-gate-1","node_id":"gate","text":"Ship?","choices":["approve","reject"]}"#,
        )
    }

    fn answered() -> String {
        line(
            2,
            "HumanInputAnswered",
            r#"{"question_id":"q-gate-1","choice":"approve","source":"terminal"}"#,
        )
    }

    /// An Index with the Run `RUN` whose journal is `journal`.
    fn setup(journal: Option<&str>) -> (tempfile::TempDir, PathBuf, RunDir) {
        let tmp = tempfile::tempdir().unwrap();
        let run = RunDir::from_path(tmp.path().join("run"));
        if let Some(journal) = journal {
            run.create_all().unwrap();
            std::fs::write(run.events(), journal).unwrap();
        }
        let index = tmp.path().join(INDEX_FILE);
        let entry = IndexEntry::new(
            RUN,
            chrono::Utc::now(),
            "/w",
            "/p.dot",
            run.path().to_path_buf(),
        );
        append_entry_at(&index, &entry).unwrap();
        (tmp, index, run)
    }

    fn go(index: &Path, run: &str, q: &str, choice: &str) -> Result<Answered, AnswerError> {
        answer(Some(index), run, q, choice, AnswerSource::Cli)
    }

    #[test]
    fn writes_the_answer_file_with_the_source() {
        let (_tmp, index, run) = setup(Some(&asked()));
        let done = answer(
            Some(&index),
            RUN,
            "q-gate-1",
            "reject",
            AnswerSource::Monitor,
        )
        .unwrap();
        assert_eq!(done.run_id, RUN);
        assert_eq!(done.answer_path, run.answer("q-gate-1"));
        let file = attractor_journal::read_answer(&run, "q-gate-1")
            .unwrap()
            .unwrap();
        assert_eq!(file.choice, "reject");
        assert_eq!(file.source, AnswerSource::Monitor);
    }

    #[test]
    fn a_second_answer_is_already_answered_and_leaves_the_file() {
        let (_tmp, index, run) = setup(Some(&asked()));
        go(&index, RUN, "q-gate-1", "approve").unwrap();
        let before = std::fs::read(run.answer("q-gate-1")).unwrap();
        let err = go(&index, RUN, "q-gate-1", "reject").unwrap_err();
        assert_eq!(err.code(), "already_answered");
        assert_eq!(err.exit_code(), EXIT_ALREADY_ANSWERED);
        assert_eq!(std::fs::read(run.answer("q-gate-1")).unwrap(), before);
    }

    #[test]
    fn an_answer_in_the_journal_creates_no_file() {
        let (_tmp, index, run) = setup(Some(&(asked() + &answered())));
        let err = go(&index, RUN, "q-gate-1", "reject").unwrap_err();
        assert_eq!(err.exit_code(), EXIT_ALREADY_ANSWERED);
        assert!(!run.answers_dir().exists() || !run.answer("q-gate-1").exists());
    }

    #[test]
    fn a_choice_not_offered_creates_no_file() {
        let (_tmp, index, run) = setup(Some(&asked()));
        for bad in ["maybe", "1", "Approve", ""] {
            let err = go(&index, RUN, "q-gate-1", bad).unwrap_err();
            assert_eq!(err.code(), "invalid_choice", "{bad:?}");
            assert_eq!(err.exit_code(), 1);
            assert!(err.to_string().contains("approve, reject"));
        }
        assert!(!run.answer("q-gate-1").exists());
    }

    #[test]
    fn unknown_ids_are_named() {
        let (_tmp, index, run) = setup(Some(&asked()));
        let other = "0192a000-0000-7000-8000-0000000000ff";
        let err = go(&index, other, "q-gate-1", "approve").unwrap_err();
        assert_eq!(err.code(), "unknown_run");
        assert!(err.to_string().contains(other));
        let err = go(&index, "not-a-uuid", "q-gate-1", "approve").unwrap_err();
        assert_eq!(err.code(), "unknown_run");
        assert!(err.to_string().contains("not-a-uuid"));
        let err = go(&index, RUN, "q-nope-9", "approve").unwrap_err();
        assert_eq!(err.code(), "unknown_question");
        assert!(err.to_string().contains("q-nope-9"));
        // Hostile IDs never become paths.
        let err = go(&index, RUN, "../x", "approve").unwrap_err();
        assert_eq!(err.code(), "unknown_question");
        assert_eq!(std::fs::read_dir(run.answers_dir()).unwrap().count(), 0);
    }

    #[test]
    fn no_index_and_missing_run_folder() {
        let err = answer(None, RUN, "q", "a", AnswerSource::Cli).unwrap_err();
        assert_eq!(err.code(), "unknown_run");
        let (_tmp, index, _run) = setup(None);
        let err = go(&index, RUN, "q-gate-1", "approve").unwrap_err();
        assert_eq!(err.code(), "run_missing");
        assert!(err.to_string().contains(RUN));
    }

    #[test]
    fn journal_without_a_request_is_an_unknown_question() {
        let (_tmp, index, _run) = setup(Some(""));
        let err = go(&index, RUN, "q-gate-1", "approve").unwrap_err();
        assert_eq!(err.code(), "unknown_question");
    }

    #[test]
    fn two_writers_race_and_exactly_one_wins() {
        let (_tmp, index, run) = setup(Some(&asked()));
        let results: Vec<_> = std::thread::scope(|s| {
            let handles: Vec<_> = ["approve", "reject"]
                .into_iter()
                .map(|c| {
                    let index = &index;
                    s.spawn(move || go(index, RUN, "q-gate-1", c))
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        let loser = results.iter().find_map(|r| r.as_ref().err()).unwrap();
        assert_eq!(loser.exit_code(), EXIT_ALREADY_ANSWERED);
        let winner = results.iter().find_map(|r| r.as_ref().ok()).unwrap();
        let file = attractor_journal::read_answer(&run, "q-gate-1")
            .unwrap()
            .unwrap();
        assert_eq!(file.choice, winner.choice);
    }
}
