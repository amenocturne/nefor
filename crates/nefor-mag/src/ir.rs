use crate::ast::{GraphValue, NodeValue, Value};
use crate::types::MagType;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphIr {
    pub terminal: String,
    pub nodes: Vec<NodeIr>,
    pub edges: Vec<EdgeIr>,
    pub hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeIr {
    pub id: String,
    pub reasoner: String,
    pub args: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fanout: Option<FanoutIr>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FanoutIr {
    #[serde(rename = "in")]
    pub input: String,
    pub out: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeIr {
    pub from: String,
    pub to: String,
    #[serde(rename = "type")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edge_type: Option<String>,
}

fn qualify_type(ty: &MagType) -> String {
    match ty {
        MagType::Named(name) => {
            if name.contains('.') {
                name.clone()
            } else {
                format!("mag.{name}")
            }
        }
        MagType::Var(name) => format!("mag.{name}"),
        MagType::Union(types) => {
            let parts: Vec<_> = types.iter().map(qualify_type).collect();
            parts.join("|")
        }
        MagType::Intersection(types) => {
            let parts: Vec<_> = types.iter().map(qualify_type).collect();
            parts.join("+")
        }
    }
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

pub(crate) fn node_to_ir(node: &NodeValue) -> NodeIr {
    let args: serde_json::Map<String, serde_json::Value> = node
        .args
        .iter()
        .map(|(k, v)| (k.clone(), value_to_json(v)))
        .collect();

    let fanout = match &node.output_type {
        MagType::Union(variants) => Some(FanoutIr {
            input: qualify_type(&node.input_type),
            out: variants.iter().map(qualify_type).collect(),
        }),
        _ => None,
    };

    NodeIr {
        id: node.id.clone(),
        reasoner: node.node_type.clone(),
        args: serde_json::Value::Object(args),
        fanout,
    }
}

fn compute_edge_type(graph: &GraphValue, from: &str, to: &str) -> Option<String> {
    let from_node = graph.nodes.iter().find(|n| n.id == from)?;
    let to_node = graph.nodes.iter().find(|n| n.id == to)?;

    match &from_node.output_type {
        MagType::Union(variants) => {
            for variant in variants {
                if to_node.input_type.accepts(variant) {
                    return Some(qualify_type(variant));
                }
            }
            None
        }
        _ => None,
    }
}

pub fn normalize(graph: GraphValue) -> GraphIr {
    let mut nodes: Vec<NodeIr> = graph.nodes.iter().map(node_to_ir).collect();
    nodes.sort_by(|a, b| a.id.cmp(&b.id));

    let mut edges: Vec<EdgeIr> = graph
        .edges
        .iter()
        .map(|e| {
            let edge_type = compute_edge_type(&graph, &e.from, &e.to);
            EdgeIr {
                from: e.from.clone(),
                to: e.to.clone(),
                edge_type,
            }
        })
        .collect();
    edges.sort_by(|a, b| (&a.from, &a.to).cmp(&(&b.from, &b.to)));

    let canonical = serde_json::json!({
        "terminal": &graph.terminal,
        "nodes": nodes,
        "edges": edges,
    });
    let hash = {
        let mut hasher = Sha256::new();
        hasher.update(canonical.to_string().as_bytes());
        format!("sha256:{:x}", hasher.finalize())
    };

    GraphIr {
        terminal: graph.terminal,
        nodes,
        edges,
        hash,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MagType;

    #[test]
    fn qualify_bare_type() {
        let ty = MagType::Named("ToolCalls".into());
        assert_eq!(qualify_type(&ty), "mag.ToolCalls");
    }

    #[test]
    fn qualify_dotted_type_passes_through() {
        let ty = MagType::Named("generic-tool.ToolCalls".into());
        assert_eq!(qualify_type(&ty), "generic-tool.ToolCalls");
    }

    #[test]
    fn qualify_union_with_mixed_types() {
        let ty = MagType::Union(vec![
            MagType::Named("generic-tool.ToolCalls".into()),
            MagType::Named("generic-provider.FinalAnswer".into()),
        ]);
        assert_eq!(
            qualify_type(&ty),
            "generic-tool.ToolCalls|generic-provider.FinalAnswer"
        );
    }

    #[test]
    fn qualify_var_always_prefixed() {
        let ty = MagType::Var("INPUT".into());
        assert_eq!(qualify_type(&ty), "mag.INPUT");
    }
}
