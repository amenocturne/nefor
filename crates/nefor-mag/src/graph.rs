use crate::ast::{EdgeValue, GraphValue, NodeValue, Port, Value};
use crate::error::MagError;
use crate::types::MagType;
use std::collections::{HashMap, HashSet, VecDeque};

/// Canonical id of the implicitly-appended program sink (mirrors ir.rs, where
/// the terminal node is always emitted under this id).
const SINK_ID: &str = "sink";

/// Resolve a graph's terminal when no `:terminal` was authored. Precedence:
///
/// 1. exactly one `sink`-factory node → that node (the pre-existing
///    auto-detect; several sink nodes stay an error);
/// 2. otherwise the graph implicitly terminates at its LAST node: the
///    canonical sink is appended after the last fragment's output ports,
///    its input contract derived from their types, and becomes the terminal.
///
/// `last_outputs` are the output ports of the last fragment in appearance
/// order (empty for a graph with no nodes — an error either way).
pub(crate) fn resolve_terminal(
    nodes: &mut Vec<NodeValue>,
    edges: &mut Vec<EdgeValue>,
    last_outputs: &[Port],
) -> Result<String, MagError> {
    let sinks: Vec<&str> = nodes
        .iter()
        .filter(|n| n.node_type == "sink")
        .map(|n| n.id.as_str())
        .collect();
    match sinks.len() {
        0 => append_implicit_sink(nodes, edges, last_outputs),
        1 => Ok(sinks[0].to_string()),
        _ => Err(MagError::Eval(format!(
            "graph has {} sink nodes ({}); exactly one is required",
            sinks.len(),
            sinks.join(", ")
        ))),
    }
}

/// Append the canonical implicit sink after the given output ports: a
/// `sink`-factory node whose input contract is the union of the port types,
/// wired from every port actor. Returns the terminal id.
pub(crate) fn append_implicit_sink(
    nodes: &mut Vec<NodeValue>,
    edges: &mut Vec<EdgeValue>,
    outputs: &[Port],
) -> Result<String, MagError> {
    if outputs.is_empty() {
        return Err(MagError::Eval(
            "graph has no nodes to derive a terminal from — a program needs at least one node"
                .into(),
        ));
    }
    if nodes.iter().any(|n| n.id == SINK_ID) {
        return Err(MagError::Graph(format!(
            "actor id '{SINK_ID}' is taken by a non-sink node; \
             name the terminal explicitly with :terminal"
        )));
    }
    let ty = if outputs.len() == 1 {
        outputs[0].ty.clone()
    } else {
        MagType::union(outputs.iter().map(|p| p.ty.clone()).collect())
    };
    nodes.push(NodeValue {
        id: SINK_ID.into(),
        node_type: "sink".into(),
        args: std::collections::BTreeMap::new(),
        input_type: ty.clone(),
        output_type: ty,
    });
    for port in outputs {
        if !edges
            .iter()
            .any(|e| e.from == port.actor && e.to == SINK_ID)
        {
            edges.push(EdgeValue {
                from: port.actor.clone(),
                to: SINK_ID.into(),
            });
        }
    }
    Ok(SINK_ID.into())
}

/// Extract the program graph from a program's final value.
///
/// - a `graph` value passes through (its terminal was resolved at eval);
/// - a bare node is a one-node program — the node is entry and terminal
///   (a non-sink node gets the canonical sink appended to carry the run
///   result);
/// - a subgraph (an inline `(a -> b)` chain, or a template instance) is a
///   program ending at its output ports — terminal resolved the same way.
pub fn extract_graph(value: Value) -> Result<GraphValue, MagError> {
    match value {
        Value::Graph(g) => Ok(g),
        Value::Node(n) => {
            let outputs = vec![Port {
                actor: n.id.clone(),
                ty: n.output_type.clone(),
            }];
            let mut nodes = vec![n];
            let mut edges = Vec::new();
            let terminal = resolve_terminal(&mut nodes, &mut edges, &outputs)?;
            Ok(GraphValue {
                nodes,
                edges,
                terminal,
            })
        }
        Value::Subgraph(s) => {
            let mut nodes = s.nodes;
            let mut edges = s.edges;
            let terminal = resolve_terminal(&mut nodes, &mut edges, &s.outputs)?;
            Ok(GraphValue {
                nodes,
                edges,
                terminal,
            })
        }
        other => Err(MagError::Graph(format!(
            "expected graph value, got {}",
            other.type_name()
        ))),
    }
}

pub fn validate(graph: &GraphValue) -> Result<(), MagError> {
    validate_has_terminals(graph)?;
    validate_terminals_exist(graph)?;
    validate_connected(graph)?;
    validate_path_to_terminal(graph)?;
    validate_dead_branches(graph)?;
    validate_edge_types(graph)?;
    Ok(())
}

