//! Typed, immutable controls for one compiled pipeline run.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

use attractor_quality::{
    ClaudeCodergenConfig, ClaudeSettingSource, ClaudeSettingsMode, ResolutionError,
    ResolvedManifest,
};
use serde_json::Value;

use crate::engine::DEFAULT_MAX_BUDGET_USD;
use crate::execution_plan::ExecutionPlan;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigurationSource {
    Caller,
    Manifest,
    Graph,
    BuiltIn,
}

#[derive(Clone, PartialEq)]
pub struct ResolvedValue<T> {
    value: T,
    source: ConfigurationSource,
    redact_debug: bool,
}

impl<T> ResolvedValue<T> {
    fn new(value: T, source: ConfigurationSource) -> Self {
        Self {
            value,
            source,
            redact_debug: false,
        }
    }

    fn sensitive(value: T, source: ConfigurationSource) -> Self {
        Self {
            value,
            source,
            redact_debug: true,
        }
    }

    pub fn value(&self) -> &T {
        &self.value
    }

    pub fn source(&self) -> ConfigurationSource {
        self.source
    }
}

impl<T: fmt::Debug> fmt::Debug for ResolvedValue<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = f.debug_struct("ResolvedValue");
        if self.redact_debug {
            debug.field("value", &"<redacted>");
        } else {
            debug.field("value", &self.value);
        }
        debug.field("source", &self.source).finish()
    }
}

#[derive(Clone, Default)]
pub struct ClaudeExecutionOptions {
    pub settings_mode: Option<ClaudeSettingsMode>,
    pub setting_sources: Option<Vec<ClaudeSettingSource>>,
    pub settings: Option<String>,
    pub tools: Option<String>,
    pub agents: Option<String>,
    pub plugin_dirs: Option<Vec<PathBuf>>,
    pub mcp_config: Option<String>,
}

impl fmt::Debug for ClaudeExecutionOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaudeExecutionOptions")
            .field("settings_mode", &self.settings_mode)
            .field("setting_sources", &self.setting_sources)
            .field("settings", &self.settings.as_ref().map(|_| "<redacted>"))
            .field("tools", &self.tools.as_ref().map(|_| "<redacted>"))
            .field("agents", &self.agents.as_ref().map(|_| "<redacted>"))
            .field(
                "plugin_dirs",
                &self.plugin_dirs.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "mcp_config",
                &self.mcp_config.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

#[derive(Debug, Clone, Default)]
pub struct ExecutionOptions {
    pub dry_run: Option<bool>,
    pub max_steps: Option<u64>,
    pub max_budget_usd: Option<f64>,
    pub workdir: Option<PathBuf>,
    pub quality_disabled: Option<bool>,
    pub quality_max_fix_iterations: Option<u32>,
    /// Skill directories. `Some` replaces the manifest list, even when empty.
    pub skills: Option<Vec<PathBuf>>,
    /// pi extensions. `Some` replaces the manifest list, even when empty.
    pub pi_extensions: Option<Vec<PathBuf>>,
    /// pi prompt templates. `Some` replaces the manifest list, even when empty.
    pub pi_prompt_templates: Option<Vec<PathBuf>>,
    pub claude: ClaudeExecutionOptions,
}

#[derive(Clone)]
pub struct ResolvedClaudeConfig {
    settings_mode: ResolvedValue<ClaudeSettingsMode>,
    setting_sources: ResolvedValue<Vec<ClaudeSettingSource>>,
    settings: ResolvedValue<Option<String>>,
    tools: ResolvedValue<Option<String>>,
    agents: ResolvedValue<Option<String>>,
    plugin_dirs: ResolvedValue<Vec<PathBuf>>,
    mcp_config: ResolvedValue<Option<String>>,
}

impl ResolvedClaudeConfig {
    pub fn settings_mode(&self) -> &ResolvedValue<ClaudeSettingsMode> {
        &self.settings_mode
    }
    pub fn setting_sources(&self) -> &ResolvedValue<Vec<ClaudeSettingSource>> {
        &self.setting_sources
    }
    pub fn settings(&self) -> &ResolvedValue<Option<String>> {
        &self.settings
    }
    pub fn tools(&self) -> &ResolvedValue<Option<String>> {
        &self.tools
    }
    pub fn agents(&self) -> &ResolvedValue<Option<String>> {
        &self.agents
    }
    pub fn plugin_dirs(&self) -> &ResolvedValue<Vec<PathBuf>> {
        &self.plugin_dirs
    }
    pub fn mcp_config(&self) -> &ResolvedValue<Option<String>> {
        &self.mcp_config
    }
}

impl fmt::Debug for ResolvedClaudeConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedClaudeConfig")
            .field("settings_mode", &self.settings_mode)
            .field("setting_sources", &self.setting_sources)
            .field(
                "settings",
                &format_args!("<redacted:{:?}>", self.settings.source()),
            )
            .field(
                "tools",
                &format_args!("<redacted:{:?}>", self.tools.source()),
            )
            .field(
                "agents",
                &format_args!("<redacted:{:?}>", self.agents.source()),
            )
            .field(
                "plugin_dirs",
                &format_args!("<redacted:{:?}>", self.plugin_dirs.source()),
            )
            .field(
                "mcp_config",
                &format_args!("<redacted:{:?}>", self.mcp_config.source()),
            )
            .finish()
    }
}

