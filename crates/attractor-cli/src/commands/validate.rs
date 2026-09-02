use anyhow;

pub fn cmd_validate(path: &std::path::Path) -> anyhow::Result<()> {
    let graph = crate::load_pipeline(path)?;
    let diagnostics = attractor_pipeline::validate(&graph);

    if diagnostics.is_empty() {
        println!("Pipeline is valid");
        return Ok(());
    }

    let mut has_error = false;
    for diag in &diagnostics {
        let severity = match diag.severity {
            attractor_pipeline::Severity::Error => {
                has_error = true;
                "ERROR"
            }
            attractor_pipeline::Severity::Warning => "WARN",
            attractor_pipeline::Severity::Info => "INFO",
        };
        println!("[{}] {}: {}", severity, diag.rule, diag.message);
    }

    if has_error {
        anyhow::bail!("Validation failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A runtime node with no explicit `llm_provider` must block `pas
    /// validate` with a non-zero exit (an `Err` here, which main.rs turns
    /// into a non-zero process exit), and the printed diagnostic must name
    /// the offending node id so the pipeline author knows exactly what to
    /// fix.
    #[test]
    fn cmd_validate_fails_on_missing_llm_provider_and_names_the_node() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pipeline.dot");
        std::fs::write(
            &path,
            r#"digraph G {
                start [shape="Mdiamond"]
                work [shape="box", prompt="Do work"]
                done [shape="Msquare"]
                start -> work -> done
            }"#,
        )
        .unwrap();

        let result = cmd_validate(&path);
        let err = result.expect_err("validation must fail for a missing llm_provider");
        assert!(err.to_string().contains("Validation failed"));

        // The underlying diagnostics (what cmd_validate prints) must name
        // the specific node id, not just report a generic failure.
        let graph = crate::load_pipeline(&path).unwrap();
        let diagnostics = attractor_pipeline::validate(&graph);
        let provider_errors: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.rule == "provider_required")
            .collect();
        assert_eq!(provider_errors.len(), 1);
        assert!(provider_errors[0].message.contains("'work'"));
    }

    /// A pipeline where every runtime node names an explicit `llm_provider`
    /// must validate cleanly (`Ok`), even for a provider other than the
    /// implicit default.
    #[test]
    fn cmd_validate_passes_when_llm_provider_is_explicit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pipeline.dot");
        std::fs::write(
            &path,
            r#"digraph G {
                start [shape="Mdiamond"]
                work [shape="box", llm_provider="codex", prompt="Do work"]
                done [shape="Msquare"]
                start -> work -> done
            }"#,
        )
        .unwrap();

        let result = cmd_validate(&path);
        assert!(result.is_ok());
    }
}
