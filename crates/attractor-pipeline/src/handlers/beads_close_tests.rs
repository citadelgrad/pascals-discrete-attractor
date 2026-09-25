use super::*;

use std::path::PathBuf;
use std::sync::Mutex;

use attractor_journal::{CommitRef, EventData, JournalEvent, JournalWriter, EVENTS_FILE};
use attractor_types::StageStatus;

use crate::beads_adapter::test_support::{stub, workspace};
use crate::beads_adapter::NewIssue;
use crate::engine::PipelineExecutor;
use crate::handler::{ConditionalHandler, ExitHandler, HandlerRegistry, StartHandler};
use crate::handlers::tests::make_node;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const RUN_ID: &str = "0192f3c4-5a6b-7c8d-9e0f-1a2b3c4d5e70";
const TASK: &str = "e.1";

#[derive(Default)]
struct EventLog(Mutex<Vec<PipelineEvent>>);

impl EventSink for EventLog {
    fn emit(&self, event: PipelineEvent) {
        self.0.lock().unwrap().push(event);
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.com",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn head(dir: &Path) -> String {
    git(dir, &["rev-parse", "HEAD"])
}

/// A repository with one commit; with `remote`, `main` tracks a bare
/// `origin` that has that commit.
struct Repo {
    _tmp: tempfile::TempDir,
    work: PathBuf,
}

fn repo(remote: bool) -> Repo {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    git(&work, &["commit", "--allow-empty", "-qm", "initial"]);
    if remote {
        add_remote(tmp.path(), &work);
    }
    Repo { _tmp: tmp, work }
}

fn add_remote(parent: &Path, work: &Path) {
    let remote = parent.join("remote.git");
    git(parent, &["init", "-q", "--bare", remote.to_str().unwrap()]);
    git(work, &["remote", "add", "origin", remote.to_str().unwrap()]);
    git(work, &["push", "-q", "-u", "origin", "HEAD"]);
}

/// A fake Beads program that logs its arguments to `calls.log`; `close`
/// answers with `close` (a shell snippet).
fn stub_bd(dir: &Path, close: &str) -> PathBuf {
    let log = dir.join("calls.log");
    stub(
        dir,
        "fake-beads",
        &format!(
            r#"printf '%s\n' "$*" >> '{log}'
case "$1" in
  close) {close} ;;
  *) echo "unexpected $*" >&2; exit 2 ;;
esac"#,
            log = log.display()
        ),
    )
}

fn closing_stub(dir: &Path) -> PathBuf {
    stub_bd(
        dir,
        r#"echo "[{\"id\":\"$2\",\"title\":\"One\",\"status\":\"closed\"}]""#,
    )
}

fn calls(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("calls.log")).unwrap_or_default()
}

fn task_keys(context: &HashMap<String, Value>) -> Vec<String> {
    let mut keys: Vec<String> = context
        .keys()
        .filter(|key| key.starts_with("task."))
        .cloned()
        .collect();
    keys.sort();
    keys
}

async fn claimed_context(task_id: Option<Value>) -> Context {
    let context = Context::new();
    if let Some(id) = task_id {
        context.set("task.id", id).await;
    }
    for key in ["title", "description", "acceptance", "design"] {
        context
            .set(
                format!("task.{key}"),
                Value::String(format!("{key} of one")),
            )
            .await;
    }
    context.set("keep", Value::String("me".into())).await;
    context
}

fn close_node(attrs: &[(&str, AttributeValue)]) -> PipelineNode {
    make_node(
        "close_task",
        "box",
        None,
        attrs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
    )
}

/// A Run folder whose journal holds `CommitsCreated` Events for `(task, shas)`.
fn journal_with(commits: &[(Option<&str>, &[&str])]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let journal = JournalWriter::open(dir.path(), RUN_ID, 1).unwrap();
    for (task_id, shas) in commits {
        journal
            .append(EventData::CommitsCreated {
                node_id: "work".into(),
                task_id: task_id.map(String::from),
                commits: shas
                    .iter()
                    .map(|sha| CommitRef {
                        sha: sha.to_string(),
                        subject: "s".into(),
                        author: "T".into(),
                        ts: "2026-09-25T10:00:00+00:00".into(),
                    })
                    .collect(),
            })
            .unwrap();
    }
    dir
}

