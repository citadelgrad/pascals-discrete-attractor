//! Run status, derived from the Run Journal (spec C3, C4).
//!
//! The Run Index never stores a status. Readers such as `pas runs` and the
//! Monitor derive it here, from the last Attempt's Events. The PID probe is
//! passed in, so this crate needs no process APIs.

use std::io;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::event::{AttemptEndReason, EventData, JournalEvent};
use crate::index::IndexEntry;
use crate::layout::RunDir;
use crate::reader::read_all;

/// A Run whose last Attempt has not ended counts as crashed once its last
/// sign of life is older than this and its process is gone.
pub const CRASH_AFTER: Duration = Duration::minutes(2);

/// The derived status of a Run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Completed,
    Failed,
    Stopped,
    Crashed,
    /// The Index entry's `run_dir` no longer exists.
    Missing,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
            Self::Crashed => "crashed",
            Self::Missing => "missing",
        }
    }
}

impl std::fmt::Display for RunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Status of a Run from its journal Events. Never returns
/// [`RunStatus::Missing`].
///
/// The last Attempt (highest `attempt`) decides. If it has an `AttemptEnded`,
/// its reason gives the status. Otherwise the Run is crashed only when its
/// last sign of life (last Heartbeat, else `AttemptStarted`, else the Index
/// `started_at`) is more than [`CRASH_AFTER`] before `now` and its PID (from
/// the same Event) is unknown or `pid_alive` says it is gone; else running.
pub fn derive_status(
    events: &[JournalEvent],
    started_at: DateTime<Utc>,
    now: DateTime<Utc>,
    pid_alive: impl Fn(u32) -> bool,
) -> RunStatus {
    let last = events.iter().map(|e| e.attempt).max();
    let mut sign_of_life: Option<(DateTime<Utc>, u32)> = None;
    let mut started: Option<(DateTime<Utc>, u32)> = None;
    for event in events.iter().filter(|e| Some(e.attempt) == last) {
        match &event.data {
            EventData::AttemptEnded { reason, .. } => {
                return match reason {
                    AttemptEndReason::Completed => RunStatus::Completed,
                    AttemptEndReason::Stopped => RunStatus::Stopped,
                    AttemptEndReason::Failed
                    | AttemptEndReason::Error
                    | AttemptEndReason::BudgetExhausted
                    | AttemptEndReason::MaxSteps => RunStatus::Failed,
                };
            }
            EventData::Heartbeat { pid } => sign_of_life = Some((event.ts, *pid)),
            EventData::AttemptStarted { pid, .. } => started = Some((event.ts, *pid)),
            _ => {}
        }
    }
    let (seen_at, pid) = match sign_of_life.or(started) {
        Some((ts, pid)) => (ts, Some(pid)),
        None => (started_at, None),
    };
    if now - seen_at > CRASH_AFTER && !pid.is_some_and(pid_alive) {
        RunStatus::Crashed
    } else {
        RunStatus::Running
    }
}

