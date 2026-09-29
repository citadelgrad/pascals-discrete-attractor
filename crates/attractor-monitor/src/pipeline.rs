//! Pure parts of the Pipeline check and Launch form: where a Plan's Pipeline
//! lives, reading `pas validate --json`, the Launch form and the `pas run`
//! argv. Nothing here touches the disk except [`LaunchForm::parse`]'s
//! directory check.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::plans::{OutputKind, PlanMeta};

/// Pre-filled Launch values; the defaults of `pas run` (checked against its
/// `--help` by the end-to-end test).
pub const DEFAULT_MAX_BUDGET_USD: f64 = 200.0;
pub const DEFAULT_MAX_STEPS: u64 = 200;
/// Largest DOT text the editor saves.
pub const MAX_DOT_BYTES: usize = 1024 * 1024;

/// Only `[A-Za-z0-9._-]`, not starting with a dot: safe as a file stem.
pub fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// The file the Plan's Pipeline is written to: `<repo>/pipelines/<name>.dot`.
/// Epic mode names it after the Epic (`epic_id` comes from `result.json`);
/// Pipeline only names it after the last stored file, like `pas generate`.
pub fn pipeline_path(meta: &PlanMeta, epic_id: Option<&str>) -> Result<PathBuf, String> {
    let name = match meta.kind {
        OutputKind::EpicAndPipeline => epic_id
            .ok_or_else(|| "create the Epic first: the Pipeline is built from it".to_string())?
            .to_string(),
        OutputKind::PipelineOnly => {
            let last = meta.files.last().ok_or("the Plan has no files")?;
            let stem = last.rsplit_once('.').map_or(last.as_str(), |(s, _)| s);
            // Stored names are `NN-<name>`.
            match stem.split_once('-') {
                Some((nn, rest)) if nn.bytes().all(|b| b.is_ascii_digit()) => rest,
                _ => stem,
            }
            .to_string()
        }
    };
    if !safe_name(&name) || name.contains("..") {
        return Err(format!("cannot name a Pipeline file after {name:?}"));
    }
    Ok(meta.repo.join("pipelines").join(format!("{name}.dot")))
}

#[derive(Debug, Clone, PartialEq)]
pub struct Diagnostic {
    pub severity: String,
    pub node_id: Option<String>,
    pub message: String,
    pub fix: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Check {
    Checked {
        valid: bool,
        diagnostics: Vec<Diagnostic>,
    },
    /// `pas validate` could not check the file; the message says why.
    Failed(String),
}

impl Check {
    pub fn is_valid(&self) -> bool {
        matches!(self, Check::Checked { valid: true, .. })
    }
}

fn tail(s: &str) -> &str {
    let mut from = s.len().saturating_sub(2048);
    while !s.is_char_boundary(from) {
        from += 1;
    }
    s[from..].trim()
}

/// Read `pas validate --json`. `valid:false` exits 1 with the JSON still on
/// stdout, so stdout is read whatever the exit code is.
pub fn parse_validate(code: Option<i32>, stdout: &str, stderr: &str) -> Check {
    let Ok(v) = serde_json::from_str::<Value>(stdout.trim()) else {
        return Check::Failed(format!(
            "pas validate produced no usable result (exit {code:?}): {}",
            tail(stderr)
        ));
    };
    if v["ok"] == Value::Bool(false) {
        let msg = v["error"]["message"].as_str().unwrap_or("validate failed");
        return Check::Failed(match v["error"]["code"].as_str() {
            Some(c) => format!("{msg} ({c})"),
            None => msg.to_string(),
        });
    }
    let Some(valid) = v["valid"]
        .as_bool()
        .filter(|_| v["ok"] == Value::Bool(true))
    else {
        return Check::Failed(format!(
            "pas validate produced no usable result (exit {code:?}): {}",
            tail(stderr)
        ));
    };
    let text = |d: &Value, k: &str| d[k].as_str().map(String::from);
    let diagnostics = v["diagnostics"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|d| Diagnostic {
                    severity: text(d, "severity").unwrap_or_else(|| "error".into()),
                    node_id: text(d, "node_id"),
                    message: text(d, "message").unwrap_or_default(),
                    fix: text(d, "fix"),
                })
                .collect()
        })
        .unwrap_or_default();
    Check::Checked { valid, diagnostics }
}

/// The Launch form as submitted.
#[derive(Debug, Clone, PartialEq)]
pub struct LaunchForm {
    pub workdir: PathBuf,
    pub max_budget_usd: f64,
    pub max_steps: u64,
    pub allow_shared: bool,
}

