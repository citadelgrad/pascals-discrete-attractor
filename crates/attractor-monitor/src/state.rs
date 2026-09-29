//! Shared Monitor state: one projection per Run, kept current by the watcher.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use attractor_journal::{IndexEntry, JournalEvent, RunDir};
use tokio::sync::broadcast;

use crate::projection::RunView;
use crate::security::CsrfToken;

/// Events buffered per Run for live subscribers before one is marked lagged.
const CHANNEL_CAPACITY: usize = 1024;

#[derive(Clone)]
pub struct AppState(Arc<Inner>);

struct Inner {
    index_path: PathBuf,
    capacity: usize,
    csrf: CsrfToken,
    pas_exe: RwLock<Option<PathBuf>>,
    /// (run id, question id) -> choice, for answers our own `pas answer` accepted.
    answers_sent: Mutex<HashMap<(String, String), String>>,
    runs: RwLock<HashMap<String, RunSlot>>,
}

struct RunSlot {
    entry: IndexEntry,
    view: RunView,
    missing: bool,
    tx: broadcast::Sender<JournalEvent>,
}

/// A copy of one Run's state.
#[derive(Debug, Clone)]
pub struct RunSnapshot {
    pub entry: IndexEntry,
    pub view: RunView,
    pub missing: bool,
    /// Answers this Monitor sent that the journal may not show yet, by question id.
    pub answers_sent: HashMap<String, String>,
}

impl AppState {
    pub fn new(index_path: impl Into<PathBuf>) -> Self {
        Self::with_capacity(index_path, CHANNEL_CAPACITY)
    }

    /// The per-process CSRF token that unsafe requests must present.
    pub fn csrf_token(&self) -> &CsrfToken {
        &self.0.csrf
    }

    /// The executable the controls run: the Monitor's own binary, unless a
    /// test replaced it.
    pub fn pas_exe(&self) -> std::io::Result<PathBuf> {
        let over = self.0.pas_exe.read().unwrap_or_else(|e| e.into_inner());
        match over.as_ref() {
            Some(p) => Ok(p.clone()),
            None => crate::spawn::pas_exe(),
        }
    }

    /// Replace the executable the controls run (tests only).
    #[doc(hidden)]
    pub fn set_pas_exe(&self, exe: impl Into<PathBuf>) {
        *self.0.pas_exe.write().unwrap_or_else(|e| e.into_inner()) = Some(exe.into());
    }

    /// Like [`AppState::new`] with a custom broadcast capacity (for tests).
    pub fn with_capacity(index_path: impl Into<PathBuf>, capacity: usize) -> Self {
        Self(Arc::new(Inner {
            index_path: index_path.into(),
            capacity,
            csrf: CsrfToken::generate(),
            pas_exe: RwLock::default(),
            answers_sent: Mutex::default(),
            runs: RwLock::default(),
        }))
    }

    pub fn index_path(&self) -> &std::path::Path {
        &self.0.index_path
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, RunSlot>> {
        self.0.runs.write().unwrap_or_else(|e| e.into_inner())
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, RunSlot>> {
        self.0.runs.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Register a Run from its Index entry. Returns true if the Run was new.
    pub fn upsert_entry(&self, entry: IndexEntry) -> bool {
        let missing = entry.is_missing();
        let mut runs = self.write();
        if let Some(slot) = runs.get_mut(&entry.run_id) {
            slot.missing = missing;
            return false;
        }
        let (tx, _) = broadcast::channel(self.0.capacity);
        runs.insert(
            entry.run_id.clone(),
            RunSlot {
                entry,
                view: RunView::default(),
                missing,
                tx,
            },
        );
        true
    }

    pub fn set_missing(&self, run_id: &str, missing: bool) {
        if let Some(slot) = self.write().get_mut(run_id) {
            slot.missing = missing;
        }
    }

    /// Fold `event` into the Run's view and broadcast it. Events at or below
    /// the view's `last_seq` are duplicates and are dropped (returns false).
    /// Applying and broadcasting happen under one lock, so a subscriber
    /// neither misses nor repeats an Event.
    pub fn apply(&self, run_id: &str, event: &JournalEvent) -> bool {
        let mut runs = self.write();
        let Some(slot) = runs.get_mut(run_id) else {
            return false;
        };
        if event.seq <= slot.view.last_seq {
            return false;
        }
        slot.view.apply(event);
        let _ = slot.tx.send(event.clone());
        true
    }

    /// The Run's `events.jsonl` and a receiver of Events applied from now on.
    pub fn subscribe(&self, run_id: &str) -> Option<(PathBuf, broadcast::Receiver<JournalEvent>)> {
        let runs = self.read();
        let slot = runs.get(run_id)?;
        Some((
            RunDir::from_path(&slot.entry.run_dir).events(),
            slot.tx.subscribe(),
        ))
    }

    /// Remember that `pas answer` accepted `choice` for the gate. This records
    /// only what the Monitor did; the journal stays the observation of the Run.
    pub fn mark_answer_sent(&self, run_id: &str, question_id: &str, choice: &str) {
        self.0
            .answers_sent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((run_id.into(), question_id.into()), choice.into());
    }

    fn sent_for(&self, run_id: &str) -> HashMap<String, String> {
        self.0
            .answers_sent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|((r, _), _)| r == run_id)
            .map(|((_, q), c)| (q.clone(), c.clone()))
            .collect()
    }

    pub fn snapshot(&self, run_id: &str) -> Option<RunSnapshot> {
        self.read().get(run_id).map(|s| RunSnapshot {
            entry: s.entry.clone(),
            view: s.view.clone(),
            missing: s.missing,
            answers_sent: self.sent_for(run_id),
        })
    }

    /// All Runs, ordered by Index `started_at` then Run id.
    pub fn list(&self) -> Vec<RunSnapshot> {
        let mut all: Vec<_> = self
            .read()
            .values()
            .map(|s| RunSnapshot {
                entry: s.entry.clone(),
                view: s.view.clone(),
                missing: s.missing,
                answers_sent: HashMap::new(),
            })
            .collect();
        all.sort_by(|a, b| {
            (a.entry.started_at, &a.entry.run_id).cmp(&(b.entry.started_at, &b.entry.run_id))
        });
        all
    }
}
