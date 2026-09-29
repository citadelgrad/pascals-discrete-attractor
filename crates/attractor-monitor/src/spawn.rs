//! Runs `pas` subcommands as child processes of the Monitor. Only the
//! Monitor's own executable is ever started, never through a shell.
//!
//! Callers of [`spawn_run_detached`] pass `--run-id <id>` and an absolute
//! `--logs <dir>` to `pas run`, and `console_log` is that Run folder's
//! `console.log` (C1, C2), so the Monitor knows the path before the child exists.

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// Captured result of a short `pas` command.
#[derive(Debug, Clone)]
pub struct PasOutput {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// The executable the Monitor spawns: its own binary.
pub fn pas_exe() -> io::Result<PathBuf> {
    std::env::current_exe()
}

/// Run `pas <args>` to completion, capturing output.
pub async fn run_pas(args: &[OsString]) -> io::Result<PasOutput> {
    run_at(&pas_exe()?, args).await
}

/// Like [`run_pas`] with an explicit executable (tests only).
#[doc(hidden)]
pub async fn run_at(exe: &Path, args: &[OsString]) -> io::Result<PasOutput> {
    let out = tokio::process::Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await?;
    Ok(PasOutput {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// Spawn `pas <args>` detached in its own session so it outlives the Monitor.
/// stdout and stderr are appended to `console_log`. Returns the child's pid.
pub fn spawn_run_detached(args: &[OsString], console_log: &Path) -> io::Result<u32> {
    spawn_detached_at(&pas_exe()?, args, console_log)
}

/// Like [`spawn_run_detached`] with an explicit executable (tests only).
#[doc(hidden)]
pub fn spawn_detached_at(exe: &Path, args: &[OsString], console_log: &Path) -> io::Result<u32> {
    spawn_detached_watch(exe, args, console_log, None).map(|(pid, _)| pid)
}

/// Spawn detached like [`spawn_detached_at`], in `cwd` when given. The
/// receiver yields the child's exit code (`None` if killed by a signal) once
/// it ends, so a caller can tell a Run that refused to start (lock held) from
/// one that is running.
#[doc(hidden)]
pub fn spawn_detached_watch(
    exe: &Path,
    args: &[OsString],
    console_log: &Path,
    cwd: Option<&Path>,
) -> io::Result<(u32, tokio::sync::oneshot::Receiver<Option<i32>>)> {
    if let Some(dir) = console_log.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(console_log)?;
    let err = log.try_clone()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err));
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe and touches no Rust state.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = cmd.spawn()?;
    let pid = child.id();
    let (tx, rx) = tokio::sync::oneshot::channel();
    // Reap the child so it leaves no zombie while the Monitor lives.
    std::thread::spawn(move || {
        let code = child.wait().ok().and_then(|s| s.code());
        let _ = tx.send(code);
    });
    Ok((pid, rx))
}
