/// Output truncation strategies for tool results.
/// How to truncate output that exceeds the maximum character limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncationMode {
    /// Keep first 40% and last 60%, with a marker in the middle.
    HeadTail,
    /// Keep last `max_chars`, with a marker at the start.
    Tail,
}

/// Truncate `output` to at most `max_chars` characters using the given mode.
///
/// If the output is within the limit, it is returned unchanged.
/// Otherwise a warning marker is inserted indicating how many characters were removed.
pub fn truncate_output(output: &str, max_chars: usize, mode: TruncationMode) -> String {
    // bytes >= chars in UTF-8, so if the byte count fits, the char count must too.
    if output.len() <= max_chars {
        return output.to_string();
    }
    // Slow path: count chars directly for multibyte strings.
    let total_chars = output.chars().count();
    if total_chars <= max_chars {
        return output.to_string();
    }

    match mode {
        TruncationMode::HeadTail => {
            let head_chars = max_chars * 40 / 100;
            let tail_chars = max_chars - head_chars;
            let head_end = output
                .char_indices()
                .nth(head_chars)
                .map(|(i, _)| i)
                .unwrap_or(output.len());
            let tail_start = output
                .char_indices()
                .nth(total_chars - tail_chars)
                .map(|(i, _)| i)
                .unwrap_or(output.len());
            let removed = total_chars - max_chars;
            let head = &output[..head_end];
            let tail = &output[tail_start..];
            format!(
                "{}\n[WARNING: Output truncated. {} characters removed from middle]\n{}",
                head, removed, tail
            )
        }
        TruncationMode::Tail => {
            let skip = total_chars - max_chars;
            let tail_start = output
                .char_indices()
                .nth(skip)
                .map(|(i, _)| i)
                .unwrap_or(output.len());
            let tail = &output[tail_start..];
            format!(
                "\n[WARNING: Output truncated. {} characters removed from start]\n{}",
                skip, tail
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_truncation_when_within_limit() {
        let input = "short";
        let result = truncate_output(input, 100, TruncationMode::HeadTail);
        assert_eq!(result, input);
    }

    #[test]
    fn no_truncation_for_multibyte_content_within_char_limit() {
        // 10 emoji = 40 bytes but only 10 chars; max_chars=15 should not truncate.
        let input = "🎉🎉🎉🎉🎉🎉🎉🎉🎉🎉";
        assert_eq!(input.len(), 40);
        assert_eq!(input.chars().count(), 10);
        let result = truncate_output(input, 15, TruncationMode::HeadTail);
        assert_eq!(result, input);
    }

    #[test]
    fn head_tail_truncation() {
        // Create a string of 100 chars
        let input: String = (0..100).map(|i| char::from(b'a' + (i % 26))).collect();
        let result = truncate_output(&input, 50, TruncationMode::HeadTail);

        assert!(result.contains("[WARNING: Output truncated."));
        assert!(result.contains("characters removed from middle"));
        // Head is 40% of 50 = 20 chars, tail is 30 chars
        assert!(result.starts_with(&input[..20]));
        assert!(result.ends_with(&input[70..]));
    }

    #[test]
    fn head_tail_truncation_multibyte() {
        // 20 emoji = 80 bytes, 20 chars; max_chars=10 → head=4 chars, tail=6 chars.
        let emoji = "🎉";
        let input: String = emoji.repeat(20);
        let result = truncate_output(&input, 10, TruncationMode::HeadTail);

        assert!(result.contains("[WARNING: Output truncated. 10 characters removed from middle]"));
        assert!(result.starts_with(&emoji.repeat(4)));
        assert!(result.ends_with(&emoji.repeat(6)));
    }

    #[test]
    fn tail_truncation() {
        let input: String = (0..100).map(|i| char::from(b'a' + (i % 26))).collect();
        let result = truncate_output(&input, 50, TruncationMode::Tail);

        assert!(result.contains("[WARNING: Output truncated."));
        assert!(result.contains("characters removed from start"));
        assert!(result.ends_with(&input[50..]));
    }

    #[test]
    fn tail_truncation_multibyte() {
        // 20 emoji = 80 bytes, 20 chars; max_chars=10 should keep the last 10 emoji.
        let emoji = "🎉";
        let input: String = emoji.repeat(20);
        let result = truncate_output(&input, 10, TruncationMode::Tail);

        assert!(result.contains("[WARNING: Output truncated. 10 characters removed from start]"));
        assert!(result.ends_with(&emoji.repeat(10)));
    }
}
