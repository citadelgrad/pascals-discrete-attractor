//! The one place that runs the Beads (`bd`) CLI.
//!
//! Every `bd` call in `crates/` goes through [`BeadsAdapter`], which spawns
//! `bd … --json` and parses the output (spec §7). If the tracker changes, this
//! is the only module to change.
//!
//! Each adapter carries its own program, working directory and extra
//! environment variables, so callers (and tests) never mutate the process
//! environment. By default it runs `bd` from `PATH` in the inherited working
//! directory, like the CLI always has.
//!
//! Tests that need a real Beads workspace skip with a notice when `bd` is not
//! on `PATH`, and fail instead when `PAS_REQUIRE_BD=1`. The error-path tests
//! use a stub program and never need `bd`.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::de::DeserializeOwned;
use serde::Deserialize;
use tokio::process::Command;

/// The only literal naming the Beads program in `crates/`.
const BD_PROGRAM: &str = "bd";

/// Whether an executable `bd` is in one of the directories of `path` (a
/// `PATH`-style list). Validation uses this to fail early instead of mid-Run;
/// it never spawns `bd`.
pub fn bd_on_path(path: Option<&OsStr>) -> bool {
    path.is_some_and(|path| {
        std::env::split_paths(path).any(|dir| is_executable(&dir.join(BD_PROGRAM)))
    })
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file() || path.with_extension("exe").is_file()
}

/// An issue as reported by `bd show`, `bd children`, `bd ready`, `bd list`,
/// `bd update` and `bd close`. Unknown fields are ignored; `status` stays a
/// string so a new Beads status never fails parsing.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BeadsIssue {
    pub id: String,
    pub title: String,
    pub status: String,
    #[serde(default)]
    pub priority: Option<i64>,
    #[serde(default)]
    pub issue_type: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub acceptance_criteria: Option<String>,
    #[serde(default)]
    pub design: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub close_reason: Option<String>,
    /// Absent from `bd ready`; `bd show` and `bd children` use different
    /// shapes, both accepted by [`BeadsDependency`].
    #[serde(default)]
    pub dependencies: Vec<BeadsDependency>,
}

/// One dependency of a [`BeadsIssue`]. `bd children` / `bd list` report
/// `{depends_on_id, type}`; `bd show` reports the depended-on issue itself as
/// `{id, dependency_type, …}`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BeadsDependency {
    #[serde(alias = "id")]
    pub depends_on_id: String,
    /// `blocks`, `parent-child`, … (empty when bd omits it).
    #[serde(default, rename = "type", alias = "dependency_type")]
    pub dep_type: String,
}

/// Fields for `bd create`.
#[derive(Debug, Clone, Default)]
pub struct NewIssue<'a> {
    pub title: &'a str,
    pub issue_type: &'a str,
    pub priority: Option<&'a str>,
    pub description: &'a str,
    pub acceptance: Option<&'a str>,
    pub design: Option<&'a str>,
    pub notes: Option<&'a str>,
    pub parent: Option<&'a str>,
}

#[derive(Debug, thiserror::Error)]
pub enum BeadsError {
    #[error("bd not found: could not run `{program}` (is beads installed and on PATH?)")]
    BdNotFound { program: String },
    #[error("`{command}` exited with {status}: {stderr}")]
    CommandFailed {
        command: String,
        status: String,
        stderr: String,
    },
    #[error("failed to run `{command}`: {source}")]
    Io {
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error("could not parse output of `{command}`: {message}")]
    InvalidOutput { command: String, message: String },
}

#[derive(Deserialize)]
struct CreatedIssue {
    id: String,
}

#[derive(Debug, Clone)]
pub struct BeadsAdapter {
    program: OsString,
    workdir: Option<PathBuf>,
    envs: Vec<(OsString, OsString)>,
}

impl Default for BeadsAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl BeadsAdapter {
    pub fn new() -> Self {
        Self {
            program: OsString::from(BD_PROGRAM),
            workdir: None,
            envs: Vec::new(),
        }
    }

