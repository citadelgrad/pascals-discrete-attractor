//! Cache-directory resolution.
//!
//! Follows the OS cache convention (the analogue of the `XDG_CONFIG_HOME`
//! pattern used by the trust store), namespaced under `pas/`. Overridable with
//! `PAS_CACHE_DIR` for tests and users who want a project-local cache.

use std::path::PathBuf;

/// Resolve the default cache root.
///
/// Precedence:
///   1. `PAS_CACHE_DIR` environment variable (used verbatim)
///   2. `dirs::cache_dir()/pas` (e.g. `~/.cache/pas` on Linux)
///   3. `./.pas/cache` fallback when no OS cache dir is available
pub fn default_cache_root() -> PathBuf {
    if let Ok(dir) = std::env::var("PAS_CACHE_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    dirs::cache_dir()
        .map(|d| d.join("pas"))
        .unwrap_or_else(|| PathBuf::from(".pas").join("cache"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn pas_cache_dir_env_overrides() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("PAS_CACHE_DIR", "/tmp/pas-cache-test");
        assert_eq!(default_cache_root(), PathBuf::from("/tmp/pas-cache-test"));
        std::env::remove_var("PAS_CACHE_DIR");
    }

    #[test]
    fn empty_env_falls_through_to_default() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("PAS_CACHE_DIR", "");
        // Should not be the empty path; ends in "pas" (or the fallback).
        let root = default_cache_root();
        assert!(root.ends_with("pas") || root.ends_with("cache"));
        std::env::remove_var("PAS_CACHE_DIR");
    }
}
