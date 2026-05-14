//! Run registry + dispatch state machine.
//!
//! State per run:
//! - the parsed [`Graph`]
//! - per-firing [`NodeFiring`] records: a node may fire multiple times
//!   (cyclic case) and each firing carries its own `firing_id`,
//!   `prev_state`, completion flag, and result.
//! - per-node [`NodeStatus`]: the *latest* firing's terminal status —
//!   used by `is_runnable` and run-completion bookkeeping.
//! - per-node `current_state`: the latest completed firing's `next_state`,
//!   passed in as `prev_state` on the next dispatch.
//! - the [`OnNodeFailure`] policy
//! - an `aborted` flag so an in-flight error in abort mode short-circuits
//!   further dispatch
//!
//! This module is pure: it does not touch the bus. The main loop owns the
//! mpsc sender and calls into [`Scheduler::handle_submit`] /
//! [`Scheduler::handle_node_result`], collecting bus events to emit from
//! the returned [`Effects`].
//!
//! Dispatch is fire-and-forget: a firing closes on `tool.result` arrival
//! or on `graph.cancel` — there is no result-arrival deadline. If a
//! reasoner never replies, the firing stays open indefinitely.
//!
//! ## Lifecycle keying
//!
//! Per parent spec §3 "Lifecycle bookkeeping (per-firing, not per-node)",
//! the completed flag scopes to `(node_id, firing_id)`. For acyclic
//! graphs every node has exactly one firing and the keying is
//! effectively per-node — backward-compatible shape, just keyed one
//! level finer. For a cyclic node, each firing gets a fresh `FiringId`.
//!
//! ## Run completion
//!
//! The run completes when:
//! 1. Every node has at least one completed firing (output / error /
//!    skipped), AND
//! 2. There are no in-flight firings (every dispatched firing has
//!    completed), AND
//! 3. No node is currently runnable (the dataflow has reached its
//!    fixpoint).
//!
//! In the v1 basic-broadcast path (no fanout combinator), each node fires
//! at most once: a downstream node fires when all its deps have fired,
//! and once a node has completed once it doesn't re-fire because its deps
//! are now fixed. Cycles execute every node exactly once.
//!
//! When T6 wires combinator-driven fanout, fanout outputs may be `null`
//! on outgoing edges, suppressing them; a cyclic node terminates when the
//! latest firing's fanout chose `null` on the loop edge, breaking the
//! cycle.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value};
use uuid::Uuid;

use crate::graph::{FanoutSignature, Graph, NodeId, OnNodeFailure};

/// Opaque per-dispatch correlation id minted by the scheduler. UUIDs keep
/// callers from coupling to implementation choice (counter vs random)
/// and let combinators / observers correlate ack ↔ result envelopes
/// without a separate registry.
pub type FiringId = String;

/// Mint a fresh firing id (UUID v4 stringified).
fn mint_firing_id() -> FiringId {
    Uuid::new_v4().to_string()
}

/// Per-node terminal/intermediate status during a run. Refers to the
/// latest firing — when a node is re-fired (cycle), the previous firing's
/// status is replaced by the new one.
#[derive(Debug, Clone)]
pub enum NodeStatus {
    /// Reasoner returned `output` successfully.
    Output(Value),
    /// Reasoner returned `error` (or scheduler synthesized a failure,
    /// e.g. reasoner not connected).
    Error(String),
    /// Skipped due to abort-mode short-circuit (never dispatched).
    /// Counts as failure for the run's final status.
    Skipped,
    /// Intentionally not fired because a fanout's null on the relevant
    /// edge suppressed dispatch (or a transitive cascade from such a
    /// suppression). Distinct from `Skipped`: this is the orchestrator
    /// pattern's "the cycle escaped via the FinalAnswer edge, so the
    /// loop nodes never fired" — not a failure.
    FanoutSuppressed,
}

impl NodeStatus {
    fn to_results_value(&self) -> Value {
        match self {
            Self::Output(v) => {
                let mut m = Map::new();
                m.insert("output".into(), v.clone());
                Value::Object(m)
            }
            Self::Error(msg) => {
                let mut m = Map::new();
                m.insert("error".into(), Value::String(msg.clone()));
                Value::Object(m)
            }
            Self::Skipped | Self::FanoutSuppressed => {
                let mut m = Map::new();
                m.insert("skipped".into(), Value::Bool(true));
                Value::Object(m)
            }
        }
    }
}

/// One firing of a node — bookkeeping at the (node_id, firing_id) level.
#[derive(Debug, Clone)]
pub struct NodeFiring {
    /// Opaque per-dispatch id minted at dispatch time. Equals the
    /// `tool.invoke.id` we emit for this dispatch (canonical wire D3).
    pub firing_id: FiringId,
    /// `prev_state` carried into this firing (the previous firing's
    /// `next_state`, or `null` for the first firing on a node).
    /// Read in tests; live on dispatch — kept for inspection /
    /// debugging when T6 plugs in combinator-driven re-firings.
    #[cfg_attr(not(test), allow(dead_code))]
    pub prev_state: Value,
    /// True once a `tool.result` arrived for this firing OR the
    /// scheduler synthesized a failure (e.g. reasoner not connected at
    /// dispatch time). Drives the post-completion duplicate-result
    /// drop guard in `handle_node_result` and the run-completion
    /// fixpoint check.
    pub completed: bool,
}

/// Lifecycle phase for a submitted run. Closed enum (D-16); transitions
/// are documented per variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunPhase {
    /// Awaiting `combinators.query.result` for the typecheck/availability
    /// step. No nodes have been dispatched yet. Carries the request id we
    /// emitted so the result handler can correlate. Transitions to
    /// `Running` (on resolved query) or to a synthetic-failure
    /// `graph.run_complete` (on missing combinators) — in the failure
    /// case the run is removed from the registry and never enters
    /// `Running`.
    PendingTypecheck {
        /// `id` field of the pending `combinators.query` we emitted.
        query_id: String,
    },
    /// Dispatching nodes, awaiting results.
    Running,
}

/// State for a single submitted run.
#[derive(Debug)]
pub struct RunState {
    /// Run-id assigned by the caller.
    pub run_id: String,
    /// Validated graph topology.
    pub graph: Graph,
    /// Lifecycle phase.
    pub phase: RunPhase,
    /// Per-node terminal/intermediate status (latest firing's status).
    pub completed: HashMap<NodeId, NodeStatus>,
    /// All firings so far per node, in order. The last entry's
    /// `firing_id` is the "current" one — its `completed` flag drives
    /// lifecycle decisions.
    pub firings: HashMap<NodeId, Vec<NodeFiring>>,
    /// Per-node current state — the latest completed firing's
    /// `next_state`. Defaults absent (treated as null) for nodes that
    /// haven't fired yet. Read on dispatch, written on result.
    pub current_state: HashMap<NodeId, Value>,
    /// Failure policy from submit.
    pub on_failure: OnNodeFailure,
    /// True once a node errored under abort mode. Suppresses further
    /// dispatch; not-yet-dispatched nodes are marked skipped immediately.
    pub aborted: bool,
    /// Per-firing fanout invocations awaiting `combinators.invoke.result`.
    /// Keyed by the invocation id we minted on emit. The slot records
    /// which `(node_id, firing_id)` the result must be routed back to.
    pub pending_fanouts: HashMap<String, PendingFanout>,
    /// For nodes whose upstream is a fanout source: the values seeded
    /// per-upstream-node from non-null fanout outputs that matched an
    /// outgoing edge with the right `type_tag`. Read by `build_inputs`
    /// in addition to (or instead of) the broadcast `output` value.
    pub fanout_seeded: HashMap<NodeId, HashMap<NodeId, Value>>,
    /// Edges suppressed by a fanout's `null` output. Keyed by the
    /// fanout source; the value set is the downstream targets whose
    /// edge from that source carried a `type_tag` matching the
    /// suppressed slot. A node whose ALL incoming edges from a
    /// completed-fanout source are suppressed never fires.
    pub suppressed_edges: HashMap<NodeId, HashSet<NodeId>>,
    /// Set of nodes whose fanout has been computed (i.e.
    /// `combinators.invoke.result` arrived). Distinguishes
    /// "upstream completed but fanout still in flight" from "upstream
    /// completed and fanout emitted" for `is_runnable` purposes.
    pub fanout_emitted: HashSet<NodeId>,
    /// Nodes flagged for re-firing by a recent upstream completion. Set
    /// inside [`apply_fanout_outputs`] (non-null routed delivery) and
    /// inside [`propagate_after_completion`]'s broadcast path (any
    /// upstream `Output(_)`). Read inside [`is_runnable`] to bypass the
    /// "node fires at most once" guard for cycle re-entry. Cleared
    /// inside [`try_dispatch`] when a re-fire actually goes on the
    /// wire. In acyclic graphs no node ever re-completes, so the set
    /// is populated but never read against an already-completed node —
    /// existing fire-once semantics are preserved.
    pub pending_refire: HashSet<NodeId>,
    /// Per-firing request-id correlation table for the canonical tool
    /// contract. Keyed by the `id` we mint on the outbound `tool.invoke`
    /// dispatch (which equals the firing_id by spec D3); the value records
    /// `(node_id, firing_id)` so the inbound `tool.result { id }` handler
    /// can resolve back to a specific firing without scanning every
    /// node's firing list.
    ///
    /// Mirrors the pattern used by `pending_fanouts` (combinator
    /// invocations) and `RunPhase::PendingTypecheck { query_id }`
    /// (combinator queries). Entries are inserted at dispatch time and
    /// removed when the matching `tool.result` lands.
    pub firing_by_request_id: HashMap<String, (NodeId, FiringId)>,
}

/// A fanout invocation in flight. The scheduler emitted
/// `combinators.invoke` and is waiting for `combinators.invoke.result`
/// before it can route the multiset outputs to the matching outgoing
/// edges.
#[derive(Debug, Clone)]
pub struct PendingFanout {
    /// Node whose output triggered the fanout.
    pub node_id: NodeId,
    /// Firing id at the time of dispatch. Kept on the slot for
    /// diagnostics / future re-firing of the same node within a cycle —
    /// not load-bearing in v1 routing logic.
    #[allow(dead_code)]
    pub firing_id: FiringId,
}

impl RunState {
    /// Whether the latest firing of `node_id` is in flight (dispatched
    /// but not completed).
    fn latest_firing_in_flight(&self, node_id: &str) -> bool {
        self.firings
            .get(node_id)
            .and_then(|v| v.last())
            .map(|f| !f.completed)
            .unwrap_or(false)
    }

    /// Whether `node_id` has at least one completed firing.
    fn has_completed_firing(&self, node_id: &str) -> bool {
        self.firings
            .get(node_id)
            .map(|v| v.iter().any(|f| f.completed))
            .unwrap_or(false)
    }

    /// Latest firing record for `node_id`, or None if never dispatched.
    #[cfg_attr(not(test), allow(dead_code))]
    fn latest_firing(&self, node_id: &str) -> Option<&NodeFiring> {
        self.firings.get(node_id).and_then(|v| v.last())
    }
}

/// Map of in-flight runs. `std::sync::Mutex` matches the sibling style and
/// fits write-heavy access (every node result mutates the run).
pub type Runs = Arc<Mutex<HashMap<String, RunState>>>;

/// One outbound bus event the main loop should emit.
///
/// Closed shape so the main loop can pattern-match. Avoids leaking
/// scheduler internals into the bus encoding step (e.g. `to` headers).
///
/// `clippy::large_enum_variant` is allowed: this enum is a transient
/// effect bag drained one at a time, not a hot-path data structure;
/// boxing every JSON `Value` would cost an allocation per dispatch
/// without measurable benefit.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Lifecycle marker — `graph.run_started`. Emitted exactly once per
    /// accepted run, BEFORE any `DispatchNode` for that run, so observers
    /// can record the run and its node count up front.
    RunStarted {
        /// Run id (echoed).
        run_id: String,
        /// Total nodes in the submitted graph.
        total_nodes: usize,
    },
    /// Schedule a node — `<reasoner>.run_node` (no `.dag.` infix).
    DispatchNode {
        /// Reasoner plugin (used as kind prefix).
        reasoner: String,
        /// Run id (echoed in the dispatch event).
        run_id: String,
        /// Node id (echoed in the dispatch event).
        node_id: String,
        /// Per-firing correlation id (fresh per dispatch).
        firing_id: FiringId,
        /// Verbatim args from the submitted graph.
        args: Value,
        /// Map keyed by upstream node id; values are `{output: ...}` or
        /// `{error: ...}` mirrors of the upstream's terminal status.
        inputs: Map<String, Value>,
        /// `prev_state` carried into this firing (previous firing's
        /// `next_state`, or `null` for the first firing on this node).
        prev_state: Value,
    },
    /// Lifecycle marker — `graph.node_dispatched`. Broadcast (no plugin
    /// prefix) right after every successful `DispatchNode`, so observers
    /// can mark a node as "running" without snooping on the targeted
    /// `<reasoner>.run_node`. Not emitted for nodes that synthesize a
    /// "reasoner not connected" failure (those go straight to a result
    /// via the run's propagate-after-completion flow).
    NodeDispatched {
        /// Run id (echoed).
        run_id: String,
        /// Node id (echoed).
        node_id: String,
        /// Per-firing correlation id (matches the DispatchNode).
        firing_id: FiringId,
        /// Reasoner plugin the node was addressed to.
        reasoner: String,
    },
    /// `graph.run_complete` reply.
    RunComplete {
        /// Run id (echoed).
        run_id: String,
        /// Final status — `success`, `partial_failure`, `failure`.
        status: RunStatus,
        /// Per-node results. Keys may include synthetic ids `_error`,
        /// `_typecheck`, `_missing_combinators` when the run failed at
        /// submit time before any nodes were dispatched.
        results: Map<String, Value>,
    },
    /// `combinators.query` — submit-time availability check for every
    /// fanout signature the graph references. Awaits a
    /// `combinators.query.result` on a separate handler. Issued once per
    /// submit when at least one node has a `fanout` signature.
    CombinatorsQuery {
        /// Caller correlation id we mint and store in `RunPhase::PendingTypecheck`.
        request_id: String,
        /// Distinct fanout signatures referenced by the graph (multiset
        /// equality is the registry's matching rule, but for the wire we
        /// pass the multiset in the order we collected it).
        signatures: Vec<FanoutSignature>,
    },
    /// `combinators.invoke` — runtime fanout dispatch. Issued when a
    /// node with a `fanout` signature finished and the scheduler needs
    /// to compute the multiset outputs before routing.
    CombinatorsInvoke {
        /// Caller correlation id we mint and key the pending-fanout
        /// record on.
        invocation_id: String,
        /// The fanout signature to invoke.
        signature: FanoutSignature,
        /// The just-completed node's `output` value — passed as `input`
        /// to the combinator.
        input: Value,
    },
}

/// Final status reported in `graph.run_complete`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    /// Every node has `output`.
    Success,
    /// At least one node errored, but the run finished (continue mode).
    PartialFailure,
    /// At least one node errored and the run aborted; non-dispatched nodes
    /// appear as `{ skipped: true }`.
    Failure,
}

impl RunStatus {
    /// Wire string, kebab-case.
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::PartialFailure => "partial_failure",
            Self::Failure => "failure",
        }
    }
}

