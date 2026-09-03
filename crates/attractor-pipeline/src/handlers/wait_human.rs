//! WaitHumanHandler — pauses pipeline execution for human input.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use attractor_types::{Context, Outcome, Result, StageStatus};

use crate::graph::{PipelineGraph, PipelineNode};
use crate::handler::NodeHandler;
use crate::interviewer::{Interviewer, Question};

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
        "wait.human"
    }

    async fn execute(
        &self,
        node: &PipelineNode,
        _ctx: &Context,
        graph: &PipelineGraph,
    ) -> Result<Outcome> {
        let edges = graph.outgoing_edges(&node.id);
        let choices: Vec<String> = edges.iter().filter_map(|e| e.label.clone()).collect();

        let prompt = node.prompt.clone().unwrap_or_else(|| node.label.clone());

        let question = Question {
            prompt,
            choices: if choices.is_empty() {
                vec!["Continue".into()]
            } else {
                choices
            },
            default: None,
            timeout: node.timeout,
        };

        let answer = self.interviewer.ask(&question).await?;

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
}
