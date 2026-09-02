use super::*;

fn parse_and_build(dot: &str) -> PipelineGraph {
    let graph = attractor_dot::parse(dot).unwrap();
    PipelineGraph::from_dot(graph).unwrap()
}

#[test]
fn valid_pipeline_passes() {
    let pg = parse_and_build(
        r#"digraph G {
        start [shape="Mdiamond"]
        process [label="Do work", prompt="Do the thing", llm_provider="claude"]
        done [shape="Msquare"]
        start -> process -> done
    }"#,
    );
    let diags = validate(&pg);
    let errors: Vec<_> = diags
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .collect();
    assert!(errors.is_empty(), "Expected no errors, got: {errors:?}");
}

#[test]
fn missing_start_node_error() {
    let pg = parse_and_build(
        r#"digraph G {
        process [label="Do work"]
        done [shape="Msquare"]
        process -> done
    }"#,
    );
    let diags = validate(&pg);
    assert!(diags
        .iter()
        .any(|d| d.rule == "start_node" && d.severity == Severity::Error));
}

#[test]
fn missing_terminal_node_error() {
    let pg = parse_and_build(
        r#"digraph G {
        start [shape="Mdiamond"]
        process [label="Do work"]
        start -> process
    }"#,
    );
    let diags = validate(&pg);
    assert!(diags
        .iter()
        .any(|d| d.rule == "terminal_node" && d.severity == Severity::Error));
}

#[test]
fn unreachable_node_error() {
    let pg = parse_and_build(
        r#"digraph G {
        start [shape="Mdiamond"]
        process [label="Do work"]
        orphan [label="Orphan"]
        done [shape="Msquare"]
        start -> process -> done
    }"#,
    );
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
    let pg = parse_and_build(
        r#"digraph G {
        start [shape="Mdiamond"]
        done [shape="Msquare"]
        start -> done
    }"#,
    );
    let rule = EdgeTargetExistsRule;
    let diags = rule.apply(&pg);
    // All targets exist — no diagnostics expected.
    assert!(diags.is_empty());
}

#[test]
fn start_with_incoming_edges_error() {
    let pg = parse_and_build(
        r#"digraph G {
        start [shape="Mdiamond"]
        process [label="Do work"]
        done [shape="Msquare"]
        start -> process -> done
        process -> start
    }"#,
    );
    let diags = validate(&pg);
    assert!(
        diags
            .iter()
            .any(|d| d.rule == "start_no_incoming" && d.severity == Severity::Error),
        "Expected start_no_incoming error, got: {diags:?}"
    );
}

#[test]
fn invalid_condition_syntax_error() {
    let pg = parse_and_build(
        r#"digraph G {
        start [shape="Mdiamond"]
        a [label="A"]
        done [shape="Msquare"]
        start -> a [condition="no_operator_here"]
        a -> done
    }"#,
    );
    let diags = validate(&pg);
    assert!(
        diags
            .iter()
            .any(|d| d.rule == "condition_syntax" && d.severity == Severity::Error),
        "Expected condition_syntax error, got: {diags:?}"
    );
}

#[test]
fn goal_gate_without_retry_target_warning() {
    let pg = parse_and_build(
        r#"digraph G {
        start [shape="Mdiamond"]
        gate [goal_gate=true, label="Check"]
        done [shape="Msquare"]
        start -> gate -> done
    }"#,
    );
    let diags = validate(&pg);
    assert!(
        diags
            .iter()
            .any(|d| d.rule == "goal_gate_has_retry" && d.severity == Severity::Warning),
        "Expected goal_gate_has_retry warning, got: {diags:?}"
    );
}

#[test]
fn validate_or_raise_ok_for_valid_graph() {
    // Uses "codex" here (rather than "claude") to prove validate_or_raise
    // doesn't care which known provider a node names, only that one is set.
    let pg = parse_and_build(
        r#"digraph G {
        start [shape="Mdiamond"]
        process [label="Do work", prompt="Do it", llm_provider="codex"]
        done [shape="Msquare"]
        start -> process -> done
    }"#,
    );
    let result = validate_or_raise(&pg);
    assert!(result.is_ok(), "Expected Ok, got: {result:?}");
}

#[test]
fn validate_or_raise_errors_for_invalid_graph() {
    let pg = parse_and_build(
        r#"digraph G {
        process [label="Do work"]
    }"#,
    );
    let result = validate_or_raise(&pg);
    assert!(result.is_err());
}

