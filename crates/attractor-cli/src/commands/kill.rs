//! `pas kill <run-id> [--grace 10s] [--json]`: end an active Run now (spec
//! File Change 12, C1, C3, C5, C6).
//!
//! The PID comes from the Run's last Heartbeat. It is signalled only when it
//! provably is the Run: the Run is `running`, the Pipeline lock is held right
//! now, and the lock file names that PID and this Run. SIGTERM goes first (the
//! Run then journals `AttemptEnded{stopped}` itself); after the grace period
//! SIGKILL follows, together with the process groups of the Run's provider
//! children, which lead their own groups. This command never writes the
//! journal.

use std::path::{Path, PathBuf};
use std::time::Duration;

use attractor_journal::{
    parse_run_id, read_all, read_index_at, run_status, EventData, RunDir, RunStatus,
};
use serde::Serialize;

use super::run::RunRefused;
use super::run_lock::{read_holder, RunLock};
use super::runs::pid_alive;

/// How often the process is polled while waiting for it to exit.
const POLL: Duration = Duration::from_millis(50);
/// How long to wait for the process to vanish after SIGKILL.
const KILL_WAIT: Duration = Duration::from_secs(1);

#[derive(Debug)]
enum KillError {
    UnknownRun(String),
    RunMissing { run_id: String, run_dir: PathBuf },
    NotActive { run_id: String, status: RunStatus },
    NoPid(String),
    NotLockHolder(String),
    InvalidGrace(String),
    KillFailed { run_id: String, pid: u32 },
    Io(String),
}

impl KillError {
    /// Stable `error.code` of the `--json` failure object.
    fn code(&self) -> &'static str {
        match self {
            Self::UnknownRun(_) => "unknown_run",
            Self::RunMissing { .. } => "run_missing",
            Self::NotActive { .. } => "not_active",
            Self::NoPid(_) => "no_pid",
            Self::NotLockHolder(_) => "pid_not_lock_holder",
            Self::InvalidGrace(_) => "invalid_grace",
            Self::KillFailed { .. } => "kill_failed",
            Self::Io(_) => "io_error",
        }
    }
}

impl std::fmt::Display for KillError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownRun(id) => write!(f, "unknown run-id {id}: not in the Run Index"),
            Self::RunMissing { run_id, run_dir } => write!(
                f,
                "run {run_id} is missing: its folder {} no longer exists",
                run_dir.display()
            ),
            Self::NotActive { run_id, status } => {
                write!(f, "run {run_id} is not active (status: {status})")
            }
            Self::NoPid(run_id) => write!(f, "run {run_id} has no recorded PID in its journal"),
            Self::NotLockHolder(message) | Self::Io(message) => f.write_str(message),
            Self::InvalidGrace(message) => write!(f, "invalid --grace: {message}"),
            Self::KillFailed { run_id, pid } => {
                write!(f, "run {run_id} (pid {pid}) is still alive after SIGKILL")
            }
        }
    }
}

