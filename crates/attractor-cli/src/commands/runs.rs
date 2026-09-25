//! `pas runs [--active] [--json]`: list the Run Index with a status derived
//! from each Run Journal (spec File Change 12, C4, C6).
//!
//! Read-only: it takes no Run lock, so it can never make a starting
//! `pas run` exit 5 or 6.

use std::io;
use std::path::Path;

use attractor_journal::{run_status, IndexEntry, RunStatus};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;

/// One listed Run: the Index entry verbatim, plus its derived status.
#[derive(Debug, Serialize)]
struct RunRow {
    #[serde(flatten)]
    entry: IndexEntry,
    status: RunStatus,
}

/// The `--json` success object (C6), keys in contract order.
#[derive(Serialize)]
struct Listing<'a> {
    v: u32,
    ok: bool,
    runs: &'a [RunRow],
}

/// A journal that could not be read; its Run is still listed.
struct JournalWarning {
    run_id: String,
    error: io::Error,
}

/// Rows for the Index at `index` (`None`: no state folder, so no Index), in
/// Index order. `active` keeps only running Runs.
fn list_runs(
    index: Option<&Path>,
    now: DateTime<Utc>,
    pid_alive: impl Fn(u32) -> bool + Copy,
    active: bool,
) -> io::Result<(Vec<RunRow>, Vec<JournalWarning>)> {
    let entries = match index {
        Some(path) => attractor_journal::read_index_at(path)?,
        None => Vec::new(),
    };
    let mut rows = Vec::new();
    let mut warnings = Vec::new();
    for entry in entries {
        let (status, error) = run_status(&entry, now, pid_alive);
        if let Some(error) = error {
            warnings.push(JournalWarning {
                run_id: entry.run_id.clone(),
                error,
            });
        }
        if !active || status == RunStatus::Running {
            rows.push(RunRow { entry, status });
        }
    }
    Ok((rows, warnings))
}

/// `pas runs`.
pub fn cmd_runs(active: bool, json: bool) -> anyhow::Result<()> {
    // No resolvable state folder means no Index can exist: an empty list.
    let index = attractor_journal::index_path().ok();
    let (rows, warnings) = match list_runs(index.as_deref(), Utc::now(), pid_alive, active) {
        Ok(listed) => listed,
        Err(error) => {
            let message = format!(
                "cannot read the Run Index {}: {error}",
                index.as_deref().unwrap_or(Path::new("")).display()
            );
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "v": 1,
                        "ok": false,
                        "error": {"code": "index_unreadable", "message": message},
                    })
                );
            }
            anyhow::bail!(message);
        }
    };
    for warning in &warnings {
        eprintln!(
            "warning: cannot read the Run Journal of {}: {}",
            warning.run_id, warning.error
        );
    }
    if json {
        let listing = Listing {
            v: 1,
            ok: true,
            runs: &rows,
        };
        println!("{}", serde_json::to_string(&listing)?);
    } else {
        print!("{}", render_table(&rows, active));
    }
    Ok(())
}

/// The human listing: a header and one line per Run, or a placeholder.
fn render_table(rows: &[RunRow], active: bool) -> String {
    if rows.is_empty() {
        return if active {
            "(no active runs)\n".to_string()
        } else {
            "(no runs)\n".to_string()
        };
    }
    let cells: Vec<[String; 5]> = rows
        .iter()
        .map(|row| {
            [
                row.entry.run_id.clone(),
                row.status.to_string(),
                row.entry
                    .started_at
                    .to_rfc3339_opts(SecondsFormat::Secs, true),
                row.entry.pipeline_path.display().to_string(),
                row.entry.workdir.display().to_string(),
            ]
        })
        .collect();
    let header = ["RUN ID", "STATUS", "STARTED", "PIPELINE", "WORKDIR"];
    let mut widths = header.map(str::len);
    for line in &cells {
        for (width, cell) in widths.iter_mut().zip(line) {
            *width = (*width).max(cell.len());
        }
    }
    let mut out = String::new();
    for line in std::iter::once(header.map(String::from)).chain(cells) {
        let mut text = String::new();
        for (i, cell) in line.iter().enumerate() {
            if i + 1 == line.len() {
                text.push_str(cell);
            } else {
                text.push_str(&format!("{cell:<width$}  ", width = widths[i]));
            }
        }
        out.push_str(text.trim_end());
        out.push('\n');
    }
    out
}

