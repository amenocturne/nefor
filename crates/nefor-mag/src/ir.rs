use crate::ast::{GraphValue, NodeValue, Value};
use crate::types::MagType;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphIr {
    pub nodes: Vec<NodeIr>,
    pub edges: Vec<EdgeIr>,
    pub terminals: Vec<String>,
    pub hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeIr {
    pub id: String,
    #[serde(rename = "type")]
    pub node_type: String,
    pub args: serde_json::Value,
    #[serde(rename = "in")]
    pub input_type: String,
    #[serde(rename = "out")]
    pub output_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeIr {
    pub from: String,
    pub to: String,
}

fn value_to_json(val: &Value) -> serde_json::Value {
    match val {
        Value::Str(s) => serde_json::Value::String(s.clone()),
        Value::Int(n) => serde_json::json!(n),
        Value::Float(n) => serde_json::json!(n),
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Nil => serde_json::Value::Null,
        Value::Keyword(k) => serde_json::Value::String(format!(":{k}")),
        Value::Symbol(s) => serde_json::Value::String(s.clone()),
        Value::List(items) | Value::Vector(items) => {
            serde_json::Value::Array(items.iter().map(value_to_json).collect())
        }
        Value::Map(map) => {
            let obj: serde_json::Map<String, serde_json::Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), value_to_json(v)))
                .collect();
            serde_json::Value::Object(obj)
        }
        _ => serde_json::Value::Null,
    }
}

fn node_to_ir(node: &NodeValue) -> NodeIr {
    let args: serde_json::Map<String, serde_json::Value> = node
        .args
        .iter()
        .map(|(k, v)| (k.clone(), value_to_json(v)))
        .collect();

    NodeIr {
        id: node.id.clone(),
        node_type: node.node_type.clone(),
        args: serde_json::Value::Object(args),
        input_type: node.input_type.to_string(),
        output_type: node.output_type.to_string(),
    }
}

pub fn normalize(graph: GraphValue) -> GraphIr {
    let mut nodes: Vec<NodeIr> = graph.nodes.iter().map(node_to_ir).collect();
    nodes.sort_by(|a, b| a.id.cmp(&b.id));

    let mut edges: Vec<EdgeIr> = graph
        .edges
        .iter()
        .map(|e| EdgeIr {
            from: e.from.clone(),
            to: e.to.clone(),
        })
        .collect();
    edges.sort_by(|a, b| (&a.from, &a.to).cmp(&(&b.from, &b.to)));

    let mut terminals = graph.terminals.clone();
    terminals.sort();

    let canonical = serde_json::json!({
        "nodes": nodes,
        "edges": edges,
        "terminals": terminals,
    });
    let hash = {
        let mut hasher = Sha256::new();
        hasher.update(canonical.to_string().as_bytes());
        format!("sha256:{:x}", hasher.finalize())
    };

    GraphIr {
        nodes,
        edges,
        terminals,
        hash,
    }
}