/// A completed kill.
#[derive(Debug, Serialize)]
struct Killed {
    v: u32,
    ok: bool,
    run_id: String,
    pid: u32,
    /// The signal that ended the Run: `SIGTERM` or `SIGKILL`.
    signal: &'static str,
    /// Provider child process groups signalled on the SIGKILL path.
    children_killed: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sig {
    Term,
    Kill,
}

/// What `kill` needs from the operating system; tests replace it.
trait Os {
    fn alive(&self, pid: u32) -> bool;
    /// Whether `pid` leads its own process group.
    fn leads_group(&self, pid: u32) -> bool;
    /// Signal process `pid`, or its whole group when `group` is set.
    fn send(&mut self, pid: u32, group: bool, sig: Sig);
    /// Process groups of the descendants of `pid` other than its own group.
    fn descendant_groups(&self, pid: u32) -> Vec<u32>;
    /// Signal a process group.
    fn send_group(&mut self, pgid: u32, sig: Sig);
    /// Sleep, then report the time elapsed since `kill` started.
    fn sleep(&mut self, d: Duration);
    fn elapsed(&self) -> Duration;
}

/// The PID of the last Heartbeat of the last Attempt, else of its
/// `AttemptStarted`.
fn recorded_pid(run_dir: &RunDir) -> Result<Option<u32>, KillError> {
    let events = read_all(run_dir.events())
        .map_err(|e| KillError::Io(format!("cannot read the Run Journal: {e}")))?;
    let Some(last) = events.iter().map(|e| e.attempt).max() else {
        return Ok(None);
    };
    let (mut heartbeat, mut started) = (None, None);
    for event in events.iter().filter(|e| e.attempt == last) {
        match &event.data {
            EventData::Heartbeat { pid } => heartbeat = Some(*pid),
            EventData::AttemptStarted { pid, .. } => started = Some(*pid),
            _ => {}
        }
    }
    Ok(heartbeat.or(started))
}

/// Check that `pid` holds the Pipeline lock of `run_id` right now.
fn verify_lock_holder(
    run_dir: &RunDir,
    run_id: &str,
    pid: u32,
    lock_held: impl Fn(&Path) -> bool,
) -> Result<(), KillError> {
    let refuse = |why: String| {
        Err(KillError::NotLockHolder(format!(
            "pid {pid} does not hold the lock of run {run_id}: {why}; no signal sent"
        )))
    };
    let Some(pipeline) = run_dir.pipeline_dir() else {
        return refuse("the Run folder is not inside a Pipeline folder".into());
    };
    let lock = pipeline.run_lock();
    if !lock_held(&lock) {
        return refuse(format!("{} is not locked", lock.display()));
    }
    let holder = read_holder(&lock);
    if holder.pid != Some(pid) {
        return refuse(format!("the lock is held by {holder}"));
    }
    if holder.run_id.as_deref().and_then(parse_run_id).as_deref() != Some(run_id) {
        return refuse(format!("the lock is held by {holder}"));
    }
    Ok(())
}

/// Kill Run `run_id` found through the Index at `index` (`None`: no state
/// folder, so an empty Index).
fn kill(
    index: Option<&Path>,
    run_id: &str,
    grace: &str,
    now: chrono::DateTime<chrono::Utc>,
    os: &mut impl Os,
    lock_held: impl Fn(&Path) -> bool,
) -> Result<Killed, KillError> {
    let grace = attractor_dot::duration_serde::parse_duration_str(grace)
        .map_err(KillError::InvalidGrace)?;
    let unknown_run = || KillError::UnknownRun(run_id.to_string());
    let run_id = parse_run_id(run_id).ok_or_else(unknown_run)?;
    let entries = match index {
        Some(path) => read_index_at(path)
            .map_err(|e| KillError::Io(format!("cannot read the Run Index: {e}")))?,
        None => Vec::new(),
    };
    let entry = entries
        .iter()
        .rev()
        .find(|e| parse_run_id(&e.run_id).as_deref() == Some(run_id.as_str()))
        .ok_or_else(unknown_run)?;
    let (status, _) = run_status(entry, now, |pid| os.alive(pid));
    match status {
        RunStatus::Running => {}
        RunStatus::Missing => {
            return Err(KillError::RunMissing {
                run_id,
                run_dir: entry.run_dir.clone(),
            })
        }
        status => return Err(KillError::NotActive { run_id, status }),
    }
    let run_dir = RunDir::from_path(&entry.run_dir);
    let pid = recorded_pid(&run_dir)?.ok_or_else(|| KillError::NoPid(run_id.clone()))?;
    // Never a PID that would address a process group or the caller.
    if pid <= 1 || pid == std::process::id() {
        return Err(KillError::NotLockHolder(format!(
            "pid {pid} cannot be the Run of {run_id}; no signal sent"
        )));
    }
    verify_lock_holder(&run_dir, &run_id, pid, lock_held)?;

    // Group signals only when the Run leads its group; a Run started from a
    // script shares its caller's group and must not take the caller down.
    let group = os.leads_group(pid);
    os.send(pid, group, Sig::Term);
    let mut signal = "SIGTERM";
    let mut children_killed = 0;
    while os.alive(pid) {
        if os.elapsed() >= grace {
            signal = "SIGKILL";
            // Snapshot before the Run dies and its children are reparented.
            let groups = os.descendant_groups(pid);
            os.send(pid, group, Sig::Kill);
            for pgid in &groups {
                os.send_group(*pgid, Sig::Kill);
            }
            children_killed = groups.len();
            let mut waited = Duration::ZERO;
            while os.alive(pid) && waited < KILL_WAIT {
                os.sleep(POLL);
                waited += POLL;
            }
            if os.alive(pid) {
                return Err(KillError::KillFailed { run_id, pid });
            }
            break;
        }
        os.sleep(POLL);
    }
    Ok(Killed {
        v: 1,
        ok: true,
        run_id,
        pid,
        signal,
        children_killed,
    })
}

/// Parse `ps -A -o pid=,ppid=,pgid=` output into the process groups of the
/// descendants of `root` other than `root`'s own group.
fn groups_of_descendants(ps: &str, root: u32) -> Vec<u32> {
    let rows: Vec<(u32, u32, u32)> = ps
        .lines()
        .filter_map(|line| {
            let mut cols = line.split_whitespace().map(|c| c.parse::<u32>());
            match (cols.next(), cols.next(), cols.next(), cols.next()) {
                (Some(Ok(pid)), Some(Ok(ppid)), Some(Ok(pgid)), None) => Some((pid, ppid, pgid)),
                _ => None,
            }
        })
        .collect();
    let own_group = rows.iter().find(|r| r.0 == root).map_or(root, |r| r.2);
    let mut seen = vec![root];
    let mut groups = Vec::new();
    let mut i = 0;
    while i < seen.len() {
        let parent = seen[i];
        i += 1;
        for &(pid, ppid, pgid) in &rows {
            if ppid == parent && !seen.contains(&pid) {
                seen.push(pid);
                if pgid > 1 && pgid != own_group && !groups.contains(&pgid) {
                    groups.push(pgid);
                }
            }
        }
    }
    groups
}

#[cfg(unix)]
struct RealOs {
    start: std::time::Instant,
}

#[cfg(unix)]
impl RealOs {
    fn signo(sig: Sig) -> libc::c_int {
        match sig {
            Sig::Term => libc::SIGTERM,
            Sig::Kill => libc::SIGKILL,
        }
    }
}

#[cfg(unix)]
impl Os for RealOs {
    fn alive(&self, pid: u32) -> bool {
        pid_alive(pid)
    }

