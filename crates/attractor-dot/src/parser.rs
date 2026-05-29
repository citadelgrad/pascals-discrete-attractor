use std::collections::HashMap;

use winnow::combinator::{alt, opt};
use winnow::error::{ContextError, ErrMode, StrContext, StrContextValue};
use winnow::token::literal;
use winnow::{ModalResult, Parser};

use crate::ast::*;

#[path = "parser_primitives.rs"]
mod primitives;
use primitives::*;

fn make_cut_error(desc: &'static str) -> ErrMode<ContextError<StrContext>> {
    let mut e = ContextError::new();
    e.push(StrContext::Expected(StrContextValue::Description(desc)));
    ErrMode::Cut(e)
}

/// Strip `//` line comments and `/* */` block comments from the input.
pub(crate) fn strip_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            // line comment — skip until newline
            i += 2;
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
        } else if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            // block comment — skip until */
            i += 2;
            while i + 1 < len && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                // preserve newlines so line numbers stay correct
                if bytes[i] == b'\n' {
                    out.push('\n');
                }
                i += 1;
            }
            if i + 1 < len {
                i += 2; // skip */
            }
        } else if bytes[i] == b'"' {
            // inside a string literal — copy verbatim, handling escapes
            out.push('"');
            i += 1;
            while i < len {
                if bytes[i] == b'\\' && i + 1 < len {
                    out.push(bytes[i] as char);
                    out.push(bytes[i + 1] as char);
                    i += 2;
                } else if bytes[i] == b'"' {
                    out.push('"');
                    i += 1;
                    break;
                } else {
                    out.push(bytes[i] as char);
                    i += 1;
                }
            }
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

/// Parse an attribute value.
fn attr_value(input: &mut &str) -> ModalResult<AttributeValue> {
    alt((
        quoted_string.map(AttributeValue::String),
        boolean_value.map(AttributeValue::Boolean),
        duration_value.map(AttributeValue::Duration),
        float_value.map(AttributeValue::Float),
        integer_value.map(AttributeValue::Integer),
    ))
    .parse_next(input)
}

/// Parse a single attribute: key '=' value.
fn attr(input: &mut &str) -> ModalResult<(String, AttributeValue)> {
    let key = qualified_or_plain_id.parse_next(input)?;
    let _ = ws.parse_next(input)?;
    let _ = '='.parse_next(input)?;
    let _ = ws.parse_next(input)?;
    let value = attr_value
        .context(StrContext::Expected(StrContextValue::Description(
            "attribute value",
        )))
        .parse_next(input)?;
    Ok((key, value))
}

/// Parse an attribute block: '[' attr ( ','? attr )* ']'
fn attr_block(input: &mut &str) -> ModalResult<HashMap<String, AttributeValue>> {
    let _ = '['.parse_next(input)?;
    let _ = ws.parse_next(input)?;

    let mut attrs = HashMap::new();
    // Try parsing first attr
    if let Some(first) = opt(attr).parse_next(input)? {
        attrs.insert(first.0, first.1);
        loop {
            let _ = ws.parse_next(input)?;
            // optional comma or semicolon separator
            let _ = opt(alt((',', ';'))).parse_next(input)?;
            let _ = ws.parse_next(input)?;
            if let Some(a) = opt(attr).parse_next(input)? {
                attrs.insert(a.0, a.1);
            } else {
                break;
            }
        }
    }

    let _ = ws.parse_next(input)?;
    let _ = ']'.parse_next(input)?;
    Ok(attrs)
}

/// Intermediate representation of a parsed statement, before we merge them into a DotGraph.
enum Statement {
    GraphAttrs(HashMap<String, AttributeValue>),
    NodeDefaults(HashMap<String, AttributeValue>),
    EdgeDefaults(HashMap<String, AttributeValue>),
    Node(String, HashMap<String, AttributeValue>),
    Edge(Vec<String>, HashMap<String, AttributeValue>),
    Subgraph(Option<String>, Vec<Statement>),
    GraphAttrDecl(String, AttributeValue),
}

/// Parse 'graph' '[' ... ']' ';'?
fn graph_attr_stmt(input: &mut &str) -> ModalResult<Statement> {
    let _ = literal("graph").parse_next(input)?;
    let _ = ws.parse_next(input)?;
    let attrs = attr_block.parse_next(input)?;
    let _ = ws.parse_next(input)?;
    let _ = opt(';').parse_next(input)?;
    Ok(Statement::GraphAttrs(attrs))
}

/// Parse 'node' '[' ... ']' ';'?
fn node_defaults_stmt(input: &mut &str) -> ModalResult<Statement> {
    let _ = literal("node").parse_next(input)?;
    let _ = ws.parse_next(input)?;
    let attrs = attr_block.parse_next(input)?;
    let _ = ws.parse_next(input)?;
    let _ = opt(';').parse_next(input)?;
    Ok(Statement::NodeDefaults(attrs))
}

/// Parse 'edge' '[' ... ']' ';'?
fn edge_defaults_stmt(input: &mut &str) -> ModalResult<Statement> {
    let _ = literal("edge").parse_next(input)?;
    let _ = ws.parse_next(input)?;
    let attrs = attr_block.parse_next(input)?;
    let _ = ws.parse_next(input)?;
    let _ = opt(';').parse_next(input)?;
    Ok(Statement::EdgeDefaults(attrs))
}

/// Parse 'subgraph' identifier? '{' statement* '}'
fn subgraph_stmt(input: &mut &str) -> ModalResult<Statement> {
    let _ = literal("subgraph").parse_next(input)?;
    let _ = ws.parse_next(input)?;
    let name = opt(identifier).parse_next(input)?;
    let _ = ws.parse_next(input)?;
    let _ = '{'.parse_next(input)?;
    let _ = ws.parse_next(input)?;
    let stmts = statements.parse_next(input)?;
    let _ = ws.parse_next(input)?;
    let _ = '}'.parse_next(input)?;
    let _ = ws.parse_next(input)?;
    let _ = opt(';').parse_next(input)?;
    Ok(Statement::Subgraph(name.map(|s| s.to_string()), stmts))
}

/// Parse a node or edge statement. An edge starts as an identifier then has '->'.
fn node_or_edge_stmt(input: &mut &str) -> ModalResult<Statement> {
    let first = identifier.parse_next(input)?;
    let _ = ws.parse_next(input)?;

    // Check for edge: '->'
    let arrow = opt(literal("->")).parse_next(input)?;
    if arrow.is_some() {
        // Edge statement — collect chain
        let mut chain = vec![first.to_string()];
        let _ = ws.parse_next(input)?;
        let next = identifier
            .context(StrContext::Expected(StrContextValue::Description(
                "edge target identifier",
            )))
            .parse_next(input)?;
        chain.push(next.to_string());

        loop {
            let _ = ws.parse_next(input)?;
            if opt(literal("->")).parse_next(input)?.is_some() {
                let _ = ws.parse_next(input)?;
                let n = identifier
                    .context(StrContext::Expected(StrContextValue::Description(
                        "edge target identifier",
                    )))
                    .parse_next(input)?;
                chain.push(n.to_string());
            } else {
                break;
            }
        }

        let _ = ws.parse_next(input)?;
        let attrs = opt(attr_block).parse_next(input)?.unwrap_or_default();
        let _ = ws.parse_next(input)?;
        let _ = opt(';').parse_next(input)?;
        return Ok(Statement::Edge(chain, attrs));
    }

    // Check for '--' to give a better error
    if opt(literal("--")).parse_next(input)?.is_some() {
        return Err(make_cut_error("only directed edges (->); undirected edges (--) are not supported"));
    }

    // Check if there's an attr block => node with attrs
    let attrs = opt(attr_block).parse_next(input)?.unwrap_or_default();
    let _ = ws.parse_next(input)?;

    // Check for `key = value` style graph attribute declaration (no attr block on identifier)
    // This is a node stmt if attrs is non-empty or there's a semicolon/end.
    // But we also need to handle bare `key = value` graph attrs.
    // If we got an attr block, it's a node statement.
    if !attrs.is_empty() {
        let _ = opt(';').parse_next(input)?;
        return Ok(Statement::Node(first.to_string(), attrs));
    }

    // Check for '=' which means this is a graph attr decl: key = value
    if opt('=').parse_next(input)?.is_some() {
        let _ = ws.parse_next(input)?;
        let val = attr_value.parse_next(input)?;
        let _ = ws.parse_next(input)?;
        let _ = opt(';').parse_next(input)?;
        return Ok(Statement::GraphAttrDecl(first.to_string(), val));
    }

    let _ = opt(';').parse_next(input)?;
    Ok(Statement::Node(first.to_string(), attrs))
}

/// Parse a single statement.
fn statement(input: &mut &str) -> ModalResult<Statement> {
    let _ = ws.parse_next(input)?;
    alt((
        graph_attr_stmt,
        node_defaults_stmt,
        edge_defaults_stmt,
        subgraph_stmt,
        node_or_edge_stmt,
    ))
    .parse_next(input)
}

/// Parse zero or more statements.
fn statements(input: &mut &str) -> ModalResult<Vec<Statement>> {
    let mut stmts = Vec::new();
    loop {
        let _ = ws.parse_next(input)?;
        if input.is_empty() || input.starts_with('}') {
            break;
        }
        let stmt = statement.parse_next(input)?;
        stmts.push(stmt);
    }
    Ok(stmts)
}

type MergeResult = (
    HashMap<String, AttributeValue>,   // graph attrs
    HashMap<String, NodeDef>,          // nodes
    Vec<EdgeDef>,                      // edges
    Vec<SubgraphDef>,                  // subgraphs
    HashMap<String, AttributeValue>,   // node defaults
    HashMap<String, AttributeValue>,   // edge defaults
);

/// Merge statements into a DotGraph-like structure.
#[allow(clippy::type_complexity)]
fn merge_statements(
    stmts: Vec<Statement>,
    parent_node_defaults: &HashMap<String, AttributeValue>,
    parent_edge_defaults: &HashMap<String, AttributeValue>,
) -> MergeResult {
    let mut graph_attrs = HashMap::new();
    let mut nodes = HashMap::new();
    let mut edges = Vec::new();
    let mut subgraphs = Vec::new();
    let mut node_defaults: HashMap<String, AttributeValue> = parent_node_defaults.clone();
    let mut edge_defaults: HashMap<String, AttributeValue> = parent_edge_defaults.clone();

    for stmt in stmts {
        match stmt {
            Statement::GraphAttrs(attrs) => {
                graph_attrs.extend(attrs);
            }
            Statement::NodeDefaults(attrs) => {
                node_defaults.extend(attrs);
            }
            Statement::EdgeDefaults(attrs) => {
                edge_defaults.extend(attrs);
            }
            Statement::Node(id, mut attrs) => {
                // Apply node defaults for keys not explicitly set
                for (k, v) in &node_defaults {
                    attrs.entry(k.clone()).or_insert_with(|| v.clone());
                }
                nodes.insert(id.clone(), NodeDef { id, attrs });
            }
            Statement::Edge(chain, attrs) => {
                // Expand chained edges: A -> B -> C => (A,B), (B,C)
                for pair in chain.windows(2) {
                    let mut merged = edge_defaults.clone();
                    merged.extend(attrs.clone());
                    edges.push(EdgeDef {
                        from: pair[0].clone(),
                        to: pair[1].clone(),
                        attrs: merged,
                    });
                    // Ensure nodes referenced in edges exist
                    for node_id in &chain {
                        nodes.entry(node_id.clone()).or_insert_with(|| {
                            let mut na = HashMap::new();
                            for (k, v) in &node_defaults {
                                na.insert(k.clone(), v.clone());
                            }
                            NodeDef {
                                id: node_id.clone(),
                                attrs: na,
                            }
                        });
                    }
                }
            }
            Statement::Subgraph(name, inner_stmts) => {
                let (sg_attrs, sg_nodes, sg_edges, sg_subgraphs, sg_nd, sg_ed) =
                    merge_statements(inner_stmts, &node_defaults, &edge_defaults);
                // nested subgraphs get pushed into the parent's subgraphs list
                let sg = SubgraphDef {
                    name,
                    attrs: sg_attrs,
                    nodes: sg_nodes,
                    edges: sg_edges,
                    node_defaults: sg_nd,
                    edge_defaults: sg_ed,
                };
                subgraphs.push(sg);
                for nested in sg_subgraphs {
                    subgraphs.push(nested);
                }
            }
            Statement::GraphAttrDecl(key, val) => {
                graph_attrs.insert(key, val);
            }
        }
    }

    (graph_attrs, nodes, edges, subgraphs, node_defaults, edge_defaults)
}

/// Top-level parser: 'digraph' identifier '{' statements '}'.
fn parse_digraph(input: &mut &str) -> ModalResult<DotGraph> {
    let _ = ws.parse_next(input)?;

    // Reject 'strict'
    if input.starts_with("strict") {
        return Err(make_cut_error("'digraph' keyword (strict graphs are not supported)"));
    }

    // Reject undirected 'graph'
    if input.starts_with("graph") && !input.starts_with("graph [") {
        let after = &input[5..];
        let trimmed = after.trim_start();
        if trimmed.starts_with('{') || trimmed.starts_with(|c: char| c.is_ascii_alphabetic()) {
            return Err(make_cut_error("'digraph' keyword (undirected graphs are not supported)"));
        }
    }

    let _ = literal("digraph")
        .context(StrContext::Expected(StrContextValue::StringLiteral(
            "digraph",
        )))
        .parse_next(input)?;
    let _ = ws.parse_next(input)?;
    let name = identifier
        .context(StrContext::Expected(StrContextValue::Description(
            "graph name identifier",
        )))
        .parse_next(input)?;
    let _ = ws.parse_next(input)?;
    let _ = '{'.parse_next(input)?;
    let _ = ws.parse_next(input)?;
    let stmts = statements.parse_next(input)?;
    let _ = ws.parse_next(input)?;
    let _ = '}'.parse_next(input)?;
    let _ = ws.parse_next(input)?;

    let empty = HashMap::new();
    let (attrs, nodes, edges, subgraphs, node_defaults, edge_defaults) =
        merge_statements(stmts, &empty, &empty);

    Ok(DotGraph {
        name: name.to_string(),
        attrs,
        nodes,
        edges,
        subgraphs,
        node_defaults,
        edge_defaults,
    })
}

/// Compute (line, col) from byte offset in the original (pre-stripped) text.
fn offset_to_line_col(original: &str, remaining_len: usize, stripped_len: usize) -> (usize, usize) {
    let consumed = stripped_len - remaining_len;
    // We map back into the original. Since strip_comments preserves newlines,
    // the line count stays the same. We just count from the stripped text.
    let prefix = &original[..consumed.min(original.len())];
    let line = prefix.matches('\n').count() + 1;
    let col = match prefix.rfind('\n') {
        Some(pos) => consumed - pos,
        None => consumed + 1,
    };
    (line, col)
}

/// Public entry point.
pub fn parse(input: &str) -> std::result::Result<DotGraph, attractor_types::AttractorError> {
    let stripped = strip_comments(input);
    let mut remaining = stripped.as_str();
    let stripped_len = stripped.len();

    parse_digraph.parse_next(&mut remaining).map_err(|e| {
        let (line, col) = offset_to_line_col(&stripped, remaining.len(), stripped_len);
        let message = format!("{}", e);

        // Extract a source snippet around the error location
        let snippet = remaining.chars().take(40).collect::<String>();
        let source_snippet = if snippet.is_empty() {
            None
        } else {
            Some(snippet)
        };

        attractor_types::AttractorError::ParseError {
            line,
            col,
            message,
            source_snippet,
        }
    })
}
