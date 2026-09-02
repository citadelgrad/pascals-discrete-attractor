//! Serializer for [`DotGraph`] back into Attractor DOT dialect source text.
//!
//! This is a *structural* round-trip, not a textual one: comment stripping happens
//! before parsing, so comments are never present in the AST and cannot be
//! reproduced here. Map key ordering (graph attrs, node attrs, node ids) is also
//! not preserved from the source text — instead, keys are sorted so that output
//! is deterministic across runs.
//!
//! The output always re-parses to an AST that is structurally equivalent to the
//! graph that produced it (see the round-trip test below).

use std::collections::BTreeMap;

use crate::ast::{AttributeValue, DotGraph, EdgeDef, NodeDef, SubgraphDef};

const INDENT: &str = "    ";

/// Render a [`DotGraph`] back to Attractor DOT dialect source text.
///
/// The result is always parseable by [`crate::parse`].
pub fn to_dot_string(graph: &DotGraph) -> String {
    let mut out = String::new();
    out.push_str("digraph ");
    out.push_str(&graph.name);
    out.push_str(" {\n");
    write_body(
        &mut out,
        1,
        &graph.attrs,
        &graph.node_defaults,
        &graph.edge_defaults,
        &graph.nodes,
        &graph.edges,
        &graph.subgraphs,
    );
    out.push_str("}\n");
    out
}

#[allow(clippy::too_many_arguments)]
fn write_body(
    out: &mut String,
    depth: usize,
    attrs: &std::collections::HashMap<String, AttributeValue>,
    node_defaults: &std::collections::HashMap<String, AttributeValue>,
    edge_defaults: &std::collections::HashMap<String, AttributeValue>,
    nodes: &std::collections::HashMap<String, NodeDef>,
    edges: &[EdgeDef],
    subgraphs: &[SubgraphDef],
) {
    write_bare_attrs(out, depth, attrs);
    write_defaults_block(out, depth, "node", node_defaults);
    write_defaults_block(out, depth, "edge", edge_defaults);
    write_nodes(out, depth, nodes);
    write_edges(out, depth, edges);
    write_subgraphs(out, depth, subgraphs);
}

/// Write graph/subgraph-level attributes as bare `key=value` declarations, sorted by key.
fn write_bare_attrs(
    out: &mut String,
    depth: usize,
    attrs: &std::collections::HashMap<String, AttributeValue>,
) {
    let sorted: BTreeMap<&String, &AttributeValue> = attrs.iter().collect();
    for (key, value) in sorted {
        push_indent(out, depth);
        out.push_str(key);
        out.push('=');
        out.push_str(&format_value(value));
        out.push('\n');
    }
}

/// Write a `node [...]` or `edge [...]` defaults block, sorted by key. Omitted if empty.
fn write_defaults_block(
    out: &mut String,
    depth: usize,
    keyword: &str,
    attrs: &std::collections::HashMap<String, AttributeValue>,
) {
    if attrs.is_empty() {
        return;
    }
    push_indent(out, depth);
    out.push_str(keyword);
    out.push_str(" [");
    write_attr_list(out, attrs);
    out.push_str("]\n");
}

/// Write all node statements, sorted by node id for deterministic output.
fn write_nodes(out: &mut String, depth: usize, nodes: &std::collections::HashMap<String, NodeDef>) {
    let sorted: BTreeMap<&String, &NodeDef> = nodes.iter().collect();
    for (id, node) in sorted {
        push_indent(out, depth);
        out.push_str(id);
        if node.attrs.is_empty() {
            out.push_str(";\n");
        } else {
            out.push_str(" [");
            write_attr_list(out, &node.attrs);
            out.push_str("]\n");
        }
    }
}

/// Write all edge statements in their original (insertion) order.
fn write_edges(out: &mut String, depth: usize, edges: &[EdgeDef]) {
    for edge in edges {
        push_indent(out, depth);
        out.push_str(&edge.from);
        out.push_str(" -> ");
        out.push_str(&edge.to);
        if !edge.attrs.is_empty() {
            out.push_str(" [");
            write_attr_list(out, &edge.attrs);
            out.push(']');
        }
        out.push('\n');
    }
}

