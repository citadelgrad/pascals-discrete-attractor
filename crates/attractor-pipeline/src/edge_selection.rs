//! 5-step edge selection algorithm.
//!
//! After a node completes, this module determines which outgoing edge to follow
//! based on a priority cascade: condition match, preferred label, suggested next
//! IDs, highest weight, and lexical tiebreak.

use std::sync::OnceLock;

use crate::condition::{evaluate_condition, parse_condition};
use crate::graph::{PipelineEdge, PipelineGraph};

/// Select the next edge to follow after a node completes.
/// Returns `None` if no edges are available (terminal node).
pub fn select_edge<'a>(
    node_id: &str,
    outcome: &attractor_types::Outcome,
    resolve: &dyn Fn(&str) -> String,
    graph: &'a PipelineGraph,
) -> Option<&'a PipelineEdge> {
    let edges = graph.outgoing_edges(node_id);
    if edges.is_empty() {
        return None;
    }

    // Step 1: Condition-matching edges
    let condition_edges: Vec<_> = edges
        .iter()
        .filter(|e| {
            if let Some(ref cond) = e.condition {
                if let Ok(expr) = parse_condition(cond) {
                    evaluate_condition(&expr, resolve)
                } else {
                    false
                }
            } else {
                false
            }
        })
        .collect();
    if !condition_edges.is_empty() {
        return Some(best_by_weight_then_lexical(&condition_edges));
    }

    // Step 2: Preferred label match
    if let Some(ref label) = outcome.preferred_label {
        let normalized = normalize_label(label);
        for edge in edges {
            if let Some(ref elabel) = edge.label {
                if normalize_label(elabel) == normalized {
                    return Some(edge);
                }
            }
        }
    }

    // Step 3: Suggested next IDs
    for suggested in &outcome.suggested_next_ids {
        for edge in edges {
            if edge.to == *suggested {
                return Some(edge);
            }
        }
    }

    // Step 4 & 5: Unconditional edges by weight with lexical tiebreak
    let unconditional: Vec<_> = edges.iter().filter(|e| e.condition.is_none()).collect();
    if unconditional.is_empty() {
        // Last resort: all edges have conditions but none matched — fall back
        // to the first edge and warn, since routing is now arbitrary.
        tracing::warn!(
            node = %node_id,
            "All outgoing edges have conditions but none matched; falling back to first edge"
        );
        return edges.first();
    }
    Some(best_by_weight_then_lexical(&unconditional))
}

/// Normalize a label for comparison: lowercase, strip accelerator prefixes like
/// `[Y]`, `Y)`, `Y-`.
fn normalize_label(label: &str) -> String {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"^(?:\[\w\]\s*|\w\)\s*|\w-\s*)").expect("valid regex")
    });
    let s = label.trim().to_lowercase();
    re.replace(&s, "").to_string()
}

/// Pick the edge with the highest weight; break ties by lexicographically
/// smallest `to` field.
fn best_by_weight_then_lexical<'a>(edges: &[&'a PipelineEdge]) -> &'a PipelineEdge {
    debug_assert!(!edges.is_empty(), "best_by_weight_then_lexical called with empty slice");
    edges
        .iter()
        .copied()
        .max_by(|a, b| {
            a.weight.cmp(&b.weight).then(b.to.cmp(&a.to)) // lexical ascending = reverse compare
        })
        .expect("edges slice must not be empty")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::PipelineGraph;

    fn parse_and_build(dot: &str) -> PipelineGraph {
        let graph = attractor_dot::parse(dot).unwrap();
        PipelineGraph::from_dot(graph).unwrap()
    }

    fn make_outcome() -> attractor_types::Outcome {
        attractor_types::Outcome::success("ok")
    }

    fn make_resolve(outcome_val: &str) -> impl Fn(&str) -> String + '_ {
        move |key: &str| match key {
            "outcome" => outcome_val.to_string(),
            _ => String::new(),
        }
    }

    // Test 1: condition match takes priority
    #[test]
    fn step1_condition_match_takes_priority() {
        let pg = parse_and_build(
            r#"digraph G {
            A -> B [condition="outcome=success"]
            A -> C
        }"#,
        );
        let outcome = make_outcome();
        let resolve = make_resolve("success");
        let edge = select_edge("A", &outcome, &resolve, &pg).unwrap();
        assert_eq!(edge.to, "B");
    }

    // Test 2: preferred label match
    #[test]
    fn step2_preferred_label_match() {
        let pg = parse_and_build(
            r#"digraph G {
            A -> B [label="approve"]
            A -> C [label="reject"]
        }"#,
        );
        let mut outcome = make_outcome();
        outcome.preferred_label = Some("approve".to_string());
        let resolve = make_resolve("unknown");
        let edge = select_edge("A", &outcome, &resolve, &pg).unwrap();
        assert_eq!(edge.to, "B");
    }

    // Test 3: suggested next ID match
    #[test]
    fn step3_suggested_next_id() {
        let pg = parse_and_build(
            r#"digraph G {
            A -> B
            A -> C
        }"#,
        );
        let mut outcome = make_outcome();
        outcome.suggested_next_ids = vec!["C".to_string()];
        let resolve = make_resolve("unknown");
        let edge = select_edge("A", &outcome, &resolve, &pg).unwrap();
        assert_eq!(edge.to, "C");
    }

    // Test 4: highest weight wins
    #[test]
    fn step4_highest_weight_wins() {
        let pg = parse_and_build(
            r#"digraph G {
            A -> B [weight=1]
            A -> C [weight=5]
        }"#,
        );
        let outcome = make_outcome();
        let resolve = make_resolve("unknown");
        let edge = select_edge("A", &outcome, &resolve, &pg).unwrap();
        assert_eq!(edge.to, "C");
    }

    // Test 5: lexical tiebreak on equal weight
    #[test]
    fn step5_lexical_tiebreak() {
        let pg = parse_and_build(
            r#"digraph G {
            A -> C [weight=1]
            A -> B [weight=1]
        }"#,
        );
        let outcome = make_outcome();
        let resolve = make_resolve("unknown");
        let edge = select_edge("A", &outcome, &resolve, &pg).unwrap();
        assert_eq!(edge.to, "B");
    }

    // Test 6: no edges returns None
    #[test]
    fn no_edges_returns_none() {
        let pg = parse_and_build(
            r#"digraph G {
            A [label="terminal"]
        }"#,
        );
        let outcome = make_outcome();
        let resolve = make_resolve("unknown");
        assert!(select_edge("A", &outcome, &resolve, &pg).is_none());
    }

    // Test 7: condition false skips to next step
    #[test]
    fn condition_false_skips_to_next_step() {
        let pg = parse_and_build(
            r#"digraph G {
            A -> B [condition="outcome=fail"]
            A -> C
        }"#,
        );
        let outcome = make_outcome();
        let resolve = make_resolve("success");
        let edge = select_edge("A", &outcome, &resolve, &pg).unwrap();
        // condition is false, so fall through to step 4/5 unconditional
        assert_eq!(edge.to, "C");
    }

    // Test 8: label normalization strips accelerators
    #[test]
    fn label_normalization_strips_accelerators() {
        assert_eq!(normalize_label("[Y] Yes, approve"), "yes, approve");
        assert_eq!(normalize_label("Y) Yes, approve"), "yes, approve");
        assert_eq!(normalize_label("Y- Yes, approve"), "yes, approve");
        assert_eq!(normalize_label("  Approve  "), "approve");
    }
}
