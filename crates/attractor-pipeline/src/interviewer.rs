//! Interviewer trait and built-in implementations for human interaction.

use std::io::IsTerminal;
use std::time::Duration;

use async_trait::async_trait;
use attractor_journal::{AnswerFile, AnswerSource, RunDir};
use attractor_types::Result;
use tokio::sync::{mpsc, Mutex};

#[derive(Debug, Clone)]
pub struct Question {
    /// Names the answer file (`answers/<question_id>.json`); always valid
    /// for [`attractor_journal::is_valid_question_id`].
    pub question_id: String,
    pub prompt: String,
    pub choices: Vec<String>,
    pub default: Option<String>,
    pub timeout: Option<std::time::Duration>,
}

#[derive(Debug, Clone)]
pub struct Answer {
    pub choice: String,
    pub custom_text: Option<String>,
    /// Where a person's answer came from; `None` for answers no person gave
    /// (auto-approve, recorded test answers). Only sourced answers are
    /// journaled as `HumanInputAnswered`.
    pub source: Option<AnswerSource>,
}

#[async_trait]
pub trait Interviewer: Send + Sync {
    async fn ask(&self, question: &Question) -> Result<Answer>;
}

// ---------------------------------------------------------------------------
// AutoApproveInterviewer
// ---------------------------------------------------------------------------

pub struct AutoApproveInterviewer;

#[async_trait]
impl Interviewer for AutoApproveInterviewer {
    async fn ask(&self, question: &Question) -> Result<Answer> {
        let choice = question
            .default
            .clone()
            .or_else(|| question.choices.first().cloned())
            .unwrap_or_default();
        Ok(Answer {
            choice,
            custom_text: None,
            source: None,
        })
    }
}

// ---------------------------------------------------------------------------
// ConsoleInterviewer
// ---------------------------------------------------------------------------

pub struct ConsoleInterviewer;

#[async_trait]
impl Interviewer for ConsoleInterviewer {
    async fn ask(&self, question: &Question) -> Result<Answer> {
        println!("\n{}", question.prompt);
        for (i, choice) in question.choices.iter().enumerate() {
            println!("  [{}] {}", i + 1, choice);
        }
        let input = tokio::task::spawn_blocking(|| {
            let mut buf = String::new();
            std::io::stdin().read_line(&mut buf).map(|_| buf)
        })
        .await
        .map_err(|e| attractor_types::AttractorError::Other(format!("stdin task failed: {}", e)))?
        .map_err(attractor_types::AttractorError::Io)?;
        let trimmed = input.trim();
        if let Ok(idx) = trimmed.parse::<usize>() {
            if idx > 0 && idx <= question.choices.len() {
                return Ok(Answer {
                    choice: question.choices[idx - 1].clone(),
                    custom_text: None,
                    source: Some(AnswerSource::Terminal),
                });
            }
        }
        Ok(Answer {
            choice: trimmed.to_string(),
            custom_text: Some(trimmed.to_string()),
            source: Some(AnswerSource::Terminal),
        })
    }
}

// ---------------------------------------------------------------------------
// JournalInterviewer
// ---------------------------------------------------------------------------

/// How often [`JournalInterviewer`] checks for an answer file.
pub const ANSWER_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// The Human Gate interviewer of `pas run` (spec File Change 9).
///
/// Waits for the first valid answer from the terminal (only when stdin is a
/// TTY) or from `answers/<question_id>.json` in the Run folder, checked every
/// [`ANSWER_POLL_INTERVAL`]. A terminal answer is written to the answer file
/// too, so the file alone decides which answer came first. An answer file
/// that does not fit the question is moved to `<question_id>.json.rejected`
/// and the gate keeps waiting.
pub struct JournalInterviewer {
    run_dir: RunDir,
    terminal: Option<Mutex<TerminalLines>>,
    poll: Duration,
}