impl LaunchForm {
    /// `Err` is a message for the user.
    pub fn parse(pairs: &[(String, String)]) -> Result<Self, String> {
        let get = |k: &str| {
            pairs
                .iter()
                .rev()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.trim())
        };
        let workdir = PathBuf::from(get("workdir").unwrap_or(""));
        if !workdir.is_absolute() {
            return Err("the working directory must be an absolute path".into());
        }
        if !workdir.is_dir() {
            return Err(format!(
                "the working directory {} is not a directory",
                workdir.display()
            ));
        }
        let budget: f64 = get("max_budget_usd")
            .unwrap_or("")
            .parse()
            .map_err(|_| "the budget must be a number".to_string())?;
        if !budget.is_finite() || budget <= 0.0 {
            return Err("the budget must be greater than zero".into());
        }
        let steps: u64 = get("max_steps")
            .unwrap_or("")
            .parse()
            .map_err(|_| "max steps must be a whole number".to_string())?;
        if steps == 0 {
            return Err("max steps must be at least 1".into());
        }
        Ok(Self {
            workdir,
            max_budget_usd: budget,
            max_steps: steps,
            allow_shared: matches!(get("allow_shared"), Some("on" | "true" | "1")),
        })
    }
}

/// `pas run <dot> --run-id <id> --fresh --json ...`. `--fresh` makes the
/// checkpoint of the same Pipeline irrelevant, so `--run-id` is honoured.
pub fn launch_args(dot: &Path, run_id: &str, f: &LaunchForm) -> Vec<OsString> {
    let mut a: Vec<OsString> = vec![
        "run".into(),
        dot.into(),
        "--run-id".into(),
        run_id.into(),
        "--fresh".into(),
        "--json".into(),
        "--workdir".into(),
        f.workdir.clone().into(),
        "--max-budget-usd".into(),
        f.max_budget_usd.to_string().into(),
        "--max-steps".into(),
        f.max_steps.to_string().into(),
    ];
    if f.allow_shared {
        a.push("--allow-shared-workdir".into());
    }
    a
}

// FNV-1a 32-bit, copied from `stable_logs_dir` in the CLI's `commands/run.rs`
// (the Monitor cannot link the CLI, ADR 0002). Guarded by an end-to-end test.
fn fnv1a32(bytes: &[u8]) -> u32 {
    bytes.iter().fold(2166136261u32, |acc, &b| {
        (acc ^ u32::from(b)).wrapping_mul(16777619)
    })
}

