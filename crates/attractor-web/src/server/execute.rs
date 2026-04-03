//! Execution chain: decompose → scaffold → run pipeline.
//!
//! Server function that orchestrates the full flow when
//! the user clicks "Approve & Execute".

use leptos::prelude::*;
use leptos::server_fn::error::NoCustomError;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum ExecutionPhase {
    Decomposing,
    Scaffolding,
    Running,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ExecutionResponse {
    pub session_id: String,
    pub epic_id: String,
    pub pipeline_path: String,
}

/// Resolve the pas CLI binary path.
///
/// Checks `PAS_CLI_PATH` env var first, then falls back to `pas` on PATH.
#[cfg(feature = "ssr")]
fn pas_cli_path() -> String {
    std::env::var("PAS_CLI_PATH").unwrap_or_else(|_| "pas".into())
}

/// Start the full execution chain: decompose spec → scaffold pipeline → run.
///
/// 1. Runs `attractor decompose {project}/.pas/spec.md` → captures epic_id
/// 2. Runs `attractor scaffold <epic-id>` → captures pipeline path
/// 3. Loads and executes pipeline with SSE streaming
///
/// # Arguments
/// - `project_id`: Database ID of the project to execute against
#[server]
pub async fn start_execution(project_id: i64) -> Result<ExecutionResponse, ServerFnError<NoCustomError>> {
    use tokio::process::Command;
    use uuid::Uuid;

    // Get the project from the database
    let pool = leptos::prelude::use_context::<sqlx::SqlitePool>()
        .ok_or_else(|| ServerFnError::<NoCustomError>::ServerError("No database pool".into()))?;

    let project = crate::server::db::get_project(&pool, project_id)
        .await
        .map_err(|e| ServerFnError::<NoCustomError>::ServerError(
            format!("Failed to get project: {}", e)
        ))?;

    let cli = pas_cli_path();
    let session_id = Uuid::new_v4().to_string();
    let spec_path = format!("{}/.pas/spec.md", project.folder_path);
    let project_dir = std::path::PathBuf::from(&project.folder_path);

    // Verify spec file exists
    if !std::path::Path::new(&spec_path).exists() {
        return Err(ServerFnError::<NoCustomError>::ServerError(
            format!("Spec file not found at {}", spec_path),
        ));
    }

    // 1. Decompose: spec → beads epic + tasks
    tracing::info!("Starting decompose of {} (cli: {})", spec_path, cli);
    let decompose_output = Command::new(&cli)
        .args(["decompose", &spec_path])
        .current_dir(&project_dir)
        .output()
        .await
        .map_err(|e| {
            ServerFnError::<NoCustomError>::ServerError(format!("Failed to run decompose: {}", e))
        })?;

    if !decompose_output.status.success() {
        let stderr = String::from_utf8_lossy(&decompose_output.stderr);
        return Err(ServerFnError::<NoCustomError>::ServerError(format!(
            "Decompose failed: {}",
            stderr
        )));
    }

    // Parse epic_id from output (format: "  Epic ID: attractor-xxx")
    let stdout = String::from_utf8_lossy(&decompose_output.stdout);
    let epic_id = stdout
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("Epic ID: ")
                .map(|id| id.trim().to_string())
        })
        .ok_or_else(|| {
            ServerFnError::<NoCustomError>::ServerError(
                "Could not parse epic_id from decompose output".into(),
            )
        })?;

    tracing::info!("Decompose complete: epic_id={}", epic_id);

    // 2. Scaffold: epic → pipeline .dot file
    tracing::info!("Starting scaffold for {}", epic_id);
    let scaffold_output = Command::new(&cli)
        .args(["scaffold", &epic_id])
        .current_dir(&project_dir)
        .output()
        .await
        .map_err(|e| {
            ServerFnError::<NoCustomError>::ServerError(format!("Failed to run scaffold: {}", e))
        })?;

    if !scaffold_output.status.success() {
        let stderr = String::from_utf8_lossy(&scaffold_output.stderr);
        return Err(ServerFnError::<NoCustomError>::ServerError(format!(
            "Scaffold failed: {}",
            stderr
        )));
    }

    let pipeline_path = format!("pipelines/{}.dot", epic_id);
    let full_pipeline_path = project_dir.join(&pipeline_path);
    tracing::info!("Scaffold complete: pipeline={}", pipeline_path);

    // 3. Load pipeline and start execution with SSE streaming
    let dot_source = std::fs::read_to_string(&full_pipeline_path).map_err(|e| {
        ServerFnError::<NoCustomError>::ServerError(format!(
            "Failed to read pipeline file: {}",
            e
        ))
    })?;

    let parsed = attractor_dot::parse(&dot_source).map_err(|e| {
        ServerFnError::<NoCustomError>::ServerError(format!("Failed to parse pipeline: {}", e))
    })?;

    let graph = attractor_pipeline::PipelineGraph::from_dot(parsed).map_err(|e| {
        ServerFnError::<NoCustomError>::ServerError(format!("Failed to build graph: {}", e))
    })?;

    // Spawn background execution task
    let sid = session_id.clone();
    tokio::spawn(async move {
        if let Err(e) = run_pipeline_with_streaming(&graph, &sid).await {
            tracing::error!("Pipeline execution failed: {:?}", e);
            crate::server::stream::publish_event(
                &sid,
                serde_json::to_string(&serde_json::json!({
                    "type": "error",
                    "message": format!("Pipeline failed: {}", e),
                }))
                .unwrap_or_default(),
            );
        }
        crate::server::stream::clear_session_state(&sid);
    });

    Ok(ExecutionResponse {
        session_id,
        epic_id,
        pipeline_path,
    })
}