/// Lines typed at the terminal, read by one long-lived thread so that a gate
/// that stops waiting never leaves a read behind that steals the next line.
enum TerminalLines {
    /// stdin is a TTY that nothing has read yet.
    Unstarted,
    Open(mpsc::UnboundedReceiver<String>),
    /// stdin reached end of file; only answer files remain.
    Closed,
}

impl TerminalLines {
    /// The next line; never resolves once stdin is closed.
    async fn next(&mut self) -> String {
        if let Self::Unstarted = self {
            *self = Self::Open(spawn_stdin_reader());
        }
        if let Self::Open(lines) = self {
            match lines.recv().await {
                Some(line) => return line,
                None => *self = Self::Closed,
            }
        }
        std::future::pending().await
    }
}

fn spawn_stdin_reader() -> mpsc::UnboundedReceiver<String> {
    let (tx, rx) = mpsc::unbounded_channel();
    let spawned = std::thread::Builder::new()
        .name("pas-stdin".into())
        .spawn(move || loop {
            let mut line = String::new();
            match std::io::stdin().read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("cannot read the terminal: {e}");
    }
    rx
}

impl JournalInterviewer {
    /// Answers for Human Gates of the Run in `run_dir`. The terminal is used
    /// only when stdin is a TTY.
    pub fn new(run_dir: RunDir) -> Self {
        Self::with_stdin_tty(run_dir, std::io::stdin().is_terminal())
    }

    fn with_stdin_tty(run_dir: RunDir, stdin_is_tty: bool) -> Self {
        Self {
            run_dir,
            terminal: stdin_is_tty.then(|| Mutex::new(TerminalLines::Unstarted)),
            poll: ANSWER_POLL_INTERVAL,
        }
    }

    /// Whether a Human Gate also waits for a terminal answer.
    pub fn reads_terminal(&self) -> bool {
        self.terminal.is_some()
    }

    #[cfg(test)]
    fn with_terminal(run_dir: RunDir, lines: mpsc::UnboundedReceiver<String>) -> Self {
        Self {
            run_dir,
            terminal: Some(Mutex::new(TerminalLines::Open(lines))),
            poll: Duration::from_millis(20),
        }
    }

    #[cfg(test)]
    fn without_terminal(run_dir: RunDir) -> Self {
        Self {
            poll: Duration::from_millis(20),
            ..Self::with_stdin_tty(run_dir, false)
        }
    }

    async fn next_terminal_line(&self) -> String {
        match &self.terminal {
            Some(terminal) => terminal.lock().await.next().await,
            None => std::future::pending().await,
        }
    }

    /// The answer in the answer file, if it is there and fits `question`.
    /// A complete file that does not fit is moved aside.
    fn check_file(&self, question: &Question) -> Option<Answer> {
        let qid = &question.question_id;
        let path = self.run_dir.answer(qid);
        let problem = match attractor_journal::read_answer(&self.run_dir, qid) {
            Ok(None) => return None,
            Ok(Some(file)) if file.question_id != *qid => {
                format!("it answers question {:?}", file.question_id)
            }
            Ok(Some(file)) if !question.choices.contains(&file.choice) => format!(
                "choice {:?} is not one of {:?}",
                file.choice, question.choices
            ),
            Ok(Some(file)) => {
                return Some(Answer {
                    choice: file.choice,
                    custom_text: None,
                    source: Some(file.source),
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                format!("it is not an answer file: {e}")
            }
            Err(e) => {
                tracing::warn!("cannot read answer file {}: {e}", path.display());
                return None;
            }
        };
        match attractor_journal::reject_answer(&self.run_dir, qid) {
            Ok(rejected) => eprintln!(
                "Ignoring answer file {}: {problem}; moved to {}",
                path.display(),
                rejected.display()
            ),
            Err(e) => tracing::warn!(
                "ignoring answer file {} ({problem}) but cannot move it aside: {e}",
                path.display()
            ),
        }
        None
    }

    /// Record a terminal answer in the answer file. `None` when another
    /// answer file is there but not readable yet.
    fn answer_from_terminal(&self, question: &Question, choice: &str) -> Option<Answer> {
        let terminal = Answer {
            choice: choice.to_string(),
            custom_text: None,
            source: Some(AnswerSource::Terminal),
        };
        let file = AnswerFile::new(&question.question_id, choice, AnswerSource::Terminal);
        // Twice: the first attempt may find an unusable file, which
        // `check_file` moves aside.
        for _ in 0..2 {
            match attractor_journal::write_answer(&self.run_dir, &file) {
                Ok(true) => return Some(terminal),
                // Someone answered first; their answer wins if it is valid.
                Ok(false) => {
                    if let Some(answer) = self.check_file(question) {
                        return Some(answer);
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "cannot record the terminal answer in {}: {e}",
                        self.run_dir.answer(&question.question_id).display()
                    );
                    return Some(terminal);
                }
            }
        }
        None
    }
}

/// The choice a terminal line names: a 1-based index or a choice label
/// (compared like edge labels). `None` for anything else.
fn terminal_choice<'a>(line: &str, choices: &'a [String]) -> Option<&'a String> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    if let Ok(index) = line.parse::<usize>() {
        if (1..=choices.len()).contains(&index) {
            return Some(&choices[index - 1]);
        }
    }
    let wanted = crate::edge_selection::normalize_label(line);
    choices
        .iter()
        .find(|choice| crate::edge_selection::normalize_label(choice) == wanted)
}