/// The logs folder `pas run <dot>` picks when run in `repo` without `--logs`:
/// `<repo>/.pas/logs/<stem>-<hash of the canonical path>`.
pub fn logs_dir(repo: &Path, dot: &Path) -> PathBuf {
    let stem = dot.file_stem().unwrap_or_default().to_string_lossy();
    let canonical = std::fs::canonicalize(dot).unwrap_or_else(|_| dot.to_path_buf());
    let hash = fnv1a32(canonical.to_string_lossy().as_bytes());
    repo.join(format!(".pas/logs/{stem}-{hash:08x}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plans::Mode;

    fn meta(kind: OutputKind, files: &[&str]) -> PlanMeta {
        PlanMeta {
            v: 1,
            id: "0".repeat(32),
            files: files.iter().map(|s| s.to_string()).collect(),
            repo: "/r".into(),
            kind,
            mode: Mode::Reviewed,
        }
    }

    fn pairs(p: &[(&str, &str)]) -> Vec<(String, String)> {
        p.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn epic_mode_names_the_file_after_the_epic() {
        let m = meta(OutputKind::EpicAndPipeline, &["01-a.md"]);
        assert_eq!(
            pipeline_path(&m, Some("attractor-ino")).unwrap(),
            PathBuf::from("/r/pipelines/attractor-ino.dot")
        );
        assert!(pipeline_path(&m, None).unwrap_err().contains("Epic"));
        for bad in ["", "..", "a/b", "../x", ".hidden", "a b", "a..b/c"] {
            assert!(pipeline_path(&m, Some(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn pipeline_only_uses_the_last_file_without_its_number() {
        let m = meta(OutputKind::PipelineOnly, &["01-a.md", "02-my_plan.v2.txt"]);
        assert_eq!(
            pipeline_path(&m, None).unwrap(),
            PathBuf::from("/r/pipelines/my_plan.v2.dot")
        );
        let m = meta(OutputKind::PipelineOnly, &["plain.md"]);
        assert_eq!(
            pipeline_path(&m, None).unwrap(),
            PathBuf::from("/r/pipelines/plain.dot")
        );
        assert!(pipeline_path(&meta(OutputKind::PipelineOnly, &[]), None).is_err());
    }

    #[test]
    fn validate_output_is_read_whatever_the_exit_code() {
        let ok = r#"{"v":1,"ok":true,"valid":true,"diagnostics":[]}"#;
        assert_eq!(
            parse_validate(Some(0), ok, ""),
            Check::Checked {
                valid: true,
                diagnostics: vec![]
            }
        );
        let bad = r#"{"v":1,"ok":true,"valid":false,"diagnostics":[
            {"severity":"error","node_id":"n1","message":"no exit","fix":"add exit"},
            {"severity":"warning","message":"odd"}]}"#;
        let c = parse_validate(Some(1), bad, "");
        assert!(!c.is_valid());
        let Check::Checked { diagnostics, .. } = c else {
            panic!()
        };
        assert_eq!(diagnostics.len(), 2);
        assert_eq!(diagnostics[0].node_id.as_deref(), Some("n1"));
        assert_eq!(diagnostics[0].fix.as_deref(), Some("add exit"));
        assert_eq!(diagnostics[1].node_id, None);
        assert_eq!(diagnostics[1].severity, "warning");
    }

    #[test]
    fn validate_failures_are_messages_never_valid() {
        let e = r#"{"v":1,"ok":false,"error":{"code":"invalid_dot","message":"parse error"}}"#;
        match parse_validate(Some(1), e, "") {
            Check::Failed(m) => assert_eq!(m, "parse error (invalid_dot)"),
            c => panic!("{c:?}"),
        }
        for out in [
            "",
            "not json",
            r#"{"ok":true}"#,
            r#"{"ok":true,"valid":"yes"}"#,
        ] {
            let c = parse_validate(Some(3), out, "boom");
            assert!(!c.is_valid());
            assert!(
                matches!(&c, Check::Failed(m) if m.contains("boom")),
                "{c:?}"
            );
        }
        // `valid:true` without `ok:true` is not trusted.
        assert!(!parse_validate(Some(0), r#"{"valid":true}"#, "").is_valid());
    }

    #[test]
    fn launch_form_accepts_good_values_and_the_checkbox() {
        let tmp = tempfile::tempdir().unwrap();
        let w = tmp.path().to_str().unwrap();
        let f = LaunchForm::parse(&pairs(&[
            ("workdir", w),
            ("max_budget_usd", "12.5"),
            ("max_steps", "40"),
        ]))
        .unwrap();
        assert_eq!(f.max_budget_usd, 12.5);
        assert_eq!(f.max_steps, 40);
        assert!(!f.allow_shared);
        let f = LaunchForm::parse(&pairs(&[
            ("workdir", w),
            ("max_budget_usd", "1"),
            ("max_steps", "1"),
            ("allow_shared", "on"),
        ]))
        .unwrap();
        assert!(f.allow_shared);
    }

    #[test]
    fn launch_form_rejects_bad_values() {
        let tmp = tempfile::tempdir().unwrap();
        let w = tmp.path().to_str().unwrap();
        let file = tmp.path().join("f");
        std::fs::write(&file, "x").unwrap();
        let f = file.to_str().unwrap();
        let cases: &[(&str, &str, &str)] = &[
            ("rel/dir", "1", "1"),
            ("", "1", "1"),
            ("/no/such/dir/xyz", "1", "1"),
            (f, "1", "1"),
            (w, "NaN", "1"),
            (w, "inf", "1"),
            (w, "0", "1"),
            (w, "-1", "1"),
            (w, "abc", "1"),
            (w, "", "1"),
            (w, "1", "0"),
            (w, "1", "1.5"),
            (w, "1", "-3"),
            (w, "1", ""),
        ];
        for (wd, b, s) in cases {
            let r = LaunchForm::parse(&pairs(&[
                ("workdir", wd),
                ("max_budget_usd", b),
                ("max_steps", s),
            ]));
            assert!(r.is_err(), "{wd:?} {b:?} {s:?}");
        }
    }

    #[test]
    fn launch_args_are_exact() {
        let mut f = LaunchForm {
            workdir: "/w".into(),
            max_budget_usd: 12.5,
            max_steps: 40,
            allow_shared: false,
        };
        let s = |v: Vec<OsString>| -> Vec<String> {
            v.into_iter().map(|o| o.into_string().unwrap()).collect()
        };
        assert_eq!(
            s(launch_args(Path::new("/r/p.dot"), "RID", &f)),
            [
                "run",
                "/r/p.dot",
                "--run-id",
                "RID",
                "--fresh",
                "--json",
                "--workdir",
                "/w",
                "--max-budget-usd",
                "12.5",
                "--max-steps",
                "40"
            ]
        );
        f.allow_shared = true;
        assert_eq!(
            s(launch_args(Path::new("/r/p.dot"), "RID", &f))
                .last()
                .unwrap(),
            "--allow-shared-workdir"
        );
    }

    #[test]
    fn logs_dir_uses_stem_and_fnv_of_the_path() {
        // FNV-1a 32 of "" and "a" (published test vectors).
        assert_eq!(fnv1a32(b""), 0x811c9dc5);
        assert_eq!(fnv1a32(b"a"), 0xe40c292c);
        let d = logs_dir(Path::new("/r"), Path::new("/nonexistent/p.dot"));
        let want = format!("/r/.pas/logs/p-{:08x}", fnv1a32(b"/nonexistent/p.dot"));
        assert_eq!(d, PathBuf::from(want));
    }
}