/// A stage that runs git commands in the workdir, standing in for an agent
/// that commits (and maybe pushes).
struct GitStage {
    workdir: PathBuf,
    steps: Vec<Vec<String>>,
}

#[async_trait]
impl NodeHandler for GitStage {
    fn handler_type(&self) -> &str {
        "test.git"
    }

    async fn execute(
        &self,
        _node: &PipelineNode,
        _context: &Context,
        _graph: &PipelineGraph,
    ) -> Result<Outcome> {
        for step in &self.steps {
            let args: Vec<&str> = step.iter().map(String::as_str).collect();
            git(&self.workdir, &args);
        }
        Ok(Outcome::success("git stage"))
    }
}

fn commit(subject: &str) -> Vec<String> {
    ["commit", "--allow-empty", "-qm", subject]
        .map(String::from)
        .to_vec()
}

fn push() -> Vec<String> {
    vec!["push".into(), "-q".into()]
}

fn close_graph(close_attrs: &str) -> PipelineGraph {
    let dot = format!(
        r#"digraph G {{
            start      [shape="Mdiamond"]
            work       [type="test.git"]
            close_task [type="beads.close"{close_attrs}]
            failed     [shape="diamond"]
            done       [shape="Msquare"]
            start -> work -> close_task
            close_task -> done   [condition="outcome=success"]
            close_task -> failed [condition="outcome=fail"]
            failed -> done
        }}"#
    );
    PipelineGraph::from_dot(attractor_dot::parse(&dot).unwrap()).unwrap()
}

struct CloseRun {
    result: Result<crate::PipelineResult>,
    journal: Vec<JournalEvent>,
}

impl CloseRun {
    fn task_closed(&self) -> Vec<EventData> {
        self.journal
            .iter()
            .filter(|e| matches!(e.data, EventData::TaskClosed { .. }))
            .map(|e| e.data.clone())
            .collect()
    }

    /// Journal Event types of the `close_task` stage, in order.
    fn close_stage(&self) -> Vec<&str> {
        self.journal
            .iter()
            .filter(|e| match &e.data {
                EventData::StageStarted { node_id, .. }
                | EventData::StageCompleted { node_id, .. }
                | EventData::StageFailed { node_id, .. } => node_id == "close_task",
                EventData::TaskClosed { .. } => true,
                _ => false,
            })
            .map(|e| e.data.type_name())
            .collect()
    }
}

/// A journaled Run: `work` runs `steps`, then `close_task` closes the Task
/// claimed in `context`.
async fn run_close(
    adapter: BeadsAdapter,
    workdir: &Path,
    steps: Vec<Vec<String>>,
    close_attrs: &str,
    context: Context,
) -> CloseRun {
    let tmp = tempfile::tempdir().unwrap();
    let run_dir = tmp.path().join("runs").join(RUN_ID);
    let journal = JournalWriter::open(&run_dir, RUN_ID, 1).unwrap();
    let mut registry = HandlerRegistry::new();
    registry.register(StartHandler);
    registry.register(ExitHandler);
    registry.register(ConditionalHandler);
    registry.register(GitStage {
        workdir: workdir.to_path_buf(),
        steps,
    });
    registry.register(BeadsCloseHandler::new(adapter));
    context
        .set("workdir", Value::String(workdir.display().to_string()))
        .await;
    let result = PipelineExecutor::new(registry)
        .with_journal(journal)
        .run_with_checkpoint(&close_graph(close_attrs), context, &tmp.path().join("logs"))
        .await;
    let journal = attractor_journal::read_all(run_dir.join(EVENTS_FILE)).unwrap();
    CloseRun { result, journal }
}

// ---------------------------------------------------------------------------
// Acceptance criteria through the engine (stub bd)
// ---------------------------------------------------------------------------

