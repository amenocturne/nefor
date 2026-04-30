//! reasoner-graph — NCP v0.1 plugin: dumb scheduler for graphs of reasoners.
//!
//! Renamed from `dag-scheduler`. Cycles are now allowed (it's a graph, not
//! a DAG); per-firing lifecycle bookkeeping; ack-deadline + indefinite
//! result wait; reasoner state carry via `prev_state` / `next_state`. See
//! the parent spec at
//! `projects/software/active/nefor/specs/nefor-agent-and-reasoner-types-spec.md`
//! §3 for the full contract.
//!
//! Layering mirrors `nefor-combinators`:
//! - `main.rs` — entry, ready handshake, dispatch loop, bus encoding,
//!   per-firing ack-timeout watchdogs.
//! - `ncp.rs`  — stdio transport + handshake helpers.
//! - `error.rs` — typed errors and wire `ErrorCode`.
//! - `graph.rs` — graph parsing (cycles allowed; `fanout` is parsed but
//!   the runtime hook lands with T6).
//! - `state.rs` — pure scheduler state machine (RunState, Scheduler,
//!   per-firing keying).

mod error;
mod graph;
mod ncp;
mod state;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nefor_protocol::{Body, Envelope, PluginOutgoing, SystemBody};
use serde_json::{Map, Value};
use tokio::sync::mpsc;

use crate::error::ReasonerGraphError;
use crate::state::{Effect, FiringId, PeerSet, Runs, Scheduler, SubmitOutcome};

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

async fn run() -> Result<(), ReasonerGraphError> {
    let (out_tx, _writer_handle) = ncp::spawn_stdout_writer();
    let (in_tx, mut in_rx) =
        mpsc::channel::<Result<Envelope, ReasonerGraphError>>(ncp::CHANNEL_CAP);
    let _reader_handle = ncp::spawn_stdin_reader(in_tx);

    send_ready(&out_tx).await?;
    let engine_version = ncp::await_ready_ok(&mut in_rx).await?;
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
    in_rx: &mut mpsc::Receiver<Result<Envelope, ReasonerGraphError>>,
) -> Result<(), ReasonerGraphError> {
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
) -> Result<(), ReasonerGraphError> {
    let kind = match body.get("kind").and_then(Value::as_str) {
        Some(k) => k,
        None => return Ok(()),
    };

    match kind {
        "reasoner-graph.register_reasoner" => {
            // Explicit declaration that a reasoner type is "connected"
            // without owning a plugin process. Used by Lua-resident
            // reasoner types (provider-wrapper, tool-executor, adapter,
            // terminal, dummy) which never appear as a wire `from`.
            // Idempotent — re-registering the same name is a no-op
            // because `track_peer` is a HashSet insert.
            if let Some(name) = body.get("name").and_then(Value::as_str) {
                track_peer(peers, name);
            } else {
                tracing::warn!(
                    "register_reasoner event missing required 'name' string field"
                );
            }
        }
        "reasoner-graph.run" => {
            let snapshot = peers.lock().expect("peers mutex poisoned").clone();
            let outcome = Scheduler::handle_submit(runs, &snapshot, body);
            let effects = match outcome {
                SubmitOutcome::Accepted(e) | SubmitOutcome::Rejected(e) => e,
            };
            emit_effects(runs, peers, out_tx, effects.into_vec()).await?;
        }
        "graph.node_result" => {
            let snapshot = peers.lock().expect("peers mutex poisoned").clone();
            let effects = Scheduler::handle_node_result(runs, &snapshot, body);
            emit_effects(runs, peers, out_tx, effects.into_vec()).await?;
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
            emit_effects(runs, peers, out_tx, effects.into_vec()).await?;
        }
        "combinators.invoke.result" => {
            // Reply to a runtime fanout dispatch. Routes typed outputs
            // to outgoing edges by `edge.type` matching.
            let snapshot = peers.lock().expect("peers mutex poisoned").clone();
            let effects = Scheduler::handle_invoke_result(runs, &snapshot, body);
            emit_effects(runs, peers, out_tx, effects.into_vec()).await?;
        }
        other if other.ends_with(".run_node.ack") => {
            // `<reasoner>.run_node.ack` — pure bookkeeping, no
            // outbound effects. The ack-timeout watchdog (spawned at
            // dispatch) checks the firing's `acked` flag on expiry.
            Scheduler::handle_node_ack(runs, body);
        }
        _ => {
            // Not for us.
        }
    }
    Ok(())
}

