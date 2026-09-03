use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use globset::{Glob, GlobSetBuilder};
use regex::RegexBuilder;

use crate::environment::{DirEntry, ExecResult, ExecutionEnvironment, GrepOptions};

const ALLOWED_ENVIRONMENT_KEYS: &[&str] = &[
    "COMSPEC",
    "HOME",
    "LANG",
    "PATH",
    "PATHEXT",
    "SHELL",
    "SYSTEMROOT",
    "TEMP",
    "TERM",
    "TMP",
    "TMPDIR",
    "USER",
    "USERNAME",
];

/// An injectable program and argument prefix used to execute command strings.
///
/// The default runner is `sh -c` on Unix and `cmd /C` on Windows. Callers can
/// inject a platform-specific runner without changing the environment policy.
#[derive(Debug, Clone)]
pub struct CommandRunner {
    program: PathBuf,
    arguments: Vec<OsString>,
}

impl CommandRunner {
    pub fn new<I, S>(program: impl Into<PathBuf>, arguments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        Self {
            program: program.into(),
            arguments: arguments.into_iter().map(Into::into).collect(),
        }
    }

    pub fn platform_default() -> Self {
        #[cfg(windows)]
        return Self::new("cmd", ["/C"]);

        #[cfg(not(windows))]
        Self::new("sh", ["-c"])
    }

    fn command(&self, command: &str) -> tokio::process::Command {
        let mut configured = tokio::process::Command::new(&self.program);
        configured.args(&self.arguments).arg(command);
        configured
    }
}

#[derive(Debug, Clone)]
enum FilesystemPolicy {
    Host,
    RootConfined { canonical_root: PathBuf },
}

#[derive(Debug, Clone, Copy)]
enum PathIntent {
    Existing,
    MayNotExist,
}

#[derive(Debug, Clone)]
struct EnvironmentCore {
    working_dir: PathBuf,
    platform: String,
    runner: CommandRunner,
    filesystem: FilesystemPolicy,
}

/// Deliberately unrestricted access to the host filesystem.
///
/// Relative paths are resolved against `working_dir`; absolute paths and
/// parent traversal are intentionally permitted. Child processes still receive
/// only the documented environment allowlist.
#[derive(Debug, Clone)]
pub struct HostExecutionEnvironment {
    core: EnvironmentCore,
}

impl HostExecutionEnvironment {
    pub fn new(working_dir: impl Into<PathBuf>) -> Self {
        Self::with_command_runner(working_dir, CommandRunner::platform_default())
    }

    pub fn with_command_runner(working_dir: impl Into<PathBuf>, runner: CommandRunner) -> Self {
        let working_dir = working_dir.into();
        Self {
            core: EnvironmentCore {
                working_dir,
                platform: std::env::consts::OS.to_string(),
                runner,
                filesystem: FilesystemPolicy::Host,
            },
        }
    }

    pub fn current_dir() -> std::io::Result<Self> {
        Ok(Self::new(std::env::current_dir()?))
    }
}

/// Filesystem-tool access confined to one canonical project root.
///
/// This rejects parent traversal, outside absolute paths, and symlink escapes.
/// It does not claim to sandbox arbitrary commands: an injected command runner
/// can invoke host APIs outside the root unless the runner provides its own OS
/// confinement.
#[derive(Debug, Clone)]
pub struct RootConfinedExecutionEnvironment {
    core: EnvironmentCore,
}

impl RootConfinedExecutionEnvironment {
    pub fn new(root: impl AsRef<Path>) -> std::io::Result<Self> {
        Self::with_command_runner(root, CommandRunner::platform_default())
    }

    pub fn with_command_runner(
        root: impl AsRef<Path>,
        runner: CommandRunner,
    ) -> std::io::Result<Self> {
        let canonical_root = std::fs::canonicalize(root)?;
        if !canonical_root.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "confined execution root '{}' is not a directory",
                    canonical_root.display()
                ),
            ));
        }
        Ok(Self {
            core: EnvironmentCore {
                working_dir: canonical_root.clone(),
                platform: std::env::consts::OS.to_string(),
                runner,
                filesystem: FilesystemPolicy::RootConfined { canonical_root },
            },
        })
    }
}

