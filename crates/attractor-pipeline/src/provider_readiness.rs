//! Readiness check for pi, run once during Run setup before any node starts.
//!
//! The check runs `pi --version` and one `pi auth check --model <model> --json`
//! per distinct model, with the environment a pi node gets. It never prints raw
//! pi output: errors carry only parsed values (the version numbers, the model
//! from the Pipeline and a sanitized reason code).

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use crate::execution_plan::{ExecutionPlan, LlmProvider};

/// Variables PAS sets on every pi process, nodes and readiness calls alike, so
/// the check cannot report ready for credentials that a node does not see.
pub(crate) const PI_NODE_ENV: [(&str, &str); 1] = [("PI_TELEMETRY", "0")];

const MINIMUM_MAJOR: u64 = 1;
const MINIMUM_VERSION: &str = "1.0";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Where to find pi and how long each call may take.
#[derive(Debug, Clone)]
pub struct ReadinessOptions {
    pub program: PathBuf,
    pub timeout: Duration,
}

impl Default for ReadinessOptions {
    fn default() -> Self {
        Self {
            program: PathBuf::from(LlmProvider::Pi.binary_name()),
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

/// Why pi cannot run the Pipeline's nodes. `Display` names the next step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadinessError {
    NotInstalled,
    VersionTooOld { found: String },
    VersionUnreadable,
    NotReady { model: String, reason: String },
    AuthCheckFailed { model: String },
    TimedOut { call: String },
}

impl std::fmt::Display for ReadinessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotInstalled => write!(
                f,
                "pi is not installed or not on PATH; install pi {MINIMUM_VERSION} or later \
                 before running a Pipeline with pi nodes"
            ),
            Self::VersionTooOld { found } => write!(
                f,
                "pi {found} is too old; pi nodes need pi {MINIMUM_VERSION} or later. \
                 Upgrade pi and run again"
            ),
            Self::VersionUnreadable => write!(
                f,
                "could not read the pi version from `pi --version`; pi nodes need pi \
                 {MINIMUM_VERSION} or later. Check the pi installation"
            ),
            Self::NotReady { model, reason } => write!(
                f,
                "pi is not ready for model {model} (reason: {reason}); log in to the model's \
                 provider with pi or set that provider's API key, then run again"
            ),
            Self::AuthCheckFailed { model } => write!(
                f,
                "pi printed no readable auth status for model {model}; check the pi \
                 installation and run `pi auth check --model {model} --json`"
            ),
            Self::TimedOut { call } => write!(
                f,
                "`{call}` did not finish in time; check that pi runs and does not wait for input"
            ),
        }
    }
}

impl std::error::Error for ReadinessError {}

/// The distinct pi models of the plan, without their `:thinking` suffix.
/// Validation guarantees each pi node has `provider/model-id[:thinking]`, so
/// the only `:` that can remain is the thinking suffix.
pub fn pi_models(plan: &ExecutionPlan) -> BTreeSet<String> {
    plan.all_nodes()
        .filter(|node| node.provider == Some(LlmProvider::Pi))
        .filter_map(|node| plan.graph().node(&node.node_id)?.llm_model.as_deref())
        .map(|model| model.rsplit_once(':').map_or(model, |(base, _)| base))
        .map(str::to_string)
        .collect()
}

/// Check that pi is installed, 1.0 or later, and logged in for each model.
/// An empty model set makes no call.
pub async fn check_pi_readiness(
    models: &BTreeSet<String>,
    options: &ReadinessOptions,
) -> Result<(), ReadinessError> {
    if models.is_empty() {
        return Ok(());
    }

    let (status, stdout) = run_pi(options, &["--version"]).await?;
    let version = status
        .then(|| parse_version(&stdout))
        .flatten()
        .ok_or(ReadinessError::VersionUnreadable)?;
    if version.0 < MINIMUM_MAJOR {
        return Err(ReadinessError::VersionTooOld {
            found: format!("{}.{}.{}", version.0, version.1, version.2),
        });
    }

    for model in models {
        let args = ["auth", "check", "--model", model.as_str(), "--json"];
        let (exited_ok, stdout) = run_pi(options, &args).await?;
        // Real pi exits 1 with valid JSON for `not_ready`, and a stub may exit 0
        // with it, so the JSON status decides and the exit code only vetoes.
        let value: serde_json::Value =
            serde_json::from_str(stdout.trim()).map_err(|_| ReadinessError::AuthCheckFailed {
                model: model.clone(),
            })?;
        let status = value.get("status").and_then(|s| s.as_str());
        match status {
            Some("ready") if exited_ok => {}
            Some("ready") => {
                return Err(ReadinessError::AuthCheckFailed {
                    model: model.clone(),
                })
            }
            Some(_) => {
                return Err(ReadinessError::NotReady {
                    model: model.clone(),
                    reason: sanitized_reason(&value),
                })
            }
            None => {
                return Err(ReadinessError::AuthCheckFailed {
                    model: model.clone(),
                })
            }
        }
    }
    Ok(())
}

