use super::*;

use std::path::PathBuf;
use std::sync::Mutex;

use attractor_journal::{EventData, JournalWriter, EVENTS_FILE};

use crate::beads_adapter::test_support::{stub, workspace, TEST_ACTOR};
use crate::beads_adapter::{BeadsDependency, NewIssue};
use crate::engine::PipelineExecutor;
use crate::handler::{ConditionalHandler, ExitHandler, HandlerRegistry, StartHandler};
use crate::handlers::tests::make_node;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

#[derive(Default)]
struct EventLog(Mutex<Vec<PipelineEvent>>);

impl EventSink for EventLog {
    fn emit(&self, event: PipelineEvent) {
        self.0.lock().unwrap().push(event);
    }
}

impl EventLog {
    fn take(&self) -> Vec<PipelineEvent> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

fn names(events: &[PipelineEvent]) -> Vec<String> {
    events
        .iter()
        .map(|e| e.to_journal_data().type_name().to_string())
        .collect()
}

fn select_node(epic: &str, order: Option<&str>, exclude: Option<&str>) -> PipelineNode {
    let mut attrs = HashMap::from([("epic".to_string(), AttributeValue::String(epic.into()))]);
    if let Some(order) = order {
        attrs.insert("order".into(), AttributeValue::String(order.into()));
    }
    if let Some(exclude) = exclude {
        attrs.insert("exclude".into(), AttributeValue::String(exclude.into()));
    }
    make_node("pick", "box", None, attrs)
}

fn issue(id: &str, status: &str, priority: Option<i64>) -> BeadsIssue {
    BeadsIssue {
        id: id.into(),
        title: format!("title {id}"),
        status: status.into(),
        priority,
        issue_type: Some("task".into()),
        description: Some(format!("description {id}")),
        acceptance_criteria: None,
        design: None,
        notes: None,
        parent: None,
        assignee: None,
        close_reason: None,
        dependencies: Vec::new(),
    }
}

fn blocks(id: &str) -> BeadsDependency {
    BeadsDependency {
        depends_on_id: id.into(),
        dep_type: "blocks".into(),
    }
}

fn ready(ids: &[&str]) -> HashSet<String> {
    ids.iter().map(|s| s.to_string()).collect()
}

fn strings(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|s| s.to_string()).collect()
}