// AC1: every Task commit on upstream → closed in bd, TaskClosed verified with
// the SHAs, task.* removed (other keys kept). The reason template expands.
#[tokio::test]
async fn engine_closes_task_when_all_commits_are_pushed() {
    let repo = repo(true);
    let bd_dir = tempfile::tempdir().unwrap();
    let program = closing_stub(bd_dir.path());
    let before = head(&repo.work);

    let run = run_close(
        BeadsAdapter::new().with_program(&program),
        &repo.work,
        vec![commit("one"), commit("two"), push()],
        r#", reason="Done ${task.id} (${task.title}): ${commits} on ${upstream}""#,
        claimed_context(Some(Value::String(TASK.into()))).await,
    )
    .await;
    let result = run.result.as_ref().unwrap();

    let expected: Vec<String> = git(
        &repo.work,
        &["log", "--format=%H", &format!("{before}..HEAD")],
    )
    .lines()
    .map(String::from)
    .collect();
    assert_eq!(expected.len(), 2);
    let reason = format!(
        "Done e.1 (title of one): {} on origin/main",
        expected.join(", ")
    );
    assert_eq!(
        run.task_closed(),
        [EventData::TaskClosed {
            task_id: TASK.into(),
            reason: reason.clone(),
            upstream_verified: true,
            commits: expected,
        }]
    );
    assert_eq!(
        run.close_stage(),
        ["StageStarted", "TaskClosed", "StageCompleted"]
    );
    assert_eq!(
        calls(bd_dir.path()),
        format!("close e.1 --reason {reason} --json\n")
    );
    assert!(
        task_keys(&result.final_context).is_empty(),
        "{:?}",
        result.final_context
    );
    assert_eq!(result.final_context["keep"], "me");
    assert!(!result.completed_nodes.contains(&"failed".to_string()));
}

// AC2: one Task commit not on upstream → fail, bd untouched, task.* kept.
#[tokio::test]
async fn engine_unpushed_commit_fails_and_leaves_task_open() {
    let repo = repo(true);
    let bd_dir = tempfile::tempdir().unwrap();
    let program = closing_stub(bd_dir.path());

    let run = run_close(
        BeadsAdapter::new().with_program(&program),
        &repo.work,
        vec![commit("pushed"), push(), commit("local")],
        "",
        claimed_context(Some(Value::String(TASK.into()))).await,
    )
    .await;
    let result = run.result.as_ref().unwrap();
    let local = head(&repo.work);

    let outcome = &result.node_outcomes["close_task"];
    assert_eq!(outcome.status, StageStatus::Fail);
    let why = outcome.failure_reason.as_deref().unwrap();
    assert!(why.contains("task e.1 left open"), "{why}");
    assert!(why.contains("1 commit(s) not on origin/main"), "{why}");
    assert!(why.contains(&local[..12]), "{why}");
    assert!(result.completed_nodes.contains(&"failed".to_string()));
    assert_eq!(calls(bd_dir.path()), "");
    assert!(run.task_closed().is_empty());
    assert_eq!(result.final_context["task.id"], TASK);
    assert_eq!(task_keys(&result.final_context).len(), 5);
}

// AC3: require_upstream=false closes with an unpushed commit, unverified.
#[tokio::test]
async fn engine_require_upstream_false_closes_unpushed_commit_unverified() {
    for attr in [", require_upstream=false", r#", require_upstream="false""#] {
        let repo = repo(true);
        let bd_dir = tempfile::tempdir().unwrap();
        let program = closing_stub(bd_dir.path());

        let run = run_close(
            BeadsAdapter::new().with_program(&program),
            &repo.work,
            vec![commit("local")],
            attr,
            claimed_context(Some(Value::String(TASK.into()))).await,
        )
        .await;
        let result = run.result.as_ref().unwrap();

        let reason = "Closed by PAS (close_task): 1 Run Commit(s) not verified on upstream";
        assert_eq!(
            run.task_closed(),
            [EventData::TaskClosed {
                task_id: TASK.into(),
                reason: reason.into(),
                upstream_verified: false,
                commits: vec![head(&repo.work)],
            }],
            "{attr}"
        );
        assert_eq!(
            calls(bd_dir.path()),
            format!("close e.1 --reason {reason} --json\n")
        );
        assert!(
            task_keys(&result.final_context).is_empty(),
            "{:?}",
            result.final_context
        );
    }
}

