//! The Run's copy of the named skills (spec C5).
//!
//! Skills are copied into `<run_dir>/agent-resources/claude-plugin/` so pi and
//! Claude load the same bytes and a source edit cannot change a running
//! Attempt. The copy is built in a temporary directory inside the Run
//! directory and then swapped in. A directory cannot be renamed over a
//! non-empty directory, so the swap is two renames; no node has started yet,
//! so none sees a half-built copy.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Total size of all copied skills, in bytes.
pub const SKILL_COPY_CAP_BYTES: u64 = 10 * 1024 * 1024;

const RESOURCES_DIR: &str = "agent-resources";
const PLUGIN_DIR: &str = "claude-plugin";
const PLUGIN_TMP: &str = "claude-plugin.tmp";
const PLUGIN_OLD: &str = "claude-plugin.old";
const PLUGIN_MANIFEST: &str = r#"{
  "name": "pas-skills",
  "version": "0.0.0",
  "description": "Skills named for this PAS Run"
}
"#;

/// Why the skill copy could not be built. `Display` names the path or cap.
#[derive(Debug)]
pub enum SkillCopyError {
    /// A symbolic link or other special entry inside a skill tree.
    UnsupportedEntry {
        path: PathBuf,
    },
    /// The skills together are larger than the cap.
    TooLarge {
        path: PathBuf,
    },
    /// The skill entry has no directory name, is not a directory, has no
    /// `SKILL.md`, or repeats another entry's name.
    InvalidSkill {
        path: PathBuf,
        reason: String,
    },
    Io {
        path: PathBuf,
        source: io::Error,
    },
}