fn claimed_id(selection: Selection<'_>) -> String {
    match selection {
        Selection::Claim(task) => task.id.clone(),
        other => panic!("expected a claim, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Pure selection (no bd)
// ---------------------------------------------------------------------------

// AC1: order "C,A" picks C, then A, then the rest.
#[test]
fn order_is_followed_then_the_rest() {
    let children = vec![
        issue("e.1", "open", Some(2)), // A
        issue("e.2", "open", Some(0)), // B: best priority, but not in order
        issue("e.3", "open", Some(2)), // C
    ];
    let order = strings(&["e.3", "e.1"]);
    let mut ready_ids = ready(&["e.1", "e.2", "e.3"]);
    let mut claimed = Vec::new();
    for _ in 0..3 {
        let id = claimed_id(choose(&children, &ready_ids, &order, &[]));
        ready_ids.remove(&id); // a claimed Task leaves `bd ready`
        claimed.push(id);
    }
    assert_eq!(claimed, strings(&["e.3", "e.1", "e.2"]));
}

// AC2: without order, priority then natural ID; missing priority last.
#[test]
fn priority_then_natural_id_without_order() {
    let children = vec![
        issue("e.10", "open", Some(1)),
        issue("e.2", "open", None),
        issue("e.9", "open", Some(1)),
        issue("e.1", "open", Some(3)),
    ];
    let mut ready_ids = ready(&["e.1", "e.2", "e.9", "e.10"]);
    let mut claimed = Vec::new();
    while let Selection::Claim(task) = choose(&children, &ready_ids, &[], &[]) {
        ready_ids.remove(&task.id);
        claimed.push(task.id.clone());
    }
    assert_eq!(claimed, strings(&["e.9", "e.10", "e.1", "e.2"]));
}

#[test]
fn natural_id_order() {
    let mut ids = vec!["e.10", "e.9", "e.1.2", "e.1", "e.1.10", "e.01", "f", "e"];
    ids.sort_by(|a, b| natural_cmp(a, b));
    assert_eq!(
        ids,
        vec!["e", "e.01", "e.1", "e.1.2", "e.1.10", "e.9", "e.10", "f"]
    );
    assert_eq!(natural_cmp("x", "x"), Ordering::Equal);
}

// AC3: exclude wins over order and priority.
#[test]
fn excluded_child_is_never_chosen() {
    let children = vec![issue("e.1", "open", Some(0)), issue("e.2", "open", Some(3))];
    let exclude = strings(&["e.1"]);
    let selection = choose(&children, &ready(&["e.1", "e.2"]), &exclude, &exclude);
    assert_eq!(claimed_id(selection), "e.2");

    // Only the excluded child is ready: BLOCKED, and it is listed as open.
    let selection = choose(&children, &ready(&["e.1"]), &[], &exclude);
    assert_eq!(
        selection,
        Selection::Blocked {
            open: strings(&["e.1", "e.2"]),
            blocked_by: BTreeMap::from([("e.1".into(), vec![]), ("e.2".into(), vec![])]),
        }
    );
}

// AC6: all children closed → DONE.
#[test]
fn all_children_closed_is_done() {
    let children = vec![
        issue("e.1", "closed", Some(1)),
        issue("e.2", "closed", None),
    ];
    assert_eq!(
        choose(&children, &ready(&["e.1", "e.2"]), &[], &[]),
        Selection::Done
    );
}

// AC7: open but unready children → BLOCKED with their open blockers.
#[test]
fn open_but_blocked_children_are_reported() {
    let mut t1 = issue("e.1", "open", Some(1));
    t1.dependencies = vec![
        blocks("x"),
        BeadsDependency {
            depends_on_id: "e".into(),
            dep_type: "parent-child".into(),
        },
    ];
    let mut t2 = issue("e.2", "open", Some(1));
    t2.dependencies = vec![blocks("e.1"), blocks("e.0")];
    let t0 = issue("e.0", "closed", Some(1));
    let t3 = issue("e.3", "in_progress", Some(1));
    let children = vec![t0, t1, t2, t3];

    let selection = choose(&children, &ready(&["e", "x"]), &[], &[]);
    assert_eq!(
        selection,
        Selection::Blocked {
            open: strings(&["e.1", "e.2", "e.3"]),
            blocked_by: BTreeMap::from([
                ("e.1".into(), strings(&["x"])),
                ("e.2".into(), strings(&["e.1"])), // closed e.0 dropped
                ("e.3".into(), vec![]),
            ]),
        }
    );
}

// AC9: ready issues outside the Epic's children are never chosen.
#[test]
fn ready_issue_outside_children_is_never_chosen() {
    let children = vec![issue("e.1", "open", Some(4))];
    let order = strings(&["x", "e"]);
    let selection = choose(&children, &ready(&["e", "x", "e.1"]), &order, &[]);
    assert_eq!(claimed_id(selection), "e.1");
    let selection = choose(&children, &ready(&["e", "x"]), &order, &[]);
    assert!(matches!(selection, Selection::Blocked { ref open, .. } if open == &strings(&["e.1"])));
}

#[test]
fn attributes_are_parsed_and_validated() {
    let node = select_node(" e ", Some(" e.3, ,e.1 "), Some("e.2"));
    assert_eq!(
        SelectAttrs::from_node(&node).unwrap(),
        SelectAttrs {
            epic: "e".into(),
            order: strings(&["e.3", "e.1"]),
            exclude: strings(&["e.2"]),
        }
    );
    let node = make_node("pick", "box", None, HashMap::new());
    assert!(SelectAttrs::from_node(&node)
        .unwrap_err()
        .contains("missing required attribute `epic`"));
    let node = select_node("  ", None, None);
    assert!(SelectAttrs::from_node(&node).is_err());
    let mut node = select_node("e", None, None);
    node.raw_attrs
        .insert("order".into(), AttributeValue::Integer(3));
    assert!(SelectAttrs::from_node(&node)
        .unwrap_err()
        .contains("`order` must be a string"));
}

#[test]
fn current_task_file_has_title_description_and_sections() {
    let mut task = issue("e.1", "open", Some(1));
    task.acceptance_criteria = Some("- [ ] works".into());
    task.dependencies = vec![
        blocks("e.0"),
        BeadsDependency {
            depends_on_id: "e".into(),
            dep_type: "parent-child".into(),
        },
    ];
    let mut claimed = issue("e.1", "in_progress", Some(1));
    claimed.assignee = Some("me".into());
    let mut epic = issue("e", "open", None);
    epic.title = "The Epic".into();

    let text = render_current_task(&task, &claimed, &epic);
    for expected in [
        "# Current Task",
        "- **ID:** e.1",
        "- **Title:** title e.1",
        "- **Epic:** e — The Epic",
        "- **Status:** in_progress (assignee me)",
        "- **Priority:** 1, type task",
        "## Description\n\ndescription e.1",
        "## Acceptance Criteria\n\n- [ ] works",
        "## Dependencies\n\n- e.0 (blocks)",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in\n{text}");
    }
    assert!(!text.contains("## Design"));
    assert!(!text.contains("## Notes"));
    assert!(!text.contains("parent-child"));
}

#[test]
fn task_context_has_the_five_spec_keys() {
    let mut task = issue("e.1", "open", Some(1));
    task.design = Some("design".into());
    let context = task_context(&task);
    let mut keys: Vec<_> = context.keys().cloned().collect();
    keys.sort();
    assert_eq!(
        keys,
        strings(&[
            "task.acceptance",
            "task.description",
            "task.design",
            "task.id",
            "task.title"
        ])
    );
    assert_eq!(context["task.id"], "e.1");
    assert_eq!(context["task.acceptance"], "");
    assert_eq!(context["task.design"], "design");
}

// ---------------------------------------------------------------------------
// Stub Beads program (no bd needed)
// ---------------------------------------------------------------------------

const STUB_EPIC: &str = r#"[{"id":"e","title":"Stub Epic","status":"open","issue_type":"epic"}]"#;
const STUB_CHILDREN: &str = r#"[{"id":"e.1","title":"One","status":"open","priority":1,"description":"first task","dependencies":[{"depends_on_id":"e","type":"parent-child"}]},{"id":"e.2","title":"Two","status":"closed","priority":1}]"#;
const STUB_READY: &str = r#"[{"id":"e","title":"Stub Epic","status":"open"},{"id":"e.1","title":"One","status":"open"}]"#;
const STUB_CLAIMED: &str =
    r#"[{"id":"e.1","title":"One","status":"in_progress","assignee":"stub"}]"#;

/// A fake Beads program that logs its arguments to `calls.log` and answers
/// from fixed JSON; `children` / `update` may be overridden.
fn stub_bd(dir: &Path, children: &str, update: &str) -> PathBuf {
    let log = dir.join("calls.log");
    stub(
        dir,
        "fake-beads",
        &format!(
            r#"echo "$*" >> '{log}'
case "$1" in
  show) if [ "$2" = e ]; then echo '{STUB_EPIC}'; else echo "Error: no issue found matching \"$2\"" >&2; exit 1; fi ;;
  children) {children} ;;
  ready) echo '{STUB_READY}' ;;
  update) {update} ;;
  *) echo "unexpected $*" >&2; exit 2 ;;
esac"#,
            log = log.display()
        ),
    )
}

