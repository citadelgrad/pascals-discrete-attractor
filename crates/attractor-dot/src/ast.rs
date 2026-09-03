use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DotGraph {
    pub name: String,
    pub attrs: HashMap<String, AttributeValue>,
    pub nodes: HashMap<String, NodeDef>,
    pub edges: Vec<EdgeDef>,
    pub subgraphs: Vec<SubgraphDef>,
    pub node_defaults: HashMap<String, AttributeValue>,
    pub edge_defaults: HashMap<String, AttributeValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeDef {
    pub id: String,
    /// Effective attributes after DOT defaults have been applied.
    pub attrs: HashMap<String, AttributeValue>,
    /// Attributes written directly on this node in source.
    #[serde(default)]
    pub authored_attrs: HashMap<String, AttributeValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeDef {
    pub from: String,
    pub to: String,
    pub attrs: HashMap<String, AttributeValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubgraphDef {
    pub name: Option<String>,
    pub attrs: HashMap<String, AttributeValue>,
    pub nodes: HashMap<String, NodeDef>,
    pub edges: Vec<EdgeDef>,
    pub node_defaults: HashMap<String, AttributeValue>,
    pub edge_defaults: HashMap<String, AttributeValue>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AttributeValue {
    String(String),
    Integer(i64),
    Float(f64),
    Boolean(bool),
    #[serde(with = "crate::duration_serde")]
    Duration(Duration),
}
