//! Keeps [`AppState`] current by tailing the Run Index and active journals.
//! Observation only: nothing is written (ADR 0001).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use attractor_journal::{read_all, read_index_at, tail_with_interval, IndexEntry, RunDir};
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;

use crate::projection::ViewStatus;
use crate::state::AppState;

/// Poll interval of a per-Run journal tail.
const TAIL_INTERVAL: Duration = Duration::from_millis(200);

/// Start the watcher. It re-reads the Index every `poll` and never exits by
/// itself; abort the handle to stop it.
pub fn spawn(state: AppState, index_path: PathBuf, poll: Duration) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tracked: HashMap<String, Tracked> = HashMap::new();
        loop {
            tick(&state, &index_path, &mut tracked).await;
            tokio::time::sleep(poll).await;
        }
    })
}

#[derive(Default)]
struct Tracked {
    /// The journal was read successfully at least once.
    loaded: bool,
    /// Journal length seen at the last load; a change on an ended Run means
    /// a resume appended to it.
    len: u64,
    tail: Option<JoinHandle<()>>,
}

impl Drop for Tracked {
    fn drop(&mut self) {
        if let Some(t) = self.tail.take() {
            t.abort();
        }
    }
}

async fn tick(state: &AppState, index_path: &Path, tracked: &mut HashMap<String, Tracked>) {
    let path = index_path.to_path_buf();
    let entries = match tokio::task::spawn_blocking(move || read_index_at(&path)).await {
        Ok(Ok(e)) => e,
        Ok(Err(e)) => {
            tracing::warn!("cannot read Run Index: {e}");
            return;
        }
        Err(_) => return,
    };
    for entry in entries {
        state.upsert_entry(entry.clone());
        let t = tracked.entry(entry.run_id.clone()).or_default();
        if t.tail.as_ref().is_some_and(|h| h.is_finished()) {
            t.tail = None;
            t.loaded = false; // reload once to pick up the final events
        }
        let events = RunDir::from_path(&entry.run_dir).events();
        let exists = entry.run_dir.exists();
        state.set_missing(&entry.run_id, !exists);
        if !exists {
            t.loaded = false;
            continue;
        }
        if t.tail.is_none() && t.loaded {
            // Ended Run: cheap stat to notice a resume.
            let len = std::fs::metadata(&events).map_or(0, |m| m.len());
            if len == t.len {
                continue;
            }
        } else if t.tail.is_some() {
            continue;
        }
        attach(state, &entry, t).await;
    }
}

/// Load the whole journal into the view, then follow it if the Run is live.
async fn attach(state: &AppState, entry: &IndexEntry, t: &mut Tracked) {
    let events_path = RunDir::from_path(&entry.run_dir).events();
    let len = std::fs::metadata(&events_path).map_or(0, |m| m.len());
    let p = events_path.clone();
    let events = match tokio::task::spawn_blocking(move || read_all(p)).await {
        Ok(Ok(e)) => e,
        Ok(Err(e)) => {
            tracing::warn!("cannot read journal of Run {}: {e}", entry.run_id);
            return;
        }
        Err(_) => return,
    };
    for ev in &events {
        state.apply(&entry.run_id, ev);
    }
    t.loaded = true;
    t.len = len;
    let live = state
        .snapshot(&entry.run_id)
        .is_some_and(|s| s.view.status == ViewStatus::Running);
    if live {
        let (state, run_id) = (state.clone(), entry.run_id.clone());
        t.tail = Some(tokio::spawn(async move {
            let mut stream = std::pin::pin!(tail_with_interval(events_path, TAIL_INTERVAL));
            while let Some(ev) = stream.next().await {
                state.apply(&run_id, &ev); // replayed seqs are dropped by `apply`
                let ended = state
                    .snapshot(&run_id)
                    .is_none_or(|s| s.view.status != ViewStatus::Running);
                if ended {
                    return;
                }
            }
        }));
    }
}
