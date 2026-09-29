//! Provider-aligned tool profiles.
//!
//! Different LLM providers have different preferences for which tools to expose
//! and how. A [`ToolProfile`] configures which tools are registered and any
//! provider-specific behavior.

use crate::builtin::*;
use crate::tool::ToolRegistry;

/// A profile that selects which tools to register for a given LLM provider.
pub struct ToolProfile {
    pub name: String,
    pub tools: Vec<String>,
}

impl ToolProfile {
    /// Profile for Anthropic models (Claude).
    /// Uses edit_file with old_string/new_string pattern.
    pub fn anthropic() -> Self {
        Self {
            name: "anthropic".into(),
            tools: vec![
                "read_file".into(),
                "write_file".into(),
                "edit_file".into(),
                "shell".into(),
                "grep".into(),
                "glob".into(),
            ],
        }
    }

    /// Profile for OpenAI models.
    /// Uses the same tool set for now; may diverge in the future.
    pub fn openai() -> Self {
        Self {
            name: "openai".into(),
            tools: vec![
                "read_file".into(),
                "write_file".into(),
                "edit_file".into(),
                "shell".into(),
                "grep".into(),
                "glob".into(),
            ],
        }
    }

    /// Profile for Google Gemini models.
    pub fn gemini() -> Self {
        Self {
            name: "gemini".into(),
            tools: vec![
                "read_file".into(),
                "write_file".into(),
                "edit_file".into(),
                "shell".into(),
                "grep".into(),
                "glob".into(),
            ],
        }
    }

    /// Build a [`ToolRegistry`] from this profile using the built-in tools.
    pub fn build_registry(&self) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        for name in &self.tools {
            match name.as_str() {
                "read_file" => registry.register(ReadFileTool),
                "write_file" => registry.register(WriteFileTool),
                "edit_file" => registry.register(EditFileTool),
                "shell" => registry.register(ShellTool),
                "grep" => registry.register(GrepTool),
                "glob" => registry.register(GlobTool),
                _ => {} // skip unknown tool names
            }
        }
        registry
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_correct_tool_names() {
        let profile = ToolProfile::anthropic();
        let registry = profile.build_registry();
        let names = registry.names();
        assert!(names.contains(&"read_file".to_string()));
        assert!(names.contains(&"write_file".to_string()));
        assert!(names.contains(&"edit_file".to_string()));
        assert!(names.contains(&"shell".to_string()));
        assert!(names.contains(&"grep".to_string()));
        assert!(names.contains(&"glob".to_string()));
    }
}
