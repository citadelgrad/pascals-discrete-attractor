//! U10: opt-in live tests that real pi and Claude CLIs load (or do not load)
//! the skills named for a Run.
//!
//! Every test is `#[ignore]` and returns early unless its provider gate is set,
//! so `cargo test --workspace` makes no model call. Each live test is one
//! one-node Run with a tiny prompt (a few cents in total).
//!
//! ```text
//! PAS_LIVE_PI=1 cargo test -p attractor-cli --test live_agent_resources -- --ignored pi_
//! PAS_LIVE_CLAUDE=1 cargo test -p attractor-cli --test live_agent_resources -- --ignored claude_
//! ```
//!
//! `PAS_LIVE_PI_MODEL` (default `openai/gpt-5.5`) and `PAS_LIVE_CLAUDE_MODEL`
//! (default `haiku`) pick the models.
#![cfg(unix)]

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

const SENTINEL: &str = "pas-sentinel";

/// True only when `var` is `1` and the CLI named `tool` is on PATH.
fn live(var: &str, tool: &str) -> bool {
    if std::env::var(var).as_deref() != Ok("1") {
        eprintln!("skipping: set {var}=1 to run this live test");
        return false;
    }
    let found = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|dir| dir.join(tool).is_file()))
        .unwrap_or(false);
    assert!(found, "{var}=1 but `{tool}` is not on PATH");
    true
}

fn sentinel_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../attractor-pipeline/tests/fixtures/agent-resources/skills")
        .join(SENTINEL)
        .canonicalize()
        .expect("sentinel skill fixture")
}

struct RunOutput {
    stderr: String,
    transcripts: Vec<Vec<Value>>,
}

/// One-node Run of `provider`/`model` in a scratch work dir with its own state
/// dir, so the machine's Run Index and project files stay out of the test.
fn run(provider: &str, model: &str, extra: &[&str]) -> RunOutput {
    let root = tempfile::tempdir().unwrap();
    let work = root.path().join("work");
    let state = root.path().join("state");
    fs::create_dir(&work).unwrap();
    fs::create_dir(&state).unwrap();
    let dot = work.join("live.dot");
    fs::write(
        &dot,
        format!(
            r#"digraph G {{
                start [shape="Mdiamond"]
                work [shape="box", prompt="Reply with the single word ok.", llm_provider="{provider}", llm_model="{model}", timeout=180s]
                done [shape="Msquare"]
                start -> work -> done
            }}"#
        ),
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_pas"))
        .args(["run", "live.dot", "--json"])
        .args(extra)
        .current_dir(&work)
        .env("PAS_STATE_DIR", &state)
        .env("PAS_NON_INTERACTIVE", "1")
        // A test started inside a PAS Claude node would otherwise force safe
        // mode on the child Claude in every settings mode.
        .env_remove("CLAUDE_CODE_SAFE_MODE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut first = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut first)
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(output.status.success(), "pas run failed: {stderr}");

    let start: Value = serde_json::from_str(first.trim())
        .unwrap_or_else(|e| panic!("first stdout line is not JSON ({e}): {first:?}"));
    let run_dir = PathBuf::from(start["run_dir"].as_str().expect("run_dir in --json line"));
    let mut transcripts = Vec::new();
    for entry in fs::read_dir(run_dir.join("transcripts")).expect("transcripts dir") {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "jsonl") {
            let lines = fs::read_to_string(&path)
                .unwrap()
                .lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect();
            transcripts.push(lines);
        }
    }
    assert!(!transcripts.is_empty(), "no Transcript written: {stderr}");
    RunOutput {
        stderr,
        transcripts,
    }
}

/// Skill names in the first pi `message_start` system message
/// (`sections.skills` is an `<available_skills>` XML string).
fn pi_skill_names(transcripts: &[Vec<Value>]) -> Vec<String> {
    let system = transcripts
        .iter()
        .flatten()
        .find(|line| line["type"] == "message_start" && line["message"]["role"] == "system")
        .expect("pi Transcript has no system message_start");
    let skills = system["message"]["sections"]["skills"]
        .as_str()
        .unwrap_or("");
    let mut names = Vec::new();
    let mut rest = skills;
    while let Some(open) = rest.find("<name>") {
        let after = &rest[open + "<name>".len()..];
        let Some(close) = after.find("</name>") else {
            break;
        };
        names.push(after[..close].trim().to_string());
        rest = &after[close..];
    }
    names
}