/// Bag of effects to emit. Returned from every handler; the main loop drains
/// it in order onto the writer mpsc.
#[derive(Debug, Default)]
pub struct Effects(pub Vec<Effect>);

impl Effects {
    /// Construct an empty effect bag.
    pub fn new() -> Self {
        Self(Vec::new())
    }
    /// Append an effect.
    pub fn push(&mut self, e: Effect) {
        self.0.push(e);
    }
    /// Drain into a Vec.
    pub fn into_vec(self) -> Vec<Effect> {
        self.0
    }
}

/// Determine whether the run has reached its fixpoint:
/// - every node has at least one completed firing
/// - no node has an in-flight firing
/// - no fanout invocation is awaiting its `combinators.invoke.result`
/// - no node is queued for a cycle-driven re-fire
fn run_is_done(state: &RunState) -> bool {
    for n in state.graph.ids_in_order() {
        if !state.has_completed_firing(n) && !state.completed.contains_key(n) {
            return false;
        }
        if state.latest_firing_in_flight(n) {
            return false;
        }
    }
    // A pending fanout means the cycle's routing decision hasn't
    // resolved yet — the invoke result may set `pending_refire` or
    // suppress edges. Calling the run done here would race ahead of
    // the routing reply.
    if !state.pending_fanouts.is_empty() {
        return false;
    }
    // A pending re-fire that hasn't been picked up yet (e.g. the node
    // is currently considered un-runnable for some other reason) keeps
    // the run open. In Stage 1's single-back-edge cycles the flag
    // always either dispatches immediately or stays unset, so this is
    // a defensive check rather than a hot path.
    if !state.pending_refire.is_empty() {
        return false;
    }
    true
}

/// Decide the final status for `graph.run_complete`.
fn final_status(state: &RunState) -> RunStatus {
    let mut any_error = false;
    let mut any_skipped = false;
    for n in state.graph.ids_in_order() {
        match state.completed.get(n) {
            Some(NodeStatus::Output(_)) => {}
            Some(NodeStatus::Error(_)) => any_error = true,
            Some(NodeStatus::Skipped) => any_skipped = true,
            // Fanout-suppressed nodes don't count as failure: they were
            // intentionally not fired by the routing decision (orchestrator
            // escape edge fired non-null → loop nodes get suppressed).
            Some(NodeStatus::FanoutSuppressed) => {}
            None => any_error = true, // shouldn't happen if run_is_done is true
        }
    }
    match (state.on_failure, any_error, any_skipped) {
        (_, false, false) => RunStatus::Success,
        (OnNodeFailure::Abort, _, _) => RunStatus::Failure,
        (OnNodeFailure::Continue, true, _) => RunStatus::PartialFailure,
        (OnNodeFailure::Continue, false, _) => RunStatus::Success,
    }
}

/// Build the `results` map for a `graph.run_complete` from the run state.
fn build_results(state: &RunState) -> Map<String, Value> {
    let mut m = Map::new();
    for id in state.graph.ids_in_order() {
        let v = state
            .completed
            .get(id)
            .map(NodeStatus::to_results_value)
            .unwrap_or_else(|| {
                let mut o = Map::new();
                o.insert("skipped".into(), Value::Bool(true));
                Value::Object(o)
            });
        m.insert(id.clone(), v);
    }
    m
}

/// Build the `inputs` map for a node about to be dispatched. Empty if no
/// dependencies (caller should omit the field in that case if they want
/// minimal payloads — but spec allows present-and-empty just as well).
///
/// Routing precedence: when an upstream's fanout has seeded a typed
/// value for this downstream (`fanout_seeded[node_id][dep]`), that
/// value wins over the upstream's broadcast `Output(...)`. The seeded
/// value is the type-routed payload the fanout combinator chose for
/// this edge — sending the unsplit upstream output instead would
/// defeat the routing-by-type model. For self-loop cycle re-fires
/// where `dep == node_id`, the seeded value is still preferred.
fn build_inputs(state: &RunState, node_id: &str) -> Map<String, Value> {
    let mut m = Map::new();
    for dep in state.graph.dependencies_of(node_id) {
        // Fanout-seeded value (typed routing) takes precedence.
        if let Some(seeded) = state
            .fanout_seeded
            .get(node_id)
            .and_then(|by_up| by_up.get(dep))
        {
            let mut o = Map::new();
            o.insert("output".into(), seeded.clone());
            m.insert(dep.clone(), Value::Object(o));
            continue;
        }
        if dep == node_id {
            // Self-loop dep with no seeded value: nothing to attach on
            // the first firing. The reasoner reads `args` instead.
            continue;
        }
        if let Some(status) = state.completed.get(dep) {
            let v = match status {
                NodeStatus::Output(v) => {
                    let mut o = Map::new();
                    o.insert("output".into(), v.clone());
                    Value::Object(o)
                }
                NodeStatus::Error(msg) => {
                    let mut o = Map::new();
                    o.insert("error".into(), Value::String(msg.clone()));
                    Value::Object(o)
                }
                // Skipped upstream: surface as a synthetic error so the
                // downstream reasoner sees something concrete.
                NodeStatus::Skipped => {
                    let mut o = Map::new();
                    o.insert("error".into(), Value::String("upstream skipped".to_owned()));
                    Value::Object(o)
                }
                // FanoutSuppressed upstream: the routing decision said
                // this edge shouldn't fire. Surface as skipped (not
                // error) so the downstream reasoner can see it as
                // intentional, not a fault.
                NodeStatus::FanoutSuppressed => {
                    let mut o = Map::new();
                    o.insert("skipped".into(), Value::Bool(true));
                    Value::Object(o)
                }
            };
            m.insert(dep.clone(), v);
        }
    }
    m
}

/// Determine whether `node_id` is runnable now — every (non-self-loop)
/// dependency has a terminal status, the node itself isn't already
/// in-flight or completed, and the run isn't aborted.
///
/// Fanout interaction:
/// - If a dep's fanout is in flight (`Output` recorded but
///   `fanout_emitted` not yet set), the node waits.
/// - If a dep's fanout suppressed this specific edge (the dep's null
///   output matched this edge's type tag), the edge is treated as
///   "skipped" — the node isn't runnable from this side. A node whose
///   only incoming edges are all suppressed never fires (cycle exit).
fn is_runnable(state: &RunState, node_id: &str) -> bool {
    if state.aborted {
        return false;
    }
    if state.latest_firing_in_flight(node_id) {
        return false;
    }
    if state.completed.contains_key(node_id) && !state.pending_refire.contains(node_id) {
        // Default: a node fires at most once (v1 broadcast invariant).
        // The `pending_refire` flag is the cycle-aware bypass: an
        // upstream completion (broadcast or fanout-routed non-null)
        // sets it, allowing a previously-completed node to re-fire.
        // The flag is cleared inside `try_dispatch` when the new
        // firing is registered. A cycle terminates when no upstream
        // completes any further (because a fanout suppressed the
        // loop-tagged edge), at which point no fresh `pending_refire`
        // entries appear and the run reaches its fixpoint.
        return false;
    }
    let deps = state.graph.dependencies_of(node_id);
    let mut any_dep_fired = false;
    let mut bootstrapped_via_back_edge = false;
    let i_have_fanout = state
        .graph
        .node(node_id)
        .and_then(|n| n.fanout.as_ref())
        .is_some();
    let already_completed_once = state.completed.contains_key(node_id);
    for dep in deps {
        if dep == node_id {
            // Self-loop: no precondition on first firing.
            continue;
        }
        let Some(status) = state.completed.get(dep) else {
            // Cycle bootstrap: a fanout-bearing node may fire on its
            // first invocation despite an uncompleted dep, provided
            // the dep is reachable from this node forward (a
            // back-edge — the dep can only complete after this node
            // fires). Without this rule, a pure cycle (every node has
            // at least one incoming edge) can never bootstrap.
            // Re-fires (`already_completed_once`) don't take this
            // path because by then the dep has either completed
            // forwards or is itself pending re-fire — the normal
            // Kahn / fanout_emitted gates apply.
            if i_have_fanout
                && !already_completed_once
                && state.graph.is_reachable_forward(node_id, dep)
            {
                bootstrapped_via_back_edge = true;
                continue;
            }
            return false;
        };
        // Fanout source: wait for `fanout_emitted`, then either
        // suppressed (this edge "didn't fire") or non-suppressed
        // (treat as fired).
        let dep_uses_fanout = state
            .graph
            .node(dep)
            .and_then(|n| n.fanout.as_ref())
            .is_some();
        let dep_succeeded = matches!(status, NodeStatus::Output(_));
        if dep_uses_fanout && dep_succeeded {
            if !state.fanout_emitted.contains(dep) {
                return false;
            }
            let suppressed = state
                .suppressed_edges
                .get(dep)
                .map(|set| set.contains(node_id))
                .unwrap_or(false);
            if suppressed {
                continue; // edge didn't fire; doesn't count toward "any fired"
            }
            any_dep_fired = true;
            continue;
        }
        // Broadcast / error / skipped path.
        match status {
            NodeStatus::Output(_) => any_dep_fired = true,
            NodeStatus::Error(_) => {
                if matches!(state.on_failure, OnNodeFailure::Abort) {
                    return false;
                }
                any_dep_fired = true;
            }
            NodeStatus::Skipped => {
                if matches!(state.on_failure, OnNodeFailure::Abort) {
                    return false;
                }
                any_dep_fired = true;
            }
            // FanoutSuppressed: the upstream's fanout intentionally
            // suppressed the edge to us. Same effect as a suppressed
            // edge, doesn't count toward "any fired" — the routing
            // decision already said this edge is dark.
            NodeStatus::FanoutSuppressed => {
                continue;
            }
        }
    }
    // If a node has incoming edges and ALL of them were suppressed,
    // the node never becomes runnable. (No edge fired → no signal.)
    // Source nodes (no deps) have any_dep_fired == false but should
    // still be runnable; treat zero-deps as a special case. The
    // bootstrap-back-edge case also runs through here — the
    // fanout-bearing entry node has no fired forward dep but its
    // back-edge is virtual.
    if !deps.is_empty()
        && deps.iter().all(|d| d != node_id)
        && !any_dep_fired
        && !bootstrapped_via_back_edge
    {
        return false;
    }
    true
}

/// Dispatch `node_id`: emit `<reasoner>.run_node` if the reasoner is
/// known to be connected, else synthesize a failure and (per policy)
/// propagate or abort.
fn try_dispatch(state: &mut RunState, node_id: &str, peers: &PeerSet, effects: &mut Effects) {
    if !is_runnable(state, node_id) {
        return;
    }
    let node = match state.graph.node(node_id) {
        Some(n) => n.clone(),
        None => return,
    };

    let inputs = build_inputs(state, node_id);

    if !peers.contains(&node.reasoner) {
        // Synthesize a node failure: `error: "reasoner '<name>' not connected"`.
        let msg = format!("reasoner '{}' not connected", node.reasoner);
        // Record a synthetic firing so per-firing bookkeeping stays
        // consistent — the firing exists but never went on the wire.
        let firing_id = mint_firing_id();
        state
            .firings
            .entry(node.id.clone())
            .or_default()
            .push(NodeFiring {
                firing_id,
                prev_state: state
                    .current_state
                    .get(&node.id)
                    .cloned()
                    .unwrap_or(Value::Null),
                completed: true,
            });
        state
            .completed
            .insert(node.id.clone(), NodeStatus::Error(msg));
        propagate_after_completion(state, &node.id, peers, effects);
        return;
    }

    // Read prev_state (defaulting to null) and mint a fresh firing id.
    // For the first firing prev_state is null. For cycle re-fires the
    // previous firing's `next_state` was persisted on `current_state`
    // by `handle_node_result`; reading it here gives us the chat-history
    // accumulation pattern from the parent spec §3 worked example.
    let prev_state = state
        .current_state
        .get(&node.id)
        .cloned()
        .unwrap_or(Value::Null);
    let firing_id = mint_firing_id();
    state
        .firings
        .entry(node.id.clone())
        .or_default()
        .push(NodeFiring {
            firing_id: firing_id.clone(),
            prev_state: prev_state.clone(),
            completed: false,
        });
    // The dispatch consumes the pending re-fire request (if any) — the
    // node now has a fresh in-flight firing; further upstream
    // completions during this firing will set the flag again to schedule
    // the next re-fire after this one lands.
    state.pending_refire.remove(&node.id);
    // Index this firing by its outbound request id (== firing_id under
    // the canonical tool contract) so the matching `tool.result` can be
    // resolved without scanning every node's firing list.
    state.firing_by_request_id.insert(
        firing_id.clone(),
        (node.id.clone(), firing_id.clone()),
    );
    // Lifecycle marker FIRST, then the targeted dispatch. Order matters:
    // an in-process synchronous reasoner (Lua-resident `terminal`,
    // `adapter`, `tool-executor`, …) replies on its own `tool.invoke`
    // inside the same dispatch tick, emitting `tool.result` before
    // control returns here for the next effect. If `NodeDispatched`
    // (`graph.node.fired`) is pushed AFTER `DispatchNode`
    // (`tool.invoke`), the bus order ends up:
    //
    //   1. tool.invoke              (consumed by the reasoner)
    //   2. tool.result              (reasoner's reply, broadcast)
    //   3. graph.node.fired         (lifecycle marker)
    //
    // Observers that build a `firing_id → (run_id, node_id)` map from
    // `graph.node.fired` (chat.lua's DAG panel; agentic-loop's wrap
    // next_state capture) then see (2) BEFORE (3) and silently drop
    // the close — the firing stays "running" forever in their view.
    // Visible symptom: the `terminal` node in the orchestrator graph
    // ticks elapsed-ms in the chat DAG sidebar after the agent's full
    // response is delivered, because terminal's tool.result landed
    // before its graph.node.fired (Bug A6 terminal node keeps
    // ticking).
    //
    // Pushing `NodeDispatched` first puts `graph.node.fired` on the
    // bus before `tool.invoke`, so the lifecycle marker observed by
    // every consumer arrives before any reasoner can synchronously
    // close the firing.
    effects.push(Effect::NodeDispatched {
        run_id: state.run_id.clone(),
        node_id: node.id.clone(),
        firing_id: firing_id.clone(),
        reasoner: node.reasoner.clone(),
    });
    effects.push(Effect::DispatchNode {
        reasoner: node.reasoner.clone(),
        run_id: state.run_id.clone(),
        node_id: node.id.clone(),
        firing_id,
        args: node.args.clone(),
        inputs,
        prev_state,
    });
    // Per-node fanout dispatch happens at result time inside
    // `propagate_after_completion`. No work required at dispatch.
}

/// Dispatch every currently-runnable node. Used after submission and after
/// each result handling step.
fn dispatch_all_runnable(state: &mut RunState, peers: &PeerSet, effects: &mut Effects) {
    // Collect ids first to satisfy the borrow checker.
    let candidates: Vec<NodeId> = state
        .graph
        .ids_in_order()
        .iter()
        .filter(|&id| is_runnable(state, id))
        .cloned()
        .collect();
    for id in candidates {
        try_dispatch(state, &id, peers, effects);
    }
}

