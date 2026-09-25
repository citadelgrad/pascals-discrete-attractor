//! Run Journal, Run Index, and Run-folder layout for PAS Runs.
//!
//! This crate owns the observation contract between the engine and anything
//! that watches a Run (ADR 0001): the Run-folder layout (spec C1), Run IDs
//! (C2), the versioned Run Journal (C3), and the machine-wide Run Index (C4).
//! It depends on no other `attractor-*` crate, so the Monitor can observe Runs
//! through it alone.

pub mod event;
pub mod index;
pub mod layout;
pub mod meta;
pub mod reader;
pub mod status;
pub mod writer;

pub use event::{AnswerSource, AttemptEndReason, CommitRef, EventData, JournalEvent, TaskSummary};
pub use index::{
    append_entry, append_entry_at, index_path, read_index, read_index_at, resolve_state_dir,
    state_dir, IndexEntry, INDEX_FILE,
};
pub use layout::{
    new_run_id, parse_run_id, PipelineDir, RunDir, ANSWERS_DIR, CHECKPOINT_FILE, CONSOLE_LOG,
    CONTROL_DIR, EVENTS_FILE, RUNS_DIR, RUN_JSON, RUN_LOCK, STOP_FILE, TRANSCRIPTS_DIR,
};
pub use meta::{read_run_meta, write_run_meta, RunMeta};
pub use reader::{read_all, read_all_raw, tail, tail_with_interval};
pub use status::{derive_status, run_status, RunStatus, CRASH_AFTER};
pub use writer::JournalWriter;

/// Version of the Run Journal envelope (`v` on every journal line).
pub const JOURNAL_VERSION: u32 = 1;
/// Version of a Run Index entry (`v` on every `runs.jsonl` line).
pub const INDEX_VERSION: u32 = 1;
/// Version of `run.json`.
pub const RUN_META_VERSION: u32 = 1;

#[cfg(test)]
mod tests {
    /// AC: the crate depends on no other `attractor-*` crate (ADR 0001).
    #[test]
    fn crate_has_no_attractor_dependencies() {
        let manifest =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
        let mut in_deps = false;
        for line in manifest.lines().map(str::trim) {
            if line.starts_with('[') {
                in_deps = line.ends_with("dependencies]");
                continue;
            }
            if in_deps {
                assert!(
                    !line.starts_with("attractor-") && !line.starts_with("\"attractor-"),
                    "attractor-journal must not depend on {line}"
                );
            }
        }
    }
}