#[derive(Clone)]
pub struct ResolvedConfig {
    dry_run: ResolvedValue<bool>,
    max_steps: ResolvedValue<u64>,
    max_budget_usd: ResolvedValue<f64>,
    workdir: ResolvedValue<PathBuf>,
    quality_disabled: ResolvedValue<bool>,
    quality_max_fix_iterations: HashMap<String, ResolvedValue<u32>>,
    claude: ResolvedClaudeConfig,
    skills: ResolvedValue<Vec<PathBuf>>,
    pi_extensions: ResolvedValue<Vec<PathBuf>>,
    pi_prompt_templates: ResolvedValue<Vec<PathBuf>>,
    manifest: Option<ResolvedManifest>,
}

impl ResolvedConfig {
    pub fn dry_run(&self) -> &ResolvedValue<bool> {
        &self.dry_run
    }

    pub fn max_steps(&self) -> &ResolvedValue<u64> {
        &self.max_steps
    }

    pub fn max_budget_usd(&self) -> &ResolvedValue<f64> {
        &self.max_budget_usd
    }

    pub fn workdir(&self) -> &ResolvedValue<PathBuf> {
        &self.workdir
    }

    pub fn quality_disabled(&self) -> &ResolvedValue<bool> {
        &self.quality_disabled
    }

    pub fn quality_max_fix_iterations(&self, node_id: &str) -> &ResolvedValue<u32> {
        self.quality_max_fix_iterations
            .get(node_id)
            .unwrap_or_else(|| panic!("node '{node_id}' is not a configured quality node"))
    }

    pub fn claude(&self) -> &ResolvedClaudeConfig {
        &self.claude
    }

    pub fn skills(&self) -> &ResolvedValue<Vec<PathBuf>> {
        &self.skills
    }

    pub fn pi_extensions(&self) -> &ResolvedValue<Vec<PathBuf>> {
        &self.pi_extensions
    }

    pub fn pi_prompt_templates(&self) -> &ResolvedValue<Vec<PathBuf>> {
        &self.pi_prompt_templates
    }

    /// The cached raw manifest is intentionally unavailable to API consumers.
    ///
    /// ```compile_fail
    /// use attractor_pipeline::ResolvedConfig;
    ///
    /// fn raw_manifest(config: &ResolvedConfig) {
    ///     let _ = config.manifest();
    /// }
    /// ```
    pub(crate) fn manifest(&self) -> Option<&ResolvedManifest> {
        self.manifest.as_ref()
    }
}

