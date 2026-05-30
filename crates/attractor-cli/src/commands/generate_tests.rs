use super::*;

// ── strip_code_fences ──────────────────────────────────────────

#[test]
fn strip_fences_dot() {
    let input = "```dot\ndigraph { a -> b }\n```";
    assert_eq!(strip_code_fences(input), "digraph { a -> b }");
}

#[test]
fn strip_fences_plain() {
    let input = "```\ndigraph { a -> b }\n```";
    assert_eq!(strip_code_fences(input), "digraph { a -> b }");
}

#[test]
fn strip_fences_graphviz() {
    let input = "```graphviz\ndigraph G {\n  start -> done\n}\n```";
    assert_eq!(strip_code_fences(input), "digraph G {\n  start -> done\n}");
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
fn build_prompt_contains_pipeline_conventions() {
    let result = build_prompt("spec", None);
    assert!(result.contains("Mdiamond"));
    assert!(result.contains("Msquare"));
    assert!(result.contains("node_type=\"conditional\""));
    assert!(result.contains("loop_restart"));
}

#[test]
fn build_prompt_contains_timeout_guidance() {
    let result = build_prompt("spec", None);
    assert!(result.contains("timeout"));
    assert!(result.contains("timeout=\"120s\""));
    assert!(result.contains("timeout=\"300s\""));
    assert!(result.contains("timeout=\"900s\""));
    assert!(result.contains("Lightweight"));
    assert!(result.contains("Heavy"));
}

#[test]
fn build_prompt_requires_commit_step() {
    let result = build_prompt("spec", None);
    assert!(result.contains("commit_changes"));
    assert!(result.contains("Commit Changes"));
    assert!(result.contains("git add -A"));
    assert!(result.contains("Bash(git:*)"));
}

#[test]
fn build_prompt_asks_for_raw_digraph() {
    let result = build_prompt("spec", None);
    assert!(result.contains("Output ONLY the raw digraph"));
    assert!(result.contains("No markdown fences"));
}

#[test]
fn build_prompt_prd_before_spec() {
    let result = build_prompt("SPEC_CONTENT", Some("PRD_CONTENT"));
    let prd_pos = result.find("PRD_CONTENT").unwrap();
    let spec_pos = result.find("SPEC_CONTENT").unwrap();
    assert!(prd_pos < spec_pos, "PRD should appear before spec in prompt");
}

// ── extract_digraph ────────────────────────────────────────────

#[test]
fn extract_raw_digraph() {
    let input = "digraph G { a -> b }";
    assert_eq!(extract_digraph(input).unwrap(), "digraph G { a -> b }");
}

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
