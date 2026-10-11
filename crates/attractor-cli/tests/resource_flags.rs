//! U13: resource flags, the pas.toml extension trust check and the Run start
//! output, with stub providers only.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn fixture_jsonl() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../attractor-pipeline/tests/fixtures/providers/pi-1.0.4.jsonl")
}

const READY: &str = r#"{"status":"ready","provider":"openai","authType":"oauth"}"#;

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        for dir in ["shims", "work", "state", "config", "res"] {
            fs::create_dir(root.path().join(dir)).unwrap();
        }
        let root = Self { root };
        root.pi_shim();
        root
    }

    fn path(&self, name: &str) -> PathBuf {
        // Canonical, so paths compare equal to what pas prints on macOS.
        fs::canonicalize(self.root.path()).unwrap().join(name)
    }

    /// pi shim: logs every call; node calls land in `pi-calls.nodes`.
    fn pi_shim(&self) {
        let calls = self.path("pi-calls");
        let path = self.path("shims/pi");
        fs::write(
            &path,
            format!(
                "#!/bin/sh\necho \"$*\" >> '{calls}'\n\
                 case \"$1\" in\n\
                 --version) echo 1.0.4 ;;\n\
                 auth) echo '{READY}' ;;\n\
                 *) echo \"$*\" >> '{calls}.nodes'; [ -f '{fail}' ] && exit 1; /bin/cat '{fixture}' ;;\n\
                 esac\n",
                calls = calls.display(),
                fixture = fixture_jsonl().display(),
                fail = self.path("fail-nodes").display(),
            ),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn node_calls(&self) -> Vec<String> {
        fs::read_to_string(self.path("pi-calls.nodes"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn dot(&self, name: &str) -> PathBuf {
        let path = self.path(name);
        fs::write(
            &path,
            r#"digraph G {
                start [shape="Mdiamond"]
                work [shape="box", prompt="work", llm_provider="pi", llm_model="openai/gpt-5.5", timeout=30s]
                done [shape="Msquare"]
                start -> work -> done
            }"#,
        )
        .unwrap();
        path
    }

    /// A skill directory with a `SKILL.md`.
    fn skill(&self, name: &str) -> PathBuf {
        let dir = self.path("res").join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), format!("---\nname: {name}\n---\n")).unwrap();
        dir
    }

    fn extension(&self, name: &str, body: &str) -> PathBuf {
        let path = self.path("res").join(name);
        fs::write(&path, body).unwrap();
        path
    }

    fn manifest(&self, dir: &str, body: &str) {
        let dir = self.path(dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("pas.toml"),
            format!("[project]\nname = \"u13\"\n\n{body}\n"),
        )
        .unwrap();
    }

    fn command(&self, args: &[&str], cwd: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pas"));
        command
            .args(args)
            .current_dir(self.path(cwd))
            .env_clear()
            .env("PATH", self.path("shims"))
            .env("HOME", self.path("state"))
            .env("PAS_STATE_DIR", self.path("state"))
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("PAS_NON_INTERACTIVE", "1");
        command
    }

    fn pas(&self, args: &[&str]) -> Output {
        self.command(args, "work").output().unwrap()
    }

    fn run_dirs(&self) -> bool {
        self.path("work/.pas").exists()
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn arg(path: &Path) -> &str {
    path.to_str().unwrap()
}

/// Everything after `flag` in `call`, split on whitespace, up to the next flag.
fn values_of(call: &str, flag: &str) -> Vec<String> {
    let words: Vec<&str> = call.split_whitespace().collect();
    words
        .iter()
        .enumerate()
        .filter(|(_, w)| **w == flag)
        .filter_map(|(i, _)| words.get(i + 1).map(|w| w.to_string()))
        .collect()
}

#[test]
fn help_lists_the_three_flags_on_run_and_launch() {
    let fx = Fixture::new();
    for command in ["run", "launch"] {
        let output = fx.pas(&[command, "--help"]);
        let all = text(&output);
        for flag in [
            "--codergen-skill",
            "--codergen-pi-extension",
            "--codergen-pi-prompt-template",
        ] {
            assert!(all.contains(flag), "{command} --help lacks {flag}: {all}");
        }
    }
}

#[test]
fn two_skill_flags_replace_the_manifest_list_and_start_output_names_source() {
    let fx = Fixture::new();
    let manifest_skill = fx.skill("from-manifest");
    let a = fx.skill("a");
    let b = fx.skill("b");
    fx.manifest(
        "work",
        &format!("[codergen]\nskills = [\"{}\"]", manifest_skill.display()),
    );
    let dot = fx.dot("p.dot");

    let output = fx.pas(&[
        "run",
        arg(&dot),
        "--codergen-skill",
        arg(&a),
        "--codergen-skill",
        arg(&b),
    ]);
    let all = text(&output);
    assert!(output.status.success(), "{all}");
    assert!(
        all.contains(&format!(
            "Skills (caller): {}, {}",
            a.display(),
            b.display()
        )),
        "{all}"
    );
    assert!(!all.contains("from-manifest"), "{all}");
    let calls = fx.node_calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    let seen = values_of(&calls[0], "--skill");
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert_copy_dir(&seen[0], "a");
    assert_copy_dir(&seen[1], "b");
}

/// `value` is `<run_dir>/agent-resources/claude-plugin/skills/<name>` and the
/// skill's `SKILL.md` is there.
fn assert_copy_dir(value: &str, name: &str) {
    assert!(value.contains("/runs/"), "{value}");
    assert!(
        value.ends_with(&format!("/agent-resources/claude-plugin/skills/{name}")),
        "{value}"
    );
    assert!(Path::new(value).join("SKILL.md").is_file(), "{value}");
}

#[test]
fn manifest_lists_are_printed_with_source_manifest() {
    let fx = Fixture::new();
    let skill = fx.skill("m");
    fx.manifest(
        "work",
        &format!("[codergen]\nskills = [\"{}\"]", skill.display()),
    );
    let dot = fx.dot("p.dot");
    let output = fx.pas(&["run", arg(&dot), "--dry-run"]);
    let all = text(&output);
    assert!(output.status.success(), "{all}");
    assert!(
        all.contains(&format!("Skills (manifest): {}", skill.display())),
        "{all}"
    );
}

#[test]
fn json_first_line_carries_resources_only_when_a_list_is_named() {
    let fx = Fixture::new();
    let skill = fx.skill("s");
    let dot = fx.dot("p.dot");

    let with = fx.pas(&[
        "run",
        arg(&dot),
        "--dry-run",
        "--json",
        "--codergen-skill",
        arg(&skill),
    ]);
    assert!(with.status.success(), "{}", text(&with));
    let stdout = String::from_utf8_lossy(&with.stdout).to_string();
    let first: serde_json::Value = serde_json::from_str(stdout.lines().next().unwrap()).unwrap();
    assert_eq!(first["ok"], true);
    assert_eq!(first["resources"]["skills"]["source"], "caller");
    assert_eq!(
        first["resources"]["skills"]["paths"],
        serde_json::json!([skill.display().to_string()])
    );
    assert!(first["resources"].get("pi_extensions").is_none());

    let without = fx.pas(&["run", arg(&dot), "--dry-run", "--json", "--fresh"]);
    let stdout = String::from_utf8_lossy(&without.stdout).to_string();
    let first: serde_json::Value = serde_json::from_str(stdout.lines().next().unwrap()).unwrap();
    assert!(first.get("resources").is_none(), "{first}");
}

#[test]
fn directory_run_passes_the_list_to_every_pipeline() {
    let fx = Fixture::new();
    let skill = fx.skill("s");
    let dir = fx.path("pipes");
    fs::create_dir(&dir).unwrap();
    let first = fx.dot("tmp1.dot");
    let second = fx.dot("tmp2.dot");
    fs::rename(first, dir.join("01.dot")).unwrap();
    fs::rename(second, dir.join("02.dot")).unwrap();

    let output = fx.pas(&["run", arg(&dir), "--codergen-skill", arg(&skill)]);
    assert!(output.status.success(), "{}", text(&output));
    let calls = fx.node_calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    for call in &calls {
        let seen = values_of(call, "--skill");
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_copy_dir(&seen[0], "s");
    }
}

#[test]
fn relative_flag_path_is_made_absolute_against_the_process_directory() {
    let fx = Fixture::new();
    let skill = fx.path("work/rel");
    fs::create_dir(&skill).unwrap();
    fs::write(skill.join("SKILL.md"), "---\nname: rel\n---\n").unwrap();
    let dot = fx.dot("p.dot");
    let output = fx
        .command(
            &["run", arg(&dot), "--dry-run", "--codergen-skill", "rel"],
            "work",
        )
        .output()
        .unwrap();
    let all = text(&output);
    assert!(output.status.success(), "{all}");
    assert!(
        all.contains(&format!("Skills (caller): {}", skill.display())),
        "{all}"
    );
}

fn assert_untrusted_error(all: &str) {
    assert!(all.contains("pas trust add "), "{all}");
    assert!(all.contains("PAS_TRUST_THIS=1"), "{all}");
}

/// The hash in `pas trust add <path> <hash>`.
fn printed_hash(all: &str) -> String {
    let start = all.find("pas trust add ").unwrap() + "pas trust add ".len();
    let rest: Vec<&str> = all[start..].split_whitespace().collect();
    rest[1]
        .trim_end_matches(|c: char| !c.is_ascii_hexdigit())
        .to_string()
}

#[test]
fn untrusted_manifest_extension_stops_before_any_node_and_leaves_no_trace() {
    let fx = Fixture::new();
    let ext = fx.extension("guard.ts", "export default {}");
    fx.manifest(
        "work",
        &format!("[codergen.pi]\nextensions = [\"{}\"]", ext.display()),
    );
    let dot = fx.dot("p.dot");
    let output = fx.pas(&["run", arg(&dot), "--logs", arg(&fx.path("logs"))]);
    assert!(!output.status.success());
    assert_untrusted_error(&text(&output));
    assert!(fx.node_calls().is_empty());
    assert!(!fx.run_dirs());
    assert!(!fx.path("logs").exists(), "no Run directory or lock");
    let state: Vec<_> = fs::read_dir(fx.path("state")).unwrap().collect();
    assert!(state.is_empty(), "no Run Index line: {state:?}");
}

#[test]
fn json_mode_reports_untrusted_manifest_as_a_setup_error() {
    let fx = Fixture::new();
    let ext = fx.extension("guard.ts", "export default {}");
    fx.manifest(
        "work",
        &format!("[codergen.pi]\nextensions = [\"{}\"]", ext.display()),
    );
    let dot = fx.dot("p.dot");
    let output = fx.pas(&["run", arg(&dot), "--json"]);
    assert!(!output.status.success());
    let all = text(&output);
    assert!(all.contains("manifest_untrusted"), "{all}");
    assert_untrusted_error(&all);
}

#[test]
fn flag_extension_needs_no_trust_even_when_manifest_names_another() {
    let fx = Fixture::new();
    let manifest_ext = fx.extension("manifest.ts", "export default {}");
    let flag_ext = fx.extension("flag.ts", "export default {}");
    fx.manifest(
        "work",
        &format!(
            "[codergen.pi]\nextensions = [\"{}\"]",
            manifest_ext.display()
        ),
    );
    let dot = fx.dot("p.dot");
    let output = fx.pas(&["run", arg(&dot), "--codergen-pi-extension", arg(&flag_ext)]);
    let all = text(&output);
    assert!(output.status.success(), "{all}");
    assert!(all.contains("pi extensions (caller)"), "{all}");
    let calls = fx.node_calls();
    assert_eq!(values_of(&calls[0], "-e"), [flag_ext.display().to_string()]);
}

#[test]
fn skills_and_prompt_templates_in_the_manifest_need_no_trust() {
    let fx = Fixture::new();
    let skill = fx.skill("s");
    let template = fx.extension("review.md", "review");
    fx.manifest(
        "work",
        &format!(
            "[codergen]\nskills = [\"{}\"]\n[codergen.pi]\nprompt_templates = [\"{}\"]",
            skill.display(),
            template.display()
        ),
    );
    let dot = fx.dot("p.dot");
    let output = fx.pas(&["run", arg(&dot)]);
    let all = text(&output);
    assert!(output.status.success(), "{all}");
    assert!(!all.contains("pas trust add"), "{all}");
    assert_eq!(fx.node_calls().len(), 1);
}

#[test]
fn manifest_in_a_parent_directory_of_the_workdir_triggers_the_check() {
    let fx = Fixture::new();
    let ext = fx.extension("guard.ts", "export default {}");
    fx.manifest(
        "work",
        &format!("[codergen.pi]\nextensions = [\"{}\"]", ext.display()),
    );
    let sub = fx.path("work/sub/deeper");
    fs::create_dir_all(&sub).unwrap();
    let dot = fx.dot("p.dot");
    let output = fx.pas(&["run", arg(&dot), "--workdir", arg(&sub)]);
    assert!(!output.status.success());
    let all = text(&output);
    assert_untrusted_error(&all);
    assert!(
        all.contains(&fx.path("work/pas.toml").display().to_string()),
        "{all}"
    );
    assert!(fx.node_calls().is_empty());
}

#[test]
fn pas_trust_this_lets_the_untrusted_extension_run() {
    let fx = Fixture::new();
    let ext = fx.extension("guard.ts", "export default {}");
    fx.manifest(
        "work",
        &format!("[codergen.pi]\nextensions = [\"{}\"]", ext.display()),
    );
    let dot = fx.dot("p.dot");
    let output = fx
        .command(&["run", arg(&dot)], "work")
        .env("PAS_TRUST_THIS", "1")
        .output()
        .unwrap();
    let all = text(&output);
    assert!(output.status.success(), "{all}");
    let calls = fx.node_calls();
    assert_eq!(values_of(&calls[0], "-e"), [ext.display().to_string()]);
}

#[test]
fn changed_extension_fails_the_trust_check_again_with_a_new_hash() {
    let fx = Fixture::new();
    let ext = fx.extension("guard.ts", "export default { v: 1 }");
    fx.manifest(
        "work",
        &format!("[codergen.pi]\nextensions = [\"{}\"]", ext.display()),
    );
    let dot = fx.dot("p.dot");

    let first = fx.pas(&["run", arg(&dot)]);
    assert!(!first.status.success());
    let first_hash = printed_hash(&text(&first));

    let manifest = fx.path("work/pas.toml");
    let trust = fx.pas(&["trust", "add", arg(&manifest), &first_hash]);
    assert!(trust.status.success(), "{}", text(&trust));

    let trusted = fx.pas(&["run", arg(&dot)]);
    assert!(trusted.status.success(), "{}", text(&trusted));
    assert_eq!(fx.node_calls().len(), 1);

    fs::write(&ext, "export default { v: 2 }").unwrap();
    let again = fx.pas(&["run", arg(&dot), "--fresh"]);
    assert!(!again.status.success());
    let all = text(&again);
    assert_untrusted_error(&all);
    assert_ne!(printed_hash(&all), first_hash);
    assert_eq!(fx.node_calls().len(), 1, "no new node may run");
}

#[test]
fn directory_run_checks_trust_before_the_first_pipeline_starts() {
    let fx = Fixture::new();
    let ext = fx.extension("guard.ts", "export default {}");
    fx.manifest(
        "work",
        &format!("[codergen.pi]\nextensions = [\"{}\"]", ext.display()),
    );
    let dir = fx.path("pipes");
    fs::create_dir(&dir).unwrap();
    let first = fx.dot("tmp1.dot");
    let second = fx.dot("tmp2.dot");
    fs::rename(first, dir.join("01.dot")).unwrap();
    fs::rename(second, dir.join("02.dot")).unwrap();

    let output = fx.pas(&["run", arg(&dir)]);
    assert!(!output.status.success());
    assert_untrusted_error(&text(&output));
    assert!(fx.node_calls().is_empty());
    assert!(!fx.run_dirs());
}

#[test]
fn dry_run_with_an_untrusted_extension_does_not_fail() {
    let fx = Fixture::new();
    let ext = fx.extension("guard.ts", "export default {}");
    fx.manifest(
        "work",
        &format!("[codergen.pi]\nextensions = [\"{}\"]", ext.display()),
    );
    let dot = fx.dot("p.dot");
    let output = fx.pas(&["run", arg(&dot), "--dry-run"]);
    assert!(output.status.success(), "{}", text(&output));
}

fn run_dir_of(output: &Output) -> PathBuf {
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let first: serde_json::Value = serde_json::from_str(stdout.lines().next().unwrap()).unwrap();
    PathBuf::from(first["run_dir"].as_str().unwrap())
}

#[test]
fn skill_copy_has_manifest_and_a_directory_per_skill_in_the_run_dir() {
    let fx = Fixture::new();
    let a = fx.skill("a");
    let b = fx.skill("b");
    let dot = fx.dot("p.dot");
    let output = fx.pas(&[
        "run",
        arg(&dot),
        "--json",
        "--codergen-skill",
        arg(&a),
        "--codergen-skill",
        arg(&b),
    ]);
    assert!(output.status.success(), "{}", text(&output));
    let root = run_dir_of(&output).join("agent-resources/claude-plugin");
    assert!(root.join(".claude-plugin/plugin.json").is_file());
    assert!(root.join("skills/a/SKILL.md").is_file());
    assert!(root.join("skills/b/SKILL.md").is_file());
    let calls = fx.node_calls();
    let seen = values_of(&calls[0], "--skill");
    assert_eq!(
        seen,
        [
            root.join("skills/a").display().to_string(),
            root.join("skills/b").display().to_string()
        ]
    );
}

#[test]
fn run_without_skills_makes_no_agent_resources_folder() {
    let fx = Fixture::new();
    let dot = fx.dot("p.dot");
    let output = fx.pas(&["run", arg(&dot), "--json"]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(!run_dir_of(&output).join("agent-resources").exists());
    assert!(values_of(&fx.node_calls()[0], "--skill").is_empty());
}

#[test]
fn over_cap_skill_is_a_setup_error_before_any_node_and_before_run_json() {
    let fx = Fixture::new();
    let big = fx.skill("big");
    fs::File::create(big.join("blob.bin"))
        .unwrap()
        .set_len(11 * 1024 * 1024)
        .unwrap();
    let dot = fx.dot("p.dot");
    let output = fx.pas(&["run", arg(&dot), "--json", "--codergen-skill", arg(&big)]);
    assert!(!output.status.success());
    let all = text(&output);
    assert!(all.contains("run_setup_failed"), "{all}");
    assert!(all.contains("10 MiB"), "{all}");
    assert!(fx.node_calls().is_empty());
    let run_json: Vec<_> = walk(&fx.path("work/.pas"))
        .into_iter()
        .filter(|p| p.file_name().is_some_and(|n| n == "run.json"))
        .collect();
    assert!(run_json.is_empty(), "{run_json:?}");
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            out.extend(walk(&path));
        }
        out.push(path);
    }
    out
}

#[test]
fn dry_run_with_skills_makes_no_copy() {
    let fx = Fixture::new();
    let a = fx.skill("a");
    let dot = fx.dot("p.dot");
    let output = fx.pas(&[
        "run",
        arg(&dot),
        "--dry-run",
        "--json",
        "--codergen-skill",
        arg(&a),
    ]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(!run_dir_of(&output).join("agent-resources").exists());
}

#[test]
fn resumed_run_with_one_skill_fewer_leaves_nothing_of_the_removed_skill() {
    let fx = Fixture::new();
    let a = fx.skill("a");
    let b = fx.skill("b");
    fs::write(b.join("only-b.txt"), "b").unwrap();
    let dot = fx.dot("p.dot");
    // The first Attempt fails at the node, so its checkpoint stays and the
    // same command resumes the same Run.
    fs::write(fx.path("fail-nodes"), "").unwrap();
    let first = fx.pas(&[
        "run",
        arg(&dot),
        "--json",
        "--codergen-skill",
        arg(&a),
        "--codergen-skill",
        arg(&b),
    ]);
    assert!(!first.status.success(), "{}", text(&first));
    fs::remove_file(fx.path("fail-nodes")).unwrap();
    let run_dir = run_dir_of(&first);
    assert!(run_dir
        .join("agent-resources/claude-plugin/skills/b/only-b.txt")
        .is_file());

    let second = fx.pas(&["run", arg(&dot), "--json", "--codergen-skill", arg(&a)]);
    // The checkpoint keeps the node's spent retries, so this Attempt may end
    // in failure; the copy is rebuilt in setup before any node.
    assert!(
        text(&second).contains("Resuming from checkpoint"),
        "{}",
        text(&second)
    );
    assert_eq!(run_dir_of(&second), run_dir);
    let root = run_dir.join("agent-resources/claude-plugin");
    assert!(root.join("skills/a/SKILL.md").is_file());
    assert!(!root.join("skills/b").exists());
    assert!(!walk(&run_dir.join("agent-resources"))
        .iter()
        .any(|p| p.file_name().is_some_and(|n| n == "only-b.txt")));
}
