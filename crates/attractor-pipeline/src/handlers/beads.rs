//! `beads.select`: claim the next Task of an Epic (spec File Change 7).
//!
//! Attributes: `epic` (required), `order` and `exclude` (optional comma lists
//! of Task IDs). The handler reads the Epic and its children, emits
//! `EpicSnapshot`, then either claims one ready child (`MORE`), reports that
//! every child is closed (`DONE`), or emits `TaskSelectionBlocked`
//! (`BLOCKED`). All Beads access goes through [`BeadsAdapter`].

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use async_trait::async_trait;
use attractor_dot::AttributeValue;
use attractor_journal::TaskSummary;
use attractor_types::{AttractorError, Context, Outcome, Result};
use serde_json::Value;

use crate::beads_adapter::{BeadsAdapter, BeadsIssue};
use crate::events::PipelineEvent;
use crate::execution_plan::ResolvedNode;
use crate::graph::{PipelineGraph, PipelineNode};
use crate::handler::{EventSink, HandlerExecutionContext, NodeHandler, ResolvedNodeHandler};

const HANDLER: &str = "beads.select";
const CLOSED: &str = "closed";
const BLOCKS: &str = "blocks";

/// Where the claimed Task is handed to later stages, relative to the workdir.
pub const CURRENT_TASK_FILE: &str = ".pas/current_task.md";

pub const LABEL_MORE: &str = "MORE";
pub const LABEL_DONE: &str = "DONE";
pub const LABEL_BLOCKED: &str = "BLOCKED";

/// Claims the next Task of an Epic in Beads.
#[derive(Debug, Clone, Default)]
pub struct BeadsSelectHandler {
    adapter: BeadsAdapter,
}

impl BeadsSelectHandler {
    /// Use `adapter` for Beads access; it is run in the Run's workdir.
    pub fn new(adapter: BeadsAdapter) -> Self {
        Self { adapter }
    }
}

#[async_trait]
impl NodeHandler for BeadsSelectHandler {
    fn handler_type(&self) -> &str {
        HANDLER
    }

    fn resolved_handler(&self) -> Option<&dyn ResolvedNodeHandler> {
        Some(self)
    }

    async fn execute(
        &self,
        node: &PipelineNode,
        context: &Context,
        _graph: &PipelineGraph,
    ) -> Result<Outcome> {
        let dry_run = context
            .get("dry_run")
            .await
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let workdir = context
            .get("workdir")
            .await
            .and_then(|value| value.as_str().map(std::path::PathBuf::from))
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        self.select(node, dry_run, &workdir, None).await
    }
}

#[async_trait]
impl ResolvedNodeHandler for BeadsSelectHandler {
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
        _graph: &PipelineGraph,
    ) -> Result<Outcome> {
        self.select(
            node,
            *execution.config().dry_run().value(),
            execution.config().workdir().value(),
            execution.events(),
        )
        .await
    }
}

