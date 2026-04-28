//! Graph parsing and validation.
//!
//! Wire shape (per parent spec §3, with T6 fanout-by-signature shape):
//! ```jsonc
//! {
//!   "nodes": [{
//!     "id": "n1",
//!     "reasoner": "openai-provider",
//!     "args": {...},
//!     "fanout": {                                // optional
//!       "in": "generic-provider.ProviderOut",
//!       "out": ["generic-tool.ToolCalls", "generic-provider.FinalAnswer"]
//!     }
//!   }],
//!   "edges": [{
//!     "from": "n1",
//!     "to": "n2",
//!     "type": "generic-tool.ToolCalls"          // optional fanout-routing tag
//!   }]
//! }
//! ```
//!
//! Edges are `(from, to, type?)` — no slot index. Routing of a fanout
//! combinator's outputs to outgoing edges is by **type tag**, not by
//! position. The scheduler reads `edge.type` to match a fanout output's
//! type to the right downstream node.
//!
//! **Names are sugar; the wire carries signatures.** Parent spec writes
//! `fanout: tool_split` as DSL spelling. The wire shape above is the
//! signature the spec's name resolves to. Combinators are looked up by
//! `(in, out_multiset)` — the human-readable name is a Lua-DSL convenience
//! that doesn't reach the scheduler.
//!
//! Cycles are allowed (the plugin used to reject them; per parent spec §3
//! "DAG is not acyclic" we now accept any topology). [`Graph::detect_cycle`]
//! stays for hypothetical other consumers — submit no longer calls it.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

/// Per-run typed identifier for a node within a single graph.
pub type NodeId = String;

/// A single node in the submitted graph.
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    /// Caller-supplied id; unique within this graph.
    pub id: NodeId,
    /// Plugin name to dispatch to (`<reasoner>.run_node`).
    pub reasoner: String,
    /// Verbatim args echoed to the reasoner.
    pub args: Value,
    /// Optional fanout-combinator signature. When unset, the scheduler
    /// uses the v1 broadcast: the node's full output is routed to every
    /// outgoing edge. When set, the runtime-fanout path issues
    /// `combinators.invoke` against this signature and routes per-type
    /// from the multiset reply (matching against `edge.type`).
    pub fanout: Option<FanoutSignature>,
}

/// A fanout combinator signature: input type + multiset of output types.
///
/// Carries fully-qualified type tags (`<plugin>.<name>`). Multiset is
/// preserved in submission order on the wire; the scheduler normalises
/// (sorts) before issuing `combinators.query` / `combinators.invoke` so
/// `[A, B]` and `[B, A]` resolve to the same registered combinator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FanoutSignature {
    /// Input type tag, fully qualified.
    pub in_type: String,
    /// Output type tags, fully qualified, multiset (order insignificant).
    pub out_multiset: Vec<String>,
}

/// A directed edge between two nodes, optionally carrying a routing type
/// tag for fanout output dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    /// Source node id.
    pub from: NodeId,
    /// Target node id.
    pub to: NodeId,
    /// Optional routing tag — when the source node has a `fanout`
    /// signature, the scheduler matches each output's `type` against
    /// `edge.type` to decide which edge fires. Absent → falls through to
    /// broadcast (legacy v1 path).
    pub type_tag: Option<String>,
}

/// Closed enum for the on-failure policy. v1 spec offers two values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnNodeFailure {
    /// On first node error: mark not-yet-dispatched nodes skipped, drain
    /// in-flight, emit `graph.run_complete { status: "failure" }`.
    Abort,
    /// Errors propagate as upstream `inputs.<id>.error`; final status is
    /// `partial_failure` if any node errored, `success` otherwise.
    Continue,
}

impl OnNodeFailure {
    /// Parse the wire-string. Default per spec is `"abort"`.
    pub fn parse(raw: Option<&str>) -> Result<Self, String> {
        match raw {
            None | Some("abort") => Ok(Self::Abort),
            Some("continue") => Ok(Self::Continue),
            Some(other) => Err(format!(
                "on_node_failure must be \"abort\" or \"continue\"; got {other:?}"
            )),
        }
    }
}

/// Default ack deadline if the submission omits `ack_deadline_ms`. Five
/// seconds is generous for a local plugin that just needs to confirm
/// receipt — actual work has no timeout per parent spec.
pub const DEFAULT_ACK_DEADLINE_MS: u64 = 5_000;

