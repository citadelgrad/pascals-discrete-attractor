use std::path::{Path, PathBuf};

use attractor_pipeline::{ExecutionPlan, PipelineGraph};

fn dot_files_under(root: &Path, files: &mut Vec<PathBuf>) {
    let mut entries = std::fs::read_dir(root)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", root.display()))
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            dot_files_under(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "dot") {
            files.push(path);
        }
    }
}

#[test]
fn checked_in_non_web_dot_files_compile_with_canonical_semantics() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap();
    let mut files = Vec::new();
    for relative in ["pipelines", "templates", "docs/examples"] {
        dot_files_under(&workspace.join(relative), &mut files);
    }
    assert!(!files.is_empty(), "expected maintained DOT contract files");

    let mut failures = Vec::new();
    for path in files {
        let source = std::fs::read_to_string(&path).unwrap();
        let result = attractor_dot::parse(&source)
            .map_err(|error| error.to_string())
            .and_then(|dot| PipelineGraph::from_dot(dot).map_err(|error| error.to_string()))
            .and_then(|graph| ExecutionPlan::compile(graph).map_err(|error| error.to_string()));
        if let Err(error) = result {
            failures.push(format!("{}: {error}", path.display()));
        }
    }

    assert!(
        failures.is_empty(),
        "non-web DOT contract failures:\n{}",
        failures.join("\n")
    );
}

fn complete_dot_fences(source: &str) -> Vec<&str> {
    let mut fences = Vec::new();
    let mut rest = source;
    while let Some(start) = rest.find("```dot") {
        rest = &rest[start + "```dot".len()..];
        let Some(end) = rest.find("```") else {
            break;
        };
        let fence = rest[..end].trim();
        if fence.starts_with("digraph ") {
            fences.push(fence);
        }
        rest = &rest[end + "```".len()..];
    }
    fences
}

#[test]
fn authoritative_markdown_dot_examples_compile_with_canonical_semantics() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap();
    let documents = [
        "README.md",
        "docs/dot-dialect.md",
        "docs/guide.md",
        "docs/cli-reference.md",
        "docs/task-verification.md",
        "templates/pas.md",
    ];

    let mut checked = 0;
    let mut failures = Vec::new();
    for relative in documents {
        let source = std::fs::read_to_string(workspace.join(relative)).unwrap();
        for (index, dot) in complete_dot_fences(&source).into_iter().enumerate() {
            checked += 1;
            let result = attractor_dot::parse(dot)
                .map_err(|error| error.to_string())
                .and_then(|parsed| {
                    PipelineGraph::from_dot(parsed).map_err(|error| error.to_string())
                })
                .and_then(|graph| ExecutionPlan::compile(graph).map_err(|error| error.to_string()));
            if let Err(error) = result {
                failures.push(format!("{relative} DOT fence {}: {error}", index + 1));
            }
        }
    }

    assert!(
        checked > 0,
        "expected complete DOT fences in maintained docs"
    );
    assert!(
        failures.is_empty(),
        "Markdown DOT contract failures:\n{}",
        failures.join("\n")
    );
}