fn default_children() -> String {
    format!("echo '{STUB_CHILDREN}'")
}

fn default_update() -> String {
    format!("echo '{STUB_CLAIMED}'")
}

fn calls(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("calls.log")).unwrap_or_default()
}

// AC4 + AC5 without bd: claim, handoff file, context, events, MORE.
#[tokio::test]
async fn stub_claim_writes_handoff_sets_context_and_emits_events() {
    let dir = tempfile::tempdir().unwrap();
    let program = stub_bd(dir.path(), &default_children(), &default_update());
    let handler = BeadsSelectHandler::new(BeadsAdapter::new().with_program(&program));
    let events = EventLog::default();

    let outcome = handler
        .select(
            &select_node("e", None, None),
            false,
            dir.path(),
            Some(&events),
        )
        .await
        .unwrap();

    assert_eq!(outcome.preferred_label.as_deref(), Some("MORE"));
    assert_eq!(outcome.context_updates["task.id"], "e.1");
    assert_eq!(outcome.context_updates["task.description"], "first task");
    assert!(calls(dir.path()).contains("update e.1 --claim --json"));
    let handoff = std::fs::read_to_string(dir.path().join(CURRENT_TASK_FILE)).unwrap();
    assert!(handoff.contains("- **Title:** One"), "{handoff}");
    assert!(handoff.contains("first task"), "{handoff}");
    assert!(handoff.contains("in_progress (assignee stub)"), "{handoff}");

    let events = events.take();
    assert_eq!(names(&events), ["EpicSnapshot", "TaskClaimed"]);
    let EventData::EpicSnapshot {
        epic_id,
        title,
        tasks,
    } = events[0].to_journal_data()
    else {
        unreachable!()
    };
    assert_eq!((epic_id.as_str(), title.as_str()), ("e", "Stub Epic"));
    assert_eq!(
        tasks,
        vec![
            TaskSummary {
                id: "e.1".into(),
                title: "One".into(),
                status: "open".into()
            },
            TaskSummary {
                id: "e.2".into(),
                title: "Two".into(),
                status: "closed".into()
            },
        ]
    );
    assert_eq!(
        events[1].to_journal_data(),
        EventData::TaskClaimed {
            task_id: "e.1".into(),
            title: "One".into(),
            epic_id: "e".into(),
            node_id: "pick".into(),
        }
    );
}