/// Parsed and topology-validated graph (cycles allowed).
#[derive(Debug, Clone)]
pub struct Graph {
    /// Nodes keyed by id for O(1) lookup.
    nodes: HashMap<NodeId, Node>,
    /// Forward adjacency: `n` → nodes that depend on `n`.
    forward: HashMap<NodeId, Vec<NodeId>>,
    /// Reverse adjacency: `n` → nodes `n` depends on.
    reverse: HashMap<NodeId, Vec<NodeId>>,
    /// Edges out of each node, in submission order. Carries the optional
    /// `type` tag for fanout routing. The forward adjacency gives
    /// dependents-deduplicated; this preserves multiplicity + tags.
    out_edges: HashMap<NodeId, Vec<Edge>>,
    /// Insertion order of nodes; used to give deterministic iteration order
    /// when callers want it (e.g. dispatch order for tests).
    order: Vec<NodeId>,
}

impl Graph {
    /// Look up a node by id.
    pub fn node(&self, id: &str) -> Option<&Node> {
        self.nodes.get(id)
    }

    /// All node ids, in submission order.
    pub fn ids_in_order(&self) -> &[NodeId] {
        &self.order
    }

    /// Nodes that depend on `id` (i.e. edges `id → x`).
    pub fn dependents_of(&self, id: &str) -> &[NodeId] {
        self.forward.get(id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Outgoing edges of `id`, in submission order. Each carries an
    /// optional `type` tag used for fanout-output routing.
    pub fn out_edges_of(&self, id: &str) -> &[Edge] {
        self.out_edges.get(id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Nodes `id` depends on (i.e. edges `x → id`).
    pub fn dependencies_of(&self, id: &str) -> &[NodeId] {
        self.reverse.get(id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Whether `target` is reachable from `start` by following forward
    /// edges (`start → ... → target`). Equivalent to "is the edge
    /// `target → start` a back-edge in a cycle that contains both."
    /// Used by the scheduler's cycle-bootstrap rule: a fanout-bearing
    /// node may fire on its first invocation despite an uncompleted
    /// dep, provided the dep is reachable from this node forward (i.e.
    /// the dep can only complete after this node fires).
    ///
    /// Linear-time per call; the graph is small (typically <10 nodes) so
    /// caching isn't worth the bookkeeping. Self (`start == target`) is
    /// considered reachable when there's a non-empty self-loop or any
    /// outgoing path that returns; the simple `start == target` short-
    /// circuit returns `true` for self-loops only when the edge exists.
    pub fn is_reachable_forward(&self, start: &str, target: &str) -> bool {
        let mut stack: Vec<&str> = self
            .dependents_of(start)
            .iter()
            .map(String::as_str)
            .collect();
        let mut visited: HashSet<&str> = HashSet::new();
        while let Some(n) = stack.pop() {
            if n == target {
                return true;
            }
            if !visited.insert(n) {
                continue;
            }
            for d in self.dependents_of(n) {
                stack.push(d.as_str());
            }
        }
        false
    }

    /// Source nodes — nodes with no incoming edges.
    ///
    /// The scheduler does not call this directly (it walks every node and
    /// uses `is_runnable` instead) but tests find it useful for asserting
    /// graph shape.
    #[cfg(test)]
    pub fn source_nodes(&self) -> Vec<NodeId> {
        self.order
            .iter()
            .filter(|id| self.dependencies_of(id).is_empty())
            .cloned()
            .collect()
    }

    /// Detect a cycle via Kahn's algorithm.
    ///
    /// Returns `None` if the graph is a DAG. Otherwise returns the ids of
    /// nodes that are part of (or downstream of) the cycle — the nodes
    /// whose in-degree never drops to zero.
    ///
    /// **Not used by submit.** Cycles are allowed in the v5 wire model;
    /// this stays available for hypothetical other consumers (debug
    /// tooling, visualizers).
    #[allow(dead_code)]
    pub fn detect_cycle(&self) -> Option<Vec<NodeId>> {
        let mut in_degree: HashMap<&str, usize> = HashMap::new();
        for id in &self.order {
            in_degree.insert(id.as_str(), self.dependencies_of(id).len());
        }
        let mut queue: Vec<&str> = in_degree
            .iter()
            .filter_map(|(k, v)| if *v == 0 { Some(*k) } else { None })
            .collect();
        let mut removed = 0usize;
        while let Some(n) = queue.pop() {
            removed += 1;
            for d in self.dependents_of(n) {
                let entry = in_degree.entry(d.as_str()).or_insert(0);
                *entry = entry.saturating_sub(1);
                if *entry == 0 {
                    queue.push(d.as_str());
                }
            }
        }
        if removed == self.order.len() {
            None
        } else {
            let stuck: Vec<NodeId> = self
                .order
                .iter()
                .filter(|id| in_degree.get(id.as_str()).copied().unwrap_or(0) > 0)
                .cloned()
                .collect();
            Some(stuck)
        }
    }
}

/// Parse a `graph` JSON value (the inner object under `reasoner-graph.run`'s
/// `graph` key) into a [`Graph`]. Returns `Err(reason)` for malformed
/// input — the scheduler turns that into a synthetic `_error` node in
/// `graph.run_complete`.
pub fn parse_graph(graph_value: &Value) -> Result<Graph, String> {
    let obj = graph_value
        .as_object()
        .ok_or_else(|| "`graph` must be an object".to_owned())?;
    let nodes_raw = obj
        .get("nodes")
        .ok_or_else(|| "`graph.nodes` is required".to_owned())?
        .as_array()
        .ok_or_else(|| "`graph.nodes` must be an array".to_owned())?;
    let edges_raw = obj
        .get("edges")
        .map(|v| {
            v.as_array()
                .ok_or_else(|| "`graph.edges` must be an array".to_owned())
        })
        .transpose()?
        .cloned()
        .unwrap_or_default();

    let mut nodes: HashMap<NodeId, Node> = HashMap::new();
    let mut order: Vec<NodeId> = Vec::with_capacity(nodes_raw.len());
    let mut seen: HashSet<NodeId> = HashSet::new();

    for (i, n) in nodes_raw.iter().enumerate() {
        let n_obj = n
            .as_object()
            .ok_or_else(|| format!("node[{i}] must be an object"))?;
        let id = n_obj
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("node[{i}] missing `id` (string)"))?
            .to_owned();
        if id.is_empty() {
            return Err(format!("node[{i}] has empty `id`"));
        }
        if id.starts_with('_') {
            return Err(format!(
                "node[{i}] id {id:?} starts with `_`; reserved for scheduler-synthesized nodes"
            ));
        }
        if !seen.insert(id.clone()) {
            return Err(format!("duplicate node id {id:?}"));
        }
        let reasoner = n_obj
            .get("reasoner")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("node {id:?} missing `reasoner` (string)"))?
            .to_owned();
        if reasoner.is_empty() {
            return Err(format!("node {id:?} has empty `reasoner`"));
        }
        let args = n_obj.get("args").cloned().unwrap_or(Value::Null);
        let fanout = match n_obj.get("fanout") {
            Some(Value::Object(obj)) => Some(parse_fanout_signature(&id, obj)?),
            Some(Value::Null) | None => None,
            Some(other) => {
                return Err(format!(
                    "node {id:?} has invalid `fanout` (must be {{in, out}} object or absent); got {other}"
                ));
            }
        };
        nodes.insert(
            id.clone(),
            Node {
                id: id.clone(),
                reasoner,
                args,
                fanout,
            },
        );
        order.push(id);
    }

    let mut forward: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    let mut reverse: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    let mut out_edges: HashMap<NodeId, Vec<Edge>> = HashMap::new();
    for (i, e) in edges_raw.iter().enumerate() {
        let e_obj = e
            .as_object()
            .ok_or_else(|| format!("edge[{i}] must be an object"))?;
        let from = e_obj
            .get("from")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("edge[{i}] missing `from` (string)"))?
            .to_owned();
        let to = e_obj
            .get("to")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("edge[{i}] missing `to` (string)"))?
            .to_owned();
        if !nodes.contains_key(&from) {
            return Err(format!(
                "edge[{i}] `from` references unknown node id {from:?}"
            ));
        }
        if !nodes.contains_key(&to) {
            return Err(format!("edge[{i}] `to` references unknown node id {to:?}"));
        }
        let type_tag = match e_obj.get("type") {
            Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
            Some(Value::Null) | None => None,
            Some(other) => {
                return Err(format!(
                    "edge[{i}] has invalid `type` (must be string or absent); got {other}"
                ));
            }
        };
        // Self-loops are allowed (cycles are allowed). A node that fans
        // out to itself is the canonical 1-node cycle.
        let fwd = forward.entry(from.clone()).or_default();
        if !fwd.contains(&to) {
            fwd.push(to.clone());
        }
        let rev = reverse.entry(to.clone()).or_default();
        if !rev.contains(&from) {
            rev.push(from.clone());
        }
        out_edges
            .entry(from.clone())
            .or_default()
            .push(Edge { from, to, type_tag });
    }

    Ok(Graph {
        nodes,
        forward,
        reverse,
        out_edges,
        order,
    })
}

/// Parse a `{ "in": "<plugin>.<Type>", "out": ["<plugin>.<Type>", ...] }`
/// fanout signature object.
///
/// Tags are passed through as-is (string equality is the scheduler's only
/// type comparison; semantic ownership lives in `nefor-combinators`).
/// Empty `out` is rejected — a fanout combinator must have at least one
/// output slot per spec §3.3.
fn parse_fanout_signature(
    node_id: &str,
    obj: &Map<String, Value>,
) -> Result<FanoutSignature, String> {
    let in_type = obj
        .get("in")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("node {node_id:?}.fanout missing `in` (string)"))?;
    if in_type.is_empty() || !in_type.contains('.') {
        return Err(format!(
            "node {node_id:?}.fanout.in must be `<plugin>.<Type>`; got {in_type:?}"
        ));
    }
    let out_arr = obj
        .get("out")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("node {node_id:?}.fanout missing `out` (array of strings)"))?;
    if out_arr.is_empty() {
        return Err(format!("node {node_id:?}.fanout.out must be non-empty"));
    }
    let mut out_multiset: Vec<String> = Vec::with_capacity(out_arr.len());
    for (i, v) in out_arr.iter().enumerate() {
        let s = v
            .as_str()
            .ok_or_else(|| format!("node {node_id:?}.fanout.out[{i}] must be a string; got {v}"))?;
        if s.is_empty() || !s.contains('.') {
            return Err(format!(
                "node {node_id:?}.fanout.out[{i}] must be `<plugin>.<Type>`; got {s:?}"
            ));
        }
        out_multiset.push(s.to_owned());
    }
    Ok(FanoutSignature {
        in_type: in_type.to_owned(),
        out_multiset,
    })
}

