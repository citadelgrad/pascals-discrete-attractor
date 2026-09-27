//! Helpers shared by the CLI integration tests.

/// A `tool_command` that blocks until a `go` file appears in the stage's
/// working directory.
///
/// The stage shell runs in its own process group. A test that SIGKILLs
/// `pas run` gives it no chance to kill that group, so the shell outlives
/// the test. It therefore also exits 1 once its working directory is
/// deleted, as the test's `TempDir` is on drop, or after `WAIT_LIMIT` polls
/// (default 2400, about two minutes), so a leaked loop cannot poll forever.
macro_rules! wait_for_go {
    () => {
        "n=${WAIT_LIMIT:-2400}; while [ ! -f go ]; do [ -d $PWD ] && [ $n -gt 0 ] || exit 1; n=$((n-1)); sleep 0.05; done"
    };
}
pub(crate) use wait_for_go;