#[test]
fn maintained_docs_do_not_advertise_stale_claude_only_or_validation_contracts() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap();
    let forbidden = [
        ("README.md", "Under the hood, each `codergen` node runs:"),
        ("README.md", "| `prompt` (required) |"),
        (
            "README.md",
            "Conditional nodes (`shape=diamond`) automatically instruct Claude Code",
        ),
        (
            "README.md",
            "Expand `${ctx.key}` references in node attributes",
        ),
        (
            "README.md",
            "The default `codergen` handler uses your local Claude Code installation",
        ),
        (
            "README.md",
            "Each provider-backed `codergen` node must",
        ),
        (
            "docs/guide.md",
            "Each node becomes a Claude Code session that runs your prompt.",
        ),
        (
            "docs/guide.md",
            "Conditional nodes let Claude's response determine",
        ),
        (
            "docs/guide.md",
            "The handler scans Claude's response for one of the edge labels",
        ),
        (
            "docs/guide.md",
            "Write the prompt so Claude outputs one of the labels",
        ),
        (
            "docs/guide.md",
            "`preferred_label` — the label extracted from Claude's response",
        ),
        (
            "docs/guide.md",
            "Set `llm_provider` on any `box` or `diamond` node:",
        ),
        (
            "docs/guide.md",
            "Required for every resolved `codergen` node",
        ),
        (
            "docs/guide.md",
            "Every `codergen` node must resolve an explicit provider",
        ),
        (
            "docs/guide.md",
            "`\"Fix the bug\"` gives Claude nothing to work with",
        ),
        (
            "docs/cli-reference.md",
            "Each `box` node spawns a Claude Code session",
        ),
        (
            "docs/cli-reference.md",
            "Working directory for Claude Code sessions during the run phase.",
        ),
        (
            "docs/cli-reference.md",
            "Claude Code can read, edit, and create files relative to this path.",
        ),
        ("docs/cli-reference.md", "Runs all 11 lint rules"),
        (
            "docs/cli-reference.md",
            "**`claude`** must be in your PATH.",
        ),
        (
            "docs/dot-dialect.md",
            "| `box` | **Task** -- runs the selected provider CLI with the prompt | CodergenHandler | `prompt`, `llm_provider` |",
        ),
        (
            "docs/dot-dialect.md",
            "Required for nodes resolved to `codergen`.",
        ),
        (
            "templates/pas.md",
            "Claude is asked to output one of the labels",
        ),
        (
            "templates/pas.md",
            "Detailed instructions for what Claude should do in this step.",
        ),
        (
            "crates/attractor-pipeline/src/lib.rs",
            "and the 13 built-in lint rules",
        ),
        (
            "crates/attractor-cli/src/commands/generate.rs",
            "Each node is a Claude Code session",
        ),
        (
            "crates/attractor-cli/src/commands/generate.rs",
            "REQUIRED on every work/decision node",
        ),
        (
            "crates/attractor-pipeline/src/provider_defaults.rs",
            "every runtime node (`box`/`diamond`,",
        ),
        (
            "CHANGELOG.md",
            "Runtime nodes (`shape=\"box\"` or `shape=\"diamond\"`,",
        ),
    ];

    let mut failures = Vec::new();
    for (relative, stale_claim) in forbidden {
        let source = std::fs::read_to_string(workspace.join(relative)).unwrap();
        if source.contains(stale_claim) {
            failures.push(format!("{relative}: {stale_claim}"));
        }
    }

    assert!(
        failures.is_empty(),
        "stale documentation contracts:\n{}",
        failures.join("\n")
    );

    let required = [
        (
            "README.md",
            "By default, a diamond without a prompt is a pass-through",
        ),
        (
            "README.md",
            "Variable transforms expand `${key}` references in node prompts from graph-level attributes",
        ),
        (
            "README.md",
            "There is no implicit runtime provider",
        ),
        (
            "README.md",
            "Every node whose resolved handler consumes a provider must",
        ),
        (
            "docs/guide.md",
            "A conditional without a prompt is pass-through routing",
        ),
        (
            "docs/guide.md",
            "Write the prompt so the selected provider outputs one of the labels",
        ),
        (
            "docs/guide.md",
            "`preferred_label` — the label extracted from the selected provider's response",
        ),
        (
            "docs/guide.md",
            "Set `llm_provider` on every node whose resolved handler consumes a provider.",
        ),
        (
            "docs/guide.md",
            "An unprompted pass-through diamond does not consume a provider, while a registered custom provider-consuming handler does regardless of its omitted or custom shape.",
        ),
        (
            "docs/guide.md",
            "Required whenever the resolved handler consumes a provider",
        ),
        (
            "docs/guide.md",
            "Every node whose resolved handler consumes a provider must resolve an explicit provider",
        ),
        (
            "docs/guide.md",
            "`\"Fix the bug\"` gives the selected provider too little context",
        ),
        (
            "docs/cli-reference.md",
            "Runs canonical semantic compilation followed by nine structural checks",
        ),
        (
            "docs/cli-reference.md",
            "Working directory for selected provider CLI sessions during the run phase.",
        ),
        (
            "docs/cli-reference.md",
            "Selected provider CLIs and tool commands use this path as their working directory.",
        ),
        (
            "docs/cli-reference.md",
            "[ERROR] provider_valid (node: analyze):",
        ),
        ("docs/cli-reference.md", "Unselected provider binaries are"),
        (
            "docs/dot-dialect.md",
            "`prompt` is optional | CodergenHandler | `llm_provider`",
        ),
        (
            "docs/dot-dialect.md",
            "Required whenever the resolved handler consumes a provider.",
        ),
        (
            "templates/pas.md",
            "By default, a diamond without a prompt is pass-through",
        ),
        (
            "templates/pas.md",
            "Detailed instructions for what the selected provider should do in this step.",
        ),
        (
            "crates/attractor-pipeline/src/lib.rs",
            "canonical semantic compilation followed by nine structural checks",
        ),
        (
            "README.md",
            "Setting `type=\"codergen\"` explicitly",
        ),
        (
            "docs/dot-dialect.md",
            "`diamond` with explicit `type=\"codergen\"`",
        ),
        (
            "templates/pas.md",
            "Explicit `type=\"codergen\"` is the exception",
        ),
        (
            "crates/attractor-cli/src/commands/generate.rs",
            "REQUIRED on every provider-backed node",
        ),
        (
            "crates/attractor-pipeline/src/provider_defaults.rs",
            "resolved handler consumes a provider",
        ),
        (
            "CHANGELOG.md",
            "resolved handler consumes a provider",
        ),
        (
            "CHANGELOG.md",
            "A diamond without a prompt is pass-through",
        ),
        (
            "CHANGELOG.md",
            "selects `type=\"codergen\"` explicitly",
        ),
        (
            "CHANGELOG.md",
            "custom provider-consuming handler with an omitted or custom shape requires a provider",
        ),
    ];
    let mut missing = Vec::new();
    for (relative, canonical_claim) in required {
        let source = std::fs::read_to_string(workspace.join(relative)).unwrap();
        if !source.contains(canonical_claim) {
            missing.push(format!("{relative}: {canonical_claim}"));
        }
    }
    assert!(
        missing.is_empty(),
        "missing canonical documentation contracts:\n{}",
        missing.join("\n")
    );
}

