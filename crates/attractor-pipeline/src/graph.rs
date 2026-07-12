use std::collections::HashMap;
use std::time::Duration;

use attractor_dot::{AttributeValue, DotGraph, EdgeDef, NodeDef};

#[derive(Debug, Clone)]
pub struct PipelineGraph {
    pub name: String,
    pub goal: String,
    pub attrs: HashMap<String, AttributeValue>,
    nodes: HashMap<String, PipelineNode>,
    edges: Vec<PipelineEdge>,
    /// Maps node_id to a range (start, count) into the sorted `edges` vec.
    /// Edges are sorted by `from` so each node's outgoing edges are contiguous.
    adjacency: HashMap<String, (usize, usize)>,
}

#[derive(Debug, Clone)]
pub struct PipelineNode {
    pub id: String,
    pub label: String,
    pub shape: String,
    pub node_type: Option<String>,
    pub prompt: Option<String>,
    pub max_retries: usize,
    pub goal_gate: bool,
    pub retry_target: Option<String>,
    pub fallback_retry_target: Option<String>,
    pub fidelity: Option<String>,
    pub thread_id: Option<String>,
    pub classes: Vec<String>,
    pub timeout: Option<Duration>,
    pub llm_model: Option<String>,
    pub llm_provider: Option<String>,
    pub reasoning_effort: Option<String>,
    pub auto_status: bool,
    pub allow_partial: bool,
    pub raw_attrs: HashMap<String, AttributeValue>,
}

#[derive(Debug, Clone)]
pub struct PipelineEdge {
    pub from: String,
    pub to: String,
    pub label: Option<String>,
    pub condition: Option<String>,
    pub weight: i32,
    pub fidelity: Option<String>,
    pub thread_id: Option<String>,
    pub loop_restart: bool,
}

// --- Attribute extraction helpers ---

fn get_string_attr(attrs: &HashMap<String, AttributeValue>, key: &str) -> Option<String> {
    attrs.get(key).and_then(|v| match v {
        AttributeValue::String(s) => Some(s.clone()),
        _ => None,
    })
}

fn get_bool_attr(attrs: &HashMap<String, AttributeValue>, key: &str) -> Option<bool> {
    attrs.get(key).and_then(|v| match v {
        AttributeValue::Boolean(b) => Some(*b),
        AttributeValue::String(s) => Some(s == "true"),
        _ => None,
    })
}

fn get_int_attr(attrs: &HashMap<String, AttributeValue>, key: &str) -> Option<i64> {
    attrs.get(key).and_then(|v| match v {
        AttributeValue::Integer(i) => Some(*i),
        _ => None,
    })
}

fn get_duration_attr(attrs: &HashMap<String, AttributeValue>, key: &str) -> Option<Duration> {
    attrs.get(key).and_then(|v| match v {
        AttributeValue::Duration(d) => Some(*d),
        AttributeValue::String(s) => attractor_dot::duration_serde::parse_duration_str(s).ok(),
        _ => None,
    })
}

// --- Conversions ---

