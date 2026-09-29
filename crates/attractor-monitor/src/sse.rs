//! `GET /runs/{id}/events`: journal Events and projection deltas as SSE.

use std::convert::Infallible;
use std::path::{Path as FsPath, PathBuf};
use std::time::Duration;

use attractor_journal::{parse_run_id, read_all, read_index_at, JournalEvent};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;

use crate::state::AppState;

/// Small summary of a Run's current projection, sent as `event: projection`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProjectionDelta {
    pub last_seq: u64,
    pub status: &'static str,
    pub current_node: Option<String>,
    pub current_task: Option<String>,
    pub steps: u64,
    pub cost_usd: f64,
}

pub async fn run_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Some(run_id) = parse_run_id(&id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut sub = state.subscribe(&run_id);
    if sub.is_none() {
        // The Run may have started since the watcher's last tick.
        rescan(&state, &run_id).await;
        sub = state.subscribe(&run_id);
    }
    let Some((path, rx)) = sub else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let cursor = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0);

    let (tx, out) = mpsc::channel::<Result<Event, Infallible>>(64);
    tokio::spawn(pump(state, run_id, path, rx, cursor, tx));
    Sse::new(ReceiverStream::new(out))
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

async fn rescan(state: &AppState, run_id: &str) {
    let path = state.index_path().to_path_buf();
    let found = tokio::task::spawn_blocking(move || read_index_at(&path))
        .await
        .ok()
        .and_then(Result::ok)
        .and_then(|es| es.into_iter().find(|e| e.run_id == run_id));
    if let Some(entry) = found {
        state.upsert_entry(entry);
    }
}

/// Send the backlog after `cursor`, then follow live Events. Ends when the
/// client goes away.
async fn pump(
    state: AppState,
    run_id: String,
    path: PathBuf,
    mut rx: broadcast::Receiver<JournalEvent>,
    cursor: u64,
    tx: mpsc::Sender<Result<Event, Infallible>>,
) {
    let mut last = cursor;
    if !send_backlog(&state, &run_id, &path, &mut last, &tx).await {
        return;
    }
    loop {
        match rx.recv().await {
            Ok(ev) => {
                if ev.seq > last {
                    if !send_event(&tx, &ev).await {
                        return;
                    }
                    last = ev.seq;
                    if !send_projection(&state, &run_id, &tx).await {
                        return;
                    }
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {
                if !send_backlog(&state, &run_id, &path, &mut last, &tx).await {
                    return;
                }
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Send every journalled Event with `seq > *last` from the file, then a
/// projection delta. Returns false when the client is gone.
async fn send_backlog(
    state: &AppState,
    run_id: &str,
    path: &FsPath,
    last: &mut u64,
    tx: &mpsc::Sender<Result<Event, Infallible>>,
) -> bool {
    let p = path.to_path_buf();
    let events = tokio::task::spawn_blocking(move || read_all(p))
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    for ev in &events {
        if ev.seq <= *last {
            continue;
        }
        if !send_event(tx, ev).await {
            return false;
        }
        *last = ev.seq;
    }
    send_projection(state, run_id, tx).await
}

async fn send_event(tx: &mpsc::Sender<Result<Event, Infallible>>, ev: &JournalEvent) -> bool {
    let Ok(data) = serde_json::to_string(ev) else {
        return true;
    };
    let frame = Event::default()
        .event("journal")
        .id(ev.seq.to_string())
        .data(data);
    tx.send(Ok(frame)).await.is_ok()
}

async fn send_projection(
    state: &AppState,
    run_id: &str,
    tx: &mpsc::Sender<Result<Event, Infallible>>,
) -> bool {
    let Some(snap) = state.snapshot(run_id) else {
        return true;
    };
    let delta = ProjectionDelta {
        last_seq: snap.view.last_seq,
        status: snap.view.status.as_str(),
        current_node: snap.view.current_node.clone(),
        current_task: snap.view.current_task.clone(),
        steps: snap.view.steps,
        cost_usd: snap.view.cost_usd,
    };
    let Ok(data) = serde_json::to_string(&delta) else {
        return true;
    };
    tx.send(Ok(Event::default().event("projection").data(data)))
        .await
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_delta_serialization_is_stable() {
        let d = ProjectionDelta {
            last_seq: 3,
            status: "running",
            current_node: Some("n".into()),
            current_task: None,
            steps: 2,
            cost_usd: 0.5,
        };
        assert_eq!(
            serde_json::to_string(&d).unwrap(),
            r#"{"last_seq":3,"status":"running","current_node":"n","current_task":null,"steps":2,"cost_usd":0.5}"#
        );
    }
}