/// Run pi with `args`; returns whether it exited 0 and its stdout. Stderr is
/// read and dropped.
async fn run_pi(
    options: &ReadinessOptions,
    args: &[&str],
) -> Result<(bool, String), ReadinessError> {
    let mut cmd = tokio::process::Command::new(&options.program);
    cmd.args(args)
        .envs(PI_NODE_ENV)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let call = || format!("pi {}", args.first().copied().unwrap_or_default());
    match tokio::time::timeout(options.timeout, cmd.output()).await {
        Ok(Ok(output)) => Ok((
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
        )),
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(ReadinessError::NotInstalled)
        }
        Ok(Err(_)) if args.first() == Some(&"--version") => Err(ReadinessError::VersionUnreadable),
        Ok(Err(_)) => Err(ReadinessError::AuthCheckFailed {
            model: args.get(3).copied().unwrap_or_default().to_string(),
        }),
        Err(_elapsed) => Err(ReadinessError::TimedOut { call: call() }),
    }
}

fn parse_version(stdout: &str) -> Option<(u64, u64, u64)> {
    let re = regex::Regex::new(r"(\d+)\.(\d+)\.(\d+)").expect("static regex");
    let caps = re.captures(stdout)?;
    let part = |i: usize| caps[i].parse::<u64>().ok();
    Some((part(1)?, part(2)?, part(3)?))
}

