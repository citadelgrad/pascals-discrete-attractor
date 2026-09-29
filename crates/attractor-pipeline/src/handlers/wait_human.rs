//! WaitHumanHandler — pauses pipeline execution for human input.
//!
//! In the engine it records `HumanInputRequested` before waiting and
//! `HumanInputAnswered` after a person answers (spec C3, File Change 9).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;

use attractor_journal::{question_id_for, EventData, JournalEvent, EVENTS_FILE};
use attractor_types::{AttractorError, Context, Outcome, Result, StageStatus};

use crate::events::PipelineEvent;
use crate::execution_plan::ResolvedNode;
use crate::graph::{PipelineGraph, PipelineNode};
use crate::handler::{EventSink, HandlerExecutionContext, NodeHandler, ResolvedNodeHandler};
use crate::interviewer::{Interviewer, Question};

const HANDLER: &str = "wait.human";

pub struct WaitHumanHandler {
    interviewer: Arc<dyn Interviewer>,
}

impl WaitHumanHandler {
    pub fn new(interviewer: Arc<dyn Interviewer>) -> Self {
        Self { interviewer }
    }
}

#[async_trait]
impl NodeHandler for WaitHumanHandler {
    fn handler_type(&self) -> &str {
        HANDLER
    }

    fn resolved_handler(&self) -> Option<&dyn ResolvedNodeHandler> {
        Some(self)
    }

    /// A Run killed while waiting for an answer asks again on resume.
    fn resumes_interrupted_attempt(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        node: &PipelineNode,
        _ctx: &Context,
        graph: &PipelineGraph,
    ) -> Result<Outcome> {
        self.wait(node, graph, None, None).await
    }
}

#[async_trait]
impl ResolvedNodeHandler for WaitHumanHandler {
    async fn execute_resolved(
        &self,
        node: &PipelineNode,
        _resolved: &ResolvedNode,
        context: &Context,
        graph: &PipelineGraph,
    ) -> Result<Outcome> {
        self.execute(node, context, graph).await
    }

    async fn execute_configured(
        &self,
        node: &PipelineNode,
        _resolved: &ResolvedNode,
        execution: HandlerExecutionContext<'_>,
        graph: &PipelineGraph,
    ) -> Result<Outcome> {
        self.wait(node, graph, execution.run_dir(), execution.events())
            .await
    }
}

impl WaitHumanHandler {
    async fn wait(
        &self,
        node: &PipelineNode,
        graph: &PipelineGraph,
        run_dir: Option<&Path>,
        events: Option<&dyn EventSink>,
    ) -> Result<Outcome> {
        let edges = graph.outgoing_edges(&node.id);
        let choices: Vec<String> = edges.iter().filter_map(|e| e.label.clone()).collect();

        let prompt = node.prompt.clone().unwrap_or_else(|| node.label.clone());

        let question_id = match run_dir {
            Some(run_dir) => {
                let journal = read_journal(&run_dir.join(EVENTS_FILE)).map_err(|e| {
                    AttractorError::HandlerError {
                        handler: HANDLER.into(),
                        node: node.id.clone(),
                        message: format!("cannot read the Run Journal: {e}"),
                    }
                })?;
                question_id(&node.id, &journal)
            }
            None => question_id_for(&node.id, 1),
        };

        let question = Question {
            question_id,
            prompt,
            choices: if choices.is_empty() {
                vec!["Continue".into()]
            } else {
                choices
            },
            default: None,
            timeout: node.timeout,
        };

        if let Some(events) = events {
            events.emit(PipelineEvent::HumanInputRequested {
                question_id: question.question_id.clone(),
                node_id: node.id.clone(),
                text: question.prompt.clone(),
                choices: question.choices.clone(),
                default: question.default.clone(),
            });
        }

        let answer = self.interviewer.ask(&question).await?;

        if let (Some(events), Some(source)) = (events, answer.source) {
            events.emit(PipelineEvent::HumanInputAnswered {
                question_id: question.question_id.clone(),
                choice: answer.choice.clone(),
                source,
            });
        }

        Ok(Outcome {
            status: StageStatus::Success,
            preferred_label: Some(answer.choice),
            suggested_next_ids: vec![],
            context_updates: HashMap::new(),
            notes: "Human responded".into(),
            failure_reason: None,
        })
    }
}

/// The Run Journal at `path`; empty when there is none yet.
fn read_journal(path: &Path) -> std::io::Result<Vec<JournalEvent>> {
    match attractor_journal::read_all(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        result => result,
    }
}

