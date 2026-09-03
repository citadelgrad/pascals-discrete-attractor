#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn pas() -> &'static str {
    env!("CARGO_BIN_EXE_pas")
}

fn write_pipeline(dir: &Path, name: &str, source: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, source).unwrap();
    path
}

fn provider_shims(dir: &Path) {
    for provider in ["claude", "codex", "gemini"] {
        let path = dir.join(provider);
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' '{provider}' >> \"$PAS_TEST_PROVIDER_MARKER\"\nresponse=\"${{PAS_TEST_PROVIDER_RESPONSE:-ok}}\"\nprintf '{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":\"%s\"}}}}\\n' \"$response\"\n"
        );
        fs::write(&path, script).unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }
}

fn run_with_shims(args: &[&str], shim_dir: &Path, marker: &Path) -> Output {
    Command::new(pas())
        .args(args)
        .env("PATH", shim_dir)
        .env("PAS_TEST_PROVIDER_MARKER", marker)
        .output()
        .unwrap()
}

fn run_with_shims_and_response(
    args: &[&str],
    shim_dir: &Path,
    marker: &Path,
    response: &str,
) -> Output {
    Command::new(pas())
        .args(args)
        .env("PATH", shim_dir)
        .env("PAS_TEST_PROVIDER_MARKER", marker)
        .env("PAS_TEST_PROVIDER_RESPONSE", response)
        .output()
        .unwrap()
}

#[test]
fn validate_info_and_run_share_the_same_resolved_provider_and_boundaries() {
    let fixture = tempfile::tempdir().unwrap();
    let shims = tempfile::tempdir().unwrap();
    provider_shims(shims.path());
    let marker = fixture.path().join("providers-started");
    let logs = fixture.path().join("logs");
    let pipeline = write_pipeline(
        fixture.path(),
        "shared.dot",
        r#"digraph Shared {
            START
            work [shape="box", prompt="work", llm_provider="OpenAI", timeout=1s]
            DONE
            START -> work -> DONE
        }"#,
    );
    let pipeline_arg = pipeline.to_str().unwrap();

    let validate = run_with_shims(&["validate", pipeline_arg], shims.path(), &marker);
    assert!(
        validate.status.success(),
        "{}",
        String::from_utf8_lossy(&validate.stderr)
    );
    assert!(String::from_utf8_lossy(&validate.stdout).contains("Pipeline is valid"));

    let info = run_with_shims(&["info", pipeline_arg], shims.path(), &marker);
    assert!(
        info.status.success(),
        "{}",
        String::from_utf8_lossy(&info.stderr)
    );
    let info_stdout = String::from_utf8_lossy(&info.stdout);
    assert!(info_stdout.contains("Start: START"));
    assert!(info_stdout.contains("Exit: DONE"));
    assert!(info_stdout.contains("work [work] kind=Task handler=codergen provider=codex"));

    let run = run_with_shims(
        &[
            "run",
            pipeline_arg,
            "--logs",
            logs.to_str().unwrap(),
            "--fresh",
        ],
        shims.path(),
        &marker,
    );
    assert!(
        run.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(fs::read_to_string(&marker).unwrap(), "codex\n");
    assert!(String::from_utf8_lossy(&run.stdout)
        .contains("Completed nodes: [\"START\", \"work\", \"DONE\"]"));
}