/// Write all subgraphs, in their original (insertion) order.
fn write_subgraphs(out: &mut String, depth: usize, subgraphs: &[SubgraphDef]) {
    for sg in subgraphs {
        push_indent(out, depth);
        out.push_str("subgraph");
        if let Some(name) = &sg.name {
            out.push(' ');
            out.push_str(name);
        }
        out.push_str(" {\n");
        write_body(
            out,
            depth + 1,
            &sg.attrs,
            &sg.node_defaults,
            &sg.edge_defaults,
            &sg.nodes,
            &sg.edges,
            &[],
        );
        push_indent(out, depth);
        out.push_str("}\n");
    }
}

/// Write a comma-separated `key=value` attribute list (no surrounding brackets), sorted by key.
fn write_attr_list(out: &mut String, attrs: &std::collections::HashMap<String, AttributeValue>) {
    let sorted: BTreeMap<&String, &AttributeValue> = attrs.iter().collect();
    let mut first = true;
    for (key, value) in sorted {
        if !first {
            out.push_str(", ");
        }
        first = false;
        out.push_str(key);
        out.push('=');
        out.push_str(&format_value(value));
    }
}

fn push_indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str(INDENT);
    }
}

/// Format a single [`AttributeValue`] as DOT source text.
fn format_value(value: &AttributeValue) -> String {
    match value {
        AttributeValue::String(s) => format!("\"{}\"", escape_string(s)),
        AttributeValue::Integer(i) => i.to_string(),
        AttributeValue::Float(f) => format_float(*f),
        AttributeValue::Boolean(b) => b.to_string(),
        AttributeValue::Duration(d) => format!("{}ms", d.as_millis()),
    }
}

/// Escape a string for embedding in a DOT double-quoted string literal.
///
/// Only `\` and `"` need escaping: the DOT parser's `quoted_string` primitive
/// passes any other raw character (including literal newlines/tabs) straight
/// through, so round-tripping those unescaped is both valid and preserves
/// human-authored multi-line prompt text exactly.
fn escape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            other => out.push(other),
        }
    }
    out
}