#[test]
fn task_verification_distinguishes_plan_binding_from_runtime_handler_failures() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap();
    let source = std::fs::read_to_string(workspace.join("docs/task-verification.md")).unwrap();

    assert!(
        source.contains(
            "Before execution, PAS verifies that every handler required by the compiled plan is registered"
        ),
        "task verification must document pre-execution handler registry binding"
    );
    assert!(
        source.contains(
            "An unavailable handler or changed provider capability is a `ValidationError` before any handler runs"
        ),
        "task verification must document plan-binding failures as ValidationError"
    );
    assert!(
        source.contains(
            "After successful plan binding, a handler execution failure is a `HandlerError`"
        ),
        "task verification must reserve HandlerError for runtime handler failures"
    );
    assert!(
        !source.contains(
            "If no handler is registered, execution fails immediately with a `HandlerError`"
        ),
        "task verification still advertises removed late handler resolution"
    );
}

#[test]
fn semantic_attribute_types_and_provider_handler_boundary_are_documented() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap();
    let dialect = std::fs::read_to_string(workspace.join("docs/dot-dialect.md")).unwrap();
    let verification =
        std::fs::read_to_string(workspace.join("docs/task-verification.md")).unwrap();
    let handler =
        std::fs::read_to_string(workspace.join("crates/attractor-pipeline/src/handler.rs"))
            .unwrap();

    assert!(dialect.contains("Semantic discriminator attributes must be strings"));
    assert!(dialect.contains("`InvalidAttributeType` compilation error"));
    for key in [
        "shape",
        "type",
        "node_type",
        "handler",
        "prompt",
        "llm_provider",
        "class",
        "classes",
        "stylesheet",
        "model_stylesheet",
    ] {
        assert!(
            dialect.contains(&format!("`{key}`")),
            "DOT contract does not name string semantic attribute {key}"
        );
    }

    assert!(verification
        .contains("A provider-consuming custom handler exposes a `ProviderNodeHandler`"));
    assert!(verification
        .contains("it cannot opt in to provider use while inheriting raw-node dispatch"));
    assert!(dialect.contains(
        "A registered custom handler may use an omitted or otherwise unknown custom shape"
    ));
    assert!(dialect.contains("cannot override a known built-in role shape"));
    assert!(handler.contains("pub trait ProviderNodeHandler"));
    assert!(handler.contains("fn provider_handler(&self) -> Option<&dyn ProviderNodeHandler>"));
    assert!(handler.contains("handler.0.provider_handler().is_some()"));
}