impl BeadsSelectHandler {
    async fn select(
        &self,
        node: &PipelineNode,
        dry_run: bool,
        workdir: &Path,
        events: Option<&dyn EventSink>,
    ) -> Result<Outcome> {
        let fail = |message: String| AttractorError::HandlerError {
            handler: HANDLER.into(),
            node: node.id.clone(),
            message,
        };
        let attrs = SelectAttrs::from_node(node).map_err(fail)?;
        let epic_id = attrs.epic.as_str();
        let emit = |event: PipelineEvent| {
            if let Some(events) = events {
                events.emit(event);
            }
        };

        let bd = self.adapter.clone().in_dir(workdir);
        let epic = bd
            .show(epic_id)
            .await
            .map_err(|e| fail(format!("cannot read epic '{epic_id}': {e}")))?;
        let children = bd
            .children(epic_id)
            .await
            .map_err(|e| fail(format!("cannot list children of epic '{epic_id}': {e}")))?;
        if children.is_empty() {
            // A childless Epic would otherwise be vacuously DONE.
            return Err(fail(format!("epic '{epic_id}' has no children")));
        }
        emit(PipelineEvent::EpicSnapshot {
            epic_id: epic_id.to_string(),
            title: epic.title.clone(),
            tasks: children
                .iter()
                .map(|task| TaskSummary {
                    id: task.id.clone(),
                    title: task.title.clone(),
                    status: task.status.clone(),
                })
                .collect(),
        });
        let ready: HashSet<String> = bd
            .ready()
            .await
            .map_err(|e| {
                fail(format!(
                    "cannot list ready issues for epic '{epic_id}': {e}"
                ))
            })?
            .into_iter()
            .map(|issue| issue.id)
            .collect();

        match choose(&children, &ready, &attrs.order, &attrs.exclude) {
            Selection::Claim(task) => {
                let mut outcome = Outcome::success(format!("claimed {}", task.id));
                outcome.preferred_label = Some(LABEL_MORE.into());
                outcome.context_updates = task_context(task);
                if dry_run {
                    outcome.notes = format!("dry run: would claim {}", task.id);
                    return Ok(outcome);
                }
                let claimed = bd
                    .claim(&task.id)
                    .await
                    .map_err(|e| fail(format!("cannot claim task '{}': {e}", task.id)))?;
                let path = workdir.join(CURRENT_TASK_FILE);
                write_current_task(&path, task, &claimed, &epic).map_err(|e| {
                    fail(format!(
                        "claimed task '{}' but cannot write {}: {e}",
                        task.id,
                        path.display()
                    ))
                })?;
                emit(PipelineEvent::TaskClaimed {
                    task_id: task.id.clone(),
                    title: task.title.clone(),
                    epic_id: epic_id.to_string(),
                    node_id: node.id.clone(),
                });
                Ok(outcome)
            }
            Selection::Done => {
                let mut outcome = Outcome::success(format!("epic {epic_id}: all children closed"));
                outcome.preferred_label = Some(LABEL_DONE.into());
                Ok(outcome)
            }
            Selection::Blocked { open, blocked_by } => {
                let notes = format!("epic {epic_id}: no claimable task among {open:?}");
                emit(PipelineEvent::TaskSelectionBlocked {
                    epic_id: epic_id.to_string(),
                    open,
                    blocked_by,
                });
                let mut outcome = Outcome::success(notes);
                outcome.preferred_label = Some(LABEL_BLOCKED.into());
                Ok(outcome)
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct SelectAttrs {
    epic: String,
    order: Vec<String>,
    exclude: Vec<String>,
}

impl SelectAttrs {
    fn from_node(node: &PipelineNode) -> std::result::Result<Self, String> {
        let text = |name: &str| match node.raw_attrs.get(name) {
            None => Ok(None),
            Some(AttributeValue::String(value)) => Ok(Some(value.trim().to_string())),
            Some(other) => Err(format!(
                "attribute `{name}` must be a string, got {other:?}"
            )),
        };
        let list = |name: &str| {
            text(name).map(|value| {
                value
                    .unwrap_or_default()
                    .split(',')
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                    .map(String::from)
                    .collect::<Vec<_>>()
            })
        };
        let epic = text("epic")?
            .filter(|epic| !epic.is_empty())
            .ok_or_else(|| "missing required attribute `epic`".to_string())?;
        Ok(Self {
            epic,
            order: list("order")?,
            exclude: list("exclude")?,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Selection<'a> {
    Claim(&'a BeadsIssue),
    Done,
    Blocked {
        open: Vec<String>,
        blocked_by: BTreeMap<String, Vec<String>>,
    },
}

/// Candidates are open children that are ready and not excluded, sorted by
/// position in `order`, then priority, then natural ID order. Issues outside
/// `children` (the Epic itself, unrelated ready issues) are never chosen.
fn choose<'a>(
    children: &'a [BeadsIssue],
    ready: &HashSet<String>,
    order: &[String],
    exclude: &[String],
) -> Selection<'a> {
    let mut open: Vec<&BeadsIssue> = children.iter().filter(|t| t.status != CLOSED).collect();
    if open.is_empty() {
        return Selection::Done;
    }
    let rank = |task: &BeadsIssue| order.iter().position(|id| *id == task.id);
    open.sort_by(|a, b| {
        rank(a)
            .unwrap_or(usize::MAX)
            .cmp(&rank(b).unwrap_or(usize::MAX))
            .then(
                a.priority
                    .unwrap_or(i64::MAX)
                    .cmp(&b.priority.unwrap_or(i64::MAX)),
            )
            .then_with(|| natural_cmp(&a.id, &b.id))
    });
    if let Some(task) = open
        .iter()
        .find(|t| ready.contains(&t.id) && !exclude.contains(&t.id))
    {
        return Selection::Claim(task);
    }
    let closed: HashSet<&str> = children
        .iter()
        .filter(|t| t.status == CLOSED)
        .map(|t| t.id.as_str())
        .collect();
    let blocked_by = open
        .iter()
        .map(|task| {
            let blockers = task
                .dependencies
                .iter()
                .filter(|d| d.dep_type == BLOCKS && !closed.contains(d.depends_on_id.as_str()))
                .map(|d| d.depends_on_id.clone())
                .collect();
            (task.id.clone(), blockers)
        })
        .collect();
    Selection::Blocked {
        open: open.iter().map(|t| t.id.clone()).collect(),
        blocked_by,
    }
}

/// Compares IDs with digit runs taken as numbers, so `e.9` < `e.10`.
fn natural_cmp(a: &str, b: &str) -> Ordering {
    fn chunks(s: &str) -> Vec<(bool, &str)> {
        let mut out = Vec::new();
        let mut start = 0;
        let bytes = s.as_bytes();
        for i in 1..=bytes.len() {
            if i == bytes.len() || bytes[i].is_ascii_digit() != bytes[start].is_ascii_digit() {
                out.push((bytes[start].is_ascii_digit(), &s[start..i]));
                start = i;
            }
        }
        out
    }
    let (ca, cb) = (chunks(a), chunks(b));
    for ((da, xa), (db, xb)) in ca.iter().zip(cb.iter()) {
        let ord = if *da && *db {
            let (ta, tb) = (xa.trim_start_matches('0'), xb.trim_start_matches('0'));
            ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb))
        } else {
            xa.cmp(xb)
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    ca.len().cmp(&cb.len()).then_with(|| a.cmp(b))
}

fn task_context(task: &BeadsIssue) -> HashMap<String, Value> {
    let text = |value: &Option<String>| Value::String(value.clone().unwrap_or_default());
    HashMap::from([
        ("task.id".to_string(), Value::String(task.id.clone())),
        ("task.title".to_string(), Value::String(task.title.clone())),
        ("task.description".to_string(), text(&task.description)),
        (
            "task.acceptance".to_string(),
            text(&task.acceptance_criteria),
        ),
        ("task.design".to_string(), text(&task.design)),
    ])
}

/// The handoff later prompts read; overwritten on every claim.
fn render_current_task(task: &BeadsIssue, claimed: &BeadsIssue, epic: &BeadsIssue) -> String {
    let mut out = String::from("# Current Task\n\n");
    out.push_str(&format!("- **ID:** {}\n", task.id));
    out.push_str(&format!("- **Title:** {}\n", task.title));
    out.push_str(&format!("- **Epic:** {} — {}\n", epic.id, epic.title));
    let assignee = claimed
        .assignee
        .as_deref()
        .map(|a| format!(" (assignee {a})"))
        .unwrap_or_default();
    out.push_str(&format!("- **Status:** {}{assignee}\n", claimed.status));
    let priority = task
        .priority
        .map(|p| p.to_string())
        .unwrap_or_else(|| "-".into());
    let kind = task.issue_type.as_deref().unwrap_or("-");
    out.push_str(&format!("- **Priority:** {priority}, type {kind}\n"));
    let sections = [
        ("Description", task.description.as_deref().or(Some(""))),
        ("Acceptance Criteria", task.acceptance_criteria.as_deref()),
        ("Design", task.design.as_deref()),
        ("Notes", task.notes.as_deref()),
    ];
    for (heading, body) in sections {
        let Some(body) = body else { continue };
        if body.trim().is_empty() && heading != "Description" {
            continue;
        }
        out.push_str(&format!("\n## {heading}\n\n{}\n", body.trim_end()));
    }
    let deps: Vec<String> = task
        .dependencies
        .iter()
        .filter(|d| d.dep_type != "parent-child")
        .map(|d| format!("- {} ({})", d.depends_on_id, d.dep_type))
        .collect();
    if !deps.is_empty() {
        out.push_str(&format!("\n## Dependencies\n\n{}\n", deps.join("\n")));
    }
    out
}

fn write_current_task(
    path: &Path,
    task: &BeadsIssue,
    claimed: &BeadsIssue,
    epic: &BeadsIssue,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, render_current_task(task, claimed, epic))
}

#[cfg(test)]
#[path = "beads_tests.rs"]
mod tests;