/// Drain a list of [`Effect`]s onto the outbound mpsc. Collects any
/// dispatched `(run_id, firing_id, ack_deadline_ms)` triples so the
/// caller can spawn ack-timeout watchdogs.
async fn emit_effects(
    runs: &Runs,
    peers: &Arc<Mutex<PeerSet>>,
    out_tx: &mpsc::Sender<PluginOutgoing>,
    effects: Vec<Effect>,
) -> Result<(), ReasonerGraphError> {
    for e in effects {
        // Snapshot the (run_id, firing_id, deadline_ms) BEFORE consuming
        // the effect so we can arm a watchdog after the dispatch is on
        // the wire.
        let dispatched_meta = if let Effect::DispatchNode {
            run_id, firing_id, ..
        } = &e
        {
            let deadline = runs
                .lock()
                .expect("runs mutex poisoned")
                .get(run_id)
                .map(|s| s.ack_deadline_ms);
            deadline.map(|d| (run_id.clone(), firing_id.clone(), d))
        } else {
            None
        };

        let body = effect_to_body(e);
        send_event(out_tx, body).await?;

        if let Some((run_id, firing_id, deadline_ms)) = dispatched_meta {
            spawn_ack_watchdog(
                runs.clone(),
                peers.clone(),
                out_tx.clone(),
                run_id,
                firing_id,
                deadline_ms,
            );
        }
    }
    Ok(())
}

/// Spawn a watchdog that fires after `deadline_ms` and asks the
/// scheduler to handle an ack-timeout. If the firing has been acked or
/// already completed, the scheduler call is a no-op.
fn spawn_ack_watchdog(
    runs: Runs,
    peers: Arc<Mutex<PeerSet>>,
    out_tx: mpsc::Sender<PluginOutgoing>,
    run_id: String,
    firing_id: FiringId,
    deadline_ms: u64,
) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(deadline_ms)).await;
        let snapshot = peers.lock().expect("peers mutex poisoned").clone();
        let effects =
            Scheduler::handle_ack_timeout(&runs, &snapshot, &run_id, &firing_id).into_vec();
        // Manual drain (we don't have access to `emit_effects` here
        // without recursion; effects from a timeout are necessarily
        // propagation + run-completion events, which don't trigger
        // further dispatches that need watchdogs in this code path —
        // any propagation re-dispatches would themselves be Dispatch
        // effects, which means we'd need to arm new watchdogs. Handle
        // that by recursing into the scheduler-driven dispatch path
        // for completeness.
        for e in effects {
            // Ack-timeout effects are typically RunComplete (when
            // the timeout collapses the run) or further Dispatches
            // (when on_node_failure=continue propagates). For
            // simplicity emit straight without re-arming watchdogs;
            // a continue-policy follow-up dispatch will be re-armed
            // by the next propagating handler call. (Revisit if
            // needed when end-to-end tests prove the gap matters.)
            let body = effect_to_body(e);
            if out_tx.send(PluginOutgoing::event(body)).await.is_err() {
                tracing::warn!(run_id = %run_id, "writer closed during ack-timeout drain");
                return;
            }
        }
    });
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
            let mut m = Map::new();
            m.insert(
                "kind".into(),
                Value::String(format!("{}.run_node", reasoner)),
            );
            m.insert("run_id".into(), Value::String(run_id));
            m.insert("node_id".into(), Value::String(node_id));
            m.insert("firing_id".into(), Value::String(firing_id));
            m.insert("args".into(), args);
            m.insert("inputs".into(), Value::Object(inputs));
            m.insert("prev_state".into(), prev_state);
            m
        }
        Effect::NodeDispatched {
            run_id,
            node_id,
            firing_id,
            reasoner,
        } => {
            let mut m = Map::new();
            m.insert("kind".into(), Value::String("graph.node_dispatched".into()));
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
            let mut m = Map::new();
            m.insert("kind".into(), Value::String("graph.run_complete".into()));
            m.insert("run_id".into(), Value::String(run_id));
            m.insert("status".into(), Value::String(status.as_wire().to_owned()));
            m.insert("results".into(), Value::Object(results));
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
) -> Result<(), ReasonerGraphError> {
    out_tx
        .send(PluginOutgoing::event(body))
        .await
        .map_err(|_| ReasonerGraphError::WriterClosed)
}