// AC8 without bd: an unknown Epic fails the stage and names the ID.
#[tokio::test]
async fn stub_unknown_epic_fails_naming_the_id() {
    let dir = tempfile::tempdir().unwrap();
    let program = stub_bd(dir.path(), &default_children(), &default_update());
    let handler = BeadsSelectHandler::new(BeadsAdapter::new().with_program(&program));
    let events = EventLog::default();

    let err = handler
        .select(
            &select_node("t-nope", None, None),
            false,
            dir.path(),
            Some(&events),
        )
        .await
        .unwrap_err();

    let message = err.to_string();
    assert!(matches!(err, AttractorError::HandlerError { .. }));
    assert!(message.contains("epic 't-nope'"), "{message}");
    assert!(message.contains("no issue found"), "{message}");
    assert!(message.contains("beads.select"), "{message}");
    assert!(events.take().is_empty());
    assert!(!dir.path().join(CURRENT_TASK_FILE).exists());
}

#[tokio::test]
async fn stub_missing_beads_program_fails_naming_the_epic() {
    let dir = tempfile::tempdir().unwrap();
    let handler =
        BeadsSelectHandler::new(BeadsAdapter::new().with_program(dir.path().join("missing")));
    let err = handler
        .select(&select_node("e", None, None), false, dir.path(), None)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("epic 'e'") && err.contains("PATH"), "{err}");
}

// D1: a childless Epic fails instead of being vacuously DONE.
#[tokio::test]
async fn stub_epic_without_children_fails() {
    let dir = tempfile::tempdir().unwrap();
    let program = stub_bd(dir.path(), "echo '[]'", &default_update());
    let handler = BeadsSelectHandler::new(BeadsAdapter::new().with_program(&program));
    let events = EventLog::default();
    let err = handler
        .select(
            &select_node("e", None, None),
            false,
            dir.path(),
            Some(&events),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("epic 'e' has no children"), "{err}");
    assert!(events.take().is_empty());
}

