pub mod detect;
pub mod manifest;
pub mod resolution;
pub mod telemetry;
pub mod trust;

// enrich is intentionally not in the public API yet — the LLM enrichment stub
// is incomplete and will be wired up in a future task.
pub(crate) mod enrich;

pub use manifest::{
    ClaudeCodergenConfig, ClaudeSettingSource, ClaudeSettingsMode, CodergenSection, HookConfig,
    Manifest, QualitySection, ResolvedManifest,
};
pub use resolution::{resolve, ResolutionError};
pub use trust::{
    add_trust, is_trusted, list_trusted, prompt_and_add, remove_trust, TrustEntry, TrustError,
};
