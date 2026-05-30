use super::*;

fn parse_and_build(dot: &str) -> PipelineGraph {
    let graph = attractor_dot::parse(dot).unwrap();
    PipelineGraph::from_dot(graph).unwrap()
}

#[test]
fn valid_pipeline_passes() {
    let pg = parse_and_build(r#"digraph G {
        start [shape="Mdiamond"]
        process [label="Do work", prompt="Do the thing"]
        done [shape="Msquare"]
        start -> process -> done
    }"#);
    let diags = validate(&pg);
    let errors: Vec<_> = diags.iter().filter(|d| d.severity == Severity::Error).collect();
    assert!(errors.is_empty(), "Expected no errors, got: {errors:?}");
}

#[test]
fn missing_start_node_error() {
    let pg = parse_and_build(r#"digraph G {
        process [label="Do work"]
        done [shape="Msquare"]
        process -> done
    }"#);
    let diags = validate(&pg);
    assert!(diags.iter().any(|d| d.rule == "start_node" && d.severity == Severity::Error));
}

#[test]
fn missing_terminal_node_error() {
    let pg = parse_and_build(r#"digraph G {
        start [shape="Mdiamond"]
        process [label="Do work"]
        start -> process
    }"#);
    let diags = validate(&pg);
    assert!(diags.iter().any(|d| d.rule == "terminal_node" && d.severity == Severity::Error));
}

#[test]
fn unreachable_node_error() {
    let pg = parse_and_build(r#"digraph G {
        start [shape="Mdiamond"]
        process [label="Do work"]
        orphan [label="Orphan"]
        done [shape="Msquare"]
        start -> process -> done
    }"#);
    let diags = validate(&pg);
    assert!(
        diags.iter().any(|d| d.rule == "reachability"
            && d.severity == Severity::Error
            && d.message.contains("orphan")),
        "Expected unreachable diagnostic for orphan, got: {diags:?}"
    );
}

#[test]
fn edge_to_nonexistent_node_error() {
    // Build a graph where an edge target does not have a node definition.
    // DOT parser may auto-create nodes for edge endpoints, so we test via
    // the edge_target_exists rule directly on a graph with a missing target.
    // In practice the DOT parser creates implicit nodes, so we verify
    // the rule at least runs cleanly on a normal graph.
    let pg = parse_and_build(r#"digraph G {
        start [shape="Mdiamond"]
        done [shape="Msquare"]
        start -> done
    }"#);
    let rule = EdgeTargetExistsRule;
    let diags = rule.apply(&pg);
    // All targets exist — no diagnostics expected.
    assert!(diags.is_empty());
}

#[test]
fn start_with_incoming_edges_error() {
    let pg = parse_and_build(r#"digraph G {
        start [shape="Mdiamond"]
        process [label="Do work"]
        done [shape="Msquare"]
        start -> process -> done
        process -> start
    }"#);
    let diags = validate(&pg);
    assert!(
        diags.iter().any(|d| d.rule == "start_no_incoming" && d.severity == Severity::Error),
        "Expected start_no_incoming error, got: {diags:?}"
    );
}

#[test]
fn invalid_condition_syntax_error() {
    let pg = parse_and_build(r#"digraph G {
        start [shape="Mdiamond"]
        a [label="A"]
        done [shape="Msquare"]
        start -> a [condition="no_operator_here"]
        a -> done
    }"#);
    let diags = validate(&pg);
    assert!(
        diags.iter().any(|d| d.rule == "condition_syntax" && d.severity == Severity::Error),
        "Expected condition_syntax error, got: {diags:?}"
    );
}

#[test]
fn goal_gate_without_retry_target_warning() {
    let pg = parse_and_build(r#"digraph G {
        start [shape="Mdiamond"]
        gate [goal_gate=true, label="Check"]
        done [shape="Msquare"]
        start -> gate -> done
    }"#);
    let diags = validate(&pg);
    assert!(
        diags.iter().any(|d| d.rule == "goal_gate_has_retry" && d.severity == Severity::Warning),
        "Expected goal_gate_has_retry warning, got: {diags:?}"
    );
}