// A failed claim fails the stage: no handoff file, no TaskClaimed.
#[tokio::test]
async fn stub_failed_claim_fails_the_stage() {
    let dir = tempfile::tempdir().unwrap();
    let program = stub_bd(
        dir.path(),
        &default_children(),
        "echo 'Error: already claimed' >&2; exit 1",
    );
    let handler = BeadsSelectHandler::new(BeadsAdapter::new().with_program(&program));
    let events = EventLog::default();
    let err = handler
        .select(
            &select_node("e", None, None),
            false,
            dir.path(),
            Some(&events),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("cannot claim task 'e.1'"), "{err}");
    assert!(err.contains("already claimed"), "{err}");
    assert_eq!(names(&events.take()), ["EpicSnapshot"]);
    assert!(!dir.path().join(CURRENT_TASK_FILE).exists());
}

// D2: dry run reads and selects but never claims or writes the handoff.
#[tokio::test]
async fn stub_dry_run_does_not_claim() {
    let dir = tempfile::tempdir().unwrap();
    let program = stub_bd(
        dir.path(),
        &default_children(),
        "echo 'update must not run' >&2; exit 9",
    );
    let handler = BeadsSelectHandler::new(BeadsAdapter::new().with_program(&program));
    let events = EventLog::default();
    let outcome = handler
        .select(
            &select_node("e", None, None),
            true,
            dir.path(),
            Some(&events),
        )
        .await
        .unwrap();
    assert_eq!(outcome.preferred_label.as_deref(), Some("MORE"));
    assert_eq!(outcome.context_updates["task.id"], "e.1");
    assert!(!calls(dir.path()).contains("update"));
    assert!(!dir.path().join(CURRENT_TASK_FILE).exists());
    assert_eq!(names(&events.take()), ["EpicSnapshot"]);
}

// Legacy `execute` reads workdir and dry_run from the Context.
#[tokio::test]
async fn legacy_execute_uses_context_workdir_and_dry_run() {
    let dir = tempfile::tempdir().unwrap();
    let program = stub_bd(dir.path(), &default_children(), "exit 9");
    let handler = BeadsSelectHandler::new(BeadsAdapter::new().with_program(&program));
    let context = Context::new();
    context.set("dry_run", Value::Bool(true)).await;
    context
        .set("workdir", Value::String(dir.path().display().to_string()))
        .await;
    let outcome = handler
        .execute(
            &select_node("e", None, None),
            &context,
            &crate::handlers::tests::make_minimal_graph(),
        )
        .await
        .unwrap();
    assert_eq!(outcome.preferred_label.as_deref(), Some("MORE"));
}

const RUN_ID: &str = "0192f3c4-5a6b-7c8d-9e0f-1a2b3c4d5e6f";

fn select_graph(epic: &str) -> PipelineGraph {
    let dot = format!(
        r#"digraph G {{
            start [shape="Mdiamond"]
            pick  [type="beads.select", epic="{epic}"]
            more  [shape="diamond"]
            done  [shape="Msquare"]
            start -> pick
            pick -> more [label="MORE"]
            pick -> done [label="DONE"]
            pick -> done [label="BLOCKED"]
            more -> done
        }}"#
    );
    PipelineGraph::from_dot(attractor_dot::parse(&dot).unwrap()).unwrap()
}

async fn run_select(
    adapter: BeadsAdapter,
    epic: &str,
    workdir: &Path,
) -> (
    Result<crate::PipelineResult>,
    Vec<attractor_journal::JournalEvent>,
) {
    let tmp = tempfile::tempdir().unwrap();
    let run_dir = tmp.path().join("runs").join(RUN_ID);
    let journal = JournalWriter::open(&run_dir, RUN_ID, 1).unwrap();
    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(ExitHandler);
    registry.register(ConditionalHandler);
    registry.register(BeadsSelectHandler::new(adapter));
    let context = Context::new();
    context
        .set("workdir", Value::String(workdir.display().to_string()))
        .await;
    let result = PipelineExecutor::new(registry)
        .with_journal(journal)
        .run_with_checkpoint(&select_graph(epic), context, &tmp.path().join("logs"))
        .await;
    let events = attractor_journal::read_all(run_dir.join(EVENTS_FILE)).unwrap();
    (result, events)
}

