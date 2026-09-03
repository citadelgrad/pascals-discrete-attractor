//! Cancellation-safe process-group cleanup for subprocess-backed handlers.

pub(super) fn configure(command: &mut tokio::process::Command) {
    #[cfg(unix)]
    command.process_group(0);

    #[cfg(not(unix))]
    let _ = command;
}

pub(super) struct ProcessGroupGuard {
    #[cfg(unix)]
    process_group: Option<libc::pid_t>,
}

impl ProcessGroupGuard {
    pub(super) fn new(pid: Option<u32>) -> Self {
        #[cfg(unix)]
        {
            Self {
                process_group: pid.and_then(|pid| libc::pid_t::try_from(pid).ok()),
            }
        }

        #[cfg(not(unix))]
        {
            let _ = pid;
            Self {}
        }
    }

    pub(super) fn disarm(&mut self) {
        #[cfg(unix)]
        {
            self.process_group = None;
        }
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(process_group) = self.process_group {
            // SAFETY: the child was placed in a new process group whose ID is
            // its PID. SIGKILL is best-effort; the child may already have exited.
            unsafe {
                libc::killpg(process_group, libc::SIGKILL);
            }
        }
    }
}
