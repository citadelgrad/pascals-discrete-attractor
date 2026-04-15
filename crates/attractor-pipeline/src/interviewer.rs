//! Interviewer trait and built-in implementations for human interaction.

use async_trait::async_trait;
use attractor_types::Result;

#[derive(Debug, Clone)]
pub struct Question {
    pub prompt: String,
    pub choices: Vec<String>,
    pub default: Option<String>,
    pub timeout: Option<std::time::Duration>,
}

#[derive(Debug, Clone)]
pub struct Answer {
    pub choice: String,
    pub custom_text: Option<String>,
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
                });
            }
        }
        Ok(Answer {
            choice: trimmed.to_string(),
            custom_text: Some(trimmed.to_string()),
        })
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
            },
            Answer {
                choice: "No".into(),
                custom_text: Some("custom".into()),
            },
        ];
        let interviewer = RecordingInterviewer::new(preset);

        let q1 = Question {
            prompt: "First?".into(),
            choices: vec!["Yes".into(), "No".into()],
            default: None,
            timeout: None,
        };
        let q2 = Question {
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
}