#[test]
fn diamond_with_explicit_codergen_routes_by_provider_label() {
    let fixture = tempfile::tempdir().unwrap();
    let shims = tempfile::tempdir().unwrap();
    provider_shims(shims.path());
    let marker = fixture.path().join("providers-started");
    let logs = fixture.path().join("logs");
    let pipeline = write_pipeline(
        fixture.path(),
        "conditional-codergen.dot",
        r#"digraph ConditionalCodergen {
            start [shape="Mdiamond"]
            choice [shape="diamond", type="codergen", prompt="choose", llm_provider="codex"]
            a_rejected [shape="diamond"]
            z_approved [shape="diamond"]
            done [shape="Msquare"]
            start -> choice
            choice -> a_rejected [label="REJECT"]
            choice -> z_approved [label="APPROVE"]
            a_rejected -> done
            z_approved -> done
        }"#,
    );

    let output = run_with_shims_and_response(
        &[
            "run",
            pipeline.to_str().unwrap(),
            "--logs",
            logs.to_str().unwrap(),
            "--fresh",
        ],
        shims.path(),
        &marker,
        "APPROVE",
    );

    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read_to_string(&marker).unwrap(), "codex\n");
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("Completed nodes: [\"start\", \"choice\", \"z_approved\", \"done\"]"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn unprompted_diamond_with_explicit_codergen_routes_by_provider_label() {
    let fixture = tempfile::tempdir().unwrap();
    let shims = tempfile::tempdir().unwrap();
    provider_shims(shims.path());
    let marker = fixture.path().join("providers-started");
    let logs = fixture.path().join("logs");
    let pipeline = write_pipeline(
        fixture.path(),
        "unprompted-conditional-codergen.dot",
        r#"digraph ConditionalCodergen {
            start [shape="Mdiamond"]
            choice [shape="diamond", type="codergen", llm_provider="codex"]
            a_rejected [shape="diamond"]
            z_approved [shape="diamond"]
            done [shape="Msquare"]
            start -> choice
            choice -> a_rejected [label="REJECT"]
            choice -> z_approved [label="APPROVE"]
            a_rejected -> done
            z_approved -> done
        }"#,
    );

    let output = run_with_shims_and_response(
        &[
            "run",
            pipeline.to_str().unwrap(),
            "--logs",
            logs.to_str().unwrap(),
            "--fresh",
        ],
        shims.path(),
        &marker,
        "APPROVE",
    );

    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read_to_string(&marker).unwrap(), "codex\n");
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("Completed nodes: [\"start\", \"choice\", \"z_approved\", \"done\"]"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn invalid_semantics_start_zero_provider_processes() {
    let fixture = tempfile::tempdir().unwrap();
    let shims = tempfile::tempdir().unwrap();
    provider_shims(shims.path());
    let marker = fixture.path().join("providers-started");
    let cases = [
        (
            "missing.dot",
            r#"digraph G { start [shape="Mdiamond"] work [shape="box"] done [shape="Msquare"] start -> work -> done }"#,
        ),
        (
            "provider.dot",
            r#"digraph G { start [shape="Mdiamond"] work [shape="box", llm_provider="llama"] done [shape="Msquare"] start -> work -> done }"#,
        ),
        (
            "shape.dot",
            r#"digraph G { start [shape="Mdiamond"] work [shape="ellipse"] done [shape="Msquare"] start -> work -> done }"#,
        ),
        (
            "conflict.dot",
            r#"digraph G { start [shape="box", llm_provider="claude"] done [shape="Msquare"] start -> done }"#,
        ),
        (
            "integer-shape.dot",
            r#"digraph G { start [shape="Mdiamond"] work [shape=123, llm_provider="claude"] done [shape="Msquare"] start -> work -> done }"#,
        ),
        (
            "boolean-prompt.dot",
            r#"digraph G { start [shape="Mdiamond"] work [shape="diamond", prompt=true, llm_provider="codex"] done [shape="Msquare"] start -> work -> done }"#,
        ),
        (
            "float-handler.dot",
            r#"digraph G { start [shape="Mdiamond"] work [handler=1.5, llm_provider="gemini"] done [shape="Msquare"] start -> work -> done }"#,
        ),
        (
            "duration-provider.dot",
            r#"digraph G { start [shape="Mdiamond", llm_provider=1s] done [shape="Msquare"] start -> done }"#,
        ),
    ];

    for (name, source) in cases {
        let pipeline = write_pipeline(fixture.path(), name, source);
        let logs = fixture.path().join(format!("{name}-logs"));
        let output = run_with_shims(
            &[
                "run",
                pipeline.to_str().unwrap(),
                "--logs",
                logs.to_str().unwrap(),
            ],
            shims.path(),
            &marker,
        );
        assert!(!output.status.success(), "{name} unexpectedly ran");
        assert!(!marker.exists(), "{name} started a provider process");
        assert!(
            !logs.join("checkpoint.json").exists(),
            "{name} wrote a checkpoint"
        );
    }
}

#[test]
fn invalid_or_reserved_controls_fail_before_provider_or_fresh_checkpoint_side_effects() {
    let fixture = tempfile::tempdir().unwrap();
    let shims = tempfile::tempdir().unwrap();
    provider_shims(shims.path());
    let marker = fixture.path().join("providers-started");
    let cases = [
        (
            "reserved.dot",
            "graph [dry_run=\"false\"]",
            Vec::<&str>::new(),
            "reserved",
        ),
        ("steps.dot", "", vec!["--max-steps", "0"], "max_steps"),
        (
            "budget.dot",
            "",
            vec!["--max-budget-usd", "NaN"],
            "max_budget_usd",
        ),
    ];

    for (name, graph_attr, extra, expected) in cases {
        let pipeline = write_pipeline(
            fixture.path(),
            name,
            &format!(
                "digraph G {{ {graph_attr} start [shape=\"Mdiamond\"] work [shape=\"box\", llm_provider=\"codex\", timeout=1s] done [shape=\"Msquare\"] start -> work -> done }}"
            ),
        );
        let logs = fixture.path().join(format!("{name}-logs"));
        fs::create_dir_all(&logs).unwrap();
        let checkpoint = logs.join("checkpoint.json");
        fs::write(&checkpoint, "sentinel").unwrap();
        let mut args = vec![
            "run",
            pipeline.to_str().unwrap(),
            "--logs",
            logs.to_str().unwrap(),
            "--fresh",
        ];
        args.extend(extra);

        let output = run_with_shims(&args, shims.path(), &marker);
        assert!(!output.status.success(), "{name} unexpectedly ran");
        let diagnostics = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(diagnostics.contains(expected), "{name}: {diagnostics}");
        assert!(!marker.exists(), "{name} started a provider");
        assert_eq!(fs::read_to_string(&checkpoint).unwrap(), "sentinel");
    }
}

#[test]
fn unsupported_topology_fails_before_provider_or_fresh_checkpoint_side_effects() {
    let fixture = tempfile::tempdir().unwrap();
    let shims = tempfile::tempdir().unwrap();
    provider_shims(shims.path());
    let marker = fixture.path().join("providers-started");
    let cases = [
        (
            "parallel.dot",
            r#"digraph G {
                start [shape="Mdiamond"]
                work [shape="box", llm_provider="codex", timeout=1s]
                fork [shape="component"]
                left [shape="diamond"]
                right [shape="diamond"]
                done [shape="Msquare"]
                start -> work -> fork
                fork -> left
                fork -> right
                left -> done
                right -> done
            }"#,
            "one successor per step",
        ),
        (
            "fan-in.dot",
            r#"digraph G {
                start [shape="Mdiamond"]
                work [shape="box", llm_provider="codex", timeout=1s]
                merge [shape="tripleoctagon"]
                done [shape="Msquare"]
                start -> work -> merge -> done
            }"#,
            "cannot merge branch results",
        ),
    ];

    for (name, source, expected) in cases {
        let pipeline = write_pipeline(fixture.path(), name, source);
        let logs = fixture.path().join(format!("{name}-logs"));
        fs::create_dir_all(&logs).unwrap();
        let checkpoint = logs.join("checkpoint.json");
        fs::write(&checkpoint, "sentinel").unwrap();

        let output = run_with_shims(
            &[
                "run",
                pipeline.to_str().unwrap(),
                "--logs",
                logs.to_str().unwrap(),
                "--fresh",
            ],
            shims.path(),
            &marker,
        );

        assert!(!output.status.success(), "{name} unexpectedly ran");
        let rendered = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(rendered.contains(expected), "{name}: {rendered}");
        assert!(!rendered.contains("Completed nodes"), "{name}: {rendered}");
        assert!(!marker.exists(), "{name} started a provider");
        assert_eq!(fs::read_to_string(&checkpoint).unwrap(), "sentinel");
    }
}