/// Status of the Run of an Index entry: [`RunStatus::Missing`] if its
/// `run_dir` is gone, else [`derive_status`] over its journal.
///
/// Never fails. A journal that does not exist yet has no Events; one that
/// cannot be read is treated the same, and the read error is returned
/// alongside so the caller can report it.
pub fn run_status(
    entry: &IndexEntry,
    now: DateTime<Utc>,
    pid_alive: impl Fn(u32) -> bool,
) -> (RunStatus, Option<io::Error>) {
    if entry.is_missing() {
        return (RunStatus::Missing, None);
    }
    let (events, error) = match read_all(RunDir::from_path(&entry.run_dir).events()) {
        Ok(events) => (events, None),
        Err(e) if e.kind() == io::ErrorKind::NotFound => (Vec::new(), None),
        Err(e) => (Vec::new(), Some(e)),
    };
    (
        derive_status(&events, entry.started_at, now, pid_alive),
        error,
    )
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_790_000_000 + secs, 0).unwrap()
    }

    fn ev(attempt: u32, ts: DateTime<Utc>, data: EventData) -> JournalEvent {
        JournalEvent::new(0, ts, "r", attempt, data)
    }

    fn started(attempt: u32, ts: DateTime<Utc>, pid: u32) -> JournalEvent {
        ev(
            attempt,
            ts,
            EventData::AttemptStarted {
                attempt,
                pid,
                pas_version: "0".into(),
                argv: vec![],
                git_head: None,
                resumed_from_node: None,
            },
        )
    }

    fn ended(attempt: u32, ts: DateTime<Utc>, reason: AttemptEndReason) -> JournalEvent {
        ev(
            attempt,
            ts,
            EventData::AttemptEnded {
                attempt,
                reason,
                message: None,
            },
        )
    }

    fn beat(attempt: u32, ts: DateTime<Utc>, pid: u32) -> JournalEvent {
        ev(attempt, ts, EventData::Heartbeat { pid })
    }

    fn run_started(ts: DateTime<Utc>) -> JournalEvent {
        ev(
            1,
            ts,
            EventData::RunStarted {
                pipeline_name: "X".into(),
                pipeline_path: "/p.dot".into(),
                workdir: "/w".into(),
                epic_id: None,
                max_budget_usd: None,
                max_steps: None,
                shared_workdir: false,
            },
        )
    }

    const DEAD: fn(u32) -> bool = |_| false;
    const ALIVE: fn(u32) -> bool = |_| true;

    #[test]
    fn ended_attempt_maps_reason() {
        use AttemptEndReason::*;
        for (reason, want) in [
            (Completed, RunStatus::Completed),
            (Stopped, RunStatus::Stopped),
            (Failed, RunStatus::Failed),
            (Error, RunStatus::Failed),
            (BudgetExhausted, RunStatus::Failed),
            (MaxSteps, RunStatus::Failed),
        ] {
            let events = [
                run_started(at(0)),
                started(1, at(0), 7),
                ended(1, at(5), reason),
            ];
            // Long after the end, with the PID gone: still not crashed.
            assert_eq!(
                derive_status(&events, at(0), at(3600), DEAD),
                want,
                "{reason:?}"
            );
            assert_eq!(derive_status(&events, at(0), at(6), ALIVE), want);
        }
    }

    #[test]
    fn last_attempt_wins() {
        let mut events = vec![
            run_started(at(0)),
            started(1, at(0), 7),
            ended(1, at(5), AttemptEndReason::Stopped),
            started(2, at(10), 8),
        ];
        assert_eq!(
            derive_status(&events, at(0), at(20), DEAD),
            RunStatus::Running
        );
        events.push(ended(2, at(30), AttemptEndReason::Completed));
        assert_eq!(
            derive_status(&events, at(0), at(40), DEAD),
            RunStatus::Completed
        );
        // An earlier Attempt's end never decides for a later one, even out of order.
        let events = [
            started(2, at(10), 8),
            ended(1, at(5), AttemptEndReason::Completed),
        ];
        assert_eq!(
            derive_status(&events, at(0), at(20), DEAD),
            RunStatus::Running
        );
    }

    /// AC: crashed needs no AttemptEnded, a Heartbeat older than 2 minutes,
    /// and a PID that is not alive — all three.
    #[test]
    fn crashed_needs_all_three() {
        let open = [started(1, at(0), 7), beat(1, at(30), 7)];
        let late = at(30 + 600);
        assert_eq!(derive_status(&open, at(0), late, DEAD), RunStatus::Crashed);
        // PID alive (hung, or still starting up): running.
        assert_eq!(derive_status(&open, at(0), late, ALIVE), RunStatus::Running);
        // Fresh Heartbeat, PID gone (killed moments ago): running.
        assert_eq!(
            derive_status(&open, at(0), at(40), DEAD),
            RunStatus::Running
        );
        // Ended Attempt: never crashed.
        let closed = [
            started(1, at(0), 7),
            beat(1, at(30), 7),
            ended(1, at(31), AttemptEndReason::Failed),
        ];
        assert_eq!(derive_status(&closed, at(0), late, DEAD), RunStatus::Failed);
    }

    #[test]
    fn boundary_is_strictly_older_than_two_minutes() {
        let events = [started(1, at(0), 7), beat(1, at(30), 7)];
        let exactly = at(30) + CRASH_AFTER;
        assert_eq!(
            derive_status(&events, at(0), exactly, DEAD),
            RunStatus::Running
        );
        assert_eq!(
            derive_status(&events, at(0), exactly + Duration::milliseconds(1), DEAD),
            RunStatus::Crashed
        );
        assert_eq!(CRASH_AFTER, Duration::seconds(120));
    }

    #[test]
    fn last_heartbeat_is_the_sign_of_life() {
        // An old Heartbeat followed by a recent one: the recent one counts.
        let events = [
            started(1, at(0), 7),
            beat(1, at(30), 7),
            beat(1, at(1000), 7),
        ];
        assert_eq!(
            derive_status(&events, at(0), at(1060), DEAD),
            RunStatus::Running
        );
        assert_eq!(
            derive_status(&events, at(0), at(1200), DEAD),
            RunStatus::Crashed
        );
    }

    #[test]
    fn no_heartbeat_uses_attempt_started() {
        // The first Heartbeat comes 30 s after AttemptStarted.
        let events = [run_started(at(0)), started(1, at(0), 7)];
        assert_eq!(
            derive_status(&events, at(0), at(10), DEAD),
            RunStatus::Running
        );
        assert_eq!(
            derive_status(&events, at(0), at(300), DEAD),
            RunStatus::Crashed
        );
        assert_eq!(
            derive_status(&events, at(0), at(300), ALIVE),
            RunStatus::Running
        );
    }

    #[test]
    fn heartbeats_of_an_earlier_attempt_do_not_count() {
        // Attempt 2 started long ago and never beat; Attempt 1's recent
        // Heartbeat (impossible in practice, but out of order) is ignored.
        let events = [started(2, at(0), 8), beat(1, at(1000), 7)];
        let asked = RefCell::new(vec![]);
        let status = derive_status(&events, at(0), at(1010), |pid| {
            asked.borrow_mut().push(pid);
            false
        });
        assert_eq!(status, RunStatus::Crashed);
        assert_eq!(*asked.borrow(), [8]);
    }

    #[test]
    fn pid_comes_from_last_heartbeat_of_last_attempt() {
        let events = [
            started(1, at(0), 7),
            ended(1, at(5), AttemptEndReason::Stopped),
            started(2, at(10), 8),
            beat(2, at(40), 8),
            beat(2, at(70), 9),
        ];
        let asked = RefCell::new(vec![]);
        let status = derive_status(&events, at(0), at(1000), |pid| {
            asked.borrow_mut().push(pid);
            pid == 9
        });
        assert_eq!(status, RunStatus::Running);
        assert_eq!(*asked.borrow(), [9]);
    }

    #[test]
    fn pid_is_not_probed_while_sign_of_life_is_fresh() {
        let events = [started(1, at(0), 7)];
        let status = derive_status(&events, at(0), at(10), |_| panic!("probed"));
        assert_eq!(status, RunStatus::Running);
    }

    #[test]
    fn empty_journal_uses_index_started_at() {
        // The Index line is written before the first journal line.
        assert_eq!(derive_status(&[], at(0), at(10), DEAD), RunStatus::Running);
        assert_eq!(derive_status(&[], at(0), at(600), DEAD), RunStatus::Crashed);
        // No PID is known, so the probe is never asked.
        assert_eq!(
            derive_status(&[], at(0), at(600), |_| panic!("probed")),
            RunStatus::Crashed
        );
        // Only RunStarted: still no Attempt Event to take a time or PID from.
        let events = [run_started(at(0))];
        assert_eq!(
            derive_status(&events, at(0), at(10), DEAD),
            RunStatus::Running
        );
        assert_eq!(
            derive_status(&events, at(0), at(600), DEAD),
            RunStatus::Crashed
        );
    }

    fn entry(run_dir: &std::path::Path, started_at: DateTime<Utc>) -> IndexEntry {
        IndexEntry::new("r", started_at, "/w", "/p.dot", run_dir)
    }

    fn write_journal(run_dir: &std::path::Path, events: &[JournalEvent]) {
        let mut text = String::new();
        for (i, e) in events.iter().enumerate() {
            let mut e = e.clone();
            e.seq = i as u64 + 1;
            text.push_str(&serde_json::to_string(&e).unwrap());
            text.push('\n');
        }
        std::fs::write(RunDir::from_path(run_dir).events(), text).unwrap();
    }

    /// AC: an Index entry whose run_dir is gone is `missing`, not an error.
    #[test]
    fn run_status_missing_run_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let e = entry(&tmp.path().join("gone"), at(0));
        let (status, error) = run_status(&e, at(10), |_| panic!("probed"));
        assert_eq!(status, RunStatus::Missing);
        assert!(error.is_none());
    }

    #[test]
    fn run_status_reads_the_journal() {
        let tmp = tempfile::tempdir().unwrap();
        write_journal(
            tmp.path(),
            &[
                run_started(at(0)),
                started(1, at(0), 7),
                ended(1, at(5), AttemptEndReason::Completed),
            ],
        );
        let (status, error) = run_status(&entry(tmp.path(), at(0)), at(10), DEAD);
        assert_eq!(status, RunStatus::Completed);
        assert!(error.is_none());
    }

    #[test]
    fn run_status_without_journal_file() {
        let tmp = tempfile::tempdir().unwrap();
        let e = entry(tmp.path(), at(0));
        assert_eq!(run_status(&e, at(10), DEAD).0, RunStatus::Running);
        let (status, error) = run_status(&e, at(600), DEAD);
        assert_eq!(status, RunStatus::Crashed);
        assert!(error.is_none(), "{error:?}");
    }

    #[test]
    fn run_status_tolerates_torn_last_line() {
        let tmp = tempfile::tempdir().unwrap();
        write_journal(tmp.path(), &[started(1, at(0), 7)]);
        let events = RunDir::from_path(tmp.path()).events();
        let mut text = std::fs::read_to_string(&events).unwrap();
        text.push_str(r#"{"v":1,"seq":2,"ts":"#);
        std::fs::write(&events, text).unwrap();
        let (status, error) = run_status(&entry(tmp.path(), at(0)), at(600), ALIVE);
        assert_eq!(status, RunStatus::Running);
        assert!(error.is_none());
    }

    #[test]
    fn run_status_corrupt_journal_reports_but_does_not_fail() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(RunDir::from_path(tmp.path()).events(), "not json\n").unwrap();
        let (status, error) = run_status(&entry(tmp.path(), at(0)), at(10), DEAD);
        assert_eq!(status, RunStatus::Running);
        assert_eq!(error.unwrap().kind(), io::ErrorKind::InvalidData);
        let (status, _) = run_status(&entry(tmp.path(), at(0)), at(600), DEAD);
        assert_eq!(status, RunStatus::Crashed);
    }

    #[test]
    fn status_serializes_snake_case() {
        for (s, want) in [
            (RunStatus::Running, "running"),
            (RunStatus::Completed, "completed"),
            (RunStatus::Failed, "failed"),
            (RunStatus::Stopped, "stopped"),
            (RunStatus::Crashed, "crashed"),
            (RunStatus::Missing, "missing"),
        ] {
            assert_eq!(serde_json::to_string(&s).unwrap(), format!("\"{want}\""));
            assert_eq!(s.to_string(), want);
        }
    }
}