// AC5 + AC4 through the engine: events are journaled inside the stage, the
// MORE edge is taken and task.id reaches the Run context.
#[tokio::test]
async fn engine_journals_snapshot_and_claim_and_routes_more() {
    let dir = tempfile::tempdir().unwrap();
    let program = stub_bd(dir.path(), &default_children(), &default_update());
    let (result, journal) =
        run_select(BeadsAdapter::new().with_program(&program), "e", dir.path()).await;
    let result = result.unwrap();

    assert_eq!(result.completed_nodes, ["start", "pick", "more", "done"]);
    assert_eq!(result.final_context["task.id"], "e.1");
    assert_eq!(
        result.node_outcomes["pick"].preferred_label.as_deref(),
        Some("MORE")
    );
    let pick: Vec<&str> = journal
        .iter()
        .filter(|e| match &e.data {
            EventData::StageStarted { node_id, .. } | EventData::StageCompleted { node_id, .. } => {
                node_id == "pick"
            }
            EventData::EpicSnapshot { .. } | EventData::TaskClaimed { .. } => true,
            _ => false,
        })
        .map(|e| e.data.type_name())
        .collect();
    assert_eq!(
        pick,
        [
            "StageStarted",
            "EpicSnapshot",
            "TaskClaimed",
            "StageCompleted"
        ]
    );
    assert!(journal.iter().any(|e| matches!(
        &e.data,
        EventData::TaskClaimed { task_id, node_id, .. } if task_id == "e.1" && node_id == "pick"
    )));
}

// AC8 through the engine: the Run fails and StageFailed names the Epic.
#[tokio::test]
async fn engine_unknown_epic_fails_the_run_naming_the_id() {
    let dir = tempfile::tempdir().unwrap();
    let program = stub_bd(dir.path(), &default_children(), &default_update());
    let (result, journal) = run_select(
        BeadsAdapter::new().with_program(&program),
        "t-nope",
        dir.path(),
    )
    .await;

    let err = result.unwrap_err().to_string();
    assert!(err.contains("t-nope"), "{err}");
    let stage_failed = journal
        .iter()
        .find_map(|e| match &e.data {
            EventData::StageFailed { node_id, error } if node_id == "pick" => Some(error.clone()),
            _ => None,
        })
        .expect("StageFailed for pick");
    assert!(stage_failed.contains("epic 't-nope'"), "{stage_failed}");
}

// ---------------------------------------------------------------------------
// Real Beads workspace (skips without bd unless PAS_REQUIRE_BD=1)
// ---------------------------------------------------------------------------

async fn create(
    bd: &BeadsAdapter,
    title: &str,
    kind: &str,
    priority: &str,
    parent: Option<&str>,
) -> String {
    let description = format!("description of {title}");
    bd.create(&NewIssue {
        title,
        issue_type: kind,
        priority: Some(priority),
        description: &description,
        parent,
        ..Default::default()
    })
    .await
    .unwrap()
}

struct Picked {
    outcome: Outcome,
    events: Vec<PipelineEvent>,
}

async fn pick(
    bd: &BeadsAdapter,
    workdir: &Path,
    epic: &str,
    order: Option<&str>,
    exclude: Option<&str>,
) -> Picked {
    let events = EventLog::default();
    let outcome = BeadsSelectHandler::new(bd.clone())
        .select(
            &select_node(epic, order, exclude),
            false,
            workdir,
            Some(&events),
        )
        .await
        .unwrap();
    Picked {
        outcome,
        events: events.take(),
    }
}

fn claimed(picked: &Picked) -> String {
    assert_eq!(picked.outcome.preferred_label.as_deref(), Some("MORE"));
    assert_eq!(names(&picked.events), ["EpicSnapshot", "TaskClaimed"]);
    picked.outcome.context_updates["task.id"]
        .as_str()
        .unwrap()
        .to_string()
}