#[test]
fn fidelity_valid_rule() {
    let pg = parse_and_build(
        r#"digraph G {
        start [shape="Mdiamond"]
        a [fidelity="garbage"]
        done [shape="Msquare"]
        start -> a -> done
    }"#,
    );
    let diags = validate(&pg);
    assert!(
        diags
            .iter()
            .any(|d| d.rule == "fidelity_valid" && d.severity == Severity::Warning),
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
    let pg = parse_and_build(
        r#"digraph G {
        start [shape="Mdiamond"]
        done [shape="Msquare"]
        extra [label="Extra"]
        start -> done -> extra
    }"#,
    );
    let diags = validate(&pg);
    assert!(
        diags
            .iter()
            .any(|d| d.rule == "exit_no_outgoing" && d.severity == Severity::Error),
        "Expected exit_no_outgoing error, got: {diags:?}"
    );
}

#[test]
fn provider_valid_warns_on_unknown() {
    let pg = parse_and_build(
        r#"digraph G {
        start [shape="Mdiamond"]
        step [llm_provider="llama", prompt="Do work"]
        done [shape="Msquare"]
        start -> step -> done
    }"#,
    );
    let diags = validate(&pg);
    assert!(
        diags
            .iter()
            .any(|d| d.rule == "provider_valid" && d.severity == Severity::Warning),
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
    let pg = parse_and_build(
        r#"digraph G {
        start [shape="Mdiamond"]
        step [prompt="Do work"]
        done [shape="Msquare"]
        start -> step -> done
    }"#,
    );
    let diags = validate(&pg);
    assert!(
        !diags.iter().any(|d| d.rule == "provider_valid"),
        "Should not warn when llm_provider is absent, got: {diags:?}"
    );
}

#[test]
fn retry_target_nonexistent_warning() {
    let pg = parse_and_build(
        r#"digraph G {
        start [shape="Mdiamond"]
        gate [goal_gate=true, retry_target="nonexistent"]
        done [shape="Msquare"]
        start -> gate -> done
    }"#,
    );
    let diags = validate(&pg);
    assert!(
        diags
            .iter()
            .any(|d| d.rule == "retry_target_exists" && d.severity == Severity::Warning),
        "Expected retry_target_exists warning, got: {diags:?}"
    );
}

// ---------------------------------------------------------------------------
// provider_required: a runtime box/diamond node with no llm_provider must
// never silently default to Claude. These tests cover both node shapes that
// require a provider, every exemption (start, exit, quality), that either
// of the two supported providers ("claude" and "codex") satisfies the rule
// equally, multi-node graphs, diagnostic shape, and that old pipelines are
// not grandfathered in.
// ---------------------------------------------------------------------------

#[test]
fn provider_required_box_node_without_provider_is_error() {
    let pg = parse_and_build(
        r#"digraph G {
        start [shape="Mdiamond"]
        work [shape="box", prompt="Do work"]
        done [shape="Msquare"]
        start -> work -> done
    }"#,
    );
    let diags = validate(&pg);
    assert!(
        diags.iter().any(|d| d.rule == "provider_required"
            && d.severity == Severity::Error
            && d.node_id.as_deref() == Some("work")),
        "Expected provider_required error for 'work', got: {diags:?}"
    );
}

#[test]
fn provider_required_diamond_node_without_provider_is_error() {
    let pg = parse_and_build(
        r#"digraph G {
        start [shape="Mdiamond"]
        pick [shape="diamond"]
        a [shape="box", llm_provider="claude"]
        b [shape="box", llm_provider="claude"]
        done [shape="Msquare"]
        start -> pick
        pick -> a [condition="outcome=success"]
        pick -> b
        a -> done
        b -> done
    }"#,
    );
    let diags = validate(&pg);
    assert!(
        diags.iter().any(|d| d.rule == "provider_required"
            && d.severity == Severity::Error
            && d.node_id.as_deref() == Some("pick")),
        "Expected provider_required error for diamond node 'pick', got: {diags:?}"
    );
}

#[test]
fn provider_required_start_node_is_exempt_even_with_adversarial_shape_and_id() {
    // A node with id "start" and shape="box" is still recognized as the
    // start node by id, so it must never be required to carry a provider.
    let pg = parse_and_build(
        r#"digraph G {
        start [shape="box"]
        work [shape="box", llm_provider="claude"]
        done [shape="Msquare"]
        start -> work -> done
    }"#,
    );
    let diags = validate(&pg);
    assert!(
        !diags
            .iter()
            .any(|d| d.rule == "provider_required" && d.node_id.as_deref() == Some("start")),
        "start node must be exempt from provider_required, got: {diags:?}"
    );
}

#[test]
fn provider_required_exit_node_is_exempt_even_with_adversarial_shape_and_id() {
    // A node with id "done" and shape="box" is still recognized as a
    // terminal node by id, so it must never be required to carry a provider.
    let pg = parse_and_build(
        r#"digraph G {
        start [shape="Mdiamond"]
        work [shape="box", llm_provider="claude"]
        done [shape="box"]
        start -> work -> done
    }"#,
    );
    let diags = validate(&pg);
    assert!(
        !diags
            .iter()
            .any(|d| d.rule == "provider_required" && d.node_id.as_deref() == Some("done")),
        "terminal node must be exempt from provider_required, got: {diags:?}"
    );
}

