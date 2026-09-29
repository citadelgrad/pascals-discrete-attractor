//! HTML views of the Monitor.

mod dot_scan;
pub mod run;
pub mod runs;
pub mod transcript;

/// `pid_alive` for the Findings environment. EPERM means the process exists.
#[cfg(unix)]
pub(crate) fn pid_alive(pid: u32) -> bool {
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
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Without a way to probe, never claim a Run crashed.
#[cfg(not(unix))]
pub(crate) fn pid_alive(_pid: u32) -> bool {
    true
}
