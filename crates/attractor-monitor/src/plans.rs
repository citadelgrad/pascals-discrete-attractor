//! Plan workspaces: `<state dir>/plans/<plan-id>/{docs/NN-<name>, plan.json}`.
//!
//! The Monitor cannot link the CLI's `load_plan` (ADR 0002), so the C7 limits
//! are repeated here for early feedback; `pas decompose/generate --plan`
//! re-validates when a Plan is launched. Uploaded text is stored, never served.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const MAX_FILES: usize = 20;
pub const MAX_FILE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputKind {
    #[serde(rename = "pipeline")]
    PipelineOnly,
    #[serde(rename = "epic_pipeline")]
    EpicAndPipeline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    #[serde(rename = "reviewed")]
    Reviewed,
    #[serde(rename = "one_click")]
    OneClick,
}

/// One uploaded file, in submitted order.
#[derive(Debug, Clone)]
pub struct Upload {
    pub name: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct PlanRequest {
    pub files: Vec<Upload>,
    pub repo: String,
    pub kind: OutputKind,
    pub mode: Mode,
}

/// `plan.json`, version 1. Order is the order of `files` (stored names).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanMeta {
    pub v: u32,
    pub id: String,
    pub files: Vec<String>,
    pub repo: PathBuf,
    pub kind: OutputKind,
    pub mode: Mode,
}

#[derive(Debug)]
pub enum PlanError {
    NoFiles,
    TooMany(usize),
    BadType(String),
    TooLarge(String),
    NotUtf8(String),
    NotAGitRepo(String),
    /// A malformed form field.
    BadForm(String),
    Io(io::Error),
}

impl PlanError {
    /// True when the client sent something wrong (not a server failure).
    pub fn is_client_error(&self) -> bool {
        !matches!(self, PlanError::Io(_))
    }
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanError::NoFiles => write!(f, "choose at least one .md or .txt file"),
            PlanError::TooMany(n) => {
                write!(f, "{n} files uploaded; at most {MAX_FILES} are accepted")
            }
            PlanError::BadType(n) => write!(f, "{n}: only .md and .txt files are accepted"),
            PlanError::TooLarge(n) => {
                write!(f, "{n}: larger than the {} MiB limit", MAX_FILE_BYTES >> 20)
            }
            PlanError::NotUtf8(n) => write!(f, "{n}: not valid UTF-8 text"),
            PlanError::NotAGitRepo(p) => write!(f, "{p}: not a git repository"),
            PlanError::BadForm(m) => write!(f, "{m}"),
            PlanError::Io(e) => write!(f, "cannot store the Plan: {e}"),
        }
    }
}

impl std::error::Error for PlanError {}

impl From<io::Error> for PlanError {
    fn from(e: io::Error) -> Self {
        PlanError::Io(e)
    }
}

/// The Plans folder lives next to the Run Index.
pub fn plans_root(index_path: &Path) -> PathBuf {
    index_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("plans")
}

fn has_allowed_extension(name: &str) -> bool {
    let base = client_basename(name);
    match base.rsplit_once('.') {
        Some((_, ext)) => ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("txt"),
        None => false,
    }
}

/// Final path component of a client-supplied name, for messages and storage.
fn client_basename(name: &str) -> &str {
    name.rsplit(['/', '\\']).next().unwrap_or(name)
}

/// `NN-<sanitized base name>`; only `[A-Za-z0-9._-]` survive.
fn stored_name(index: usize, name: &str) -> String {
    let clean: String = client_basename(name)
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let clean = clean.trim_start_matches('.');
    format!("{:02}-{clean}", index + 1)
}

