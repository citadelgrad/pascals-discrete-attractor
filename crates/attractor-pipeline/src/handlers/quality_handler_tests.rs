#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use attractor_dot::AttributeValue;
    use attractor_types::{Context, StageStatus};

    use crate::handler::default_registry;
    use crate::handler::NodeHandler;
    use crate::handlers::quality_handler::QualityHandler;
    use crate::handlers::tests::{make_minimal_graph, make_node};

    // -----------------------------------------------------------------------
    // Test 1: disabled via node attribute (enabled=false)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn quality_handler_disabled_via_node_attribute_returns_success() {
        let handler = QualityHandler;
        let mut attrs = HashMap::new();
        attrs.insert("enabled".into(), AttributeValue::Boolean(false));
        let node = make_node("verify", "box", None, attrs);
        let ctx = Context::default();
        let graph = make_minimal_graph();

        let outcome = handler.execute(&node, &ctx, &graph).await.unwrap();
        assert_eq!(outcome.status, StageStatus::Success);
    }

    // -----------------------------------------------------------------------
    // Test 2: disabled via runtime context flag
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn quality_handler_disabled_via_runtime_flag_returns_success() {
        let handler = QualityHandler;
        let node = make_node("verify", "box", None, HashMap::new());
        let ctx = Context::default();
        ctx.set("quality_disabled", serde_json::Value::Bool(true))
            .await;
        let graph = make_minimal_graph();

        let outcome = handler.execute(&node, &ctx, &graph).await.unwrap();
        assert_eq!(outcome.status, StageStatus::Success);
    }

    // -----------------------------------------------------------------------
    // Test 3: error on missing quality_checks attribute
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn quality_handler_errors_on_missing_quality_checks() {
        let handler = QualityHandler;
        // node_type="quality" but no quality_checks attribute
        let mut node = make_node("verify", "box", None, HashMap::new());
        node.node_type = Some("quality".into());
        let ctx = Context::default();
        let graph = make_minimal_graph();

        let result = handler.execute(&node, &ctx, &graph).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("quality_checks"),
            "Expected error mentioning 'quality_checks', got: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // Test 4: all checks pass → Success + context written
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn quality_handler_all_checks_pass_returns_success() {
        let handler = QualityHandler;
        let mut attrs = HashMap::new();
        // "true" is a shell built-in that always exits 0
        attrs.insert(
            "quality_checks".into(),
            AttributeValue::String("true|true".into()),
        );
        let node = make_node("verify", "box", None, attrs);
        let ctx = Context::default();
        let graph = make_minimal_graph();

        let outcome = handler.execute(&node, &ctx, &graph).await.unwrap();
        assert_eq!(outcome.status, StageStatus::Success);

        // verify.completed = true
        assert_eq!(
            outcome.context_updates.get("verify.completed"),
            Some(&serde_json::Value::Bool(true)),
            "Expected verify.completed = true in context_updates"
        );

        // verify.results is a JSON array with 2 entries, both passed=true
        let results = outcome
            .context_updates
            .get("verify.results")
            .expect("verify.results should be in context_updates");
        let arr = results
            .as_array()
            .expect("verify.results should be a JSON array");
        assert_eq!(arr.len(), 2, "Expected 2 result entries");
        for entry in arr {
            assert_eq!(
                entry.get("passed").and_then(|v| v.as_bool()),
                Some(true),
                "Expected each entry to have passed=true, got: {entry}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Test 5: fail-fast on first failure
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn quality_handler_fail_fast_on_first_failure() {
        let handler = QualityHandler;
        let mut attrs = HashMap::new();
        // "false" exits 1; "true" exits 0 — only the first should run
        attrs.insert(
            "quality_checks".into(),
            AttributeValue::String("false|true".into()),
        );
        let node = make_node("verify", "box", None, attrs);
        let ctx = Context::default();
        let graph = make_minimal_graph();

        let outcome = handler.execute(&node, &ctx, &graph).await.unwrap();
        assert_eq!(outcome.status, StageStatus::Fail);

        // Only 1 result entry — second command must not have run
        let results = outcome
            .context_updates
            .get("verify.results")
            .expect("verify.results should be in context_updates");
        let arr = results
            .as_array()
            .expect("verify.results should be a JSON array");
        assert_eq!(
            arr.len(),
            1,
            "Only first command should have run (fail-fast)"
        );

        // First entry should have passed=false
        assert_eq!(
            arr[0].get("passed").and_then(|v| v.as_bool()),
            Some(false),
            "First result should have passed=false"
        );

        // failure_reason must be set
        assert!(
            outcome.failure_reason.is_some(),
            "failure_reason should be set on Fail outcome"
        );
    }

    // -----------------------------------------------------------------------
    // Test 6: failure_summary contains stderr from failing command
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn quality_handler_failure_summary_contains_stderr() {
        let handler = QualityHandler;
        let mut attrs = HashMap::new();
        attrs.insert(
            "quality_checks".into(),
            AttributeValue::String("sh -c 'echo OOPS >&2; exit 1'".into()),
        );
        let node = make_node("verify", "box", None, attrs);
        let ctx = Context::default();
        let graph = make_minimal_graph();

        let outcome = handler.execute(&node, &ctx, &graph).await.unwrap();
        assert_eq!(outcome.status, StageStatus::Fail);

        let summary = outcome
            .context_updates
            .get("verify.failure_summary")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(
            summary.contains("OOPS"),
            "failure_summary should contain stderr 'OOPS', got: {summary:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Test 7: QualityHandler appears in default_registry()
    // -----------------------------------------------------------------------

    #[test]
    fn quality_handler_registers_in_default_registry() {
        let reg = default_registry();
        assert!(
            reg.has("quality"),
            "default_registry() should include the 'quality' handler"
        );
    }

    // -----------------------------------------------------------------------
    // Test 8: failure_footprint is present in results on stage failure
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn quality_handler_failure_footprint_set_on_fail() {
        let handler = QualityHandler;
        let mut attrs = HashMap::new();
        attrs.insert(
            "quality_checks".into(),
            AttributeValue::String("sh -c 'echo stderr_output >&2; exit 1'".into()),
        );
        let node = make_node("verify", "box", None, attrs);
        let ctx = Context::default();
        let graph = make_minimal_graph();

        let outcome = handler.execute(&node, &ctx, &graph).await.unwrap();
        assert_eq!(outcome.status, StageStatus::Fail);

        let results = outcome
            .context_updates
            .get("verify.results")
            .and_then(|v| v.as_array())
            .expect("verify.results should be a JSON array");

        let footprint = results[0]
            .get("failure_footprint")
            .expect("failure_footprint should be present");
        assert!(
            !footprint.is_null(),
            "failure_footprint should be non-null on failure"
        );
        let fp_str = footprint.as_str().unwrap_or("");
        assert_eq!(fp_str.len(), 16, "failure_footprint should be 16 hex chars");
    }

    // -----------------------------------------------------------------------
    // Test 9: truncate_head_tail keeps head + tail, omits middle
    // -----------------------------------------------------------------------

    #[test]
    fn truncate_head_tail_keeps_head_and_tail() {
        use crate::handlers::quality_handler::truncate_head_tail;

        let lines: Vec<String> = (1..=200).map(|i| format!("line {i}")).collect();
        let text = lines.join("\n");

        let result = truncate_head_tail(&text, 5, 5);
        assert!(result.contains("line 1"), "should contain head");
        assert!(result.contains("line 5"), "should contain last head line");
        assert!(result.contains("line 200"), "should contain tail");
        assert!(result.contains("omitted"), "should mention omitted lines");
        assert!(
            !result.contains("line 100"),
            "middle lines should be omitted"
        );
    }

    #[test]
    fn truncate_head_tail_short_text_unchanged() {
        use crate::handlers::quality_handler::truncate_head_tail;

        let text = "line 1\nline 2\nline 3";
        let result = truncate_head_tail(text, 50, 50);
        assert_eq!(result, text.trim_end());
    }

    #[test]
    fn truncate_head_tail_exact_boundary_not_truncated() {
        use crate::handlers::quality_handler::truncate_head_tail;

        // Exactly head+tail lines: total <= head+tail → return unchanged.
        // Mutation (< instead of <=): 10 < 10 = false → produces "(0 lines omitted)" marker.
        let text: String = (1..=10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = truncate_head_tail(&text, 5, 5);
        assert!(
            !result.contains("omitted"),
            "10 lines with head=5 tail=5 should not truncate, got: {result:?}"
        );
        assert_eq!(result, text.trim_end());
    }

    // -----------------------------------------------------------------------
    // Test 10: manifest-driven stages run from [quality.stages]
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn quality_handler_manifest_stages_run_in_order() {
        use std::io::Write;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let manifest = r#"
[project]
name = "test-project"

[quality]
stages = ["check-a", "check-b"]

[quality.hooks.check-a]
cmd = "true"

[quality.hooks.check-b]
cmd = "true"
"#;
        let toml_path = tmp.path().join("pas.toml");
        std::fs::File::create(&toml_path)
            .unwrap()
            .write_all(manifest.as_bytes())
            .unwrap();

        // Also write a sentinel .git dir so resolve() stops here
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();

        let handler = QualityHandler;
        // No quality_checks attr — must use manifest
        let node = make_node("verify", "box", None, HashMap::new());
        let ctx = Context::default();
        ctx.set(
            "workdir",
            serde_json::Value::String(tmp.path().to_string_lossy().to_string()),
        )
        .await;
        let graph = make_minimal_graph();

        std::env::set_var("PAS_TRUST_THIS", "1");
        let outcome = handler.execute(&node, &ctx, &graph).await.unwrap();
        std::env::remove_var("PAS_TRUST_THIS");
        assert_eq!(outcome.status, StageStatus::Success);

        let results = outcome
            .context_updates
            .get("verify.results")
            .and_then(|v| v.as_array())
            .expect("verify.results should be an array");
        assert_eq!(results.len(), 2, "both manifest stages should have run");
        assert_eq!(
            results[0].get("stage").and_then(|v| v.as_str()),
            Some("check-a")
        );
        assert_eq!(
            results[1].get("stage").and_then(|v| v.as_str()),
            Some("check-b")
        );
    }

    // -----------------------------------------------------------------------
    // Test 11: empty manifest stages fall back to legacy quality_checks
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn quality_handler_empty_manifest_stages_fall_back_to_attr() {
        use std::io::Write;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let manifest = r#"
[project]
name = "test-project"

[quality]
stages = []
"#;
        let toml_path = tmp.path().join("pas.toml");
        std::fs::File::create(&toml_path)
            .unwrap()
            .write_all(manifest.as_bytes())
            .unwrap();
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();

        let handler = QualityHandler;
        let mut attrs = HashMap::new();
        attrs.insert(
            "quality_checks".into(),
            AttributeValue::String("true".into()),
        );
        let node = make_node("verify", "box", None, attrs);
        let ctx = Context::default();
        ctx.set(
            "workdir",
            serde_json::Value::String(tmp.path().to_string_lossy().to_string()),
        )
        .await;
        let graph = make_minimal_graph();

        let outcome = handler.execute(&node, &ctx, &graph).await.unwrap();
        assert_eq!(outcome.status, StageStatus::Success);
        let results = outcome
            .context_updates
            .get("verify.results")
            .and_then(|v| v.as_array())
            .expect("verify.results should be an array");
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].get("stage").and_then(|v| v.as_str()),
            Some("true")
        );
    }

    // -----------------------------------------------------------------------
    // Test 12 (integration): 2-node pass+fail outcomes
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn quality_handler_two_node_pipeline_pass_then_fail() {
        let handler = QualityHandler;
        let graph = make_minimal_graph();

        // Node 1: passes
        let mut attrs1 = HashMap::new();
        attrs1.insert(
            "quality_checks".into(),
            AttributeValue::String("true".into()),
        );
        let node1 = make_node("node1", "box", None, attrs1);
        let ctx1 = Context::default();
        let out1 = handler.execute(&node1, &ctx1, &graph).await.unwrap();
        assert_eq!(out1.status, StageStatus::Success);

        // Node 2: fails
        let mut attrs2 = HashMap::new();
        attrs2.insert(
            "quality_checks".into(),
            AttributeValue::String("false".into()),
        );
        let node2 = make_node("node2", "box", None, attrs2);
        let ctx2 = Context::default();
        let out2 = handler.execute(&node2, &ctx2, &graph).await.unwrap();
        assert_eq!(out2.status, StageStatus::Fail);
        assert!(out2.failure_reason.is_some());
        assert_eq!(
            out2.context_updates.get("node2.completed"),
            Some(&serde_json::Value::Bool(false))
        );
    }
}
