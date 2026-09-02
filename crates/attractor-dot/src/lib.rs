//! DOT parser for the strict Graphviz subset used by Attractor pipelines.
//!
//! Parses `digraph Name { ... }` with nodes, edges, subgraphs, and typed attributes.
//! Produces a typed AST: [`DotGraph`], [`NodeDef`], [`EdgeDef`], [`SubgraphDef`], [`AttributeValue`].
//!
//! # Example
//! ```
//! let dot = r#"digraph Pipeline { start -> process -> done }"#;
//! let graph = attractor_dot::parse(dot).unwrap();
//! assert_eq!(graph.name, "Pipeline");
//! assert_eq!(graph.edges.len(), 2);
//! ```

pub mod ast;
pub mod duration_serde;
mod parser;
pub mod serializer;

pub use ast::*;
pub use parser::parse;
pub use serializer::to_dot_string;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn parse_simple_linear_pipeline() {
        let input = "digraph Test { start -> plan -> done }";
        let graph = parse(input).unwrap();
        assert_eq!(graph.name, "Test");
        assert_eq!(graph.edges.len(), 2);
        assert_eq!(graph.edges[0].from, "start");
        assert_eq!(graph.edges[0].to, "plan");
        assert_eq!(graph.edges[1].from, "plan");
        assert_eq!(graph.edges[1].to, "done");
        assert!(graph.nodes.contains_key("start"));
        assert!(graph.nodes.contains_key("plan"));
        assert!(graph.nodes.contains_key("done"));
    }

    #[test]
    fn parse_node_with_attributes() {
        let input = r#"digraph G {
            start [shape="Mdiamond", label="Begin"]
        }"#;
        let graph = parse(input).unwrap();
        let node = graph.nodes.get("start").unwrap();
        assert_eq!(
            node.attrs.get("shape"),
            Some(&AttributeValue::String("Mdiamond".to_string()))
        );
        assert_eq!(
            node.attrs.get("label"),
            Some(&AttributeValue::String("Begin".to_string()))
        );
    }

    #[test]
    fn parse_edge_with_attributes() {
        let input = r#"digraph G {
            A -> B [label="ok", weight=10]
        }"#;
        let graph = parse(input).unwrap();
        assert_eq!(graph.edges.len(), 1);
        assert_eq!(
            graph.edges[0].attrs.get("label"),
            Some(&AttributeValue::String("ok".to_string()))
        );
        assert_eq!(
            graph.edges[0].attrs.get("weight"),
            Some(&AttributeValue::Integer(10))
        );
    }

    #[test]
    fn chained_edge_expansion() {
        let input = r#"digraph G {
            A -> B -> C [label="chain"]
        }"#;
        let graph = parse(input).unwrap();
        assert_eq!(graph.edges.len(), 2);
        assert_eq!(graph.edges[0].from, "A");
        assert_eq!(graph.edges[0].to, "B");
        assert_eq!(graph.edges[1].from, "B");
        assert_eq!(graph.edges[1].to, "C");
        // Both edges share the same attrs
        assert_eq!(
            graph.edges[0].attrs.get("label"),
            Some(&AttributeValue::String("chain".to_string()))
        );
        assert_eq!(
            graph.edges[1].attrs.get("label"),
            Some(&AttributeValue::String("chain".to_string()))
        );
    }

    #[test]
    fn parse_subgraph() {
        let input = r#"digraph G {
            subgraph cluster_inner {
                node [shape="box"]
                A -> B
            }
        }"#;
        let graph = parse(input).unwrap();
        assert_eq!(graph.subgraphs.len(), 1);
        let sg = &graph.subgraphs[0];
        assert_eq!(sg.name.as_deref(), Some("cluster_inner"));
        assert!(sg.nodes.contains_key("A"));
        assert!(sg.nodes.contains_key("B"));
        assert_eq!(sg.edges.len(), 1);
        // node defaults should have been applied
        assert_eq!(
            sg.nodes.get("A").unwrap().attrs.get("shape"),
            Some(&AttributeValue::String("box".to_string()))
        );
    }

    #[test]
    fn duration_value_parsing() {
        let input = r#"digraph G {
            step [timeout=900s, delay=250ms, interval=15m]
        }"#;
        let graph = parse(input).unwrap();
        let node = graph.nodes.get("step").unwrap();
        assert_eq!(
            node.attrs.get("timeout"),
            Some(&AttributeValue::Duration(Duration::from_secs(900)))
        );
        assert_eq!(
            node.attrs.get("delay"),
            Some(&AttributeValue::Duration(Duration::from_millis(250)))
        );
        assert_eq!(
            node.attrs.get("interval"),
            Some(&AttributeValue::Duration(Duration::from_secs(15 * 60)))
        );
    }

    #[test]
    fn comment_stripping() {
        let input = r#"
            // This is a comment
            digraph G {
                /* block comment */
                A -> B // inline comment
            }
        "#;
        let graph = parse(input).unwrap();
        assert_eq!(graph.edges.len(), 1);
        assert_eq!(graph.edges[0].from, "A");
        assert_eq!(graph.edges[0].to, "B");
    }

    #[test]
    fn reject_undirected_graph() {
        let input = "graph G { A -- B }";
        let result = parse(input);
        assert!(result.is_err());
    }

    #[test]
    fn reject_undirected_edges() {
        let input = "digraph G { A -- B }";
        let result = parse(input);
        assert!(result.is_err());
    }

    #[test]
    fn parse_graph_attrs() {
        let input = r#"digraph G {
            graph [rankdir="LR"]
            label = "My Graph"
        }"#;
        let graph = parse(input).unwrap();
        assert_eq!(
            graph.attrs.get("rankdir"),
            Some(&AttributeValue::String("LR".to_string()))
        );
        assert_eq!(
            graph.attrs.get("label"),
            Some(&AttributeValue::String("My Graph".to_string()))
        );
    }

    #[test]
    fn parse_node_and_edge_defaults() {
        let input = r#"digraph G {
            node [shape="ellipse"]
            edge [style="dashed"]
            A -> B
        }"#;
        let graph = parse(input).unwrap();
        // node defaults apply to nodes
        assert_eq!(
            graph.nodes.get("A").unwrap().attrs.get("shape"),
            Some(&AttributeValue::String("ellipse".to_string()))
        );
        // edge defaults apply to edges
        assert_eq!(
            graph.edges[0].attrs.get("style"),
            Some(&AttributeValue::String("dashed".to_string()))
        );
    }

    #[test]
    fn parse_float_attribute() {
        let input = r#"digraph G {
            A [weight=3.14]
        }"#;
        let graph = parse(input).unwrap();
        // Using explicit literal to avoid clippy::approx_constant (not actually using PI)
        #[allow(clippy::approx_constant)]
        let expected = 3.14_f64;
        assert_eq!(
            graph.nodes.get("A").unwrap().attrs.get("weight"),
            Some(&AttributeValue::Float(expected))
        );
    }

    #[test]
    fn parse_boolean_attribute() {
        let input = r#"digraph G {
            A [visible=true, hidden=false]
        }"#;
        let graph = parse(input).unwrap();
        let node = graph.nodes.get("A").unwrap();
        assert_eq!(
            node.attrs.get("visible"),
            Some(&AttributeValue::Boolean(true))
        );
        assert_eq!(
            node.attrs.get("hidden"),
            Some(&AttributeValue::Boolean(false))
        );
    }

    #[test]
    fn parse_qualified_key() {
        let input = r#"digraph G {
            A [style.model="gpt4"]
        }"#;
        let graph = parse(input).unwrap();
        assert_eq!(
            graph.nodes.get("A").unwrap().attrs.get("style.model"),
            Some(&AttributeValue::String("gpt4".to_string()))
        );
    }

    #[test]
    fn parse_string_escapes() {
        let input = r#"digraph G {
            A [label="line1\nline2\ttab\\slash\"quote"]
        }"#;
        let graph = parse(input).unwrap();
        assert_eq!(
            graph.nodes.get("A").unwrap().attrs.get("label"),
            Some(&AttributeValue::String(
                "line1\nline2\ttab\\slash\"quote".to_string()
            ))
        );
    }

    #[test]
    fn error_includes_line_and_col() {
        let input = "not_a_graph { }";
        let err = parse(input).unwrap_err();
        match err {
            attractor_types::AttractorError::ParseError { line, col, .. } => {
                assert!(line >= 1);
                assert!(col >= 1);
            }
            _ => panic!("expected ParseError"),
        }
    }

    #[test]
    fn semicolons_optional() {
        let input = r#"digraph G {
            A [label="first"];
            B [label="second"]
            A -> B;
            B -> C
        }"#;
        let graph = parse(input).unwrap();
        assert_eq!(graph.nodes.len(), 3);
        assert_eq!(graph.edges.len(), 2);
    }

    #[test]
    fn quoted_duration_parsed_as_string() {
        // Quoted durations are parsed as String by the DOT parser (since quoted_string
        // matches first in alt()). The pipeline's get_duration_attr handles this by
        // falling back to parse_duration_str for String values.
        let input = r#"digraph G {
            step [timeout="20m", delay="500ms"]
        }"#;
        let graph = parse(input).unwrap();
        let node = graph.nodes.get("step").unwrap();
        // Quoted values become AttributeValue::String, not Duration
        assert_eq!(
            node.attrs.get("timeout"),
            Some(&AttributeValue::String("20m".to_string()))
        );
        assert_eq!(
            node.attrs.get("delay"),
            Some(&AttributeValue::String("500ms".to_string()))
        );
        // But parse_duration_str can convert them
        assert_eq!(
            duration_serde::parse_duration_str("20m").unwrap(),
            Duration::from_secs(20 * 60)
        );
        assert_eq!(
            duration_serde::parse_duration_str("500ms").unwrap(),
            Duration::from_millis(500)
        );
    }

    #[test]
    fn duration_hours_and_days() {
        let input = r#"digraph G {
            A [ttl=2h, retention=7d]
        }"#;
        let graph = parse(input).unwrap();
        let node = graph.nodes.get("A").unwrap();
        assert_eq!(
            node.attrs.get("ttl"),
            Some(&AttributeValue::Duration(Duration::from_secs(2 * 3600)))
        );
        assert_eq!(
            node.attrs.get("retention"),
            Some(&AttributeValue::Duration(Duration::from_secs(7 * 86400)))
        );
    }
}