/// Check everything without writing; returns the canonical repository path.
pub fn validate(req: &PlanRequest) -> Result<PathBuf, PlanError> {
    if req.files.is_empty() {
        return Err(PlanError::NoFiles);
    }
    if req.files.len() > MAX_FILES {
        return Err(PlanError::TooMany(req.files.len()));
    }
    for f in &req.files {
        let shown = client_basename(&f.name).to_string();
        if !has_allowed_extension(&f.name) {
            return Err(PlanError::BadType(shown));
        }
        if f.bytes.len() > MAX_FILE_BYTES {
            return Err(PlanError::TooLarge(shown));
        }
        if std::str::from_utf8(&f.bytes).is_err() {
            return Err(PlanError::NotUtf8(shown));
        }
    }
    let not_repo = || PlanError::NotAGitRepo(req.repo.clone());
    let repo = std::fs::canonicalize(&req.repo).map_err(|_| not_repo())?;
    if !repo.is_dir() || !repo.join(".git").exists() {
        return Err(not_repo());
    }
    Ok(repo)
}

/// Validate, then write the Plan all-or-nothing (temp folder, then rename).
pub fn create(plans_root: &Path, req: PlanRequest) -> Result<PlanMeta, PlanError> {
    let repo = validate(&req)?;
    std::fs::create_dir_all(plans_root)?;
    let id = uuid::Uuid::new_v4().simple().to_string();
    let tmp = plans_root.join(format!(".tmp-{id}"));
    let result = write_plan(&tmp, &id, &req, repo);
    match result {
        Ok(meta) => {
            if let Err(e) = std::fs::rename(&tmp, plans_root.join(&id)) {
                let _ = std::fs::remove_dir_all(&tmp);
                return Err(e.into());
            }
            Ok(meta)
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            Err(e.into())
        }
    }
}

