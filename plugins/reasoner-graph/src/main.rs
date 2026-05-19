//! reasoner-graph — NCP v0.1 plugin: dumb scheduler for graphs of reasoners.
//!
//! Renamed from `dag-scheduler`. Cycles are now allowed (it's a graph, not
//! a DAG); per-firing lifecycle bookkeeping; fire-and-forget dispatch
//! (cancellation via `graph.cancel`); reasoner state carry via
//! `prev_state` / `next_state`. See the parent spec at
//! `projects/software/active/nefor/specs/nefor-agent-and-reasoner-types-spec.md`
//! §3 for the full contract.
//!
//! Layering mirrors `nefor-combinators`:
//! - `main.rs` — entry, ready handshake, dispatch loop, bus encoding.
//! - `ncp.rs`  — stdio transport + handshake helpers.
//! - `error.rs` — typed errors.
//! - `graph.rs` — graph parsing (cycles allowed; `fanout` is parsed but
//!   the runtime hook lands with T6).
//! - `state.rs` — pure scheduler state machine (RunState, Scheduler,
//!   per-firing keying).

mod graph;
mod state;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use nefor_plugin_sdk::{spawn_stdin_reader, spawn_stdout_writer, await_ready_ok, TransportError};
use nefor_protocol::{Body, Envelope, PluginOutgoing, SystemBody};
use serde_json::{Map, Value};
use tokio::sync::mpsc;

const CHANNEL_CAP: usize = 256;
use crate::state::{Effect, PeerSet, Runs, Scheduler, SubmitOutcome};

/// NCP version this plugin speaks.
const PROTOCOL_VERSION: &str = "0.1";

/// Plugin name (matches engine's spawn-config identity).
const PLUGIN_NAME: &str = "reasoner-graph";

/// Plugin version, advertised in `reasoner-graph.hello`.
const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    if let Err(e) = run().await {
        tracing::error!(error = %e, "reasoner-graph exited with error");
        eprintln!("reasoner-graph: {e}");
        std::process::exit(1);
    }
    // Force exit: `tokio::io::stdin()` parks a non-cancellable blocking
    // reader thread; letting the runtime drop naturally would hang the
    // process. Same fix as mock-plugin / nefor-tui / nefor-combinators.
    std::process::exit(0);
}

async fn run() -> Result<(), TransportError> {
    let (out_tx, _writer_handle) = spawn_stdout_writer(CHANNEL_CAP);
    let (in_tx, mut in_rx) =
        mpsc::channel::<Result<Envelope, TransportError>>(CHANNEL_CAP);
    let _reader_handle = spawn_stdin_reader(in_tx);

    send_ready(&out_tx).await?;
    let engine_version = await_ready_ok(&mut in_rx).await?;
    tracing::info!(engine_version = %engine_version, "ready");

    send_event(&out_tx, hello_body()).await?;
    send_event(&out_tx, ready_body()).await?;

    let runs: Runs = Arc::new(Mutex::new(HashMap::new()));
    let peers: Arc<Mutex<PeerSet>> = Arc::new(Mutex::new(HashSet::new()));

    run_dispatch_loop(&runs, &peers, &out_tx, &mut in_rx).await?;

    let _ = out_tx.send(PluginOutgoing::event(goodbye_body())).await;
    Ok(())
}