impl EnvironmentCore {
    fn resolve(&self, path: &Path, intent: PathIntent) -> attractor_types::Result<PathBuf> {
        match &self.filesystem {
            FilesystemPolicy::Host => Ok(if path.is_absolute() {
                path.to_path_buf()
            } else {
                self.working_dir.join(path)
            }),
            FilesystemPolicy::RootConfined { canonical_root } => {
                resolve_confined(canonical_root, path, intent)
            }
        }
    }

    fn follows_directory_symlinks(&self) -> bool {
        matches!(self.filesystem, FilesystemPolicy::Host)
    }

    async fn read_file(&self, path: &Path) -> attractor_types::Result<String> {
        let resolved = self.resolve(path, PathIntent::Existing)?;
        Ok(tokio::fs::read_to_string(resolved).await?)
    }

    async fn write_file(&self, path: &Path, content: &str) -> attractor_types::Result<()> {
        let resolved = self.resolve(path, PathIntent::MayNotExist)?;
        if let Some(parent) = resolved.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        Ok(tokio::fs::write(resolved, content).await?)
    }

    async fn file_exists(&self, path: &Path) -> attractor_types::Result<bool> {
        let resolved = self.resolve(path, PathIntent::MayNotExist)?;
        Ok(tokio::fs::try_exists(resolved).await?)
    }

    async fn list_directory(
        &self,
        path: &Path,
        depth: usize,
    ) -> attractor_types::Result<Vec<DirEntry>> {
        let resolved = self.resolve(path, PathIntent::Existing)?;
        let mut entries = Vec::new();
        list_dir_recursive(
            &resolved,
            depth,
            self.follows_directory_symlinks(),
            &mut entries,
        )
        .await?;
        Ok(entries)
    }

    async fn exec_command(
        &self,
        command: &str,
        timeout_ms: u64,
        cwd: Option<&Path>,
        env_vars: Option<&HashMap<String, String>>,
    ) -> attractor_types::Result<ExecResult> {
        let work_dir = match cwd {
            Some(path) => self.resolve(path, PathIntent::Existing)?,
            None => self.working_dir.clone(),
        };
        let child_environment = child_environment(env_vars)?;

        let mut cmd = self.runner.command(command);
        cmd.current_dir(work_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .env_clear()
            .envs(child_environment);
        configure_process_group(&mut cmd);

        let start = tokio::time::Instant::now();
        let child = cmd.spawn()?;
        let mut process_group = ProcessGroupGuard::new(child.id());
        let timeout = std::time::Duration::from_millis(timeout_ms);

        match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(output) => {
                let output = output?;
                process_group.disarm();
                Ok(ExecResult {
                    stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                    stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                    exit_code: output.status.code().unwrap_or(-1),
                    timed_out: false,
                    duration_ms: start.elapsed().as_millis() as u64,
                })
            }
            Err(_) => Ok(ExecResult {
                stdout: String::new(),
                stderr: format!("Command timed out after {timeout_ms}ms"),
                exit_code: -1,
                timed_out: true,
                duration_ms: start.elapsed().as_millis() as u64,
            }),
        }
    }

    async fn grep(
        &self,
        pattern: &str,
        path: &Path,
        options: &GrepOptions,
    ) -> attractor_types::Result<String> {
        let resolved = self.resolve(path, PathIntent::Existing)?;
        let environment = child_environment(None)?;
        if let Ok(output) = try_ripgrep(pattern, &resolved, options, &environment).await {
            return Ok(output);
        }
        grep_with_regex(
            pattern,
            &resolved,
            options,
            self.follows_directory_symlinks(),
        )
        .await
    }

    async fn glob_files(
        &self,
        pattern: &str,
        base: &Path,
    ) -> attractor_types::Result<Vec<PathBuf>> {
        let resolved = self.resolve(base, PathIntent::Existing)?;
        let glob = Glob::new(pattern).map_err(|error| environment_error(error.to_string()))?;
        let mut builder = GlobSetBuilder::new();
        builder.add(glob);
        let set = builder
            .build()
            .map_err(|error| environment_error(error.to_string()))?;

        let mut matches = Vec::new();
        collect_glob_matches(
            &resolved,
            &resolved,
            &set,
            self.follows_directory_symlinks(),
            &mut matches,
        )
        .await?;
        matches.sort();
        Ok(matches)
    }
}