/// The `skills` array of the Claude `system`/`init` line.
fn claude_init_skills(transcripts: &[Vec<Value>]) -> Vec<String> {
    let init = transcripts
        .iter()
        .flatten()
        .find(|line| line["type"] == "system" && line["subtype"] == "init")
        .expect("Claude Transcript has no system/init line");
    init["skills"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Entry names under the personal pi skill roots, minus the sentinel.
fn personal_pi_skill_names() -> Vec<String> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    [".pi/agent/skills", ".agents/skills"]
        .iter()
        .filter_map(|root| fs::read_dir(home.join(root)).ok())
        .flatten()
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name != SENTINEL && !name.starts_with('.'))
        .collect()
}

fn is_sentinel(skill: &str) -> bool {
    skill == SENTINEL || skill.ends_with(&format!(":{SENTINEL}"))
}

fn pi_model() -> String {
    std::env::var("PAS_LIVE_PI_MODEL").unwrap_or_else(|_| "openai/gpt-5.5".into())
}

fn claude_model() -> String {
    std::env::var("PAS_LIVE_CLAUDE_MODEL").unwrap_or_else(|_| "haiku".into())
}

#[test]
#[ignore = "paid model call; needs PAS_LIVE_PI=1"]
fn pi_named_skill_loads_and_personal_skills_do_not() {
    if !live("PAS_LIVE_PI", "pi") {
        return;
    }
    let skill = sentinel_dir();
    let out = run(
        "pi",
        &pi_model(),
        &["--codergen-skill", skill.to_str().unwrap()],
    );
    let names = pi_skill_names(&out.transcripts);
    assert!(names.iter().any(|n| n == SENTINEL), "{names:?}");
    let leaked: Vec<_> = personal_pi_skill_names()
        .into_iter()
        .filter(|p| names.contains(p))
        .collect();
    assert!(leaked.is_empty(), "personal skills leaked: {leaked:?}");
}

#[test]
#[ignore = "paid model call; needs PAS_LIVE_PI=1"]
fn pi_without_skills_does_not_show_the_sentinel() {
    if !live("PAS_LIVE_PI", "pi") {
        return;
    }
    let out = run("pi", &pi_model(), &[]);
    let names = pi_skill_names(&out.transcripts);
    assert!(!names.iter().any(|n| n == SENTINEL), "{names:?}");
}

#[test]
#[ignore = "paid model call; needs PAS_LIVE_CLAUDE=1"]
fn claude_inherit_lists_the_named_skill() {
    if !live("PAS_LIVE_CLAUDE", "claude") {
        return;
    }
    let skill = sentinel_dir();
    let out = run(
        "claude",
        &claude_model(),
        &[
            "--codergen-claude-settings-mode",
            "inherit",
            "--codergen-claude-setting-sources",
            "user,project,local",
            "--codergen-skill",
            skill.to_str().unwrap(),
        ],
    );
    let skills = claude_init_skills(&out.transcripts);
    assert!(skills.iter().any(|s| is_sentinel(s)), "{skills:?}");
}

#[test]
#[ignore = "paid model call; needs PAS_LIVE_CLAUDE=1"]
fn claude_subscription_bare_does_not_list_and_warns() {
    if !live("PAS_LIVE_CLAUDE", "claude") {
        return;
    }
    let skill = sentinel_dir();
    let out = run(
        "claude",
        &claude_model(),
        &["--codergen-skill", skill.to_str().unwrap()],
    );
    let skills = claude_init_skills(&out.transcripts);
    assert!(!skills.iter().any(|s| is_sentinel(s)), "{skills:?}");
    assert!(
        out.stderr
            .lines()
            .any(|l| l.contains("CODERGEN_SKILLS_NOT_LOADED") && l.contains("'work'")),
        "no warning naming node 'work': {}",
        out.stderr
    );
}

#[test]
#[ignore = "paid model call; needs PAS_LIVE_CLAUDE=1"]
fn claude_without_skills_does_not_list_the_sentinel() {
    if !live("PAS_LIVE_CLAUDE", "claude") {
        return;
    }
    let out = run("claude", &claude_model(), &[]);
    let skills = claude_init_skills(&out.transcripts);
    assert!(!skills.iter().any(|s| is_sentinel(s)), "{skills:?}");
    assert!(
        !out.stderr.contains("CODERGEN_SKILLS_NOT_LOADED"),
        "{}",
        out.stderr
    );
}