// AC1, AC2, AC3, AC4, AC5, AC6, AC9 against real bd.
#[tokio::test]
async fn real_beads_order_priority_exclude_claim_and_done() {
    let Some((dir, bd)) = workspace().await else {
        return;
    };
    let workdir = dir.path();

    // Unrelated ready issue with the best priority, and the Epics themselves,
    // are in `bd ready` but must never be claimed (AC9).
    let unrelated = create(&bd, "Unrelated", "task", "0", None).await;

    // AC1: order "C,A" → C, A, then B.
    let e1 = create(&bd, "Ordered epic", "epic", "2", None).await;
    let a = create(&bd, "Task A", "task", "2", Some(&e1)).await;
    let b = create(&bd, "Task B", "task", "1", Some(&e1)).await;
    let c = create(&bd, "Task C", "task", "2", Some(&e1)).await;
    let ready_ids: Vec<String> = bd
        .ready()
        .await
        .unwrap()
        .into_iter()
        .map(|i| i.id)
        .collect();
    assert!(ready_ids.contains(&unrelated) && ready_ids.contains(&e1));

    let order = format!("{c},{a}");
    let mut sequence = Vec::new();
    for _ in 0..3 {
        let picked = pick(&bd, workdir, &e1, Some(&order), None).await;
        let id = claimed(&picked);

        // AC4: bd sees the claim; handoff and context carry the Task.
        let shown = bd.show(&id).await.unwrap();
        assert_eq!(shown.status, "in_progress");
        assert_eq!(shown.assignee.as_deref(), Some(TEST_ACTOR));
        let handoff = std::fs::read_to_string(workdir.join(CURRENT_TASK_FILE)).unwrap();
        assert!(handoff.contains(&format!("- **ID:** {id}")), "{handoff}");
        assert!(handoff.contains(&shown.title), "{handoff}");
        assert!(
            handoff.contains(&format!("description of {}", shown.title)),
            "{handoff}"
        );
        assert_eq!(picked.outcome.context_updates["task.title"], shown.title);

        // AC5: the snapshot lists every child with its status.
        let EventData::EpicSnapshot { epic_id, tasks, .. } = picked.events[0].to_journal_data()
        else {
            unreachable!()
        };
        assert_eq!(epic_id, e1);
        assert_eq!(tasks.len(), 3);
        let EventData::TaskClaimed {
            task_id, epic_id, ..
        } = picked.events[1].to_journal_data()
        else {
            unreachable!()
        };
        assert_eq!((task_id, epic_id), (id.clone(), e1.clone()));
        sequence.push(id);
    }
    assert_eq!(sequence, [c.clone(), a.clone(), b.clone()]);

    // Every child claimed, none closed: BLOCKED, not DONE.
    let picked = pick(&bd, workdir, &e1, Some(&order), None).await;
    assert_eq!(picked.outcome.preferred_label.as_deref(), Some("BLOCKED"));

    // AC6: all children closed → DONE, nothing claimed.
    for id in [&a, &b, &c] {
        bd.close(id, Some("test")).await.unwrap();
    }
    let before = bd.show(&unrelated).await.unwrap();
    let picked = pick(&bd, workdir, &e1, None, None).await;
    assert_eq!(picked.outcome.preferred_label.as_deref(), Some("DONE"));
    assert_eq!(names(&picked.events), ["EpicSnapshot"]);
    assert!(picked.outcome.context_updates.is_empty());
    for id in [&a, &b, &c] {
        assert_eq!(bd.show(id).await.unwrap().status, "closed");
    }

    // AC2 + AC3: no order → priority then ID; the excluded child is skipped.
    let e2 = create(&bd, "Priority epic", "epic", "2", None).await;
    let p2 = create(&bd, "P2 first created", "task", "2", Some(&e2)).await;
    let p1_low = create(&bd, "P1 lower id", "task", "1", Some(&e2)).await;
    let p1_high = create(&bd, "P1 higher id", "task", "1", Some(&e2)).await;
    let skipped = create(&bd, "P0 excluded", "task", "0", Some(&e2)).await;
    let (first_p1, second_p1) = if natural_cmp(&p1_low, &p1_high) == Ordering::Less {
        (p1_low, p1_high)
    } else {
        (p1_high, p1_low)
    };
    let exclude = skipped.clone();
    let mut sequence = Vec::new();
    for _ in 0..3 {
        sequence.push(claimed(
            &pick(&bd, workdir, &e2, Some(&skipped), Some(&exclude)).await,
        ));
    }
    assert_eq!(sequence, [first_p1, second_p1, p2]);
    let picked = pick(&bd, workdir, &e2, Some(&skipped), Some(&exclude)).await;
    assert_eq!(picked.outcome.preferred_label.as_deref(), Some("BLOCKED"));
    let shown = bd.show(&skipped).await.unwrap();
    assert_eq!(shown.status, "open");
    assert_eq!(shown.assignee, None);

    // AC9: the unrelated ready issue and the Epics were never claimed.
    assert_eq!(bd.show(&unrelated).await.unwrap(), before);
    assert_eq!(before.status, "open");
    for epic in [&e1, &e2] {
        assert_eq!(bd.show(epic).await.unwrap().assignee, None);
    }
}

