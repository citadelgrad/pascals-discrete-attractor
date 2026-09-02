pub mod decompose;
pub mod generate;
pub mod info;
pub mod init;
pub mod launch;
pub mod plan;
pub mod run;
pub mod scaffold;
pub mod validate;

pub use decompose::{cmd_decompose, validate_decomposition};
pub use generate::{cmd_generate, cmd_generate_dir};
pub use info::cmd_info;
pub use init::{cmd_init, InitOpts};
pub use launch::cmd_launch;
pub use plan::cmd_plan;
pub use run::{cmd_run, cmd_run_dir, CodergenClaudeCliOpts};
pub use scaffold::cmd_scaffold;
pub use validate::cmd_validate;

/// Parse `dot_content`, fill in any missing `llm_provider` on runtime nodes
/// with `default_provider`, and re-serialize. Returns the normalized DOT
/// source plus the sorted list of node ids that were defaulted.
///
/// Shared by `cmd_scaffold` and `cmd_generate` so neither one ever writes a
/// pipeline file whose runtime nodes silently depend on an implicit
/// llm_provider default.
pub(crate) fn normalize_provider_defaults(
    dot_content: &str,
    default_provider: &str,
) -> anyhow::Result<(String, Vec<String>)> {
    let mut dot_graph = attractor_dot::parse(dot_content)?;
    let defaulted =
        attractor_pipeline::fill_missing_llm_providers(&mut dot_graph, default_provider);
    let normalized = attractor_dot::to_dot_string(&dot_graph);
    Ok((normalized, defaulted))
}