macro_rules! impl_execution_environment {
    ($environment:ty) => {
        #[async_trait]
        impl ExecutionEnvironment for $environment {
            async fn read_file(&self, path: &Path) -> attractor_types::Result<String> {
                self.core.read_file(path).await
            }

            async fn write_file(&self, path: &Path, content: &str) -> attractor_types::Result<()> {
                self.core.write_file(path, content).await
            }

            async fn file_exists(&self, path: &Path) -> attractor_types::Result<bool> {
                self.core.file_exists(path).await
            }

            async fn list_directory(
                &self,
                path: &Path,
                depth: usize,
            ) -> attractor_types::Result<Vec<DirEntry>> {
                self.core.list_directory(path, depth).await
            }

            async fn exec_command(
                &self,
                command: &str,
                timeout_ms: u64,
                cwd: Option<&Path>,
                env_vars: Option<&HashMap<String, String>>,
            ) -> attractor_types::Result<ExecResult> {
                self.core
                    .exec_command(command, timeout_ms, cwd, env_vars)
                    .await
            }

            async fn grep(
                &self,
                pattern: &str,
                path: &Path,
                options: &GrepOptions,
            ) -> attractor_types::Result<String> {
                self.core.grep(pattern, path, options).await
            }

            async fn glob_files(
                &self,
                pattern: &str,
                base: &Path,
            ) -> attractor_types::Result<Vec<PathBuf>> {
                self.core.glob_files(pattern, base).await
            }

            fn working_directory(&self) -> &Path {
                &self.core.working_dir
            }

            fn platform(&self) -> &str {
                &self.core.platform
            }
        }
    };
}

impl_execution_environment!(HostExecutionEnvironment);
impl_execution_environment!(RootConfinedExecutionEnvironment);

fn resolve_confined(
    canonical_root: &Path,
    path: &Path,
    intent: PathIntent,
) -> attractor_types::Result<PathBuf> {
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(environment_error(format!(
            "path '{}' contains forbidden parent traversal",
            path.display()
        )));
    }

    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        canonical_root.join(path)
    };
    if !candidate.starts_with(canonical_root) {
        return Err(outside_root_error(path, canonical_root));
    }

    let resolved = match intent {
        PathIntent::Existing => std::fs::canonicalize(&candidate)?,
        PathIntent::MayNotExist => canonicalize_allow_missing(&candidate)?,
    };
    if !resolved.starts_with(canonical_root) {
        return Err(outside_root_error(path, canonical_root));
    }
    Ok(resolved)
}

fn canonicalize_allow_missing(path: &Path) -> std::io::Result<PathBuf> {
    let mut existing = path;
    let mut missing = Vec::new();
    loop {
        match std::fs::symlink_metadata(existing) {
            Ok(_) => {
                let mut resolved = std::fs::canonicalize(existing)?;
                for component in missing.iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = existing.file_name().ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("no existing ancestor for '{}'", path.display()),
                    )
                })?;
                missing.push(name.to_os_string());
                existing = existing.parent().ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("no existing ancestor for '{}'", path.display()),
                    )
                })?;
            }
            Err(error) => return Err(error),
        }
    }
}

fn outside_root_error(path: &Path, root: &Path) -> attractor_types::AttractorError {
    environment_error(format!(
        "path '{}' resolves outside confined root '{}'",
        path.display(),
        root.display()
    ))
}

fn environment_error(message: String) -> attractor_types::AttractorError {
    attractor_types::AttractorError::ToolError {
        tool: "execution_environment".into(),
        message,
    }
}

fn environment_key_allowed(key: &str) -> bool {
    let normalized = key.to_ascii_uppercase();
    ALLOWED_ENVIRONMENT_KEYS.contains(&normalized.as_str()) || normalized.starts_with("LC_")
}

fn child_environment(
    overrides: Option<&HashMap<String, String>>,
) -> attractor_types::Result<HashMap<String, String>> {
    let mut environment = std::env::vars()
        .filter(|(key, _)| environment_key_allowed(key))
        .collect::<HashMap<_, _>>();

    if let Some(overrides) = overrides {
        let mut denied = overrides
            .keys()
            .filter(|key| !environment_key_allowed(key))
            .cloned()
            .collect::<Vec<_>>();
        denied.sort();
        if !denied.is_empty() {
            return Err(environment_error(format!(
                "child environment keys are not allowlisted: {}",
                denied.join(", ")
            )));
        }
        environment.extend(overrides.clone());
    }
    Ok(environment)
}

fn configure_process_group(command: &mut tokio::process::Command) {
    #[cfg(unix)]
    command.process_group(0);

    #[cfg(not(unix))]
    let _ = command;
}