// D4: with require_upstream=false a fully pushed Task still reports verified.
#[tokio::test]
async fn engine_require_upstream_false_reports_verified_when_pushed() {
    let repo = repo(true);
    let bd_dir = tempfile::tempdir().unwrap();
    let program = closing_stub(bd_dir.path());
    let run = run_close(
        BeadsAdapter::new().with_program(&program),
        &repo.work,
        vec![commit("one"), push()],
        ", require_upstream=false",
        claimed_context(Some(Value::String(TASK.into()))).await,
    )
    .await;
    run.result.as_ref().unwrap();
    let closed = run.task_closed();
    let [EventData::TaskClosed {
        upstream_verified,
        commits,
        ..
    }] = closed.as_slice()
    else {
        panic!("one TaskClosed expected: {:?}", run.task_closed());
    };
    assert!(upstream_verified);
    assert_eq!(commits, &[head(&repo.work)]);
}

// AC4: no task.id → the stage fails saying no Task is claimed; bd not run.
#[tokio::test]
async fn engine_without_claimed_task_fails_naming_it() {
    let repo = repo(true);
    let bd_dir = tempfile::tempdir().unwrap();
    let program = closing_stub(bd_dir.path());

    let run = run_close(
        BeadsAdapter::new().with_program(&program),
        &repo.work,
        vec![],
        "",
        claimed_context(None).await,
    )
    .await;

    let err = run.result.as_ref().err().unwrap().to_string();
    assert!(err.contains("no Task is claimed"), "{err}");
    let stage_failed = run
        .journal
        .iter()
        .find_map(|e| match &e.data {
            EventData::StageFailed { node_id, error } if node_id == "close_task" => {
                Some(error.clone())
            }
            _ => None,
        })
        .expect("StageFailed for close_task");
    assert!(
        stage_failed.contains("no Task is claimed"),
        "{stage_failed}"
    );
    assert_eq!(calls(bd_dir.path()), "");
    assert!(run.task_closed().is_empty());
}

// AC4 boundaries: an empty, blank, or non-string task.id is no Task.
#[tokio::test]
async fn empty_or_non_string_task_id_is_no_claimed_task() {
    let bd_dir = tempfile::tempdir().unwrap();
    let program = closing_stub(bd_dir.path());
    let handler = BeadsCloseHandler::new(BeadsAdapter::new().with_program(&program));
    let run_dir = journal_with(&[]);
    for value in [
        Value::String("".into()),
        Value::String("  ".into()),
        Value::from(7),
    ] {
        let context = claimed_context(Some(value.clone())).await;
        let err = handler
            .close(
                &close_node(&[]),
                &context,
                false,
                bd_dir.path(),
                Some(run_dir.path()),
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AttractorError::HandlerError { .. }));
        let message = err.to_string();
        assert!(message.contains("no Task is claimed"), "{value}: {message}");
        assert!(message.contains("beads.close"), "{message}");
    }
    assert_eq!(calls(bd_dir.path()), "");
}

// AC5: no upstream and require_upstream=true → fail, Task left open.
#[tokio::test]
async fn engine_without_upstream_fails_and_leaves_task_open() {
    let repo = repo(false);
    let bd_dir = tempfile::tempdir().unwrap();
    let program = closing_stub(bd_dir.path());

    let run = run_close(
        BeadsAdapter::new().with_program(&program),
        &repo.work,
        vec![commit("local")],
        ", require_upstream=true",
        claimed_context(Some(Value::String(TASK.into()))).await,
    )
    .await;
    let result = run.result.as_ref().unwrap();

    let outcome = &result.node_outcomes["close_task"];
    assert_eq!(outcome.status, StageStatus::Fail);
    let why = outcome.failure_reason.as_deref().unwrap();
    assert!(why.contains("task e.1 left open"), "{why}");
    assert!(why.contains("no upstream"), "{why}");
    assert_eq!(calls(bd_dir.path()), "");
    assert!(run.task_closed().is_empty());
    assert_eq!(result.final_context["task.id"], TASK);
}

// AC5 boundaries: a detached HEAD, or a Task with no commits, still needs an
// upstream when require_upstream=true.
#[tokio::test]
async fn detached_head_or_zero_commits_without_upstream_fails() {
    let bd_dir = tempfile::tempdir().unwrap();
    let program = closing_stub(bd_dir.path());
    let handler = BeadsCloseHandler::new(BeadsAdapter::new().with_program(&program));
    let run_dir = journal_with(&[]);

    let detached = repo(true);
    git(&detached.work, &["checkout", "-q", "--detach"]);
    let no_remote = repo(false);
    for workdir in [&detached.work, &no_remote.work] {
        let context = claimed_context(Some(Value::String(TASK.into()))).await;
        let outcome = handler
            .close(
                &close_node(&[]),
                &context,
                false,
                workdir,
                Some(run_dir.path()),
                None,
            )
            .await
            .unwrap();
        assert_eq!(outcome.status, StageStatus::Fail);
        assert!(outcome.failure_reason.unwrap().contains("no upstream"));
        assert_eq!(context.get("task.id").await.unwrap(), TASK);
    }
    assert_eq!(calls(bd_dir.path()), "");
}