async fn run_dispatch_loop(
    runs: &Runs,
    peers: &Arc<Mutex<PeerSet>>,
    out_tx: &mpsc::Sender<PluginOutgoing>,
    in_rx: &mut mpsc::Receiver<Result<Envelope, TransportError>>,
) -> Result<(), TransportError> {
    loop {
        tokio::select! {
            maybe = in_rx.recv() => {
                match maybe {
                    Some(Ok(env)) => {
                        match &env.body {
                            Body::System(SystemBody::Shutdown { .. }) => {
                                tracing::info!("shutdown received");
                                return Ok(());
                            }
                            Body::System(_) => {
                                tracing::warn!(?env, "unexpected system envelope after handshake");
                            }
                            Body::Event(map) => {
                                let sender = env.from.as_str().to_owned();
                                track_peer(peers, &sender);
                                dispatch_event(runs, peers, out_tx, &sender, map).await?;
                            }
                        }
                    }
                    Some(Err(e)) => {
                        tracing::error!(error = %e, "stdin parse error; dropping line");
                    }
                    None => {
                        tracing::info!("stdin closed; exiting");
                        return Ok(());
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("ctrl-c; exiting");
                return Ok(());
            }
        }
    }
}

/// Track every peer we hear from. The protocol crate doesn't yet expose
/// peer-presence events, so we approximate the connected-plugins set by
/// recording any `from` field we see.
fn track_peer(peers: &Arc<Mutex<PeerSet>>, sender: &str) {
    if sender == "engine" || sender == PLUGIN_NAME {
        return;
    }
    peers
        .lock()
        .expect("peers mutex poisoned")
        .insert(sender.to_owned());
}

async fn dispatch_event(
    runs: &Runs,
    peers: &Arc<Mutex<PeerSet>>,
    out_tx: &mpsc::Sender<PluginOutgoing>,
    _sender: &str,
    body: &Map<String, Value>,
) -> Result<(), TransportError> {
    let kind = match body.get("kind").and_then(Value::as_str) {
        Some(k) => k,
        None => return Ok(()),
    };

    match kind {
        "tool.invoke" => {
            // Canonical tool contract: only `name="spawn_graph"` is ours.
            // Anything else on the bus is targeted at a different tool
            // and we ignore it. (Routing-by-name is the substitute for
            // the old per-plugin envelope kinds.)
            let name = body.get("name").and_then(Value::as_str).unwrap_or("");
            if name != crate::graph::SPAWN_GRAPH_TOOL_NAME {
                return Ok(());
            }
            let snapshot = peers.lock().expect("peers mutex poisoned").clone();
            let outcome = Scheduler::handle_submit(runs, &snapshot, body);
            let effects = match outcome {
                SubmitOutcome::Accepted(e) | SubmitOutcome::Rejected(e) => e,
            };
            emit_effects(out_tx, effects.into_vec()).await?;
        }
        "tool.result" => {
            // Inbound node-firing reply on the canonical contract.
            // Resolve `id` → `(run_id, node_id, firing_id)` via the
            // per-RunState `firing_by_request_id` table, then synthesize
            // an in-process `graph.node_result`-shape body and feed it
            // to the existing scheduler handler. The synthesized body
            // never goes on the wire.
            let id = match body.get("id").and_then(Value::as_str) {
                Some(s) => s,
                None => {
                    tracing::warn!("tool.result missing `id`; dropping");
                    return Ok(());
                }
            };
            let resolved = Scheduler::resolve_request_id(runs, id);
            let (run_id, node_id, firing_id) = match resolved {
                Some(t) => t,
                None => {
                    // Either belongs to another tool (combinator
                    // queries/invokes use `combinators.*.result`, not
                    // `tool.result`) or the firing was already cancelled
                    // (via `graph.cancel`) or completed. Silent drop.
                    tracing::debug!(id = %id, "tool.result for unknown request_id; dropping");
                    return Ok(());
                }
            };
            let synthesized = synthesize_node_result(&run_id, &node_id, &firing_id, body);
            let snapshot = peers.lock().expect("peers mutex poisoned").clone();
            let effects = Scheduler::handle_node_result(runs, &snapshot, &synthesized);
            emit_effects(out_tx, effects.into_vec()).await?;
        }
        "graph.cancel" => {
            // Stage 1: accept-and-drop. The reserved kind is honored;
            // full cancel-fanout to in-flight reasoners lands in
            // Stage 2.
            Scheduler::handle_cancel(runs, body);
        }
        "combinators.query.result" => {
            // Reply to the submit-time typecheck/availability check.
            // Either resolves into normal dispatch or synthesises a
            // `_missing_combinators` failure.
            let snapshot = peers.lock().expect("peers mutex poisoned").clone();
            let effects = Scheduler::handle_query_result(runs, &snapshot, body);
            emit_effects(out_tx, effects.into_vec()).await?;
        }
        "combinators.invoke.result" => {
            // Reply to a runtime fanout dispatch. Routes typed outputs
            // to outgoing edges by `edge.type` matching.
            let snapshot = peers.lock().expect("peers mutex poisoned").clone();
            let effects = Scheduler::handle_invoke_result(runs, &snapshot, body);
            emit_effects(out_tx, effects.into_vec()).await?;
        }
        _ => {
            // Not for us.
        }
    }
    Ok(())
}

/// Translate an inbound `tool.result { id, result | error }` body into
/// the legacy `graph.node_result { run_id, node_id, firing_id, output |
/// error, next_state? }` shape consumed by [`Scheduler::handle_node_result`].
///
/// The legacy shape is internal — never goes back on the bus — and
/// keeps the scheduler's parsing untouched. Convention for the
/// `result` payload: tools may include `next_state` inside it; we
/// surface it at the synthesized body root because the scheduler
/// threads it across cycle re-firings.
fn synthesize_node_result(
    run_id: &str,
    node_id: &str,
    firing_id: &str,
    body: &Map<String, Value>,
) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("graph.node_result".into()));
    m.insert("run_id".into(), Value::String(run_id.to_owned()));
    m.insert("node_id".into(), Value::String(node_id.to_owned()));
    m.insert("firing_id".into(), Value::String(firing_id.to_owned()));
    if let Some(err) = body.get("error") {
        m.insert("error".into(), err.clone());
    } else if let Some(result) = body.get("result") {
        m.insert("output".into(), result.clone());
        // Tools that thread state across firings put `next_state`
        // inside `result`. Surface it at the synthesized root so the
        // scheduler picks it up as `current_state` for the next firing.
        if let Some(ns) = result.as_object().and_then(|o| o.get("next_state")) {
            m.insert("next_state".into(), ns.clone());
        }
    }
    m
}

