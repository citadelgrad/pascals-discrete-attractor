//! CSS-like model configuration system for pipeline graphs.
//!
//! Parses stylesheet rules with selectors (`*`, `#id`, `.class`) and applies
//! supported model/provider declarations to pipeline graph nodes respecting
//! specificity ordering. Recognized unsupported declarations are retained for
//! canonical compilation to reject rather than silently ignore.

use crate::graph::{PipelineGraph, PipelineNode};
use attractor_types::AttractorError;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Stylesheet {
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    pub selector: Selector,
    pub declarations: Vec<Declaration>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Selector {
    /// `*` — matches every node.
    Universal,
    /// `#node_id` — matches a node by its id.
    Id(String),
    /// `.class_name` — matches nodes that contain the class.
    Class(String),
}

impl Selector {
    pub fn specificity(&self) -> u8 {
        match self {
            Selector::Universal => 0,
            Selector::Class(_) => 1,
            Selector::Id(_) => 2,
        }
    }

    pub fn matches(&self, node: &PipelineNode) -> bool {
        match self {
            Selector::Universal => true,
            Selector::Id(id) => node.id == *id,
            Selector::Class(class) => node.classes.contains(class),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Declaration {
    pub property: String,
    pub value: String,
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

struct Parser<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, pos: 0 }
    }

    fn remaining(&self) -> &'a str {
        &self.input[self.pos..]
    }

    fn skip_whitespace_and_comments(&mut self) {
        loop {
            // Skip whitespace
            while self.pos < self.input.len()
                && self.input.as_bytes()[self.pos].is_ascii_whitespace()
            {
                self.pos += 1;
            }
            // Skip line comments (// ...)
            if self.remaining().starts_with("//") {
                while self.pos < self.input.len() && self.input.as_bytes()[self.pos] != b'\n' {
                    self.pos += 1;
                }
                continue;
            }
            // Skip block comments (/* ... */)
            if self.remaining().starts_with("/*") {
                self.pos += 2;
                while self.pos + 1 < self.input.len() {
                    if &self.input[self.pos..self.pos + 2] == "*/" {
                        self.pos += 2;
                        break;
                    }
                    self.pos += 1;
                }
                continue;
            }
            break;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.as_bytes().get(self.pos).copied()
    }

    fn expect(&mut self, ch: u8) -> Result<(), AttractorError> {
        self.skip_whitespace_and_comments();
        match self.peek() {
            Some(c) if c == ch => {
                self.pos += 1;
                Ok(())
            }
            other => Err(self.error(format!(
                "expected '{}', found '{}'",
                ch as char,
                other.map(|c| c as char).unwrap_or('\0')
            ))),
        }
    }

    fn parse_identifier(&mut self) -> Result<String, AttractorError> {
        self.skip_whitespace_and_comments();
        let start = self.pos;
        while self.pos < self.input.len() {
            let c = self.input.as_bytes()[self.pos];
            if c.is_ascii_alphanumeric() || c == b'_' || c == b'-' {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos == start {
            return Err(self.error("expected identifier".to_string()));
        }
        Ok(self.input[start..self.pos].to_string())
    }

    fn parse_value(&mut self) -> Result<String, AttractorError> {
        self.skip_whitespace_and_comments();
        let start = self.pos;
        while self.pos < self.input.len() {
            let c = self.input.as_bytes()[self.pos];
            if c == b';' || c == b'}' {
                break;
            }
            self.pos += 1;
        }
        let val = self.input[start..self.pos].trim();
        if val.is_empty() {
            return Err(self.error("expected property value".to_string()));
        }
        Ok(val.to_string())
    }

    fn error(&self, message: String) -> AttractorError {
        // Compute line/col from pos
        let consumed = &self.input[..self.pos];
        let line = consumed.chars().filter(|&c| c == '\n').count() + 1;
        let col = consumed
            .rfind('\n')
            .map(|i| self.pos - i)
            .unwrap_or(self.pos + 1);
        AttractorError::ParseError {
            line,
            col,
            message,
            source_snippet: None,
        }
    }

    fn parse_selector(&mut self) -> Result<Selector, AttractorError> {
        self.skip_whitespace_and_comments();
        match self.peek() {
            Some(b'*') => {
                self.pos += 1;
                Ok(Selector::Universal)
            }
            Some(b'#') => {
                self.pos += 1;
                let id = self.parse_identifier()?;
                Ok(Selector::Id(id))
            }
            Some(b'.') => {
                self.pos += 1;
                let class = self.parse_identifier()?;
                Ok(Selector::Class(class))
            }
            other => Err(self.error(format!(
                "expected selector ('*', '#', or '.'), found '{}'",
                other.map(|c| c as char).unwrap_or('\0')
            ))),
        }
    }

    fn parse_declaration(&mut self) -> Result<Declaration, AttractorError> {
        let property = self.parse_identifier()?;
        self.expect(b':')?;
        let value = self.parse_value()?;
        Ok(Declaration { property, value })
    }

    fn parse_rule(&mut self) -> Result<Rule, AttractorError> {
        let selector = self.parse_selector()?;
        self.expect(b'{')?;

        let mut declarations = Vec::new();
        loop {
            self.skip_whitespace_and_comments();
            match self.peek() {
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                None => return Err(self.error("unexpected end of input in rule body".to_string())),
                _ => {
                    let decl = self.parse_declaration()?;
                    declarations.push(decl);
                    // Consume optional semicolon
                    self.skip_whitespace_and_comments();
                    if self.peek() == Some(b';') {
                        self.pos += 1;
                    }
                }
            }
        }

        Ok(Rule {
            selector,
            declarations,
        })
    }

    fn parse_stylesheet(&mut self) -> Result<Stylesheet, AttractorError> {
        let mut rules = Vec::new();
        loop {
            self.skip_whitespace_and_comments();
            if self.pos >= self.input.len() {
                break;
            }
            rules.push(self.parse_rule()?);
        }
        if rules.is_empty() {
            return Err(self.error("stylesheet must contain at least one rule".to_string()));
        }
        Ok(Stylesheet { rules })
    }
}

/// Parse a CSS-like model stylesheet from a string.
pub fn parse_stylesheet(input: &str) -> Result<Stylesheet, AttractorError> {
    let mut parser = Parser::new(input);
    parser.parse_stylesheet()
}

// ---------------------------------------------------------------------------
// Application
// ---------------------------------------------------------------------------

/// Apply a stylesheet to a `PipelineGraph`, mutating nodes in-place.
///
/// Rules are applied in specificity order (Universal < Class < Id).
/// Within the same specificity level, later rules in the stylesheet win.
/// Explicitly authored node attributes are never overwritten; stylesheet rules
/// do override lower-precedence DOT node defaults.
pub fn apply_stylesheet(graph: &mut PipelineGraph, stylesheet: &Stylesheet) {
    let authored_attrs = graph.authored_node_attrs().clone();
    for node in graph.all_nodes_mut() {
        let authored = authored_attrs.get(&node.id);
        let had_llm_model = authored.is_some_and(|attrs| attrs.contains("llm_model"));
        let had_llm_provider = authored.is_some_and(|attrs| attrs.contains("llm_provider"));

        // Collect matching rules, sorted by specificity ascending so that
        // higher-specificity rules overwrite lower-specificity ones.
        let mut matching: Vec<&Rule> = stylesheet
            .rules
            .iter()
            .filter(|r| r.selector.matches(node))
            .collect();
        matching.sort_by_key(|r| r.selector.specificity());

        // Apply declarations from lowest to highest specificity.
        // Higher specificity overwrites lower specificity values.
        // Explicit node attributes are never overwritten.
        for rule in matching {
            for decl in &rule.declarations {
                match decl.property.as_str() {
                    "llm_model" if !had_llm_model => {
                        node.llm_model = Some(decl.value.clone());
                    }
                    "llm_provider" if !had_llm_provider => {
                        node.llm_provider = Some(decl.value.clone());
                    }
                    "reasoning_effort" => {
                        node.raw_attrs.insert(
                            "reasoning_effort".into(),
                            attractor_dot::AttributeValue::String(decl.value.clone()),
                        );
                    }
                    _ => {}
                }
            }
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
    fn parse_universal_selector_rule() {
        let css = r#"* { llm_model: claude-sonnet-4-20250514; }"#;
        let ss = parse_stylesheet(css).unwrap();
        assert_eq!(ss.rules.len(), 1);
        assert_eq!(ss.rules[0].selector, Selector::Universal);
        assert_eq!(ss.rules[0].declarations.len(), 1);
        assert_eq!(ss.rules[0].declarations[0].property, "llm_model");
        assert_eq!(
            ss.rules[0].declarations[0].value,
            "claude-sonnet-4-20250514"
        );
    }

    #[test]
    fn parse_id_selector_rule() {
        let css = r#"#summarize { llm_provider: anthropic; reasoning_effort: high; }"#;
        let ss = parse_stylesheet(css).unwrap();
        assert_eq!(ss.rules.len(), 1);
        assert_eq!(ss.rules[0].selector, Selector::Id("summarize".to_string()));
        assert_eq!(ss.rules[0].declarations.len(), 2);
        assert_eq!(ss.rules[0].declarations[0].property, "llm_provider");
        assert_eq!(ss.rules[0].declarations[0].value, "anthropic");
        assert_eq!(ss.rules[0].declarations[1].property, "reasoning_effort");
        assert_eq!(ss.rules[0].declarations[1].value, "high");
    }

    #[test]
    fn parse_class_selector_rule() {
        let css = r#".fast { llm_model: gpt-4o-mini; }"#;
        let ss = parse_stylesheet(css).unwrap();
        assert_eq!(ss.rules.len(), 1);
        assert_eq!(ss.rules[0].selector, Selector::Class("fast".to_string()));
        assert_eq!(ss.rules[0].declarations[0].value, "gpt-4o-mini");
    }

    #[test]
    fn parse_multiple_rules() {
        let css = r#"
            * { llm_model: default-model; }
            .fast { llm_model: fast-model; }
            #special { llm_model: special-model; }
        "#;
        let ss = parse_stylesheet(css).unwrap();
        assert_eq!(ss.rules.len(), 3);
        assert_eq!(ss.rules[0].selector, Selector::Universal);
        assert_eq!(ss.rules[1].selector, Selector::Class("fast".to_string()));
        assert_eq!(ss.rules[2].selector, Selector::Id("special".to_string()));
    }

    #[test]
    fn parse_trailing_semicolon_optional() {
        let css = r#"* { llm_model: m }"#;
        let ss = parse_stylesheet(css).unwrap();
        assert_eq!(ss.rules[0].declarations[0].value, "m");
    }

    #[test]
    fn parse_empty_input_is_error() {
        let result = parse_stylesheet("   ");
        assert!(result.is_err());
    }

    // --- Application tests ---

    fn make_test_graph() -> PipelineGraph {
        let dot = r#"digraph G {
            start [shape="Mdiamond"]
            analyze [label="Analyze", class="fast"]
            summarize [label="Summarize", class="slow"]
            done [shape="Msquare"]
            start -> analyze -> summarize -> done
        }"#;
        let graph = attractor_dot::parse(dot).unwrap();
        PipelineGraph::from_dot(graph).unwrap()
    }

    #[test]
    fn apply_universal_sets_model_on_all_nodes() {
        let mut graph = make_test_graph();
        let ss = parse_stylesheet(r#"* { llm_model: default-model; }"#).unwrap();
        apply_stylesheet(&mut graph, &ss);

        for node in graph.all_nodes() {
            assert_eq!(
                node.llm_model.as_deref(),
                Some("default-model"),
                "node '{}' should have llm_model set",
                node.id
            );
        }
    }

    #[test]
    fn apply_id_overrides_class_specificity() {
        let mut graph = make_test_graph();
        let css = r#"
            .fast { llm_model: fast-model; }
            #analyze { llm_model: analyze-model; }
        "#;
        let ss = parse_stylesheet(css).unwrap();
        apply_stylesheet(&mut graph, &ss);

        // The #analyze ID selector (specificity 2) should win over .fast (specificity 1).
        // Both match the "analyze" node, but ID has higher specificity so it overwrites.
        let node = graph.node("analyze").unwrap();
        assert_eq!(node.llm_model.as_deref(), Some("analyze-model"));
    }

    #[test]
    fn explicit_node_attribute_overrides_stylesheet() {
        let dot = r#"digraph G {
            work [llm_model="explicit-model"]
        }"#;
        let graph_dot = attractor_dot::parse(dot).unwrap();
        let mut graph = PipelineGraph::from_dot(graph_dot).unwrap();

        let ss = parse_stylesheet(r#"* { llm_model: stylesheet-model; }"#).unwrap();
        apply_stylesheet(&mut graph, &ss);

        let node = graph.node("work").unwrap();
        assert_eq!(node.llm_model.as_deref(), Some("explicit-model"));
    }

    #[test]
    fn specificity_ordering() {
        assert_eq!(Selector::Universal.specificity(), 0);
        assert_eq!(Selector::Class("x".into()).specificity(), 1);
        assert_eq!(Selector::Id("x".into()).specificity(), 2);
    }
}