// AC7, AC8 and the engine path against real bd.
#[tokio::test]
async fn real_beads_blocked_unknown_epic_and_engine_run() {
    let Some((dir, bd)) = workspace().await else {
        return;
    };
    let workdir = dir.path();

    // AC7: T1 is blocked by an open issue outside the Epic, T2 by T1.
    let outside = create(&bd, "Outside blocker", "task", "1", None).await;
    let e = create(&bd, "Blocked epic", "epic", "1", None).await;
    let t1 = create(&bd, "T1", "task", "1", Some(&e)).await;
    let t2 = create(&bd, "T2", "task", "1", Some(&e)).await;
    bd.add_dependency(&t1, &outside).await.unwrap();
    bd.add_dependency(&t2, &t1).await.unwrap();

    let picked = pick(&bd, workdir, &e, None, None).await;
    assert_eq!(picked.outcome.preferred_label.as_deref(), Some("BLOCKED"));
    assert_eq!(
        names(&picked.events),
        ["EpicSnapshot", "TaskSelectionBlocked"]
    );
    let EventData::TaskSelectionBlocked {
        epic_id,
        mut open,
        blocked_by,
    } = picked.events[1].to_journal_data()
    else {
        unreachable!()
    };
    open.sort();
    let mut expected_open = vec![t1.clone(), t2.clone()];
    expected_open.sort();
    assert_eq!(epic_id, e);
    assert_eq!(open, expected_open);
    assert_eq!(
        blocked_by,
        BTreeMap::from([
            (t1.clone(), vec![outside.clone()]),
            (t2.clone(), vec![t1.clone()])
        ])
    );
    for id in [&t1, &t2] {
        let shown = bd.show(id).await.unwrap();
        assert_eq!((shown.status.as_str(), shown.assignee), ("open", None));
    }
    assert!(!workdir.join(CURRENT_TASK_FILE).exists());

    // AC8: an unknown Epic fails with a message naming the ID.
    let err = BeadsSelectHandler::new(bd.clone())
        .select(&select_node("t-nope", None, None), false, workdir, None)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("t-nope"), "{err}");

    // Engine path: unblocking T1 lets a journaled Run claim it and take MORE.
    bd.close(&outside, None).await.unwrap();
    let (result, journal) = run_select(bd.clone(), &e, workdir).await;
    let result = result.unwrap();
    assert_eq!(result.final_context["task.id"], Value::String(t1.clone()));
    assert!(result.completed_nodes.contains(&"more".to_string()));
    assert!(journal.iter().any(|ev| matches!(
        &ev.data,
        EventData::TaskClaimed { task_id, .. } if *task_id == t1
    )));
    assert_eq!(bd.show(&t1).await.unwrap().status, "in_progress");
}