/// Drain a list of [`Effect`]s onto the outbound mpsc.
///
/// Pure fire-and-forget: dispatches are emitted and that's it. If a
/// reasoner never replies with `tool.result`, the firing stays open
/// indefinitely; cancellation goes through `graph.cancel`.
async fn emit_effects(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    effects: Vec<Effect>,
) -> Result<(), TransportError> {
    for e in effects {
        let body = effect_to_body(e);
        send_event(out_tx, body).await?;
    }
    Ok(())
}

fn effect_to_body(effect: Effect) -> Map<String, Value> {
    match effect {
        Effect::RunStarted {
            run_id,
            total_nodes,
        } => {
            let mut m = Map::new();
            m.insert("kind".into(), Value::String("graph.run_started".into()));
            m.insert("run_id".into(), Value::String(run_id));
            m.insert("total_nodes".into(), Value::Number(total_nodes.into()));
            m
        }
        Effect::DispatchNode {
            reasoner,
            run_id,
            node_id,
            firing_id,
            args,
            inputs,
            prev_state,
        } => {
            // Canonical tool contract: dispatch a node by issuing a
            // `tool.invoke { id=firing_id, name=<reasoner>, args }`.
            // `args` carries `{ run_id, node_id, args, inputs, prev_state }`
            // per spec D2 — `firing_id` is implicit in the envelope `id`
            // and not duplicated.
            let mut tool_args = Map::new();
            tool_args.insert("run_id".into(), Value::String(run_id));
            tool_args.insert("node_id".into(), Value::String(node_id));
            tool_args.insert("args".into(), args);
            tool_args.insert("inputs".into(), Value::Object(inputs));
            tool_args.insert("prev_state".into(), prev_state);
            let mut m = Map::new();
            m.insert("kind".into(), Value::String("tool.invoke".into()));
            m.insert("id".into(), Value::String(firing_id));
            m.insert("name".into(), Value::String(reasoner));
            m.insert("args".into(), Value::Object(tool_args));
            m
        }
        Effect::NodeDispatched {
            run_id,
            node_id,
            firing_id,
            reasoner,
        } => {
            // Paired observer envelope alongside each `tool.invoke` —
            // pure observability for agentic-loop's per-node stream
            // filtering (D-26 in agentic_workflow_map.md). No
            // correctness depends on it. Spec D3.
            let mut m = Map::new();
            m.insert("kind".into(), Value::String("graph.node.fired".into()));
            m.insert("run_id".into(), Value::String(run_id));
            m.insert("node_id".into(), Value::String(node_id));
            m.insert("firing_id".into(), Value::String(firing_id));
            m.insert("reasoner".into(), Value::String(reasoner));
            m
        }
        Effect::RunComplete {
            run_id,
            status,
            results,
        } => {
            // Canonical tool contract: close the spawn_graph invocation
            // with `tool.result { id=run_id, result: { status, results } }`.
            // The result body carries today's `graph.run_complete` shape
            // verbatim per coordination point 4 in wire_protocol_spec.md.
            let mut result_body = Map::new();
            result_body.insert("status".into(), Value::String(status.as_wire().to_owned()));
            result_body.insert("results".into(), Value::Object(results));
            let mut m = Map::new();
            m.insert("kind".into(), Value::String("tool.result".into()));
            m.insert("id".into(), Value::String(run_id));
            m.insert("result".into(), Value::Object(result_body));
            m
        }
        Effect::CombinatorsQuery {
            request_id,
            signatures,
        } => {
            let mut m = Map::new();
            m.insert("kind".into(), Value::String("combinators.query".into()));
            m.insert("id".into(), Value::String(request_id));
            let arr: Vec<Value> = signatures
                .into_iter()
                .map(|sig| {
                    let mut e = Map::new();
                    e.insert("in".into(), Value::String(sig.in_type));
                    e.insert(
                        "out".into(),
                        Value::Array(sig.out_multiset.into_iter().map(Value::String).collect()),
                    );
                    Value::Object(e)
                })
                .collect();
            m.insert("signatures".into(), Value::Array(arr));
            m
        }
        Effect::CombinatorsInvoke {
            invocation_id,
            signature,
            input,
        } => {
            let mut m = Map::new();
            m.insert("kind".into(), Value::String("combinators.invoke".into()));
            m.insert("id".into(), Value::String(invocation_id));
            let mut sig = Map::new();
            sig.insert("in".into(), Value::String(signature.in_type));
            sig.insert(
                "out".into(),
                Value::Array(
                    signature
                        .out_multiset
                        .into_iter()
                        .map(Value::String)
                        .collect(),
                ),
            );
            m.insert("signature".into(), Value::Object(sig));
            m.insert("input".into(), input);
            m
        }
    }
}