// AC6: a Task with no commits closes verified with an empty list.
#[tokio::test]
async fn engine_task_without_commits_closes_verified_with_empty_list() {
    let repo = repo(true);
    let bd_dir = tempfile::tempdir().unwrap();
    let program = closing_stub(bd_dir.path());

    let run = run_close(
        BeadsAdapter::new().with_program(&program),
        &repo.work,
        vec![],
        "",
        claimed_context(Some(Value::String(TASK.into()))).await,
    )
    .await;
    let result = run.result.as_ref().unwrap();

    let reason = "Closed by PAS (close_task): 0 Run Commit(s) on origin/main";
    assert_eq!(
        run.task_closed(),
        [EventData::TaskClosed {
            task_id: TASK.into(),
            reason: reason.into(),
            upstream_verified: true,
            commits: vec![],
        }]
    );
    assert_eq!(
        calls(bd_dir.path()),
        format!("close e.1 --reason {reason} --json\n")
    );
    assert!(
        task_keys(&result.final_context).is_empty(),
        "{:?}",
        result.final_context
    );
}

// AC6 + attribution: unpushed commits of other Tasks, or of no Task, are
// ignored; only this Task's commits count.
#[tokio::test]
async fn commits_of_other_tasks_are_ignored() {
    let repo = repo(true);
    git(&repo.work, &["commit", "--allow-empty", "-qm", "other"]);
    let other = head(&repo.work);
    let bd_dir = tempfile::tempdir().unwrap();
    let program = closing_stub(bd_dir.path());
    let handler = BeadsCloseHandler::new(BeadsAdapter::new().with_program(&program));
    let run_dir = journal_with(&[(Some("e.9"), &[&other]), (None, &[&other])]);
    let events = EventLog::default();
    let context = claimed_context(Some(Value::String(TASK.into()))).await;

    let outcome = handler
        .close(
            &close_node(&[]),
            &context,
            false,
            &repo.work,
            Some(run_dir.path()),
            Some(&events),
        )
        .await
        .unwrap();

    assert_eq!(outcome.status, StageStatus::Success);
    let events = events.0.lock().unwrap();
    assert!(matches!(
        &events[..],
        [PipelineEvent::TaskClosed { upstream_verified: true, commits, .. }] if commits.is_empty()
    ));
}

// ---------------------------------------------------------------------------
// Other behaviour
// ---------------------------------------------------------------------------

#[test]
fn task_commits_filters_by_task_and_deduplicates() {
    let run_dir = journal_with(&[
        (Some("e.1"), &["b", "a"]),
        (Some("e.2"), &["x"]),
        (None, &["y"]),
        (Some("e.1"), &["c", "a"]),
    ]);
    let path = run_dir.path().join(EVENTS_FILE);
    assert_eq!(task_commits(&path, "e.1").unwrap(), ["b", "a", "c"]);
    assert_eq!(task_commits(&path, "e.2").unwrap(), ["x"]);
    assert!(task_commits(&path, "e.3").unwrap().is_empty());
    assert!(task_commits(&run_dir.path().join("missing.jsonl"), "e.1").is_err());
}

#[test]
fn close_attributes_are_parsed_and_validated() {
    let parse = |attrs: &[(&str, AttributeValue)]| CloseAttrs::from_node(&close_node(attrs));
    assert_eq!(
        parse(&[]).unwrap(),
        CloseAttrs {
            require_upstream: true,
            reason: None
        }
    );
    for (value, expected) in [
        (AttributeValue::Boolean(false), false),
        (AttributeValue::Boolean(true), true),
        (AttributeValue::String("false".into()), false),
        (AttributeValue::String(" true ".into()), true),
    ] {
        let attrs = parse(&[("require_upstream", value)]).unwrap();
        assert_eq!(attrs.require_upstream, expected);
    }
    for bad in [
        AttributeValue::String("no".into()),
        AttributeValue::Integer(0),
    ] {
        let err = parse(&[("require_upstream", bad)]).unwrap_err();
        assert!(err.contains("require_upstream"), "{err}");
    }
    let attrs = parse(&[("reason", AttributeValue::String("r ${task.id}".into()))]).unwrap();
    assert_eq!(attrs.reason.as_deref(), Some("r ${task.id}"));
    assert!(parse(&[("reason", AttributeValue::Integer(1))])
        .unwrap_err()
        .contains("reason"));
}

