//! Cross-run, content-addressed response cache for `pas`.
//!
//! This crate memoizes whole LLM/codergen invocations keyed on a blake3
//! fingerprint of their deterministic inputs. It is deliberately narrow:
//!
//! * It is a **response cache**, distinct from Anthropic's token-level prompt
//!   caching (which the `claude` CLI handles internally) and from the pipeline
//!   **checkpoint** system (which is intra-run resume, deleted on success). This
//!   cache survives successful runs and is keyed on input content, not graph
//!   position.
//! * It is **opt-in** and only ever stores *successful* results — caching a
//!   non-deterministic LLM answer is a deliberate choice the caller makes per
//!   run (and per node), so silent stale hits never happen by default.
//!
//! See `crates/attractor-pipeline/src/handlers/codergen_handler.rs` for the
//! integration point.

mod dir;
mod key;
mod store;

use std::path::PathBuf;
use std::time::Duration;

pub use dir::default_cache_root;
pub use key::{CacheKey, CACHE_SCHEMA_VERSION};
pub use store::{Cache, CacheEntry, CacheStats};

/// How the cache behaves for a given run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheMode {
    /// Caching disabled — never reads, never writes.
    #[default]
    Off,
    /// Read existing entries and write new ones (the normal `--cache` mode).
    ReadWrite,
    /// Ignore existing entries but repopulate them — force fresh calls while
    /// still updating the cache (`--refresh-cache`).
    Refresh,
}

impl CacheMode {
    /// Parse a mode string (`off` | `readwrite` | `refresh`); unknown → `Off`.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "readwrite" | "read_write" | "on" | "1" | "true" => Self::ReadWrite,
            "refresh" => Self::Refresh,
            _ => Self::Off,
        }
    }

    /// String form used to carry the mode through the pipeline context.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::ReadWrite => "readwrite",
            Self::Refresh => "refresh",
        }
    }

    /// Whether reads are consulted.
    pub fn reads(&self) -> bool {
        matches!(self, Self::ReadWrite)
    }

    /// Whether new entries are written.
    pub fn writes(&self) -> bool {
        matches!(self, Self::ReadWrite | Self::Refresh)
    }

    /// Whether the cache is active at all (reads or writes).
    pub fn is_enabled(&self) -> bool {
        !matches!(self, Self::Off)
    }
}

/// Immutable configuration for a [`Cache`].
#[derive(Debug, Clone)]
pub struct CacheConfig {
    pub mode: CacheMode,
    pub root: PathBuf,
    pub ttl: Option<Duration>,
}

impl CacheConfig {
    /// Build a config, defaulting the root to [`default_cache_root`] when `root`
    /// is `None`.
    pub fn new(mode: CacheMode, root: Option<PathBuf>, ttl: Option<Duration>) -> Self {
        Self {
            mode,
            root: root.unwrap_or_else(default_cache_root),
            ttl,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_parse_roundtrip() {
        assert_eq!(CacheMode::parse("off"), CacheMode::Off);
        assert_eq!(CacheMode::parse("readwrite"), CacheMode::ReadWrite);
        assert_eq!(CacheMode::parse("refresh"), CacheMode::Refresh);
        assert_eq!(CacheMode::parse("nonsense"), CacheMode::Off);
        assert_eq!(
            CacheMode::parse(CacheMode::Refresh.as_str()),
            CacheMode::Refresh
        );
        assert_eq!(
            CacheMode::parse(CacheMode::ReadWrite.as_str()),
            CacheMode::ReadWrite
        );
    }

    #[test]
    fn mode_capability_flags() {
        assert!(!CacheMode::Off.reads() && !CacheMode::Off.writes());
        assert!(CacheMode::ReadWrite.reads() && CacheMode::ReadWrite.writes());
        assert!(!CacheMode::Refresh.reads() && CacheMode::Refresh.writes());
        assert!(CacheMode::ReadWrite.is_enabled());
        assert!(CacheMode::Refresh.is_enabled());
        assert!(!CacheMode::Off.is_enabled());
    }
}