#[test]
fn provider_guide_matches_the_tested_gemini_invocation_contract() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap();
    let source = std::fs::read_to_string(workspace.join("docs/guide.md")).unwrap();

    assert!(
        source.contains(
            "Uses `--output-format json --approval-mode yolo` with the prompt as a positional argument"
        ),
        "provider guide must document Gemini's actual approval and prompt arguments"
    );
    assert!(
        source.contains("PAS does not pass a `--sandbox` flag to Gemini"),
        "provider guide must state the Gemini sandbox-flag boundary"
    );
    assert!(
        !source.contains("`-p` for the prompt, plus `--sandbox none`"),
        "provider guide still advertises obsolete Gemini arguments"
    );
}

#[test]
fn provider_guide_documents_the_canonical_gemini_cli_package() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap();
    let source = std::fs::read_to_string(workspace.join("docs/guide.md")).unwrap();

    assert!(
        source.contains("Gemini: `npm install -g @google/gemini-cli`"),
        "provider guide must document Google's published Gemini CLI package"
    );
    assert!(
        !source.contains("@anthropic-ai/gemini-cli"),
        "provider guide still advertises the nonexistent Anthropic-scoped Gemini package"
    );
}

#[test]
fn unsupported_parallel_topology_is_documented_truthfully() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap();
    let required = [
        (
            "README.md",
            "PAS rejects multi-edge `component` fan-out and every fan-in node during semantic compilation.",
        ),
        (
            "README.md",
            "A `component` node with at most one outgoing edge remains available as sequential compatibility.",
        ),
        ("docs/guide.md", "Parallel and fan-in compatibility"),
        ("docs/dot-dialect.md", "Unsupported execution topology"),
        (
            "docs/task-verification.md",
            "`unsupported_execution_topology`",
        ),
        (
            "docs/emergence-analysis.md",
            "Sequential compatibility only; multi-edge fan-out is rejected",
        ),
        (
            "CHANGELOG.md",
            "Fail closed on unsupported execution topology",
        ),
    ];
    for (relative, phrase) in required {
        let source = std::fs::read_to_string(workspace.join(relative)).unwrap();
        assert!(
            source.contains(phrase),
            "{relative} must contain {phrase:?}"
        );
    }

    let forbidden = [
        ("README.md", "human gate, parallel fan-out"),
        (
            "README.md",
            "condition evaluation, parallel fan-out/fan-in, manager loops",
        ),
        (
            "docs/emergence-analysis.md",
            "| component | parallel | Fan-out |",
        ),
    ];
    for (relative, phrase) in forbidden {
        let source = std::fs::read_to_string(workspace.join(relative)).unwrap();
        assert!(
            !source.contains(phrase),
            "{relative} still advertises unsupported behavior: {phrase:?}"
        );
    }
}

#[test]
fn goal_gate_docs_use_canonical_compiled_exit_membership() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap();
    let source = std::fs::read_to_string(workspace.join("docs/task-verification.md")).unwrap();

    assert!(
        source.contains(
            "When the pipeline reaches the canonical exit node recorded by `ExecutionPlan`, the engine checks"
        ),
        "goal-gate verification must refer to compiled exit membership"
    );
    assert!(
        source.contains(
            "Exit nodes may use canonical `shape=\"Msquare\"`, explicit `type=\"exit\"`, or a compatible magic-ID form"
        ),
        "goal-gate verification must cover all supported exit forms"
    );
    assert!(
        !source.contains("reaches the exit node (`shape=\"Msquare\"`)"),
        "goal-gate verification still limits exits to their canonical shape"
    );
}

#[test]
fn cli_info_docs_describe_resolved_execution_semantics() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap();
    let source = std::fs::read_to_string(workspace.join("docs/cli-reference.md")).unwrap();

    assert!(
        source.contains("a list of all nodes with their resolved kind, handler, and provider"),
        "CLI reference must describe the semantic fields emitted by pas info"
    );
    assert!(
        !source.contains("a list of all nodes with their shapes and types"),
        "CLI reference still describes the pre-ExecutionPlan info output"
    );
}

