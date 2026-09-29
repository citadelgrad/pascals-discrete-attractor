#![allow(dead_code)]

use std::path::PathBuf;

use crate::detect::DetectedToolchain;
use crate::manifest::Manifest;

const ALLOWED_FILENAMES: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "requirements.txt",
    "requirements-dev.txt",
    "setup.py",
    "setup.cfg",
    "go.mod",
    "build.gradle",
    "pom.xml",
    "Makefile",
    "CMakeLists.txt",
    "tsconfig.json",
];

const SECRET_PATTERNS: &[&str] = &[
    ".env",
    "secret",
    "credential",
    "password",
    "token",
    "key",
    "private",
];

const MAX_FILES: usize = 256;
const MAX_PAYLOAD_BYTES: usize = 8 * 1024;

pub(crate) fn enrich_needed(detected: &[DetectedToolchain]) -> bool {
    detected.len() >= 2 || detected.is_empty()
}

pub(crate) fn filter_files(files: &[PathBuf]) -> Vec<&PathBuf> {
    let mut result: Vec<&PathBuf> = files
        .iter()
        .filter(|p| {
            let name = p
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_lowercase();

            // Exclude secret-like filenames
            if SECRET_PATTERNS.iter().any(|pat| name.contains(pat)) {
                return false;
            }

            // Exclude hidden files (starting with .)
            if name.starts_with('.') {
                return false;
            }

            // Must match allowlist (exact or prefix pattern)
            let allowed = ALLOWED_FILENAMES.iter().any(|allowed| {
                let allowed_lower = allowed.to_lowercase();
                name == allowed_lower
                    || (name.starts_with("tsconfig.") && name.ends_with(".json"))
                    || (allowed_lower.starts_with("requirements")
                        && name.starts_with("requirements"))
            });

            allowed
        })
        .take(MAX_FILES)
        .collect();

    // Enforce total payload limit by size (approximate: count file bytes)
    let mut total = 0usize;
    result.retain(|p| {
        let size = p.metadata().map(|m| m.len() as usize).unwrap_or(100);
        if total + size > MAX_PAYLOAD_BYTES {
            return false;
        }
        total += size;
        true
    });

    result
}

/// Attempt LLM enrichment for polyglot repos.
/// Returns `None` if enrichment is not needed or LLM is unavailable.
pub fn enrich(_detected: &[DetectedToolchain], _file_list: &[PathBuf]) -> Option<Manifest> {
    // LLM enrichment is a stub — the --no-enrich flag is the primary escape hatch.
    // Full implementation pending attractor-r30 (quality handler sequential execution).
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_detected(lang: &str) -> DetectedToolchain {
        DetectedToolchain {
            language: lang.to_string(),
            config_file: PathBuf::from(format!("{}.toml", lang)),
        }
    }

    #[test]
    fn enrich_not_triggered_for_single_toolchain() {
        let detected = vec![make_detected("rust")];
        assert!(!enrich_needed(&detected));
    }

    #[test]
    fn enrich_triggered_for_polyglot() {
        let detected = vec![make_detected("rust"), make_detected("typescript")];
        assert!(enrich_needed(&detected));
    }

    #[test]
    fn enrich_triggered_for_empty() {
        assert!(enrich_needed(&[]));
    }

    #[test]
    fn file_filter_excludes_secrets() {
        let files = vec![
            PathBuf::from("Cargo.toml"),
            PathBuf::from(".env"),
            PathBuf::from("secrets.json"),
            PathBuf::from("package.json"),
            PathBuf::from("pyproject.toml"),
            PathBuf::from(".env.local"),
            PathBuf::from("api_key.txt"),
        ];
        let filtered = filter_files(&files);
        let names: Vec<_> = filtered
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap())
            .collect();
        assert!(names.contains(&"Cargo.toml"));
        assert!(names.contains(&"package.json"));
        assert!(names.contains(&"pyproject.toml"));
        assert!(!names.contains(&".env"));
        assert!(!names.iter().any(|n| n.contains("secret")));
        assert!(!names.iter().any(|n| n.contains("key")));
    }

    #[test]
    fn file_filter_limits_to_max_entries() {
        let tmp = tempfile::TempDir::new().unwrap();
        let files: Vec<PathBuf> = (0..300)
            .map(|i| {
                let path = tmp.path().join(format!("tsconfig.{i}.json"));
                std::fs::write(&path, "").unwrap();
                path
            })
            .collect();
        let filtered = filter_files(&files);
        assert_eq!(filtered.len(), MAX_FILES);
    }
}