impl fmt::Debug for ResolvedConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedConfig")
            .field("dry_run", &self.dry_run)
            .field("max_steps", &self.max_steps)
            .field("max_budget_usd", &self.max_budget_usd)
            .field("workdir", &self.workdir)
            .field("quality_disabled", &self.quality_disabled)
            .field(
                "quality_max_fix_iterations",
                &self.quality_max_fix_iterations,
            )
            .field("claude", &self.claude)
            .field("skills", &self.skills)
            .field("pi_extensions", &self.pi_extensions)
            .field("pi_prompt_templates", &self.pi_prompt_templates)
            .field("manifest", &self.manifest.as_ref().map(|m| &m.path))
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigurationError {
    #[error("graph attribute '{0}' is reserved for typed run configuration")]
    ReservedGraphAttribute(String),
    #[error("invalid run configuration: {0}")]
    Invalid(String),
    #[error("failed to resolve workdir {path}: {source}")]
    Workdir {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to resolve pas.toml: {0}")]
    Manifest(String),
}

#[derive(Clone)]
pub struct RunConfiguration {
    plan: ExecutionPlan,
    controls: ResolvedConfig,
    graph_context_defaults: HashMap<String, Value>,
}

impl fmt::Debug for RunConfiguration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunConfiguration")
            .field("plan", &self.plan)
            .field("controls", &self.controls)
            .field("graph_context_defaults", &self.graph_context_defaults)
            .finish()
    }
}

impl RunConfiguration {
    pub fn prepare(
        plan: ExecutionPlan,
        options: ExecutionOptions,
    ) -> Result<Self, ConfigurationError> {
        let mut graph_context_defaults = HashMap::new();
        let mut reserved = plan
            .graph()
            .attrs
            .keys()
            .filter(|key| is_reserved_key(key))
            .cloned()
            .collect::<Vec<_>>();
        reserved.sort();
        if let Some(key) = reserved.into_iter().next() {
            return Err(ConfigurationError::ReservedGraphAttribute(key));
        }
        for (key, value) in &plan.graph().attrs {
            graph_context_defaults.insert(key.clone(), attr_to_json(value));
        }

        let workdir_is_caller = options.workdir.is_some();
        let workdir = options
            .workdir
            .clone()
            .unwrap_or(
                std::env::current_dir().map_err(|source| ConfigurationError::Workdir {
                    path: PathBuf::from("."),
                    source,
                })?,
            );
        let workdir = workdir
            .canonicalize()
            .map_err(|source| ConfigurationError::Workdir {
                path: workdir.clone(),
                source,
            })?;

        let manifest = match attractor_quality::resolve(&workdir) {
            Ok(manifest) => Some(manifest),
            Err(ResolutionError::NotFound) => None,
            Err(error) => return Err(ConfigurationError::Manifest(error.to_string())),
        };
        let manifest_claude = manifest
            .as_ref()
            .and_then(|resolved| resolved.manifest.codergen.as_ref())
            .and_then(|codergen| codergen.claude.as_ref());

        let manifest_codergen = manifest
            .as_ref()
            .and_then(|resolved| resolved.manifest.codergen.as_ref());
        let manifest_pi = manifest_codergen.and_then(|codergen| codergen.pi.as_ref());
        let manifest_dir = manifest
            .as_ref()
            .and_then(|resolved| resolved.path.parent())
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let skills = resolve_path_list(
            &options.skills,
            manifest_codergen.map_or(&[][..], |c| c.skills.as_slice()),
            &manifest_dir,
        );
        let pi_extensions = resolve_path_list(
            &options.pi_extensions,
            manifest_pi.map_or(&[][..], |c| c.extensions.as_slice()),
            &manifest_dir,
        );
        let pi_prompt_templates = resolve_path_list(
            &options.pi_prompt_templates,
            manifest_pi.map_or(&[][..], |c| c.prompt_templates.as_slice()),
            &manifest_dir,
        );

        let caller = ConfigurationSource::Caller;
        let built_in = ConfigurationSource::BuiltIn;
        let controls = ResolvedConfig {
            dry_run: ResolvedValue::new(
                options.dry_run.unwrap_or(false),
                if options.dry_run.is_some() {
                    caller
                } else {
                    built_in
                },
            ),
            max_steps: ResolvedValue::new(
                options.max_steps.unwrap_or(200),
                if options.max_steps.is_some() {
                    caller
                } else {
                    built_in
                },
            ),
            max_budget_usd: ResolvedValue::new(
                options.max_budget_usd.unwrap_or(DEFAULT_MAX_BUDGET_USD),
                if options.max_budget_usd.is_some() {
                    caller
                } else {
                    built_in
                },
            ),
            workdir: ResolvedValue::new(workdir, if workdir_is_caller { caller } else { built_in }),
            quality_disabled: ResolvedValue::new(
                options.quality_disabled.unwrap_or(false),
                if options.quality_disabled.is_some() {
                    caller
                } else {
                    built_in
                },
            ),
            quality_max_fix_iterations: resolve_quality_limits(&plan, &options, manifest.as_ref())?,
            claude: resolve_claude(&options.claude, manifest_claude, manifest.as_ref())?,
            skills,
            pi_extensions,
            pi_prompt_templates,
            manifest,
        };

        validate_skills(&controls.skills)?;
        validate_existing("pi extension", &controls.pi_extensions)?;
        validate_existing("pi prompt template", &controls.pi_prompt_templates)?;

        if *controls.max_steps.value() == 0 {
            return Err(ConfigurationError::Invalid(
                "max_steps must be greater than zero".into(),
            ));
        }
        let budget = *controls.max_budget_usd.value();
        if !budget.is_finite() || budget < 0.0 {
            return Err(ConfigurationError::Invalid(
                "max_budget_usd must be finite and non-negative".into(),
            ));
        }

        Ok(Self {
            plan,
            controls,
            graph_context_defaults,
        })
    }