    /// Run `bd` in `workdir` instead of the inherited working directory.
    pub fn in_dir(mut self, workdir: impl Into<PathBuf>) -> Self {
        self.workdir = Some(workdir.into());
        self
    }

    /// Run `program` instead of `bd` from `PATH`.
    pub fn with_program(mut self, program: impl Into<OsString>) -> Self {
        self.program = program.into();
        self
    }

    /// Set an environment variable for the `bd` child process only.
    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.envs.push((key.into(), value.into()));
        self
    }

    /// `bd init --prefix <prefix> --quiet --skip-agents --skip-hooks
    /// --non-interactive`: create a Beads workspace in the adapter's
    /// directory (or `BEADS_DIR`) without touching agent files or git hooks.
    pub async fn init(&self, prefix: &str) -> Result<(), BeadsError> {
        self.run(&[
            "init",
            "--prefix",
            prefix,
            "--quiet",
            "--skip-agents",
            "--skip-hooks",
            "--non-interactive",
        ])
        .await
        .map(|_| ())
    }

    /// `bd show <id> --json`.
    pub async fn show(&self, id: &str) -> Result<BeadsIssue, BeadsError> {
        self.single(&["show", id, "--json"]).await
    }

    /// `bd children <parent> --json`; includes closed children.
    pub async fn children(&self, parent: &str) -> Result<Vec<BeadsIssue>, BeadsError> {
        self.json(&["children", parent, "--json"]).await
    }

    /// `bd ready --json --limit 0`: every ready issue in the workspace.
    pub async fn ready(&self) -> Result<Vec<BeadsIssue>, BeadsError> {
        self.json(&["ready", "--json", "--limit", "0"]).await
    }

    /// `bd update <id> --claim --json`.
    pub async fn claim(&self, id: &str) -> Result<BeadsIssue, BeadsError> {
        self.single(&["update", id, "--claim", "--json"]).await
    }

    /// `bd close <id> [--reason <reason>] --json`.
    pub async fn close(&self, id: &str, reason: Option<&str>) -> Result<BeadsIssue, BeadsError> {
        let mut args = vec!["close", id];
        if let Some(reason) = reason {
            args.extend(["--reason", reason]);
        }
        args.push("--json");
        self.single(&args).await
    }

    /// `bd create … --json`; returns the new issue's ID.
    pub async fn create(&self, issue: &NewIssue<'_>) -> Result<String, BeadsError> {
        let mut args = vec!["create", "--title", issue.title, "--type", issue.issue_type];
        if let Some(priority) = issue.priority {
            args.extend(["--priority", priority]);
        }
        args.extend(["--description", issue.description]);
        for (flag, value) in [
            ("--acceptance", issue.acceptance),
            ("--design", issue.design),
            ("--notes", issue.notes),
            ("--parent", issue.parent),
        ] {
            if let Some(value) = value {
                args.extend([flag, value]);
            }
        }
        args.push("--json");
        let created: CreatedIssue = self.json(&args).await?;
        Ok(created.id)
    }

    /// `bd dep add <blocked> <blocker> --json`.
    pub async fn add_dependency(&self, blocked: &str, blocker: &str) -> Result<(), BeadsError> {
        self.run(&["dep", "add", blocked, blocker, "--json"])
            .await
            .map(|_| ())
    }

    /// `bd list --parent <parent> --json --limit 0`: open children only.
    pub async fn list_open_children(&self, parent: &str) -> Result<Vec<BeadsIssue>, BeadsError> {
        self.json(&["list", "--parent", parent, "--json", "--limit", "0"])
            .await
    }

    async fn single(&self, args: &[&str]) -> Result<BeadsIssue, BeadsError> {
        let issues: Vec<BeadsIssue> = self.json(args).await?;
        issues
            .into_iter()
            .next()
            .ok_or_else(|| BeadsError::InvalidOutput {
                command: self.command_line(args),
                message: "expected one issue, got an empty array".to_string(),
            })
    }

    async fn json<T: DeserializeOwned>(&self, args: &[&str]) -> Result<T, BeadsError> {
        let stdout = self.run(args).await?;
        serde_json::from_slice(&stdout).map_err(|e| BeadsError::InvalidOutput {
            command: self.command_line(args),
            message: e.to_string(),
        })
    }

    /// The single spawn point for `bd`.
    async fn run(&self, args: &[&str]) -> Result<Vec<u8>, BeadsError> {
        let mut command = Command::new(&self.program);
        command
            .args(args)
            .envs(
                self.envs
                    .iter()
                    .map(|(k, v)| (k.as_os_str(), v.as_os_str())),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(workdir) = &self.workdir {
            command.current_dir(workdir);
        }
        let output = command.output().await.map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                BeadsError::BdNotFound {
                    program: self.program.to_string_lossy().into_owned(),
                }
            } else {
                BeadsError::Io {
                    command: self.command_line(args),
                    source,
                }
            }
        })?;
        if !output.status.success() {
            // bd also prints `{"error": …}` on stdout; use it when stderr is empty.
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stderr = if stderr.is_empty() {
                String::from_utf8_lossy(&output.stdout).trim().to_string()
            } else {
                stderr
            };
            return Err(BeadsError::CommandFailed {
                command: self.command_line(args),
                status: output.status.to_string(),
                stderr,
            });
        }
        Ok(output.stdout)
    }

    fn command_line(&self, args: &[&str]) -> String {
        let program = self.program.to_string_lossy();
        let words = std::iter::once(program.as_ref()).chain(args.iter().copied());
        shlex::try_join(words).unwrap_or_else(|_| {
            std::iter::once(program.as_ref())
                .chain(args.iter().copied())
                .collect::<Vec<_>>()
                .join(" ")
        })
    }
}