/// The `reason` code if it looks like a code; anything else could carry text
/// from pi that must not be echoed.
fn sanitized_reason(value: &serde_json::Value) -> String {
    let re = regex::Regex::new(r"^[A-Za-z0-9_.-]{1,64}$").expect("static regex");
    value
        .get("reason")
        .and_then(|r| r.as_str())
        .filter(|r| re.is_match(r) && !r.contains("sk-"))
        .unwrap_or("unspecified")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    /// A stub `pi` that appends `argv` and the two env values it sees to
    /// `calls.log`, then answers from `body` (a shell fragment that gets `$1`).
    struct Stub {
        dir: tempfile::TempDir,
    }

    impl Stub {
        fn new(body: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let script = format!(
                "#!/bin/sh\necho \"$* | telemetry=$PI_TELEMETRY marker=$READINESS_MARKER\" >> \"{}/calls.log\"\n{body}\n",
                dir.path().display()
            );
            let path = dir.path().join("pi");
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            Self { dir }
        }

        fn options(&self, timeout: Duration) -> ReadinessOptions {
            ReadinessOptions {
                program: self.dir.path().join("pi"),
                timeout,
            }
        }

        fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(self.dir.path().join("calls.log"))
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }
    }

    fn models(list: &[&str]) -> BTreeSet<String> {
        list.iter().map(|m| m.to_string()).collect()
    }

    const READY: &str = r#"if [ "$1" = "--version" ]; then echo 1.0.4; else echo '{"status":"ready","provider":"openai","authType":"oauth"}'; fi"#;

    fn assert_no_forbidden_flags(stub: &Stub) {
        for call in stub.calls() {
            assert!(!call.contains("--credentials"), "{call}");
            assert!(!call.contains("--no-refresh"), "{call}");
        }
    }

    #[tokio::test]
    async fn ready_stub_passes_with_version_then_auth_check() {
        let stub = Stub::new(READY);
        let result = check_pi_readiness(
            &models(&["openai/gpt-5.5"]),
            &stub.options(Duration::from_secs(10)),
        )
        .await;
        assert_eq!(result, Ok(()));
        let calls = stub.calls();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(calls[0].starts_with("--version"), "{calls:?}");
        assert!(
            calls[1].starts_with("auth check --model openai/gpt-5.5 --json"),
            "{calls:?}"
        );
        assert_no_forbidden_flags(&stub);
    }

    #[tokio::test]
    async fn old_version_names_found_and_minimum_and_skips_auth() {
        let stub = Stub::new("echo 0.51.2");
        let error = check_pi_readiness(
            &models(&["openai/gpt-5.5"]),
            &stub.options(Duration::from_secs(10)),
        )
        .await
        .unwrap_err();
        let text = error.to_string();
        assert!(text.contains("0.51.2"), "{text}");
        assert!(text.contains("1.0"), "{text}");
        assert_eq!(stub.calls().len(), 1);
    }

    #[tokio::test]
    async fn banner_around_the_version_is_accepted() {
        let stub = Stub::new(
            r#"if [ "$1" = "--version" ]; then echo "pi coding agent v1.2.0 (build x)"; else echo '{"status":"ready"}'; fi"#,
        );
        let result = check_pi_readiness(
            &models(&["openai/gpt-5.5"]),
            &stub.options(Duration::from_secs(10)),
        )
        .await;
        assert_eq!(result, Ok(()));
    }

    #[tokio::test]
    async fn unreadable_version_fails_before_auth() {
        for body in ["echo 'pi: command ok'", "true", "echo 1.0.4; exit 3"] {
            let stub = Stub::new(body);
            let error = check_pi_readiness(
                &models(&["openai/gpt-5.5"]),
                &stub.options(Duration::from_secs(10)),
            )
            .await
            .unwrap_err();
            assert_eq!(error, ReadinessError::VersionUnreadable, "{body}");
            assert_eq!(stub.calls().len(), 1, "{body}");
        }
    }

    #[tokio::test]
    async fn not_ready_names_model_and_reason_for_exit_0_and_exit_1() {
        for exit in [0, 1] {
            let stub = Stub::new(&format!(
                r#"if [ "$1" = "--version" ]; then echo 1.0.4; else echo '{{"status":"not_ready","provider":"openai","reason":"credentials_not_configured"}}'; exit {exit}; fi"#
            ));
            let error = check_pi_readiness(
                &models(&["openai/gpt-5.5"]),
                &stub.options(Duration::from_secs(10)),
            )
            .await
            .unwrap_err();
            let text = error.to_string();
            assert!(text.contains("openai/gpt-5.5"), "{text}");
            assert!(text.contains("credentials_not_configured"), "{text}");
        }
    }

    #[tokio::test]
    async fn ready_status_with_failing_exit_is_not_ready() {
        let stub = Stub::new(
            r#"if [ "$1" = "--version" ]; then echo 1.0.4; else echo '{"status":"ready"}'; exit 1; fi"#,
        );
        let error = check_pi_readiness(
            &models(&["openai/gpt-5.5"]),
            &stub.options(Duration::from_secs(10)),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ReadinessError::AuthCheckFailed { .. }));
    }

    #[tokio::test]
    async fn token_in_json_and_stderr_never_reaches_the_error() {
        let bodies = [
            // not_ready with a token in the reason and an extra field, plus stderr
            r#"if [ "$1" = "--version" ]; then echo 1.0.4; else echo '{"status":"not_ready","reason":"sk-test-ABC123","apiKey":"sk-test-ABC123"}'; echo sk-test-ABC123 >&2; exit 1; fi"#,
            // not JSON at all
            r#"if [ "$1" = "--version" ]; then echo 1.0.4; else echo 'token sk-test-ABC123'; echo sk-test-ABC123 >&2; exit 1; fi"#,
            // token as status value
            r#"if [ "$1" = "--version" ]; then echo 1.0.4; else echo '{"status":"sk-test-ABC123"}'; echo sk-test-ABC123 >&2; fi"#,
            // token in the version output
            r#"echo 'pi sk-test-ABC123'; echo sk-test-ABC123 >&2"#,
        ];
        for body in bodies {
            let stub = Stub::new(body);
            let error = check_pi_readiness(
                &models(&["openai/gpt-5.5"]),
                &stub.options(Duration::from_secs(10)),
            )
            .await
            .unwrap_err();
            let text = format!("{error} / {error:?}");
            assert!(!text.contains("sk-test-"), "{body}: {text}");
        }
    }

    #[tokio::test]
    async fn one_auth_check_per_distinct_model_without_thinking_suffix() {
        let stub = Stub::new(READY);
        let set = models(&[
            "openai/gpt-5.5",
            "openai/gpt-5.5:high",
            "anthropic/claude-sonnet-4-6",
        ]);
        // The set passed in is already stripped by `pi_models`; strip the same
        // way here to exercise the check on the raw list.
        let stripped: BTreeSet<String> = set
            .iter()
            .map(|m| {
                m.rsplit_once(':')
                    .map_or(m.as_str(), |(b, _)| b)
                    .to_string()
            })
            .collect();
        check_pi_readiness(&stripped, &stub.options(Duration::from_secs(10)))
            .await
            .unwrap();
        let auth: Vec<String> = stub
            .calls()
            .into_iter()
            .filter(|c| c.starts_with("auth check"))
            .collect();
        assert_eq!(auth.len(), 2, "{auth:?}");
        assert!(auth.iter().all(|c| !c.contains(":high")), "{auth:?}");
    }

    #[test]
    fn pi_models_dedups_and_strips_thinking_for_pi_nodes_only() {
        let dot = r#"digraph G {
            start [shape="Mdiamond"]
            a [llm_provider="pi", llm_model="openai/gpt-5.5", timeout="60s"]
            b [llm_provider="pi", llm_model="openai/gpt-5.5:high", timeout="60s"]
            c [llm_provider="pi", llm_model="anthropic/claude-sonnet-4-6", timeout="60s"]
            d [llm_provider="codex", llm_model="gpt-5.5", timeout="60s"]
            done [shape="Msquare"]
            start -> a -> b -> c -> d -> done
        }"#;
        let graph =
            crate::graph::PipelineGraph::from_dot(attractor_dot::parse(dot).unwrap()).unwrap();
        let plan = ExecutionPlan::compile(graph).unwrap();
        assert_eq!(
            pi_models(&plan),
            models(&["openai/gpt-5.5", "anthropic/claude-sonnet-4-6"])
        );
    }

    #[test]
    fn pi_models_is_empty_without_pi_nodes() {
        let dot = r#"digraph G {
            start [shape="Mdiamond"]
            a [llm_provider="codex", timeout="60s"]
            done [shape="Msquare"]
            start -> a -> done
        }"#;
        let graph =
            crate::graph::PipelineGraph::from_dot(attractor_dot::parse(dot).unwrap()).unwrap();
        let plan = ExecutionPlan::compile(graph).unwrap();
        assert!(pi_models(&plan).is_empty());
    }

    #[tokio::test]
    async fn no_models_makes_no_call() {
        let stub = Stub::new(READY);
        check_pi_readiness(&BTreeSet::new(), &stub.options(Duration::from_secs(10)))
            .await
            .unwrap();
        assert!(stub.calls().is_empty());
    }

    #[tokio::test]
    async fn missing_binary_is_not_installed() {
        let options = ReadinessOptions {
            program: Path::new("/nonexistent/dir/pi").to_path_buf(),
            timeout: Duration::from_secs(10),
        };
        let error = check_pi_readiness(&models(&["openai/gpt-5.5"]), &options)
            .await
            .unwrap_err();
        assert_eq!(error, ReadinessError::NotInstalled);
    }

    #[tokio::test]
    async fn hanging_stub_times_out_quickly() {
        let stub = Stub::new("sleep 30");
        let started = std::time::Instant::now();
        let error = check_pi_readiness(
            &models(&["openai/gpt-5.5"]),
            &stub.options(Duration::from_millis(200)),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ReadinessError::TimedOut { .. }), "{error}");
        assert!(error.to_string().contains("did not finish"));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn stub_sees_the_node_environment_and_inherited_variables() {
        let stub = Stub::new(READY);
        std::env::set_var("READINESS_MARKER", "present");
        check_pi_readiness(
            &models(&["openai/gpt-5.5"]),
            &stub.options(Duration::from_secs(10)),
        )
        .await
        .unwrap();
        std::env::remove_var("READINESS_MARKER");
        for call in stub.calls() {
            assert!(call.contains("telemetry=0"), "{call}");
            assert!(call.contains("marker=present"), "{call}");
        }
        assert_eq!(PI_NODE_ENV, [("PI_TELEMETRY", "0")]);
    }
}
