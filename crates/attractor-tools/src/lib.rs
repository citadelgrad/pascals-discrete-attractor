//! Tool trait, built-in tools, and execution environment for Attractor agents.
//!
//! Provides `Tool` trait, `ToolRegistry`, `ExecutionEnvironment` trait,
//! explicit host and root-confined implementations, and built-in tools
//! (read_file, write_file, edit_file, shell, grep, glob).

pub mod builtin;
pub mod environment;
pub mod local_env;
pub mod profiles;
pub mod tool;
pub mod truncation;

pub use builtin::{EditFileTool, GlobTool, GrepTool, ReadFileTool, ShellTool, WriteFileTool};
pub use environment::{DirEntry, ExecResult, ExecutionEnvironment, GrepOptions};
pub use local_env::{CommandRunner, HostExecutionEnvironment, RootConfinedExecutionEnvironment};
pub use profiles::ToolProfile;
pub use tool::{Tool, ToolDefinition, ToolExecutionOptions, ToolRegistry};

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    /// A mock environment that stores files in memory.
    struct MockEnv {
        files: Mutex<HashMap<String, String>>,
        exec_stdout: String,
        exec_stderr: String,
        exec_exit_code: i32,
    }

    impl MockEnv {
        fn new() -> Self {
            Self {
                files: Mutex::new(HashMap::new()),
                exec_stdout: String::new(),
                exec_stderr: String::new(),
                exec_exit_code: 0,
            }
        }

        fn with_file(self, path: &str, content: &str) -> Self {
            self.files
                .lock()
                .unwrap()
                .insert(path.to_string(), content.to_string());
            self
        }

        fn with_exec_result(mut self, stdout: &str, stderr: &str, exit_code: i32) -> Self {
            self.exec_stdout = stdout.to_string();
            self.exec_stderr = stderr.to_string();
            self.exec_exit_code = exit_code;
            self
        }
    }

    #[async_trait]
    impl ExecutionEnvironment for MockEnv {
        async fn read_file(&self, path: &Path) -> attractor_types::Result<String> {
            let key = path.to_string_lossy().to_string();
            self.files
                .lock()
                .unwrap()
                .get(&key)
                .cloned()
                .ok_or_else(|| attractor_types::AttractorError::ToolError {
                    tool: "read_file".into(),
                    message: format!("File not found: {}", key),
                })
        }
        async fn write_file(&self, path: &Path, content: &str) -> attractor_types::Result<()> {
            let key = path.to_string_lossy().to_string();
            self.files.lock().unwrap().insert(key, content.to_string());
            Ok(())
        }
        async fn file_exists(&self, path: &Path) -> attractor_types::Result<bool> {
            let key = path.to_string_lossy().to_string();
            Ok(self.files.lock().unwrap().contains_key(&key))
        }
        async fn list_directory(
            &self,
            _path: &Path,
            _depth: usize,
        ) -> attractor_types::Result<Vec<DirEntry>> {
            Ok(vec![])
        }
        async fn exec_command(
            &self,
            _command: &str,
            _timeout_ms: u64,
            _cwd: Option<&Path>,
            _env_vars: Option<&HashMap<String, String>>,
        ) -> attractor_types::Result<ExecResult> {
            Ok(ExecResult {
                stdout: self.exec_stdout.clone(),
                stderr: self.exec_stderr.clone(),
                exit_code: self.exec_exit_code,
                timed_out: false,
                duration_ms: 0,
            })
        }
        async fn grep(
            &self,
            _pattern: &str,
            _path: &Path,
            _options: &GrepOptions,
        ) -> attractor_types::Result<String> {
            Ok("match1\nmatch2".to_string())
        }
        async fn glob_files(
            &self,
            _pattern: &str,
            _base: &Path,
        ) -> attractor_types::Result<Vec<PathBuf>> {
            Ok(vec![PathBuf::from("/a.rs"), PathBuf::from("/b.rs")])
        }
        fn working_directory(&self) -> &Path {
            Path::new("/tmp")
        }
        fn platform(&self) -> &str {
            "test"
        }
    }

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "echo".to_string(),
                description: "Echoes the input".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "text": { "type": "string" }
                    }
                }),
            }
        }

        async fn execute(
            &self,
            arguments: serde_json::Value,
            _env: &dyn ExecutionEnvironment,
        ) -> attractor_types::Result<String> {
            let text = arguments
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("(empty)");
            Ok(text.to_string())
        }
    }

    #[test]
    fn registry_register_and_get() {
        let mut registry = ToolRegistry::new();
        assert!(registry.is_empty());

        registry.register(EchoTool);
        assert_eq!(registry.len(), 1);
        assert!(!registry.is_empty());
        assert!(registry.get("echo").is_some());
        assert!(registry.get("nonexistent").is_none());
    }

    #[test]
    fn registry_unregister() {
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool);
        assert!(registry.unregister("echo"));
        assert!(!registry.unregister("echo"));
        assert!(registry.is_empty());
    }

    #[test]
    fn registry_definitions() {
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool);
        let defs = registry.definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "echo");
    }

    #[test]
    fn registry_names() {
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool);
        let names = registry.names();
        assert_eq!(names.len(), 1);
        assert!(names.contains(&"echo".to_string()));
    }

    #[tokio::test]
    async fn mock_tool_execute() {
        let tool = EchoTool;
        let env = MockEnv::new();
        let args = serde_json::json!({"text": "hello"});
        let result = tool.execute(args, &env).await.unwrap();
        assert_eq!(result, "hello");
    }

    #[test]
    fn exec_result_creation() {
        let result = ExecResult {
            stdout: "out".to_string(),
            stderr: "err".to_string(),
            exit_code: 0,
            timed_out: false,
            duration_ms: 42,
        };
        assert_eq!(result.exit_code, 0);
        assert!(!result.timed_out);
        assert_eq!(result.duration_ms, 42);
    }

    #[test]
    fn grep_options_default() {
        let opts = GrepOptions::default();
        assert!(!opts.case_insensitive);
        assert!(opts.max_results.is_none());
        assert!(!opts.include_line_numbers);
        assert_eq!(opts.context_lines, 0);
    }

    // ---- Built-in tool tests ----

    #[tokio::test]
    async fn read_file_adds_line_numbers() {
        let env = MockEnv::new().with_file("/test.txt", "line one\nline two\nline three");
        let tool = ReadFileTool;
        let args = serde_json::json!({"file_path": "/test.txt"});
        let result = tool.execute(args, &env).await.unwrap();
        assert!(result.contains("1 | line one"));
        assert!(result.contains("2 | line two"));
        assert!(result.contains("3 | line three"));
    }

    #[tokio::test]
    async fn write_file_creates_file() {
        let env = MockEnv::new();
        let tool = WriteFileTool;
        let args = serde_json::json!({"file_path": "/new.txt", "content": "hello world"});
        let result = tool.execute(args, &env).await.unwrap();
        assert!(result.contains("11 bytes"));
        assert!(result.contains("/new.txt"));

        // Verify file was stored
        let content = env.read_file(Path::new("/new.txt")).await.unwrap();
        assert_eq!(content, "hello world");
    }

    #[tokio::test]
    async fn edit_file_replaces_exact_string() {
        let env = MockEnv::new().with_file("/code.rs", "fn main() {\n    println!(\"hello\");\n}");
        let tool = EditFileTool;
        let args = serde_json::json!({
            "file_path": "/code.rs",
            "old_string": "hello",
            "new_string": "world"
        });
        let result = tool.execute(args, &env).await.unwrap();
        assert!(result.contains("1 occurrence"));

        let content = env.read_file(Path::new("/code.rs")).await.unwrap();
        assert!(content.contains("world"));
        assert!(!content.contains("hello"));
    }

    #[tokio::test]
    async fn edit_file_errors_on_non_unique_match() {
        let env = MockEnv::new().with_file("/dup.txt", "foo bar foo baz foo");
        let tool = EditFileTool;
        let args = serde_json::json!({
            "file_path": "/dup.txt",
            "old_string": "foo",
            "new_string": "qux"
        });
        let result = tool.execute(args, &env).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("3 times"));
    }

    #[tokio::test]
    async fn shell_tool_captures_output() {
        let env = MockEnv::new().with_exec_result("hello\n", "", 0);
        let tool = ShellTool;
        let args = serde_json::json!({"command": "echo hello"});
        let result = tool.execute(args, &env).await.unwrap();
        assert!(result.contains("Exit code: 0"));
        assert!(result.contains("hello"));
    }

    #[test]
    fn truncation_head_tail_mode() {
        use truncation::{truncate_output, TruncationMode};
        let input: String = "x".repeat(100);
        let result = truncate_output(&input, 50, TruncationMode::HeadTail);
        assert!(result.contains("[WARNING: Output truncated."));
        assert!(result.contains("characters removed from middle"));
        // Head portion is 20 chars (40% of 50)
        assert!(result.starts_with(&"x".repeat(20)));
    }

    #[test]
    fn truncation_tail_mode() {
        use truncation::{truncate_output, TruncationMode};
        let input: String = "y".repeat(100);
        let result = truncate_output(&input, 50, TruncationMode::Tail);
        assert!(result.contains("[WARNING: Output truncated."));
        assert!(result.contains("characters removed from start"));
        assert!(result.ends_with(&"y".repeat(50)));
    }
}
