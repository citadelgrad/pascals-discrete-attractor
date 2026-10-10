//! U7: `pas run` checks pi before the first node, with stub providers only.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn fixture_jsonl() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../attractor-pipeline/tests/fixtures/providers/pi-1.0.4.jsonl")
}

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        for dir in ["shims", "work", "state"] {
            fs::create_dir(root.path().join(dir)).unwrap();
        }
        Self { root }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    fn script(&self, name: &str, body: &str) {
        let path = self.path("shims").join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A pi shim: every call is logged to `pi-calls`; `--version` prints
    /// `version`; `auth check` prints `auth` and exits `auth_exit`; any other
    /// call is a node and prints the recorded pi 1.0.4 session.
    fn pi_shim(&self, version: &str, auth: &str, auth_exit: i32) {
        let calls = self.path("pi-calls");
        self.script(
            "pi",
            &format!(
                "echo \"$*\" >> '{calls}'\n\
                 case \"$1\" in\n\
                 --version) echo '{version}' ;;\n\
                 auth) echo '{auth}'; exit {auth_exit} ;;\n\
                 *) echo node >> '{calls}.nodes'; cat '{fixture}' ;;\n\
                 esac",
                calls = calls.display(),
                fixture = fixture_jsonl().display(),
            ),
        );
    }

    /// A codex shim that records that a node ran.
    fn codex_shim(&self) {
        self.script(
            "codex",
            &format!(
                "echo codex >> '{}'\n\
                 echo '{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":\"ok\"}}}}'",
                self.path("codex-started").display()
            ),
        );
    }

    fn pas(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_pas"))
            .args(args)
            .current_dir(self.path("work"))
            .env("PATH", self.path("shims"))
            .env("PAS_STATE_DIR", self.path("state"))
            .output()
            .unwrap()
    }

    fn pi_calls(&self) -> Vec<String> {
        fs::read_to_string(self.path("pi-calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn pi_nodes_started(&self) -> usize {
        fs::read_to_string(self.path("pi-calls.nodes"))
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    fn dot(&self, name: &str, provider: &str, model: &str) -> PathBuf {
        let path = self.path(name);
        fs::write(
            &path,
            format!(
                r#"digraph G {{
                start [shape="Mdiamond"]
                work [shape="box", prompt="work", llm_provider="{provider}", llm_model="{model}", timeout=30s]
                done [shape="Msquare"]
                start -> work -> done
            }}"#
            ),
        )
        .unwrap();
        path
    }

    fn assert_no_trace(&self) {
        assert!(
            !self.path("work/.pas").exists(),
            "a refused Run must write no Run directory or lock"
        );
        let state: Vec<_> = fs::read_dir(self.path("state")).unwrap().collect();
        assert!(state.is_empty(), "state dir must stay empty: {state:?}");
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

const READY: &str = r#"{"status":"ready","provider":"openai","authType":"oauth"}"#;

#[test]
fn ready_pi_starts_the_first_node_with_no_readiness_error() {
    let fx = Fixture::new();
    fx.pi_shim("1.0.4", READY, 0);
    let dot = fx.dot("p.dot", "pi", "openai/gpt-5.5");
    let output = fx.pas(&["run", dot.to_str().unwrap()]);
    let all = text(&output);
    assert!(!all.contains("not ready"), "{all}");
    assert!(!all.contains("pi_not_ready"), "{all}");
    assert_eq!(fx.pi_nodes_started(), 1, "{all}");
    let calls = fx.pi_calls();
    assert!(calls[0].starts_with("--version"), "{calls:?}");
    assert!(calls[1].starts_with("auth check --model openai/gpt-5.5 --json"));
    assert!(calls.iter().all(|c| !c.contains("--credentials")));
    assert!(calls.iter().all(|c| !c.contains("--no-refresh")));
}

#[test]
fn old_pi_stops_before_the_first_node_and_leaves_no_trace() {
    let fx = Fixture::new();
    fx.pi_shim("0.51.2", READY, 0);
    let dot = fx.dot("p.dot", "pi", "openai/gpt-5.5");
    let output = fx.pas(&["run", dot.to_str().unwrap()]);
    assert!(!output.status.success());
    let all = text(&output);
    assert!(all.contains("0.51.2") && all.contains("1.0"), "{all}");
    assert_eq!(fx.pi_nodes_started(), 0);
    fx.assert_no_trace();
}

#[test]
fn unparseable_version_stops_before_the_first_node() {
    let fx = Fixture::new();
    fx.pi_shim("pi is fine", READY, 0);
    let dot = fx.dot("p.dot", "pi", "openai/gpt-5.5");
    let output = fx.pas(&["run", dot.to_str().unwrap()]);
    assert!(!output.status.success());
    assert_eq!(fx.pi_nodes_started(), 0);
    fx.assert_no_trace();
}

#[test]
fn not_ready_names_model_and_reason_and_json_error_code() {
    let fx = Fixture::new();
    fx.pi_shim(
        "1.0.4",
        r#"{"status":"not_ready","provider":"openai","reason":"credentials_not_configured"}"#,
        0,
    );
    let dot = fx.dot("p.dot", "pi", "openai/gpt-5.5:high");
    let output = fx.pas(&["run", dot.to_str().unwrap(), "--json"]);
    assert!(!output.status.success());
    let all = text(&output);
    assert!(all.contains("openai/gpt-5.5"), "{all}");
    assert!(!all.contains(":high"), "{all}");
    assert!(all.contains("credentials_not_configured"), "{all}");
    assert!(all.contains("pi_not_ready"), "{all}");
    assert_eq!(fx.pi_nodes_started(), 0);
    fx.assert_no_trace();
}

#[test]
fn token_in_pi_output_never_reaches_the_cli_error() {
    let fx = Fixture::new();
    let auth = r#"{"status":"not_ready","reason":"sk-test-SECRET","apiKey":"sk-test-SECRET"}"#;
    fx.script(
        "pi",
        &format!(
            "case \"$1\" in --version) echo 1.0.4 ;; *) echo '{auth}'; echo sk-test-SECRET >&2; exit 1 ;; esac"
        ),
    );
    let dot = fx.dot("p.dot", "pi", "openai/gpt-5.5");
    let output = fx.pas(&["run", dot.to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(!text(&output).contains("sk-test-"), "{}", text(&output));
    fx.assert_no_trace();
}

#[test]
fn missing_pi_stops_before_any_node_and_writes_nothing() {
    let fx = Fixture::new();
    let dot = fx.dot("p.dot", "pi", "openai/gpt-5.5");
    let output = fx.pas(&[
        "run",
        dot.to_str().unwrap(),
        "--logs",
        fx.path("logs").to_str().unwrap(),
    ]);
    assert!(!output.status.success());
    assert!(text(&output).contains("pi"), "{}", text(&output));
    assert!(!fx.path("logs").exists(), "no Run directory or lock");
    fx.assert_no_trace();
}

#[test]
fn directory_run_checks_every_pipeline_before_the_first_starts() {
    let fx = Fixture::new();
    fx.codex_shim();
    fx.pi_shim("0.51.2", READY, 0);
    let dir = fx.path("pipes");
    fs::create_dir(&dir).unwrap();
    let first = fx.dot("tmp1.dot", "codex", "gpt-5.5");
    let second = fx.dot("tmp2.dot", "pi", "openai/gpt-5.5");
    fs::rename(first, dir.join("01-first.dot")).unwrap();
    fs::rename(second, dir.join("02-second.dot")).unwrap();
    let output = fx.pas(&["run", dir.to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(text(&output).contains("0.51.2"), "{}", text(&output));
    assert!(
        !fx.path("codex-started").exists(),
        "no node of the first Pipeline may run"
    );
    assert!(!fx.path("work/.pas").exists());
    assert!(!fx.path("work/.pas/logs").exists());
}

#[test]
fn dry_run_and_pipelines_without_pi_make_no_readiness_call() {
    let fx = Fixture::new();
    fx.codex_shim();
    fx.pi_shim("0.51.2", READY, 0);

    let pi_dot = fx.dot("pi.dot", "pi", "openai/gpt-5.5");
    let dry = fx.pas(&["run", pi_dot.to_str().unwrap(), "--dry-run"]);
    assert!(dry.status.success(), "{}", text(&dry));

    let codex_dot = fx.dot("codex.dot", "codex", "gpt-5.5");
    let real = fx.pas(&["run", codex_dot.to_str().unwrap()]);
    assert!(real.status.success(), "{}", text(&real));
    assert!(fx.path("codex-started").exists());

    assert!(fx.pi_calls().is_empty(), "{:?}", fx.pi_calls());
}
