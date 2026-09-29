//! Assembles an ordered multi-file Plan into one text (spec C7).

use std::fs::File;
use std::io::Read;
use std::path::PathBuf;

use anyhow::{anyhow, bail};

const MAX_FILES: usize = 20;
const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// Joins `files` into one Plan text, in the given order.
///
/// Each document is preceded by `# Plan document N of M: <file name>`.
/// Only `.md` and `.txt` files are accepted (case-insensitive), each valid
/// UTF-8 and at most 1 MiB; at most 20 files. All files are validated before
/// anything is returned.
pub fn load_plan(files: &[PathBuf]) -> anyhow::Result<String> {
    if files.is_empty() {
        bail!("a Plan needs at least one file");
    }
    if files.len() > MAX_FILES {
        bail!(
            "a Plan can have at most {MAX_FILES} files, got {}",
            files.len()
        );
    }

    let total = files.len();
    let mut docs = Vec::with_capacity(total);
    for (i, path) in files.iter().enumerate() {
        let ext_ok = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("txt"));
        if !ext_ok {
            bail!(
                "Plan file '{}' must have a .md or .txt extension",
                path.display()
            );
        }

        let file = File::open(path)
            .map_err(|e| anyhow!("Failed to read Plan file '{}': {}", path.display(), e))?;
        let mut bytes = Vec::new();
        file.take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| anyhow!("Failed to read Plan file '{}': {}", path.display(), e))?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            bail!(
                "Plan file '{}' is larger than {MAX_FILE_BYTES} bytes (1 MiB)",
                path.display()
            );
        }
        let content = String::from_utf8(bytes)
            .map_err(|_| anyhow!("Plan file '{}' is not valid UTF-8", path.display()))?;

        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut doc = format!(
            "# Plan document {} of {}: {}\n{}",
            i + 1,
            total,
            name,
            content
        );
        if !doc.ends_with('\n') {
            doc.push('\n');
        }
        docs.push(doc);
    }
    Ok(docs.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(dir: &TempDir, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.path().join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn three_files_get_numbered_headings_in_order() {
        let d = TempDir::new().unwrap();
        let a = write(&d, "a.md", b"alpha\n");
        let b = write(&d, "b.txt", b"beta\n");
        let c = write(&d, "c.md", b"gamma\n");
        let text = load_plan(&[a.clone(), b.clone(), c.clone()]).unwrap();
        let h1 = text.find("# Plan document 1 of 3: a.md\nalpha").unwrap();
        let h2 = text.find("# Plan document 2 of 3: b.txt\nbeta").unwrap();
        let h3 = text.find("# Plan document 3 of 3: c.md\ngamma").unwrap();
        assert!(h1 < h2 && h2 < h3);

        let rev = load_plan(&[c, b, a]).unwrap();
        assert!(rev.starts_with("# Plan document 1 of 3: c.md\ngamma"));
        assert!(rev.contains("# Plan document 3 of 3: a.md\nalpha"));
    }

    #[test]
    fn other_extension_is_rejected_naming_the_file() {
        let d = TempDir::new().unwrap();
        let pdf = write(&d, "notes.pdf", b"x");
        let err = load_plan(&[pdf]).unwrap_err().to_string();
        assert!(err.contains("notes.pdf"), "{err}");
        let none = write(&d, "README", b"x");
        let err = load_plan(&[none]).unwrap_err().to_string();
        assert!(err.contains("README"), "{err}");
        assert!(load_plan(&[write(&d, "UP.MD", b"x")]).is_ok());
    }

    #[test]
    fn invalid_utf8_is_rejected_naming_the_file() {
        let d = TempDir::new().unwrap();
        let bad = write(&d, "bad.md", &[0xff, 0xfe, 0x00]);
        let err = load_plan(&[bad]).unwrap_err().to_string();
        assert!(err.contains("bad.md"), "{err}");
    }

    #[test]
    fn size_limit_is_exactly_one_mib() {
        let d = TempDir::new().unwrap();
        let ok = write(&d, "ok.md", &vec![b'a'; 1_048_576]);
        assert!(load_plan(&[ok]).is_ok());
        let big = write(&d, "big.md", &vec![b'a'; 1_048_577]);
        let err = load_plan(&[big]).unwrap_err().to_string();
        assert!(err.contains("big.md"), "{err}");
    }

    #[test]
    fn file_count_limit_is_twenty() {
        let d = TempDir::new().unwrap();
        let files: Vec<PathBuf> = (0..21)
            .map(|i| write(&d, &format!("f{i}.md"), b"x"))
            .collect();
        assert!(load_plan(&files[..20]).is_ok());
        assert!(load_plan(&files).is_err());
        let missing: Vec<PathBuf> = (0..21)
            .map(|i| d.path().join(format!("nope{i}.md")))
            .collect();
        let err = load_plan(&missing).unwrap_err().to_string();
        assert!(err.contains("at most 20"), "{err}");
    }

    #[test]
    fn zero_files_are_rejected() {
        assert!(load_plan(&[]).is_err());
    }

    #[test]
    fn missing_file_is_rejected_naming_the_file() {
        let d = TempDir::new().unwrap();
        let err = load_plan(&[d.path().join("gone.md")])
            .unwrap_err()
            .to_string();
        assert!(err.contains("gone.md"), "{err}");
    }

    #[test]
    fn bad_file_after_good_ones_fails_the_whole_plan() {
        let d = TempDir::new().unwrap();
        let a = write(&d, "a.md", b"ok");
        let b = write(&d, "b.md", b"ok");
        let bad = write(&d, "z.bin", b"ok");
        assert!(load_plan(&[a, b, bad]).is_err());
    }
}