/// Execute a pipeline graph with streaming progress events.
#[cfg(feature = "ssr")]
async fn run_pipeline_with_streaming(
    graph: &attractor_pipeline::PipelineGraph,
    session_id: &str,
) -> Result<attractor_pipeline::PipelineResult, attractor_types::AttractorError> {
    use attractor_pipeline::{default_registry, select_edge};
    use attractor_types::{AttractorError, Context};
    use std::collections::HashMap;

    let registry = default_registry();
    let context = Context::new();

    // Set workdir to current directory
    if let Ok(cwd) = std::env::current_dir() {
        context
            .set(
                "workdir",
                serde_json::Value::String(cwd.to_string_lossy().into()),
            )
            .await;
    }

    // Initialize context from graph attrs
    for (key, val) in &graph.attrs {
        context.set(key, attr_to_json(val)).await;
    }

    let mut completed_nodes = Vec::new();
    let mut node_outcomes = HashMap::new();
    let mut current_node = graph
        .start_node()
        .ok_or_else(|| AttractorError::Other("No start node found".into()))?;
    let mut total_cost = 0.0;

    loop {
        // Emit node_start event
        crate::server::stream::publish_event(
            session_id,
            serde_json::to_string(&serde_json::json!({
                "type": "node_start",
                "node_id": current_node.id,
                "label": current_node.label,
            }))
            .unwrap_or_default(),
        );

        // Execute node
        let handler_type = registry.resolve_type(current_node);
        let handler = registry.get(&handler_type).ok_or_else(|| {
            AttractorError::HandlerError {
                handler: handler_type.clone(),
                node: current_node.id.clone(),
                message: format!("No handler for '{}'", handler_type),
            }
        })?;

        let outcome = handler.execute(current_node, &context, graph).await?;

        // Track cost
        if let Some(cost) = outcome
            .context_updates
            .get(&format!("{}.cost_usd", current_node.id))
        {
            if let Some(c) = cost.as_f64() {
                total_cost += c;
            }
        }

        // Emit node_complete event
        crate::server::stream::publish_event(
            session_id,
            serde_json::to_string(&serde_json::json!({
                "type": "node_complete",
                "node_id": current_node.id,
                "status": format!("{:?}", outcome.status),
                "cost_usd": total_cost,
                "notes": outcome.notes,
            }))
            .unwrap_or_default(),
        );

        completed_nodes.push(current_node.id.clone());
        node_outcomes.insert(current_node.id.clone(), outcome.clone());

        // Apply context updates
        context
            .apply_updates(outcome.context_updates.clone())
            .await;
        context
            .set(
                "outcome",
                serde_json::Value::String(status_str(outcome.status)),
            )
            .await;
        if let Some(ref label) = outcome.preferred_label {
            context
                .set(
                    "preferred_label",
                    serde_json::Value::String(label.clone()),
                )
                .await;
        }

        // Terminal check
        if current_node.shape == "Msquare" {
            break;
        }

        // Select next edge
        let resolve = |key: &str| -> String {
            match key {
                "outcome" => format!("{:?}", outcome.status),
                "preferred_label" => outcome.preferred_label.clone().unwrap_or_default(),
                _ => String::new(),
            }
        };
        let next_edge = select_edge(&current_node.id, &outcome, &resolve, graph);

        match next_edge {
            Some(edge) => {
                let next_id = edge.to.clone();
                current_node = graph.node(&next_id).ok_or_else(|| {
                    AttractorError::Other(format!("Edge target '{}' not found", next_id))
                })?;
            }
            None => {
                return Err(AttractorError::Other(
                    "No outgoing edge from non-terminal node".into(),
                ));
            }
        }
    }

    // Emit pipeline_complete
    crate::server::stream::publish_event(
        session_id,
        serde_json::to_string(&serde_json::json!({
            "type": "pipeline_complete",
            "total_cost_usd": total_cost,
            "completed_nodes": completed_nodes,
        }))
        .unwrap_or_default(),
    );

    Ok(attractor_pipeline::PipelineResult {
        completed_nodes,
        node_outcomes,
        final_context: context.snapshot().await,
    })
}

#[cfg(feature = "ssr")]
fn attr_to_json(attr: &attractor_dot::AttributeValue) -> serde_json::Value {
    use attractor_dot::AttributeValue;
    match attr {
        AttributeValue::String(s) => serde_json::Value::String(s.clone()),
        AttributeValue::Integer(i) => serde_json::Value::Number(serde_json::Number::from(*i)),
        AttributeValue::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        AttributeValue::Boolean(b) => serde_json::Value::Bool(*b),
        AttributeValue::Duration(d) => {
            serde_json::Value::Number(serde_json::Number::from(d.as_secs()))
        }
    }
}

#[cfg(feature = "ssr")]
fn status_str(status: attractor_types::StageStatus) -> String {
    use attractor_types::StageStatus;
    match status {
        StageStatus::Success => "success",
        StageStatus::PartialSuccess => "partial_success",
        StageStatus::Retry => "retry",
        StageStatus::Fail => "fail",
        StageStatus::Skipped => "skipped",
    }
    .to_string()
}