// ---- static body constructors ----------------------------------------------

fn hello_body() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("reasoner-graph.hello".into()));
    m.insert("version".into(), Value::String(PLUGIN_VERSION.into()));
    m
}

fn ready_body() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("reasoner-graph.ready".into()));
    m
}

fn goodbye_body() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String("reasoner-graph.goodbye".into()),
    );
    m.insert("reason".into(), Value::String("stream closed".into()));
    m
}

async fn send_event(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: Map<String, Value>,
) -> Result<(), TransportError> {
    out_tx
        .send(PluginOutgoing::event(body))
        .await
        .map_err(|_| TransportError::WriterClosed)
}

async fn send_ready(out_tx: &mpsc::Sender<PluginOutgoing>) -> Result<(), TransportError> {
    out_tx
        .send(PluginOutgoing::system(SystemBody::Ready {
            protocol_version: PROTOCOL_VERSION.into(),
        }))
        .await
        .map_err(|_| TransportError::WriterClosed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::RunStatus;
    use serde_json::json;

    #[test]
    fn hello_body_advertises_plugin_version() {
        let b = hello_body();
        assert_eq!(
            b.get("kind").and_then(Value::as_str),
            Some("reasoner-graph.hello")
        );
        assert_eq!(
            b.get("version").and_then(Value::as_str),
            Some(PLUGIN_VERSION)
        );
    }

    #[test]
    fn ready_body_is_kind_only() {
        let b = ready_body();
        assert_eq!(
            b.get("kind").and_then(Value::as_str),
            Some("reasoner-graph.ready")
        );
        assert_eq!(b.len(), 1);
    }

    #[test]
    fn goodbye_body_carries_reason() {
        let b = goodbye_body();
        assert_eq!(
            b.get("kind").and_then(Value::as_str),
            Some("reasoner-graph.goodbye")
        );
        assert!(b.get("reason").and_then(Value::as_str).is_some());
    }

    #[test]
    fn dispatch_node_effect_renders_tool_invoke_with_args_payload() {
        // Canonical tool contract: dispatch is a `tool.invoke { id=firing_id,
        // name=<reasoner>, args: { run_id, node_id, args, inputs, prev_state } }`.
        // firing_id is the envelope id, not duplicated inside args (spec D2).
        let mut inputs = Map::new();
        inputs.insert("n1".into(), json!({"output": "x"}));
        let body = effect_to_body(Effect::DispatchNode {
            reasoner: "openai-provider".into(),
            run_id: "run-1".into(),
            node_id: "n2".into(),
            firing_id: "f-abc".into(),
            args: json!({"prompt": "hi"}),
            inputs,
            prev_state: json!({"history": [1]}),
        });
        assert_eq!(
            body.get("kind").and_then(Value::as_str),
            Some("tool.invoke")
        );
        assert_eq!(body.get("id").and_then(Value::as_str), Some("f-abc"));
        assert_eq!(
            body.get("name").and_then(Value::as_str),
            Some("openai-provider")
        );
        let args = body.get("args").and_then(Value::as_object).expect("args");
        assert_eq!(args.get("run_id").and_then(Value::as_str), Some("run-1"));
        assert_eq!(args.get("node_id").and_then(Value::as_str), Some("n2"));
        assert_eq!(args.get("args"), Some(&json!({"prompt": "hi"})));
        assert_eq!(args.get("prev_state"), Some(&json!({"history": [1]})));
        // firing_id MUST NOT be duplicated inside args — it lives only
        // on the envelope id.
        assert!(args.get("firing_id").is_none());
    }

    #[test]
    fn run_started_effect_renders_kind_and_total_nodes() {
        let body = effect_to_body(Effect::RunStarted {
            run_id: "run-1".into(),
            total_nodes: 3,
        });
        assert_eq!(
            body.get("kind").and_then(Value::as_str),
            Some("graph.run_started")
        );
        assert_eq!(body.get("run_id").and_then(Value::as_str), Some("run-1"));
        assert_eq!(body.get("total_nodes").and_then(Value::as_u64), Some(3));
    }

    #[test]
    fn node_dispatched_effect_renders_graph_node_fired_observer() {
        // Paired observer alongside each tool.invoke (spec D3).
        let body = effect_to_body(Effect::NodeDispatched {
            run_id: "run-1".into(),
            node_id: "n1".into(),
            firing_id: "f-1".into(),
            reasoner: "r".into(),
        });
        assert_eq!(
            body.get("kind").and_then(Value::as_str),
            Some("graph.node.fired")
        );
        assert_eq!(body.get("run_id").and_then(Value::as_str), Some("run-1"));
        assert_eq!(body.get("node_id").and_then(Value::as_str), Some("n1"));
        assert_eq!(body.get("firing_id").and_then(Value::as_str), Some("f-1"));
        assert_eq!(body.get("reasoner").and_then(Value::as_str), Some("r"));
    }

    #[test]
    fn run_complete_effect_renders_tool_result_with_status_in_result() {
        // Canonical close: `tool.result { id=run_id, result: { status, results } }`.
        let body = effect_to_body(Effect::RunComplete {
            run_id: "run-1".into(),
            status: RunStatus::PartialFailure,
            results: Map::new(),
        });
        assert_eq!(
            body.get("kind").and_then(Value::as_str),
            Some("tool.result")
        );
        assert_eq!(body.get("id").and_then(Value::as_str), Some("run-1"));
        let result = body
            .get("result")
            .and_then(Value::as_object)
            .expect("result");
        assert_eq!(
            result.get("status").and_then(Value::as_str),
            Some("partial_failure")
        );
        assert!(result.get("results").is_some());
    }

    #[tokio::test]
    async fn track_peer_records_senders() {
        let peers: Arc<Mutex<PeerSet>> = Arc::new(Mutex::new(HashSet::new()));
        track_peer(&peers, "openai-provider");
        track_peer(&peers, "engine"); // ignored
        track_peer(&peers, "reasoner-graph"); // self ignored
        track_peer(&peers, "basic-tools");
        let snap = peers.lock().unwrap().clone();
        assert!(snap.contains("openai-provider"));
        assert!(snap.contains("basic-tools"));
        assert!(!snap.contains("engine"));
        assert!(!snap.contains("reasoner-graph"));
    }

    #[tokio::test]
    async fn dispatch_event_ignores_tool_invoke_for_other_names() {
        // tool.invoke envelopes targeting other tools must be a no-op.
        // Routing-by-name replaces the per-plugin envelope kinds.
        let runs: Runs = Arc::new(Mutex::new(HashMap::new()));
        let peers: Arc<Mutex<PeerSet>> = Arc::new(Mutex::new(HashSet::new()));
        let (out_tx, _out_rx) = mpsc::channel::<PluginOutgoing>(8);

        let mut body = Map::new();
        body.insert("kind".into(), Value::String("tool.invoke".into()));
        body.insert("id".into(), Value::String("call-1".into()));
        body.insert("name".into(), Value::String("read_file".into()));
        body.insert("args".into(), Value::Object(Map::new()));

        dispatch_event(&runs, &peers, &out_tx, "agentic-loop", &body)
            .await
            .expect("dispatch ok");

        // No run should have been registered.
        assert!(runs.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn dispatch_event_routes_spawn_graph_invocation() {
        // tool.invoke { name="spawn_graph" } should be parsed and the
        // run accepted (single-source-node graph dispatches immediately).
        let runs: Runs = Arc::new(Mutex::new(HashMap::new()));
        let peers: Arc<Mutex<PeerSet>> = Arc::new(Mutex::new(HashSet::new()));
        // Pre-seed the reasoner as a peer so try_dispatch doesn't
        // synthesize a "not connected" failure.
        peers.lock().unwrap().insert("r".into());
        let (out_tx, mut out_rx) = mpsc::channel::<PluginOutgoing>(16);

        let mut args = Map::new();
        args.insert(
            "graph".into(),
            json!({
                "nodes": [{"id": "n1", "reasoner": "r", "args": {}}],
                "edges": []
            }),
        );
        let mut body = Map::new();
        body.insert("kind".into(), Value::String("tool.invoke".into()));
        body.insert("id".into(), Value::String("run-1".into()));
        body.insert("name".into(), Value::String("spawn_graph".into()));
        body.insert("args".into(), Value::Object(args));

        dispatch_event(&runs, &peers, &out_tx, "agentic-loop", &body)
            .await
            .expect("dispatch ok");

        // Run is registered and the source node is in flight.
        assert!(runs.lock().unwrap().contains_key("run-1"));

        // Drain the writer channel and assert the canonical envelope
        // shapes we expect: graph.run_started, tool.invoke (the node
        // dispatch), graph.node.fired (the observer pair).
        let mut kinds: Vec<String> = Vec::new();
        for _ in 0..3 {
            let msg = out_rx.recv().await.expect("envelope");
            if let Body::Event(map) = &msg.body {
                if let Some(k) = map.get("kind").and_then(Value::as_str) {
                    kinds.push(k.to_owned());
                }
            }
        }
        assert!(kinds.iter().any(|k| k == "graph.run_started"));
        assert!(kinds.iter().any(|k| k == "tool.invoke"));
        assert!(kinds.iter().any(|k| k == "graph.node.fired"));
    }

    #[tokio::test]
    async fn dispatch_event_resolves_tool_result_into_node_result() {
        // tool.result { id, result } for an in-flight firing must
        // close the node and emit the run-completing tool.result.
        let runs: Runs = Arc::new(Mutex::new(HashMap::new()));
        let peers: Arc<Mutex<PeerSet>> = Arc::new(Mutex::new(HashSet::new()));
        peers.lock().unwrap().insert("r".into());
        let (out_tx, mut out_rx) = mpsc::channel::<PluginOutgoing>(16);

        // Submit a single-node spawn_graph.
        let mut args = Map::new();
        args.insert(
            "graph".into(),
            json!({
                "nodes": [{"id": "n1", "reasoner": "r", "args": {}}],
                "edges": []
            }),
        );
        let mut submit = Map::new();
        submit.insert("kind".into(), Value::String("tool.invoke".into()));
        submit.insert("id".into(), Value::String("run-tr".into()));
        submit.insert("name".into(), Value::String("spawn_graph".into()));
        submit.insert("args".into(), Value::Object(args));
        dispatch_event(&runs, &peers, &out_tx, "agentic-loop", &submit)
            .await
            .expect("dispatch ok");

        // Pull the firing_id off the dispatched tool.invoke envelope.
        let mut firing_id: Option<String> = None;
        for _ in 0..3 {
            let msg = out_rx.recv().await.expect("envelope");
            if let Body::Event(map) = &msg.body {
                if map.get("kind").and_then(Value::as_str) == Some("tool.invoke") {
                    firing_id = map.get("id").and_then(Value::as_str).map(ToOwned::to_owned);
                    break;
                }
            }
        }
        let firing_id = firing_id.expect("dispatched tool.invoke carries id");

        // Reply with a tool.result on that id.
        let mut result_body = Map::new();
        result_body.insert("kind".into(), Value::String("tool.result".into()));
        result_body.insert("id".into(), Value::String(firing_id));
        result_body.insert("result".into(), json!({"text": "hello"}));
        dispatch_event(&runs, &peers, &out_tx, "r", &result_body)
            .await
            .expect("dispatch ok");

        // Run should have completed and been removed from the registry.
        assert!(runs.lock().unwrap().is_empty());

        // Final outbound envelope should be tool.result for the run_id.
        let mut found_run_complete = false;
        while let Ok(msg) = out_rx.try_recv() {
            if let Body::Event(map) = &msg.body {
                if map.get("kind").and_then(Value::as_str) == Some("tool.result")
                    && map.get("id").and_then(Value::as_str) == Some("run-tr")
                {
                    found_run_complete = true;
                }
            }
        }
        assert!(
            found_run_complete,
            "expected tool.result {{ id=run-tr }} closing the spawn_graph invocation"
        );
    }

    #[tokio::test]
    async fn dispatch_event_drops_tool_result_for_unknown_id() {
        // tool.result for an id we don't track is a silent drop —
        // covers cancellation race + envelopes addressed to other tools.
        let runs: Runs = Arc::new(Mutex::new(HashMap::new()));
        let peers: Arc<Mutex<PeerSet>> = Arc::new(Mutex::new(HashSet::new()));
        let (out_tx, _out_rx) = mpsc::channel::<PluginOutgoing>(8);

        let mut body = Map::new();
        body.insert("kind".into(), Value::String("tool.result".into()));
        body.insert("id".into(), Value::String("ghost".into()));
        body.insert("result".into(), json!({}));
        dispatch_event(&runs, &peers, &out_tx, "r", &body)
            .await
            .expect("dispatch ok");
    }
}