impl fmt::Display for SkillCopyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedEntry { path } => write!(
                f,
                "skill contains a symbolic link or special file at {}; skills are copied into \
                 the Run folder and may not contain links. Replace it with a regular file",
                path.display()
            ),
            Self::TooLarge { path } => write!(
                f,
                "named skills are larger than the 10 MiB copy cap (reached at {}); \
                 remove files from the skills",
                path.display()
            ),
            Self::InvalidSkill { path, reason } => {
                write!(f, "skill {} cannot be copied: {reason}", path.display())
            }
            Self::Io { path, source } => {
                write!(f, "cannot copy skills at {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for SkillCopyError {}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> SkillCopyError + '_ {
    move |source| SkillCopyError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// `<run_dir>/agent-resources/claude-plugin`.
pub fn skill_copy_root(run_dir: &Path) -> PathBuf {
    run_dir.join(RESOURCES_DIR).join(PLUGIN_DIR)
}

/// The directories pi loads: `<root>/skills/<name>` per entry, in list order.
/// Pure path math; it reads no file.
pub fn copied_skill_dirs(run_dir: &Path, skills: &[PathBuf]) -> Vec<PathBuf> {
    let root = skill_copy_root(run_dir).join("skills");
    skills
        .iter()
        .filter_map(|skill| skill.file_name())
        .map(|name| root.join(name))
        .collect()
}

/// Build the copy for one Attempt. Returns the copy root, or `None` (and no
/// copy) for an empty list. An older copy is removed first in that case so no
/// file of an earlier Attempt stays. On an error the previous copy is kept.
pub fn build_skill_copy(
    run_dir: &Path,
    skills: &[PathBuf],
) -> Result<Option<PathBuf>, SkillCopyError> {
    let resources = run_dir.join(RESOURCES_DIR);
    let root = resources.join(PLUGIN_DIR);

    if skills.is_empty() {
        if exists_no_follow(&root) {
            remove_no_follow(&root).map_err(io_err(&root))?;
        }
        return Ok(None);
    }

    ensure_real_dir(&resources)?;
    let tmp = resources.join(PLUGIN_TMP);
    let old = resources.join(PLUGIN_OLD);
    for leftover in [&tmp, &old] {
        if exists_no_follow(leftover) {
            remove_no_follow(leftover).map_err(io_err(leftover))?;
        }
    }
    fs::create_dir(&tmp).map_err(io_err(&tmp))?;

    if let Err(e) = fill_copy(&tmp, skills) {
        let _ = remove_no_follow(&tmp);
        return Err(e);
    }

    let swap = (|| -> Result<(), SkillCopyError> {
        if exists_no_follow(&root) {
            fs::rename(&root, &old).map_err(io_err(&root))?;
        }
        fs::rename(&tmp, &root).map_err(io_err(&root))?;
        Ok(())
    })();
    if let Err(e) = swap {
        let _ = remove_no_follow(&tmp);
        if !exists_no_follow(&root) && exists_no_follow(&old) {
            let _ = fs::rename(&old, &root);
        }
        return Err(e);
    }
    if exists_no_follow(&old) {
        remove_no_follow(&old).map_err(io_err(&old))?;
    }
    Ok(Some(root))
}

fn fill_copy(tmp: &Path, skills: &[PathBuf]) -> Result<(), SkillCopyError> {
    let manifest_dir = tmp.join(".claude-plugin");
    fs::create_dir(&manifest_dir).map_err(io_err(&manifest_dir))?;
    let manifest = manifest_dir.join("plugin.json");
    fs::write(&manifest, PLUGIN_MANIFEST).map_err(io_err(&manifest))?;
    let skills_dir = tmp.join("skills");
    fs::create_dir(&skills_dir).map_err(io_err(&skills_dir))?;

    let mut total = 0u64;
    let mut names = std::collections::BTreeSet::new();
    for skill in skills {
        let invalid = |reason: &str| SkillCopyError::InvalidSkill {
            path: skill.clone(),
            reason: reason.to_string(),
        };
        let name = skill
            .file_name()
            .ok_or_else(|| invalid("the path has no directory name"))?;
        if !names.insert(name.to_os_string()) {
            return Err(invalid("another skill has the same directory name"));
        }
        // `metadata` follows a link on the entry itself only.
        let meta = fs::metadata(skill).map_err(io_err(skill))?;
        if !meta.is_dir() {
            return Err(invalid("it is not a directory"));
        }
        if !skill.join("SKILL.md").is_file() {
            return Err(invalid("it has no SKILL.md"));
        }
        let dest = skills_dir.join(name);
        fs::create_dir(&dest).map_err(io_err(&dest))?;
        copy_tree(skill, &dest, &mut total)?;
    }
    Ok(())
}

fn copy_tree(src: &Path, dest: &Path, total: &mut u64) -> Result<(), SkillCopyError> {
    let mut entries = fs::read_dir(src)
        .map_err(io_err(src))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(io_err(src))?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let target = dest.join(entry.file_name());
        let file_type = entry.file_type().map_err(io_err(&path))?;
        if file_type.is_dir() {
            fs::create_dir(&target).map_err(io_err(&target))?;
            copy_tree(&path, &target, total)?;
        } else if file_type.is_file() {
            let len = entry.metadata().map_err(io_err(&path))?.len();
            *total = total.saturating_add(len);
            if *total > SKILL_COPY_CAP_BYTES {
                return Err(SkillCopyError::TooLarge { path });
            }
            fs::copy(&path, &target).map_err(io_err(&path))?;
        } else {
            return Err(SkillCopyError::UnsupportedEntry { path });
        }
    }
    Ok(())
}

fn exists_no_follow(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// Remove a file, link or directory tree without following a link.
fn remove_no_follow(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// Make `path` a real directory; a link or file in its place is removed first.
fn ensure_real_dir(path: &Path) -> Result<(), SkillCopyError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => return Ok(()),
        Ok(_) => remove_no_follow(path).map_err(io_err(path))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_err(path)(e)),
    }
    fs::create_dir(path).map_err(io_err(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn skill(base: &Path, name: &str, body: &str) -> PathBuf {
        let dir = base.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), body).unwrap();
        dir
    }

    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let run = tmp.path().join("run");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&run).unwrap();
        (tmp, src, run)
    }

    fn big_skill(base: &Path, name: &str, bytes: u64) -> PathBuf {
        let dir = skill(base, name, "x");
        let f = fs::File::create(dir.join("big.bin")).unwrap();
        f.set_len(bytes).unwrap();
        dir
    }

    #[test]
    fn two_skills_make_one_manifest_and_two_skill_dirs() {
        let (_t, src, run) = setup();
        let a = skill(&src, "a", "alpha");
        fs::create_dir(a.join("nested")).unwrap();
        fs::write(a.join("nested/extra.txt"), "extra").unwrap();
        let b = skill(&src, "b", "beta");
        let root = build_skill_copy(&run, &[a, b]).unwrap().unwrap();
        assert_eq!(root, skill_copy_root(&run));
        let manifest: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(root.join(".claude-plugin/plugin.json")).unwrap(),
        )
        .unwrap();
        for key in ["name", "version", "description"] {
            assert!(manifest[key].is_string(), "{key}");
        }
        assert_eq!(
            fs::read_to_string(root.join("skills/a/SKILL.md")).unwrap(),
            "alpha"
        );
        assert_eq!(
            fs::read_to_string(root.join("skills/b/SKILL.md")).unwrap(),
            "beta"
        );
        assert_eq!(
            fs::read_to_string(root.join("skills/a/nested/extra.txt")).unwrap(),
            "extra"
        );
        let mut top: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        top.sort();
        assert_eq!(top, [".claude-plugin", "skills"]);
        let resources: Vec<_> = fs::read_dir(run.join("agent-resources")).unwrap().collect();
        assert_eq!(resources.len(), 1);
    }

    #[test]
    fn inner_symlink_fails_and_names_the_link() {
        let (_t, src, run) = setup();
        let a = skill(&src, "a", "alpha");
        fs::create_dir(a.join("deep")).unwrap();
        fs::write(src.join("secret"), "s").unwrap();
        let link = a.join("deep/leak");
        symlink(src.join("secret"), &link).unwrap();
        let err = build_skill_copy(&run, &[a]).unwrap_err();
        assert!(
            err.to_string().contains(&link.display().to_string()),
            "{err}"
        );
        assert!(!skill_copy_root(&run).exists());
        assert!(!run.join("agent-resources/claude-plugin.tmp").exists());
    }

    #[test]
    fn symlinked_entry_is_copied_from_its_target() {
        let (_t, src, run) = setup();
        let real = skill(&src, "real-other", "target bytes");
        let entry = src.join("link-skill");
        symlink(&real, &entry).unwrap();
        let root = build_skill_copy(&run, &[entry]).unwrap().unwrap();
        let copied = root.join("skills/link-skill");
        assert!(!fs::symlink_metadata(&copied).unwrap().is_symlink());
        let file = copied.join("SKILL.md");
        assert!(fs::symlink_metadata(&file).unwrap().is_file());
        assert_eq!(fs::read_to_string(file).unwrap(), "target bytes");
    }

    #[test]
    fn source_change_after_the_copy_does_not_change_it() {
        let (_t, src, run) = setup();
        let a = skill(&src, "a", "before");
        fs::write(a.join("keep.txt"), "keep").unwrap();
        let root = build_skill_copy(&run, std::slice::from_ref(&a))
            .unwrap()
            .unwrap();
        fs::write(a.join("SKILL.md"), "after").unwrap();
        fs::remove_file(a.join("keep.txt")).unwrap();
        assert_eq!(
            fs::read_to_string(root.join("skills/a/SKILL.md")).unwrap(),
            "before"
        );
        assert_eq!(
            fs::read_to_string(root.join("skills/a/keep.txt")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn rebuild_with_one_skill_fewer_leaves_no_removed_file() {
        let (_t, src, run) = setup();
        let a = skill(&src, "a", "alpha");
        let b = skill(&src, "b", "beta");
        fs::write(b.join("only-b.txt"), "b").unwrap();
        build_skill_copy(&run, &[a.clone(), b]).unwrap();
        let root = build_skill_copy(&run, &[a]).unwrap().unwrap();
        assert!(root.join("skills/a/SKILL.md").is_file());
        assert!(!root.join("skills/b").exists());
        let names: Vec<_> = fs::read_dir(run.join("agent-resources"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, ["claude-plugin"]);
    }

    #[test]
    fn over_the_cap_fails_and_names_the_cap() {
        let (_t, src, run) = setup();
        let a = skill(&src, "a", "alpha");
        let keep = build_skill_copy(&run, &[a]).unwrap().unwrap();
        let six = 6 * 1024 * 1024;
        let big = [big_skill(&src, "x", six), big_skill(&src, "y", six)];
        let err = build_skill_copy(&run, &big).unwrap_err();
        assert!(err.to_string().contains("10 MiB"), "{err}");
        assert!(matches!(err, SkillCopyError::TooLarge { .. }));
        assert!(keep.join("skills/a/SKILL.md").is_file());
        assert!(!run.join("agent-resources/claude-plugin.tmp").exists());
    }

    #[test]
    fn exactly_the_cap_is_allowed() {
        let (_t, src, run) = setup();
        // 1 byte of SKILL.md plus the rest.
        let s = big_skill(&src, "x", SKILL_COPY_CAP_BYTES - 1);
        assert!(build_skill_copy(&run, &[s]).unwrap().is_some());
        let s2 = big_skill(&src, "y", SKILL_COPY_CAP_BYTES);
        assert!(build_skill_copy(&run, &[s2]).is_err());
    }

    #[test]
    fn rebuild_does_not_follow_existing_links() {
        let (t, src, run) = setup();
        let outside = t.path().join("outside");
        let outside_tmp = t.path().join("outside-tmp");
        for dir in [&outside, &outside_tmp] {
            fs::create_dir(dir).unwrap();
            fs::write(dir.join("sentinel"), "keep").unwrap();
        }
        let resources = run.join("agent-resources");
        fs::create_dir(&resources).unwrap();
        symlink(&outside, resources.join("claude-plugin")).unwrap();
        symlink(&outside_tmp, resources.join("claude-plugin.tmp")).unwrap();
        let a = skill(&src, "a", "alpha");
        let root = build_skill_copy(&run, std::slice::from_ref(&a))
            .unwrap()
            .unwrap();
        assert!(!fs::symlink_metadata(&root).unwrap().is_symlink());
        assert!(root.join("skills/a/SKILL.md").is_file());
        for dir in [&outside, &outside_tmp] {
            assert_eq!(fs::read_to_string(dir.join("sentinel")).unwrap(), "keep");
            assert_eq!(fs::read_dir(dir).unwrap().count(), 1);
        }

        // `agent-resources` itself is a link.
        let (t2, src2, run2) = setup();
        let outside2 = t2.path().join("outside");
        fs::create_dir(&outside2).unwrap();
        fs::write(outside2.join("sentinel"), "keep").unwrap();
        symlink(&outside2, run2.join("agent-resources")).unwrap();
        let a2 = skill(&src2, "a", "alpha");
        build_skill_copy(&run2, &[a2]).unwrap().unwrap();
        assert_eq!(fs::read_dir(&outside2).unwrap().count(), 1);
        assert!(!fs::symlink_metadata(run2.join("agent-resources"))
            .unwrap()
            .is_symlink());
    }

    #[test]
    fn empty_list_makes_no_copy() {
        let (_t, _src, run) = setup();
        assert!(build_skill_copy(&run, &[]).unwrap().is_none());
        assert!(!run.join("agent-resources").exists());
    }

    #[test]
    fn empty_list_removes_an_old_copy() {
        let (_t, src, run) = setup();
        build_skill_copy(&run, &[skill(&src, "a", "alpha")]).unwrap();
        assert!(build_skill_copy(&run, &[]).unwrap().is_none());
        assert!(!skill_copy_root(&run).exists());
    }

    #[test]
    fn duplicate_name_and_missing_skill_md_are_errors() {
        let (_t, src, run) = setup();
        let one = skill(&src, "a", "alpha");
        let other_base = src.join("other");
        fs::create_dir(&other_base).unwrap();
        let two = skill(&other_base, "a", "alpha2");
        let err = build_skill_copy(&run, &[one, two]).unwrap_err();
        assert!(err.to_string().contains("same directory name"), "{err}");

        let empty = src.join("empty");
        fs::create_dir(&empty).unwrap();
        let err = build_skill_copy(&run, &[empty]).unwrap_err();
        assert!(err.to_string().contains("SKILL.md"), "{err}");
    }

    #[test]
    fn copied_dirs_keep_list_order() {
        let run = Path::new("/r");
        let dirs = copied_skill_dirs(run, &[PathBuf::from("/x/b"), PathBuf::from("/y/a")]);
        assert_eq!(
            dirs,
            [
                PathBuf::from("/r/agent-resources/claude-plugin/skills/b"),
                PathBuf::from("/r/agent-resources/claude-plugin/skills/a"),
            ]
        );
    }

    #[test]
    fn leftover_temp_dir_from_a_crashed_attempt_is_cleaned() {
        let (_t, src, run) = setup();
        let tmp = run.join("agent-resources/claude-plugin.tmp/skills/stale");
        fs::create_dir_all(&tmp).unwrap();
        build_skill_copy(&run, &[skill(&src, "a", "alpha")]).unwrap();
        assert!(!run.join("agent-resources/claude-plugin.tmp").exists());
        assert!(!skill_copy_root(&run).join("skills/stale").exists());
    }
}