    pub fn plan(&self) -> &ExecutionPlan {
        &self.plan
    }

    pub fn controls(&self) -> &ResolvedConfig {
        &self.controls
    }

    pub fn graph_context_defaults(&self) -> &HashMap<String, Value> {
        &self.graph_context_defaults
    }

    /// The `pas.toml` and trust hash to check before pi loads the extensions
    /// it names (spec C7). `None` unless the extension list came from
    /// `pas.toml` and is not empty; flag lists need no trust.
    ///
    /// The hash is BLAKE3 over the `pas.toml` bytes, then the bytes of each
    /// extension in list order. A directory adds its files in sorted
    /// relative-path order. The bytes are read now, not at `prepare`, so the
    /// hash covers what pi will load.
    pub fn manifest_extension_trust(
        &self,
    ) -> Result<Option<ManifestExtensionTrust>, ConfigurationError> {
        let extensions = self.controls.pi_extensions();
        if extensions.source() != ConfigurationSource::Manifest || extensions.value().is_empty() {
            return Ok(None);
        }
        let Some(manifest) = self.controls.manifest() else {
            return Ok(None);
        };
        let unreadable = |path: &Path, error: std::io::Error| {
            ConfigurationError::Invalid(format!(
                "cannot read {} for the trust check: {error}",
                path.display()
            ))
        };
        let mut hasher = blake3::Hasher::new();
        let bytes = std::fs::read(&manifest.path).map_err(|e| unreadable(&manifest.path, e))?;
        hasher.update(&bytes);
        for extension in extensions.value() {
            hash_extension(&mut hasher, extension, &unreadable)?;
        }
        Ok(Some(ManifestExtensionTrust {
            manifest_path: manifest.path.clone(),
            hash: hasher.finalize().to_hex().to_string(),
        }))
    }
}

/// A `pas.toml` that names pi extensions, with the hash to trust (spec C7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestExtensionTrust {
    pub manifest_path: PathBuf,
    pub hash: String,
}

fn hash_extension(
    hasher: &mut blake3::Hasher,
    path: &Path,
    unreadable: &dyn Fn(&Path, std::io::Error) -> ConfigurationError,
) -> Result<(), ConfigurationError> {
    let meta = std::fs::metadata(path).map_err(|e| unreadable(path, e))?;
    if !meta.is_dir() {
        hasher.update(&std::fs::read(path).map_err(|e| unreadable(path, e))?);
        return Ok(());
    }
    let mut files = Vec::new();
    collect_files(path, Path::new(""), &mut files, unreadable)?;
    files.sort();
    for relative in files {
        let full = path.join(&relative);
        hasher.update(&std::fs::read(&full).map_err(|e| unreadable(&full, e))?);
    }
    Ok(())
}