fn node_def_to_pipeline_node(
    id: &str,
    node_def: &NodeDef,
    graph_defaults: &HashMap<String, AttributeValue>,
    subgraph_defaults: Option<&HashMap<String, AttributeValue>>,
) -> PipelineNode {
    // Layer defaults: graph-level, then subgraph-level, then explicit node attrs
    let mut attrs = graph_defaults.clone();
    if let Some(sg_defaults) = subgraph_defaults {
        attrs.extend(sg_defaults.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    attrs.extend(node_def.attrs.iter().map(|(k, v)| (k.clone(), v.clone())));

    let shape = get_string_attr(&attrs, "shape").unwrap_or_else(|| "box".to_string());
    let label = get_string_attr(&attrs, "label").unwrap_or_else(|| id.to_string());
    let node_type = get_string_attr(&attrs, "type");
    let prompt = get_string_attr(&attrs, "prompt");
    let max_retries = get_int_attr(&attrs, "max_retries")
        .map(|v| v as usize)
        .unwrap_or(0);
    let goal_gate = get_bool_attr(&attrs, "goal_gate").unwrap_or(false);
    let retry_target = get_string_attr(&attrs, "retry_target");
    let fallback_retry_target = get_string_attr(&attrs, "fallback_retry_target");
    let fidelity = get_string_attr(&attrs, "fidelity");
    let thread_id = get_string_attr(&attrs, "thread_id");
    let classes = get_string_attr(&attrs, "class")
        .map(|s| s.split_whitespace().map(String::from).collect())
        .unwrap_or_default();
    let timeout = get_duration_attr(&attrs, "timeout");
    let llm_model = get_string_attr(&attrs, "llm_model");
    let llm_provider = get_string_attr(&attrs, "llm_provider");
    let reasoning_effort = get_string_attr(&attrs, "reasoning_effort");
    let auto_status = get_bool_attr(&attrs, "auto_status").unwrap_or(true);
    let allow_partial = get_bool_attr(&attrs, "allow_partial").unwrap_or(false);

    PipelineNode {
        id: id.to_string(),
        label,
        shape,
        node_type,
        prompt,
        max_retries,
        goal_gate,
        retry_target,
        fallback_retry_target,
        fidelity,
        thread_id,
        classes,
        timeout,
        llm_model,
        llm_provider,
        reasoning_effort,
        auto_status,
        allow_partial,
        raw_attrs: attrs,
    }
}

fn edge_def_to_pipeline_edge(
    edge_def: &EdgeDef,
    edge_defaults: &HashMap<String, AttributeValue>,
) -> PipelineEdge {
    let mut attrs = edge_defaults.clone();
    attrs.extend(edge_def.attrs.iter().map(|(k, v)| (k.clone(), v.clone())));

    PipelineEdge {
        from: edge_def.from.clone(),
        to: edge_def.to.clone(),
        label: get_string_attr(&attrs, "label"),
        condition: get_string_attr(&attrs, "condition"),
        weight: get_int_attr(&attrs, "weight")
            .map(|v| v as i32)
            .unwrap_or(0),
        fidelity: get_string_attr(&attrs, "fidelity"),
        thread_id: get_string_attr(&attrs, "thread_id"),
        loop_restart: get_bool_attr(&attrs, "loop_restart").unwrap_or(false),
    }
}

impl PipelineGraph {
    pub fn from_dot(graph: DotGraph) -> attractor_types::Result<Self> {
        let mut nodes = HashMap::new();
        let mut all_edges = Vec::new();

        // Collect top-level nodes with graph-level defaults
        for (id, node_def) in &graph.nodes {
            let pn = node_def_to_pipeline_node(id, node_def, &graph.node_defaults, None);
            nodes.insert(id.clone(), pn);
        }

        // Collect subgraph nodes (with subgraph-level defaults layered on top)
        for sg in &graph.subgraphs {
            for (id, node_def) in &sg.nodes {
                let pn = node_def_to_pipeline_node(
                    id,
                    node_def,
                    &graph.node_defaults,
                    Some(&sg.node_defaults),
                );
                nodes.insert(id.clone(), pn);
            }
        }

        // Collect top-level edges
        for edge_def in &graph.edges {
            all_edges.push(edge_def_to_pipeline_edge(edge_def, &graph.edge_defaults));
        }

        // Collect subgraph edges
        for sg in &graph.subgraphs {
            let mut sg_edge_defaults = graph.edge_defaults.clone();
            sg_edge_defaults.extend(sg.edge_defaults.iter().map(|(k, v)| (k.clone(), v.clone())));
            for edge_def in &sg.edges {
                all_edges.push(edge_def_to_pipeline_edge(edge_def, &sg_edge_defaults));
            }
        }

        // Sort edges by `from` so each node's outgoing edges form a contiguous slice
        all_edges.sort_by(|a, b| a.from.cmp(&b.from));

        // Build adjacency: map from node_id -> (start_index, count)
        let mut adjacency: HashMap<String, (usize, usize)> = HashMap::new();
        let mut i = 0;
        while i < all_edges.len() {
            let start = i;
            let from = &all_edges[i].from;
            while i < all_edges.len() && all_edges[i].from == *from {
                i += 1;
            }
            adjacency.insert(from.clone(), (start, i - start));
        }

        let goal = get_string_attr(&graph.attrs, "goal").unwrap_or_default();

        Ok(PipelineGraph {
            name: graph.name,
            goal,
            attrs: graph.attrs,
            nodes,
            edges: all_edges,
            adjacency,
        })
    }

    /// Find the start node: shape == "Mdiamond" or id is "start"/"Start".
    pub fn start_node(&self) -> Option<&PipelineNode> {
        self.nodes
            .values()
            .find(|n| n.shape == "Mdiamond")
            .or_else(|| self.nodes.get("start").or_else(|| self.nodes.get("Start")))
    }

    /// Find the exit node: shape == "Msquare".
    pub fn exit_node(&self) -> Option<&PipelineNode> {
        self.nodes.values().find(|n| n.shape == "Msquare")
    }

    pub fn node(&self, id: &str) -> Option<&PipelineNode> {
        self.nodes.get(id)
    }

    pub fn outgoing_edges(&self, node_id: &str) -> &[PipelineEdge] {
        match self.adjacency.get(node_id) {
            Some(&(start, count)) => &self.edges[start..start + count],
            None => &[],
        }
    }

    pub fn all_nodes(&self) -> impl Iterator<Item = &PipelineNode> {
        self.nodes.values()
    }

    pub fn all_nodes_mut(&mut self) -> impl Iterator<Item = &mut PipelineNode> {
        self.nodes.values_mut()
    }

    pub fn all_edges(&self) -> &[PipelineEdge] {
        &self.edges
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_and_build(dot: &str) -> PipelineGraph {
        let graph = attractor_dot::parse(dot).unwrap();
        PipelineGraph::from_dot(graph).unwrap()
    }

    #[test]
    fn from_dot_simple_linear_pipeline() {
        let pg = parse_and_build(
            r#"digraph Pipeline {
            start [shape="Mdiamond"]
            process [label="Process Data"]
            done [shape="Msquare"]
            start -> process -> done
        }"#,
        );

        assert_eq!(pg.name, "Pipeline");
        assert_eq!(pg.all_edges().len(), 2);
        assert!(pg.node("start").is_some());
        assert!(pg.node("process").is_some());
        assert!(pg.node("done").is_some());
        assert_eq!(pg.node("process").unwrap().label, "Process Data");
    }

    #[test]
    fn start_node_finds_mdiamond() {
        let pg = parse_and_build(
            r#"digraph G {
            begin [shape="Mdiamond", label="Start Here"]
            work [shape="box"]
            begin -> work
        }"#,
        );

        let start = pg.start_node().unwrap();
        assert_eq!(start.id, "begin");
        assert_eq!(start.shape, "Mdiamond");
    }

    #[test]
    fn start_node_falls_back_to_id() {
        let pg = parse_and_build(
            r#"digraph G {
            start [label="Go"]
            work [shape="box"]
            start -> work
        }"#,
        );

        let start = pg.start_node().unwrap();
        assert_eq!(start.id, "start");
    }

    #[test]
    fn exit_node_finds_msquare() {
        let pg = parse_and_build(
            r#"digraph G {
            work -> done
            done [shape="Msquare"]
        }"#,
        );

        let exit = pg.exit_node().unwrap();
        assert_eq!(exit.id, "done");
        assert_eq!(exit.shape, "Msquare");
    }

    #[test]
    fn outgoing_edges_returns_correct_edges() {
        let pg = parse_and_build(
            r#"digraph G {
            A -> B [label="first"]
            A -> C [label="second"]
            B -> C
        }"#,
        );

        let edges_a = pg.outgoing_edges("A");
        assert_eq!(edges_a.len(), 2);
        let labels: Vec<_> = edges_a.iter().filter_map(|e| e.label.as_deref()).collect();
        assert!(labels.contains(&"first"));
        assert!(labels.contains(&"second"));

        let edges_b = pg.outgoing_edges("B");
        assert_eq!(edges_b.len(), 1);
        assert_eq!(edges_b[0].to, "C");

        let edges_c = pg.outgoing_edges("C");
        assert_eq!(edges_c.len(), 0);
    }

    #[test]
    fn typed_attribute_extraction() {
        let pg = parse_and_build(
            r#"digraph G {
            step [max_retries=3, goal_gate=true, timeout=30s, allow_partial=false]
        }"#,
        );

        let node = pg.node("step").unwrap();
        assert_eq!(node.max_retries, 3);
        assert!(node.goal_gate);
        assert_eq!(node.timeout, Some(Duration::from_secs(30)));
        assert!(!node.allow_partial);
        assert!(node.auto_status); // default true
    }

    #[test]
    fn subgraph_nodes_included() {
        let pg = parse_and_build(
            r#"digraph G {
            start -> A
            subgraph cluster_inner {
                node [shape="ellipse"]
                A -> B
            }
            B -> done
        }"#,
        );

        // Subgraph nodes should be present
        assert!(pg.node("A").is_some());
        assert!(pg.node("B").is_some());

        // Subgraph node defaults should be applied
        let a = pg.node("A").unwrap();
        assert_eq!(a.shape, "ellipse");

        // All edges should be present (top-level + subgraph)
        assert_eq!(pg.all_edges().len(), 3);
    }

    #[test]
    fn goal_extracted_from_graph_attrs() {
        let pg = parse_and_build(
            r#"digraph G {
            goal = "Complete the pipeline"
            A -> B
        }"#,
        );

        assert_eq!(pg.goal, "Complete the pipeline");
    }

    #[test]
    fn edge_weight_and_condition() {
        let pg = parse_and_build(
            r#"digraph G {
            A -> B [weight=5, condition="status == success", loop_restart=true]
        }"#,
        );

        let edges = pg.outgoing_edges("A");
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].weight, 5);
        assert_eq!(edges[0].condition.as_deref(), Some("status == success"));
        assert!(edges[0].loop_restart);
    }

    #[test]
    fn default_shape_is_box() {
        let pg = parse_and_build(
            r#"digraph G {
            plain_node [label="No shape set"]
        }"#,
        );

        assert_eq!(pg.node("plain_node").unwrap().shape, "box");
    }
}