    fn leads_group(&self, pid: u32) -> bool {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        // SAFETY: getpgid only reads process state.
        pid > 1 && unsafe { libc::getpgid(pid) } == pid
    }

    fn send(&mut self, pid: u32, group: bool, sig: Sig) {
        if group {
            self.send_group(pid, sig);
        } else if let Ok(pid) = libc::pid_t::try_from(pid) {
            if pid > 1 {
                // SAFETY: the caller verified that this PID is the Run.
                unsafe { libc::kill(pid, Self::signo(sig)) };
            }
        }
    }

    fn descendant_groups(&self, pid: u32) -> Vec<u32> {
        let own = unsafe { libc::getpgrp() } as u32;
        std::process::Command::new("ps")
            .args(["-A", "-o", "pid=,ppid=,pgid="])
            .output()
            .map(|out| groups_of_descendants(&String::from_utf8_lossy(&out.stdout), pid))
            .unwrap_or_default()
            .into_iter()
            .filter(|g| *g != own)
            .collect()
    }

    fn send_group(&mut self, pgid: u32, sig: Sig) {
        if let Ok(pgid) = libc::pid_t::try_from(pgid) {
            // Never group 0 or 1, and never our own group.
            // SAFETY: getpgrp reads process state; killpg targets a verified group.
            if pgid > 1 && pgid != unsafe { libc::getpgrp() } {
                unsafe { libc::killpg(pgid, Self::signo(sig)) };
            }
        }
    }

    fn sleep(&mut self, d: Duration) {
        std::thread::sleep(d);
    }

    fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }
}

/// `pas kill`.
#[cfg(unix)]
pub fn cmd_kill(run_id: &str, grace: &str, json: bool) -> anyhow::Result<()> {
    let index = attractor_journal::index_path().ok();
    let mut os = RealOs {
        start: std::time::Instant::now(),
    };
    let result = kill(
        index.as_deref(),
        run_id,
        grace,
        chrono::Utc::now(),
        &mut os,
        RunLock::is_held,
    );
    report(run_id, json, result)
}

/// `pas kill` cannot signal processes here.
#[cfg(not(unix))]
pub fn cmd_kill(run_id: &str, _grace: &str, json: bool) -> anyhow::Result<()> {
    report(
        run_id,
        json,
        Err(KillError::Io(
            "pas kill is not supported on this platform".into(),
        )),
    )
}