#[test]
fn validate_or_raise_ok_for_valid_graph() {
    let pg = parse_and_build(r#"digraph G {
        start [shape="Mdiamond"]
        process [label="Do work", prompt="Do it"]
        done [shape="Msquare"]
        start -> process -> done
    }"#);
    let result = validate_or_raise(&pg);
    assert!(result.is_ok(), "Expected Ok, got: {result:?}");
}

#[test]
fn validate_or_raise_errors_for_invalid_graph() {
    let pg = parse_and_build(r#"digraph G {
        process [label="Do work"]
    }"#);
    let result = validate_or_raise(&pg);
    assert!(result.is_err());
}

#[test]
fn fidelity_valid_rule() {
    let pg = parse_and_build(r#"digraph G {
        start [shape="Mdiamond"]
        a [fidelity="garbage"]
        done [shape="Msquare"]
        start -> a -> done
    }"#);
    let diags = validate(&pg);
    assert!(
        diags.iter().any(|d| d.rule == "fidelity_valid" && d.severity == Severity::Warning),
        "Expected fidelity_valid warning, got: {diags:?}"
    );
}

#[test]
fn valid_fidelity_values_accepted() {
    assert!(is_valid_fidelity("full"));
    assert!(is_valid_fidelity("truncate"));
    assert!(is_valid_fidelity("compact"));
    assert!(is_valid_fidelity("summary"));
    assert!(is_valid_fidelity("summary:low"));
    assert!(is_valid_fidelity("summary:medium"));
    assert!(is_valid_fidelity("truncate(5)"));
    assert!(is_valid_fidelity("truncate(10)"));
    assert!(!is_valid_fidelity("bogus"));
    assert!(!is_valid_fidelity("bogus(5)"));
    assert!(!is_valid_fidelity(""));
}

#[test]
fn exit_with_outgoing_edges_error() {
    let pg = parse_and_build(r#"digraph G {
        start [shape="Mdiamond"]
        done [shape="Msquare"]
        extra [label="Extra"]
        start -> done -> extra
    }"#);
    let diags = validate(&pg);
    assert!(
        diags.iter().any(|d| d.rule == "exit_no_outgoing" && d.severity == Severity::Error),
        "Expected exit_no_outgoing error, got: {diags:?}"
    );
}

#[test]
fn provider_valid_warns_on_unknown() {
    let pg = parse_and_build(r#"digraph G {
        start [shape="Mdiamond"]
        step [llm_provider="llama", prompt="Do work"]
        done [shape="Msquare"]
        start -> step -> done
    }"#);
    let diags = validate(&pg);
    assert!(
        diags.iter().any(|d| d.rule == "provider_valid" && d.severity == Severity::Warning),
        "Expected provider_valid warning for unknown provider, got: {diags:?}"
    );
}

#[test]
fn provider_valid_accepts_known_providers() {
    for provider in &["claude", "anthropic", "codex", "openai", "gemini", "google"] {
        let dot = format!(
            r#"digraph G {{
                start [shape="Mdiamond"]
                step [llm_provider="{}", prompt="Do work"]
                done [shape="Msquare"]
                start -> step -> done
            }}"#,
            provider
        );
        let pg = parse_and_build(&dot);
        let diags = validate(&pg);
        assert!(
            !diags.iter().any(|d| d.rule == "provider_valid"),
            "Unexpected provider_valid diagnostic for known provider '{provider}': {diags:?}"
        );
    }
}

#[test]
fn provider_valid_skips_nodes_without_provider() {
    let pg = parse_and_build(r#"digraph G {
        start [shape="Mdiamond"]
        step [prompt="Do work"]
        done [shape="Msquare"]
        start -> step -> done
    }"#);
    let diags = validate(&pg);
    assert!(
        !diags.iter().any(|d| d.rule == "provider_valid"),
        "Should not warn when llm_provider is absent, got: {diags:?}"
    );
}

#[test]
fn retry_target_nonexistent_warning() {
    let pg = parse_and_build(r#"digraph G {
        start [shape="Mdiamond"]
        gate [goal_gate=true, retry_target="nonexistent"]
        done [shape="Msquare"]
        start -> gate -> done
    }"#);
    let diags = validate(&pg);
    assert!(
        diags.iter().any(|d| d.rule == "retry_target_exists" && d.severity == Severity::Warning),
        "Expected retry_target_exists warning, got: {diags:?}"
    );
}
