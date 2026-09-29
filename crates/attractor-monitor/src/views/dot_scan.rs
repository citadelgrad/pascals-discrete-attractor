//! Best-effort scan of a Pipeline `.dot` file for per-node `timeout=` values.
//!
//! The Monitor cannot use the engine's parser (ADR 0001), so this is a
//! tolerant text scan, not a DOT parser. Anything it cannot read yields no
//! timeout, never an error.

use std::collections::BTreeMap;

use chrono::Duration;

#[derive(Debug, Default, PartialEq)]
pub struct Timeouts {
    /// From a `node [timeout=...]` statement; applies to nodes without their own.
    pub default: Option<Duration>,
    pub nodes: BTreeMap<String, Duration>,
}

impl Timeouts {
    pub fn get(&self, node: &str) -> Option<Duration> {
        self.nodes.get(node).copied().or(self.default)
    }
}

fn strip_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let b = src.as_bytes();
    let (mut i, mut quoted) = (0, false);
    while i < b.len() {
        let c = b[i];
        if quoted {
            if c == b'\\' && i + 1 < b.len() {
                out.push(c as char);
                i += 1;
            } else if c == b'"' {
                quoted = false;
            }
            out.push(b[i] as char);
            i += 1;
        } else if c == b'"' {
            quoted = true;
            out.push('"');
            i += 1;
        } else if src[i..].starts_with("//") {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if src[i..].starts_with("/*") {
            i = src[i + 2..].find("*/").map_or(b.len(), |e| i + 2 + e + 2);
            out.push(' ');
        } else {
            // Push whole chars so multi-byte UTF-8 survives.
            let ch = src[i..].chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn parse_duration(v: &str) -> Option<Duration> {
    let v = v.trim().trim_matches('"');
    let digits = v.find(|c: char| !c.is_ascii_digit())?;
    let n: i64 = v[..digits].parse().ok()?;
    match &v[digits..] {
        "ms" => Some(Duration::milliseconds(n)),
        "s" => Some(Duration::seconds(n)),
        "m" => Some(Duration::minutes(n)),
        "h" => Some(Duration::hours(n)),
        "d" => Some(Duration::days(n)),
        _ => None,
    }
}

/// The `timeout` value inside one `[...]` attribute list.
fn timeout_attr(attrs: &str) -> Option<Duration> {
    let mut rest = attrs;
    while let Some(p) = rest.find("timeout") {
        let before_ok = rest[..p]
            .chars()
            .next_back()
            .is_none_or(|c| c.is_whitespace() || c == ',' || c == ';');
        let after = rest[p + 7..].trim_start();
        if before_ok {
            if let Some(v) = after.strip_prefix('=') {
                let v = v.trim_start();
                let end = if let Some(q) = v.strip_prefix('"') {
                    q.find('"').map_or(v.len(), |e| e + 2)
                } else {
                    v.find(|c: char| c.is_whitespace() || c == ',' || c == ';')
                        .unwrap_or(v.len())
                };
                return parse_duration(&v[..end]);
            }
        }
        rest = &rest[p + 7..];
    }
    None
}

pub fn scan(src: &str) -> Timeouts {
    let src = strip_comments(src);
    let mut out = Timeouts::default();
    let mut from = 0;
    while let Some(open) = src[from..].find('[').map(|i| i + from) {
        let Some(close) = src[open..].find(']').map(|i| i + open) else {
            break;
        };
        from = close + 1;
        let head = src[..open].trim_end();
        // An edge statement's attributes belong to an edge, not a node.
        let id_start = head
            .rfind(|c: char| c.is_whitespace() || c == ';' || c == '{' || c == '}')
            .map_or(0, |i| i + 1);
        let id = head[id_start..].trim_matches('"');
        let prefix = head[..id_start].trim_end();
        if id.is_empty() || prefix.ends_with("->") || prefix.ends_with("--") {
            continue;
        }
        let Some(t) = timeout_attr(&src[open + 1..close]) else {
            continue;
        };
        match id {
            "node" => out.default = Some(t),
            "graph" | "edge" => {}
            _ => {
                out.nodes.insert(id.to_string(), t);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_node_timeouts_in_every_separator_style() {
        let t = scan(
            "digraph g {\n a [label=\"A\", timeout=600s]\n b [label=\"B\"; timeout=15m];\n c [label=\"C\" timeout=250ms]\n d [timeout=\"2h\"]\n}",
        );
        assert_eq!(t.get("a"), Some(Duration::seconds(600)));
        assert_eq!(t.get("b"), Some(Duration::minutes(15)));
        assert_eq!(t.get("c"), Some(Duration::milliseconds(250)));
        assert_eq!(t.get("d"), Some(Duration::hours(2)));
        assert_eq!(t.get("zzz"), None);
    }

    #[test]
    fn node_default_applies_to_nodes_without_their_own() {
        let t = scan("digraph { node [shape=box, timeout=30s]\n a [timeout=5s]\n b [label=x] }");
        assert_eq!(t.get("a"), Some(Duration::seconds(5)));
        assert_eq!(t.get("b"), Some(Duration::seconds(30)));
    }

    #[test]
    fn comments_edges_and_junk_are_ignored() {
        let t = scan(
            "digraph {\n // a [timeout=1s]\n /* b [timeout=2s] */\n x -> y [timeout=9s]\n # z\n q [label=\"timeout=3s\"]\n r [timeout=abc]\n s [notimeout=4s]\n é [timeout=7s] }",
        );
        assert_eq!(t.get("a"), None);
        assert_eq!(t.get("b"), None);
        assert_eq!(t.get("y"), None);
        assert_eq!(t.get("r"), None);
        assert_eq!(t.get("s"), None);
        assert_eq!(t.get("é"), Some(Duration::seconds(7)));
        assert_eq!(t.get("q"), None, "a label that mentions timeout is not one");
    }

    #[test]
    fn garbage_and_unclosed_input_do_not_panic() {
        assert_eq!(scan(""), Timeouts::default());
        assert_eq!(scan("a [timeout=5s"), Timeouts::default());
        let _ = scan("\"unterminated [timeout=5s]");
        let _ = scan("a [timeout=\"5s");
    }
}
