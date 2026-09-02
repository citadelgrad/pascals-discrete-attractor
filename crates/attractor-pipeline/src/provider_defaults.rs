//! Fill in a default `llm_provider` on runtime nodes that don't have one.
//!
//! `pas scaffold` and `pas generate` produce DOT pipelines whose `box`/`diamond`
//! runtime nodes had no explicit `llm_provider`. Such nodes silently defaulted to
//! Claude deep inside the handler layer, with no warning to the pipeline author.
//! [`fill_missing_llm_providers`] makes that default explicit in the DOT source
//! itself, at the point the pipeline is written to disk, so the file always says
//! what it does.

use attractor_dot::{AttributeValue, DotGraph};

use crate::graph::PipelineGraph;
use crate::validation::is_provider_required_node;

/// Insert `llm_provider="<default_provider>"` on every runtime node (`box`/`diamond`,
/// not start, not terminal, not `type="quality"`) that has no effective `llm_provider`
/// — resolved through graph/subgraph defaults, same as the pipeline engine sees it.
///
/// Nodes that already have an explicit (or defaulted-through-`node [...]`) provider
/// are left untouched, even if it's not `default_provider` (e.g. `"codex"`).
///
/// Returns the sorted list of node ids that were defaulted.
pub fn fill_missing_llm_providers(dot_graph: &mut DotGraph, default_provider: &str) -> Vec<String> {
    // Build a read-only `PipelineGraph` view from a clone so we can resolve each
    // node's *effective* llm_provider/shape/node_type (graph defaults -> subgraph
    // defaults -> explicit node attrs) without consuming the graph we still need
    // to mutate and hand back to the caller.
    let pipeline_graph = match PipelineGraph::from_dot(dot_graph.clone()) {
        Ok(pg) => pg,
        Err(_) => return Vec::new(),
    };

    let mut defaulted = Vec::new();
    for node in pipeline_graph.all_nodes() {
        if !is_provider_required_node(&node.id, &node.shape, node.node_type.as_deref()) {
            continue;
        }
        if node.llm_provider.is_some() {
            continue;
        }

        let node_def = dot_graph.nodes.get_mut(&node.id).or_else(|| {
            dot_graph
                .subgraphs
                .iter_mut()
                .find_map(|sg| sg.nodes.get_mut(&node.id))
        });

        if let Some(node_def) = node_def {
            node_def.attrs.insert(
                "llm_provider".to_string(),
                AttributeValue::String(default_provider.to_string()),
            );
            defaulted.push(node.id.clone());
        }
    }

    defaulted.sort();
    defaulted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(dot: &str, default_provider: &str) -> (DotGraph, Vec<String>) {
        let mut graph = attractor_dot::parse(dot).unwrap();
        let defaulted = fill_missing_llm_providers(&mut graph, default_provider);
        (graph, defaulted)
    }

    fn provider_of<'a>(graph: &'a DotGraph, id: &str) -> Option<&'a str> {
        match graph.nodes.get(id)?.attrs.get("llm_provider")? {
            AttributeValue::String(s) => Some(s.as_str()),
            _ => None,
        }
    }

    #[test]
    fn box_node_missing_provider_gets_defaulted() {
        let (graph, defaulted) = fill(
            r#"digraph G {
                start [shape="Mdiamond"]
                done [shape="Msquare"]
                work [shape="box"]
                start -> work -> done
            }"#,
            "claude",
        );
        assert_eq!(defaulted, vec!["work".to_string()]);
        assert_eq!(provider_of(&graph, "work"), Some("claude"));
    }

    #[test]
    fn diamond_node_missing_provider_gets_defaulted() {
        let (graph, defaulted) = fill(
            r#"digraph G {
                start [shape="Mdiamond"]
                done [shape="Msquare"]
                pick [shape="diamond", node_type="conditional"]
                start -> pick -> done
            }"#,
            "claude",
        );
        assert_eq!(defaulted, vec!["pick".to_string()]);
        assert_eq!(provider_of(&graph, "pick"), Some("claude"));
    }

    #[test]
    fn explicit_non_default_provider_is_untouched() {
        let (graph, defaulted) = fill(
            r#"digraph G {
                start [shape="Mdiamond"]
                done [shape="Msquare"]
                work [shape="box", llm_provider="codex"]
                start -> work -> done
            }"#,
            "claude",
        );
        assert!(defaulted.is_empty());
        assert_eq!(provider_of(&graph, "work"), Some("codex"));
    }

    #[test]
    fn start_and_exit_nodes_are_never_touched() {
        let (graph, defaulted) = fill(
            r#"digraph G {
                start [shape="Mdiamond"]
                done [shape="Msquare"]
                work [shape="box"]
                start -> work -> done
            }"#,
            "claude",
        );
        assert!(!defaulted.contains(&"start".to_string()));
        assert!(!defaulted.contains(&"done".to_string()));
        assert_eq!(provider_of(&graph, "start"), None);
        assert_eq!(provider_of(&graph, "done"), None);
    }

    #[test]
    fn adversarial_id_based_start_and_terminal_nodes_are_never_touched() {
        // Nodes identified as start/terminal purely by id (not shape) must also
        // be exempt, even though their shape is "box" and would otherwise match.
        let (graph, defaulted) = fill(
            r#"digraph G {
                start [shape="box"]
                exit [shape="box"]
                end [shape="box"]
                done [shape="box"]
                middle [shape="box"]
                start -> middle -> done
                middle -> exit
                middle -> end
            }"#,
            "claude",
        );
        assert!(!defaulted.contains(&"start".to_string()));
        assert!(!defaulted.contains(&"exit".to_string()));
        assert!(!defaulted.contains(&"end".to_string()));
        assert!(!defaulted.contains(&"done".to_string()));
        assert!(defaulted.contains(&"middle".to_string()));
        assert_eq!(provider_of(&graph, "start"), None);
        assert_eq!(provider_of(&graph, "exit"), None);
        assert_eq!(provider_of(&graph, "end"), None);
        assert_eq!(provider_of(&graph, "done"), None);
    }

    #[test]
    fn quality_node_is_never_touched() {
        let (graph, defaulted) = fill(
            r#"digraph G {
                start [shape="Mdiamond"]
                done [shape="Msquare"]
                gate [shape="box", type="quality"]
                start -> gate -> done
            }"#,
            "claude",
        );
        assert!(defaulted.is_empty());
        assert_eq!(provider_of(&graph, "gate"), None);
    }

    #[test]
    fn already_defaulted_via_node_defaults_block_is_untouched() {
        // A node [...] defaults block that sets llm_provider counts as an
        // "effective" provider (same resolution the pipeline engine uses),
        // so fill_missing_llm_providers must not report the node as
        // defaulted, and must not overwrite the defaults-block value with
        // a different default_provider.
        let (graph, defaulted) = fill(
            r#"digraph G {
                node [llm_provider="gemini"]
                start [shape="Mdiamond"]
                done [shape="Msquare"]
                work [shape="box"]
                start -> work -> done
            }"#,
            "claude",
        );
        assert!(defaulted.is_empty());
        // DOT `node [...]` defaults are merged into each node's own attrs
        // at parse time (see attractor_dot::parse), so "work" already
        // carries llm_provider="gemini" from the defaults block itself —
        // fill_missing_llm_providers correctly leaves it exactly as-is
        // rather than stamping over it with the "claude" default_provider.
        assert_eq!(provider_of(&graph, "work"), Some("gemini"));
    }

    #[test]
    fn multiple_missing_nodes_all_defaulted() {
        let (graph, defaulted) = fill(
            r#"digraph G {
                start [shape="Mdiamond"]
                done [shape="Msquare"]
                a [shape="box"]
                b [shape="diamond", node_type="conditional"]
                c [shape="box", llm_provider="codex"]
                start -> a -> b -> c -> done
            }"#,
            "claude",
        );
        assert_eq!(defaulted, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(provider_of(&graph, "a"), Some("claude"));
        assert_eq!(provider_of(&graph, "b"), Some("claude"));
        assert_eq!(provider_of(&graph, "c"), Some("codex"));
    }
}