fn node_map(graph: &GraphValue) -> HashMap<&str, &NodeValue> {
    graph.nodes.iter().map(|n| (n.id.as_str(), n)).collect()
}

/// Build adjacency list: node_id -> list of successor node_ids
fn adjacency(graph: &GraphValue) -> HashMap<&str, Vec<&str>> {
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    for node in &graph.nodes {
        adj.entry(node.id.as_str()).or_default();
    }
    for edge in &graph.edges {
        adj.entry(edge.from.as_str())
            .or_default()
            .push(edge.to.as_str());
    }
    adj
}

/// Build reverse adjacency: node_id -> list of predecessor node_ids
fn reverse_adjacency(graph: &GraphValue) -> HashMap<&str, Vec<&str>> {
    let mut rev: HashMap<&str, Vec<&str>> = HashMap::new();
    for node in &graph.nodes {
        rev.entry(node.id.as_str()).or_default();
    }
    for edge in &graph.edges {
        rev.entry(edge.to.as_str())
            .or_default()
            .push(edge.from.as_str());
    }
    rev
}

// 1. Terminal declared
fn validate_has_terminals(graph: &GraphValue) -> Result<(), MagError> {
    if graph.terminal.is_empty() {
        return Err(MagError::NoTerminal);
    }
    Ok(())
}

// 2. Terminal references an actual node
fn validate_terminals_exist(graph: &GraphValue) -> Result<(), MagError> {
    let nodes = node_map(graph);
    if !nodes.contains_key(graph.terminal.as_str()) {
        return Err(MagError::Graph(format!(
            "terminal '{}' does not reference an existing node",
            graph.terminal
        )));
    }
    Ok(())
}

// 3. Weakly connected — every node reachable when treating edges as undirected
fn validate_connected(graph: &GraphValue) -> Result<(), MagError> {
    if graph.nodes.is_empty() {
        return Ok(());
    }

    // Build undirected adjacency
    let mut undirected: HashMap<&str, HashSet<&str>> = HashMap::new();
    for node in &graph.nodes {
        undirected.entry(node.id.as_str()).or_default();
    }
    for edge in &graph.edges {
        undirected
            .entry(edge.from.as_str())
            .or_default()
            .insert(edge.to.as_str());
        undirected
            .entry(edge.to.as_str())
            .or_default()
            .insert(edge.from.as_str());
    }

    let start = graph.nodes[0].id.as_str();
    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();
    queue.push_back(start);
    visited.insert(start);

    while let Some(node) = queue.pop_front() {
        if let Some(neighbors) = undirected.get(node) {
            for &neighbor in neighbors {
                if visited.insert(neighbor) {
                    queue.push_back(neighbor);
                }
            }
        }
    }

    for node in &graph.nodes {
        if !visited.contains(node.id.as_str()) {
            return Err(MagError::Disconnected {
                node: node.id.clone(),
            });
        }
    }
    Ok(())
}

// 4. Every node has a path to the terminal
fn validate_path_to_terminal(graph: &GraphValue) -> Result<(), MagError> {
    let adj = adjacency(graph);
    let terminal = graph.terminal.as_str();

    // BFS backward from terminal through reverse edges
    let rev = reverse_adjacency(graph);
    let mut reaches_terminal: HashSet<&str> = HashSet::new();
    let mut queue: VecDeque<&str> = VecDeque::new();

    reaches_terminal.insert(terminal);
    queue.push_back(terminal);

    while let Some(node) = queue.pop_front() {
        if let Some(preds) = rev.get(node) {
            for &pred in preds {
                if reaches_terminal.insert(pred) {
                    queue.push_back(pred);
                }
            }
        }
    }

    // Also include nodes that can reach a terminal via forward edges
    // (already handled by reverse traversal from terminals)
    let _ = adj; // used for building graph context only

    for node in &graph.nodes {
        if !reaches_terminal.contains(node.id.as_str()) {
            return Err(MagError::Disconnected {
                node: node.id.clone(),
            });
        }
    }
    Ok(())
}

// 5. Dead branches: for union output types, every variant must have a destination edge
fn validate_dead_branches(graph: &GraphValue) -> Result<(), MagError> {
    let nodes = node_map(graph);
    let adj = adjacency(graph);

    for node in &graph.nodes {
        let variants = node.output_type.variants();
        if variants.len() <= 1 {
            continue;
        }

        let successors = adj
            .get(node.id.as_str())
            .map(|v| v.as_slice())
            .unwrap_or(&[]);

        for variant in &variants {
            let has_edge = successors.iter().any(|&succ_id| {
                if let Some(succ_node) = nodes.get(succ_id) {
                    succ_node.input_type.accepts(variant)
                } else {
                    false
                }
            });

            if !has_edge {
                return Err(MagError::DeadBranch {
                    node: node.id.clone(),
                    variant: (*variant).clone(),
                    source_type: node.output_type.clone(),
                });
            }
        }
    }
    Ok(())
}