/// Shared by every module whose tests need a real Beads workspace.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub(crate) const TEST_ACTOR: &str = "pas-test";

    /// A temporary, isolated Beads workspace, or `None` when `bd` is missing
    /// (panics instead under `PAS_REQUIRE_BD=1`).
    pub(crate) async fn workspace() -> Option<(tempfile::TempDir, BeadsAdapter)> {
        match BeadsAdapter::new().run(&["--version"]).await {
            Err(BeadsError::BdNotFound { .. }) => {
                assert!(
                    std::env::var("PAS_REQUIRE_BD").as_deref() != Ok("1"),
                    "PAS_REQUIRE_BD=1 but bd is not on PATH"
                );
                eprintln!("skipping: bd not on PATH");
                return None;
            }
            Err(e) => panic!("bd --version failed: {e}"),
            Ok(_) => {}
        }
        if std::env::var_os("BEADS_DB").is_some() {
            eprintln!("skipping: BEADS_DB is set and would override the test workspace");
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        let git = Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .await
            .unwrap();
        assert!(git.success());
        let adapter = BeadsAdapter::new()
            .in_dir(dir.path())
            .with_env("BEADS_DIR", dir.path().join(".beads"))
            .with_env("BEADS_ACTOR", TEST_ACTOR);
        adapter.init("t").await.unwrap();
        Some((dir, adapter))
    }

    /// An executable shell script standing in for `bd`.
    pub(crate) fn stub(dir: &std::path::Path, name: &str, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::ffi::OsStr;
    use std::path::Path;

    use super::test_support::{stub, workspace, TEST_ACTOR};

    async fn raw(adapter: &BeadsAdapter, args: &[&str]) -> Vec<serde_json::Value> {
        serde_json::from_slice(&adapter.run(args).await.unwrap()).unwrap()
    }

    fn raw_triples(values: &[serde_json::Value]) -> BTreeSet<(String, String, String)> {
        values
            .iter()
            .map(|v| {
                (
                    v["id"].as_str().unwrap().to_string(),
                    v["title"].as_str().unwrap().to_string(),
                    v["status"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    fn triples(issues: &[BeadsIssue]) -> BTreeSet<(String, String, String)> {
        issues
            .iter()
            .map(|i| (i.id.clone(), i.title.clone(), i.status.clone()))
            .collect()
    }

    fn ids(issues: &[BeadsIssue]) -> BTreeSet<&str> {
        issues.iter().map(|i| i.id.as_str()).collect()
    }

    fn task<'a>(title: &'a str, priority: &'a str, parent: Option<&'a str>) -> NewIssue<'a> {
        NewIssue {
            title,
            issue_type: "task",
            priority: Some(priority),
            description: "test task",
            parent,
            ..Default::default()
        }
    }

    // AC1 + AC2 + AC3 against a real workspace. One workspace per test
    // function because `bd init` takes ~9 s.
    #[tokio::test]
    async fn epic_children_ready_claim_and_close_match_bd() {
        let Some((_dir, bd)) = workspace().await else {
            return;
        };

        let epic = bd
            .create(&NewIssue {
                title: "Epic E",
                issue_type: "epic",
                description: "the epic",
                ..Default::default()
            })
            .await
            .unwrap();
        let a = bd.create(&task("Task A", "1", Some(&epic))).await.unwrap();
        let b = bd.create(&task("Task B", "2", Some(&epic))).await.unwrap();
        let c = bd.create(&task("Task C", "2", Some(&epic))).await.unwrap();
        let x = bd.create(&task("Unrelated X", "3", None)).await.unwrap();
        bd.add_dependency(&c, &b).await.unwrap();

        // Dependencies parse from both the `children` and the `show` shape.
        let blocks_b = BeadsDependency {
            depends_on_id: b.clone(),
            dep_type: "blocks".into(),
        };
        let c_child = bd
            .children(&epic)
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.id == c)
            .unwrap();
        assert!(c_child.dependencies.contains(&blocks_b), "{c_child:?}");
        assert!(bd.show(&c).await.unwrap().dependencies.contains(&blocks_b));

        // AC1: the Epic matches `bd show`.
        let shown = bd.show(&epic).await.unwrap();
        assert_eq!(shown.id, epic);
        assert_eq!(shown.title, "Epic E");
        assert_eq!(shown.status, "open");
        assert_eq!(shown.issue_type.as_deref(), Some("epic"));
        assert_eq!(shown.description.as_deref(), Some("the epic"));
        assert_eq!(
            triples(std::slice::from_ref(&shown)),
            raw_triples(&raw(&bd, &["show", &epic, "--json"]).await)
        );

        // AC1: children with status match `bd children`.
        let children = bd.children(&epic).await.unwrap();
        assert_eq!(ids(&children), BTreeSet::from([&*a, &*b, &*c]));
        assert!(children.iter().all(|t| t.status == "open"));
        assert!(children.iter().all(|t| t.parent.as_deref() == Some(&*epic)));
        assert_eq!(
            triples(&children),
            raw_triples(&raw(&bd, &["children", &epic, "--json"]).await)
        );

        // AC1: the ready set matches `bd ready`; blocked C is absent.
        let ready = bd.ready().await.unwrap();
        let ready_ids = ids(&ready);
        for id in [&a, &b, &x] {
            assert!(ready_ids.contains(id.as_str()), "{id} not ready");
        }
        assert!(!ready_ids.contains(c.as_str()));
        assert_eq!(
            triples(&ready),
            raw_triples(&raw(&bd, &["ready", "--json", "--limit", "0"]).await)
        );

        // CLI path: open children via `bd list --parent`.
        assert_eq!(
            ids(&bd.list_open_children(&epic).await.unwrap()),
            BTreeSet::from([&*a, &*b, &*c])
        );

        // AC2: claim and close change the status `bd show` sees.
        let claimed = bd.claim(&b).await.unwrap();
        assert_eq!(claimed.status, "in_progress");
        let shown_b = bd.show(&b).await.unwrap();
        assert_eq!(shown_b.status, "in_progress");
        assert_eq!(shown_b.assignee.as_deref(), Some(TEST_ACTOR));

        let closed = bd.close(&b, Some("done by test")).await.unwrap();
        assert_eq!(closed.status, "closed");
        let shown_b = bd.show(&b).await.unwrap();
        assert_eq!(shown_b.status, "closed");
        assert_eq!(shown_b.close_reason.as_deref(), Some("done by test"));
        assert!(ids(&bd.ready().await.unwrap()).contains(c.as_str()));

        // Close without a reason, and children still lists closed Tasks.
        assert_eq!(bd.close(&a, None).await.unwrap().status, "closed");
        let children = bd.children(&epic).await.unwrap();
        let status_of = |id: &str| {
            children
                .iter()
                .find(|t| t.id == id)
                .map(|t| t.status.clone())
        };
        assert_eq!(status_of(&a).as_deref(), Some("closed"));
        assert_eq!(status_of(&b).as_deref(), Some("closed"));
        assert_eq!(status_of(&c).as_deref(), Some("open"));
        assert_eq!(
            triples(&children),
            raw_triples(&raw(&bd, &["children", &epic, "--json"]).await)
        );
        assert_eq!(
            ids(&bd.list_open_children(&epic).await.unwrap()),
            BTreeSet::from([c.as_str()])
        );

        // Children of a childless issue is an empty list, not an error.
        assert!(bd.children(&x).await.unwrap().is_empty());
    }

    // AC3 against a real bd: non-zero exit carries the command and stderr.
    #[tokio::test]
    async fn bd_nonzero_exit_reports_command_and_stderr() {
        let Some((_dir, bd)) = workspace().await else {
            return;
        };
        let err = bd.show("t-nope").await.unwrap_err();
        let BeadsError::CommandFailed {
            command, stderr, ..
        } = &err
        else {
            panic!("expected CommandFailed, got {err:?}");
        };
        assert_eq!(command, "bd show t-nope --json");
        assert!(stderr.contains("no issue found"), "stderr: {stderr}");
        let message = err.to_string();
        assert!(message.contains("bd show t-nope --json"), "{message}");
        assert!(message.contains("no issue found"), "{message}");

        // Claiming an already-closed or missing issue also fails loudly.
        assert!(matches!(
            bd.claim("t-nope").await,
            Err(BeadsError::CommandFailed { .. })
        ));
        assert!(matches!(
            bd.close("t-nope", Some("x")).await,
            Err(BeadsError::CommandFailed { .. })
        ));
    }

    // AC3 without bd: a stub that fails.
    #[tokio::test]
    async fn nonzero_exit_from_stub_reports_command_and_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let program = stub(dir.path(), "fake-bd", r#"echo "boom: $*" >&2; exit 3"#);
        let bd = BeadsAdapter::new().with_program(&program);

        let err = bd.show("x").await.unwrap_err();
        let expected_command = format!("{} show x --json", program.display());
        match &err {
            BeadsError::CommandFailed {
                command,
                status,
                stderr,
            } => {
                assert_eq!(command, &expected_command);
                assert_eq!(stderr, "boom: show x --json");
                assert!(status.contains('3'), "status: {status}");
            }
            other => panic!("expected CommandFailed, got {other:?}"),
        }
        let message = err.to_string();
        assert!(message.contains(&expected_command), "{message}");
        assert!(message.contains("boom: show x --json"), "{message}");

        // Arguments with spaces are quoted in the reported command.
        let err = bd.close("x", Some("two words")).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("close x --reason 'two words' --json"),
            "{err}"
        );

        // Empty stderr falls back to bd's `{"error": …}` on stdout.
        let program = stub(
            dir.path(),
            "fake-bd-stdout",
            r#"echo '{"error": "epics can only block other epics"}'; exit 1"#,
        );
        let err = BeadsAdapter::new()
            .with_program(&program)
            .add_dependency("e", "t")
            .await
            .unwrap_err();
        assert!(matches!(err, BeadsError::CommandFailed { .. }));
        assert!(
            err.to_string().contains("epics can only block other epics"),
            "{err}"
        );
    }

    // `init` passes the non-interactive flags, ignores stdout, and reports a
    // failed init as CommandFailed.
    #[tokio::test]
    async fn init_runs_non_interactive_bd_init_and_reports_failure() {
        let dir = tempfile::tempdir().unwrap();
        let args_file = dir.path().join("args");
        let program = stub(
            dir.path(),
            "fake-bd",
            &format!(r#"echo "$*" > '{}'; echo 'not json'"#, args_file.display()),
        );
        BeadsAdapter::new()
            .with_program(&program)
            .init("pe")
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&args_file).unwrap().trim(),
            "init --prefix pe --quiet --skip-agents --skip-hooks --non-interactive"
        );

        let program = stub(dir.path(), "fake-bd-fail", r#"echo "no init" >&2; exit 1"#);
        let err = BeadsAdapter::new()
            .with_program(&program)
            .init("pe")
            .await
            .unwrap_err();
        match &err {
            BeadsError::CommandFailed {
                command, stderr, ..
            } => {
                assert!(
                    command.ends_with(
                        "init --prefix pe --quiet --skip-agents --skip-hooks --non-interactive"
                    ),
                    "{command}"
                );
                assert_eq!(stderr, "no init");
            }
            other => panic!("expected CommandFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn malformed_or_empty_output_is_invalid_output() {
        let dir = tempfile::tempdir().unwrap();
        let program = stub(dir.path(), "fake-bd", "echo 'not json'");
        let err = BeadsAdapter::new()
            .with_program(&program)
            .ready()
            .await
            .unwrap_err();
        match &err {
            BeadsError::InvalidOutput { command, .. } => {
                assert!(command.ends_with("ready --json --limit 0"), "{command}")
            }
            other => panic!("expected InvalidOutput, got {other:?}"),
        }

        let program = stub(dir.path(), "fake-bd-empty", "echo '[]'");
        let bd = BeadsAdapter::new().with_program(&program);
        assert!(matches!(
            bd.show("x").await,
            Err(BeadsError::InvalidOutput { .. })
        ));
        assert!(bd.children("x").await.unwrap().is_empty());

        // Missing required fields are rejected, unknown ones are ignored.
        let program = stub(
            dir.path(),
            "fake-bd-fields",
            r#"echo '[{"id":"t-1","title":"T","status":"hooked","extra":{"a":1}}]'"#,
        );
        let issue = BeadsAdapter::new()
            .with_program(&program)
            .show("t-1")
            .await
            .unwrap();
        assert_eq!(issue.status, "hooked");
        assert_eq!(issue.description, None);
        let program = stub(dir.path(), "fake-bd-noid", r#"echo '[{"title":"T"}]'"#);
        assert!(matches!(
            BeadsAdapter::new().with_program(&program).show("t-1").await,
            Err(BeadsError::InvalidOutput { .. })
        ));
    }

    #[test]
    fn dependencies_parse_from_children_and_show_shapes() {
        let children: BeadsIssue = serde_json::from_str(
            r#"{"id":"e.2","title":"T","status":"open","dependencies":[
                {"issue_id":"e.2","depends_on_id":"e.1","type":"blocks","metadata":"{}"},
                {"issue_id":"e.2","depends_on_id":"e","type":"parent-child"}]}"#,
        )
        .unwrap();
        let shown: BeadsIssue = serde_json::from_str(
            r#"{"id":"e.2","title":"T","status":"open","dependencies":[
                {"id":"e.1","title":"A","status":"open","dependency_type":"blocks"},
                {"id":"e","title":"E","status":"open","dependency_type":"parent-child"}]}"#,
        )
        .unwrap();
        let expected = vec![
            BeadsDependency {
                depends_on_id: "e.1".into(),
                dep_type: "blocks".into(),
            },
            BeadsDependency {
                depends_on_id: "e".into(),
                dep_type: "parent-child".into(),
            },
        ];
        assert_eq!(children.dependencies, expected);
        assert_eq!(shown.dependencies, expected);
        let ready: BeadsIssue =
            serde_json::from_str(r#"{"id":"e.2","title":"T","status":"open"}"#).unwrap();
        assert!(ready.dependencies.is_empty());
    }

    #[tokio::test]
    async fn workdir_and_env_apply_only_to_the_child() {
        let dir = tempfile::tempdir().unwrap();
        let program = stub(
            dir.path(),
            "fake-bd",
            r#"printf '{"id":"%s|%s"}' "$(pwd -P)" "$PAS_BEADS_TEST_VAR""#,
        );
        let id = BeadsAdapter::new()
            .with_program(&program)
            .in_dir(dir.path())
            .with_env("PAS_BEADS_TEST_VAR", "v1")
            .create(&NewIssue {
                title: "t",
                issue_type: "task",
                description: "d",
                ..Default::default()
            })
            .await
            .unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        assert_eq!(id, format!("{}|v1", canonical.display()));
        assert!(std::env::var_os("PAS_BEADS_TEST_VAR").is_none());
    }

    // AC4: bd missing from PATH is a distinct error.
    #[tokio::test]
    async fn missing_bd_on_path_is_bd_not_found() {
        let empty = tempfile::tempdir().unwrap();
        let err = BeadsAdapter::new()
            .with_env("PATH", empty.path())
            .ready()
            .await
            .unwrap_err();
        assert!(
            matches!(&err, BeadsError::BdNotFound { program } if program == BD_PROGRAM),
            "expected BdNotFound, got {err:?}"
        );
        assert!(err.to_string().contains("PATH"), "{err}");

        let err = BeadsAdapter::new()
            .with_program("/nonexistent/bd")
            .show("x")
            .await
            .unwrap_err();
        assert!(
            matches!(&err, BeadsError::BdNotFound { .. }),
            "expected BdNotFound, got {err:?}"
        );
    }

    #[test]
    fn bd_on_path_finds_only_an_executable_bd() {
        let empty = tempfile::tempdir().unwrap();
        let plain = tempfile::tempdir().unwrap();
        std::fs::write(plain.path().join(BD_PROGRAM), "not executable").unwrap();
        let nested = tempfile::tempdir().unwrap();
        std::fs::create_dir(nested.path().join(BD_PROGRAM)).unwrap();
        let bin = tempfile::tempdir().unwrap();
        stub(bin.path(), BD_PROGRAM, "exit 0");

        let joined = |dirs: &[&Path]| std::env::join_paths(dirs).unwrap();
        assert!(!bd_on_path(None));
        assert!(!bd_on_path(Some(OsStr::new(""))));
        assert!(!bd_on_path(Some(empty.path().as_os_str())));
        assert!(!bd_on_path(Some(plain.path().as_os_str())));
        assert!(!bd_on_path(Some(nested.path().as_os_str())));
        assert!(bd_on_path(Some(bin.path().as_os_str())));
        assert!(bd_on_path(Some(&joined(&[
            empty.path(),
            plain.path(),
            bin.path()
        ]))));
        assert!(!bd_on_path(Some(&joined(&[empty.path(), plain.path()]))));
    }

    #[tokio::test]
    async fn spawn_failure_other_than_not_found_is_io() {
        let dir = tempfile::tempdir().unwrap();
        let not_executable = dir.path().join("bd-not-executable");
        std::fs::write(&not_executable, "not a program").unwrap();
        let err = BeadsAdapter::new()
            .with_program(&not_executable)
            .ready()
            .await
            .unwrap_err();
        assert!(
            matches!(&err, BeadsError::Io { .. }),
            "expected Io, got {err:?}"
        );
    }

    #[test]
    fn default_program_is_bd() {
        assert_eq!(BeadsAdapter::default().program, OsStr::new(BD_PROGRAM));
    }

    // AC5: no other file in crates/ names the bd program.
    #[test]
    fn only_beads_adapter_names_bd() {
        let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let needle = format!("\"{BD_PROGRAM}\"");
        let mut offenders = Vec::new();
        let mut stack = vec![crates.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    if path.file_name() != Some(OsStr::new("target")) {
                        stack.push(path);
                    }
                } else if path.extension() == Some(OsStr::new("rs"))
                    && !path.ends_with("attractor-pipeline/src/beads_adapter.rs")
                    && std::fs::read_to_string(&path)
                        .map(|s| s.contains(&needle))
                        .unwrap_or(false)
                {
                    offenders.push(path);
                }
            }
        }
        assert!(offenders.is_empty(), "files naming bd: {offenders:?}");
    }
}