/// Collect regular files under `root/relative`. A symbolic link inside an
/// extension directory is an error, never followed.
fn collect_files(
    root: &Path,
    relative: &Path,
    out: &mut Vec<PathBuf>,
    unreadable: &dyn Fn(&Path, std::io::Error) -> ConfigurationError,
) -> Result<(), ConfigurationError> {
    let dir = root.join(relative);
    for entry in std::fs::read_dir(&dir).map_err(|e| unreadable(&dir, e))? {
        let entry = entry.map_err(|e| unreadable(&dir, e))?;
        let file_type = entry
            .file_type()
            .map_err(|e| unreadable(&entry.path(), e))?;
        let child = relative.join(entry.file_name());
        if file_type.is_symlink() {
            return Err(ConfigurationError::Invalid(format!(
                "pi extension directory contains a symbolic link: {}",
                entry.path().display()
            )));
        }
        if file_type.is_dir() {
            collect_files(root, &child, out, unreadable)?;
        } else {
            out.push(child);
        }
    }
    Ok(())
}

fn resolve_quality_limits(
    plan: &ExecutionPlan,
    options: &ExecutionOptions,
    manifest: Option<&ResolvedManifest>,
) -> Result<HashMap<String, ResolvedValue<u32>>, ConfigurationError> {
    if options.quality_max_fix_iterations == Some(0) {
        return Err(ConfigurationError::Invalid(
            "quality_max_fix_iterations must be greater than zero".into(),
        ));
    }
    let manifest_value = manifest
        .and_then(|resolved| resolved.manifest.quality.as_ref())
        .and_then(|quality| quality.max_fix_iterations);
    if manifest_value == Some(0) {
        return Err(ConfigurationError::Invalid(
            "[quality].max_fix_iterations must be greater than zero".into(),
        ));
    }

    let mut result = HashMap::new();
    for resolved in plan
        .all_nodes()
        .filter(|node| node.handler == crate::execution_plan::HandlerIdentity::Quality)
    {
        let source_node = plan
            .source_node(&resolved.node_id)
            .expect("compiled node has source");
        let graph_value = match source_node.raw_attrs.get("max_fix_iterations") {
            None => None,
            Some(attractor_dot::AttributeValue::Integer(value)) => Some(
                u32::try_from(*value)
                    .ok()
                    .filter(|value| *value > 0)
                    .ok_or_else(|| {
                        ConfigurationError::Invalid(format!(
                            "quality node '{}' max_fix_iterations must be a positive integer no greater than {}",
                            resolved.node_id,
                            u32::MAX
                        ))
                    })?,
            ),
            Some(_) => {
                return Err(ConfigurationError::Invalid(format!(
                    "quality node '{}' max_fix_iterations must be a positive integer",
                    resolved.node_id
                )));
            }
        };
        let value = if let Some(value) = options.quality_max_fix_iterations {
            ResolvedValue::new(value, ConfigurationSource::Caller)
        } else if let Some(value) = manifest_value {
            ResolvedValue::new(value, ConfigurationSource::Manifest)
        } else if let Some(value) = graph_value {
            ResolvedValue::new(value, ConfigurationSource::Graph)
        } else {
            ResolvedValue::new(3, ConfigurationSource::BuiltIn)
        };
        result.insert(resolved.node_id.clone(), value);
    }
    Ok(result)
}

fn resolved_sensitive_optional<T: Clone>(
    caller: &Option<T>,
    manifest: Option<&T>,
) -> ResolvedValue<Option<T>> {
    if let Some(value) = caller {
        ResolvedValue::sensitive(Some(value.clone()), ConfigurationSource::Caller)
    } else if let Some(value) = manifest {
        ResolvedValue::sensitive(Some(value.clone()), ConfigurationSource::Manifest)
    } else {
        ResolvedValue::sensitive(None, ConfigurationSource::BuiltIn)
    }
}