fn write_plan(tmp: &Path, id: &str, req: &PlanRequest, repo: PathBuf) -> io::Result<PlanMeta> {
    let docs = tmp.join("docs");
    std::fs::create_dir_all(&docs)?;
    let mut files = Vec::new();
    for (i, f) in req.files.iter().enumerate() {
        let name = stored_name(i, &f.name);
        std::fs::write(docs.join(&name), &f.bytes)?;
        files.push(name);
    }
    let meta = PlanMeta {
        v: 1,
        id: id.to_string(),
        files,
        repo,
        kind: req.kind,
        mode: req.mode,
    };
    let json = serde_json::to_vec_pretty(&meta).map_err(io::Error::other)?;
    std::fs::write(tmp.join("plan.json"), json)?;
    Ok(meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up(name: &str, body: &str) -> Upload {
        Upload {
            name: name.into(),
            bytes: body.as_bytes().to_vec(),
        }
    }

    fn repo(tmp: &Path) -> String {
        let r = tmp.join("repo");
        std::fs::create_dir_all(r.join(".git")).unwrap();
        r.to_string_lossy().into_owned()
    }

    fn req(tmp: &Path, files: Vec<Upload>) -> PlanRequest {
        PlanRequest {
            files,
            repo: repo(tmp),
            kind: OutputKind::EpicAndPipeline,
            mode: Mode::OneClick,
        }
    }

    fn no_plans(root: &Path) -> bool {
        !root.exists() || std::fs::read_dir(root).unwrap().next().is_none()
    }

    #[test]
    fn three_files_keep_the_submitted_order() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("plans");
        let r = req(
            t.path(),
            vec![up("c.md", "C"), up("a.md", "A"), up("b.txt", "B")],
        );
        let meta = create(&root, r).unwrap();
        assert_eq!(meta.files, ["01-c.md", "02-a.md", "03-b.txt"]);
        let dir = root.join(&meta.id);
        assert_eq!(
            std::fs::read_to_string(dir.join("docs/01-c.md")).unwrap(),
            "C"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("docs/03-b.txt")).unwrap(),
            "B"
        );
        let on_disk: PlanMeta =
            serde_json::from_slice(&std::fs::read(dir.join("plan.json")).unwrap()).unwrap();
        assert_eq!(on_disk, meta);
        assert_eq!(on_disk.v, 1);
        assert!(on_disk.repo.is_absolute());
    }

    #[test]
    fn pdf_is_rejected_naming_the_file_and_nothing_is_stored() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("plans");
        let e = create(
            &root,
            req(t.path(), vec![up("a.md", "x"), up("notes.pdf", "x")]),
        )
        .unwrap_err();
        assert!(e.to_string().contains("notes.pdf"), "{e}");
        assert!(no_plans(&root));
    }

    #[test]
    fn size_limit_is_inclusive_at_one_mib() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("plans");
        let big = |n| Upload {
            name: "big.md".into(),
            bytes: vec![b'a'; n],
        };
        let e = create(&root, req(t.path(), vec![big(MAX_FILE_BYTES + 1)])).unwrap_err();
        assert!(matches!(e, PlanError::TooLarge(_)));
        assert!(e.to_string().contains("big.md"));
        assert!(no_plans(&root));
        create(&root, req(t.path(), vec![big(MAX_FILE_BYTES)])).unwrap();
    }

    #[test]
    fn file_count_limit() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("plans");
        let many = |n: usize| (0..n).map(|i| up(&format!("f{i}.md"), "x")).collect();
        let e = create(&root, req(t.path(), many(21))).unwrap_err();
        assert!(matches!(e, PlanError::TooMany(21)));
        assert!(e.to_string().contains("20"));
        assert!(no_plans(&root));
        let meta = create(&root, req(t.path(), many(20))).unwrap();
        assert_eq!(meta.files.len(), 20);
    }

    #[test]
    fn no_files_and_bad_utf8_are_rejected() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("plans");
        assert!(matches!(
            create(&root, req(t.path(), vec![])).unwrap_err(),
            PlanError::NoFiles
        ));
        let bad = Upload {
            name: "x.md".into(),
            bytes: vec![0xff, 0xfe],
        };
        let e = create(&root, req(t.path(), vec![bad])).unwrap_err();
        assert!(matches!(e, PlanError::NotUtf8(_)) && e.to_string().contains("x.md"));
        assert!(no_plans(&root));
    }

    #[test]
    fn target_must_be_a_git_repository() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("plans");
        let mut r = req(t.path(), vec![up("a.md", "x")]);
        let plain = t.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        let file = t.path().join("afile");
        std::fs::write(&file, "x").unwrap();
        for bad in [
            plain.to_string_lossy().into_owned(),
            t.path().join("missing").to_string_lossy().into_owned(),
            file.to_string_lossy().into_owned(),
            "relative/nowhere".to_string(),
            String::new(),
        ] {
            let good = r.repo.clone();
            r.repo = bad.clone();
            let e = create(&root, r.clone()).unwrap_err();
            assert!(matches!(e, PlanError::NotAGitRepo(_)), "{bad}");
            assert!(e.to_string().contains(&bad));
            r.repo = good;
        }
        assert!(no_plans(&root));
        // A `.git` file (worktree) counts.
        let wt = t.path().join("wt");
        std::fs::create_dir(&wt).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: elsewhere").unwrap();
        r.repo = wt.to_string_lossy().into_owned();
        create(&root, r).unwrap();
    }

    #[test]
    fn hostile_names_stay_inside_docs() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("plans");
        let r = req(
            t.path(),
            vec![
                up("../../etc/x.md", "1"),
                up("a/b.md", "2"),
                up("na\0me é.txt", "3"),
                up("..\\win.md", "4"),
                up(".hidden.md", "5"),
            ],
        );
        let meta = create(&root, r).unwrap();
        assert_eq!(
            meta.files,
            [
                "01-x.md",
                "02-b.md",
                "03-na_me__.txt",
                "04-win.md",
                "05-hidden.md"
            ]
        );
        let docs = root.join(&meta.id).join("docs");
        assert_eq!(std::fs::read_dir(docs).unwrap().count(), 5);
        assert!(!t.path().join("etc").exists());
    }

    #[test]
    fn extension_is_case_insensitive_and_required() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("plans");
        create(&root, req(t.path(), vec![up("A.MD", "x")])).unwrap();
        let e = create(&root, req(t.path(), vec![up("md", "x")])).unwrap_err();
        assert!(matches!(e, PlanError::BadType(_)));
    }
}
