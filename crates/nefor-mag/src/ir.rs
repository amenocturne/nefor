use crate::ast::{GraphValue, NodeValue, Value};
use crate::env::Env;
use crate::error::MagError;
use crate::types::MagType;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};

/// Canonical id of the single program sink. Terminality is structural in the
/// modification model (a sink emits nothing downstream), so the authoring
/// `:terminal` node is always emitted under this id.
const SINK_ID: &str = "sink";

/// A graph modification — the shape the MAG runtime consumes (see ir.md).
/// Replaces the older graph-shaped `GraphIr{terminal, nodes, edges}`: edges
/// dissolve into each actor's `routes`, and terminality is structural.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModificationIr {
    pub actors: Vec<ActorIr>,
    pub messages: Vec<MessageIr>,
    pub kills: Vec<String>,
    pub rules: Vec<RuleIr>,
    /// Deterministic hash over the canonicalized modification. Out-of-band
    /// bookkeeping — not part of the modification the kernel folds.
    pub hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActorIr {
    pub id: String,
    pub factory: String,
    pub params: serde_json::Value,
    /// Typed output → destination ids. Kernel-owned wiring, sibling of
    /// `params`. Keys are fully-qualified output type tags; values are always
    /// arrays (fanout needs no special-casing). Insertion order is preserved
    /// for readable emission; hashing sorts.
    pub routes: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageIr {
    pub to: String,
    pub content: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleIr {
    pub on: String,
    #[serde(rename = "fn")]
    pub fn_name: String,
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

fn value_to_json(val: &Value) -> Result<serde_json::Value, MagError> {
    match val {
        Value::Str(s) => Ok(serde_json::Value::String(s.clone())),
        Value::Int(n) => Ok(serde_json::json!(n)),
        Value::Float(n) => Ok(serde_json::json!(n)),
        Value::Bool(b) => Ok(serde_json::Value::Bool(*b)),
        Value::Nil => Ok(serde_json::Value::Null),
        Value::Keyword(k) => Ok(serde_json::Value::String(format!(":{k}"))),
        Value::Symbol(s) => Ok(serde_json::Value::String(s.clone())),
        Value::List(items) | Value::Vector(items) => {
            let arr: Result<Vec<_>, _> = items.iter().map(value_to_json).collect();
            Ok(serde_json::Value::Array(arr?))
        }
        Value::Map(map) => {
            let obj: Result<serde_json::Map<String, serde_json::Value>, _> = map
                .iter()
                .map(|(k, v)| value_to_json(v).map(|jv| (k.clone(), jv)))
                .collect();
            Ok(serde_json::Value::Object(obj?))
        }
        Value::Node(_)
        | Value::Graph(_)
        | Value::Subgraph(_)
        | Value::Fn(_)
        | Value::BuiltinFn(_)
        | Value::TypeDecl(_) => Err(MagError::Eval(format!(
            "cannot serialize {} to JSON in node args",
            val.type_name()
        ))),
    }
}

fn node_params(node: &NodeValue) -> Result<serde_json::Value, MagError> {
    let obj: Result<serde_json::Map<String, serde_json::Value>, _> = node
        .args
        .iter()
        .map(|(k, v)| value_to_json(v).map(|jv| (k.clone(), jv)))
        .collect();
    Ok(serde_json::Value::Object(obj?))
}

/// The fully-qualified type an edge carries: the first output variant of the
/// source that the destination's input contract accepts. Post-validation there
/// is always one (validate_edge_types guarantees compatibility).
fn edge_route_type(from: &NodeValue, to: &NodeValue) -> Option<MagType> {
    from.output_type
        .variants()
        .into_iter()
        .find(|v| to.input_type.accepts(v))
        .cloned()
}

fn initial_activation_content() -> serde_json::Value {
    serde_json::json!({ "kind": "task", "prompt": "<initial task text>" })
}

/// Lower a validated authoring graph into a graph modification. Edges invert
/// into per-source `routes`; the terminal node becomes the `sink` actor with
/// empty routes; a source (no-inbound) actor gets an initial activation.
pub fn lower(graph: GraphValue) -> Result<ModificationIr, MagError> {
    // The terminal node is emitted under the canonical `sink` id; rewrite any
    // route destination that points at it.
    let sink_source =
        (graph.terminal != SINK_ID && !graph.terminal.is_empty()).then(|| graph.terminal.clone());
    let rename = |id: &str| -> String {
        match &sink_source {
            Some(old) if id == old => SINK_ID.to_string(),
            _ => id.to_string(),
        }
    };

    let node_index: HashMap<&str, &NodeValue> =
        graph.nodes.iter().map(|n| (n.id.as_str(), n)).collect();

    // Actors in authoring (node) order.
    let mut actors: Vec<ActorIr> = Vec::with_capacity(graph.nodes.len());
    let mut actor_pos: HashMap<&str, usize> = HashMap::new();
    for node in &graph.nodes {
        actor_pos.insert(node.id.as_str(), actors.len());
        actors.push(ActorIr {
            id: rename(&node.id),
            factory: node.node_type.clone(),
            params: node_params(node)?,
            routes: serde_json::Map::new(),
        });
    }

    // Invert edges into routes on the emitting actor.
    for edge in &graph.edges {
        let from = node_index.get(edge.from.as_str()).ok_or_else(|| {
            MagError::Graph(format!(
                "edge references unknown source node '{}'",
                edge.from
            ))
        })?;
        let to = node_index.get(edge.to.as_str()).ok_or_else(|| {
            MagError::Graph(format!("edge references unknown target node '{}'", edge.to))
        })?;
        let route_ty = edge_route_type(from, to).ok_or_else(|| {
            MagError::Graph(format!(
                "edge {} -> {} carries no type compatible with the destination",
                edge.from, edge.to
            ))
        })?;
        let key = qualify_type(&route_ty);
        let dest = serde_json::Value::String(rename(&edge.to));
        let pos = actor_pos[edge.from.as_str()];
        let slot = actors[pos]
            .routes
            .entry(key)
            .or_insert_with(|| serde_json::Value::Array(Vec::new()));
        if let serde_json::Value::Array(arr) = slot {
            if !arr.contains(&dest) {
                arr.push(dest);
            }
        }
    }

    // Initial activation: seed every source (no-inbound) actor.
    let inbound: HashSet<&str> = graph.edges.iter().map(|e| e.to.as_str()).collect();
    let messages: Vec<MessageIr> = graph
        .nodes
        .iter()
        .filter(|n| !inbound.contains(n.id.as_str()))
        .map(|n| MessageIr {
            to: rename(&n.id),
            content: initial_activation_content(),
        })
        .collect();

    // Static programs remove nothing and bind no rules.
    let kills: Vec<String> = Vec::new();
    let rules: Vec<RuleIr> = Vec::new();

    let hash = hash_modification(&actors, &messages, &kills, &rules);

    Ok(ModificationIr {
        actors,
        messages,
        kills,
        rules,
        hash,
    })
}

/// Deterministic hash over the canonicalized modification: actors sorted by id,
/// route keys and destination arrays sorted, messages/kills/rules sorted. This
/// makes the hash independent of authoring order while emission stays readable.
fn hash_modification(
    actors: &[ActorIr],
    messages: &[MessageIr],
    kills: &[String],
    rules: &[RuleIr],
) -> String {
    let mut sorted_actors: Vec<&ActorIr> = actors.iter().collect();
    sorted_actors.sort_by(|a, b| a.id.cmp(&b.id));
    let actor_json: Vec<serde_json::Value> = sorted_actors
        .iter()
        .map(|a| {
            let mut routes: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for (k, v) in &a.routes {
                let mut dests: Vec<String> = v
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|x| x.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                dests.sort();
                routes.insert(k.clone(), dests);
            }
            serde_json::json!({
                "id": a.id,
                "factory": a.factory,
                "params": a.params,
                "routes": routes,
            })
        })
        .collect();

    let mut msgs: Vec<serde_json::Value> = messages
        .iter()
        .map(|m| serde_json::json!({ "to": m.to, "content": m.content }))
        .collect();
    msgs.sort_by_key(|a| a.to_string());

    let mut kills_sorted = kills.to_vec();
    kills_sorted.sort();

    let mut rules_sorted: Vec<serde_json::Value> = rules
        .iter()
        .map(|r| serde_json::json!({ "on": r.on, "fn": r.fn_name }))
        .collect();
    rules_sorted.sort_by_key(|a| a.to_string());

    let canonical = serde_json::json!({
        "actors": actor_json,
        "messages": msgs,
        "kills": kills_sorted,
        "rules": rules_sorted,
    });

    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string().as_bytes());
    format!("sha256:{:x}", hasher.finalize())
}

/// Load-time validation of a modification: id uniqueness, message targets, and
/// rule references (the rule `fn` exists, is a function, and is unary).
pub fn validate_modification(ir: &ModificationIr, env: &Env) -> Result<(), MagError> {
    let mut seen: HashSet<&str> = HashSet::new();
    for actor in &ir.actors {
        if !seen.insert(actor.id.as_str()) {
            return Err(MagError::Graph(format!(
                "duplicate actor id '{}' — instance ids must be unique across the program",
                actor.id
            )));
        }
    }

    for msg in &ir.messages {
        if !ir.actors.iter().any(|a| a.id == msg.to) {
            return Err(MagError::Graph(format!(
                "message targets unknown actor '{}'",
                msg.to
            )));
        }
    }

    for rule in &ir.rules {
        match env.lookup(&rule.fn_name) {
            Ok(Value::Fn(fv)) if fv.params.len() == 1 => {}
            Ok(Value::Fn(fv)) => {
                return Err(MagError::Eval(format!(
                    "rule fn '{}' must be unary, takes {} arguments",
                    rule.fn_name,
                    fv.params.len()
                )))
            }
            Ok(other) => {
                return Err(MagError::Eval(format!(
                    "rule fn '{}' is not a function (got {})",
                    rule.fn_name,
                    other.type_name()
                )))
            }
            Err(_) => {
                return Err(MagError::Eval(format!(
                    "rule references undefined fn '{}'",
                    rule.fn_name
                )))
            }
        }
        if !ir.actors.iter().any(|a| a.id == rule.on) {
            return Err(MagError::Graph(format!(
                "rule 'on' references unknown actor '{}'",
                rule.on
            )));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{EdgeValue, NodeValue};
    use crate::types::MagType;
    use std::collections::BTreeMap;

    fn node(id: &str, ty: &str, input: MagType, output: MagType) -> NodeValue {
        NodeValue {
            id: id.into(),
            node_type: ty.into(),
            args: BTreeMap::new(),
            input_type: input,
            output_type: output,
        }
    }

    fn dests<'a>(actor: &'a ActorIr, key: &str) -> Vec<&'a str> {
        actor.routes[key]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect()
    }

    #[test]
    fn qualify_bare_type() {
        assert_eq!(
            qualify_type(&MagType::Named("ToolCalls".into())),
            "mag.ToolCalls"
        );
    }

    #[test]
    fn qualify_dotted_type_passes_through() {
        assert_eq!(
            qualify_type(&MagType::Named("generic-tool.ToolCalls".into())),
            "generic-tool.ToolCalls"
        );
    }

    #[test]
    fn qualify_var_always_prefixed() {
        assert_eq!(qualify_type(&MagType::Var("INPUT".into())), "mag.INPUT");
    }

    #[test]
    fn value_to_json_rejects_function() {
        let val = Value::Fn(crate::ast::FnValue {
            params: vec![],
            body: vec![],
            closure: vec![],
        });
        assert!(value_to_json(&val).is_err());
    }

    #[test]
    fn edges_invert_into_routes() {
        // a -> sink, terminal sink.
        let graph = GraphValue {
            nodes: vec![
                node("a", "llm", MagType::named("A"), MagType::named("B")),
                node("sink", "sink", MagType::named("B"), MagType::named("B")),
            ],
            edges: vec![EdgeValue {
                from: "a".into(),
                to: "sink".into(),
            }],
            terminal: "sink".into(),
        };
        let ir = lower(graph).unwrap();
        assert_eq!(ir.actors.len(), 2);
        assert_eq!(ir.actors[0].id, "a");
        assert_eq!(ir.actors[0].factory, "llm");
        assert_eq!(dests(&ir.actors[0], "mag.B"), vec!["sink"]);
        // Sink emits nothing downstream.
        assert!(ir.actors[1].routes.is_empty());
    }

    #[test]
    fn union_output_becomes_multiple_route_keys() {
        let graph = GraphValue {
            nodes: vec![
                node(
                    "router",
                    "router",
                    MagType::named("IN"),
                    MagType::Union(vec![MagType::named("X"), MagType::named("Y")]),
                ),
                node("hx", "hx", MagType::named("X"), MagType::named("Out")),
                node("hy", "hy", MagType::named("Y"), MagType::named("Out")),
                node("sink", "sink", MagType::named("Out"), MagType::named("Out")),
            ],
            edges: vec![
                EdgeValue {
                    from: "router".into(),
                    to: "hx".into(),
                },
                EdgeValue {
                    from: "router".into(),
                    to: "hy".into(),
                },
                EdgeValue {
                    from: "hx".into(),
                    to: "sink".into(),
                },
                EdgeValue {
                    from: "hy".into(),
                    to: "sink".into(),
                },
            ],
            terminal: "sink".into(),
        };
        let ir = lower(graph).unwrap();
        let router = &ir.actors[0];
        assert_eq!(dests(router, "mag.X"), vec!["hx"]);
        assert_eq!(dests(router, "mag.Y"), vec!["hy"]);
    }

    #[test]
    fn fanout_same_type_many_targets() {
        let graph = GraphValue {
            nodes: vec![
                node("a", "a", MagType::named("A"), MagType::named("T")),
                node("b", "b", MagType::named("T"), MagType::named("Out")),
                node("c", "c", MagType::named("T"), MagType::named("Out")),
                node("sink", "sink", MagType::named("Out"), MagType::named("Out")),
            ],
            edges: vec![
                EdgeValue {
                    from: "a".into(),
                    to: "b".into(),
                },
                EdgeValue {
                    from: "a".into(),
                    to: "c".into(),
                },
                EdgeValue {
                    from: "b".into(),
                    to: "sink".into(),
                },
                EdgeValue {
                    from: "c".into(),
                    to: "sink".into(),
                },
            ],
            terminal: "sink".into(),
        };
        let ir = lower(graph).unwrap();
        assert_eq!(dests(&ir.actors[0], "mag.T"), vec!["b", "c"]);
    }

    #[test]
    fn terminal_renamed_to_sink_and_routes_follow() {
        // Terminal node bound to "out" is emitted as "sink".
        let graph = GraphValue {
            nodes: vec![
                node("a", "llm", MagType::named("A"), MagType::named("B")),
                node("out", "sink", MagType::named("B"), MagType::named("B")),
            ],
            edges: vec![EdgeValue {
                from: "a".into(),
                to: "out".into(),
            }],
            terminal: "out".into(),
        };
        let ir = lower(graph).unwrap();
        assert_eq!(ir.actors[1].id, "sink");
        assert_eq!(dests(&ir.actors[0], "mag.B"), vec!["sink"]);
    }

    #[test]
    fn source_gets_initial_activation() {
        let graph = GraphValue {
            nodes: vec![
                node("entry", "adapter", MagType::named("A"), MagType::named("B")),
                node("sink", "sink", MagType::named("B"), MagType::named("B")),
            ],
            edges: vec![EdgeValue {
                from: "entry".into(),
                to: "sink".into(),
            }],
            terminal: "sink".into(),
        };
        let ir = lower(graph).unwrap();
        assert_eq!(ir.messages.len(), 1);
        assert_eq!(ir.messages[0].to, "entry");
        assert_eq!(ir.messages[0].content["kind"], "task");
    }

    #[test]
    fn hash_is_order_independent() {
        let g1 = GraphValue {
            nodes: vec![
                node("a", "a", MagType::named("A"), MagType::named("B")),
                node("sink", "sink", MagType::named("B"), MagType::named("B")),
            ],
            edges: vec![EdgeValue {
                from: "a".into(),
                to: "sink".into(),
            }],
            terminal: "sink".into(),
        };
        let g2 = GraphValue {
            nodes: vec![
                node("sink", "sink", MagType::named("B"), MagType::named("B")),
                node("a", "a", MagType::named("A"), MagType::named("B")),
            ],
            edges: vec![EdgeValue {
                from: "a".into(),
                to: "sink".into(),
            }],
            terminal: "sink".into(),
        };
        assert_eq!(lower(g1).unwrap().hash, lower(g2).unwrap().hash);
    }

    #[test]
    fn duplicate_actor_id_rejected() {
        let ir = ModificationIr {
            actors: vec![
                ActorIr {
                    id: "dup".into(),
                    factory: "llm".into(),
                    params: serde_json::json!({}),
                    routes: serde_json::Map::new(),
                },
                ActorIr {
                    id: "dup".into(),
                    factory: "sink".into(),
                    params: serde_json::json!({}),
                    routes: serde_json::Map::new(),
                },
            ],
            messages: vec![],
            kills: vec![],
            rules: vec![],
            hash: String::new(),
        };
        let env = Env::new_with_stdlib();
        let err = validate_modification(&ir, &env).unwrap_err();
        assert!(err.to_string().contains("dup"), "got: {err}");
    }

    #[test]
    fn rule_ref_to_missing_fn_rejected() {
        let ir = ModificationIr {
            actors: vec![ActorIr {
                id: "a".into(),
                factory: "llm".into(),
                params: serde_json::json!({}),
                routes: serde_json::Map::new(),
            }],
            messages: vec![],
            kills: vec![],
            rules: vec![RuleIr {
                on: "a".into(),
                fn_name: "does-not-exist".into(),
            }],
            hash: String::new(),
        };
        let env = Env::new_with_stdlib();
        let err = validate_modification(&ir, &env).unwrap_err();
        assert!(err.to_string().contains("undefined fn"), "got: {err}");
    }
}