#[test]
fn invalid_graph_quality_limit_fails_before_fresh_checkpoint_side_effects() {
    let fixture = tempfile::tempdir().unwrap();
    let shims = tempfile::tempdir().unwrap();
    provider_shims(shims.path());
    let marker = fixture.path().join("providers-started");
    let pipeline = write_pipeline(
        fixture.path(),
        "quality-zero.dot",
        r#"digraph G {
            start [shape="Mdiamond"]
            check [shape="ellipse", type="quality", max_fix_iterations=0]
            work [shape="box", llm_provider="codex", timeout=1s]
            done [shape="Msquare"]
            start -> check -> work -> done
        }"#,
    );
    let logs = fixture.path().join("logs");
    fs::create_dir_all(&logs).unwrap();
    let checkpoint = logs.join("checkpoint.json");
    fs::write(&checkpoint, "sentinel").unwrap();

    let output = run_with_shims(
        &[
            "run",
            pipeline.to_str().unwrap(),
            "--logs",
            logs.to_str().unwrap(),
            "--fresh",
        ],
        shims.path(),
        &marker,
    );

    assert!(!output.status.success());
    let diagnostics = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(diagnostics.contains("max_fix_iterations"), "{diagnostics}");
    assert!(diagnostics.contains("check"), "{diagnostics}");
    assert!(
        !marker.exists(),
        "invalid quality control started a provider"
    );
    assert_eq!(fs::read_to_string(&checkpoint).unwrap(), "sentinel");
}

