//! Run Commit detection (spec File Change 5).
//!
//! The engine reads `HEAD` before and after each stage attempt. When it moved,
//! the commits in `before..after` are the stage's Run Commits. PAS only
//! observes commits; it never creates them.
//!
//! Known limits (observation only, nothing fails): a stage that pulls or
//! switches branches also lists commits it did not author, and with
//! `--allow-shared-workdir` another Run's commits can be listed.

use std::path::Path;
use std::process::Stdio;

use attractor_journal::CommitRef;
use tokio::process::Command;

/// `git rev-parse HEAD` in `workdir`, or `None` outside a git repository or
/// with an unborn HEAD (both exit non-zero).
pub(crate) async fn head(workdir: &Path) -> Option<String> {
    let output = git(workdir)
        .args(["rev-parse", "--verify", "--quiet", "HEAD"])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

/// Commits in `before..after`, newest first (git's order).
pub(crate) async fn commits_between(
    workdir: &Path,
    before: &str,
    after: &str,
) -> std::io::Result<Vec<CommitRef>> {
    let output = git(workdir)
        .args([
            "log",
            "--no-show-signature",
            "--no-color",
            "--format=%H%x00%s%x00%an%x00%cI",
            &format!("{before}..{after}"),
            "--",
        ])
        .output()
        .await?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "git log {before}..{after} exited with {}",
            output.status
        )));
    }
    Ok(parse_log(&String::from_utf8_lossy(&output.stdout)))
}

fn git(workdir: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(workdir)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command
}

/// Parse `%H%x00%s%x00%an%x00%cI` lines. Lines without 4 fields are skipped.
fn parse_log(stdout: &str) -> Vec<CommitRef> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\0');
            let (Some(sha), Some(subject), Some(author), Some(ts), None) = (
                fields.next(),
                fields.next(),
                fields.next(),
                fields.next(),
                fields.next(),
            ) else {
                return None;
            };
            (!sha.is_empty()).then(|| CommitRef {
                sha: sha.to_string(),
                subject: subject.to_string(),
                author: author.to_string(),
                ts: ts.to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_log_keeps_git_order() {
        let commits = parse_log(
            "bbb\0two\0Ann\x002026-09-25T10:00:01+00:00\n\
             aaa\0one\0Ann\x002026-09-25T10:00:00+00:00\n",
        );
        let shas: Vec<_> = commits.iter().map(|commit| commit.sha.as_str()).collect();
        assert_eq!(shas, ["bbb", "aaa"]);
        assert_eq!(commits[0].subject, "two");
        assert_eq!(commits[0].author, "Ann");
        assert_eq!(commits[0].ts, "2026-09-25T10:00:01+00:00");
    }

    #[test]
    fn parse_log_keeps_subject_text_intact() {
        let commits = parse_log("abc\0fix: a\tb | ünï “q”\0Bo Li\x002026-01-01T00:00:00Z\n");
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].subject, "fix: a\tb | ünï “q”");
        assert_eq!(commits[0].author, "Bo Li");
    }

    #[test]
    fn parse_log_skips_malformed_lines() {
        assert!(parse_log("").is_empty());
        assert!(parse_log("\n").is_empty());
        assert!(parse_log("gpg: Signature made ...\n").is_empty());
        assert!(parse_log("a\0b\0c\n").is_empty());
        assert!(parse_log("a\0b\0c\0d\0e\n").is_empty());
        assert!(parse_log("\0b\0c\0d\n").is_empty());
        assert_eq!(parse_log("junk\nabc\0s\0a\0t\n").len(), 1);
    }

    #[tokio::test]
    async fn head_is_none_outside_a_repository() {
        let dir = tempfile::tempdir().unwrap();
        let inside = std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["rev-parse", "--git-dir"])
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(
            !inside,
            "temp dir {} is inside a repository",
            dir.path().display()
        );
        assert_eq!(head(dir.path()).await, None);
    }

    #[tokio::test]
    async fn head_is_none_for_a_missing_workdir() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(head(&dir.path().join("missing")).await, None);
    }

    #[tokio::test]
    async fn commits_between_unknown_revisions_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(commits_between(dir.path(), "deadbeef", "cafebabe")
            .await
            .is_err());
    }
}