#[test]
fn typed_run_configuration_boundary_is_documented_consistently() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap();
    let required = [
        (
            "README.md",
            "Run controls are immutable typed configuration",
        ),
        (
            "docs/cli-reference.md",
            "caller > manifest > permitted graph defaults > built-ins",
        ),
        (
            "docs/cli-reference.md",
            "before `--fresh` deletes a checkpoint",
        ),
        ("docs/dot-dialect.md", "Reserved graph attributes"),
        ("docs/dot-dialect.md", "`codergen.claude.*`"),
        ("docs/guide.md", "Context contains workflow data only"),
        ("docs/task-verification.md", "RunConfiguration"),
        ("CHANGELOG.md", "typed `RunConfiguration`"),
    ];
    for (relative, claim) in required {
        let source = std::fs::read_to_string(workspace.join(relative)).unwrap();
        assert!(source.contains(claim), "{relative} is missing: {claim}");
    }

    let verification =
        std::fs::read_to_string(workspace.join("docs/task-verification.md")).unwrap();
    for stale in [
        "context tracks the full pipeline state",
        "The engine also writes the current outcome into `context[\"outcome\"]`",
        "**Relevant code:** `crates/attractor-types/src/lib.rs` — `Context` implementation.",
    ] {
        assert!(
            !verification.contains(stale),
            "stale Context claim: {stale}"
        );
    }
}

#[test]
fn cli_dry_run_docs_are_provider_neutral() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap();
    let source = std::fs::read_to_string(workspace.join("docs/cli-reference.md")).unwrap();

    assert!(
        source.contains("doesn't execute any selected provider CLI or tool command"),
        "CLI reference must describe dry-run isolation for every provider and tools"
    );
    assert!(
        !source.contains("doesn't spawn any Claude Code sessions"),
        "CLI reference still describes dry run as Claude-only"
    );
}

#[test]
fn documented_unprompted_diamond_exception_matches_compiled_semantics() {
    use attractor_pipeline::{HandlerIdentity, LlmProvider, ResolvedNodeKind};

    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap();
    let changelog = std::fs::read_to_string(workspace.join("CHANGELOG.md")).unwrap();
    assert!(changelog.contains("resolved handler consumes a provider"));
    assert!(changelog.contains("A diamond without a prompt is pass-through"));
    assert!(changelog.contains("selects `type=\"codergen\"` explicitly"));
    assert!(changelog.contains(
        "custom provider-consuming handler with an omitted or custom shape requires a provider"
    ));
    assert!(!changelog.contains("Runtime nodes (`shape=\"box\"` or `shape=\"diamond\"`,"));

    let cases = [
        (
            "plain unprompted diamond",
            r#"subject [shape="diamond"]"#,
            ResolvedNodeKind::Conditional { llm_backed: false },
            HandlerIdentity::Conditional,
            None,
        ),
        (
            "unprompted diamond with unused provider",
            r#"subject [shape="diamond", llm_provider="codex"]"#,
            ResolvedNodeKind::Conditional { llm_backed: false },
            HandlerIdentity::Conditional,
            None,
        ),
        (
            "unprompted diamond with explicit codergen",
            r#"subject [shape="diamond", type="codergen", llm_provider="codex"]"#,
            ResolvedNodeKind::Conditional { llm_backed: true },
            HandlerIdentity::Codergen,
            Some(LlmProvider::Codex),
        ),
        (
            "prompted diamond",
            r#"subject [shape="diamond", prompt="choose", llm_provider="codex"]"#,
            ResolvedNodeKind::Conditional { llm_backed: true },
            HandlerIdentity::Codergen,
            Some(LlmProvider::Codex),
        ),
    ];

    for (name, subject, kind, handler, provider) in cases {
        let source = format!(
            "digraph G {{ start [shape=\"Mdiamond\"] {subject} done [shape=\"Msquare\"] start -> subject -> done }}"
        );
        let plan = attractor_dot::parse(&source)
            .map_err(|error| error.to_string())
            .and_then(|dot| PipelineGraph::from_dot(dot).map_err(|error| error.to_string()))
            .and_then(|graph| ExecutionPlan::compile(graph).map_err(|error| error.to_string()))
            .unwrap_or_else(|error| panic!("{name} did not compile: {error}"));
        let resolved = plan.node("subject").unwrap();
        assert_eq!(resolved.kind, kind, "kind for {name}");
        assert_eq!(resolved.handler, handler, "handler for {name}");
        assert_eq!(resolved.provider, provider, "provider for {name}");
    }
}
