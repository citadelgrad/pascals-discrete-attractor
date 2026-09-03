use anyhow;

pub fn cmd_info(path: &std::path::Path) -> anyhow::Result<()> {
    let plan = crate::load_execution_plan(path)?;
    let graph = plan.graph();

    println!("Pipeline: {}", graph.name);
    if !graph.goal.is_empty() {
        println!("Goal: {}", graph.goal);
    }

    let node_count = graph.all_nodes().count();
    let edge_count = graph.all_edges().len();
    println!("Nodes: {}", node_count);
    println!("Edges: {}", edge_count);

    let start = plan.start_node();
    let start_source = plan
        .source_node(&start.node_id)
        .expect("compiled source node");
    println!("Start: {} ({})", start.node_id, start_source.label);
    for exit_id in plan.exit_ids() {
        let exit = plan.source_node(exit_id).expect("compiled source node");
        println!("Exit: {} ({})", exit.id, exit.label);
    }

    // List nodes with their types
    println!("\nNodes:");
    let mut nodes = plan.all_nodes().collect::<Vec<_>>();
    nodes.sort_by(|left, right| left.node_id.cmp(&right.node_id));
    for resolved in nodes {
        let node = plan
            .source_node(&resolved.node_id)
            .expect("compiled source node");
        let provider = resolved
            .provider
            .map(|provider| provider.as_str())
            .unwrap_or("-");
        println!(
            "  {} [{}] kind={:?} handler={} provider={}",
            node.id, node.label, resolved.kind, resolved.handler, provider
        );
    }

    Ok(())
}