/// The question ID for this entry into the gate `node_id`. When the gate's
/// last question was never answered (the Run stopped while waiting) it is
/// asked again under the same ID; otherwise the ID is the first
/// `q-<node>-<n>` no question in the Run has used.
fn question_id(node_id: &str, journal: &[JournalEvent]) -> String {
    let mut used = HashSet::new();
    let mut answered = HashSet::new();
    let mut last_here = None;
    for event in journal {
        match &event.data {
            EventData::HumanInputRequested {
                question_id,
                node_id: asked_at,
                ..
            } => {
                used.insert(question_id.as_str());
                if asked_at == node_id {
                    last_here = Some(question_id.as_str());
                }
            }
            EventData::HumanInputAnswered { question_id, .. } => {
                answered.insert(question_id.as_str());
            }
            _ => {}
        }
    }
    if let Some(id) = last_here.filter(|id| !answered.contains(id)) {
        return id.to_string();
    }
    (1..)
        .map(|n| question_id_for(node_id, n))
        .find(|id| !used.contains(id.as_str()))
        .expect("an unused question id")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interviewer::{Answer, RecordingInterviewer};

    fn make_node(id: &str, label: &str, prompt: Option<&str>) -> PipelineNode {
        PipelineNode {
            id: id.to_string(),
            label: label.to_string(),
            shape: "hexagon".to_string(),
            node_type: Some("wait.human".to_string()),
            prompt: prompt.map(String::from),
            goal_gate: false,
            retry_target: None,
            fallback_retry_target: None,
            classes: Vec::new(),
            timeout: None,
            llm_model: None,
            llm_provider: None,
            raw_attrs: HashMap::new(),
        }
    }

    fn make_graph_with_labeled_edges(node_id: &str, labels: &[&str]) -> PipelineGraph {
        let mut dot = String::from("digraph G {\n");
        dot.push_str(&format!("  {} [shape=\"hexagon\"]\n", node_id));
        for (i, label) in labels.iter().enumerate() {
            let target = format!("target_{}", i);
            dot.push_str(&format!("  {} [shape=\"box\"]\n", target));
            dot.push_str(&format!(
                "  {} -> {} [label=\"{}\"]\n",
                node_id, target, label
            ));
        }
        dot.push_str("}\n");
        let parsed = attractor_dot::parse(&dot).unwrap();
        PipelineGraph::from_dot(parsed).unwrap()
    }

    #[tokio::test]
    async fn derives_choices_from_edges() {
        let answers = vec![Answer {
            choice: "Approve".into(),
            custom_text: None,
            source: None,
        }];
        let interviewer = Arc::new(RecordingInterviewer::new(answers));
        let handler = WaitHumanHandler::new(interviewer.clone());

        let node = make_node("review", "Review Step", Some("Please review"));
        let graph = make_graph_with_labeled_edges("review", &["Approve", "Reject"]);

        let ctx = Context::default();
        let outcome = handler.execute(&node, &ctx, &graph).await.unwrap();

        assert_eq!(outcome.status, StageStatus::Success);
        assert_eq!(outcome.preferred_label, Some("Approve".into()));
        assert_eq!(outcome.notes, "Human responded");

        let questions = interviewer.questions();
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].prompt, "Please review");
        assert!(questions[0].choices.contains(&"Approve".to_string()));
        assert!(questions[0].choices.contains(&"Reject".to_string()));
    }

    #[tokio::test]
    async fn returns_preferred_label_from_answer() {
        let answers = vec![Answer {
            choice: "Reject".into(),
            custom_text: Some("Not ready".into()),
            source: None,
        }];
        let interviewer = Arc::new(RecordingInterviewer::new(answers));
        let handler = WaitHumanHandler::new(interviewer);

        let node = make_node("gate", "Gate", None);
        let graph = make_graph_with_labeled_edges("gate", &["Approve", "Reject"]);

        let ctx = Context::default();
        let outcome = handler.execute(&node, &ctx, &graph).await.unwrap();

        assert_eq!(outcome.preferred_label, Some("Reject".into()));
    }

    #[tokio::test]
    async fn uses_continue_when_no_edge_labels() {
        let answers = vec![Answer {
            choice: "Continue".into(),
            custom_text: None,
            source: None,
        }];
        let interviewer = Arc::new(RecordingInterviewer::new(answers));
        let handler = WaitHumanHandler::new(interviewer.clone());

        let dot = r#"digraph G {
            gate [shape="hexagon"]
            next [shape="box"]
            gate -> next
        }"#;
        let parsed = attractor_dot::parse(dot).unwrap();
        let graph = PipelineGraph::from_dot(parsed).unwrap();

        let node = make_node("gate", "Gate", None);
        let ctx = Context::default();
        let outcome = handler.execute(&node, &ctx, &graph).await.unwrap();

        assert_eq!(outcome.preferred_label, Some("Continue".into()));

        let questions = interviewer.questions();
        assert_eq!(questions[0].choices, vec!["Continue".to_string()]);
    }

    #[tokio::test]
    async fn uses_label_as_prompt_fallback() {
        let answers = vec![Answer {
            choice: "OK".into(),
            custom_text: None,
            source: None,
        }];
        let interviewer = Arc::new(RecordingInterviewer::new(answers));
        let handler = WaitHumanHandler::new(interviewer.clone());

        let node = make_node("confirm", "Confirm Deployment", None);
        let graph = make_graph_with_labeled_edges("confirm", &["OK"]);

        let ctx = Context::default();
        handler.execute(&node, &ctx, &graph).await.unwrap();

        let questions = interviewer.questions();
        assert_eq!(questions[0].prompt, "Confirm Deployment");
    }

    // --- Human Gate Events through the engine (spec File Change 9) ---

    use std::path::PathBuf;
    use std::time::Duration;

    use attractor_journal::{
        write_answer, AnswerFile, AnswerSource, JournalWriter, RunDir, EVENTS_FILE,
    };

    use crate::interviewer::JournalInterviewer;
    use crate::{ConditionalHandler, ExitHandler, HandlerRegistry, PipelineExecutor, StartHandler};

    const RUN_ID: &str = "0192f3c4-5a6b-7c8d-9e0f-1a2b3c4d5e6f";

    fn gate_graph() -> PipelineGraph {
        let dot = r#"digraph G {
            start [shape="Mdiamond"]
            gate  [shape="hexagon", prompt="Ship?"]
            no    [shape="diamond"]
            done  [shape="Msquare"]
            start -> gate
            gate -> done [label="approve"]
            gate -> no   [label="reject"]
            gate -> gate [label="again"]
            no -> done
        }"#;
        PipelineGraph::from_dot(attractor_dot::parse(dot).unwrap()).unwrap()
    }

    struct Run {
        _tmp: tempfile::TempDir,
        dir: PathBuf,
    }

    impl Run {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let dir = tmp.path().join("runs").join(RUN_ID);
            RunDir::from_path(&dir).create_all().unwrap();
            Self { _tmp: tmp, dir }
        }

        fn run_dir(&self) -> RunDir {
            RunDir::from_path(&self.dir)
        }

        fn journal(&self) -> Vec<JournalEvent> {
            attractor_journal::read_all(self.dir.join(EVENTS_FILE)).unwrap()
        }

        fn answer(&self, qid: &str, choice: &str, source: AnswerSource) {
            let file = AnswerFile::new(qid, choice, source);
            assert!(write_answer(&self.run_dir(), &file).unwrap());
        }

        /// Run `gate_graph` with a journal already holding `seed`.
        fn execute(
            &self,
            seed: Vec<EventData>,
        ) -> tokio::task::JoinHandle<Result<crate::PipelineResult>> {
            self.execute_attempt(1, seed)
        }

        fn execute_attempt(
            &self,
            attempt: u32,
            seed: Vec<EventData>,
        ) -> tokio::task::JoinHandle<Result<crate::PipelineResult>> {
            let journal = JournalWriter::open(&self.dir, RUN_ID, attempt).unwrap();
            for data in seed {
                journal.append(data).unwrap();
            }
            let mut registry = HandlerRegistry::new();
            registry.register(StartHandler);
            registry.register(ExitHandler);
            registry.register(ConditionalHandler);
            registry.register(WaitHumanHandler::new(Arc::new(JournalInterviewer::new(
                self.run_dir(),
            ))));
            let logs = self._tmp.path().join("logs");
            tokio::spawn(async move {
                PipelineExecutor::new(registry)
                    .with_journal(journal)
                    .run_with_checkpoint(&gate_graph(), Context::new(), &logs)
                    .await
            })
        }

        async fn wait_for_request(&self, count: usize) -> Vec<JournalEvent> {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                let journal = self.journal();
                let asked = journal
                    .iter()
                    .filter(|e| matches!(e.data, EventData::HumanInputRequested { .. }))
                    .count();
                if asked >= count {
                    return journal;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "no HumanInputRequested"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }

    async fn finished(
        handle: tokio::task::JoinHandle<Result<crate::PipelineResult>>,
    ) -> crate::PipelineResult {
        tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("the Run finished")
            .unwrap()
            .unwrap()
    }

    fn requested(qid: &str, node: &str) -> EventData {
        EventData::HumanInputRequested {
            question_id: qid.into(),
            node_id: node.into(),
            text: "Ship?".into(),
            choices: vec!["approve".into(), "reject".into(), "again".into()],
            default: None,
        }
    }

    fn answered(qid: &str) -> EventData {
        EventData::HumanInputAnswered {
            question_id: qid.into(),
            choice: "again".into(),
            source: AnswerSource::Cli,
        }
    }

    fn requests(journal: &[JournalEvent]) -> Vec<String> {
        journal
            .iter()
            .filter_map(|e| match &e.data {
                EventData::HumanInputRequested { question_id, .. } => Some(question_id.clone()),
                _ => None,
            })
            .collect()
    }

    // AC1 + AC2: the request is journaled before the gate waits; an answer
    // file ends the wait, routes by its label, and names its source.
    #[tokio::test]
    async fn engine_journals_request_then_answer_and_routes_by_the_file() {
        let run = Run::new();
        let handle = run.execute(vec![]);
        let journal = run.wait_for_request(1).await;

        let last = journal.last().unwrap();
        let EventData::HumanInputRequested {
            question_id,
            node_id,
            text,
            choices,
            default,
        } = &last.data
        else {
            panic!("the last Event is {:?}", last.data);
        };
        assert_eq!(question_id, "q-gate-1");
        assert_eq!(node_id, "gate");
        assert_eq!(text, "Ship?");
        assert_eq!(choices, &["approve", "reject", "again"]);
        assert_eq!(default, &None);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!handle.is_finished(), "the gate waits");
        assert!(!run.journal().iter().any(|e| matches!(
            &e.data,
            EventData::StageCompleted { node_id, .. } if node_id == "gate"
        )));

        let written = chrono::Utc::now();
        run.answer("q-gate-1", "reject", AnswerSource::Monitor);
        let result = finished(handle).await;
        assert_eq!(result.completed_nodes, ["start", "gate", "no", "done"]);

        let journal = run.journal();
        let gate: Vec<&str> = journal
            .iter()
            .filter(|e| match &e.data {
                EventData::StageStarted { node_id, .. }
                | EventData::StageCompleted { node_id, .. } => node_id == "gate",
                EventData::EdgeSelected { from_node, .. } => from_node == "gate",
                EventData::HumanInputRequested { .. } | EventData::HumanInputAnswered { .. } => {
                    true
                }
                _ => false,
            })
            .map(|e| e.data.type_name())
            .collect();
        assert_eq!(
            gate,
            [
                "StageStarted",
                "HumanInputRequested",
                "HumanInputAnswered",
                "StageCompleted",
                "EdgeSelected"
            ]
        );
        let answer = journal
            .iter()
            .find(|e| matches!(
                &e.data,
                EventData::HumanInputAnswered { question_id, choice, source: AnswerSource::Monitor }
                    if question_id == "q-gate-1" && choice == "reject"
            ))
            .expect("HumanInputAnswered from the monitor");
        let waited = answer.ts - written;
        assert!(
            waited < chrono::Duration::seconds(2),
            "answered after {waited}"
        );
        assert!(journal.iter().any(|e| matches!(
            &e.data,
            EventData::EdgeSelected { from_node, to_node, edge_label: Some(label) }
                if from_node == "gate" && to_node == "no" && label == "reject"
        )));
    }

    // AC6: a Run resumed at an unanswered gate asks again under the same
    // question ID, and an answer written while it was down is taken.
    #[tokio::test]
    async fn resumed_run_asks_the_unanswered_question_again() {
        let run = Run::new();
        run.answer("q-gate-1", "approve", AnswerSource::Cli);
        let handle = run.execute(vec![requested("q-gate-1", "gate")]);
        let result = finished(handle).await;
        assert_eq!(result.completed_nodes, ["start", "gate", "done"]);
        assert_eq!(requests(&run.journal()), ["q-gate-1", "q-gate-1"]);
    }

    // AC6 through the real checkpoint: the attempt the process did not live
    // through is not counted, so the resumed Run waits at the gate again
    // (the gate has no retries) under the same question ID.
    #[tokio::test]
    async fn run_stopped_while_waiting_resumes_at_the_gate() {
        let run = Run::new();
        let first = run.execute_attempt(1, vec![]);
        run.wait_for_request(1).await;
        let checkpoint = crate::checkpoint::load_checkpoint(&run._tmp.path().join("logs"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(checkpoint.current_node_id, "gate");
        assert_eq!(checkpoint.active_node_id.as_deref(), Some("gate"));
        assert_eq!(checkpoint.active_node_attempts, 0);
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());

        let second = run.execute_attempt(2, vec![]);
        let journal = run.wait_for_request(2).await;
        assert_eq!(requests(&journal), ["q-gate-1", "q-gate-1"]);
        assert_eq!(journal.last().unwrap().attempt, 2);
        run.answer("q-gate-1", "approve", AnswerSource::Terminal);
        let result = finished(second).await;
        assert_eq!(result.completed_nodes, ["start", "gate", "done"]);
    }

    // Re-entering an answered gate (a loop) asks a new question.
    #[tokio::test]
    async fn looping_back_to_an_answered_gate_asks_a_new_question() {
        let run = Run::new();
        run.answer("q-gate-1", "again", AnswerSource::Cli);
        run.answer("q-gate-2", "approve", AnswerSource::Cli);
        let result = finished(run.execute(vec![])).await;
        assert_eq!(result.completed_nodes, ["start", "gate", "gate", "done"]);
        let journal = run.journal();
        assert_eq!(requests(&journal), ["q-gate-1", "q-gate-2"]);
        let answers: Vec<(String, String)> = journal
            .iter()
            .filter_map(|e| match &e.data {
                EventData::HumanInputAnswered {
                    question_id,
                    choice,
                    ..
                } => Some((question_id.clone(), choice.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            answers,
            [
                ("q-gate-1".to_string(), "again".to_string()),
                ("q-gate-2".to_string(), "approve".to_string())
            ]
        );
    }

    // Answers no person gave (auto-approve, recordings) are not journaled.
    #[tokio::test]
    async fn unsourced_answers_are_not_journaled_as_human_answers() {
        let run = Run::new();
        let journal = JournalWriter::open(&run.dir, RUN_ID, 1).unwrap();
        let mut registry = HandlerRegistry::new();
        registry.register(StartHandler);
        registry.register(ExitHandler);
        registry.register(ConditionalHandler);
        registry.register(WaitHumanHandler::new(Arc::new(
            crate::interviewer::AutoApproveInterviewer,
        )));
        PipelineExecutor::new(registry)
            .with_journal(journal)
            .run_with_checkpoint(&gate_graph(), Context::new(), &run._tmp.path().join("logs"))
            .await
            .unwrap();
        let journal = run.journal();
        assert_eq!(requests(&journal), ["q-gate-1"]);
        assert!(!journal
            .iter()
            .any(|e| matches!(e.data, EventData::HumanInputAnswered { .. })));
    }

    fn event(data: EventData) -> JournalEvent {
        JournalEvent::new(1, chrono::Utc::now(), RUN_ID, 1, data)
    }

    #[test]
    fn question_id_reuses_only_the_gates_own_unanswered_question() {
        assert_eq!(question_id("gate", &[]), "q-gate-1");

        let unanswered = [event(requested("q-gate-1", "gate"))];
        assert_eq!(question_id("gate", &unanswered), "q-gate-1");
        // Another gate never reuses it, and never collides with it.
        assert_eq!(question_id("other", &unanswered), "q-other-1");

        let answered_once = [
            event(requested("q-gate-1", "gate")),
            event(answered("q-gate-1")),
        ];
        assert_eq!(question_id("gate", &answered_once), "q-gate-2");

        // Only the gate's last question counts.
        let second_unanswered = [
            event(requested("q-gate-1", "gate")),
            event(answered("q-gate-1")),
            event(requested("q-gate-2", "gate")),
        ];
        assert_eq!(question_id("gate", &second_unanswered), "q-gate-2");

        // Node IDs that slug to the same text still get distinct IDs.
        let slug_clash = [
            event(requested("q-a_b-1", "a.b")),
            event(answered("q-a_b-1")),
        ];
        assert_eq!(question_id("a/b", &slug_clash), "q-a_b-2");
    }

    #[tokio::test]
    async fn without_a_run_folder_the_first_question_id_is_used() {
        let interviewer = Arc::new(RecordingInterviewer::new(vec![]));
        let handler = WaitHumanHandler::new(interviewer.clone());
        let graph = make_graph_with_labeled_edges("gate", &["Approve"]);
        handler
            .execute(
                &make_node("gate", "Gate", None),
                &Context::default(),
                &graph,
            )
            .await
            .unwrap();
        assert_eq!(interviewer.questions()[0].question_id, "q-gate-1");
    }
}