fn resolve_claude(
    options: &ClaudeExecutionOptions,
    manifest: Option<&ClaudeCodergenConfig>,
    resolved_manifest: Option<&ResolvedManifest>,
) -> Result<ResolvedClaudeConfig, ConfigurationError> {
    let settings_mode = if let Some(value) = options.settings_mode {
        ResolvedValue::new(value, ConfigurationSource::Caller)
    } else if let Some(value) = manifest.and_then(|config| config.settings_mode) {
        ResolvedValue::new(value, ConfigurationSource::Manifest)
    } else {
        ResolvedValue::new(
            ClaudeSettingsMode::SubscriptionBare,
            ConfigurationSource::BuiltIn,
        )
    };
    let setting_sources = if let Some(value) = &options.setting_sources {
        ResolvedValue::new(value.clone(), ConfigurationSource::Caller)
    } else if let Some(value) = manifest.and_then(|config| config.setting_sources.as_ref()) {
        ResolvedValue::new(value.clone(), ConfigurationSource::Manifest)
    } else {
        ResolvedValue::new(Vec::new(), ConfigurationSource::BuiltIn)
    };
    if *settings_mode.value() == ClaudeSettingsMode::Inherit && setting_sources.value().is_empty() {
        return Err(ConfigurationError::Invalid(
            "Claude settings_mode=inherit requires explicit setting_sources".into(),
        ));
    }

    let plugin_dirs = if let Some(value) = &options.plugin_dirs {
        ResolvedValue::sensitive(value.clone(), ConfigurationSource::Caller)
    } else if let Some(config) = manifest.filter(|config| !config.plugin_dirs.is_empty()) {
        let manifest_dir = resolved_manifest
            .and_then(|resolved| resolved.path.parent())
            .unwrap_or_else(|| std::path::Path::new("."));
        let values = config
            .plugin_dirs
            .iter()
            .map(|path| {
                if path.is_absolute() {
                    path.clone()
                } else {
                    manifest_dir.join(path)
                }
            })
            .collect();
        ResolvedValue::sensitive(values, ConfigurationSource::Manifest)
    } else {
        ResolvedValue::sensitive(Vec::new(), ConfigurationSource::BuiltIn)
    };

    Ok(ResolvedClaudeConfig {
        settings_mode,
        setting_sources,
        settings: resolved_sensitive_optional(
            &options.settings,
            manifest.and_then(|c| c.settings_json.as_ref()),
        ),
        tools: resolved_sensitive_optional(&options.tools, manifest.and_then(|c| c.tools.as_ref())),
        agents: resolved_sensitive_optional(
            &options.agents,
            manifest.and_then(|c| c.agents_json.as_ref()),
        ),
        plugin_dirs,
        mcp_config: resolved_sensitive_optional(
            &options.mcp_config,
            manifest.and_then(|c| c.mcp_config_json.as_ref()),
        ),
    })
}

/// Caller entries are kept as given (flag paths are relative to the process
/// directory); manifest entries are joined to the `pas.toml` directory. Paths
/// are not canonicalized so an absolute path stays unchanged and a symbolic
/// link entry keeps its own name.
fn resolve_path_list(
    caller: &Option<Vec<PathBuf>>,
    manifest: &[PathBuf],
    manifest_dir: &Path,
) -> ResolvedValue<Vec<PathBuf>> {
    if let Some(value) = caller {
        ResolvedValue::new(value.clone(), ConfigurationSource::Caller)
    } else if !manifest.is_empty() {
        let values = manifest
            .iter()
            .map(|path| {
                if path.is_absolute() {
                    path.clone()
                } else {
                    manifest_dir.join(path)
                }
            })
            .collect();
        ResolvedValue::new(values, ConfigurationSource::Manifest)
    } else {
        ResolvedValue::new(Vec::new(), ConfigurationSource::BuiltIn)
    }
}