fn report(run_id: &str, json: bool, result: Result<Killed, KillError>) -> anyhow::Result<()> {
    match result {
        Ok(done) => {
            if json {
                println!("{}", serde_json::to_string(&done)?);
            } else {
                println!(
                    "killed run {} (pid {}, {})",
                    done.run_id, done.pid, done.signal
                );
            }
            Ok(())
        }
        Err(error) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "v": 1,
                        "ok": false,
                        "run_id": run_id,
                        "error": {"code": error.code(), "message": error.to_string()},
                    })
                );
            }
            Err(anyhow::Error::new(RunRefused {
                exit_code: 1,
                message: error.to_string(),
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use attractor_journal::{append_entry_at, IndexEntry, PipelineDir, INDEX_FILE};

    const RUN: &str = "0192a000-0000-7000-8000-000000000001";
    const PID: u32 = 4242;

    fn line(seq: u32, ty: &str, data: &str) -> String {
        format!(
            r#"{{"v":1,"seq":{seq},"ts":"2026-09-24T10:00:00.000Z","run_id":"{RUN}","attempt":1,"type":"{ty}","data":{data}}}"#
        ) + "\n"
    }

    fn started(pid: u32) -> String {
        line(
            1,
            "AttemptStarted",
            &format!(
                r#"{{"attempt":1,"pid":{pid},"argv":[],"pas_version":"0","resumed_from_node":null}}"#
            ),
        )
    }

    fn heartbeat(pid: u32) -> String {
        line(2, "Heartbeat", &format!(r#"{{"pid":{pid}}}"#))
    }

    fn ended(reason: &str) -> String {
        line(
            3,
            "AttemptEnded",
            &format!(r#"{{"attempt":1,"reason":"{reason}"}}"#),
        )
    }

    struct Setup {
        _tmp: tempfile::TempDir,
        index: PathBuf,
        lock: PathBuf,
    }

    fn setup(journal: Option<&str>) -> Setup {
        let tmp = tempfile::tempdir().unwrap();
        let pipeline = PipelineDir::new(tmp.path().join("pipe"));
        let run_dir = pipeline.run(RUN).unwrap();
        if let Some(journal) = journal {
            run_dir.create_all().unwrap();
            std::fs::write(run_dir.events(), journal).unwrap();
        }
        let index = tmp.path().join(INDEX_FILE);
        let entry = IndexEntry::new(
            RUN,
            chrono::Utc::now(),
            "/w",
            "/p.dot",
            run_dir.path().to_path_buf(),
        );
        append_entry_at(&index, &entry).unwrap();
        let lock = pipeline.run_lock();
        std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
        std::fs::write(&lock, format!(r#"{{"pid":{PID},"run_id":"{RUN}"}}"#)).unwrap();
        Setup {
            _tmp: tmp,
            index,
            lock,
        }
    }

    /// A fake OS: the process dies at the first signal in `dies_on`.
    struct Fake {
        dies_on: Option<Sig>,
        leader: bool,
        children: Vec<u32>,
        alive: bool,
        clock: Duration,
        sent: Vec<(u32, bool, Sig)>,
        group_sent: Vec<(u32, Sig)>,
    }

    impl Fake {
        fn new(dies_on: Option<Sig>) -> Self {
            Self {
                dies_on,
                leader: true,
                children: vec![],
                alive: true,
                clock: Duration::ZERO,
                sent: vec![],
                group_sent: vec![],
            }
        }
    }

    impl Os for Fake {
        fn alive(&self, _pid: u32) -> bool {
            self.alive
        }
        fn leads_group(&self, _pid: u32) -> bool {
            self.leader
        }
        fn send(&mut self, pid: u32, group: bool, sig: Sig) {
            self.sent.push((pid, group, sig));
            if self.dies_on == Some(sig) || sig == Sig::Kill {
                self.alive = false;
            }
        }
        fn descendant_groups(&self, _pid: u32) -> Vec<u32> {
            self.children.clone()
        }
        fn send_group(&mut self, pgid: u32, sig: Sig) {
            self.group_sent.push((pgid, sig));
        }
        fn sleep(&mut self, d: Duration) {
            self.clock += d;
        }
        fn elapsed(&self) -> Duration {
            self.clock
        }
    }

    fn go(s: &Setup, os: &mut Fake, grace: &str) -> Result<Killed, KillError> {
        kill(Some(&s.index), RUN, grace, chrono::Utc::now(), os, |p| {
            p.exists()
        })
    }

    #[test]
    fn sigterm_is_enough_for_a_cooperative_run() {
        let s = setup(Some(&(started(1) + &heartbeat(PID))));
        let mut os = Fake::new(Some(Sig::Term));
        let done = go(&s, &mut os, "10s").unwrap();
        assert_eq!(done.signal, "SIGTERM");
        assert_eq!(done.pid, PID);
        assert_eq!(os.sent, vec![(PID, true, Sig::Term)]);
        assert!(os.clock < Duration::from_secs(1));
    }

    #[test]
    fn sigkill_follows_after_the_grace_period_with_children() {
        let s = setup(Some(&(started(PID) + &heartbeat(PID))));
        let mut os = Fake::new(None);
        os.children = vec![777, 778];
        let done = go(&s, &mut os, "2s").unwrap();
        assert_eq!(done.signal, "SIGKILL");
        assert_eq!(done.children_killed, 2);
        assert_eq!(
            os.sent,
            vec![(PID, true, Sig::Term), (PID, true, Sig::Kill)]
        );
        assert_eq!(os.group_sent, vec![(777, Sig::Kill), (778, Sig::Kill)]);
        assert!(os.clock >= Duration::from_secs(2), "{:?}", os.clock);
        assert!(os.clock < Duration::from_secs(3), "{:?}", os.clock);
    }

    #[test]
    fn a_run_that_shares_its_callers_group_gets_a_single_pid_signal() {
        let s = setup(Some(&heartbeat(PID)));
        let mut os = Fake::new(Some(Sig::Term));
        os.leader = false;
        go(&s, &mut os, "1s").unwrap();
        assert_eq!(os.sent, vec![(PID, false, Sig::Term)]);
    }

    #[test]
    fn the_last_heartbeat_wins_and_started_is_the_fallback() {
        let s = setup(Some(&(started(1) + &heartbeat(PID))));
        assert_eq!(
            go(&s, &mut Fake::new(Some(Sig::Term)), "1s").unwrap().pid,
            PID
        );
        let s = setup(Some(&started(PID)));
        assert_eq!(
            go(&s, &mut Fake::new(Some(Sig::Term)), "1s").unwrap().pid,
            PID
        );
        let s = setup(Some(""));
        let error = go(&s, &mut Fake::new(None), "1s").unwrap_err();
        assert_eq!(error.code(), "no_pid");
    }

    #[test]
    fn lock_holder_mismatches_send_nothing() {
        let journal = started(PID) + &heartbeat(PID);
        // Lock file names another PID.
        let s = setup(Some(&journal));
        std::fs::write(&s.lock, format!(r#"{{"pid":9,"run_id":"{RUN}"}}"#)).unwrap();
        let mut os = Fake::new(Some(Sig::Term));
        let error = go(&s, &mut os, "1s").unwrap_err();
        assert_eq!(error.code(), "pid_not_lock_holder");
        assert!(os.sent.is_empty());
        // Lock file names another Run.
        std::fs::write(
            &s.lock,
            format!(r#"{{"pid":{PID},"run_id":"0192a000-0000-7000-8000-0000000000ff"}}"#),
        )
        .unwrap();
        assert_eq!(
            go(&s, &mut os, "1s").unwrap_err().code(),
            "pid_not_lock_holder"
        );
        // Lock file without contents.
        std::fs::write(&s.lock, "").unwrap();
        assert_eq!(
            go(&s, &mut os, "1s").unwrap_err().code(),
            "pid_not_lock_holder"
        );
        // Matching contents but nobody holds the lock.
        std::fs::write(&s.lock, format!(r#"{{"pid":{PID},"run_id":"{RUN}"}}"#)).unwrap();
        let error = kill(
            Some(&s.index),
            RUN,
            "1s",
            chrono::Utc::now(),
            &mut os,
            |_| false,
        )
        .unwrap_err();
        assert_eq!(error.code(), "pid_not_lock_holder");
        assert!(error.to_string().contains("not locked"), "{error}");
        assert!(os.sent.is_empty() && os.group_sent.is_empty());
    }

    #[test]
    fn inactive_runs_are_refused_before_the_lock_is_looked_at() {
        for reason in ["completed", "failed", "stopped"] {
            let s = setup(Some(&(started(PID) + &heartbeat(PID) + &ended(reason))));
            let mut os = Fake::new(Some(Sig::Term));
            let error = go(&s, &mut os, "1s").unwrap_err();
            assert_eq!(error.code(), "not_active", "{reason}");
            assert!(error.to_string().contains("not active"));
            assert!(os.sent.is_empty());
        }
        let s = setup(Some(&started(PID)));
        let mut os = Fake::new(None);
        os.alive = false;
        let error = kill(
            Some(&s.index),
            RUN,
            "1s",
            chrono::Utc::now() + chrono::Duration::minutes(10),
            &mut os,
            |_| true,
        )
        .unwrap_err();
        assert_eq!(error.code(), "not_active");
        assert!(os.sent.is_empty());
    }

    #[test]
    fn unknown_missing_and_invalid_inputs() {
        let s = setup(None);
        assert_eq!(
            go(&s, &mut Fake::new(None), "1s").unwrap_err().code(),
            "run_missing"
        );
        let other = "0192a000-0000-7000-8000-0000000000ff";
        let error = kill(
            Some(&s.index),
            other,
            "1s",
            chrono::Utc::now(),
            &mut Fake::new(None),
            |_| true,
        )
        .unwrap_err();
        assert_eq!(error.code(), "unknown_run");
        let error = kill(
            None,
            RUN,
            "1s",
            chrono::Utc::now(),
            &mut Fake::new(None),
            |_| true,
        )
        .unwrap_err();
        assert_eq!(error.code(), "unknown_run");
        for bad in ["abc", "", "10", "-1s"] {
            let s = setup(Some(&heartbeat(PID)));
            let mut os = Fake::new(Some(Sig::Term));
            assert_eq!(
                go(&s, &mut os, bad).unwrap_err().code(),
                "invalid_grace",
                "{bad}"
            );
            assert!(os.sent.is_empty());
        }
    }

    #[test]
    fn grace_accepts_units_and_zero() {
        let s = setup(Some(&heartbeat(PID)));
        for good in ["500ms", "1m", "0s"] {
            let mut os = Fake::new(Some(Sig::Term));
            assert!(go(&s, &mut os, good).is_ok(), "{good}");
        }
        // Zero grace escalates at once for a Run that ignores SIGTERM.
        let mut os = Fake::new(None);
        assert_eq!(go(&s, &mut os, "0s").unwrap().signal, "SIGKILL");
    }

    #[test]
    fn a_survivor_of_sigkill_is_a_failure() {
        struct Undead(Fake);
        impl Os for Undead {
            fn alive(&self, _: u32) -> bool {
                true
            }
            fn leads_group(&self, p: u32) -> bool {
                self.0.leads_group(p)
            }
            fn send(&mut self, p: u32, g: bool, s: Sig) {
                self.0.send(p, g, s)
            }
            fn descendant_groups(&self, p: u32) -> Vec<u32> {
                self.0.descendant_groups(p)
            }
            fn send_group(&mut self, p: u32, s: Sig) {
                self.0.send_group(p, s)
            }
            fn sleep(&mut self, d: Duration) {
                self.0.sleep(d)
            }
            fn elapsed(&self) -> Duration {
                self.0.elapsed()
            }
        }
        let s = setup(Some(&heartbeat(PID)));
        let mut os = Undead(Fake::new(None));
        let error = kill(
            Some(&s.index),
            RUN,
            "0s",
            chrono::Utc::now(),
            &mut os,
            |_| true,
        )
        .unwrap_err();
        assert_eq!(error.code(), "kill_failed");
    }

    #[test]
    fn descendant_groups_walks_the_process_tree() {
        let ps = "\
  1     0     1
 10     1    10
 11    10    10
 12    11    12
 13    12    13
 20     1    20
 21     1    10
garbage line
";
        // 10 leads group 10; 11 shares it; 12 and 13 lead their own groups.
        assert_eq!(groups_of_descendants(ps, 10), vec![12, 13]);
        assert_eq!(groups_of_descendants(ps, 20), Vec::<u32>::new());
        assert_eq!(groups_of_descendants("", 10), Vec::<u32>::new());
    }
}