#[test]
fn built_in_budget_warning_does_not_claim_an_explicit_cli_flag() {
    let fixture = tempfile::tempdir().unwrap();
    let shims = tempfile::tempdir().unwrap();
    provider_shims(shims.path());
    let marker = fixture.path().join("providers-started");
    let pipeline = write_pipeline(
        fixture.path(),
        "built-in-budget.dot",
        r#"digraph G {
            start [shape="Mdiamond"]
            work [shape="box", llm_provider="codex", timeout=1s]
            done [shape="Msquare"]
            start -> work -> done
        }"#,
    );

    let output = run_with_shims(
        &["run", pipeline.to_str().unwrap(), "--dry-run"],
        shims.path(),
        &marker,
    );

    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let diagnostics = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(diagnostics.contains("implicit $200"), "{diagnostics}");
    assert!(!diagnostics.contains("--max-budget-usd"), "{diagnostics}");
    assert!(!marker.exists(), "dry-run started a provider");
}

#[test]
fn dry_run_uses_typed_controls_and_starts_no_provider() {
    let fixture = tempfile::tempdir().unwrap();
    let shims = tempfile::tempdir().unwrap();
    provider_shims(shims.path());
    let marker = fixture.path().join("providers-started");
    let logs = fixture.path().join("logs");
    let pipeline = write_pipeline(
        fixture.path(),
        "dry.dot",
        r#"digraph G { start [shape="Mdiamond"] work [shape="box", llm_provider="codex"] done [shape="Msquare"] start -> work -> done }"#,
    );
    let output = run_with_shims(
        &[
            "run",
            pipeline.to_str().unwrap(),
            "--logs",
            logs.to_str().unwrap(),
            "--dry-run",
        ],
        shims.path(),
        &marker,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!marker.exists());
}

#[test]
fn directory_mode_prepares_every_plan_before_starting_any_provider() {
    let fixture = tempfile::tempdir().unwrap();
    let pipelines = fixture.path().join("pipelines");
    fs::create_dir(&pipelines).unwrap();
    let shims = tempfile::tempdir().unwrap();
    provider_shims(shims.path());
    let marker = fixture.path().join("providers-started");
    write_pipeline(
        &pipelines,
        "01-valid.dot",
        r#"digraph G { start [shape="Mdiamond"] work [shape="box", llm_provider="codex", timeout=1s] done [shape="Msquare"] start -> work -> done }"#,
    );
    write_pipeline(
        &pipelines,
        "02-reserved.dot",
        r#"digraph G { graph [max_steps=999] start [shape="Mdiamond"] done [shape="Msquare"] start -> done }"#,
    );

    let output = Command::new(pas())
        .args(["run", pipelines.to_str().unwrap(), "--fresh"])
        .current_dir(fixture.path())
        .env("PATH", shims.path())
        .env("PAS_TEST_PROVIDER_MARKER", &marker)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        !marker.exists(),
        "a provider started before all plans were prepared"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("max_steps"));
}

