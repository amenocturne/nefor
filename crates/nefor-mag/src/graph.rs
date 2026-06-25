use crate::ast::{GraphValue, NodeValue, Value};
use crate::error::MagError;
use std::collections::{HashMap, HashSet, VecDeque};

pub fn extract_graph(value: Value) -> Result<GraphValue, MagError> {
    match value {
        Value::Graph(g) => Ok(g),
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
    validate_bounded_loops(graph)?;
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

// 1. At least one terminal declared
fn validate_has_terminals(graph: &GraphValue) -> Result<(), MagError> {
    if graph.terminals.is_empty() {
        return Err(MagError::NoTerminal);
    }
    Ok(())
}

// 2. Each terminal references an actual node
fn validate_terminals_exist(graph: &GraphValue) -> Result<(), MagError> {
    let nodes = node_map(graph);
    for terminal in &graph.terminals {
        if !nodes.contains_key(terminal.as_str()) {
            return Err(MagError::Graph(format!(
                "terminal '{terminal}' does not reference an existing node"
            )));
        }
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

// 4. Every node has a path to at least one terminal
fn validate_path_to_terminal(graph: &GraphValue) -> Result<(), MagError> {
    let adj = adjacency(graph);
    let terminal_set: HashSet<&str> = graph.terminals.iter().map(|s| s.as_str()).collect();

    // BFS backward from terminals through reverse edges
    let rev = reverse_adjacency(graph);
    let mut reaches_terminal: HashSet<&str> = HashSet::new();
    let mut queue: VecDeque<&str> = VecDeque::new();

    for &t in &terminal_set {
        reaches_terminal.insert(t);
        queue.push_back(t);
    }

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

// 6. Every cycle must pass through a loop-counter node
fn validate_bounded_loops(graph: &GraphValue) -> Result<(), MagError> {
    let adj = adjacency(graph);
    let node_ids: Vec<&str> = graph.nodes.iter().map(|n| n.id.as_str()).collect();
    let nodes = node_map(graph);

    // Find all cycles using DFS and check each contains a loop-counter
    let mut visited: HashSet<&str> = HashSet::new();
    let mut on_stack: HashSet<&str> = HashSet::new();
    let mut stack_path: Vec<&str> = Vec::new();

    for &start in &node_ids {
        if !visited.contains(start) {
            find_cycles_dfs(
                start,
                &adj,
                &nodes,
                &mut visited,
                &mut on_stack,
                &mut stack_path,
            )?;
        }
    }
    Ok(())
}

fn find_cycles_dfs<'a>(
    node: &'a str,
    adj: &HashMap<&'a str, Vec<&'a str>>,
    nodes: &HashMap<&'a str, &'a NodeValue>,
    visited: &mut HashSet<&'a str>,
    on_stack: &mut HashSet<&'a str>,
    stack_path: &mut Vec<&'a str>,
) -> Result<(), MagError> {
    visited.insert(node);
    on_stack.insert(node);
    stack_path.push(node);

    if let Some(successors) = adj.get(node) {
        for &succ in successors {
            if !visited.contains(succ) {
                find_cycles_dfs(succ, adj, nodes, visited, on_stack, stack_path)?;
            } else if on_stack.contains(succ) {
                // Found a cycle: extract it from stack_path
                let cycle_start = stack_path.iter().position(|&n| n == succ).unwrap_or(0);
                let cycle: Vec<&str> = stack_path[cycle_start..].to_vec();

                let has_loop_counter = cycle.iter().any(|&n| {
                    nodes
                        .get(n)
                        .map_or(false, |node| node.node_type == "loop-counter")
                });

                if !has_loop_counter {
                    return Err(MagError::UnboundedLoop {
                        nodes: cycle.join(", "),
                    });
                }
            }
        }
    }

    stack_path.pop();
    on_stack.remove(node);
    Ok(())
}

// 7. Edge type compatibility
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
            terminals: vec![],
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
            terminals: vec!["b".into()],
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
            terminals: vec![],
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
            terminals: vec!["missing".into()],
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
            terminals: vec!["b".into()],
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
            terminals: vec!["b".into()],
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
            terminals: vec!["b".into()],
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
            ],
            edges: vec![make_edge("a", "b"), make_edge("a", "c")],
            terminals: vec!["b".into(), "c".into()],
        };
        assert!(validate(&graph).is_ok());
    }

    #[test]
    fn unbounded_loop_errors() {
        // a -> b -> a (cycle, no loop-counter)
        let graph = GraphValue {
            nodes: vec![
                make_node("a", "llm", MagType::named("A"), MagType::named("B")),
                make_node("b", "check", MagType::named("B"), MagType::named("A")),
            ],
            edges: vec![make_edge("a", "b"), make_edge("b", "a")],
            terminals: vec!["b".into()],
        };
        let err = validate(&graph).unwrap_err();
        assert!(
            matches!(err, MagError::UnboundedLoop { .. }),
            "got: {err:?}"
        );
    }

    #[test]
    fn bounded_loop_with_counter() {
        // a -> b -> counter -> a, terminal counter
        let graph = GraphValue {
            nodes: vec![
                make_node("a", "llm", MagType::named("A"), MagType::named("B")),
                make_node("b", "check", MagType::named("B"), MagType::named("C")),
                make_node(
                    "counter",
                    "loop-counter",
                    MagType::named("C"),
                    MagType::named("A"),
                ),
            ],
            edges: vec![
                make_edge("a", "b"),
                make_edge("b", "counter"),
                make_edge("counter", "a"),
            ],
            terminals: vec!["counter".into()],
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
            terminals: vec!["b".into()],
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
            terminals: vec!["b".into()],
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
            ],
            edges: vec![make_edge("a", "b"), make_edge("a", "c")],
            terminals: vec!["b".into(), "c".into()],
        };
        assert!(validate(&graph).is_ok());
    }
}
