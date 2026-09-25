//! Pipeline lock and Worktree lock (spec C5): an exclusive non-blocking
//! `flock` on a file whose contents name the holder, `{"pid":…,"run_id":…}`.
//! The kernel releases the lock when the holder dies, so there is no
//! stale-lock cleanup.

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Worktree lock file name, inside `git rev-parse --absolute-git-dir`.
pub(crate) const WORKTREE_LOCK: &str = "pas-run.lock";

/// Contents of a lock file. A field is `None` when the holder has not written
/// it yet or the file cannot be read.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct LockHolder {
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub run_id: Option<String>,
}

impl std::fmt::Display for LockHolder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.pid {
            Some(pid) => write!(f, "pid {pid}")?,
            None => write!(f, "pid unknown")?,
        }
        match &self.run_id {
            Some(run_id) => write!(f, ", run {run_id}"),
            None => write!(f, ", run unknown"),
        }
    }
}

#[derive(Debug)]
pub(crate) enum LockError {
    /// Another open file description holds the lock.
    Busy(LockHolder),
    Io(std::io::Error),
}

/// An exclusive `flock`, held for as long as this value lives.
#[derive(Debug)]
pub(crate) struct RunLock {
    file: File,
    path: PathBuf,
}

impl RunLock {
    /// Take the lock at `path` without waiting. The file is created if
    /// missing and never truncated here, so a failed attempt cannot wipe the
    /// holder's contents.
    pub(crate) fn try_acquire(path: &Path) -> Result<Self, LockError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(LockError::Io)?;
        match file.try_lock() {
            Ok(()) => Ok(Self {
                file,
                path: path.to_path_buf(),
            }),
            Err(std::fs::TryLockError::WouldBlock) => Err(LockError::Busy(read_holder(path))),
            Err(std::fs::TryLockError::Error(e)) => Err(LockError::Io(e)),
        }
    }

    /// Replace the contents with this process's PID and `run_id`.
    /// Overwritten before truncating, so a reader never sees an empty file
    /// after the first record.
    pub(crate) fn record(&self, run_id: Option<&str>) -> std::io::Result<()> {
        let holder = LockHolder {
            pid: Some(std::process::id()),
            run_id: run_id.map(str::to_string),
        };
        let mut bytes = serde_json::to_vec(&holder).map_err(std::io::Error::other)?;
        bytes.push(b'\n');
        let mut file = &self.file;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&bytes)?;
        file.set_len(bytes.len() as u64)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for RunLock {
    /// Unlock explicitly: closing our descriptor alone does not release the
    /// `flock` while a child forked by another thread still holds a copy of
    /// it (until that child's `exec` closes it).
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// The holder named in a lock file. Missing, empty, or partly written files
/// give an all-`None` holder.
pub(crate) fn read_holder(path: &Path) -> LockHolder {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0192a3b2-7c4d-7e5f-8a6b-1c2d3e4f5a6b";

    fn busy(path: &Path) -> LockHolder {
        match RunLock::try_acquire(path) {
            Err(LockError::Busy(holder)) => holder,
            other => panic!("expected Busy, got {other:?}"),
        }
    }

    #[test]
    fn second_acquire_is_busy_and_reports_holder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.lock");
        let lock = RunLock::try_acquire(&path).unwrap();
        lock.record(Some(ID)).unwrap();

        let holder = busy(&path);
        assert_eq!(
            holder,
            LockHolder {
                pid: Some(std::process::id()),
                run_id: Some(ID.to_string()),
            }
        );
        assert_eq!(
            holder.to_string(),
            format!("pid {}, run {ID}", std::process::id())
        );
    }

    #[test]
    fn busy_before_record_reports_unknown_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.lock");
        let lock = RunLock::try_acquire(&path).unwrap();
        lock.record(None).unwrap();

        let holder = busy(&path);
        assert_eq!(holder.pid, Some(std::process::id()));
        assert_eq!(holder.run_id, None);
        assert_eq!(
            holder.to_string(),
            format!("pid {}, run unknown", std::process::id())
        );
    }

    #[test]
    fn busy_on_empty_file_reports_unknown_holder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.lock");
        let _lock = RunLock::try_acquire(&path).unwrap();
        let holder = busy(&path);
        assert_eq!(holder, LockHolder::default());
        assert_eq!(holder.to_string(), "pid unknown, run unknown");
    }

    #[test]
    fn acquire_does_not_truncate_holders_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.lock");
        let lock = RunLock::try_acquire(&path).unwrap();
        lock.record(Some(ID)).unwrap();
        let before = std::fs::read(&path).unwrap();

        busy(&path);
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn drop_releases_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.lock");
        let lock = RunLock::try_acquire(&path).unwrap();
        lock.record(Some(ID)).unwrap();
        drop(lock);

        let again = RunLock::try_acquire(&path).expect("lock released on drop");
        assert_eq!(again.path(), path);
        // The file and its old contents stay; only the flock is the truth.
        assert!(path.exists());
    }

    #[test]
    fn drop_releases_lock_while_a_duplicate_descriptor_is_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.lock");
        let lock = RunLock::try_acquire(&path).unwrap();
        // Stands in for a child forked by another thread and not yet exec'd:
        // it shares the open file description that holds the flock.
        let _inherited = lock.file.try_clone().unwrap();
        drop(lock);

        RunLock::try_acquire(&path).expect("lock released on drop");
    }

    #[test]
    fn record_overwrites_longer_old_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.lock");
        std::fs::write(&path, "x".repeat(500)).unwrap();
        let lock = RunLock::try_acquire(&path).unwrap();
        lock.record(Some(ID)).unwrap();
        assert_eq!(
            read_holder(&path),
            LockHolder {
                pid: Some(std::process::id()),
                run_id: Some(ID.to_string()),
            }
        );

        lock.record(None).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            format!("{{\"pid\":{},\"run_id\":null}}\n", std::process::id())
        );
    }

    #[test]
    fn read_holder_tolerates_missing_empty_and_torn() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.lock");
        assert_eq!(read_holder(&path), LockHolder::default());
        std::fs::write(&path, "").unwrap();
        assert_eq!(read_holder(&path), LockHolder::default());
        std::fs::write(&path, "{\"pid\":12").unwrap();
        assert_eq!(read_holder(&path), LockHolder::default());
        std::fs::write(&path, "{\"pid\":12}").unwrap();
        assert_eq!(
            read_holder(&path),
            LockHolder {
                pid: Some(12),
                run_id: None,
            }
        );
    }

    #[test]
    fn acquire_in_missing_directory_is_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no-such-dir").join("run.lock");
        assert!(matches!(RunLock::try_acquire(&path), Err(LockError::Io(_))));
    }
}
