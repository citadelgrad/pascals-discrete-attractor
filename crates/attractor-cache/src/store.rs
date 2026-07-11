//! Content-addressed filesystem store for cache entries.
//!
//! Layout: `<root>/v1/<first-2-hex>/<full-key>.json`. Entries are written
//! atomically (temp file in the shard dir + rename). Reads honour an optional
//! TTL by comparing the entry's `created_at` against now.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::key::CACHE_SCHEMA_VERSION;
use crate::{CacheConfig, CacheMode};

/// On-disk layout version. Distinct from the key schema version — bump this only
/// when the directory/record layout changes.
const STORE_LAYOUT: &str = "v1";

/// A cached codergen/LLM result. Only *successful* results are ever stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    /// Key-recipe version the entry was written under.
    #[serde(default)]
    pub schema_version: u32,
    /// RFC 3339 timestamp of when the entry was written.
    pub created_at: String,
    /// Provider display name that produced the result (for observability).
    #[serde(default)]
    pub provider: String,
    /// The model response text.
    pub result_text: String,
    /// Whether the original result was an error (kept for completeness; the
    /// store refuses to persist errors, so this is `false` in practice).
    #[serde(default)]
    pub is_error: bool,
    /// Cost of the original call in USD — the amount saved on a hit.
    #[serde(default)]
    pub cost_usd: Option<f64>,
    /// Number of turns the original call took.
    #[serde(default)]
    pub turns: Option<u32>,
    /// Routing label extracted from a conditional node's response, if any.
    #[serde(default)]
    pub label: Option<String>,
}

impl CacheEntry {
    /// Build a new successful entry stamped with the current time and schema.
    pub fn new(
        provider: impl Into<String>,
        result_text: impl Into<String>,
        cost_usd: Option<f64>,
        turns: Option<u32>,
        label: Option<String>,
    ) -> Self {
        Self {
            schema_version: CACHE_SCHEMA_VERSION,
            created_at: chrono::Utc::now().to_rfc3339(),
            provider: provider.into(),
            result_text: result_text.into(),
            is_error: false,
            cost_usd,
            turns,
            label,
        }
    }
}

/// Summary of the on-disk cache, for `pas cache stats`.
#[derive(Debug, Default, Clone)]
pub struct CacheStats {
    pub entries: usize,
    pub total_bytes: u64,
}

/// The cache handle. Cheap to clone/construct; all state lives on disk.
#[derive(Debug, Clone)]
pub struct Cache {
    config: CacheConfig,
}

impl Cache {
    pub fn new(config: CacheConfig) -> Self {
        Self { config }
    }

    pub fn mode(&self) -> CacheMode {
        self.config.mode
    }

    pub fn root(&self) -> &Path {
        &self.config.root
    }

    /// Directory holding this layout version's shards.
    fn versioned_root(&self) -> PathBuf {
        self.config.root.join(STORE_LAYOUT)
    }

    fn entry_path(&self, key: &str) -> PathBuf {
        let shard = if key.len() >= 2 { &key[..2] } else { "00" };
        self.versioned_root()
            .join(shard)
            .join(format!("{key}.json"))
    }

