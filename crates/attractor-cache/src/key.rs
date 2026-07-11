//! Content-addressed cache keys derived from a canonical, ordered serialization
//! of the inputs that make a cached result deterministic.
//!
//! Every field is length-prefixed before hashing so that no pair of `(name,
//! value)` fields can be confused with a differently-split pair — i.e.
//! `field("a", "bc")` and `field("ab", "c")` produce different digests.

use blake3::Hasher;

/// Version of the key recipe. Bump this whenever the set of inputs folded into a
/// key changes so that all previously-written entries become unreachable (and
/// are eventually pruned by TTL / `pas cache clear`) instead of returning stale
/// hits under a new recipe.
pub const CACHE_SCHEMA_VERSION: u32 = 1;

/// Builder that accumulates ordered, length-prefixed fields into a blake3 digest.
#[derive(Clone)]
pub struct CacheKey {
    hasher: Hasher,
}

impl Default for CacheKey {
    fn default() -> Self {
        Self::new()
    }
}

impl CacheKey {
    /// Start a new key. The schema version is folded in first so a recipe bump
    /// changes every key.
    pub fn new() -> Self {
        let mut hasher = Hasher::new();
        hasher.update(b"attractor-cache");
        hasher.update(&CACHE_SCHEMA_VERSION.to_le_bytes());
        Self { hasher }
    }

    /// Fold a named field into the key. Order matters — call fields in a stable
    /// order at every call site.
    pub fn field(mut self, name: &str, value: &str) -> Self {
        self.hasher.update(&(name.len() as u64).to_le_bytes());
        self.hasher.update(name.as_bytes());
        self.hasher.update(&(value.len() as u64).to_le_bytes());
        self.hasher.update(value.as_bytes());
        self
    }

    /// Fold an optional field. `None` hashes distinctly from an empty string.
    pub fn field_opt(mut self, name: &str, value: Option<&str>) -> Self {
        match value {
            Some(v) => {
                self.hasher.update(&[1u8]);
                self = self.field(name, v);
            }
            None => {
                self.hasher.update(&[0u8]);
                self.hasher.update(&(name.len() as u64).to_le_bytes());
                self.hasher.update(name.as_bytes());
            }
        }
        self
    }

    /// Finish and return the hex-encoded digest used as the on-disk filename.
    pub fn finish(&self) -> String {
        self.hasher.finalize().to_hex().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_inputs_produce_identical_keys() {
        let a = CacheKey::new()
            .field("prompt", "hello")
            .field("model", "opus");
        let b = CacheKey::new()
            .field("prompt", "hello")
            .field("model", "opus");
        assert_eq!(a.finish(), b.finish());
    }

    #[test]
    fn different_values_produce_different_keys() {
        let a = CacheKey::new().field("prompt", "hello").finish();
        let b = CacheKey::new().field("prompt", "world").finish();
        assert_ne!(a, b);
    }

    #[test]
    fn field_order_matters() {
        let a = CacheKey::new().field("a", "1").field("b", "2").finish();
        let b = CacheKey::new().field("b", "2").field("a", "1").finish();
        assert_ne!(a, b);
    }

    #[test]
    fn length_prefixing_prevents_boundary_collisions() {
        // Without length prefixing these would hash the same bytes.
        let a = CacheKey::new().field("ab", "c").finish();
        let b = CacheKey::new().field("a", "bc").finish();
        assert_ne!(a, b);
    }

    #[test]
    fn field_opt_none_differs_from_empty() {
        let none = CacheKey::new().field_opt("model", None).finish();
        let empty = CacheKey::new().field_opt("model", Some("")).finish();
        assert_ne!(none, empty);
    }

    #[test]
    fn finish_is_hex_of_expected_length() {
        let k = CacheKey::new().field("x", "y").finish();
        assert_eq!(k.len(), 64);
        assert!(k.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