/// Format an `f64` so it always re-parses as `AttributeValue::Float` (never
/// `Integer` or `Duration`) — the DOT dialect's `float_value` primitive requires
/// a literal `.` with at least one digit on each side.
fn format_float(f: f64) -> String {
    let s = format!("{f}");
    if s.contains('.') {
        s
    } else {
        format!("{s}.0")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;
    use std::time::Duration;

    fn assert_structurally_equal(a: &DotGraph, b: &DotGraph) {
        assert_eq!(a.name, b.name, "graph name");
        assert_eq!(a.attrs, b.attrs, "graph attrs");
        assert_eq!(a.node_defaults, b.node_defaults, "node defaults");
        assert_eq!(a.edge_defaults, b.edge_defaults, "edge defaults");
        assert_eq!(a.nodes.len(), b.nodes.len(), "node count");
        for (id, node) in &a.nodes {
            let other = b
                .nodes
                .get(id)
                .unwrap_or_else(|| panic!("missing node {id}"));
            assert_eq!(node.id, other.id, "node id {id}");
            assert_eq!(node.attrs, other.attrs, "node attrs for {id}");
        }
        assert_eq!(a.edges.len(), b.edges.len(), "edge count");
        for edge in &a.edges {
            let found = b
                .edges
                .iter()
                .find(|e| e.from == edge.from && e.to == edge.to);
            let other = found.unwrap_or_else(|| panic!("missing edge {}->{}", edge.from, edge.to));
            assert_eq!(
                edge.attrs, other.attrs,
                "edge attrs for {}->{}",
                edge.from, edge.to
            );
        }
        assert_eq!(a.subgraphs.len(), b.subgraphs.len(), "subgraph count");
    }

    #[test]
    fn round_trip_full_graph() {
        let input = r#"digraph Pipeline {
            label="Test pipeline"
            goal="Do the thing"

            node [shape="box"]
            edge [style="solid"]

            start [shape="Mdiamond"]
            done  [shape="Msquare"]

            plan [
                shape="box"
                label="Plan"
                llm_provider="claude"
                timeout=120s
                weight=3.5
                enabled=true
                style.model="haiku"
            ]

            subgraph cluster_review {
                review [shape="diamond", node_type="conditional"]
            }

            start -> plan [label="go"]
            plan -> done
            plan -> review
        }"#;

        let graph = parse(input).unwrap();
        let serialized = to_dot_string(&graph);
        let reparsed = parse(&serialized).unwrap_or_else(|e| {
            panic!("serialized output failed to re-parse: {e}\n---\n{serialized}")
        });
        assert_structurally_equal(&graph, &reparsed);
    }

    #[test]
    fn round_trip_preserves_llm_provider_string() {
        let input = r#"digraph G { A [shape="box", llm_provider="claude"] }"#;
        let graph = parse(input).unwrap();
        let serialized = to_dot_string(&graph);
        assert!(
            serialized.contains(r#"llm_provider="claude""#),
            "expected llm_provider=\"claude\" in:\n{serialized}"
        );
        let reparsed = parse(&serialized).unwrap();
        assert_eq!(
            reparsed.nodes.get("A").unwrap().attrs.get("llm_provider"),
            Some(&AttributeValue::String("claude".to_string()))
        );
    }

    #[test]
    fn round_trip_preserves_timeout_duration() {
        let input = r#"digraph G { A [shape="box", timeout=120s] }"#;
        let graph = parse(input).unwrap();
        let serialized = to_dot_string(&graph);
        assert!(
            serialized.contains("timeout=120000ms"),
            "expected timeout=120000ms in:\n{serialized}"
        );
        let reparsed = parse(&serialized).unwrap();
        assert_eq!(
            reparsed.nodes.get("A").unwrap().attrs.get("timeout"),
            Some(&AttributeValue::Duration(Duration::from_secs(120)))
        );
    }

    #[test]
    fn round_trip_preserves_boolean_and_numeric_attrs() {
        let input = r#"digraph G { A [flag=true, off=false, count=42, ratio=1.5] }"#;
        let graph = parse(input).unwrap();
        let serialized = to_dot_string(&graph);
        assert!(serialized.contains("flag=true"));
        assert!(serialized.contains("off=false"));
        assert!(serialized.contains("count=42"));
        assert!(serialized.contains("ratio=1.5"));
        let reparsed = parse(&serialized).unwrap();
        let node = reparsed.nodes.get("A").unwrap();
        assert_eq!(node.attrs.get("flag"), Some(&AttributeValue::Boolean(true)));
        assert_eq!(node.attrs.get("off"), Some(&AttributeValue::Boolean(false)));
        assert_eq!(node.attrs.get("count"), Some(&AttributeValue::Integer(42)));
        assert_eq!(node.attrs.get("ratio"), Some(&AttributeValue::Float(1.5)));
    }

    #[test]
    fn round_trip_whole_number_float_keeps_float_type() {
        // A float that happens to be a whole number (e.g. 3.0) must not silently
        // become AttributeValue::Integer on re-parse.
        let input = r#"digraph G { A [weight=3.0] }"#;
        let graph = parse(input).unwrap();
        let serialized = to_dot_string(&graph);
        let reparsed = parse(&serialized).unwrap();
        assert_eq!(
            reparsed.nodes.get("A").unwrap().attrs.get("weight"),
            Some(&AttributeValue::Float(3.0))
        );
    }

    #[test]
    fn round_trip_escapes_backslash_and_quote() {
        let input = r#"digraph G { A [label="a \"quoted\" \\ value"] }"#;
        let graph = parse(input).unwrap();
        let serialized = to_dot_string(&graph);
        let reparsed = parse(&serialized).unwrap();
        assert_eq!(
            reparsed.nodes.get("A").unwrap().attrs.get("label"),
            graph.nodes.get("A").unwrap().attrs.get("label")
        );
    }

    #[test]
    fn round_trip_preserves_multiline_prompt_text() {
        let input = "digraph G {\n    A [shape=\"box\", prompt=\"line one\nline two\"]\n}";
        let graph = parse(input).unwrap();
        let serialized = to_dot_string(&graph);
        let reparsed = parse(&serialized).unwrap();
        assert_eq!(
            reparsed.nodes.get("A").unwrap().attrs.get("prompt"),
            Some(&AttributeValue::String("line one\nline two".to_string()))
        );
    }

    #[test]
    fn round_trip_empty_attrs_node() {
        let input = "digraph G { A -> B }";
        let graph = parse(input).unwrap();
        let serialized = to_dot_string(&graph);
        let reparsed = parse(&serialized).unwrap();
        assert!(reparsed.nodes.contains_key("A"));
        assert!(reparsed.nodes.contains_key("B"));
        assert_eq!(reparsed.edges.len(), 1);
    }
}