struct ProcessGroupGuard {
    #[cfg(unix)]
    process_group: Option<libc::pid_t>,
}

impl ProcessGroupGuard {
    fn new(pid: Option<u32>) -> Self {
        #[cfg(unix)]
        {
            Self {
                process_group: pid.and_then(|pid| libc::pid_t::try_from(pid).ok()),
            }
        }

        #[cfg(not(unix))]
        {
            let _ = pid;
            Self {}
        }
    }

    fn disarm(&mut self) {
        #[cfg(unix)]
        {
            self.process_group = None;
        }
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(process_group) = self.process_group {
            // SAFETY: the child is placed in a fresh process group whose ID is
            // the child's PID. Cleanup is best-effort if it already exited.
            unsafe {
                libc::killpg(process_group, libc::SIGKILL);
            }
        }
    }
}

async fn list_dir_recursive(
    path: &Path,
    depth: usize,
    follow_symlinks: bool,
    entries: &mut Vec<DirEntry>,
) -> attractor_types::Result<()> {
    let mut read_dir = tokio::fs::read_dir(path).await?;
    while let Some(entry) = read_dir.next_entry().await? {
        let file_type = entry.file_type().await?;
        let metadata = if follow_symlinks {
            entry.metadata().await?
        } else {
            tokio::fs::symlink_metadata(entry.path()).await?
        };
        let is_dir = if file_type.is_symlink() && !follow_symlinks {
            false
        } else {
            metadata.is_dir()
        };
        entries.push(DirEntry {
            path: entry.path(),
            is_dir,
            size: metadata.len(),
        });
        if is_dir && depth > 1 {
            Box::pin(list_dir_recursive(
                &entry.path(),
                depth - 1,
                follow_symlinks,
                entries,
            ))
            .await?;
        }
    }
    Ok(())
}

