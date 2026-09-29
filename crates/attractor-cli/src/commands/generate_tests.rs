use super::*;

// ── strip_code_fences ──────────────────────────────────────────

#[test]
fn strip_fences_dot() {
    let input = "```dot\ndigraph { a -> b }\n```";
    assert_eq!(strip_code_fences(input), "digraph { a -> b }");
}

#[test]
fn strip_fences_trailing_whitespace() {
    let input = "```dot\ndigraph { a -> b }\n```  ";
    assert_eq!(strip_code_fences(input), "digraph { a -> b }");
}

#[test]
fn strip_fences_noop_when_no_fences() {
    let input = "digraph { a -> b }";
    assert_eq!(strip_code_fences(input), input);
}

#[test]
fn strip_fences_noop_single_line() {
    let input = "```dot```";
    assert_eq!(strip_code_fences(input), input);
}

#[test]
fn strip_fences_preserves_inner_content() {
    let input = "```dot\nline1\nline2\nline3\n```";
    assert_eq!(strip_code_fences(input), "line1\nline2\nline3");
}

// ── build_prompt ───────────────────────────────────────────────

#[test]
fn build_prompt_spec_only() {
    let result = build_prompt("my spec content", None);
    assert!(result.contains("## Technical Specification"));
    assert!(result.contains("my spec content"));
    assert!(!result.contains("## PRD"));
}

#[test]
fn build_prompt_with_prd() {
    let result = build_prompt("my spec", Some("my prd"));
    assert!(result.contains("## PRD (Product Requirements Document)"));
    assert!(result.contains("my prd"));
    assert!(result.contains("my spec"));
}

#[test]
fn build_prompt_prd_before_spec() {
    let result = build_prompt("SPEC_CONTENT", Some("PRD_CONTENT"));
    let prd_pos = result.find("PRD_CONTENT").unwrap();
    let spec_pos = result.find("SPEC_CONTENT").unwrap();
    assert!(
        prd_pos < spec_pos,
        "PRD should appear before spec in prompt"
    );
}

#[test]
fn build_prompt_requires_llm_provider() {
    // A generated pipeline's runtime nodes must never silently default to
    // Claude -- the prompt must tell the model to name llm_provider
    // explicitly, and the example node format must demonstrate it.
    let result = build_prompt("spec", None);
    assert!(result.contains("llm_provider"));
    assert!(result.contains("REQUIRED on every provider-backed node"));
    assert!(result.contains("An unprompted `diamond` is pass-through"));
    assert!(result.contains("`type=\"codergen\"` explicitly"));
    assert!(result.contains("llm_provider=\"claude\""));
    // The example node format block should itself set llm_provider.
    let example_start = result.find("Example node format:").unwrap();
    assert!(result[example_start..].contains("llm_provider=\"claude\""));
}

// ── extract_digraph ────────────────────────────────────────────

#[test]
fn extract_from_fenced() {
    let input = "```dot\ndigraph G { a -> b }\n```";
    assert_eq!(extract_digraph(input).unwrap(), "digraph G { a -> b }");
}

#[test]
fn extract_from_preamble() {
    let input = "Looking for skills...\n<function_calls>\n</function_calls>\n\ndigraph G {\n  start -> done\n}";
    let result = extract_digraph(input).unwrap();
    assert!(result.starts_with("digraph G {"));
    assert!(result.ends_with('}'));
    assert!(result.contains("start -> done"));
}

#[test]
fn extract_nested_braces() {
    let input = r#"digraph G {
  subgraph cluster_0 {
    a -> b
  }
  b -> c
}"#;
    let result = extract_digraph(input).unwrap();
    assert_eq!(result, input);
}

#[test]
fn extract_from_fenced_with_preamble() {
    let input = "Here's the pipeline:\n\n```dot\ndigraph Pipeline {\n  start -> work\n  work -> done\n}\n```\n\nHope that helps!";
    let result = extract_digraph(input).unwrap();
    assert!(result.starts_with("digraph Pipeline {"));
    assert!(result.contains("start -> work"));
}

#[test]
fn extract_none_when_no_digraph() {
    assert!(extract_digraph("no graph here").is_none());
    assert!(extract_digraph("").is_none());
    assert!(extract_digraph("graph { a -> b }").is_none());
}

#[test]
fn extract_with_braces_in_prompts() {
    let input = r#"digraph G {
  node1 [prompt="if (x) { return true; }"]
  node1 -> done
}"#;
    let result = extract_digraph(input).unwrap();
    assert!(result.contains("node1 -> done"));
}

// ── provider normalization (post-extraction, pre-write) ─────────
//
// cmd_generate runs extract_digraph's output through
// normalize_provider_defaults before writing the pipeline to disk. These
// tests exercise that exact pipeline (extract -> normalize) against
// Claude-response-shaped input, so a generated pipeline is proven to have
// explicit llm_provider on every runtime node even when the model itself
// forgot to set one.

#[test]
fn generated_output_gets_missing_provider_filled_in() {
    let claude_response = r#"Here's the pipeline:

```dot
digraph Generated {
    start [shape="Mdiamond"]
    implement [shape="box", timeout="900s", prompt="Implement the feature"]
    done [shape="Msquare"]
    start -> implement -> done
}
```"#;

    let extracted = extract_digraph(claude_response).unwrap();
    let (normalized, defaulted) = normalize_provider_defaults(&extracted, "claude").unwrap();

    assert_eq!(defaulted, vec!["implement".to_string()]);
    let parsed = attractor_dot::parse(&normalized).unwrap();
    assert_eq!(
        parsed
            .nodes
            .get("implement")
            .unwrap()
            .attrs
            .get("llm_provider"),
        Some(&attractor_dot::AttributeValue::String("claude".to_string()))
    );
}

#[test]
fn generated_output_with_explicit_provider_is_left_untouched() {
    let claude_response = r#"digraph Generated {
    start [shape="Mdiamond"]
    implement [shape="box", timeout="900s", llm_provider="codex", prompt="Implement the feature"]
    done [shape="Msquare"]
    start -> implement -> done
}"#;

    let extracted = extract_digraph(claude_response).unwrap();
    let (normalized, defaulted) = normalize_provider_defaults(&extracted, "claude").unwrap();

    assert!(defaulted.is_empty());
    let parsed = attractor_dot::parse(&normalized).unwrap();
    assert_eq!(
        parsed
            .nodes
            .get("implement")
            .unwrap()
            .attrs
            .get("llm_provider"),
        Some(&attractor_dot::AttributeValue::String("codex".to_string()))
    );
}

#[test]
fn build_plan_prompt_contains_plan_once_and_conventions() {
    let plan = "# Plan document 1 of 2: a.md\n\nAAA\n\n# Plan document 2 of 2: b.md\n\nBBB";
    let prompt = build_plan_prompt(plan);
    assert_eq!(prompt.matches(plan).count(), 1);
    assert!(prompt.contains("## Pipeline conventions"));
    assert!(prompt.contains("llm_provider"));
}

#[test]
fn build_prompt_shape_is_unchanged() {
    let prompt = build_prompt("SPEC", Some("PRD"));
    assert!(prompt.starts_with("Generate a Graphviz DOT pipeline"));
    assert!(prompt.contains(
        "## PRD (Product Requirements Document)\n\nPRD\n\n## Technical Specification\n\nSPEC\n\n## Pipeline conventions"
    ));
    assert!(prompt.ends_with("No markdown fences, no commentary."));
}