// A blank reason template falls back to the default reason.
#[tokio::test]
async fn blank_reason_uses_the_default() {
    let repo = repo(true);
    let bd_dir = tempfile::tempdir().unwrap();
    let program = closing_stub(bd_dir.path());
    let handler = BeadsCloseHandler::new(BeadsAdapter::new().with_program(&program));
    let run_dir = journal_with(&[]);
    let context = claimed_context(Some(Value::String(TASK.into()))).await;
    handler
        .close(
            &close_node(&[("reason", AttributeValue::String("  ".into()))]),
            &context,
            false,
            &repo.work,
            Some(run_dir.path()),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        calls(bd_dir.path()),
        "close e.1 --reason Closed by PAS (close_task): 0 Run Commit(s) on origin/main --json\n"
    );
}

// D2: dry run runs no git or bd, emits nothing and keeps task.*.
#[tokio::test]
async fn dry_run_does_not_close() {
    let bd_dir = tempfile::tempdir().unwrap();
    let program = closing_stub(bd_dir.path());
    let handler = BeadsCloseHandler::new(BeadsAdapter::new().with_program(&program));
    let events = EventLog::default();
    let context = claimed_context(Some(Value::String(TASK.into()))).await;
    let outcome = handler
        .close(
            &close_node(&[]),
            &context,
            true,
            &bd_dir.path().join("not-a-repo"),
            None,
            Some(&events),
        )
        .await
        .unwrap();
    assert_eq!(outcome.status, StageStatus::Success);
    assert!(outcome.notes.contains("dry run: would close e.1"));
    assert_eq!(calls(bd_dir.path()), "");
    assert!(events.0.lock().unwrap().is_empty());
    assert_eq!(context.get("task.id").await.unwrap(), TASK);
}

// A failing `bd close` fails the stage naming the Task; nothing is emitted
// and task.* is kept.
#[tokio::test]
async fn failed_bd_close_fails_the_stage() {
    let repo = repo(true);
    let bd_dir = tempfile::tempdir().unwrap();
    let program = stub_bd(bd_dir.path(), "echo 'Error: database locked' >&2; exit 1");
    let handler = BeadsCloseHandler::new(BeadsAdapter::new().with_program(&program));
    let run_dir = journal_with(&[]);
    let events = EventLog::default();
    let context = claimed_context(Some(Value::String(TASK.into()))).await;
    let err = handler
        .close(
            &close_node(&[]),
            &context,
            false,
            &repo.work,
            Some(run_dir.path()),
            Some(&events),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("cannot close task 'e.1'"), "{err}");
    assert!(err.contains("database locked"), "{err}");
    assert!(events.0.lock().unwrap().is_empty());
    assert_eq!(context.get("task.id").await.unwrap(), TASK);
}

// D3: without a Run Journal (legacy `execute`) the commits are unknown:
// require_upstream=true fails closed, false closes unverified.
#[tokio::test]
async fn without_run_journal_commits_are_unknown() {
    let repo = repo(true);
    let bd_dir = tempfile::tempdir().unwrap();
    let program = closing_stub(bd_dir.path());
    let handler = BeadsCloseHandler::new(BeadsAdapter::new().with_program(&program));
    let graph = crate::handlers::tests::make_minimal_graph();
    let context = claimed_context(Some(Value::String(TASK.into()))).await;
    context
        .set("workdir", Value::String(repo.work.display().to_string()))
        .await;

    let outcome = handler
        .execute(&close_node(&[]), &context, &graph)
        .await
        .unwrap();
    assert_eq!(outcome.status, StageStatus::Fail);
    assert!(outcome
        .failure_reason
        .unwrap()
        .contains("no Run Journal records its commits"));
    assert_eq!(calls(bd_dir.path()), "");

    let node = close_node(&[("require_upstream", AttributeValue::Boolean(false))]);
    let outcome = handler.execute(&node, &context, &graph).await.unwrap();
    assert_eq!(outcome.status, StageStatus::Success);
    assert!(calls(bd_dir.path()).contains("not verified on upstream"));
    assert!(task_keys(&context.snapshot().await).is_empty());
    assert_eq!(context.get("keep").await.unwrap(), "me");
}