fn invalid_path(
    kind: &str,
    path: &Path,
    source: ConfigurationSource,
    problem: &str,
) -> ConfigurationError {
    ConfigurationError::Invalid(format!(
        "{kind} path '{}' (from {source:?}) {problem}",
        path.display()
    ))
}

/// `metadata` follows links, so a broken link counts as missing.
fn validate_existing(
    kind: &str,
    list: &ResolvedValue<Vec<PathBuf>>,
) -> Result<(), ConfigurationError> {
    for path in list.value() {
        if std::fs::metadata(path).is_err() {
            return Err(invalid_path(kind, path, list.source(), "does not exist"));
        }
    }
    Ok(())
}

fn validate_skills(list: &ResolvedValue<Vec<PathBuf>>) -> Result<(), ConfigurationError> {
    const KIND: &str = "skill";
    let source = list.source();
    let mut names: HashMap<std::ffi::OsString, &Path> = HashMap::new();
    for path in list.value() {
        let metadata = std::fs::metadata(path)
            .map_err(|_| invalid_path(KIND, path, source, "does not exist"))?;
        if !metadata.is_dir() {
            return Err(invalid_path(KIND, path, source, "is not a directory"));
        }
        let skill_md = std::fs::metadata(path.join("SKILL.md"));
        if !skill_md.map(|m| m.is_file()).unwrap_or(false) {
            return Err(invalid_path(KIND, path, source, "has no SKILL.md"));
        }
        if let Some(name) = path.file_name() {
            if let Some(first) = names.insert(name.to_os_string(), path) {
                return Err(ConfigurationError::Invalid(format!(
                    "skill paths '{}' and '{}' (from {source:?}) have the same directory name",
                    first.display(),
                    path.display()
                )));
            }
        }
        reject_inner_links(path, source)?;
    }
    Ok(())
}

fn reject_inner_links(dir: &Path, source: ConfigurationSource) -> Result<(), ConfigurationError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|error| invalid_path("skill", dir, source, &format!("cannot be read: {error}")))?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            invalid_path("skill", dir, source, &format!("cannot be read: {error}"))
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|error| {
            invalid_path("skill", &path, source, &format!("cannot be read: {error}"))
        })?;
        if file_type.is_symlink() {
            return Err(invalid_path(
                "skill",
                &path,
                source,
                "is a symbolic link inside a skill directory",
            ));
        }
        if file_type.is_dir() {
            reject_inner_links(&path, source)?;
        }
    }
    Ok(())
}

pub fn is_reserved_key(key: &str) -> bool {
    matches!(
        key,
        "dry_run"
            | "workdir"
            | "max_steps"
            | "max_budget_usd"
            | "quality_disabled"
            | "quality_max_fix_iterations"
            | "outcome"
            | "preferred_label"
    ) || key.starts_with("codergen.claude.")
        || key.starts_with("codergen.pi.")
        || key == "codergen.skills"
        || key.starts_with("__pas.")
        || key.starts_with("__pas::")
}

fn attr_to_json(value: &attractor_dot::AttributeValue) -> Value {
    match value {
        attractor_dot::AttributeValue::String(value) => Value::String(value.clone()),
        attractor_dot::AttributeValue::Integer(value) => serde_json::json!(value),
        attractor_dot::AttributeValue::Float(value) => serde_json::json!(value),
        attractor_dot::AttributeValue::Boolean(value) => Value::Bool(*value),
        attractor_dot::AttributeValue::Duration(value) => serde_json::json!(value.as_millis()),
    }
}

#[cfg(test)]
mod tests {
    use super::is_reserved_key;

    #[test]
    fn resource_list_keys_are_reserved_and_other_codergen_keys_are_not() {
        assert!(is_reserved_key("codergen.skills"));
        assert!(is_reserved_key("codergen.pi.extensions"));
        assert!(is_reserved_key("codergen.pi.prompt_templates"));
        assert!(!is_reserved_key("codergen.other"));
        assert!(!is_reserved_key("codergen.skills_note"));
    }
}