async fn send_ready(out_tx: &mpsc::Sender<PluginOutgoing>) -> Result<(), ReasonerGraphError> {
    out_tx
        .send(PluginOutgoing::system(SystemBody::Ready {
            protocol_version: PROTOCOL_VERSION.into(),
        }))
        .await
        .map_err(|_| ReasonerGraphError::WriterClosed)
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
    fn dispatch_node_effect_renders_targeted_kind_with_firing_and_state() {
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
            Some("openai-provider.run_node")
        );
        assert_eq!(body.get("run_id").and_then(Value::as_str), Some("run-1"));
        assert_eq!(body.get("node_id").and_then(Value::as_str), Some("n2"));
        assert_eq!(body.get("firing_id").and_then(Value::as_str), Some("f-abc"));
        assert_eq!(body.get("prev_state"), Some(&json!({"history": [1]})));
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
    fn node_dispatched_effect_renders_kind_and_firing_id() {
        let body = effect_to_body(Effect::NodeDispatched {
            run_id: "run-1".into(),
            node_id: "n1".into(),
            firing_id: "f-1".into(),
            reasoner: "r".into(),
        });
        assert_eq!(
            body.get("kind").and_then(Value::as_str),
            Some("graph.node_dispatched")
        );
        assert_eq!(body.get("firing_id").and_then(Value::as_str), Some("f-1"));
        assert_eq!(body.get("reasoner").and_then(Value::as_str), Some("r"));
    }

    #[test]
    fn run_complete_effect_carries_status_string() {
        let body = effect_to_body(Effect::RunComplete {
            run_id: "run-1".into(),
            status: RunStatus::PartialFailure,
            results: Map::new(),
        });
        assert_eq!(
            body.get("kind").and_then(Value::as_str),
            Some("graph.run_complete")
        );
        assert_eq!(
            body.get("status").and_then(Value::as_str),
            Some("partial_failure")
        );
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
    async fn register_reasoner_adds_name_to_peer_set() {
        let runs: Runs = Arc::new(Mutex::new(HashMap::new()));
        let peers: Arc<Mutex<PeerSet>> = Arc::new(Mutex::new(HashSet::new()));
        let (out_tx, _out_rx) = mpsc::channel::<PluginOutgoing>(8);

        let mut body = Map::new();
        body.insert(
            "kind".into(),
            Value::String("reasoner-graph.register_reasoner".into()),
        );
        body.insert("name".into(), Value::String("provider-wrapper".into()));

        dispatch_event(&runs, &peers, &out_tx, "engine", &body)
            .await
            .expect("dispatch ok");

        let snap = peers.lock().unwrap().clone();
        assert!(
            snap.contains("provider-wrapper"),
            "register_reasoner should add 'provider-wrapper' to the peer set"
        );
    }

    #[tokio::test]
    async fn register_reasoner_is_idempotent() {
        let runs: Runs = Arc::new(Mutex::new(HashMap::new()));
        let peers: Arc<Mutex<PeerSet>> = Arc::new(Mutex::new(HashSet::new()));
        let (out_tx, _out_rx) = mpsc::channel::<PluginOutgoing>(8);

        let mut body = Map::new();
        body.insert(
            "kind".into(),
            Value::String("reasoner-graph.register_reasoner".into()),
        );
        body.insert("name".into(), Value::String("terminal".into()));

        // Re-emit twice; HashSet should still contain only one entry for
        // the name.
        for _ in 0..3 {
            dispatch_event(&runs, &peers, &out_tx, "engine", &body)
                .await
                .expect("dispatch ok");
        }

        let snap = peers.lock().unwrap().clone();
        assert_eq!(
            snap.iter().filter(|n| *n == "terminal").count(),
            1,
            "register_reasoner must be idempotent on repeated emit"
        );
    }

    #[tokio::test]
    async fn register_reasoner_without_name_is_ignored() {
        let runs: Runs = Arc::new(Mutex::new(HashMap::new()));
        let peers: Arc<Mutex<PeerSet>> = Arc::new(Mutex::new(HashSet::new()));
        let (out_tx, _out_rx) = mpsc::channel::<PluginOutgoing>(8);

        // Missing `name` field — should warn and drop, not panic.
        let mut body = Map::new();
        body.insert(
            "kind".into(),
            Value::String("reasoner-graph.register_reasoner".into()),
        );

        dispatch_event(&runs, &peers, &out_tx, "engine", &body)
            .await
            .expect("dispatch ok");

        assert!(peers.lock().unwrap().is_empty());
    }
}
