//! Context fidelity modes control how conversation history is managed as it grows.

/// Fidelity mode for managing conversation context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FidelityMode {
    /// Keep full history — no truncation.
    #[default]
    Full,
    /// Keep the system prompt (first message) plus the most recent `keep_last - 1`
    /// messages, for `keep_last` total. The first message is always preserved.
    Truncate { keep_last: usize },
    /// Compact old messages into a summary.
    Compact,
    /// Replace history with an LLM-generated summary.
    Summary,
}

impl FidelityMode {
    /// Parse from a string attribute value.
    pub fn parse(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "full" => FidelityMode::Full,
            "compact" => FidelityMode::Compact,
            "summary" => FidelityMode::Summary,
            s if s.starts_with("truncate") => {
                // Parse "truncate:N" or "truncate(N)"
                let n = s
                    .trim_start_matches("truncate")
                    .trim_start_matches(|c: char| c == ':' || c == '(' || c.is_whitespace())
                    .trim_end_matches(')')
                    .parse::<usize>()
                    .unwrap_or(50);
                FidelityMode::Truncate { keep_last: n }
            }
            _ => FidelityMode::Full,
        }
    }
}

/// Apply fidelity mode to a message history.
/// Returns the processed messages (does NOT modify in place).
///
/// The first message (typically the system prompt) is always preserved
/// by `Truncate` mode to avoid losing LLM persona/instructions.
pub fn apply_fidelity<T: Clone>(messages: &[T], mode: &FidelityMode) -> Vec<T> {
    match mode {
        FidelityMode::Full => messages.to_vec(),
        FidelityMode::Truncate { keep_last } => {
            if messages.len() <= *keep_last {
                messages.to_vec()
            } else {
                // Always preserve the first message (system prompt), then
                // keep the most recent (keep_last - 1) messages.
                // saturating_sub handles keep_last=0 without underflow and
                // ensures the system prompt is always included.
                let tail_count = keep_last.saturating_sub(1);
                let mut result = vec![messages[0].clone()];
                if tail_count > 0 {
                    result.extend_from_slice(&messages[messages.len() - tail_count..]);
                }
                result
            }
        }
        FidelityMode::Compact | FidelityMode::Summary => {
            // These would need LLM calls to summarize; not yet implemented.
            tracing::warn!(
                "FidelityMode::{:?} is not yet implemented, using Full mode",
                mode
            );
            messages.to_vec()
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_str_parses_full() {
        assert_eq!(FidelityMode::parse("full"), FidelityMode::Full);
        assert_eq!(FidelityMode::parse("Full"), FidelityMode::Full);
        assert_eq!(FidelityMode::parse("FULL"), FidelityMode::Full);
    }

    #[test]
    fn from_str_parses_compact() {
        assert_eq!(FidelityMode::parse("compact"), FidelityMode::Compact);
        assert_eq!(FidelityMode::parse("Compact"), FidelityMode::Compact);
    }

    #[test]
    fn from_str_parses_summary() {
        assert_eq!(FidelityMode::parse("summary"), FidelityMode::Summary);
        assert_eq!(FidelityMode::parse("Summary"), FidelityMode::Summary);
    }

    #[test]
    fn from_str_parses_truncate_colon() {
        assert_eq!(
            FidelityMode::parse("truncate:20"),
            FidelityMode::Truncate { keep_last: 20 }
        );
    }

    #[test]
    fn from_str_parses_truncate_parens() {
        assert_eq!(
            FidelityMode::parse("truncate(30)"),
            FidelityMode::Truncate { keep_last: 30 }
        );
    }

    #[test]
    fn from_str_truncate_default() {
        // No number given -> defaults to 50
        assert_eq!(
            FidelityMode::parse("truncate"),
            FidelityMode::Truncate { keep_last: 50 }
        );
    }

    #[test]
    fn from_str_unknown_defaults_to_full() {
        assert_eq!(FidelityMode::parse("garbage"), FidelityMode::Full);
        assert_eq!(FidelityMode::parse(""), FidelityMode::Full);
    }

    #[test]
    fn apply_fidelity_full_keeps_all() {
        let msgs = vec![1, 2, 3, 4, 5];
        let result = apply_fidelity(&msgs, &FidelityMode::Full);
        assert_eq!(result, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn apply_fidelity_truncate_preserves_first_and_keeps_last() {
        let msgs = vec![1, 2, 3, 4, 5];
        // keep_last=3 → first message (system prompt) + last 2 messages
        let result = apply_fidelity(&msgs, &FidelityMode::Truncate { keep_last: 3 });
        assert_eq!(result, vec![1, 4, 5]);
    }

    #[test]
    fn apply_fidelity_truncate_with_fewer_messages() {
        let msgs = vec![1, 2];
        let result = apply_fidelity(&msgs, &FidelityMode::Truncate { keep_last: 10 });
        assert_eq!(result, vec![1, 2]);
    }

    #[test]
    fn apply_fidelity_truncate_keep_last_zero_preserves_first() {
        // keep_last=0 should still preserve the system prompt (first message),
        // consistent with the invariant that the first message is never dropped.
        let msgs = vec![1, 2, 3, 4, 5];
        let result = apply_fidelity(&msgs, &FidelityMode::Truncate { keep_last: 0 });
        assert_eq!(result, vec![1]);
    }

    #[test]
    fn apply_fidelity_compact_keeps_all_for_now() {
        let msgs = vec![1, 2, 3];
        let result = apply_fidelity(&msgs, &FidelityMode::Compact);
        assert_eq!(result, vec![1, 2, 3]);
    }

    #[test]
    fn apply_fidelity_summary_keeps_all_for_now() {
        let msgs = vec![1, 2, 3];
        let result = apply_fidelity(&msgs, &FidelityMode::Summary);
        assert_eq!(result, vec![1, 2, 3]);
    }

    #[test]
    fn default_is_full() {
        assert_eq!(FidelityMode::default(), FidelityMode::Full);
    }
}