async fn try_ripgrep(
    pattern: &str,
    path: &Path,
    options: &GrepOptions,
    environment: &HashMap<String, String>,
) -> std::result::Result<String, ()> {
    let mut args = vec!["--no-heading".to_string()];
    if options.case_insensitive {
        args.push("-i".to_string());
    }
    if options.include_line_numbers {
        args.push("-n".to_string());
    }
    if options.context_lines > 0 {
        args.push(format!("-C{}", options.context_lines));
    }
    if let Some(max) = options.max_results {
        args.push(format!("-m{max}"));
    }
    args.push("--".to_string());
    args.push(pattern.to_string());
    args.push(path.to_string_lossy().to_string());

    let output = tokio::process::Command::new("rg")
        .args(&args)
        .env_clear()
        .envs(environment)
        .output()
        .await
        .map_err(|_| ())?;
    if output.status.code() == Some(2) {
        return Err(());
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

async fn grep_with_regex(
    pattern: &str,
    path: &Path,
    options: &GrepOptions,
    follow_symlinks: bool,
) -> attractor_types::Result<String> {
    let regex = RegexBuilder::new(pattern)
        .case_insensitive(options.case_insensitive)
        .build()
        .map_err(|error| environment_error(error.to_string()))?;
    let mut results = Vec::new();
    let max = options.max_results.unwrap_or(usize::MAX);
    grep_path_recursive(&regex, path, options, follow_symlinks, &mut results, max).await?;
    Ok(results.join("\n"))
}

async fn grep_path_recursive(
    regex: &regex::Regex,
    path: &Path,
    options: &GrepOptions,
    follow_symlinks: bool,
    results: &mut Vec<String>,
    max: usize,
) -> attractor_types::Result<()> {
    if results.len() >= max {
        return Ok(());
    }

    let symlink_metadata = tokio::fs::symlink_metadata(path).await?;
    if symlink_metadata.file_type().is_symlink() && !follow_symlinks {
        return Ok(());
    }
    let metadata = if follow_symlinks {
        tokio::fs::metadata(path).await?
    } else {
        symlink_metadata
    };
    if metadata.is_file() {
        if let Ok(content) = tokio::fs::read_to_string(path).await {
            for (index, line) in content.lines().enumerate() {
                if results.len() >= max {
                    break;
                }
                if regex.is_match(line) {
                    if options.include_line_numbers {
                        results.push(format!("{}:{}:{}", path.display(), index + 1, line));
                    } else {
                        results.push(format!("{}:{line}", path.display()));
                    }
                }
            }
        }
    } else if metadata.is_dir() {
        let mut read_dir = tokio::fs::read_dir(path).await?;
        while let Some(entry) = read_dir.next_entry().await? {
            if results.len() >= max {
                break;
            }
            Box::pin(grep_path_recursive(
                regex,
                &entry.path(),
                options,
                follow_symlinks,
                results,
                max,
            ))
            .await?;
        }
    }
    Ok(())
}

async fn collect_glob_matches(
    base: &Path,
    current: &Path,
    set: &globset::GlobSet,
    follow_symlinks: bool,
    matches: &mut Vec<PathBuf>,
) -> attractor_types::Result<()> {
    let symlink_metadata = tokio::fs::symlink_metadata(current).await?;
    if symlink_metadata.file_type().is_symlink() && !follow_symlinks {
        return Ok(());
    }
    let metadata = if follow_symlinks {
        tokio::fs::metadata(current).await?
    } else {
        symlink_metadata
    };
    if metadata.is_file() {
        if let Ok(relative) = current.strip_prefix(base) {
            if set.is_match(relative) {
                matches.push(current.to_path_buf());
            }
        }
    } else if metadata.is_dir() {
        let mut read_dir = tokio::fs::read_dir(current).await?;
        while let Some(entry) = read_dir.next_entry().await? {
            Box::pin(collect_glob_matches(
                base,
                &entry.path(),
                set,
                follow_symlinks,
                matches,
            ))
            .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn confined(root: &TempDir) -> RootConfinedExecutionEnvironment {
        RootConfinedExecutionEnvironment::new(root.path()).unwrap()
    }

    #[test]
    fn confined_root_must_exist_and_be_a_directory() {
        let root = TempDir::new().unwrap();
        let file = root.path().join("file.txt");
        std::fs::write(&file, "not a directory").unwrap();

        assert!(RootConfinedExecutionEnvironment::new(root.path().join("missing")).is_err());
        assert!(RootConfinedExecutionEnvironment::new(file).is_err());
    }

    #[tokio::test]
    async fn confined_read_write_round_trip_and_parent_creation() {
        let root = TempDir::new().unwrap();
        let environment = confined(&root);

        environment
            .write_file(Path::new("sub/dir/file.txt"), "hello world")
            .await
            .unwrap();
        assert_eq!(
            environment
                .read_file(Path::new("sub/dir/file.txt"))
                .await
                .unwrap(),
            "hello world"
        );
        assert!(environment
            .file_exists(Path::new("sub/dir/file.txt"))
            .await
            .unwrap());
        assert!(!environment
            .file_exists(Path::new("missing.txt"))
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn confined_environment_rejects_parent_traversal_for_every_filesystem_method() {
        let root = TempDir::new().unwrap();
        let environment = confined(&root);
        let path = Path::new("../escape");
        let options = GrepOptions::default();

        assert!(environment.read_file(path).await.is_err());
        assert!(environment.write_file(path, "escape").await.is_err());
        assert!(environment.file_exists(path).await.is_err());
        assert!(environment.list_directory(path, 2).await.is_err());
        assert!(environment.grep("x", path, &options).await.is_err());
        assert!(environment.glob_files("*", path).await.is_err());
    }

    #[tokio::test]
    async fn confined_environment_rejects_absolute_paths_outside_root() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let environment = confined(&root);
        let path = outside.path().join("escape.txt");
        std::fs::write(&path, "secret").unwrap();
        let options = GrepOptions::default();

        assert!(environment.read_file(&path).await.is_err());
        assert!(environment.write_file(&path, "escape").await.is_err());
        assert!(environment.file_exists(&path).await.is_err());
        assert!(environment.list_directory(outside.path(), 2).await.is_err());
        assert!(environment.grep("secret", &path, &options).await.is_err());
        assert!(environment.glob_files("*", outside.path()).await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn confined_environment_rejects_symlink_escapes_for_every_filesystem_method() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        symlink(
            outside.path().join("secret.txt"),
            root.path().join("file-link"),
        )
        .unwrap();
        symlink(outside.path(), root.path().join("dir-link")).unwrap();
        let environment = confined(&root);
        let options = GrepOptions::default();

        assert!(environment.read_file(Path::new("file-link")).await.is_err());
        assert!(environment
            .write_file(Path::new("dir-link/new.txt"), "escape")
            .await
            .is_err());
        assert!(environment
            .file_exists(Path::new("file-link"))
            .await
            .is_err());
        assert!(environment
            .list_directory(Path::new("dir-link"), 2)
            .await
            .is_err());
        assert!(environment
            .grep("secret", Path::new("file-link"), &options)
            .await
            .is_err());
        assert!(environment
            .glob_files("*", Path::new("dir-link"))
            .await
            .is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn confined_recursive_tools_do_not_follow_nested_symlink_escapes() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "needle").unwrap();
        symlink(outside.path(), root.path().join("escape")).unwrap();
        let environment = confined(&root);

        let listed = environment.list_directory(Path::new("."), 3).await.unwrap();
        assert!(listed.iter().any(|entry| entry.path.ends_with("escape")));
        assert!(!listed
            .iter()
            .any(|entry| entry.path.ends_with("secret.txt")));
        assert!(environment
            .grep("needle", Path::new("."), &GrepOptions::default())
            .await
            .unwrap()
            .is_empty());
        assert!(environment
            .glob_files("**/*.txt", Path::new("."))
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn host_environment_explicitly_allows_absolute_outside_access() {
        let working = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let path = outside.path().join("host.txt");
        std::fs::write(&path, "host access").unwrap();
        let environment = HostExecutionEnvironment::new(working.path());

        assert_eq!(environment.read_file(&path).await.unwrap(), "host access");
    }

    #[tokio::test]
    async fn commands_receive_only_allowlisted_environment_variables() {
        let root = TempDir::new().unwrap();
        let environment = confined(&root);
        let secret_key = "PAS_EXECUTION_ENV_SECRET_TOKEN";
        std::env::set_var(secret_key, "must-not-leak");

        let result = environment
            .exec_command(
                "printf '%s|%s' \"${PAS_EXECUTION_ENV_SECRET_TOKEN-unset}\" \"${PATH-unset}\"",
                5_000,
                None,
                None,
            )
            .await
            .unwrap();
        std::env::remove_var(secret_key);

        assert_eq!(result.exit_code, 0);
        let (secret, path) = result.stdout.split_once('|').unwrap();
        assert_eq!(secret, "unset");
        assert_ne!(path, "unset");
        assert!(!path.is_empty());
    }

    #[tokio::test]
    async fn command_environment_overrides_must_be_allowlisted() {
        let root = TempDir::new().unwrap();
        let environment = confined(&root);
        let overrides = HashMap::from([("DATABASE_PASSWORD".to_string(), "secret".to_string())]);

        let error = environment
            .exec_command("true", 5_000, None, Some(&overrides))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("DATABASE_PASSWORD"));
        assert!(error.to_string().contains("not allowlisted"));
    }

    #[tokio::test]
    async fn command_runner_is_injectable() {
        let root = TempDir::new().unwrap();
        let environment = RootConfinedExecutionEnvironment::with_command_runner(
            root.path(),
            CommandRunner::new("missing-pas-command-runner", ["--command"]),
        )
        .unwrap();

        let error = environment
            .exec_command("echo ignored", 5_000, None, None)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("missing-pas-command-runner")
                || error.to_string().contains("No such file")
        );
    }

    #[tokio::test]
    async fn confined_glob_list_and_grep_work_inside_root() {
        let root = TempDir::new().unwrap();
        let environment = confined(&root);
        environment
            .write_file(Path::new("a.rs"), "needle")
            .await
            .unwrap();
        environment
            .write_file(Path::new("b.txt"), "other")
            .await
            .unwrap();

        assert_eq!(
            environment
                .glob_files("*.rs", Path::new("."))
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            environment
                .list_directory(Path::new("."), 1)
                .await
                .unwrap()
                .len(),
            2
        );
        assert!(environment
            .grep("needle", Path::new("."), &GrepOptions::default())
            .await
            .unwrap()
            .contains("needle"));
    }

    #[tokio::test]
    async fn command_timeout_terminates() {
        let root = TempDir::new().unwrap();
        let environment = confined(&root);
        let result = environment
            .exec_command("sleep 60", 50, None, None)
            .await
            .unwrap();
        assert!(result.timed_out);
        assert!(result.duration_ms >= 50);
    }

    #[test]
    fn platform_and_working_directory_are_reported() {
        let root = TempDir::new().unwrap();
        let environment = confined(&root);
        assert_eq!(environment.platform(), std::env::consts::OS);
        assert_eq!(
            environment.working_directory(),
            std::fs::canonicalize(root.path()).unwrap()
        );
    }
}
