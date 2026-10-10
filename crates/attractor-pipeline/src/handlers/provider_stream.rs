//! Stream a provider process's stdout into a Transcript as it is produced.
//!
//! Each raw stdout line (bytes up to and including `\n`, or a final line with
//! no `\n`) is appended to the Transcript and flushed before the next line is
//! read, so a reader tailing the file sees it grow while the provider runs.
//! The Transcript is the provider's stdout byte for byte; stderr is collected
//! separately and never written to it.
//!
//! A Transcript that cannot be created or written is logged and dropped: a
//! Transcript problem never fails the stage.

use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Child;

/// An open Transcript file (`transcripts/<invocation-id>.jsonl`).
pub(super) struct Transcript {
    path: PathBuf,
    file: Option<tokio::fs::File>,
}

impl Transcript {
    /// Create an empty Transcript at `path`, making its folder if needed.
    /// Returns `None` (after a warning) when the file cannot be created.
    pub(super) async fn create(path: PathBuf) -> Option<Self> {
        let result = async {
            if let Some(dir) = path.parent() {
                tokio::fs::create_dir_all(dir).await?;
            }
            tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .await
        }
        .await;
        match result {
            Ok(file) => Some(Self {
                path,
                file: Some(file),
            }),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "Cannot create Transcript");
                None
            }
        }
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    /// Remove the Transcript; used when the provider never started.
    pub(super) async fn discard(mut self) {
        self.file = None;
        if let Err(error) = tokio::fs::remove_file(&self.path).await {
            tracing::warn!(path = %self.path.display(), %error, "Cannot remove Transcript");
        }
    }

    async fn append(&mut self, bytes: &[u8]) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let result = async {
            file.write_all(bytes).await?;
            file.flush().await
        }
        .await;
        if let Err(error) = result {
            tracing::warn!(
                path = %self.path.display(),
                %error,
                "Cannot write Transcript; continuing without it"
            );
            self.file = None;
        }
    }
}

/// What a stdout hook asks for after seeing one line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LineAction {
    Continue,
    /// Kill the provider's process group and return the output so far.
    Stop,
}

/// Called with each stdout line after it is written to the Transcript.
pub(super) type LineHook<'a> = &'a mut (dyn FnMut(&[u8]) -> LineAction + Send);

/// What the provider process produced.
pub(super) struct StreamedOutput {
    pub(super) status: ExitStatus,
    /// The hook asked for a stop and the process group was killed.
    pub(super) stopped: bool,
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
}