#[async_trait]
impl Interviewer for JournalInterviewer {
    async fn ask(&self, question: &Question) -> Result<Answer> {
        let qid = &question.question_id;
        if !attractor_journal::is_valid_question_id(qid) {
            return Err(attractor_types::AttractorError::Other(format!(
                "invalid question id: {qid:?}"
            )));
        }
        if self.reads_terminal() {
            println!("\n{}", question.prompt);
            for (i, choice) in question.choices.iter().enumerate() {
                println!("  [{}] {}", i + 1, choice);
            }
        } else {
            eprintln!(
                "Human Gate {qid}: {}\n  choices: {}\n  waiting for an answer file: {}",
                question.prompt,
                question.choices.join(", "),
                self.run_dir.answer(qid).display()
            );
        }

        let mut poll = tokio::time::interval(self.poll);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                // The first tick is immediate: an answer written while the
                // Run was not running is picked up at once.
                _ = poll.tick() => {
                    if let Some(answer) = self.check_file(question) {
                        return Ok(answer);
                    }
                }
                line = self.next_terminal_line() => {
                    let Some(choice) = terminal_choice(&line, &question.choices) else {
                        println!(
                            "Please enter 1-{} or one of: {}",
                            question.choices.len(),
                            question.choices.join(", ")
                        );
                        continue;
                    };
                    if let Some(answer) = self.answer_from_terminal(question, choice) {
                        return Ok(answer);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// RecordingInterviewer
// ---------------------------------------------------------------------------

pub struct RecordingInterviewer {
    answers: std::sync::Mutex<Vec<Answer>>,
    questions: std::sync::Mutex<Vec<Question>>,
}

impl RecordingInterviewer {
    pub fn new(answers: Vec<Answer>) -> Self {
        let mut reversed = answers;
        reversed.reverse();
        Self {
            answers: std::sync::Mutex::new(reversed),
            questions: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub fn questions(&self) -> Vec<Question> {
        self.questions.lock().unwrap().clone()
    }
}

#[async_trait]
impl Interviewer for RecordingInterviewer {
    async fn ask(&self, question: &Question) -> Result<Answer> {
        self.questions.lock().unwrap().push(question.clone());
        let answer = self
            .answers
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_else(|| Answer {
                choice: question.choices.first().cloned().unwrap_or_default(),
                custom_text: None,
                source: None,
            });
        Ok(answer)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn auto_approve_picks_first_choice() {
        let interviewer = AutoApproveInterviewer;
        let question = Question {
            question_id: "q-pick-1".into(),
            prompt: "Pick one".into(),
            choices: vec!["Alpha".into(), "Beta".into()],
            default: None,
            timeout: None,
        };
        let answer = interviewer.ask(&question).await.unwrap();
        assert_eq!(answer.choice, "Alpha");
        assert!(answer.custom_text.is_none());
    }

    #[tokio::test]
    async fn auto_approve_picks_default_when_set() {
        let interviewer = AutoApproveInterviewer;
        let question = Question {
            question_id: "q-pick-1".into(),
            prompt: "Pick one".into(),
            choices: vec!["Alpha".into(), "Beta".into()],
            default: Some("Beta".into()),
            timeout: None,
        };
        let answer = interviewer.ask(&question).await.unwrap();
        assert_eq!(answer.choice, "Beta");
    }

    #[tokio::test]
    async fn recording_plays_back_answers() {
        let preset = vec![
            Answer {
                choice: "Yes".into(),
                custom_text: None,
                source: None,
            },
            Answer {
                choice: "No".into(),
                custom_text: Some("custom".into()),
                source: None,
            },
        ];
        let interviewer = RecordingInterviewer::new(preset);

        let q1 = Question {
            question_id: "q-first-1".into(),
            prompt: "First?".into(),
            choices: vec!["Yes".into(), "No".into()],
            default: None,
            timeout: None,
        };
        let q2 = Question {
            question_id: "q-second-1".into(),
            prompt: "Second?".into(),
            choices: vec!["Yes".into(), "No".into()],
            default: None,
            timeout: None,
        };

        let a1 = interviewer.ask(&q1).await.unwrap();
        assert_eq!(a1.choice, "Yes");

        let a2 = interviewer.ask(&q2).await.unwrap();
        assert_eq!(a2.choice, "No");
        assert_eq!(a2.custom_text.as_deref(), Some("custom"));

        let recorded = interviewer.questions();
        assert_eq!(recorded.len(), 2);
        assert_eq!(recorded[0].prompt, "First?");
        assert_eq!(recorded[1].prompt, "Second?");
    }

    // --- JournalInterviewer ---

    use attractor_journal::{read_answer, write_answer};
    use std::sync::Arc;
    use std::time::Instant;

    fn gate_question() -> Question {
        Question {
            question_id: "q-gate-1".into(),
            prompt: "Ship?".into(),
            choices: vec!["approve".into(), "reject".into()],
            default: None,
            timeout: None,
        }
    }

    fn run_dir() -> (tempfile::TempDir, RunDir) {
        let tmp = tempfile::tempdir().unwrap();
        let run = RunDir::from_path(tmp.path().join("run"));
        run.create_all().unwrap();
        (tmp, run)
    }

    fn write(run: &RunDir, qid: &str, choice: &str, source: AnswerSource) {
        assert!(write_answer(run, &AnswerFile::new(qid, choice, source)).unwrap());
    }

    fn spawn_ask(
        interviewer: JournalInterviewer,
    ) -> (
        Arc<JournalInterviewer>,
        tokio::task::JoinHandle<Result<Answer>>,
    ) {
        let interviewer = Arc::new(interviewer);
        let asking = interviewer.clone();
        let handle = tokio::spawn(async move { asking.ask(&gate_question()).await });
        (interviewer, handle)
    }

    async fn answered(handle: tokio::task::JoinHandle<Result<Answer>>) -> Answer {
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("the gate answered")
            .unwrap()
            .unwrap()
    }

    /// The gate is still waiting after several polls.
    async fn assert_still_waiting(handle: &tokio::task::JoinHandle<Result<Answer>>) {
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!handle.is_finished(), "the gate stopped waiting");
    }

    #[tokio::test]
    async fn answer_file_answers_with_its_source() {
        let (_tmp, run) = run_dir();
        let (_i, handle) = spawn_ask(JournalInterviewer::without_terminal(run.clone()));
        assert_still_waiting(&handle).await;
        let written = Instant::now();
        write(&run, "q-gate-1", "reject", AnswerSource::Monitor);
        let answer = answered(handle).await;
        assert!(written.elapsed() < Duration::from_secs(2));
        assert_eq!(answer.choice, "reject");
        assert_eq!(answer.source, Some(AnswerSource::Monitor));
        assert_eq!(answer.custom_text, None);
    }

    #[tokio::test]
    async fn answer_written_before_asking_is_taken_at_once() {
        let (_tmp, run) = run_dir();
        write(&run, "q-gate-1", "approve", AnswerSource::Cli);
        let mut interviewer = JournalInterviewer::without_terminal(run);
        interviewer.poll = Duration::from_secs(3600);
        let answer =
            tokio::time::timeout(Duration::from_secs(1), interviewer.ask(&gate_question()))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(answer.choice, "approve");
        assert_eq!(answer.source, Some(AnswerSource::Cli));
    }

    #[tokio::test]
    async fn terminal_answer_by_index_is_sourced_terminal_and_written_to_the_file() {
        let (_tmp, run) = run_dir();
        let (tx, rx) = mpsc::unbounded_channel();
        let (_i, handle) = spawn_ask(JournalInterviewer::with_terminal(run.clone(), rx));
        tx.send("2\n".into()).unwrap();
        let answer = answered(handle).await;
        assert_eq!(answer.choice, "reject");
        assert_eq!(answer.source, Some(AnswerSource::Terminal));
        let file = read_answer(&run, "q-gate-1").unwrap().unwrap();
        assert_eq!(file.choice, "reject");
        assert_eq!(file.source, AnswerSource::Terminal);
        assert_eq!(file.question_id, "q-gate-1");
    }

    #[tokio::test]
    async fn terminal_lines_that_name_no_choice_are_asked_again() {
        let (_tmp, run) = run_dir();
        let (tx, rx) = mpsc::unbounded_channel();
        let (_i, handle) = spawn_ask(JournalInterviewer::with_terminal(run.clone(), rx));
        for line in ["\n", "maybe\n", "0\n", "3\n", "  \n"] {
            tx.send(line.into()).unwrap();
        }
        assert_still_waiting(&handle).await;
        assert_eq!(read_answer(&run, "q-gate-1").unwrap(), None);
        tx.send("  APPROVE \n".into()).unwrap();
        let answer = answered(handle).await;
        assert_eq!(answer.choice, "approve");
        assert_eq!(answer.source, Some(AnswerSource::Terminal));
    }

    #[tokio::test]
    async fn terminal_matches_labels_like_edge_selection() {
        let choices = vec!["[Y] Yes".to_string(), "[N] No".to_string()];
        assert_eq!(terminal_choice("yes", &choices), Some(&choices[0]));
        assert_eq!(terminal_choice("[n] no", &choices), Some(&choices[1]));
        assert_eq!(terminal_choice("2", &choices), Some(&choices[1]));
        assert_eq!(terminal_choice("y", &choices), None);
        assert_eq!(terminal_choice("", &choices), None);
    }

    #[tokio::test]
    async fn earlier_answer_file_beats_a_later_terminal_answer() {
        let (_tmp, run) = run_dir();
        let (tx, rx) = mpsc::unbounded_channel();
        let mut interviewer = JournalInterviewer::with_terminal(run.clone(), rx);
        // The poll never runs after the first tick, so only the terminal
        // path can see the file.
        interviewer.poll = Duration::from_secs(3600);
        let (_i, handle) = spawn_ask(interviewer);
        tokio::time::sleep(Duration::from_millis(50)).await;
        write(&run, "q-gate-1", "reject", AnswerSource::Cli);
        tx.send("1\n".into()).unwrap();
        let answer = answered(handle).await;
        assert_eq!(answer.choice, "reject");
        assert_eq!(answer.source, Some(AnswerSource::Cli));
        let file = read_answer(&run, "q-gate-1").unwrap().unwrap();
        assert_eq!(file.source, AnswerSource::Cli, "the file is unchanged");
    }

    #[tokio::test]
    async fn closed_terminal_leaves_the_answer_file() {
        let (_tmp, run) = run_dir();
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        drop(tx);
        let (_i, handle) = spawn_ask(JournalInterviewer::with_terminal(run.clone(), rx));
        assert_still_waiting(&handle).await;
        write(&run, "q-gate-1", "approve", AnswerSource::Cli);
        assert_eq!(answered(handle).await.source, Some(AnswerSource::Cli));
    }

    #[tokio::test]
    async fn without_a_tty_the_terminal_is_not_read() {
        let (_tmp, run) = run_dir();
        assert!(!JournalInterviewer::with_stdin_tty(run.clone(), false).reads_terminal());
        assert!(JournalInterviewer::with_stdin_tty(run.clone(), true).reads_terminal());
        let interviewer = JournalInterviewer::without_terminal(run.clone());
        assert!(interviewer.terminal.is_none());
        let (_i, handle) = spawn_ask(interviewer);
        assert_still_waiting(&handle).await;
        write(&run, "q-gate-1", "approve", AnswerSource::Cli);
        assert_eq!(answered(handle).await.choice, "approve");
    }

    #[tokio::test]
    async fn answer_file_with_unknown_choice_is_rejected_and_the_gate_keeps_waiting() {
        let (_tmp, run) = run_dir();
        let (_i, handle) = spawn_ask(JournalInterviewer::without_terminal(run.clone()));
        write(&run, "q-gate-1", "maybe", AnswerSource::Cli);
        assert_still_waiting(&handle).await;
        assert!(!run.answer("q-gate-1").exists());
        let rejected = run.answers_dir().join("q-gate-1.json.rejected");
        assert!(std::fs::read_to_string(&rejected)
            .unwrap()
            .contains("maybe"));

        // Choices are exact: a differently cased label is not a choice.
        write(&run, "q-gate-1", "Approve", AnswerSource::Cli);
        assert_still_waiting(&handle).await;

        write(&run, "q-gate-1", "approve", AnswerSource::Monitor);
        let answer = answered(handle).await;
        assert_eq!(answer.choice, "approve");
        assert_eq!(answer.source, Some(AnswerSource::Monitor));
    }

    #[tokio::test]
    async fn answer_file_for_another_question_or_not_json_is_rejected() {
        let (_tmp, run) = run_dir();
        let (_i, handle) = spawn_ask(JournalInterviewer::without_terminal(run.clone()));
        let other = AnswerFile::new("q-other-1", "approve", AnswerSource::Cli);
        std::fs::write(run.answer("q-gate-1"), serde_json::to_vec(&other).unwrap()).unwrap();
        assert_still_waiting(&handle).await;
        assert!(!run.answer("q-gate-1").exists());

        std::fs::write(run.answer("q-gate-1"), "approve\n").unwrap();
        assert_still_waiting(&handle).await;
        assert!(!run.answer("q-gate-1").exists());

        write(&run, "q-gate-1", "approve", AnswerSource::Cli);
        assert_eq!(answered(handle).await.choice, "approve");
    }

    #[tokio::test]
    async fn torn_answer_file_is_waited_for_not_rejected() {
        let (_tmp, run) = run_dir();
        let (_i, handle) = spawn_ask(JournalInterviewer::without_terminal(run.clone()));
        let full = serde_json::to_string(&AnswerFile::new("q-gate-1", "reject", AnswerSource::Cli))
            .unwrap();
        std::fs::write(run.answer("q-gate-1"), &full[..full.len() / 2]).unwrap();
        assert_still_waiting(&handle).await;
        assert!(run.answer("q-gate-1").exists(), "not moved aside");
        std::fs::write(run.answer("q-gate-1"), &full).unwrap();
        assert_eq!(answered(handle).await.choice, "reject");
    }

    #[tokio::test]
    async fn invalid_question_id_is_an_error() {
        let (_tmp, run) = run_dir();
        let interviewer = JournalInterviewer::without_terminal(run);
        let mut question = gate_question();
        question.question_id = "../escape".into();
        assert!(interviewer.ask(&question).await.is_err());
    }
}