// ---------------------------------------------------------------------------
// Real Beads workspace (skips without bd unless PAS_REQUIRE_BD=1)
// ---------------------------------------------------------------------------

// AC1, AC2, AC3, AC5 against real bd, through journaled engine Runs.
#[tokio::test]
async fn real_beads_close_follows_upstream() {
    let Some((dir, bd)) = workspace().await else {
        return;
    };
    let work = dir.path();
    git(work, &["commit", "--allow-empty", "-qm", "initial"]);
    let create = |title: &'static str, parent: Option<String>| {
        let bd = bd.clone();
        async move {
            bd.create(&NewIssue {
                title,
                issue_type: if parent.is_some() { "task" } else { "epic" },
                priority: Some("1"),
                description: "close test",
                parent: parent.as_deref(),
                ..Default::default()
            })
            .await
            .unwrap()
        }
    };
    let epic = create("Close epic", None).await;
    let t1 = create("T1", Some(epic.clone())).await;
    let t2 = create("T2", Some(epic.clone())).await;
    for task in [&t1, &t2] {
        bd.claim(task).await.unwrap();
    }
    let status = |id: String| {
        let bd = bd.clone();
        async move { bd.show(&id).await.unwrap().status }
    };

    // AC5: no upstream yet → fail, T1 stays in progress.
    let run = run_close(
        bd.clone(),
        work,
        vec![commit("a")],
        "",
        claimed_context(Some(Value::String(t1.clone()))).await,
    )
    .await;
    assert_eq!(
        run.result.as_ref().unwrap().node_outcomes["close_task"].status,
        StageStatus::Fail
    );
    assert_eq!(status(t1.clone()).await, "in_progress");

    // AC2: pushed, then one unpushed commit → fail, T1 stays in progress.
    let remote_parent = tempfile::tempdir().unwrap();
    add_remote(remote_parent.path(), work);
    let run = run_close(
        bd.clone(),
        work,
        vec![commit("b"), push(), commit("c")],
        "",
        claimed_context(Some(Value::String(t1.clone()))).await,
    )
    .await;
    assert_eq!(
        run.result.as_ref().unwrap().node_outcomes["close_task"].status,
        StageStatus::Fail
    );
    assert!(run.task_closed().is_empty());
    assert_eq!(status(t1.clone()).await, "in_progress");

    // AC1: commit and push → T1 closed, verified, with its SHAs.
    let run = run_close(
        bd.clone(),
        work,
        vec![commit("d"), push()],
        "",
        claimed_context(Some(Value::String(t1.clone()))).await,
    )
    .await;
    let result = run.result.as_ref().unwrap();
    assert!(
        task_keys(&result.final_context).is_empty(),
        "{:?}",
        result.final_context
    );
    assert!(matches!(
        run.task_closed().as_slice(),
        [EventData::TaskClosed { task_id, upstream_verified: true, commits, .. }]
            if *task_id == t1 && *commits == [head(work)]
    ));
    let shown = bd.show(&t1).await.unwrap();
    assert_eq!(shown.status, "closed");
    assert!(
        shown
            .close_reason
            .as_deref()
            .unwrap_or_default()
            .contains("1 Run Commit(s) on origin/"),
        "{shown:?}"
    );

    // AC3: require_upstream=false with an unpushed commit → T2 closed, unverified.
    let run = run_close(
        bd.clone(),
        work,
        vec![commit("e")],
        ", require_upstream=false",
        claimed_context(Some(Value::String(t2.clone()))).await,
    )
    .await;
    run.result.as_ref().unwrap();
    assert!(matches!(
        run.task_closed().as_slice(),
        [EventData::TaskClosed { task_id, upstream_verified: false, commits, .. }]
            if *task_id == t2 && *commits == [head(work)]
    ));
    assert_eq!(status(t2.clone()).await, "closed");
}
