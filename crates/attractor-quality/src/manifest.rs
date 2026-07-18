use serde::{Deserialize, Deserializer};
use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub project: ProjectSection,
    pub toolchain: Option<ToolchainSection>,
    pub quality: Option<QualitySection>,
    pub codergen: Option<CodergenSection>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProjectSection {
    pub name: String,
    pub version: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolchainSection {
    pub language: String,
    pub version: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QualitySection {
    pub stages: Vec<String>,
    pub max_fix_iterations: Option<u32>,
    #[serde(default)]
    pub hooks: HashMap<String, HookConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HookConfig {
    pub cmd: Option<String>,
    pub cmd_argv: Option<Vec<String>>,
    pub timeout_secs: Option<u64>,
    pub allow_failure: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CodergenSection {
    pub claude: Option<ClaudeCodergenConfig>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ClaudeCodergenConfig {
    pub settings_mode: Option<ClaudeSettingsMode>,
    pub setting_sources: Option<Vec<ClaudeSettingSource>>,
    #[serde(default, deserialize_with = "deserialize_optional_json_string")]
    pub settings_json: Option<String>,
    pub tools: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_json_string")]
    pub agents_json: Option<String>,
    #[serde(default)]
    pub plugin_dirs: Vec<PathBuf>,
    #[serde(default, deserialize_with = "deserialize_optional_json_string")]
    pub mcp_config_json: Option<String>,
}

fn deserialize_optional_json_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    if let Some(json) = &value {
        serde_json::from_str::<serde_json::Value>(json).map_err(serde::de::Error::custom)?;
    }
    Ok(value)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClaudeSettingsMode {
    #[default]
    SubscriptionBare,
    StrictBare,
    Inherit,
}

impl FromStr for ClaudeSettingsMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.replace('-', "_").to_ascii_lowercase().as_str() {
            "subscription_bare" => Ok(Self::SubscriptionBare),
            "strict_bare" => Ok(Self::StrictBare),
            "inherit" => Ok(Self::Inherit),
            other => Err(format!(
                "invalid Claude settings mode '{other}' (expected subscription_bare, strict_bare, or inherit)"
            )),
        }
    }
}

impl<'de> Deserialize<'de> for ClaudeSettingsMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeSettingSource {
    User,
    Project,
    Local,
}

impl ClaudeSettingSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Local => "local",
        }
    }
}

impl FromStr for ClaudeSettingSource {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "user" => Ok(Self::User),
            "project" => Ok(Self::Project),
            "local" => Ok(Self::Local),
            other => Err(format!(
                "invalid Claude setting source '{other}' (expected user, project, or local)"
            )),
        }
    }
}

impl<'de> Deserialize<'de> for ClaudeSettingSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedManifest {
    pub manifest: Manifest,
    pub path: PathBuf,
    pub blake3_hash: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_manifest(toml: &str) -> Result<Manifest, toml::de::Error> {
        toml::from_str(toml)
    }

    #[test]
    fn codergen_claude_config_is_optional() {
        let manifest = parse_manifest(
            r#"
[project]
name = "test"
"#,
        )
        .unwrap();

        assert!(manifest.codergen.is_none());
    }

    #[test]
    fn parses_subscription_bare_mode() {
        let manifest = parse_manifest(
            r#"
[project]
name = "test"

[codergen.claude]
settings_mode = "subscription_bare"
tools = "Read,Edit"
"#,
        )
        .unwrap();

        let claude = manifest.codergen.unwrap().claude.unwrap();
        assert_eq!(
            claude.settings_mode,
            Some(ClaudeSettingsMode::SubscriptionBare)
        );
        assert_eq!(claude.tools.as_deref(), Some("Read,Edit"));
    }

    #[test]
    fn parses_hyphenated_strict_bare_mode() {
        let manifest = parse_manifest(
            r#"
[project]
name = "test"

[codergen.claude]
settings_mode = "strict-bare"
"#,
        )
        .unwrap();

        assert_eq!(
            manifest.codergen.unwrap().claude.unwrap().settings_mode,
            Some(ClaudeSettingsMode::StrictBare)
        );
    }

    #[test]
    fn parses_inherit_setting_sources() {
        let manifest = parse_manifest(
            r#"
[project]
name = "test"

[codergen.claude]
settings_mode = "inherit"
setting_sources = ["user", "project", "local"]
"#,
        )
        .unwrap();

        let sources = manifest
            .codergen
            .unwrap()
            .claude
            .unwrap()
            .setting_sources
            .unwrap();
        assert_eq!(
            sources,
            vec![
                ClaudeSettingSource::User,
                ClaudeSettingSource::Project,
                ClaudeSettingSource::Local,
            ]
        );
    }

    #[test]
    fn rejects_invalid_claude_settings_mode() {
        let err = parse_manifest(
            r#"
[project]
name = "test"

[codergen.claude]
settings_mode = "ambient"
"#,
        )
        .unwrap_err();

        assert!(err.to_string().contains("invalid Claude settings mode"));
    }

    #[test]
    fn rejects_invalid_claude_setting_source() {
        let err = parse_manifest(
            r#"
[project]
name = "test"

[codergen.claude]
settings_mode = "inherit"
setting_sources = ["global"]
"#,
        )
        .unwrap_err();

        assert!(err.to_string().contains("invalid Claude setting source"));
    }

    #[test]
    fn rejects_invalid_json_string_fields() {
        let err = parse_manifest(
            r#"
[project]
name = "test"

[codergen.claude]
settings_json = "not-json"
"#,
        )
        .unwrap_err();

        assert!(err.to_string().contains("expected ident"));
    }
}