/// Whether a process with this PID exists (`kill(pid, 0)`; `EPERM` means it
/// exists under another user).
#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    // 0 and values beyond `pid_t` would address process groups, not a process.
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 only checks that the process exists; nothing is sent.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Without a way to probe, never claim a Run crashed.
#[cfg(not(unix))]
fn pid_alive(_pid: u32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEAD: fn(u32) -> bool = |_| false;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_790_000_000 + secs, 0).unwrap()
    }

    /// An Index at `<tmp>/runs.jsonl` with one entry per `(run_id, journal)`;
    /// a `None` journal leaves the Run folder absent.
    fn index(tmp: &Path, runs: &[(&str, Option<&str>)]) -> std::path::PathBuf {
        let path = tmp.join(attractor_journal::INDEX_FILE);
        for (run_id, journal) in runs {
            let run_dir = tmp.join(run_id);
            if let Some(journal) = journal {
                std::fs::create_dir_all(&run_dir).unwrap();
                std::fs::write(run_dir.join(attractor_journal::EVENTS_FILE), journal).unwrap();
            }
            let entry = IndexEntry::new(*run_id, at(0), "/w", "/p.dot", run_dir);
            attractor_journal::append_entry_at(&path, &entry).unwrap();
        }
        path
    }

    fn line(attempt: u32, secs: i64, ty: &str, data: &str) -> String {
        let ts = at(secs).to_rfc3339_opts(SecondsFormat::Millis, true);
        format!(
            r#"{{"v":1,"seq":1,"ts":"{ts}","run_id":"r","attempt":{attempt},"type":"{ty}","data":{data}}}"#
        ) + "\n"
    }

    fn done() -> String {
        line(
            1,
            0,
            "AttemptStarted",
            r#"{"attempt":1,"pid":7,"pas_version":"0","argv":[]}"#,
        ) + &line(
            1,
            5,
            "AttemptEnded",
            r#"{"attempt":1,"reason":"completed"}"#,
        )
    }

    fn open() -> String {
        line(
            1,
            0,
            "AttemptStarted",
            r#"{"attempt":1,"pid":7,"pas_version":"0","argv":[]}"#,
        ) + &line(1, 30, "Heartbeat", r#"{"pid":7}"#)
    }

    fn statuses(rows: &[RunRow]) -> Vec<(&str, RunStatus)> {
        rows.iter()
            .map(|r| (r.entry.run_id.as_str(), r.status))
            .collect()
    }

    #[test]
    fn absent_index_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(attractor_journal::INDEX_FILE);
        let (rows, warnings) = list_runs(Some(&path), at(0), DEAD, false).unwrap();
        assert!(rows.is_empty() && warnings.is_empty());
        let (rows, _) = list_runs(None, at(0), DEAD, true).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn empty_index_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(attractor_journal::INDEX_FILE);
        std::fs::write(&path, "").unwrap();
        assert!(list_runs(Some(&path), at(0), DEAD, false)
            .unwrap()
            .0
            .is_empty());
    }

    #[test]
    fn rows_keep_index_order_and_derive_status() {
        let tmp = tempfile::tempdir().unwrap();
        let done = done();
        let open = open();
        let path = index(
            tmp.path(),
            &[("b", Some(&done)), ("a", Some(&open)), ("c", None)],
        );
        let (rows, warnings) = list_runs(Some(&path), at(60), DEAD, false).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(
            statuses(&rows),
            [
                ("b", RunStatus::Completed),
                ("a", RunStatus::Running),
                ("c", RunStatus::Missing)
            ]
        );
        // Ten minutes on, with PID 7 gone, the open Run has crashed.
        let (rows, _) = list_runs(Some(&path), at(630), DEAD, false).unwrap();
        assert_eq!(rows[1].status, RunStatus::Crashed);
    }

    #[test]
    fn active_filters_to_running() {
        let tmp = tempfile::tempdir().unwrap();
        let done = done();
        let open = open();
        let path = index(
            tmp.path(),
            &[("b", Some(&done)), ("a", Some(&open)), ("c", None)],
        );
        let (rows, _) = list_runs(Some(&path), at(60), DEAD, true).unwrap();
        assert_eq!(statuses(&rows), [("a", RunStatus::Running)]);
        let (rows, _) = list_runs(Some(&path), at(630), DEAD, true).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn corrupt_journal_is_a_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let done = done();
        let path = index(tmp.path(), &[("x", Some("garbage\n")), ("y", Some(&done))]);
        let (rows, warnings) = list_runs(Some(&path), at(10), DEAD, false).unwrap();
        assert_eq!(
            statuses(&rows),
            [("x", RunStatus::Running), ("y", RunStatus::Completed)]
        );
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].run_id, "x");
    }

    #[test]
    fn row_json_is_index_entry_plus_status() {
        let tmp = tempfile::tempdir().unwrap();
        let done = done();
        let path = index(tmp.path(), &[("a", Some(&done))]);
        let (rows, _) = list_runs(Some(&path), at(10), DEAD, false).unwrap();
        let value = serde_json::to_value(&rows[0]).unwrap();
        let mut expected = serde_json::to_value(&rows[0].entry).unwrap();
        expected["status"] = "completed".into();
        assert_eq!(value, expected);
    }

    #[test]
    fn table_aligns_columns_and_has_placeholders() {
        assert_eq!(render_table(&[], false), "(no runs)\n");
        assert_eq!(render_table(&[], true), "(no active runs)\n");
        let row = |id: &str, status| RunRow {
            entry: IndexEntry::new(id, at(0), "/w", "/p.dot", "/r"),
            status,
        };
        let table = render_table(
            &[
                row("aaaa", RunStatus::Running),
                row("b", RunStatus::Completed),
            ],
            false,
        );
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(
            lines[0].starts_with("RUN ID  STATUS     STARTED"),
            "{table}"
        );
        assert!(lines[1].starts_with("aaaa    running    2026-"), "{table}");
        assert!(lines[2].starts_with("b       completed  2026-"), "{table}");
        assert!(lines[2].ends_with("/p.dot    /w"), "{table}");
    }

    #[cfg(unix)]
    #[test]
    fn pid_alive_probes_real_processes() {
        assert!(pid_alive(std::process::id()));
        // PID 1 always exists and, unless we are root, belongs to another
        // user: `kill` fails with EPERM, which still means alive.
        assert!(pid_alive(1));
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(!pid_alive(pid));
        assert!(!pid_alive(0));
        assert!(!pid_alive(u32::MAX));
    }
}