// 6. Edge type compatibility
fn validate_edge_types(graph: &GraphValue) -> Result<(), MagError> {
    let nodes = node_map(graph);

    for edge in &graph.edges {
        let from_node = nodes.get(edge.from.as_str()).ok_or_else(|| {
            MagError::Graph(format!(
                "edge references unknown source node '{}'",
                edge.from
            ))
        })?;
        let to_node = nodes.get(edge.to.as_str()).ok_or_else(|| {
            MagError::Graph(format!("edge references unknown target node '{}'", edge.to))
        })?;

        // Check if any variant of the source output is accepted by the target input
        let output_variants = from_node.output_type.variants();
        let any_compatible = output_variants
            .iter()
            .any(|v| to_node.input_type.accepts(v));

        if !any_compatible {
            return Err(MagError::EdgeTypeMismatch {
                from: edge.from.clone(),
                to: edge.to.clone(),
                output: from_node.output_type.clone(),
                input: to_node.input_type.clone(),
            });
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

    fn make_node(id: &str, node_type: &str, input: MagType, output: MagType) -> NodeValue {
        NodeValue {
            id: id.into(),
            node_type: node_type.into(),
            args: BTreeMap::new(),
            input_type: input,
            output_type: output,
        }
    }

    fn make_edge(from: &str, to: &str) -> EdgeValue {
        EdgeValue {
            from: from.into(),
            to: to.into(),
        }
    }

    #[test]
    fn extract_graph_from_value() {
        let graph = GraphValue {
            nodes: vec![],
            edges: vec![],
            terminal: String::new(),
        };
        let val = Value::Graph(graph);
        assert!(extract_graph(val).is_ok());
    }

    #[test]
    fn extract_graph_rejects_non_graph() {
        assert!(extract_graph(Value::Int(42)).is_err());
    }

    #[test]
    fn valid_simple_graph() {
        let graph = GraphValue {
            nodes: vec![
                make_node("a", "llm", MagType::named("A"), MagType::named("B")),
                make_node("b", "check", MagType::named("B"), MagType::named("C")),
            ],
            edges: vec![make_edge("a", "b")],
            terminal: "b".into(),
        };
        assert!(validate(&graph).is_ok());
    }

    #[test]
    fn missing_terminal_errors() {
        let graph = GraphValue {
            nodes: vec![make_node(
                "a",
                "llm",
                MagType::named("A"),
                MagType::named("B"),
            )],
            edges: vec![],
            terminal: String::new(),
        };
        assert!(matches!(validate(&graph), Err(MagError::NoTerminal)));
    }

    #[test]
    fn terminal_references_nonexistent_node() {
        let graph = GraphValue {
            nodes: vec![make_node(
                "a",
                "llm",
                MagType::named("A"),
                MagType::named("B"),
            )],
            edges: vec![],
            terminal: "missing".into(),
        };
        assert!(validate(&graph).is_err());
    }

    #[test]
    fn disconnected_node_errors() {
        let graph = GraphValue {
            nodes: vec![
                make_node("a", "llm", MagType::named("A"), MagType::named("B")),
                make_node("b", "check", MagType::named("B"), MagType::named("C")),
                make_node("c", "isolated", MagType::named("X"), MagType::named("Y")),
            ],
            edges: vec![make_edge("a", "b")],
            terminal: "b".into(),
        };
        let err = validate(&graph).unwrap_err();
        // "c" is disconnected (not connected to the main component)
        assert!(
            matches!(err, MagError::Disconnected { ref node } if node == "c"),
            "expected Disconnected error for 'c', got: {err:?}"
        );
    }

    #[test]
    fn no_path_to_terminal_errors() {
        // a -> b, c -> b, terminal b. 'a' and 'c' reach terminal. But if we have:
        // a -> b -> c, terminal c, plus isolated d -> nothing, terminal c
        // d has no path to c
        let graph = GraphValue {
            nodes: vec![
                make_node("a", "llm", MagType::named("A"), MagType::named("B")),
                make_node("b", "check", MagType::named("B"), MagType::named("C")),
                make_node("c", "output", MagType::named("C"), MagType::named("D")),
            ],
            edges: vec![make_edge("a", "b")],
            terminal: "b".into(),
        };
        // 'c' is weakly disconnected from a-b
        let err = validate(&graph).unwrap_err();
        assert!(matches!(err, MagError::Disconnected { .. }), "got: {err:?}");
    }

    #[test]
    fn dead_branch_union_not_covered() {
        // Node 'a' outputs (X | Y), but only edge to node accepting X
        let graph = GraphValue {
            nodes: vec![
                make_node(
                    "a",
                    "router",
                    MagType::named("Input"),
                    MagType::Union(vec![MagType::named("X"), MagType::named("Y")]),
                ),
                make_node("b", "handler-x", MagType::named("X"), MagType::named("Out")),
            ],
            edges: vec![make_edge("a", "b")],
            terminal: "b".into(),
        };
        let err = validate(&graph).unwrap_err();
        assert!(
            matches!(err, MagError::DeadBranch { ref variant, .. }
                if *variant == MagType::named("Y")),
            "got: {err:?}"
        );
    }

    #[test]
    fn dead_branch_union_fully_covered() {
        let graph = GraphValue {
            nodes: vec![
                make_node(
                    "a",
                    "router",
                    MagType::named("Input"),
                    MagType::Union(vec![MagType::named("X"), MagType::named("Y")]),
                ),
                make_node("b", "handler-x", MagType::named("X"), MagType::named("Out")),
                make_node("c", "handler-y", MagType::named("Y"), MagType::named("Out")),
                make_node("sink", "sink", MagType::named("Out"), MagType::named("Out")),
            ],
            edges: vec![
                make_edge("a", "b"),
                make_edge("a", "c"),
                make_edge("b", "sink"),
                make_edge("c", "sink"),
            ],
            terminal: "sink".into(),
        };
        assert!(validate(&graph).is_ok());
    }

    #[test]
    fn bare_cycle_is_accepted() {
        // a -> b -> a. A cycle is a legal shape as-is — the exit is a typed
        // output port, and stopping a runaway run is the control plane's
        // kill/interrupt, not a compiled bound.
        let graph = GraphValue {
            nodes: vec![
                make_node("a", "llm", MagType::named("A"), MagType::named("B")),
                make_node("b", "check", MagType::named("B"), MagType::named("A")),
            ],
            edges: vec![make_edge("a", "b"), make_edge("b", "a")],
            terminal: "b".into(),
        };
        assert!(validate(&graph).is_ok());
    }

    #[test]
    fn longer_cycle_is_accepted() {
        // a -> b -> c -> a, terminal c.
        let graph = GraphValue {
            nodes: vec![
                make_node("a", "llm", MagType::named("A"), MagType::named("B")),
                make_node("b", "check", MagType::named("B"), MagType::named("C")),
                make_node("c", "route", MagType::named("C"), MagType::named("A")),
            ],
            edges: vec![
                make_edge("a", "b"),
                make_edge("b", "c"),
                make_edge("c", "a"),
            ],
            terminal: "c".into(),
        };
        assert!(validate(&graph).is_ok());
    }

    #[test]
    fn edge_type_mismatch_errors() {
        let graph = GraphValue {
            nodes: vec![
                make_node("a", "llm", MagType::named("A"), MagType::named("B")),
                make_node("b", "check", MagType::named("C"), MagType::named("D")),
            ],
            edges: vec![make_edge("a", "b")],
            terminal: "b".into(),
        };
        let err = validate(&graph).unwrap_err();
        assert!(
            matches!(err, MagError::EdgeTypeMismatch { .. }),
            "got: {err:?}"
        );
    }

    #[test]
    fn edge_type_compatible_with_var() {
        // Type variables are universally compatible
        let graph = GraphValue {
            nodes: vec![
                make_node("a", "llm", MagType::named("A"), MagType::var("OUTPUT")),
                make_node("b", "check", MagType::var("INPUT"), MagType::named("D")),
            ],
            edges: vec![make_edge("a", "b")],
            terminal: "b".into(),
        };
        assert!(validate(&graph).is_ok());
    }

    #[test]
    fn edge_type_union_partial_match() {
        // Source outputs (X | Y), target accepts X — at least one variant matches
        let graph = GraphValue {
            nodes: vec![
                make_node(
                    "a",
                    "router",
                    MagType::named("Input"),
                    MagType::Union(vec![MagType::named("X"), MagType::named("Y")]),
                ),
                make_node("b", "handler-x", MagType::named("X"), MagType::named("Out")),
                make_node("c", "handler-y", MagType::named("Y"), MagType::named("Out")),
                make_node("sink", "sink", MagType::named("Out"), MagType::named("Out")),
            ],
            edges: vec![
                make_edge("a", "b"),
                make_edge("a", "c"),
                make_edge("b", "sink"),
                make_edge("c", "sink"),
            ],
            terminal: "sink".into(),
        };
        assert!(validate(&graph).is_ok());
    }
}