/// Wait for `child`, copying each stdout line to `transcript` as it arrives.
///
/// stdout and stderr are drained concurrently so a provider that writes a lot
/// to stderr cannot block on a full pipe. Dropping the returned future (e.g.
/// on timeout) leaves everything already flushed in the Transcript.
///
/// When `hook` returns [`LineAction::Stop`] the process group is killed before
/// waiting for the pipes, so a surviving grandchild cannot hold them open.
pub(super) async fn run_streaming(
    mut child: Child,
    mut transcript: Option<Transcript>,
    mut hook: Option<LineHook<'_>>,
) -> std::io::Result<StreamedOutput> {
    let group = child.id();
    let mut stopped = false;
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    let read_stdout = async {
        let mut stdout = Vec::new();
        if let Some(pipe) = stdout_pipe {
            let mut reader = BufReader::new(pipe);
            let mut line = Vec::new();
            loop {
                line.clear();
                if reader.read_until(b'\n', &mut line).await? == 0 {
                    break;
                }
                if let Some(transcript) = transcript.as_mut() {
                    transcript.append(&line).await;
                }
                stdout.extend_from_slice(&line);
                if let Some(hook) = hook.as_mut() {
                    if hook(&line) == LineAction::Stop {
                        stopped = true;
                        super::process_group::kill(group);
                        break;
                    }
                }
            }
        }
        Ok::<_, std::io::Error>(stdout)
    };
    let read_stderr = async {
        let mut stderr = Vec::new();
        if let Some(mut pipe) = stderr_pipe {
            pipe.read_to_end(&mut stderr).await?;
        }
        Ok::<_, std::io::Error>(stderr)
    };

    let (stdout, stderr, status) = tokio::try_join!(read_stdout, read_stderr, child.wait())?;
    Ok(StreamedOutput {
        status,
        stopped,
        stdout,
        stderr,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use std::process::Stdio;

    use super::*;

    fn sh(script: &str) -> Child {
        tokio::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0)
            .spawn()
            .unwrap()
    }

    #[tokio::test]
    async fn transcript_equals_stdout_including_unterminated_last_line() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t").join("x.jsonl");
        let transcript = Transcript::create(path.clone()).await.unwrap();
        let out = run_streaming(
            sh(r"printf 'a\r\n\303\251\tb\n\377\nlast'; printf 'err' >&2"),
            Some(transcript),
            None,
        )
        .await
        .unwrap();
        let expected = b"a\r\n\xc3\xa9\tb\n\xff\nlast".to_vec();
        assert!(out.status.success());
        assert_eq!(out.stdout, expected);
        assert_eq!(out.stderr, b"err");
        assert_eq!(std::fs::read(&path).unwrap(), expected);
    }

    #[tokio::test]
    async fn large_stderr_does_not_block() {
        let out = run_streaming(
            sh("head -c 1048576 /dev/zero >&2; echo done; exit 4"),
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(out.status.code(), Some(4));
        assert_eq!(out.stdout, b"done\n");
        assert_eq!(out.stderr.len(), 1_048_576);
    }

    #[tokio::test]
    async fn create_fails_softly_and_does_not_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let existing = tmp.path().join("x.jsonl");
        std::fs::write(&existing, "keep").unwrap();
        assert!(Transcript::create(existing.clone()).await.is_none());
        assert_eq!(std::fs::read(&existing).unwrap(), b"keep");

        let blocker = tmp.path().join("file");
        std::fs::write(&blocker, "").unwrap();
        assert!(Transcript::create(blocker.join("y.jsonl")).await.is_none());
    }

    #[tokio::test]
    async fn discard_removes_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("x.jsonl");
        let transcript = Transcript::create(path.clone()).await.unwrap();
        assert!(path.exists());
        transcript.discard().await;
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn always_continue_hook_matches_no_hook() {
        let script = r"printf 'a\nb\nlast'; printf 'err' >&2";
        let tmp = tempfile::tempdir().unwrap();
        let plain_path = tmp.path().join("plain.jsonl");
        let hooked_path = tmp.path().join("hooked.jsonl");
        let plain = run_streaming(
            sh(script),
            Transcript::create(plain_path.clone()).await,
            None,
        )
        .await
        .unwrap();
        let mut seen = 0;
        let mut hook = |_: &[u8]| {
            seen += 1;
            LineAction::Continue
        };
        let hooked = run_streaming(
            sh(script),
            Transcript::create(hooked_path.clone()).await,
            Some(&mut hook),
        )
        .await
        .unwrap();
        assert!(!plain.stopped && !hooked.stopped);
        assert_eq!(plain.stdout, hooked.stdout);
        assert_eq!(plain.stderr, hooked.stderr);
        assert_eq!(
            std::fs::read(&plain_path).unwrap(),
            std::fs::read(&hooked_path).unwrap()
        );
        assert_eq!(seen, 3, "the unterminated last line reaches the hook");
    }

    #[tokio::test]
    async fn stop_returns_output_so_far_and_kills_the_process_group() {
        let tmp = tempfile::tempdir().unwrap();
        let pidfile = tmp.path().join("pid");
        let path = tmp.path().join("x.jsonl");
        let script = format!(
            "sleep 60 & echo $! > {}; printf 'one\\nstop\\n'; wait; printf 'never\\n'",
            pidfile.display()
        );
        let mut hook = |line: &[u8]| {
            if line == b"stop\n" {
                LineAction::Stop
            } else {
                LineAction::Continue
            }
        };
        let started = std::time::Instant::now();
        let out = run_streaming(
            sh(&script),
            Transcript::create(path.clone()).await,
            Some(&mut hook),
        )
        .await
        .unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
        assert!(out.stopped);
        assert!(!out.status.success());
        assert_eq!(out.stdout, b"one\nstop\n");
        assert_eq!(std::fs::read(&path).unwrap(), b"one\nstop\n");

        let pid: libc::pid_t = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            // SAFETY: signal 0 only checks that the process exists.
            let alive = unsafe { libc::kill(pid, 0) } == 0;
            if !alive {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "grandchild survived");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn stop_on_unterminated_last_line_and_without_transcript() {
        let mut hook = |line: &[u8]| {
            if line == b"last" {
                LineAction::Stop
            } else {
                LineAction::Continue
            }
        };
        let out = run_streaming(sh("printf 'a\\nlast'"), None, Some(&mut hook))
            .await
            .unwrap();
        assert!(out.stopped);
        assert_eq!(out.stdout, b"a\nlast");
    }
}
