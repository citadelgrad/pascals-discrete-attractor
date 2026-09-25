//! Run Journal writer (spec C3, write side).

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::Deserialize;

use crate::event::{EventData, JournalEvent};
use crate::layout::EVENTS_FILE;

/// Longest time between fsyncs of the journal (C3).
const FSYNC_INTERVAL: Duration = Duration::from_secs(5);

/// Appends Events to one Run's `events.jsonl`.
///
/// There must be exactly one writer per Run; `run.lock` guarantees that, not
/// this type. The writer is synchronous because the engine emits Events
/// synchronously, and works through `&self` so it can be shared (e.g. in an
/// `Arc`) with the heartbeat task.
#[derive(Debug)]
pub struct JournalWriter {
    path: PathBuf,
    run_id: String,
    attempt: u32,
    inner: Mutex<Inner>,
}

#[derive(Debug)]
struct Inner {
    file: File,
    next_seq: u64,
    last_sync: Instant,
}

#[derive(Deserialize)]
struct SeqOnly {
    seq: u64,
}

impl JournalWriter {
    /// Open (or create) `<run_dir>/events.jsonl` for a new Attempt.
    ///
    /// The first Event written gets `seq` = last `seq` in the journal + 1, or
    /// 1 for a new journal. A torn final line, left by a process killed
    /// mid-write, is cut off so the next line is not glued onto it.
    pub fn open(run_dir: &Path, run_id: &str, attempt: u32) -> io::Result<Self> {
        std::fs::create_dir_all(run_dir)?;
        let path = run_dir.join(EVENTS_FILE);
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(&path)?;

        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let complete = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        if complete < bytes.len() {
            file.set_len(complete as u64)?;
            file.sync_data()?;
        }
        let last_seq = bytes[..complete]
            .split(|&b| b == b'\n')
            .filter_map(|line| serde_json::from_slice::<SeqOnly>(line).ok())
            .map(|s| s.seq)
            .max()
            .unwrap_or(0);

        Ok(Self {
            path,
            run_id: run_id.to_string(),
            attempt,
            inner: Mutex::new(Inner {
                file,
                next_seq: last_seq + 1,
                last_sync: Instant::now(),
            }),
        })
    }

    /// Append one Event as a single `write` and return it as stored.
    ///
    /// `seq` advances only when the write succeeds, so a failed write leaves
    /// no gap and the caller may simply carry on with the next Event.
    pub fn append(&self, data: EventData) -> io::Result<JournalEvent> {
        let mut inner = self.lock();
        let event = JournalEvent::new(
            inner.next_seq,
            Utc::now(),
            self.run_id.clone(),
            self.attempt,
            data,
        );
        let mut line = serde_json::to_vec(&event).map_err(io::Error::other)?;
        line.push(b'\n');
        inner.file.write_all(&line)?;
        inner.next_seq += 1;
        if event.data.needs_fsync() || inner.last_sync.elapsed() >= FSYNC_INTERVAL {
            inner.file.sync_data()?;
            inner.last_sync = Instant::now();
        }
        Ok(event)
    }

    /// Force the journal to disk.
    pub fn sync(&self) -> io::Result<()> {
        let mut inner = self.lock();
        inner.file.sync_data()?;
        inner.last_sync = Instant::now();
        Ok(())
    }

    /// The `seq` the next appended Event will get.
    pub fn next_seq(&self) -> u64 {
        self.lock().next_seq
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}