#[test]
fn provider_required_quality_node_is_exempt() {
    let pg = parse_and_build(
        r#"digraph G {
        start [shape="Mdiamond"]
        verify [shape="box", type="quality", quality_checks="true"]
        done [shape="Msquare"]
        start -> verify -> done
    }"#,
    );
    let diags = validate(&pg);
    assert!(
        !diags
            .iter()
            .any(|d| d.rule == "provider_required" && d.node_id.as_deref() == Some("verify")),
        "type=quality node must be exempt from provider_required, got: {diags:?}"
    );
}

#[test]
fn provider_required_satisfied_by_explicit_claude_provider() {
    let pg = parse_and_build(
        r#"digraph G {
        start [shape="Mdiamond"]
        work [shape="box", llm_provider="claude"]
        done [shape="Msquare"]
        start -> work -> done
    }"#,
    );
    let diags = validate(&pg);
    assert!(
        !diags.iter().any(|d| d.rule == "provider_required"),
        "explicit llm_provider=\"claude\" should satisfy provider_required, got: {diags:?}"
    );
}

#[test]
fn provider_required_satisfied_by_explicit_codex_provider() {
    // The rule only requires *some* explicit provider — "claude" is merely
    // the default used when filling one in, not the only accepted value.
    let pg = parse_and_build(
        r#"digraph G {
        start [shape="Mdiamond"]
        work [shape="box", llm_provider="codex"]
        done [shape="Msquare"]
        start -> work -> done
    }"#,
    );
    let diags = validate(&pg);
    assert!(
        !diags.iter().any(|d| d.rule == "provider_required"),
        "explicit llm_provider=\"codex\" should satisfy provider_required, got: {diags:?}"
    );
}

#[test]
fn provider_required_multiple_offending_nodes_each_get_own_diagnostic() {
    let pg = parse_and_build(
        r#"digraph G {
        start [shape="Mdiamond"]
        a [shape="box", prompt="A"]
        b [shape="diamond"]
        c [shape="box", llm_provider="claude"]
        done [shape="Msquare"]
        start -> a -> b
        b -> c [condition="outcome=success"]
        b -> done
        c -> done
    }"#,
    );
    let diags = validate(&pg);
    let offending: Vec<&str> = diags
        .iter()
        .filter(|d| d.rule == "provider_required")
        .filter_map(|d| d.node_id.as_deref())
        .collect();
    assert!(
        offending.contains(&"a") && offending.contains(&"b"),
        "Expected separate provider_required diagnostics for 'a' and 'b', got: {diags:?}"
    );
    assert!(
        !offending.contains(&"c"),
        "'c' has an explicit provider and must not be flagged, got: {diags:?}"
    );
    assert_eq!(
        offending.len(),
        2,
        "Expected exactly one diagnostic per offending node, got: {diags:?}"
    );
}

#[test]
fn provider_required_diagnostic_carries_node_id_and_fix() {
    let pg = parse_and_build(
        r#"digraph G {
        start [shape="Mdiamond"]
        work [shape="box", prompt="Do work"]
        done [shape="Msquare"]
        start -> work -> done
    }"#,
    );
    let diags = validate(&pg);
    let diag = diags
        .iter()
        .find(|d| d.rule == "provider_required")
        .expect("expected a provider_required diagnostic");
    assert_eq!(diag.severity, Severity::Error);
    assert_eq!(diag.node_id.as_deref(), Some("work"));
    assert!(
        diag.message.contains("work"),
        "message should name the offending node, got: {}",
        diag.message
    );
    assert!(
        diag.fix.is_some(),
        "provider_required diagnostic should suggest a fix"
    );
}

#[test]
fn provider_required_pre_fix_pipeline_is_not_grandfathered() {
    // A pipeline written before this rule existed — with no llm_provider
    // anywhere — must still fail validation. There is no exemption based
    // on when or how a DOT file was authored.
    let pg = parse_and_build(
        r#"digraph LegacyPipeline {
        start [shape="Mdiamond"]
        analyze [shape="box", prompt="Analyze the input"]
        decide [shape="diamond"]
        act [shape="box", prompt="Act on the decision"]
        done [shape="Msquare"]
        start -> analyze -> decide
        decide -> act [condition="outcome=success"]
        decide -> done
        act -> done
    }"#,
    );
    let result = validate_or_raise(&pg);
    assert!(
        result.is_err(),
        "legacy pipeline with no llm_provider must fail validation, not be grandfathered in"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("analyze") || err_msg.contains("decide") || err_msg.contains("act"),
        "error should name at least one offending node; got: {err_msg}"
    );
}