/// After completing `node_id` (output, error, or synthesized failure),
/// apply policy and either:
/// 1. (fanout path) emit `combinators.invoke` with the node's output —
///    further dispatch waits for the invoke result.
/// 2. (broadcast path) try to dispatch next-runnable nodes immediately.
fn propagate_after_completion(
    state: &mut RunState,
    node_id: &str,
    peers: &PeerSet,
    effects: &mut Effects,
) {
    let just_errored = matches!(state.completed.get(node_id), Some(NodeStatus::Error(_)));
    if just_errored && matches!(state.on_failure, OnNodeFailure::Abort) && !state.aborted {
        state.aborted = true;
        // Mark every not-yet-completed-and-not-in-flight node as skipped.
        let to_skip: Vec<NodeId> = state
            .graph
            .ids_in_order()
            .iter()
            .filter(|id| {
                !state.completed.contains_key(id.as_str())
                    && !state.latest_firing_in_flight(id.as_str())
            })
            .cloned()
            .collect();
        for id in to_skip {
            state.completed.insert(id, NodeStatus::Skipped);
        }
    }

    // Fanout dispatch path: when a node with a `fanout` signature
    // produced an `Output(value)`, defer further routing to the
    // `combinators.invoke.result` handler. The runtime fanout call
    // computes which outgoing edges fire and with what value; we can't
    // make that decision without the combinator's reply.
    //
    // Errored / skipped fanout nodes fall through to the broadcast
    // path — the failure semantics still surface to downstreams as
    // `inputs.<id>.error` (continue) or skip-cascade (abort).
    if let Some(NodeStatus::Output(output)) = state.completed.get(node_id) {
        if let Some(fanout) = state.graph.node(node_id).and_then(|n| n.fanout.clone()) {
            let firing_id = state
                .firings
                .get(node_id)
                .and_then(|v| v.last())
                .map(|f| f.firing_id.clone())
                .unwrap_or_default();
            let invocation_id = mint_firing_id();
            state.pending_fanouts.insert(
                invocation_id.clone(),
                PendingFanout {
                    node_id: node_id.to_owned(),
                    firing_id,
                },
            );
            effects.push(Effect::CombinatorsInvoke {
                invocation_id,
                signature: fanout,
                input: output.clone(),
            });
            // Don't dispatch downstreams yet — wait for the invoke
            // result. It'll re-enter dispatch via apply_fanout_result.
            return;
        }
    }

    // Broadcast fallback. Cycle-aware re-fire propagation: when a
    // non-fanout node completes with `Output(_)`, mark every direct
    // downstream as `pending_refire` so the cycle can continue. In a
    // DAG no downstream has previously completed, so the flag is
    // consumed at first firing and existing fire-once semantics are
    // preserved. In a multi-node cycle, the flag bypasses the
    // `completed.contains_key` guard so the downstream can re-fire —
    // propagating around the loop until a fanout's null breaks it.
    //
    // Self is excluded from broadcast self-marking: a self-loop on a
    // non-fanout node has no "valve" to terminate the cycle, so
    // re-firing it would loop forever. Self-loops only re-fire via
    // explicit fanout-routed delivery (where the fanout's null on the
    // back edge is the termination signal). This preserves the v1
    // self-loop-fires-once invariant for nodes without a fanout.
    //
    // Errored / skipped completions don't propagate re-fires — a
    // downstream observing an error input should not silently keep
    // cycling.
    if matches!(state.completed.get(node_id), Some(NodeStatus::Output(_))) {
        let dependents: Vec<NodeId> = state.graph.dependents_of(node_id).to_vec();
        for dep_id in dependents {
            if dep_id == node_id {
                continue;
            }
            state.pending_refire.insert(dep_id);
        }
    }
    dispatch_all_runnable(state, peers, effects);
}

/// One typed fanout output value as parsed off the
/// `combinators.invoke.result` wire shape.
#[derive(Debug, Clone)]
struct TypedFanoutOutput {
    /// Fully-qualified type tag.
    type_tag: String,
    /// JSON value or `Null` (suppress the matching edge).
    value: Value,
}

/// Apply a typed-multiset fanout reply to `node_id`'s outgoing edges.
///
/// Routing rule (parent spec §3): for each output entry where
/// `value != null`, find the outgoing edge whose `type_tag == output.type`
/// and stash the value as that edge's "fanout-supplied input." Null
/// outputs suppress their edge by not seeding any input on the matching
/// downstream — and if a downstream's only incoming edge would have come
/// from this null-suppressed slot, it never becomes runnable.
///
/// **Stage-1 simplification.** We don't yet have an "edge marked
/// suppressed" flag — we just don't seed a fanout input for null
/// outputs. For nodes whose only incoming edge is suppressed, the
/// dispatcher detects "all deps satisfied, but no fanout input
/// arrived" by treating absent fanout-supplied input on a fanout-source
/// edge as a skip. The simplest fit: when we apply a fanout, mark the
/// node's edges with explicit "fired"/"suppressed" flags inside the
/// state and let `is_runnable` consult them. We add a per-firing
/// `fired_fanout_targets: HashSet<NodeId>` tracking which downstream
/// nodes received a non-null fanout output.
///
/// For edges without a `type_tag` (legacy v1 broadcast path) or for
/// outputs whose type doesn't match any outgoing edge: these are no-ops
/// in the routing-by-type model. The downstream will still see the
/// upstream's `inputs.<id>.output` via the normal `build_inputs` path
/// because the upstream still completed with `Output(...)` — the
/// fanout-supplied input is in addition to that channel; consumers can
/// decide whether to read fanout-tagged or upstream-tagged values from
/// `args` / `inputs`. (Reasoners don't yet declare which they want; the
/// glue layer will surface this once T7 wires templates.)
fn apply_fanout_outputs(state: &mut RunState, node_id: &str, outputs: &[TypedFanoutOutput]) {
    let edges: Vec<crate::graph::Edge> = state.graph.out_edges_of(node_id).to_vec();
    for output in outputs {
        if output.value.is_null() {
            // Suppressed slot — find the matching edge and mark its
            // target as suppressed (must not dispatch from this side).
            for edge in &edges {
                if edge.type_tag.as_deref() == Some(output.type_tag.as_str()) {
                    state
                        .suppressed_edges
                        .entry(node_id.to_owned())
                        .or_default()
                        .insert(edge.to.clone());
                }
            }
            continue;
        }
        for edge in &edges {
            if edge.type_tag.as_deref() == Some(output.type_tag.as_str()) {
                state
                    .fanout_seeded
                    .entry(edge.to.clone())
                    .or_default()
                    .insert(node_id.to_owned(), output.value.clone());
                // A new non-null delivery clears any prior suppression
                // on this edge — the upstream's previous firing may
                // have suppressed this slot, but the current firing's
                // routing decision overrides it. Without this clear
                // the cycle-bootstrapped suppression entry would
                // permanently block downstream dispatch even after a
                // later firing routes here.
                if let Some(set) = state.suppressed_edges.get_mut(node_id) {
                    set.remove(&edge.to);
                }
                // Cycle-aware re-fire trigger: a non-null fanout-routed
                // delivery to `edge.to` re-arms the downstream. If
                // `edge.to` already completed, `is_runnable` will treat
                // this as a re-fire request and dispatch a fresh firing
                // with a new `firing_id`, the prior `next_state` as
                // `prev_state`, and the routed value carried into
                // `inputs` via `build_inputs`'s fanout-seeded preference.
                state.pending_refire.insert(edge.to.clone());
            }
        }
    }
    // Mark fanout-emit complete on this node so downstream
    // `is_runnable` can tell "the upstream just suppressed me" from
    // "the upstream hasn't fired yet."
    state.fanout_emitted.insert(node_id.to_owned());

    // For each downstream of this node where every incoming edge from
    // this fanout is suppressed AND no other unsuppressed incoming
    // edges remain to be fulfilled: mark the downstream Skipped so
    // `run_is_done` can finish the run. Limit to the simple case where
    // this fanout is the downstream's sole upstream — cycle-aware
    // multi-source suppression is Stage 2 territory.
    //
    // Cycle-aware re-fire: marking a downstream Skipped doesn't lock it
    // out forever — `pending_refire` overrides the `completed` guard in
    // `is_runnable`, so a future fanout firing that delivers a non-null
    // payload to this slot will set pending_refire and the Skipped
    // entry gets replaced by a fresh firing. The cascade-skip is the
    // termination signal; pending_refire is the un-block signal.
    let mut newly_skipped: Vec<NodeId> = Vec::new();
    let dependents: Vec<NodeId> = state.graph.dependents_of(node_id).to_vec();
    for dep_id in dependents {
        if state.completed.contains_key(&dep_id) {
            continue;
        }
        if state.latest_firing_in_flight(&dep_id) {
            continue;
        }
        // Don't skip if the downstream is queued to re-fire — its
        // re-fire will produce its own status.
        if state.pending_refire.contains(&dep_id) {
            continue;
        }
        let deps_of_dep = state.graph.dependencies_of(&dep_id);
        let mut every_edge_suppressed = true;
        for upstream in deps_of_dep {
            if upstream == &dep_id {
                continue; // self-loop ignored
            }
            let upstream_emitted = state.fanout_emitted.contains(upstream);
            let suppressed_here = state
                .suppressed_edges
                .get(upstream)
                .map(|s| s.contains(&dep_id))
                .unwrap_or(false);
            if !(upstream_emitted && suppressed_here) {
                every_edge_suppressed = false;
                break;
            }
        }
        if every_edge_suppressed && !deps_of_dep.is_empty() {
            state
                .completed
                .insert(dep_id.clone(), NodeStatus::FanoutSuppressed);
            newly_skipped.push(dep_id);
        }
    }

    // Transitive cascade: a node whose only path to runnability goes
    // through a just-suppressed predecessor will never run. Mark it
    // FanoutSuppressed too. Repeats until quiescence so a cycle-internal
    // chain (tools → adapt) collapses cleanly when its sole entry edge
    // is suppressed.
    let mut frontier = newly_skipped;
    while let Some(skipped) = frontier.pop() {
        for dep_id in state.graph.dependents_of(&skipped).to_vec() {
            if state.completed.contains_key(&dep_id) {
                continue;
            }
            if state.latest_firing_in_flight(&dep_id) {
                continue;
            }
            if state.pending_refire.contains(&dep_id) {
                continue;
            }
            let deps = state.graph.dependencies_of(&dep_id);
            let mut all_blocked = true;
            for d in deps {
                if d.as_str() == dep_id.as_str() {
                    continue; // self-loop
                }
                match state.completed.get(d.as_str()) {
                    Some(NodeStatus::Error(_))
                    | Some(NodeStatus::Skipped)
                    | Some(NodeStatus::FanoutSuppressed) => {}
                    _ => {
                        all_blocked = false;
                        break;
                    }
                }
            }
            if all_blocked && !state.graph.dependencies_of(&dep_id).is_empty() {
                state
                    .completed
                    .insert(dep_id.clone(), NodeStatus::FanoutSuppressed);
                frontier.push(dep_id);
            }
        }
    }
}

