//! `pas cache` — inspect and manage the cross-run response cache.

use attractor_cache::{Cache, CacheConfig, CacheMode};

/// What the `pas cache` subcommand should do.
pub enum CacheAction {
    /// Delete every cached entry.
    Clear,
    /// Print entry count and total size.
    Stats,
    /// Print the resolved cache directory.
    Path,
}

pub fn cmd_cache(action: CacheAction) -> anyhow::Result<()> {
    let root = attractor_cache::default_cache_root();
    // Mode is irrelevant for path/stats/clear (none of them read or write entries
    // through the mode gate), so any enabled mode works for constructing a handle.
    let cache = Cache::new(CacheConfig::new(
        CacheMode::ReadWrite,
        Some(root.clone()),
        None,
    ));

    match action {
        CacheAction::Path => {
            println!("{}", root.display());
        }
        CacheAction::Stats => {
            let stats = cache.stats();
            println!("Cache dir: {}", root.display());
            println!("Entries:   {}", stats.entries);
            println!("Size:      {} bytes", stats.total_bytes);
        }
        CacheAction::Clear => {
            let removed = cache.clear()?;
            println!(
                "Cleared {} cache entrie(s) from {}",
                removed,
                root.display()
            );
        }
    }
    Ok(())
}