#[test]
fn tool_handler_uses_typed_canonical_workdir() {
    let fixture = tempfile::tempdir().unwrap();
    let workdir = tempfile::tempdir().unwrap();
    let logs = fixture.path().join("logs");
    let pipeline = write_pipeline(
        fixture.path(),
        "workdir.dot",
        r#"digraph G { start [shape="Mdiamond"] tool [shape="parallelogram", tool_command="pwd > typed-workdir-marker"] done [shape="Msquare"] start -> tool -> done }"#,
    );
    let output = Command::new(pas())
        .args([
            "run",
            pipeline.to_str().unwrap(),
            "--workdir",
            workdir.path().to_str().unwrap(),
            "--logs",
            logs.to_str().unwrap(),
            "--fresh",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let actual = fs::read_to_string(workdir.path().join("typed-workdir-marker")).unwrap();
    assert_eq!(
        Path::new(actual.trim()).canonicalize().unwrap(),
        workdir.path().canonicalize().unwrap()
    );
}

#[test]
fn run_renders_missing_provider_as_typed_diagnostic_before_side_effects() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap();
    let changelog = fs::read_to_string(workspace.join("CHANGELOG.md")).unwrap();
    assert!(changelog
        .contains("`pas validate` and `pas run` report the blocking `provider_required` rule"));

    let fixture = tempfile::tempdir().unwrap();
    let shims = tempfile::tempdir().unwrap();
    provider_shims(shims.path());
    let marker = fixture.path().join("providers-started");
    let logs = fixture.path().join("logs");
    let pipeline = write_pipeline(
        fixture.path(),
        "missing-provider.dot",
        r#"digraph G {
            start [shape="Mdiamond"]
            work [shape="box", prompt="work"]
            done [shape="Msquare"]
            start -> work -> done
        }"#,
    );

    let output = run_with_shims(
        &[
            "run",
            pipeline.to_str().unwrap(),
            "--logs",
            logs.to_str().unwrap(),
            "--fresh",
        ],
        shims.path(),
        &marker,
    );

    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("[ERROR] provider_required (node: work):"),
        "stdout={stdout}\nstderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("Fix: Add llm_provider=\"claude\", \"codex\", or \"gemini\""),
        "{stdout}"
    );
    assert!(
        !marker.exists(),
        "missing provider started a provider process"
    );
    assert!(
        !logs.join("checkpoint.json").exists(),
        "missing provider wrote a checkpoint"
    );
}

#[test]
fn validate_renders_every_semantic_error_with_rule_node_and_fix() {
    let fixture = tempfile::tempdir().unwrap();
    let pipeline = write_pipeline(
        fixture.path(),
        "diagnostics.dot",
        r#"digraph G {
            start [shape="Mdiamond"]
            bad_provider [shape="box", llm_provider="llama"]
            bad_shape [shape="ellipse"]
            done [shape="Msquare"]
            start -> bad_provider -> bad_shape -> done
        }"#,
    );

    let output = Command::new(pas())
        .args(["validate", pipeline.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("[ERROR] provider_valid (node: bad_provider):"),
        "{stdout}"
    );
    assert!(
        stdout.contains("Fix: Use claude/anthropic, codex/openai, or gemini/google"),
        "{stdout}"
    );
    assert!(
        stdout.contains("[ERROR] semantic_unknown (node: bad_shape):"),
        "{stdout}"
    );
    assert!(
        stdout.contains("Fix: Use a supported shape or name a registered handler with type="),
        "{stdout}"
    );
}

#[test]
fn validate_renders_unsupported_topology_as_a_blocking_semantic_error() {
    let fixture = tempfile::tempdir().unwrap();
    let cases = [
        (
            "parallel.dot",
            r#"digraph G {
                start [shape="Mdiamond"]
                fork [shape="component"]
                left [shape="diamond"]
                right [shape="diamond"]
                done [shape="Msquare"]
                start -> fork
                fork -> left
                fork -> right
                left -> done
                right -> done
            }"#,
            "fork",
            "Rewrite this node as a linear sequence",
        ),
        (
            "fan-in.dot",
            r#"digraph G {
                start [shape="Mdiamond"]
                merge [shape="tripleoctagon"]
                done [shape="Msquare"]
                start -> merge -> done
            }"#,
            "merge",
            "Remove the fan-in node",
        ),
    ];

    for (name, source, node, fix) in cases {
        let pipeline = write_pipeline(fixture.path(), name, source);
        let output = Command::new(pas())
            .args(["validate", pipeline.to_str().unwrap()])
            .output()
            .unwrap();

        assert!(!output.status.success(), "{name} unexpectedly validated");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains(&format!(
                "[ERROR] unsupported_execution_topology (node: {node}):"
            )),
            "{stdout}"
        );
        assert!(stdout.contains(&format!("Fix: {fix}")), "{stdout}");

        let info = Command::new(pas())
            .args(["info", pipeline.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(!info.status.success(), "{name} unexpectedly produced info");
        let info_rendered = format!(
            "{}{}",
            String::from_utf8_lossy(&info.stdout),
            String::from_utf8_lossy(&info.stderr)
        );
        assert!(info_rendered.contains(node), "{info_rendered}");
    }
}