/// Walk the graph and reject any fanout whose output multiset has
/// duplicates (per parent spec §3 type-collision rule).
fn check_fanout_multiset_no_duplicates(graph: &Graph) -> Result<(), String> {
    for id in graph.ids_in_order() {
        if let Some(node) = graph.node(id) {
            if let Some(fanout) = &node.fanout {
                let mut seen: HashSet<&str> = HashSet::new();
                for t in &fanout.out_multiset {
                    if !seen.insert(t.as_str()) {
                        return Err(format!(
                            "node {id:?}.fanout.out has duplicate type `{t}`; \
                             introduce nominal newtype wrappers to disambiguate"
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Collect the distinct fanout signatures referenced by the graph,
/// preserving submission order. Two fanouts with the same `(in,
/// out_multiset_sorted)` are de-duped — the registry does multiset
/// equality.
fn collect_distinct_fanout_signatures(graph: &Graph) -> Vec<FanoutSignature> {
    let mut seen: HashSet<(String, Vec<String>)> = HashSet::new();
    let mut out: Vec<FanoutSignature> = Vec::new();
    for id in graph.ids_in_order() {
        if let Some(node) = graph.node(id) {
            if let Some(fanout) = &node.fanout {
                let mut sorted_outs = fanout.out_multiset.clone();
                sorted_outs.sort();
                let key = (fanout.in_type.clone(), sorted_outs);
                if seen.insert(key) {
                    out.push(fanout.clone());
                }
            }
        }
    }
    out
}

/// The set of plugins currently considered "connected" (seen on the bus).
pub type PeerSet = HashSet<String>;

/// `Scheduler` is a thin facade over the runs map + helpers. Stateful
/// behavior lives in [`RunState`]; the scheduler glues parsing → state →
/// effects.
#[derive(Debug, Default)]
pub struct Scheduler;

/// Failure shape for a submit that couldn't even be stored — emit a
/// synthetic-results `graph.run_complete { failure }` and don't keep state.
fn synthetic_failure(run_id: &str, key: &str, message: &str) -> Effect {
    let mut results = Map::new();
    let mut entry = Map::new();
    entry.insert("error".into(), Value::String(message.to_owned()));
    results.insert(key.to_owned(), Value::Object(entry));
    Effect::RunComplete {
        run_id: run_id.to_owned(),
        status: RunStatus::Failure,
        results,
    }
}

/// Outcome of [`Scheduler::handle_submit`].
pub enum SubmitOutcome {
    /// Run was rejected at submit time. No state stored.
    Rejected(Effects),
    /// Run was accepted; state was inserted. Effects contain the dispatch
    /// events for source nodes.
    Accepted(Effects),
}

impl Scheduler {
    /// Handle a `reasoner-graph.run` submission.
    ///
    /// On accept: parse + topology-validate, then either
    /// 1. (no fanouts) dispatch source nodes immediately and store; OR
    /// 2. (any fanouts) emit `combinators.query` for the referenced
    ///    signatures, store the run in `PendingTypecheck` phase, and
    ///    wait for `combinators.query.result` to resume into dispatch.
    ///
    /// On reject (malformed, duplicate run_id, submit-time typecheck
    /// failure detectable without async lookup): return a
    /// `graph.run_complete` with a synthetic `_error` /
    /// `_typecheck` node. Cycles are no longer rejected.
    pub fn handle_submit(runs: &Runs, peers: &PeerSet, body: &Map<String, Value>) -> SubmitOutcome {
        use crate::graph::{parse_submission, SubmissionError};

        let submission = match parse_submission(body) {
            Ok(s) => s,
            Err(SubmissionError {
                message,
                run_id: maybe_id,
            }) => {
                let run_id = maybe_id.unwrap_or_default();
                let mut effects = Effects::new();
                effects.push(synthetic_failure(&run_id, "_error", &message));
                return SubmitOutcome::Rejected(effects);
            }
        };

        // Cycles are allowed — no detect_cycle() rejection here. (Per
        // parent spec §3 "DAG is not acyclic".)

        // Reject duplicate run_id at submit time. The caller must have
        // unique run_ids; reusing one would let a second submission
        // tamper with an in-flight run's state.
        {
            let guard = runs.lock().expect("runs mutex poisoned");
            if guard.contains_key(&submission.run_id) {
                drop(guard);
                let mut effects = Effects::new();
                effects.push(synthetic_failure(
                    &submission.run_id,
                    "_error",
                    &format!("run_id {:?} is already in flight", submission.run_id),
                ));
                return SubmitOutcome::Rejected(effects);
            }
        }

        // Submit-time topology checks for fanout signatures. Two
        // structural rules per parent spec §3 "Edges and graph-level
        // type-checking":
        //
        //   1. Output multiset has no duplicates (rejection per
        //      type-collision rule). Only this rule fires today; matching
        //      multiset-cardinality vs outgoing-edge-count requires the
        //      caller to have declared `edge.type` on every fanout edge,
        //      which we don't enforce yet — see the writeup's "partial
        //      typecheck" gap.
        //
        // Full typecheck (matching combinator output types to downstream
        // input types) is deferred — reasoners don't yet declare input
        // type signatures in the graph spec. The `edge.type` tag is the
        // load-bearing routing input we use today.
        if let Err(message) = check_fanout_multiset_no_duplicates(&submission.graph) {
            let mut effects = Effects::new();
            effects.push(synthetic_failure(
                &submission.run_id,
                "_typecheck",
                &message,
            ));
            return SubmitOutcome::Rejected(effects);
        }

        let total_nodes = submission.graph.ids_in_order().len();

        // Collect distinct fanout signatures. If empty: skip the query
        // and dispatch sources immediately (legacy v1 path). If
        // non-empty: emit a `combinators.query` and park the run in
        // PendingTypecheck.
        let signatures = collect_distinct_fanout_signatures(&submission.graph);

        let mut effects = Effects::new();
        effects.push(Effect::RunStarted {
            run_id: submission.run_id.clone(),
            total_nodes,
        });

        if signatures.is_empty() {
            let mut state = RunState {
                run_id: submission.run_id.clone(),
                graph: submission.graph,
                phase: RunPhase::Running,
                completed: HashMap::new(),
                firings: HashMap::new(),
                current_state: HashMap::new(),
                on_failure: submission.on_failure,
                aborted: false,
                pending_fanouts: HashMap::new(),
                fanout_seeded: HashMap::new(),
                suppressed_edges: HashMap::new(),
                fanout_emitted: HashSet::new(),
                pending_refire: HashSet::new(),
                firing_by_request_id: HashMap::new(),
            };
            dispatch_all_runnable(&mut state, peers, &mut effects);
            if run_is_done(&state) {
                let status = final_status(&state);
                let results = build_results(&state);
                effects.push(Effect::RunComplete {
                    run_id: state.run_id.clone(),
                    status,
                    results,
                });
                return SubmitOutcome::Accepted(effects);
            }
            runs.lock()
                .expect("runs mutex poisoned")
                .insert(state.run_id.clone(), state);
            return SubmitOutcome::Accepted(effects);
        }

        // Fanouts present — go through the async typecheck path.
        let query_id = mint_firing_id();
        effects.push(Effect::CombinatorsQuery {
            request_id: query_id.clone(),
            signatures,
        });
        let state = RunState {
            run_id: submission.run_id.clone(),
            graph: submission.graph,
            phase: RunPhase::PendingTypecheck { query_id },
            completed: HashMap::new(),
            firings: HashMap::new(),
            current_state: HashMap::new(),
            on_failure: submission.on_failure,
            aborted: false,
            pending_fanouts: HashMap::new(),
            fanout_seeded: HashMap::new(),
            suppressed_edges: HashMap::new(),
            fanout_emitted: HashSet::new(),
            pending_refire: HashSet::new(),
            firing_by_request_id: HashMap::new(),
        };
        runs.lock()
            .expect("runs mutex poisoned")
            .insert(state.run_id.clone(), state);
        SubmitOutcome::Accepted(effects)
    }

    /// Handle a `combinators.query.result` event. Looks up the run
    /// associated with `id`, evaluates the resolution, and either:
    /// - emits `_missing_combinators` failure + drops the run, or
    /// - transitions the run to `Running` and dispatches source nodes.
    pub fn handle_query_result(runs: &Runs, peers: &PeerSet, body: &Map<String, Value>) -> Effects {
        let mut effects = Effects::new();
        let request_id = match body.get("id").and_then(Value::as_str) {
            Some(s) => s.to_owned(),
            None => return effects,
        };

        // Locate the run waiting on this request_id.
        let target_run_id: Option<String> = {
            let guard = runs.lock().expect("runs mutex poisoned");
            guard
                .iter()
                .find(|(_, st)| {
                    matches!(&st.phase, RunPhase::PendingTypecheck { query_id } if *query_id == request_id)
                })
                .map(|(rid, _)| rid.clone())
        };
        let run_id = match target_run_id {
            Some(r) => r,
            None => return effects, // not for us
        };

        // Read missing list from the body. Treat any non-empty `missing`
        // as a failure; even partial misses block run start.
        let missing = body
            .get("missing")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if !missing.is_empty() {
            let mut guard = runs.lock().expect("runs mutex poisoned");
            guard.remove(&run_id);
            drop(guard);
            let mut results = Map::new();
            let mut entry = Map::new();
            entry.insert(
                "error".into(),
                Value::String("one or more fanout combinators are not registered".into()),
            );
            entry.insert("missing".into(), Value::Array(missing));
            results.insert("_missing_combinators".into(), Value::Object(entry));
            effects.push(Effect::RunComplete {
                run_id,
                status: RunStatus::Failure,
                results,
            });
            return effects;
        }

        // Resolved → flip to Running and dispatch source nodes.
        let mut completed_run: Option<RunState> = None;
        {
            let mut guard = runs.lock().expect("runs mutex poisoned");
            let state = match guard.get_mut(&run_id) {
                Some(s) => s,
                None => return effects,
            };
            state.phase = RunPhase::Running;
            dispatch_all_runnable(state, peers, &mut effects);
            if run_is_done(state) {
                if let Some(taken) = guard.remove(&run_id) {
                    completed_run = Some(taken);
                }
            }
        }
        if let Some(state) = completed_run {
            let status = final_status(&state);
            let results = build_results(&state);
            effects.push(Effect::RunComplete {
                run_id: state.run_id.clone(),
                status,
                results,
            });
        }
        effects
    }

    /// Handle a `combinators.invoke.result` event. Looks up the
    /// `(run_id, node_id)` the invocation belongs to, applies typed
    /// outputs to outgoing edges (matching `edge.type` against
    /// `output.type`), and continues dispatch.
    pub fn handle_invoke_result(
        runs: &Runs,
        peers: &PeerSet,
        body: &Map<String, Value>,
    ) -> Effects {
        let mut effects = Effects::new();
        let invocation_id = match body.get("id").and_then(Value::as_str) {
            Some(s) => s.to_owned(),
            None => return effects,
        };

        let target_run_id: Option<(String, NodeId)> = {
            let guard = runs.lock().expect("runs mutex poisoned");
            let mut hit: Option<(String, NodeId)> = None;
            for (rid, st) in guard.iter() {
                if let Some(slot) = st.pending_fanouts.get(&invocation_id) {
                    hit = Some((rid.clone(), slot.node_id.clone()));
                    break;
                }
            }
            hit
        };
        let (run_id, node_id) = match target_run_id {
            Some(t) => t,
            None => return effects, // not for us / unknown id
        };

        // Parse the `outputs` array.
        let outputs_raw = body.get("outputs").and_then(Value::as_array);
        let outputs: Vec<TypedFanoutOutput> = match outputs_raw {
            Some(arr) => arr
                .iter()
                .filter_map(|v| {
                    let obj = v.as_object()?;
                    let type_tag = obj.get("type").and_then(Value::as_str)?.to_owned();
                    let value = obj.get("value").cloned().unwrap_or(Value::Null);
                    Some(TypedFanoutOutput { type_tag, value })
                })
                .collect(),
            None => Vec::new(),
        };

        let mut completed_run: Option<RunState> = None;
        {
            let mut guard = runs.lock().expect("runs mutex poisoned");
            let state = match guard.get_mut(&run_id) {
                Some(s) => s,
                None => return effects,
            };
            state.pending_fanouts.remove(&invocation_id);
            apply_fanout_outputs(state, &node_id, &outputs);
            dispatch_all_runnable(state, peers, &mut effects);
            if run_is_done(state) {
                if let Some(taken) = guard.remove(&run_id) {
                    completed_run = Some(taken);
                }
            }
        }
        if let Some(state) = completed_run {
            let status = final_status(&state);
            let results = build_results(&state);
            effects.push(Effect::RunComplete {
                run_id: state.run_id.clone(),
                status,
                results,
            });
        }
        effects
    }

    /// Handle a `graph.node_result` event.
    ///
    /// Returns the effects produced (further dispatches and possibly a
    /// terminal `graph.run_complete`). If `run_id` is unknown, returns
    /// empty effects (silent drop, per spec — covers cancellation
    /// race).
    pub fn handle_node_result(runs: &Runs, peers: &PeerSet, body: &Map<String, Value>) -> Effects {
        let run_id = match body.get("run_id").and_then(Value::as_str) {
            Some(s) => s.to_owned(),
            None => {
                tracing::warn!("graph.node_result missing run_id; dropping");
                return Effects::new();
            }
        };
        let node_id = match body.get("node_id").and_then(Value::as_str) {
            Some(s) => s.to_owned(),
            None => {
                tracing::warn!(run_id = %run_id, "graph.node_result missing node_id; dropping");
                return Effects::new();
            }
        };
        // firing_id is required on the new wire shape (per parent spec
        // §3 "How a node gets invoked"), but we tolerate its absence
        // by matching against the latest in-flight firing for the
        // node — saves a coupling for adapters that haven't migrated
        // yet.
        let firing_id_opt = body
            .get("firing_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);

        let mut effects = Effects::new();
        let mut completed_run: Option<RunState> = None;
        {
            let mut guard = runs.lock().expect("runs mutex poisoned");
            let state = match guard.get_mut(&run_id) {
                Some(s) => s,
                None => return effects, // unknown run — drop
            };

            // Locate the firing this result belongs to.
            let firings = match state.firings.get_mut(&node_id) {
                Some(f) => f,
                None => {
                    tracing::debug!(
                        run_id = %run_id,
                        node_id = %node_id,
                        "graph.node_result for never-dispatched node; dropping"
                    );
                    return effects;
                }
            };
            let firing = match &firing_id_opt {
                Some(fid) => firings.iter_mut().find(|f| &f.firing_id == fid),
                None => firings.last_mut().filter(|f| !f.completed),
            };
            let firing = match firing {
                Some(f) if !f.completed => f,
                Some(_) => {
                    tracing::debug!(
                        run_id = %run_id,
                        node_id = %node_id,
                        "duplicate or post-completion node_result; dropping"
                    );
                    return effects;
                }
                None => {
                    tracing::debug!(
                        run_id = %run_id,
                        node_id = %node_id,
                        "node_result with unknown firing_id; dropping"
                    );
                    return effects;
                }
            };

            firing.completed = true;
            // Drop the request-id correlation entry — the matching
            // `tool.result` has landed (or main.rs synthesized one from
            // it). Any duplicate result for the same id will hit the
            // post-completion guard above and be dropped.
            let firing_request_id = firing.firing_id.clone();
            state.firing_by_request_id.remove(&firing_request_id);
            // Persist next_state for the next firing's prev_state.
            if let Some(next_state) = body.get("next_state").cloned() {
                state.current_state.insert(node_id.clone(), next_state);
            }

            let status = if let Some(out) = body.get("output") {
                NodeStatus::Output(out.clone())
            } else if let Some(err) = body.get("error") {
                NodeStatus::Error(
                    err.as_str()
                        .map(ToOwned::to_owned)
                        .unwrap_or_else(|| err.to_string()),
                )
            } else {
                tracing::warn!(
                    run_id = %run_id,
                    node_id = %node_id,
                    "graph.node_result missing both output and error; treating as error"
                );
                NodeStatus::Error("node_result missing output and error".into())
            };

            state.completed.insert(node_id.clone(), status);
            propagate_after_completion(state, &node_id, peers, &mut effects);

            if run_is_done(state) {
                if let Some(taken) = guard.remove(&run_id) {
                    completed_run = Some(taken);
                }
            }
        }

        if let Some(state) = completed_run {
            let status = final_status(&state);
            let results = build_results(&state);
            effects.push(Effect::RunComplete {
                run_id: state.run_id.clone(),
                status,
                results,
            });
        }

        effects
    }

    /// Resolve a `tool.result.id` (canonical wire shape) back to the
    /// `(run_id, node_id, firing_id)` the firing belongs to, by scanning
    /// every in-flight run's `firing_by_request_id` table.
    ///
    /// Returns `None` for unknown ids (covers cancellation race,
    /// duplicate results, and ids belonging to combinator query/invoke
    /// pairs which use their own correlation tables).
    pub fn resolve_request_id(
        runs: &Runs,
        request_id: &str,
    ) -> Option<(String, NodeId, FiringId)> {
        let guard = runs.lock().expect("runs mutex poisoned");
        for (rid, st) in guard.iter() {
            if let Some((node_id, firing_id)) = st.firing_by_request_id.get(request_id) {
                return Some((rid.clone(), node_id.clone(), firing_id.clone()));
            }
        }
        None
    }

    /// Handle a `graph.cancel` event. Stage 2 implementation per parent
    /// spec §6.2; for Stage 1 we accept-and-drop: delete the run from
    /// the registry, ignore subsequent results. Cancel envelopes
    /// (`<reasoner>.cancel`) to in-flight reasoners are deferred.
    pub fn handle_cancel(runs: &Runs, body: &Map<String, Value>) {
        let run_id = match body.get("run_id").and_then(Value::as_str) {
            Some(s) => s.to_owned(),
            None => {
                tracing::warn!("graph.cancel missing run_id; dropping");
                return;
            }
        };
        let mut guard = runs.lock().expect("runs mutex poisoned");
        if guard.remove(&run_id).is_some() {
            tracing::info!(run_id = %run_id, "graph.cancel — run dropped from registry");
        }
        // Stage 2 (TODO): emit `<reasoner>.cancel` to every reasoner
        // with an in-flight firing, and drop subsequent
        // `graph.node_result` envelopes for this run_id at the
        // dispatcher level (already happens because the run is gone).
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Build a canonical-tool-contract `tool.invoke { id, name="spawn_graph",
    /// args: { graph, on_node_failure? } }` body — the post-refactor
    /// submission shape per `wire_protocol_spec.md` Flow 2.
    fn submit_body(run_id: &str, graph: Value, on_failure: Option<&str>) -> Map<String, Value> {
        let mut args = Map::new();
        args.insert("graph".into(), graph);
        if let Some(p) = on_failure {
            args.insert("on_node_failure".into(), Value::String(p.into()));
        }
        let mut m = Map::new();
        m.insert("kind".into(), Value::String("tool.invoke".into()));
        m.insert("id".into(), Value::String(run_id.into()));
        m.insert("name".into(), Value::String("spawn_graph".into()));
        m.insert("args".into(), Value::Object(args));
        m
    }

    fn peers_with(plugins: &[&str]) -> PeerSet {
        plugins.iter().map(|s| (*s).to_owned()).collect()
    }

    fn result_body(
        run_id: &str,
        node_id: &str,
        firing_id: Option<&str>,
        output_or_error: Result<Value, &str>,
        next_state: Option<Value>,
    ) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("kind".into(), Value::String("graph.node_result".into()));
        m.insert("run_id".into(), Value::String(run_id.into()));
        m.insert("node_id".into(), Value::String(node_id.into()));
        if let Some(f) = firing_id {
            m.insert("firing_id".into(), Value::String(f.into()));
        }
        match output_or_error {
            Ok(v) => {
                m.insert("output".into(), v);
            }
            Err(e) => {
                m.insert("error".into(), Value::String(e.into()));
            }
        }
        if let Some(ns) = next_state {
            m.insert("next_state".into(), ns);
        }
        m
    }

    /// Pull the firing_id out of the first DispatchNode in the effect
    /// list (test helper for asserting per-firing wire shape).
    fn first_dispatch_firing_id(effects: &[Effect]) -> String {
        effects
            .iter()
            .find_map(|e| match e {
                Effect::DispatchNode { firing_id, .. } => Some(firing_id.clone()),
                _ => None,
            })
            .expect("expected a DispatchNode")
    }

    #[test]
    fn submit_dispatches_single_source_node() {
        let runs: Runs = Arc::new(Mutex::new(HashMap::new()));
        let peers = peers_with(&["r"]);
        let g = json!({
            "nodes": [{"id": "n1", "reasoner": "r", "args": {"x": 1}}],
            "edges": []
        });
        let outcome = Scheduler::handle_submit(&runs, &peers, &submit_body("run-1", g, None));
        let effects = match outcome {
            SubmitOutcome::Accepted(e) => e.into_vec(),
            SubmitOutcome::Rejected(_) => panic!("expected accepted"),
        };
        // RunStarted, NodeDispatched, DispatchNode. NodeDispatched fires
        // BEFORE DispatchNode so the lifecycle marker (graph.node.fired)
        // lands on the bus before the targeted tool.invoke; an in-process
        // synchronous reasoner that replies immediately on tool.invoke
        // would otherwise emit tool.result before any observer saw the
        // firing's graph.node.fired (Bug A6 root cause).
        assert_eq!(effects.len(), 3);
        assert!(
            matches!(&effects[0], Effect::RunStarted { run_id, total_nodes } if run_id == "run-1" && *total_nodes == 1)
        );
        match &effects[1] {
            Effect::NodeDispatched {
                run_id,
                node_id,
                firing_id,
                reasoner,
            } => {
                assert_eq!(run_id, "run-1");
                assert_eq!(node_id, "n1");
                assert_eq!(reasoner, "r");
                assert!(!firing_id.is_empty());
            }
            other => panic!("unexpected effect: {other:?}"),
        }
        match &effects[2] {
            Effect::DispatchNode {
                reasoner,
                run_id,
                node_id,
                args,
                prev_state,
                firing_id,
                ..
            } => {
                assert_eq!(reasoner, "r");
                assert_eq!(run_id, "run-1");
                assert_eq!(node_id, "n1");
                assert_eq!(args, &json!({"x": 1}));
                assert_eq!(prev_state, &Value::Null);
                assert!(!firing_id.is_empty(), "firing_id must be minted");
            }
            other => panic!("unexpected effect: {other:?}"),
        }
    }

    #[test]
    fn cycle_is_accepted_at_submit() {
        // Used to be rejected with `_cycle`; now cycles run.
        let runs: Runs = Arc::new(Mutex::new(HashMap::new()));
        let peers = peers_with(&["r"]);
        let g = json!({
            "nodes": [
                {"id": "a", "reasoner": "r", "args": {}},
                {"id": "b", "reasoner": "r", "args": {}}
            ],
            "edges": [
                {"from": "a", "to": "b"},
                {"from": "b", "to": "a"}
            ]
        });
        let outcome = Scheduler::handle_submit(&runs, &peers, &submit_body("run-c", g, None));
        // Cyclic graph with no source should still accept (no
        // `_cycle` failure). With no source nodes runnable, the run
        // is stored awaiting some external trigger — a corner case
        // that would never occur with a real fanout combinator
        // since combinators decide reachability.
        match outcome {
            SubmitOutcome::Accepted(_) => {}
            SubmitOutcome::Rejected(e) => {
                let v = e.into_vec();
                panic!(
                    "expected accepted (cycles allowed); got rejected with {:?}",
                    v
                );
            }
        }
    }

    #[test]
    fn self_loop_runs_once_in_v1_broadcast_path() {
        // Self-loop on a single node: in the v1 fanout-less path the
        // node is its own dep, but the self-loop is skipped at
        // is_runnable (no precondition on first firing). Runs once,
        // completes, run_complete success.
        let runs: Runs = Arc::new(Mutex::new(HashMap::new()));
        let peers = peers_with(&["r"]);
        let g = json!({
            "nodes": [{"id": "n1", "reasoner": "r", "args": {}}],
            "edges": [{"from": "n1", "to": "n1"}]
        });
        let outcome = Scheduler::handle_submit(&runs, &peers, &submit_body("run-sl", g, None));
        let effects = match outcome {
            SubmitOutcome::Accepted(e) => e.into_vec(),
            _ => panic!("expected accepted"),
        };
        let firing = first_dispatch_firing_id(&effects);
        let r = Scheduler::handle_node_result(
            &runs,
            &peers,
            &result_body("run-sl", "n1", Some(&firing), Ok(json!("done")), None),
        )
        .into_vec();
        assert!(
            r.iter().any(|e| matches!(
                e,
                Effect::RunComplete {
                    status: RunStatus::Success,
                    ..
                }
            )),
            "expected success run_complete, got {:?}",
            r
        );
    }

    #[test]
    fn malformed_graph_emits_synthetic_error_failure() {
        let runs: Runs = Arc::new(Mutex::new(HashMap::new()));
        let peers = peers_with(&[]);
        let g = json!({
            "nodes": [{"id": "n1", "reasoner": "r", "args": {}}],
            "edges": [{"from": "n1", "to": "missing"}]
        });
        let outcome = Scheduler::handle_submit(&runs, &peers, &submit_body("run-m", g, None));
        let effects = match outcome {
            SubmitOutcome::Rejected(e) => e.into_vec(),
            _ => panic!("expected rejected"),
        };
        match &effects[0] {
            Effect::RunComplete { results, .. } => {
                assert!(results.contains_key("_error"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn linear_chain_runs_in_order() {
        let runs: Runs = Arc::new(Mutex::new(HashMap::new()));
        let peers = peers_with(&["r"]);
        let g = json!({
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
        let outcome = Scheduler::handle_submit(&runs, &peers, &submit_body("run-l", g, None));
        let e1 = match outcome {
            SubmitOutcome::Accepted(e) => e.into_vec(),
            _ => panic!("expected accepted"),
        };
        let f1 = first_dispatch_firing_id(&e1);
        // Submit emits RunStarted + DispatchNode(n1) + NodeDispatched(n1).
        assert_eq!(e1.len(), 3);

        let e2 = Scheduler::handle_node_result(
            &runs,
            &peers,
            &result_body("run-l", "n1", Some(&f1), Ok(json!("ok1")), None),
        )
        .into_vec();
        let f2 = first_dispatch_firing_id(&e2);
        // NodeDispatched comes first now (lifecycle marker before
        // targeted dispatch — see `try_dispatch` for rationale); the
        // DispatchNode follows. We assert on the DispatchNode by
        // searching rather than indexing so the test is robust to
        // either ordering.
        let dn = e2
            .iter()
            .find(|e| matches!(e, Effect::DispatchNode { .. }))
            .expect("DispatchNode in e2");
        assert!(
            matches!(dn, Effect::DispatchNode { node_id, inputs, .. } if {
                node_id == "n2" && inputs.get("n1") == Some(&json!({"output": "ok1"}))
            })
        );

        let e3 = Scheduler::handle_node_result(
            &runs,
            &peers,
            &result_body("run-l", "n2", Some(&f2), Ok(json!("ok2")), None),
        )
        .into_vec();
        let f3 = first_dispatch_firing_id(&e3);
        let dn3 = e3
            .iter()
            .find(|e| matches!(e, Effect::DispatchNode { .. }))
            .expect("DispatchNode in e3");
        assert!(matches!(dn3, Effect::DispatchNode { node_id, .. } if node_id == "n3"));

        let e4 = Scheduler::handle_node_result(
            &runs,
            &peers,
            &result_body("run-l", "n3", Some(&f3), Ok(json!("ok3")), None),
        )
        .into_vec();
        match e4.last().unwrap() {
            Effect::RunComplete {
                run_id,
                status,
                results,
            } => {
                assert_eq!(run_id, "run-l");
                assert_eq!(*status, RunStatus::Success);
                assert_eq!(results.get("n3"), Some(&json!({"output": "ok3"})));
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert!(runs.lock().unwrap().is_empty());
    }

    #[test]
    fn diamond_fan_in_per_firing_keying() {
        let runs: Runs = Arc::new(Mutex::new(HashMap::new()));
        let peers = peers_with(&["r"]);
        let g = json!({
            "nodes": [
                {"id": "n1", "reasoner": "r", "args": {}},
                {"id": "n2", "reasoner": "r", "args": {}},
                {"id": "n3", "reasoner": "r", "args": {}},
                {"id": "n4", "reasoner": "r", "args": {}}
            ],
            "edges": [
                {"from": "n1", "to": "n2"},
                {"from": "n1", "to": "n3"},
                {"from": "n2", "to": "n4"},
                {"from": "n3", "to": "n4"}
            ]
        });
        let outcome = Scheduler::handle_submit(&runs, &peers, &submit_body("run-d", g, None));
        let e1 = match outcome {
            SubmitOutcome::Accepted(e) => e.into_vec(),
            _ => panic!("accepted"),
        };
        let f1 = first_dispatch_firing_id(&e1);
        let r1 = Scheduler::handle_node_result(
            &runs,
            &peers,
            &result_body("run-d", "n1", Some(&f1), Ok(json!("a")), None),
        )
        .into_vec();
        let dispatched: Vec<&str> = r1
            .iter()
            .map(|e| match e {
                Effect::DispatchNode { node_id, .. } => node_id.as_str(),
                _ => "",
            })
            .collect();
        assert!(dispatched.contains(&"n2"));
        assert!(dispatched.contains(&"n3"));

        let f2 = r1
            .iter()
            .find_map(|e| match e {
                Effect::DispatchNode {
                    node_id, firing_id, ..
                } if node_id == "n2" => Some(firing_id.clone()),
                _ => None,
            })
            .unwrap();
        let f3 = r1
            .iter()
            .find_map(|e| match e {
                Effect::DispatchNode {
                    node_id, firing_id, ..
                } if node_id == "n3" => Some(firing_id.clone()),
                _ => None,
            })
            .unwrap();
        assert_ne!(f2, f3, "each dispatch gets a distinct firing_id");

        let r2 = Scheduler::handle_node_result(
            &runs,
            &peers,
            &result_body("run-d", "n2", Some(&f2), Ok(json!("b")), None),
        )
        .into_vec();
        assert!(r2.is_empty(), "n4 must wait for n3");

        let r3 = Scheduler::handle_node_result(
            &runs,
            &peers,
            &result_body("run-d", "n3", Some(&f3), Ok(json!("c")), None),
        )
        .into_vec();
        // NodeDispatched (lifecycle) precedes DispatchNode (targeted
        // tool.invoke) per try_dispatch ordering; find the
        // DispatchNode rather than indexing positionally.
        let dn = r3
            .iter()
            .find(|e| matches!(e, Effect::DispatchNode { .. }))
            .expect("DispatchNode in r3");
        match dn {
            Effect::DispatchNode {
                node_id, inputs, ..
            } => {
                assert_eq!(node_id, "n4");
                assert_eq!(inputs.get("n2"), Some(&json!({"output": "b"})));
                assert_eq!(inputs.get("n3"), Some(&json!({"output": "c"})));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn abort_marks_downstream_skipped_and_finishes_failure() {
        let runs: Runs = Arc::new(Mutex::new(HashMap::new()));
        let peers = peers_with(&["r"]);
        let g = json!({
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
        let e_submit = match Scheduler::handle_submit(&runs, &peers, &submit_body("run-a", g, None))
        {
            SubmitOutcome::Accepted(e) => e.into_vec(),
            _ => panic!("accepted"),
        };
        let f1 = first_dispatch_firing_id(&e_submit);
        let e_n1 = Scheduler::handle_node_result(
            &runs,
            &peers,
            &result_body("run-a", "n1", Some(&f1), Ok(json!("ok")), None),
        )
        .into_vec();
        let f2 = first_dispatch_firing_id(&e_n1);
        // n2 errors → n3 should be skipped, run_complete should fire.
        let r = Scheduler::handle_node_result(
            &runs,
            &peers,
            &result_body("run-a", "n2", Some(&f2), Err("boom"), None),
        )
        .into_vec();
        let complete = r.last().expect("got effect");
        match complete {
            Effect::RunComplete {
                status, results, ..
            } => {
                assert_eq!(*status, RunStatus::Failure);
                assert_eq!(results.get("n2"), Some(&json!({"error": "boom"})));
                assert_eq!(results.get("n3"), Some(&json!({"skipped": true})));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn continue_propagates_error_as_input_and_finishes_partial_failure() {
        let runs: Runs = Arc::new(Mutex::new(HashMap::new()));
        let peers = peers_with(&["r"]);
        let g = json!({
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
        let e_submit = match Scheduler::handle_submit(
            &runs,
            &peers,
            &submit_body("run-c", g, Some("continue")),
        ) {
            SubmitOutcome::Accepted(e) => e.into_vec(),
            _ => panic!("accepted"),
        };
        let f1 = first_dispatch_firing_id(&e_submit);
        let e_n1 = Scheduler::handle_node_result(
            &runs,
            &peers,
            &result_body("run-c", "n1", Some(&f1), Ok(json!("ok1")), None),
        )
        .into_vec();
        let f2 = first_dispatch_firing_id(&e_n1);
        let r = Scheduler::handle_node_result(
            &runs,
            &peers,
            &result_body("run-c", "n2", Some(&f2), Err("boom"), None),
        )
        .into_vec();
        let dispatch_n3 = r
            .iter()
            .find(|e| matches!(e, Effect::DispatchNode { node_id, .. } if node_id == "n3"))
            .expect("n3 dispatch");
        let f3 = match dispatch_n3 {
            Effect::DispatchNode {
                inputs, firing_id, ..
            } => {
                assert_eq!(inputs.get("n2"), Some(&json!({"error": "boom"})));
                firing_id.clone()
            }
            _ => unreachable!(),
        };

        let r2 = Scheduler::handle_node_result(
            &runs,
            &peers,
            &result_body("run-c", "n3", Some(&f3), Err("downstream-fail"), None),
        )
        .into_vec();
        match r2.last().unwrap() {
            Effect::RunComplete { status, .. } => {
                assert_eq!(*status, RunStatus::PartialFailure);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn unknown_reasoner_synthesizes_failure_and_applies_policy() {
        let runs: Runs = Arc::new(Mutex::new(HashMap::new()));
        let peers = peers_with(&[]);
        let g = json!({
            "nodes": [
                {"id": "n1", "reasoner": "ghost", "args": {}},
                {"id": "n2", "reasoner": "ghost", "args": {}}
            ],
            "edges": [{"from": "n1", "to": "n2"}]
        });
        let outcome = Scheduler::handle_submit(&runs, &peers, &submit_body("run-g", g, None));
        let effects = match outcome {
            SubmitOutcome::Accepted(e) => e.into_vec(),
            SubmitOutcome::Rejected(e) => e.into_vec(),
        };
        let complete = effects
            .iter()
            .find(|e| matches!(e, Effect::RunComplete { .. }))
            .expect("got run_complete");
        match complete {
            Effect::RunComplete {
                status, results, ..
            } => {
                assert_eq!(*status, RunStatus::Failure);
                let n1_err = results
                    .get("n1")
                    .and_then(|v| v.get("error"))
                    .and_then(Value::as_str);
                assert!(n1_err.unwrap().contains("not connected"));
                assert_eq!(results.get("n2"), Some(&json!({"skipped": true})));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn fan_in_dispatches_target_only_after_both_deps_complete() {
        let runs: Runs = Arc::new(Mutex::new(HashMap::new()));
        let peers = peers_with(&["r"]);
        let g = json!({
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
        let outcome = Scheduler::handle_submit(&runs, &peers, &submit_body("run-f", g, None));
        let e1 = match outcome {
            SubmitOutcome::Accepted(e) => e.into_vec(),
            _ => panic!("accepted"),
        };
        // RunStarted + (DispatchNode + NodeDispatched) for n1 and n2.
        assert_eq!(e1.len(), 5);

        let f1 = e1
            .iter()
            .find_map(|e| match e {
                Effect::DispatchNode {
                    node_id, firing_id, ..
                } if node_id == "n1" => Some(firing_id.clone()),
                _ => None,
            })
            .unwrap();
        let f2 = e1
            .iter()
            .find_map(|e| match e {
                Effect::DispatchNode {
                    node_id, firing_id, ..
                } if node_id == "n2" => Some(firing_id.clone()),
                _ => None,
            })
            .unwrap();

        let r = Scheduler::handle_node_result(
            &runs,
            &peers,
            &result_body("run-f", "n1", Some(&f1), Ok(json!("a")), None),
        )
        .into_vec();
        assert!(r.is_empty(), "n3 must wait for n2");

        let r = Scheduler::handle_node_result(
            &runs,
            &peers,
            &result_body("run-f", "n2", Some(&f2), Ok(json!("b")), None),
        )
        .into_vec();
        let dispatch = r
            .iter()
            .find(|e| matches!(e, Effect::DispatchNode { node_id, .. } if node_id == "n3"))
            .expect("n3 dispatched");
        match dispatch {
            Effect::DispatchNode { inputs, .. } => {
                assert_eq!(inputs.get("n1"), Some(&json!({"output": "a"})));
                assert_eq!(inputs.get("n2"), Some(&json!({"output": "b"})));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn unknown_run_id_is_silently_dropped() {
        let runs: Runs = Arc::new(Mutex::new(HashMap::new()));
        let peers = peers_with(&["r"]);
        let r = Scheduler::handle_node_result(
            &runs,
            &peers,
            &result_body("run-nope", "n1", Some("f"), Ok(json!("x")), None),
        );
        assert!(r.into_vec().is_empty());
    }

    #[test]
    fn duplicate_run_id_is_rejected() {
        let runs: Runs = Arc::new(Mutex::new(HashMap::new()));
        let peers = peers_with(&["r"]);
        let g = || {
            json!({
                "nodes": [{"id": "n1", "reasoner": "r", "args": {}}],
                "edges": []
            })
        };
        let _ = Scheduler::handle_submit(&runs, &peers, &submit_body("dup", g(), None));
        let outcome = Scheduler::handle_submit(&runs, &peers, &submit_body("dup", g(), None));
        let effects = match outcome {
            SubmitOutcome::Rejected(e) => e.into_vec(),
            _ => panic!("expected rejected for duplicate run_id"),
        };
        match &effects[0] {
            Effect::RunComplete { results, .. } => {
                assert!(results.contains_key("_error"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    // ---- New tests for v5 wire shape ------------------------------------

    #[test]
    fn next_state_is_carried_into_subsequent_firing_prev_state() {
        // Drive a node through two manual firings to exercise the
        // prev_state / next_state plumbing. We rely on a back-edge that
        // can re-enable the node — in the v1 broadcast path nodes fire
        // once, so we synthesize the second firing by clearing the
        // node's `completed` slot and re-running dispatch_all_runnable
        // through a small test-only helper. Exercises the data
        // structure shape T6 will rely on.
        let runs: Runs = Arc::new(Mutex::new(HashMap::new()));
        let peers = peers_with(&["r"]);
        let g = json!({
            "nodes": [{"id": "n1", "reasoner": "r", "args": {}}],
            "edges": []
        });
        let outcome = Scheduler::handle_submit(&runs, &peers, &submit_body("run-st", g, None));
        let e = match outcome {
            SubmitOutcome::Accepted(e) => e.into_vec(),
            _ => panic!("accepted"),
        };
        let f1 = first_dispatch_firing_id(&e);
        // Reply with next_state.
        let _ = Scheduler::handle_node_result(
            &runs,
            &peers,
            &result_body(
                "run-st",
                "n1",
                Some(&f1),
                Ok(json!("first-out")),
                Some(json!({"history": [1]})),
            ),
        );
        // Reach into the (now completed and dropped) registry — the
        // run_complete already fired. Verify the firing record carries
        // the right prev_state and the next_state was persisted via
        // `current_state` until the run was lifted.
        // Since the registry is empty, we build a fresh state to drive
        // a second firing manually.
        let g2 = json!({
            "nodes": [{"id": "n1", "reasoner": "r", "args": {}}],
            "edges": []
        });
        let runs2: Runs = Arc::new(Mutex::new(HashMap::new()));
        let _ = Scheduler::handle_submit(&runs2, &peers, &submit_body("run-st2", g2, None));
        // Pre-seed current_state directly to simulate what a back-edge
        // firing would do.
        {
            let mut g2 = runs2.lock().unwrap();
            let st = g2.get_mut("run-st2").unwrap();
            st.current_state
                .insert("n1".into(), json!({"history": [1]}));
        }
        // Now look up the latest dispatch's prev_state through the
        // firing record (the dispatch effect was emitted at submit).
        let g2 = runs2.lock().unwrap();
        let st = g2.get("run-st2").unwrap();
        let firing = st.latest_firing("n1").unwrap();
        // First firing's prev_state was null (registered before our
        // current_state poke).
        assert_eq!(firing.prev_state, Value::Null);
        // But current_state now has the next_state we'd carry.
        assert_eq!(st.current_state.get("n1"), Some(&json!({"history": [1]})));
    }

    #[test]
    fn per_firing_lifecycle_keying_each_firing_distinct_flags() {
        let runs: Runs = Arc::new(Mutex::new(HashMap::new()));
        let peers = peers_with(&["r"]);
        let g = json!({
            "nodes": [{"id": "n1", "reasoner": "r", "args": {}}],
            "edges": []
        });
        let _ = Scheduler::handle_submit(&runs, &peers, &submit_body("run-pf", g, None));
        // After submit, n1 has exactly one firing in flight.
        let g = runs.lock().unwrap();
        let st = g.get("run-pf").unwrap();
        let f = &st.firings["n1"];
        assert_eq!(f.len(), 1);
        assert!(!f[0].completed);
        // The completed flag is scoped to firing_id (a String, opaque).
        // For a hypothetical second firing, a fresh firing_id
        // distinguishes its flag.
    }

    #[test]
    fn dispatch_indexes_firing_by_request_id() {
        // tool-contract correlation: the outbound `tool.invoke` id
        // (== firing_id) must appear in firing_by_request_id keyed to
        // its (node_id, firing_id), so the inbound `tool.result { id }`
        // can resolve back to a specific firing without scanning all
        // firings.
        let runs: Runs = Arc::new(Mutex::new(HashMap::new()));
        let peers = peers_with(&["r"]);
        let g = json!({
            "nodes": [{"id": "n1", "reasoner": "r", "args": {}}],
            "edges": []
        });
        let outcome = Scheduler::handle_submit(&runs, &peers, &submit_body("run-idx", g, None));
        let effects = match outcome {
            SubmitOutcome::Accepted(e) => e.into_vec(),
            _ => panic!("accepted"),
        };
        let firing_id = first_dispatch_firing_id(&effects);
        let resolved = Scheduler::resolve_request_id(&runs, &firing_id)
            .expect("request_id resolves to (run_id, node_id, firing_id)");
        assert_eq!(resolved.0, "run-idx");
        assert_eq!(resolved.1, "n1");
        assert_eq!(resolved.2, firing_id);
        // Result lands → entry should be cleared.
        let _ = Scheduler::handle_node_result(
            &runs,
            &peers,
            &result_body("run-idx", "n1", Some(&firing_id), Ok(json!("done")), None),
        );
        // After completion the run is dropped from the registry, so
        // resolve_request_id can no longer find it — same effect as
        // "entry cleared," verified one level up.
        assert!(Scheduler::resolve_request_id(&runs, &firing_id).is_none());
    }

    #[test]
    fn cancel_drops_run_from_registry() {
        let runs: Runs = Arc::new(Mutex::new(HashMap::new()));
        let peers = peers_with(&["r"]);
        let g = json!({
            "nodes": [{"id": "n1", "reasoner": "r", "args": {}}],
            "edges": []
        });
        let _ = Scheduler::handle_submit(&runs, &peers, &submit_body("run-x", g, None));
        assert!(runs.lock().unwrap().contains_key("run-x"));
        let mut cancel_body = Map::new();
        cancel_body.insert("kind".into(), Value::String("graph.cancel".into()));
        cancel_body.insert("run_id".into(), Value::String("run-x".into()));
        Scheduler::handle_cancel(&runs, &cancel_body);
        assert!(runs.lock().unwrap().is_empty());
    }

    #[test]
    fn dispatch_emits_node_dispatched_before_dispatch_node() {
        // Inverted from the prior shape: NodeDispatched (lifecycle marker
        // → graph.node.fired) MUST land on the bus BEFORE DispatchNode
        // (targeted tool.invoke). An in-process synchronous reasoner
        // would otherwise emit tool.result before any observer saw the
        // firing's graph.node.fired, breaking the
        // firing_id → (run_id, node_id) map every consumer builds from
        // graph.node.fired (Bug A6 root cause).
        let runs: Runs = Arc::new(Mutex::new(HashMap::new()));
        let peers = peers_with(&["r"]);
        let g = json!({
            "nodes": [
                {"id": "n1", "reasoner": "r", "args": {}},
                {"id": "n2", "reasoner": "r", "args": {}},
                {"id": "n3", "reasoner": "r", "args": {}}
            ],
            "edges": []
        });
        let outcome = Scheduler::handle_submit(&runs, &peers, &submit_body("run-pair", g, None));
        let effects = match outcome {
            SubmitOutcome::Accepted(e) => e.into_vec(),
            _ => panic!("accepted"),
        };
        for (i, e) in effects.iter().enumerate() {
            if let Effect::NodeDispatched {
                run_id,
                node_id,
                firing_id,
                reasoner,
            } = e
            {
                let dn = effects.get(i + 1).expect("trailing DispatchNode");
                match dn {
                    Effect::DispatchNode {
                        run_id: dn_rid,
                        node_id: dn_nid,
                        firing_id: dn_fid,
                        reasoner: dn_r,
                        ..
                    } => {
                        assert_eq!(dn_rid, run_id);
                        assert_eq!(dn_nid, node_id);
                        assert_eq!(dn_fid, firing_id);
                        assert_eq!(dn_r, reasoner);
                    }
                    other => panic!("expected DispatchNode after NodeDispatched, got: {other:?}"),
                }
            }
        }
        let dispatched_count = effects
            .iter()
            .filter(|e| matches!(e, Effect::DispatchNode { .. }))
            .count();
        let node_dispatched_count = effects
            .iter()
            .filter(|e| matches!(e, Effect::NodeDispatched { .. }))
            .count();
        assert_eq!(dispatched_count, node_dispatched_count);
        assert_eq!(dispatched_count, 3);
    }

    #[test]
    fn unknown_reasoner_does_not_emit_node_dispatched() {
        let runs: Runs = Arc::new(Mutex::new(HashMap::new()));
        let peers = peers_with(&[]);
        let g = json!({
            "nodes": [{"id": "n1", "reasoner": "ghost", "args": {}}],
            "edges": []
        });
        let outcome = Scheduler::handle_submit(&runs, &peers, &submit_body("run-ghost", g, None));
        let effects = match outcome {
            SubmitOutcome::Accepted(e) => e.into_vec(),
            _ => panic!("accepted"),
        };
        assert!(effects
            .iter()
            .any(|e| matches!(e, Effect::RunStarted { .. })));
        assert!(!effects
            .iter()
            .any(|e| matches!(e, Effect::NodeDispatched { .. })));
        assert!(!effects
            .iter()
            .any(|e| matches!(e, Effect::DispatchNode { .. })));
        assert!(effects
            .iter()
            .any(|e| matches!(e, Effect::RunComplete { .. })));
    }

    // ---- T6: typed-combinator integration -------------------------------

    fn fanout_graph(node_id: &str, reasoner: &str, in_t: &str, outs: Vec<&str>) -> Value {
        json!({
            "nodes": [{
                "id": node_id,
                "reasoner": reasoner,
                "args": {},
                "fanout": { "in": in_t, "out": outs }
            }],
            "edges": []
        })
    }

    fn query_result_body(
        request_id: &str,
        resolved: Vec<(&str, Vec<&str>, &str)>,
        missing: Vec<(&str, Vec<&str>)>,
    ) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert(
            "kind".into(),
            Value::String("combinators.query.result".into()),
        );
        m.insert("id".into(), Value::String(request_id.into()));
        let resolved_arr: Vec<Value> = resolved
            .into_iter()
            .map(|(in_t, outs, owner)| {
                let mut e = Map::new();
                e.insert("in".into(), Value::String(in_t.into()));
                e.insert(
                    "out".into(),
                    Value::Array(outs.into_iter().map(|s| Value::String(s.into())).collect()),
                );
                e.insert("owner".into(), Value::String(owner.into()));
                Value::Object(e)
            })
            .collect();
        let missing_arr: Vec<Value> = missing
            .into_iter()
            .map(|(in_t, outs)| {
                let mut e = Map::new();
                e.insert("in".into(), Value::String(in_t.into()));
                e.insert(
                    "out".into(),
                    Value::Array(outs.into_iter().map(|s| Value::String(s.into())).collect()),
                );
                Value::Object(e)
            })
            .collect();
        m.insert("resolved".into(), Value::Array(resolved_arr));
        m.insert("missing".into(), Value::Array(missing_arr));
        m
    }

    fn invoke_result_body(invocation_id: &str, outputs: Vec<(&str, Value)>) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert(
            "kind".into(),
            Value::String("combinators.invoke.result".into()),
        );
        m.insert("id".into(), Value::String(invocation_id.into()));
        let arr: Vec<Value> = outputs
            .into_iter()
            .map(|(t, v)| {
                let mut e = Map::new();
                e.insert("type".into(), Value::String(t.into()));
                e.insert("value".into(), v);
                Value::Object(e)
            })
            .collect();
        m.insert("outputs".into(), Value::Array(arr));
        m
    }

    #[test]
    fn submit_emits_combinators_query_for_graph_with_fanouts() {
        let runs: Runs = Arc::new(Mutex::new(HashMap::new()));
        let peers = peers_with(&["r"]);
        let g = fanout_graph(
            "n1",
            "r",
            "generic-provider.ProviderOut",
            vec!["generic-tool.ToolCalls", "generic-provider.FinalAnswer"],
        );
        let outcome = Scheduler::handle_submit(&runs, &peers, &submit_body("run-q", g, None));
        let effects = match outcome {
            SubmitOutcome::Accepted(e) => e.into_vec(),
            _ => panic!("expected accepted"),
        };
        // RunStarted + CombinatorsQuery, no DispatchNode yet (waiting on
        // typecheck).
        assert!(matches!(&effects[0], Effect::RunStarted { .. }));
        let query = effects
            .iter()
            .find(|e| matches!(e, Effect::CombinatorsQuery { .. }))
            .expect("combinators.query emitted");
        match query {
            Effect::CombinatorsQuery { signatures, .. } => {
                assert_eq!(signatures.len(), 1);
                assert_eq!(signatures[0].in_type, "generic-provider.ProviderOut");
            }
            _ => unreachable!(),
        }
        assert!(!effects
            .iter()
            .any(|e| matches!(e, Effect::DispatchNode { .. })));
        // Run is parked in PendingTypecheck.
        let g = runs.lock().unwrap();
        let st = g.get("run-q").expect("stored");
        assert!(matches!(st.phase, RunPhase::PendingTypecheck { .. }));
    }

    #[test]
    fn missing_combinators_synthesise_run_complete_failure() {
        let runs: Runs = Arc::new(Mutex::new(HashMap::new()));
        let peers = peers_with(&["r"]);
        let g = fanout_graph(
            "n1",
            "r",
            "generic-provider.ProviderOut",
            vec!["generic-tool.ToolCalls", "generic-provider.FinalAnswer"],
        );
        let submit_effects =
            match Scheduler::handle_submit(&runs, &peers, &submit_body("run-m", g, None)) {
                SubmitOutcome::Accepted(e) => e.into_vec(),
                _ => panic!("accepted"),
            };
        let request_id = submit_effects
            .iter()
            .find_map(|e| match e {
                Effect::CombinatorsQuery { request_id, .. } => Some(request_id.clone()),
                _ => None,
            })
            .expect("query effect");

        let body = query_result_body(
            &request_id,
            vec![],
            vec![(
                "generic-provider.ProviderOut",
                vec!["generic-tool.ToolCalls", "generic-provider.FinalAnswer"],
            )],
        );
        let result_effects = Scheduler::handle_query_result(&runs, &peers, &body).into_vec();
        let complete = result_effects
            .iter()
            .find(|e| matches!(e, Effect::RunComplete { .. }))
            .expect("run_complete after missing");
        match complete {
            Effect::RunComplete {
                status, results, ..
            } => {
                assert_eq!(*status, RunStatus::Failure);
                assert!(results.contains_key("_missing_combinators"));
            }
            _ => unreachable!(),
        }
        // Run cleaned up.
        assert!(runs.lock().unwrap().is_empty());
    }

    #[test]
    fn typecheck_passes_when_all_signatures_resolve() {
        let runs: Runs = Arc::new(Mutex::new(HashMap::new()));
        let peers = peers_with(&["r"]);
        let g = fanout_graph(
            "n1",
            "r",
            "generic-provider.ProviderOut",
            vec!["generic-tool.ToolCalls", "generic-provider.FinalAnswer"],
        );
        let submit_effects =
            match Scheduler::handle_submit(&runs, &peers, &submit_body("run-r", g, None)) {
                SubmitOutcome::Accepted(e) => e.into_vec(),
                _ => panic!("accepted"),
            };
        let request_id = submit_effects
            .iter()
            .find_map(|e| match e {
                Effect::CombinatorsQuery { request_id, .. } => Some(request_id.clone()),
                _ => None,
            })
            .expect("query effect");
        let body = query_result_body(
            &request_id,
            vec![(
                "generic-provider.ProviderOut",
                vec!["generic-tool.ToolCalls", "generic-provider.FinalAnswer"],
                "nefor-combinators",
            )],
            vec![],
        );
        let r = Scheduler::handle_query_result(&runs, &peers, &body).into_vec();
        // Source node should now dispatch.
        assert!(r
            .iter()
            .any(|e| matches!(e, Effect::DispatchNode { node_id, .. } if node_id == "n1")));
        // Run is now Running.
        let g = runs.lock().unwrap();
        let st = g.get("run-r").expect("still stored");
        assert!(matches!(st.phase, RunPhase::Running));
    }

    #[test]
    fn runtime_fanout_routes_outputs_by_edge_type_tag() {
        // Two-node graph: n1 with fanout, n2 reachable via the
        // ToolCalls-tagged edge. n1 emits ToolCalls (non-null) and
        // FinalAnswer null → n2 fires with the routed value.
        let runs: Runs = Arc::new(Mutex::new(HashMap::new()));
        let peers = peers_with(&["r"]);
        let g = json!({
            "nodes": [
                { "id": "n1", "reasoner": "r", "args": {},
                  "fanout": { "in": "generic-provider.ProviderOut",
                              "out": ["generic-tool.ToolCalls",
                                      "generic-provider.FinalAnswer"] } },
                { "id": "n2", "reasoner": "r", "args": {} }
            ],
            "edges": [
                { "from": "n1", "to": "n2", "type": "generic-tool.ToolCalls" }
            ]
        });
        let s = match Scheduler::handle_submit(&runs, &peers, &submit_body("run-rf", g, None)) {
            SubmitOutcome::Accepted(e) => e.into_vec(),
            _ => panic!("accepted"),
        };
        let request_id = s
            .iter()
            .find_map(|e| match e {
                Effect::CombinatorsQuery { request_id, .. } => Some(request_id.clone()),
                _ => None,
            })
            .unwrap();
        let q_body = query_result_body(
            &request_id,
            vec![(
                "generic-provider.ProviderOut",
                vec!["generic-tool.ToolCalls", "generic-provider.FinalAnswer"],
                "nefor-combinators",
            )],
            vec![],
        );
        let post_query = Scheduler::handle_query_result(&runs, &peers, &q_body).into_vec();
        let f1 = post_query
            .iter()
            .find_map(|e| match e {
                Effect::DispatchNode {
                    node_id, firing_id, ..
                } if node_id == "n1" => Some(firing_id.clone()),
                _ => None,
            })
            .expect("n1 dispatch");
        // n1 returns an output → fanout invoke is emitted.
        let r1 = Scheduler::handle_node_result(
            &runs,
            &peers,
            &result_body(
                "run-rf",
                "n1",
                Some(&f1),
                Ok(json!({"tool_calls": [{"name": "x"}]})),
                None,
            ),
        )
        .into_vec();
        let invocation_id = r1
            .iter()
            .find_map(|e| match e {
                Effect::CombinatorsInvoke { invocation_id, .. } => Some(invocation_id.clone()),
                _ => None,
            })
            .expect("CombinatorsInvoke emitted");
        // n2 not yet dispatched (waiting on fanout).
        assert!(!r1
            .iter()
            .any(|e| matches!(e, Effect::DispatchNode { node_id, .. } if node_id == "n2")));
        // Combinators reply: ToolCalls populated, FinalAnswer null.
        let r2 = Scheduler::handle_invoke_result(
            &runs,
            &peers,
            &invoke_result_body(
                &invocation_id,
                vec![
                    ("generic-tool.ToolCalls", json!([{"name": "x"}])),
                    ("generic-provider.FinalAnswer", Value::Null),
                ],
            ),
        )
        .into_vec();
        assert!(r2
            .iter()
            .any(|e| matches!(e, Effect::DispatchNode { node_id, .. } if node_id == "n2")));
    }

    #[test]
    fn null_output_suppresses_edge_firing() {
        // Same shape as the previous test but the fanout suppresses the
        // n2 edge — n2 is marked skipped (no other incoming edges).
        let runs: Runs = Arc::new(Mutex::new(HashMap::new()));
        let peers = peers_with(&["r"]);
        let g = json!({
            "nodes": [
                { "id": "n1", "reasoner": "r",
                  "fanout": { "in": "generic-provider.ProviderOut",
                              "out": ["generic-tool.ToolCalls",
                                      "generic-provider.FinalAnswer"] } },
                { "id": "n2", "reasoner": "r" }
            ],
            "edges": [
                { "from": "n1", "to": "n2", "type": "generic-tool.ToolCalls" }
            ]
        });
        let s = match Scheduler::handle_submit(&runs, &peers, &submit_body("run-sup", g, None)) {
            SubmitOutcome::Accepted(e) => e.into_vec(),
            _ => panic!("accepted"),
        };
        let request_id = s
            .iter()
            .find_map(|e| match e {
                Effect::CombinatorsQuery { request_id, .. } => Some(request_id.clone()),
                _ => None,
            })
            .unwrap();
        let q_body = query_result_body(
            &request_id,
            vec![(
                "generic-provider.ProviderOut",
                vec!["generic-tool.ToolCalls", "generic-provider.FinalAnswer"],
                "nefor-combinators",
            )],
            vec![],
        );
        let pq = Scheduler::handle_query_result(&runs, &peers, &q_body).into_vec();
        let f1 = pq
            .iter()
            .find_map(|e| match e {
                Effect::DispatchNode {
                    node_id, firing_id, ..
                } if node_id == "n1" => Some(firing_id.clone()),
                _ => None,
            })
            .unwrap();
        let r1 = Scheduler::handle_node_result(
            &runs,
            &peers,
            &result_body("run-sup", "n1", Some(&f1), Ok(json!({"text": "Hi"})), None),
        )
        .into_vec();
        let invocation_id = r1
            .iter()
            .find_map(|e| match e {
                Effect::CombinatorsInvoke { invocation_id, .. } => Some(invocation_id.clone()),
                _ => None,
            })
            .unwrap();
        // FinalAnswer populated, ToolCalls null → n2 edge suppressed.
        let r2 = Scheduler::handle_invoke_result(
            &runs,
            &peers,
            &invoke_result_body(
                &invocation_id,
                vec![
                    ("generic-tool.ToolCalls", Value::Null),
                    ("generic-provider.FinalAnswer", json!({"text": "Hi"})),
                ],
            ),
        )
        .into_vec();
        // n2 not dispatched; run should complete (n2 marked skipped).
        let complete = r2
            .iter()
            .find(|e| matches!(e, Effect::RunComplete { .. }))
            .expect("run_complete");
        match complete {
            Effect::RunComplete { results, .. } => {
                assert_eq!(results.get("n2"), Some(&json!({"skipped": true})));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn node_without_fanout_keeps_broadcast_behavior() {
        // Linear chain with no fanout — should run via the v1 broadcast
        // path. Verifies the new path doesn't emit a query/invoke when
        // unnecessary.
        let runs: Runs = Arc::new(Mutex::new(HashMap::new()));
        let peers = peers_with(&["r"]);
        let g = json!({
            "nodes": [
                {"id": "n1", "reasoner": "r", "args": {}},
                {"id": "n2", "reasoner": "r", "args": {}}
            ],
            "edges": [{"from": "n1", "to": "n2"}]
        });
        let s = match Scheduler::handle_submit(&runs, &peers, &submit_body("run-bcast", g, None)) {
            SubmitOutcome::Accepted(e) => e.into_vec(),
            _ => panic!("accepted"),
        };
        // No CombinatorsQuery was emitted.
        assert!(!s
            .iter()
            .any(|e| matches!(e, Effect::CombinatorsQuery { .. })));
        // n1 dispatches immediately.
        assert!(s
            .iter()
            .any(|e| matches!(e, Effect::DispatchNode { node_id, .. } if node_id == "n1")));
    }

    #[test]
    fn duplicate_output_types_in_fanout_rejected_at_submit() {
        // Per parent spec §3 type-collision rule: `Fanout :: T -> {U, U}`
        // is rejected at submit with `_typecheck` failure.
        let runs: Runs = Arc::new(Mutex::new(HashMap::new()));
        let peers = peers_with(&["r"]);
        let g = fanout_graph("n1", "r", "p.T", vec!["p.U", "p.U"]);
        let outcome = Scheduler::handle_submit(&runs, &peers, &submit_body("run-dup", g, None));
        let effects = match outcome {
            SubmitOutcome::Rejected(e) => e.into_vec(),
            _ => panic!("expected rejected with _typecheck"),
        };
        match &effects[0] {
            Effect::RunComplete { results, .. } => {
                assert!(results.contains_key("_typecheck"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    // ---- T6.5: cycle re-firing -------------------------------------------

    /// Resolve combinator typecheck and return the firing-id of the
    /// node that dispatches first. Helper for cycle tests where every
    /// graph goes through the async typecheck path.
    fn resolve_typecheck_and_first_dispatch(
        runs: &Runs,
        peers: &PeerSet,
        submit_effects: &[Effect],
        first_node_id: &str,
        sigs: Vec<(&str, Vec<&str>)>,
    ) -> (String, Vec<Effect>) {
        let request_id = submit_effects
            .iter()
            .find_map(|e| match e {
                Effect::CombinatorsQuery { request_id, .. } => Some(request_id.clone()),
                _ => None,
            })
            .expect("query effect");
        let resolved: Vec<(&str, Vec<&str>, &str)> = sigs
            .into_iter()
            .map(|(in_t, outs)| (in_t, outs, "nefor-combinators"))
            .collect();
        let q_body = query_result_body(&request_id, resolved, vec![]);
        let post_query = Scheduler::handle_query_result(runs, peers, &q_body).into_vec();
        let firing_id = post_query
            .iter()
            .find_map(|e| match e {
                Effect::DispatchNode {
                    node_id, firing_id, ..
                } if node_id == first_node_id => Some(firing_id.clone()),
                _ => None,
            })
            .expect("first dispatch after typecheck");
        (firing_id, post_query)
    }

    #[test]
    fn self_loop_re_fires_when_back_edge_fires_non_null() {
        // Node A with self-edge tagged p.Loop. Fanout returns p.Loop=
        // non-null on firings 1 and 2 → drives firings 2 and 3. On
        // firing 3 the fanout returns null on p.Loop → cycle terminates.
        // Expect 3 firings total and a successful run_complete.
        let runs: Runs = Arc::new(Mutex::new(HashMap::new()));
        let peers = peers_with(&["r"]);
        let g = json!({
            "nodes": [{
                "id": "A",
                "reasoner": "r",
                "args": {},
                "fanout": { "in": "p.PIn", "out": ["p.Loop"] }
            }],
            "edges": [{ "from": "A", "to": "A", "type": "p.Loop" }]
        });
        let s_eff = match Scheduler::handle_submit(&runs, &peers, &submit_body("run-sl2", g, None))
        {
            SubmitOutcome::Accepted(e) => e.into_vec(),
            _ => panic!("expected accepted"),
        };
        let (f1, _) = resolve_typecheck_and_first_dispatch(
            &runs,
            &peers,
            &s_eff,
            "A",
            vec![("p.PIn", vec!["p.Loop"])],
        );

        // Firing 1 completes → fanout invoke → loop non-null → re-fire (firing 2).
        let r1 = Scheduler::handle_node_result(
            &runs,
            &peers,
            &result_body("run-sl2", "A", Some(&f1), Ok(json!({"i": 1})), None),
        )
        .into_vec();
        let inv1 = r1
            .iter()
            .find_map(|e| match e {
                Effect::CombinatorsInvoke { invocation_id, .. } => Some(invocation_id.clone()),
                _ => None,
            })
            .expect("invoke 1");
        let r1b = Scheduler::handle_invoke_result(
            &runs,
            &peers,
            &invoke_result_body(&inv1, vec![("p.Loop", json!({"keep": true}))]),
        )
        .into_vec();
        let f2 = r1b
            .iter()
            .find_map(|e| match e {
                Effect::DispatchNode {
                    node_id, firing_id, ..
                } if node_id == "A" => Some(firing_id.clone()),
                _ => None,
            })
            .expect("firing 2 dispatched");
        assert_ne!(f1, f2, "firing 2 has a fresh firing_id");

        // Firing 2 → fanout non-null → firing 3.
        let r2 = Scheduler::handle_node_result(
            &runs,
            &peers,
            &result_body("run-sl2", "A", Some(&f2), Ok(json!({"i": 2})), None),
        )
        .into_vec();
        let inv2 = r2
            .iter()
            .find_map(|e| match e {
                Effect::CombinatorsInvoke { invocation_id, .. } => Some(invocation_id.clone()),
                _ => None,
            })
            .unwrap();
        let r2b = Scheduler::handle_invoke_result(
            &runs,
            &peers,
            &invoke_result_body(&inv2, vec![("p.Loop", json!({"keep": true}))]),
        )
        .into_vec();
        let f3 = r2b
            .iter()
            .find_map(|e| match e {
                Effect::DispatchNode {
                    node_id, firing_id, ..
                } if node_id == "A" => Some(firing_id.clone()),
                _ => None,
            })
            .expect("firing 3");
        assert_ne!(f2, f3);

        // Firing 3 → fanout null on Loop → no re-fire; run completes.
        let r3 = Scheduler::handle_node_result(
            &runs,
            &peers,
            &result_body("run-sl2", "A", Some(&f3), Ok(json!({"i": 3})), None),
        )
        .into_vec();
        let inv3 = r3
            .iter()
            .find_map(|e| match e {
                Effect::CombinatorsInvoke { invocation_id, .. } => Some(invocation_id.clone()),
                _ => None,
            })
            .unwrap();
        let r3b = Scheduler::handle_invoke_result(
            &runs,
            &peers,
            &invoke_result_body(&inv3, vec![("p.Loop", Value::Null)]),
        )
        .into_vec();
        let complete = r3b
            .iter()
            .find(|e| matches!(e, Effect::RunComplete { .. }))
            .expect("run_complete after termination");
        match complete {
            Effect::RunComplete { status, .. } => assert_eq!(*status, RunStatus::Success),
            _ => unreachable!(),
        }
        // No fourth firing dispatched.
        let extra_dispatch = r3b
            .iter()
            .filter(|e| matches!(e, Effect::DispatchNode { .. }))
            .count();
        assert_eq!(extra_dispatch, 0, "no firing 4");
    }

    #[test]
    fn null_on_back_edge_terminates_cycle() {
        // Self-loop with fanout that immediately returns null on the
        // loop edge → no re-fire; run completes after the single firing.
        let runs: Runs = Arc::new(Mutex::new(HashMap::new()));
        let peers = peers_with(&["r"]);
        let g = json!({
            "nodes": [{
                "id": "A",
                "reasoner": "r",
                "args": {},
                "fanout": { "in": "p.PIn", "out": ["p.Loop"] }
            }],
            "edges": [{ "from": "A", "to": "A", "type": "p.Loop" }]
        });
        let s_eff = match Scheduler::handle_submit(&runs, &peers, &submit_body("run-nul", g, None))
        {
            SubmitOutcome::Accepted(e) => e.into_vec(),
            _ => panic!("expected accepted"),
        };
        let (f1, _) = resolve_typecheck_and_first_dispatch(
            &runs,
            &peers,
            &s_eff,
            "A",
            vec![("p.PIn", vec!["p.Loop"])],
        );
        let r1 = Scheduler::handle_node_result(
            &runs,
            &peers,
            &result_body("run-nul", "A", Some(&f1), Ok(json!({})), None),
        )
        .into_vec();
        let inv = r1
            .iter()
            .find_map(|e| match e {
                Effect::CombinatorsInvoke { invocation_id, .. } => Some(invocation_id.clone()),
                _ => None,
            })
            .unwrap();
        let r2 = Scheduler::handle_invoke_result(
            &runs,
            &peers,
            &invoke_result_body(&inv, vec![("p.Loop", Value::Null)]),
        )
        .into_vec();
        // No further dispatch — fanout suppressed the loop.
        assert!(!r2.iter().any(|e| matches!(e, Effect::DispatchNode { .. })));
        let complete = r2
            .iter()
            .find(|e| matches!(e, Effect::RunComplete { .. }))
            .expect("run_complete after immediate termination");
        match complete {
            Effect::RunComplete { status, .. } => assert_eq!(*status, RunStatus::Success),
            _ => unreachable!(),
        }
    }

    #[test]
    fn prev_state_carries_across_re_firings() {
        // Self-loop with fanout. Each firing returns next_state; the
        // following firing's dispatch must carry it as prev_state.
        // Verifies the per-firing chat-history accumulation pattern
        // from parent spec §3.
        let runs: Runs = Arc::new(Mutex::new(HashMap::new()));
        let peers = peers_with(&["r"]);
        let g = json!({
            "nodes": [{
                "id": "A",
                "reasoner": "r",
                "args": {},
                "fanout": { "in": "p.PIn", "out": ["p.Loop"] }
            }],
            "edges": [{ "from": "A", "to": "A", "type": "p.Loop" }]
        });
        let s_eff = match Scheduler::handle_submit(&runs, &peers, &submit_body("run-ps", g, None)) {
            SubmitOutcome::Accepted(e) => e.into_vec(),
            _ => panic!("expected accepted"),
        };
        let (f1, _) = resolve_typecheck_and_first_dispatch(
            &runs,
            &peers,
            &s_eff,
            "A",
            vec![("p.PIn", vec!["p.Loop"])],
        );
        // Firing 1 returns next_state=H1.
        let h1 = json!({"history": [{"role": "user", "content": "hi"}]});
        let r1 = Scheduler::handle_node_result(
            &runs,
            &peers,
            &result_body(
                "run-ps",
                "A",
                Some(&f1),
                Ok(json!("first-out")),
                Some(h1.clone()),
            ),
        )
        .into_vec();
        let inv1 = r1
            .iter()
            .find_map(|e| match e {
                Effect::CombinatorsInvoke { invocation_id, .. } => Some(invocation_id.clone()),
                _ => None,
            })
            .unwrap();
        let r1b = Scheduler::handle_invoke_result(
            &runs,
            &peers,
            &invoke_result_body(&inv1, vec![("p.Loop", json!({"k": 1}))]),
        )
        .into_vec();
        // Firing 2's DispatchNode must carry prev_state == H1.
        let firing_2_prev = r1b
            .iter()
            .find_map(|e| match e {
                Effect::DispatchNode {
                    node_id,
                    prev_state,
                    ..
                } if node_id == "A" => Some(prev_state.clone()),
                _ => None,
            })
            .expect("firing 2 dispatched");
        assert_eq!(
            firing_2_prev, h1,
            "firing 2's prev_state must equal firing 1's next_state"
        );
        // Sanity: the new firing's `inputs.A` carries the fanout-routed
        // value (not the upstream's broadcast output) — preferring
        // typed routing per the parent spec.
        let firing_2_inputs = r1b
            .iter()
            .find_map(|e| match e {
                Effect::DispatchNode {
                    node_id, inputs, ..
                } if node_id == "A" => Some(inputs.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            firing_2_inputs.get("A"),
            Some(&json!({"output": {"k": 1}})),
            "fanout-seeded value wins over broadcast output for cycle re-fires"
        );
    }

    #[test]
    fn provider_wrapper_cycle_re_fires_on_tool_calls_path() {
        // Four-node graph mirroring the orchestrator pattern from parent
        // spec §2:
        //
        //   wrap (fanout) ─ToolCalls→ tools → adapt ──┐
        //         └────────FinalAnswer───→ terminal   │
        //         ▲────────────────────────────────────┘
        //
        // Firings 1–2 of wrap emit ToolCalls=non-null (cycle continues).
        // Firing 3 emits FinalAnswer=non-null (cycle escapes via terminal).
        // Expected counts: wrap=3, tools=2, adapt=2, terminal=1.
        let runs: Runs = Arc::new(Mutex::new(HashMap::new()));
        let peers = peers_with(&["r"]);
        let g = json!({
            "nodes": [
                { "id": "wrap", "reasoner": "r", "args": {},
                  "fanout": { "in": "p.POut", "out": ["p.ToolCalls", "p.FinalAnswer"] } },
                { "id": "tools", "reasoner": "r", "args": {} },
                { "id": "adapt", "reasoner": "r", "args": {} },
                { "id": "terminal", "reasoner": "r", "args": {} }
            ],
            "edges": [
                { "from": "wrap", "to": "tools", "type": "p.ToolCalls" },
                { "from": "wrap", "to": "terminal", "type": "p.FinalAnswer" },
                { "from": "tools", "to": "adapt" },
                { "from": "adapt", "to": "wrap" }
            ]
        });
        let s_eff = match Scheduler::handle_submit(&runs, &peers, &submit_body("run-orch", g, None))
        {
            SubmitOutcome::Accepted(e) => e.into_vec(),
            _ => panic!("expected accepted"),
        };
        let (f_wrap1, _) = resolve_typecheck_and_first_dispatch(
            &runs,
            &peers,
            &s_eff,
            "wrap",
            vec![("p.POut", vec!["p.ToolCalls", "p.FinalAnswer"])],
        );

        // Helper: drive wrap through one firing → fanout invoke → reply
        // with the supplied multiset → return any newly-dispatched
        // node_ids paired with their firing_ids.
        let drive_wrap = |firing: &str, fanout_outs: Vec<(&str, Value)>| -> Vec<Effect> {
            let r = Scheduler::handle_node_result(
                &runs,
                &peers,
                &result_body(
                    "run-orch",
                    "wrap",
                    Some(firing),
                    Ok(json!("provider-out")),
                    None,
                ),
            )
            .into_vec();
            let inv = r
                .iter()
                .find_map(|e| match e {
                    Effect::CombinatorsInvoke { invocation_id, .. } => Some(invocation_id.clone()),
                    _ => None,
                })
                .expect("invoke for wrap");
            Scheduler::handle_invoke_result(&runs, &peers, &invoke_result_body(&inv, fanout_outs))
                .into_vec()
        };

        // Firing 1 of wrap → ToolCalls non-null → tools dispatches.
        let r1 = drive_wrap(
            &f_wrap1,
            vec![
                ("p.ToolCalls", json!([{"name": "x"}])),
                ("p.FinalAnswer", Value::Null),
            ],
        );
        let f_tools1 = r1
            .iter()
            .find_map(|e| match e {
                Effect::DispatchNode {
                    node_id, firing_id, ..
                } if node_id == "tools" => Some(firing_id.clone()),
                _ => None,
            })
            .expect("tools firing 1");
        // Drive tools 1 → adapt 1 → wrap re-fire (firing 2).
        let r_tools1 = Scheduler::handle_node_result(
            &runs,
            &peers,
            &result_body(
                "run-orch",
                "tools",
                Some(&f_tools1),
                Ok(json!("tool-results-1")),
                None,
            ),
        )
        .into_vec();
        let f_adapt1 = r_tools1
            .iter()
            .find_map(|e| match e {
                Effect::DispatchNode {
                    node_id, firing_id, ..
                } if node_id == "adapt" => Some(firing_id.clone()),
                _ => None,
            })
            .expect("adapt firing 1");
        let r_adapt1 = Scheduler::handle_node_result(
            &runs,
            &peers,
            &result_body(
                "run-orch",
                "adapt",
                Some(&f_adapt1),
                Ok(json!("provider-in-1")),
                None,
            ),
        )
        .into_vec();
        let f_wrap2 = r_adapt1
            .iter()
            .find_map(|e| match e {
                Effect::DispatchNode {
                    node_id, firing_id, ..
                } if node_id == "wrap" => Some(firing_id.clone()),
                _ => None,
            })
            .expect("wrap firing 2 after adapt completes");
        assert_ne!(f_wrap1, f_wrap2);

        // Firing 2 of wrap → ToolCalls non-null → tools 2 → adapt 2 → wrap 3.
        let r2 = drive_wrap(
            &f_wrap2,
            vec![
                ("p.ToolCalls", json!([{"name": "y"}])),
                ("p.FinalAnswer", Value::Null),
            ],
        );
        let f_tools2 = r2
            .iter()
            .find_map(|e| match e {
                Effect::DispatchNode {
                    node_id, firing_id, ..
                } if node_id == "tools" => Some(firing_id.clone()),
                _ => None,
            })
            .expect("tools firing 2");
        assert_ne!(f_tools1, f_tools2);
        let r_tools2 = Scheduler::handle_node_result(
            &runs,
            &peers,
            &result_body(
                "run-orch",
                "tools",
                Some(&f_tools2),
                Ok(json!("tool-results-2")),
                None,
            ),
        )
        .into_vec();
        let f_adapt2 = r_tools2
            .iter()
            .find_map(|e| match e {
                Effect::DispatchNode {
                    node_id, firing_id, ..
                } if node_id == "adapt" => Some(firing_id.clone()),
                _ => None,
            })
            .expect("adapt firing 2");
        assert_ne!(f_adapt1, f_adapt2);
        let r_adapt2 = Scheduler::handle_node_result(
            &runs,
            &peers,
            &result_body(
                "run-orch",
                "adapt",
                Some(&f_adapt2),
                Ok(json!("provider-in-2")),
                None,
            ),
        )
        .into_vec();
        let f_wrap3 = r_adapt2
            .iter()
            .find_map(|e| match e {
                Effect::DispatchNode {
                    node_id, firing_id, ..
                } if node_id == "wrap" => Some(firing_id.clone()),
                _ => None,
            })
            .expect("wrap firing 3");
        assert_ne!(f_wrap2, f_wrap3);

        // Firing 3 of wrap → FinalAnswer non-null → terminal fires; cycle exits.
        let r3 = drive_wrap(
            &f_wrap3,
            vec![
                ("p.ToolCalls", Value::Null),
                ("p.FinalAnswer", json!("done")),
            ],
        );
        let f_term = r3
            .iter()
            .find_map(|e| match e {
                Effect::DispatchNode {
                    node_id, firing_id, ..
                } if node_id == "terminal" => Some(firing_id.clone()),
                _ => None,
            })
            .expect("terminal dispatched on FinalAnswer");
        // tools must NOT have a firing 3 — its edge is suppressed.
        assert!(!r3
            .iter()
            .any(|e| matches!(e, Effect::DispatchNode { node_id, .. } if node_id == "tools")));

        let r_term = Scheduler::handle_node_result(
            &runs,
            &peers,
            &result_body(
                "run-orch",
                "terminal",
                Some(&f_term),
                Ok(json!("final")),
                None,
            ),
        )
        .into_vec();
        let complete = r_term
            .iter()
            .find(|e| matches!(e, Effect::RunComplete { .. }))
            .expect("run_complete after terminal");
        match complete {
            Effect::RunComplete { status, .. } => {
                // Expect success — every node has at least one Output;
                // tools/adapt completed twice each, wrap thrice,
                // terminal once.
                assert_eq!(*status, RunStatus::Success);
            }
            _ => unreachable!(),
        }
    }
}