/// Parse the top-level `reasoner-graph.run` body into its pieces.
pub struct RunSubmission {
    /// Caller-supplied opaque correlation id.
    pub run_id: String,
    /// Validated graph (cycles permitted).
    pub graph: Graph,
    /// Failure policy (default abort).
    pub on_failure: OnNodeFailure,
    /// Per-dispatch ack deadline. Defaults to
    /// [`DEFAULT_ACK_DEADLINE_MS`] when unset on the submission.
    pub ack_deadline_ms: u64,
}

/// Errors from parsing a `reasoner-graph.run` submission. The scheduler
/// turns these into a synthetic `_error` node in `graph.run_complete`.
#[derive(Debug, Clone)]
pub struct SubmissionError {
    /// Human-readable reason carried into `_error.error`.
    pub message: String,
    /// Caller-supplied run_id if we managed to parse it before failing.
    /// `None` means the caller can't correlate the reply — we emit
    /// `graph.run_complete` with an empty run_id, which is the best we
    /// can do; callers should always include `run_id` first.
    pub run_id: Option<String>,
}

/// Parse the body of a `reasoner-graph.run` event.
pub fn parse_submission(body: &Map<String, Value>) -> Result<RunSubmission, SubmissionError> {
    let run_id = body
        .get("run_id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    // Pre-fetch run_id even if other parsing fails, so the caller can
    // correlate the reply.
    let run_id_for_error = run_id.clone();

    let run_id = run_id.ok_or_else(|| SubmissionError {
        message: "missing `run_id` (string)".into(),
        run_id: None,
    })?;

    let graph_value = body.get("graph").ok_or_else(|| SubmissionError {
        message: "missing `graph` (object)".into(),
        run_id: run_id_for_error.clone(),
    })?;
    let graph = parse_graph(graph_value).map_err(|e| SubmissionError {
        message: e,
        run_id: run_id_for_error.clone(),
    })?;

    let on_failure = body
        .get("on_node_failure")
        .map(|v| v.as_str().unwrap_or(""))
        .map(Some)
        .unwrap_or(None);
    let on_failure = OnNodeFailure::parse(on_failure).map_err(|e| SubmissionError {
        message: e,
        run_id: run_id_for_error.clone(),
    })?;

    let ack_deadline_ms = match body.get("ack_deadline_ms") {
        None | Some(Value::Null) => DEFAULT_ACK_DEADLINE_MS,
        Some(Value::Number(n)) => n.as_u64().ok_or_else(|| SubmissionError {
            message: "`ack_deadline_ms` must be a non-negative integer".into(),
            run_id: run_id_for_error.clone(),
        })?,
        Some(other) => {
            return Err(SubmissionError {
                message: format!("`ack_deadline_ms` must be a number; got {other}"),
                run_id: run_id_for_error,
            });
        }
    };

    Ok(RunSubmission {
        run_id,
        graph,
        on_failure,
        ack_deadline_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_minimal_graph_with_one_node() {
        let v = json!({
            "nodes": [{"id": "n1", "reasoner": "r", "args": {"k": 1}}],
            "edges": []
        });
        let g = parse_graph(&v).expect("ok");
        assert_eq!(g.ids_in_order(), &["n1".to_string()]);
        assert!(g.dependencies_of("n1").is_empty());
        assert_eq!(g.source_nodes(), vec!["n1".to_string()]);
        assert!(g.node("n1").unwrap().fanout.is_none());
    }

    #[test]
    fn parse_node_with_fanout_signature() {
        let v = json!({
            "nodes": [{
                "id": "n1",
                "reasoner": "openai-provider",
                "args": {},
                "fanout": {
                    "in": "generic-provider.ProviderOut",
                    "out": ["generic-tool.ToolCalls", "generic-provider.FinalAnswer"]
                }
            }],
            "edges": []
        });
        let g = parse_graph(&v).expect("ok");
        let f = g
            .node("n1")
            .unwrap()
            .fanout
            .as_ref()
            .expect("fanout parsed");
        assert_eq!(f.in_type, "generic-provider.ProviderOut");
        assert_eq!(f.out_multiset.len(), 2);
        assert!(f
            .out_multiset
            .contains(&"generic-tool.ToolCalls".to_string()));
        assert!(f
            .out_multiset
            .contains(&"generic-provider.FinalAnswer".to_string()));
    }

    #[test]
    fn parse_rejects_fanout_without_dot_in_in_type() {
        let v = json!({
            "nodes": [{
                "id": "n1",
                "reasoner": "r",
                "fanout": { "in": "ToolCalls", "out": ["A.B"] }
            }],
            "edges": []
        });
        let err = parse_graph(&v).unwrap_err();
        assert!(err.contains("plugin"));
    }

    #[test]
    fn parse_rejects_fanout_with_empty_out() {
        let v = json!({
            "nodes": [{
                "id": "n1",
                "reasoner": "r",
                "fanout": { "in": "p.T", "out": [] }
            }],
            "edges": []
        });
        let err = parse_graph(&v).unwrap_err();
        assert!(err.contains("non-empty"));
    }

    #[test]
    fn parse_edge_with_type_tag() {
        let v = json!({
            "nodes": [
                {"id": "n1", "reasoner": "r"},
                {"id": "n2", "reasoner": "r"}
            ],
            "edges": [{ "from": "n1", "to": "n2", "type": "generic-tool.ToolCalls" }]
        });
        let g = parse_graph(&v).expect("ok");
        let edges = g.out_edges_of("n1");
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].type_tag.as_deref(), Some("generic-tool.ToolCalls"));
    }

    #[test]
    fn parse_edge_without_type_tag_keeps_none() {
        let v = json!({
            "nodes": [
                {"id": "n1", "reasoner": "r"},
                {"id": "n2", "reasoner": "r"}
            ],
            "edges": [{ "from": "n1", "to": "n2" }]
        });
        let g = parse_graph(&v).expect("ok");
        let edges = g.out_edges_of("n1");
        assert_eq!(edges.len(), 1);
        assert!(edges[0].type_tag.is_none());
    }

    #[test]
    fn parse_linear_chain() {
        let v = json!({
            "nodes": [
                {"id": "n1", "reasoner": "r", "args": {}},
                {"id": "n2", "reasoner": "r", "args": {}},
                {"id": "n3", "reasoner": "r", "args": {}}
            ],
            "edges": [
                {"from": "n1", "to": "n2"},
                {"from": "n2", "to": "n3"}
            ]
        });
        let g = parse_graph(&v).expect("ok");
        assert_eq!(g.dependents_of("n1"), &["n2".to_string()]);
        assert_eq!(g.dependents_of("n2"), &["n3".to_string()]);
        assert_eq!(g.dependencies_of("n3"), &["n2".to_string()]);
        assert_eq!(g.source_nodes(), vec!["n1".to_string()]);
    }

    #[test]
    fn rejects_duplicate_id() {
        let v = json!({
            "nodes": [
                {"id": "n1", "reasoner": "r", "args": {}},
                {"id": "n1", "reasoner": "r", "args": {}}
            ],
            "edges": []
        });
        let err = parse_graph(&v).unwrap_err();
        assert!(err.contains("duplicate"));
    }

    #[test]
    fn rejects_dangling_edge() {
        let v = json!({
            "nodes": [{"id": "n1", "reasoner": "r", "args": {}}],
            "edges": [{"from": "n1", "to": "missing"}]
        });
        let err = parse_graph(&v).unwrap_err();
        assert!(err.contains("missing"));
    }

    #[test]
    fn rejects_underscore_id_reserved_for_scheduler() {
        let v = json!({
            "nodes": [{"id": "_cycle", "reasoner": "r", "args": {}}],
            "edges": []
        });
        let err = parse_graph(&v).unwrap_err();
        assert!(err.contains("reserved"));
    }

    #[test]
    fn self_loop_is_allowed() {
        // Cycles (including 1-node self-loops) parse cleanly. The
        // scheduler no longer rejects cycles at submit.
        let v = json!({
            "nodes": [{"id": "n1", "reasoner": "r", "args": {}}],
            "edges": [{"from": "n1", "to": "n1"}]
        });
        let g = parse_graph(&v).expect("self-loop ok");
        assert_eq!(g.dependents_of("n1"), &["n1".to_string()]);
        assert_eq!(g.dependencies_of("n1"), &["n1".to_string()]);
    }

    #[test]
    fn detects_simple_cycle() {
        let v = json!({
            "nodes": [
                {"id": "n1", "reasoner": "r", "args": {}},
                {"id": "n2", "reasoner": "r", "args": {}}
            ],
            "edges": [
                {"from": "n1", "to": "n2"},
                {"from": "n2", "to": "n1"}
            ]
        });
        let g = parse_graph(&v).expect("ok");
        let stuck = g.detect_cycle().expect("cycle present");
        assert_eq!(stuck.len(), 2);
    }

    #[test]
    fn no_cycle_in_dag() {
        let v = json!({
            "nodes": [
                {"id": "n1", "reasoner": "r", "args": {}},
                {"id": "n2", "reasoner": "r", "args": {}},
                {"id": "n3", "reasoner": "r", "args": {}}
            ],
            "edges": [
                {"from": "n1", "to": "n3"},
                {"from": "n2", "to": "n3"}
            ]
        });
        let g = parse_graph(&v).expect("ok");
        assert!(g.detect_cycle().is_none());
    }

    #[test]
    fn on_node_failure_default_is_abort() {
        assert_eq!(OnNodeFailure::parse(None).unwrap(), OnNodeFailure::Abort);
    }

    #[test]
    fn on_node_failure_parses_continue() {
        assert_eq!(
            OnNodeFailure::parse(Some("continue")).unwrap(),
            OnNodeFailure::Continue
        );
    }

    #[test]
    fn on_node_failure_rejects_unknown() {
        assert!(OnNodeFailure::parse(Some("retry")).is_err());
    }

    #[test]
    fn parse_submission_default_ack_deadline() {
        let body = json!({
            "kind": "reasoner-graph.run",
            "run_id": "r1",
            "graph": {"nodes": [{"id": "n1", "reasoner": "r"}], "edges": []}
        });
        let sub = parse_submission(body.as_object().unwrap()).unwrap();
        assert_eq!(sub.ack_deadline_ms, DEFAULT_ACK_DEADLINE_MS);
    }

    #[test]
    fn parse_submission_custom_ack_deadline() {
        let body = json!({
            "kind": "reasoner-graph.run",
            "run_id": "r1",
            "graph": {"nodes": [{"id": "n1", "reasoner": "r"}], "edges": []},
            "ack_deadline_ms": 250
        });
        let sub = parse_submission(body.as_object().unwrap()).unwrap();
        assert_eq!(sub.ack_deadline_ms, 250);
    }
}
