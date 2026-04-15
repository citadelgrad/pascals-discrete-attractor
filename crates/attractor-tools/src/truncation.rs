/// Output truncation strategies for tool results.
/// How to truncate output that exceeds the maximum character limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncationMode {
    /// Keep first 40% and last 60%, with a marker in the middle.
    HeadTail,
    /// Keep last `max_chars`, with a marker at the start.
    Tail,
}

/// Find the largest byte index <= `target` that lies on a UTF-8 char boundary.
fn floor_char_boundary(s: &str, target: usize) -> usize {
    if target >= s.len() {
        return s.len();
    }
    let mut i = target;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Find the smallest byte index >= `target` that lies on a UTF-8 char boundary.
fn ceil_char_boundary(s: &str, target: usize) -> usize {
    if target >= s.len() {
        return s.len();
    }
    let mut i = target;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Truncate `output` to at most `max_chars` characters using the given mode.
///
/// If the output is within the limit, it is returned unchanged.
/// Otherwise a warning marker is inserted indicating how many characters were removed.
pub fn truncate_output(output: &str, max_chars: usize, mode: TruncationMode) -> String {
    // Use byte length as a fast path: if byte length is within the limit,
    // char count is guaranteed to be <= byte length, so we can skip counting.
    if output.len() <= max_chars {
        return output.to_string();
    }

    match mode {
        TruncationMode::HeadTail => {
            let head_size = floor_char_boundary(output, max_chars * 40 / 100);
            let tail_start = ceil_char_boundary(output, output.len() - (max_chars - head_size));
            let removed = output[head_size..tail_start].chars().count();
            let head = &output[..head_size];
            let tail = &output[tail_start..];
            format!(
                "{}\n[WARNING: Output truncated. {} characters removed from middle]\n{}",
                head, removed, tail
            )
        }
        TruncationMode::Tail => {
            let tail_start = ceil_char_boundary(output, output.len() - max_chars);
            let removed = output[..tail_start].chars().count();
            let tail = &output[tail_start..];
            format!(
                "\n[WARNING: Output truncated. {} characters removed from start]\n{}",
                removed, tail
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
    fn tail_truncation() {
        let input: String = (0..100).map(|i| char::from(b'a' + (i % 26))).collect();
        let result = truncate_output(&input, 50, TruncationMode::Tail);

        assert!(result.contains("[WARNING: Output truncated."));
        assert!(result.contains("characters removed from start"));
        assert!(result.ends_with(&input[50..]));
    }
}