    /// Look up a cached entry. Returns `None` when caching is disabled for reads,
    /// the entry is missing, unreadable, or expired past the configured TTL.
    pub fn get(&self, key: &str) -> Option<CacheEntry> {
        if !self.config.mode.reads() {
            return None;
        }
        let path = self.entry_path(key);
        let raw = std::fs::read_to_string(&path).ok()?;
        let entry: CacheEntry = match serde_json::from_str(&raw) {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!(path = %path.display(), error = %e, "Ignoring corrupt cache entry");
                return None;
            }
        };
        if self.is_expired(&entry) {
            tracing::debug!(path = %path.display(), "Cache entry expired");
            return None;
        }
        Some(entry)
    }

    /// Store an entry. No-op when caching does not write, or when the entry is an
    /// error (transient failures must not be pinned).
    pub fn put(&self, key: &str, entry: &CacheEntry) -> std::io::Result<()> {
        if !self.config.mode.writes() || entry.is_error {
            return Ok(());
        }
        let path = self.entry_path(key);
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(dir)?;

        let json = serde_json::to_string_pretty(entry)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        let tmp = tempfile::Builder::new()
            .prefix("entry")
            .suffix(".json.tmp")
            .tempfile_in(dir)?;
        std::fs::write(tmp.path(), json.as_bytes())?;
        tmp.persist(&path)
            .map_err(|e| std::io::Error::other(e.error))?;
        Ok(())
    }

    fn is_expired(&self, entry: &CacheEntry) -> bool {
        let ttl = match self.config.ttl {
            Some(t) => t,
            None => return false,
        };
        let created = match chrono::DateTime::parse_from_rfc3339(&entry.created_at) {
            Ok(dt) => dt.with_timezone(&chrono::Utc),
            Err(_) => return false, // unparseable timestamp: treat as fresh rather than lose it
        };
        let age = chrono::Utc::now().signed_duration_since(created);
        match age.to_std() {
            Ok(age) => age > ttl,
            Err(_) => false, // negative age (clock skew): not expired
        }
    }

    /// Delete every entry under this cache root. Returns the number removed.
    pub fn clear(&self) -> std::io::Result<usize> {
        let root = self.versioned_root();
        let count = self.stats().entries;
        if root.exists() {
            std::fs::remove_dir_all(&root)?;
        }
        Ok(count)
    }

    /// Walk the store and count entries + total bytes.
    pub fn stats(&self) -> CacheStats {
        let mut stats = CacheStats::default();
        let root = self.versioned_root();
        let shards = match std::fs::read_dir(&root) {
            Ok(s) => s,
            Err(_) => return stats,
        };
        for shard in shards.flatten() {
            let entries = match std::fs::read_dir(shard.path()) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "json") {
                    stats.entries += 1;
                    if let Ok(meta) = entry.metadata() {
                        stats.total_bytes += meta.len();
                    }
                }
            }
        }
        stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::TempDir;

    fn cache_at(dir: &TempDir, mode: CacheMode, ttl: Option<Duration>) -> Cache {
        Cache::new(CacheConfig {
            mode,
            root: dir.path().to_path_buf(),
            ttl,
        })
    }

    fn sample() -> CacheEntry {
        CacheEntry::new("Claude Code", "hello world", Some(0.12), Some(3), None)
    }

    #[test]
    fn mode_returns_configured_mode() {
        let dir = TempDir::new().unwrap();
        for m in [CacheMode::Off, CacheMode::ReadWrite, CacheMode::Refresh] {
            assert_eq!(cache_at(&dir, m, None).mode(), m);
        }
    }

    #[test]
    fn entry_path_shards_by_key_prefix() {
        let dir = TempDir::new().unwrap();
        let cache = cache_at(&dir, CacheMode::ReadWrite, None);
        let path = cache.entry_path("ab12ef");
        assert!(
            path.ends_with("v1/ab/ab12ef.json"),
            "expected prefix shard, got {}",
            path.display()
        );
    }

    #[test]
    fn put_then_get_round_trips() {
        let dir = TempDir::new().unwrap();
        let cache = cache_at(&dir, CacheMode::ReadWrite, None);
        cache.put("abcd1234", &sample()).unwrap();
        let got = cache.get("abcd1234").unwrap();
        assert_eq!(got.result_text, "hello world");
        assert_eq!(got.cost_usd, Some(0.12));
    }

    #[test]
    fn miss_returns_none() {
        let dir = TempDir::new().unwrap();
        let cache = cache_at(&dir, CacheMode::ReadWrite, None);
        assert!(cache.get("deadbeef").is_none());
    }

    #[test]
    fn off_mode_never_reads_or_writes() {
        let dir = TempDir::new().unwrap();
        let rw = cache_at(&dir, CacheMode::ReadWrite, None);
        rw.put("key0001", &sample()).unwrap();

        let off = cache_at(&dir, CacheMode::Off, None);
        assert!(off.get("key0001").is_none());
        off.put("key0002", &sample()).unwrap();
        // Nothing new written by the Off cache.
        assert!(rw.get("key0002").is_none());
    }

    #[test]
    fn refresh_mode_writes_but_does_not_read() {
        let dir = TempDir::new().unwrap();
        let rw = cache_at(&dir, CacheMode::ReadWrite, None);
        rw.put("key0003", &sample()).unwrap();

        let refresh = cache_at(&dir, CacheMode::Refresh, None);
        assert!(refresh.get("key0003").is_none()); // ignores existing
        refresh.put("key0004", &sample()).unwrap(); // still writes
        assert!(rw.get("key0004").is_some());
    }

    #[test]
    fn errors_are_not_persisted() {
        let dir = TempDir::new().unwrap();
        let cache = cache_at(&dir, CacheMode::ReadWrite, None);
        let mut e = sample();
        e.is_error = true;
        cache.put("key0005", &e).unwrap();
        assert!(cache.get("key0005").is_none());
    }

    #[test]
    fn ttl_expiry_ignores_old_entries() {
        let dir = TempDir::new().unwrap();
        let cache = cache_at(&dir, CacheMode::ReadWrite, Some(Duration::from_secs(60)));
        let mut e = sample();
        // Force an old timestamp.
        e.created_at = (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
        cache.put("key0006", &e).unwrap();
        assert!(cache.get("key0006").is_none());
    }

    #[test]
    fn stats_and_clear() {
        let dir = TempDir::new().unwrap();
        let cache = cache_at(&dir, CacheMode::ReadWrite, None);
        cache.put("aa000001", &sample()).unwrap();
        cache.put("bb000002", &sample()).unwrap();
        let stats = cache.stats();
        assert_eq!(stats.entries, 2);
        assert!(stats.total_bytes > 0);

        let removed = cache.clear().unwrap();
        assert_eq!(removed, 2);
        assert_eq!(cache.stats().entries, 0);
    }
}
