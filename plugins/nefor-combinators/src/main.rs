//! nefor-combinators — NCP v0.1 plugin: unified combinator registry +
//! signature query + runtime invoke (Stage 1 reshape per
//! `nefor-combinators-spec`).
//!
//! Wire surface:
//!
//! - `combinators.register` (extended) — plugins declare `Merge`, `Into`,
//!   `Fanout`, and `Equivalent` (sugar) trait implementations.
//! - `combinators.query` / `combinators.query.result` — scheduler asks
//!   "do these signatures all resolve?" at submit time.
//! - `combinators.invoke` / `combinators.invoke.result` — typed-multiset
//!   invocation; replaces `combinators.run` over time.
//! - `combinators.run` / `combinators.result` — Slice 1 legacy path, kept
//!   so mock-plugin's existing `Merge<Message>` callers don't break during
//!   migration (per spec §8 / D-15: short coexistence, then full cut).
//! - `combinators.error` — every failure mode carries a closed [`ErrorCode`].

mod dispatch;
mod error;
mod registry;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use nefor_plugin_sdk::{await_ready_ok, spawn_stdin_reader, spawn_stdout_writer, TransportError};
use nefor_protocol::{Body, Envelope, PluginOutgoing, SystemBody};
use serde_json::{Map, Value};
use tokio::sync::{mpsc, oneshot, Mutex};

const CHANNEL_CAP: usize = 256;

use crate::dispatch::{
    caller_error_body, caller_result_body, classify_reply_kind, handler_dispatch_body,
    invoke_dispatch_body, invoke_result_body, parse_invoke_body, parse_query_body, parse_run_body,
    parse_typed_outputs, query_result_body, validate_output_multiset, HandlerOutcome,
    HandlerReplyKind, InternalId, InvokeRequest, Op, QueryResolution, RunRequest, TypedOutput,
};
use crate::error::{CombinatorsError, ErrorCode};
use crate::registry::{
    parse_register_body, FullyQualifiedKind, FullyQualifiedType, Identity, Registry, TraitImpl,
    PASS_THROUGH_HANDLER, PASS_THROUGH_OWNER,
};

/// NCP version this plugin speaks.
const PROTOCOL_VERSION: &str = "0.1";

/// Plugin version, advertised in `combinators.hello`.
const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Handler timeout bound (v1: 30s, per spec).
const HANDLER_TIMEOUT: Duration = Duration::from_secs(30);

/// Pending-channel payload — covers single-output (legacy) and typed-multiset
/// (new) reply shapes.
type PendingOutcome = Result<HandlerOutcome, CombinatorsError>;

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
        tracing::error!(error = %e, "nefor-combinators exited with error");
        eprintln!("nefor-combinators: {e}");
        std::process::exit(1);
    }
    // Force exit: `tokio::io::stdin()` parks a non-cancellable blocking
    // reader thread; letting the runtime drop naturally would hang the
    // process and keep the engine's `child.wait()` pending. Same fix as
    // mock-plugin / nefor-tui.
    std::process::exit(0);
}

async fn run() -> Result<(), CombinatorsError> {
    let (out_tx, _writer_handle) = spawn_stdout_writer(CHANNEL_CAP);
    let (in_tx, mut in_rx) = mpsc::channel::<Result<Envelope, TransportError>>(CHANNEL_CAP);
    let _reader_handle = spawn_stdin_reader(in_tx);

    send_ready(&out_tx).await?;
    let engine_version = await_ready_ok(&mut in_rx).await?;
    tracing::info!(engine_version = %engine_version, "ready");

    send_event(&out_tx, hello_body()).await?;
    send_event(&out_tx, ready_body()).await?;

    let registry: Arc<Mutex<Registry>> = Arc::new(Mutex::new(Registry::new()));
    let pending: Arc<Mutex<HashMap<InternalId, PendingSlot>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Built-in registrations: tool_split and retry_split.
    install_builtin_tool_split(&registry).await;
    install_builtin_retry_split(&registry).await;

    run_dispatch_loop(&registry, &pending, &out_tx, &mut in_rx).await?;

    let _ = out_tx.send(PluginOutgoing::event(goodbye_body())).await;
    Ok(())
}

/// In-flight invocation entry. Both the Slice 1 legacy reader (which
/// expects a single `output` field) and the new `invoke` reader (which
/// expects an `outputs[]` multiset) feed back through the same oneshot
/// after we re-shape the owner's reply into a [`HandlerOutcome`].
struct PendingSlot {
    /// Outcome receiver — fulfilled by the dispatch loop once the owner
    /// replies (or by the timeout task on a no-reply).
    tx: oneshot::Sender<PendingOutcome>,
    /// Whether the owner's reply should be parsed as `output` (legacy) or
    /// `outputs[]` (multiset).
    expect_multi: bool,
}

async fn run_dispatch_loop(
    registry: &Arc<Mutex<Registry>>,
    pending: &Arc<Mutex<HashMap<InternalId, PendingSlot>>>,
    out_tx: &mpsc::Sender<PluginOutgoing>,
    in_rx: &mut mpsc::Receiver<Result<Envelope, TransportError>>,
) -> Result<(), CombinatorsError> {
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
                                dispatch_event(
                                    registry,
                                    pending,
                                    out_tx,
                                    &sender,
                                    map,
                                ).await?;
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

/// Route a bus event body based on its `kind`. Broadcast means every plugin
/// sees every event, so filtering is this plugin's job.
async fn dispatch_event(
    registry: &Arc<Mutex<Registry>>,
    pending: &Arc<Mutex<HashMap<InternalId, PendingSlot>>>,
    out_tx: &mpsc::Sender<PluginOutgoing>,
    sender: &str,
    body: &Map<String, Value>,
) -> Result<(), CombinatorsError> {
    let kind = match body.get("kind").and_then(Value::as_str) {
        Some(k) => k,
        None => return Ok(()),
    };

    // A handler reply for an in-flight dispatch of ours? Check the pending
    // map first — these syntactically look like `<plugin>.<bare>.result` /
    // `<plugin>.<bare>.error`, so the `id` match is what actually confirms
    // it's ours.
    if let Some(shape) = classify_reply_kind(kind) {
        if let Some(internal_id) = body.get("id").and_then(Value::as_str) {
            let slot = pending.lock().await.remove(internal_id);
            if let Some(slot) = slot {
                let outcome: PendingOutcome = match shape {
                    HandlerReplyKind::Result => {
                        if slot.expect_multi {
                            match parse_typed_outputs(body) {
                                Some(outputs) => Ok(HandlerOutcome::Multi(outputs)),
                                None => Err(CombinatorsError::Handler(
                                    "handler reply missing or malformed `outputs[]`".into(),
                                )),
                            }
                        } else {
                            match body.get("output").cloned() {
                                Some(v) => Ok(HandlerOutcome::Single(v)),
                                None => Err(CombinatorsError::Handler(
                                    "handler reply missing `output`".into(),
                                )),
                            }
                        }
                    }
                    HandlerReplyKind::Error => {
                        let msg = body
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("<no message>")
                            .to_owned();
                        Ok(HandlerOutcome::Error(msg))
                    }
                };
                let _ = slot.tx.send(outcome);
                return Ok(());
            }
        }
    }

    match kind {
        "combinators.register" => {
            handle_register(registry, out_tx, sender, body).await?;
        }
        "combinators.run" => {
            handle_run(registry, pending, out_tx, sender, body).await?;
        }
        "combinators.query" => {
            handle_query(registry, out_tx, sender, body).await?;
        }
        "combinators.invoke" => {
            handle_invoke(registry, pending, out_tx, sender, body).await?;
        }
        _ => {
            // Not for us.
        }
    }
    Ok(())
}

async fn handle_register(
    registry: &Arc<Mutex<Registry>>,
    out_tx: &mpsc::Sender<PluginOutgoing>,
    sender: &str,
    body: &Map<String, Value>,
) -> Result<(), CombinatorsError> {
    let parse_result = parse_register_body(sender, body);
    let install_result = match parse_result {
        Ok((declared, impls)) => {
            let mut guard = registry.lock().await;
            guard.install(sender, declared, impls)
        }
        Err(e) => Err(e),
    };

    if let Err(e) = install_result {
        let (code, msg) = match &e {
            CombinatorsError::RegisterRejected { code, message } => (*code, message.clone()),
            other => (ErrorCode::MalformedEntry, other.to_string()),
        };
        tracing::warn!(sender = sender, code = %code, message = %msg, "register rejected");
        send_event(out_tx, caller_error_body(sender, None, code, &msg)).await?;
    } else {
        tracing::info!(sender = sender, "register accepted");
    }
    Ok(())
}

async fn handle_run(
    registry: &Arc<Mutex<Registry>>,
    pending: &Arc<Mutex<HashMap<InternalId, PendingSlot>>>,
    out_tx: &mpsc::Sender<PluginOutgoing>,
    caller: &str,
    body: &Map<String, Value>,
) -> Result<(), CombinatorsError> {
    let req = match parse_run_body(body) {
        Ok(r) => r,
        Err(e) => {
            let (code, message, caller_id) = match &e {
                CombinatorsError::RunRejected { code, message } => {
                    let id = body
                        .get("id")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                    (*code, message.clone(), id)
                }
                other => (ErrorCode::MalformedEntry, other.to_string(), None),
            };
            tracing::warn!(caller = caller, code = %code, message = %message, "run rejected");
            send_event(
                out_tx,
                caller_error_body(caller, caller_id.as_deref(), code, &message),
            )
            .await?;
            return Ok(());
        }
    };

    // Stage 1: both Merge and Into route through the unified registry. Into
    // worked-stub from Slice 1 is gone — if a sender registered an Into,
    // we now actually dispatch.
    let identity = req.identity();
    let owned = {
        let guard = registry.lock().await;
        guard.lookup_or_pass_through(&identity)
    };
    let owned = match owned {
        Some(o) => o,
        None => {
            send_event(
                out_tx,
                caller_error_body(
                    caller,
                    Some(&req.caller_id),
                    ErrorCode::NoHandlerRegistered,
                    &format!(
                        "no handler for op `{}` on `{}`",
                        match req.op {
                            Op::Merge => "Merge",
                            Op::Into => "Into",
                        },
                        req.type_.to_wire()
                    ),
                ),
            )
            .await?;
            return Ok(());
        }
    };

    dispatch_run_via(req, owned, pending, out_tx, caller).await
}

/// Dispatch a legacy `combinators.run` invocation. Single-output reply
/// shape (`output` field) — kept for mock-plugin Slice 1.
async fn dispatch_run_via(
    req: RunRequest,
    owned: registry::OwnedHandler,
    pending: &Arc<Mutex<HashMap<InternalId, PendingSlot>>>,
    out_tx: &mpsc::Sender<PluginOutgoing>,
    caller: &str,
) -> Result<(), CombinatorsError> {
    // Synthesised handlers (pass_through, tool_split) are owned by us:
    // resolve in-process instead of going through the bus. For the legacy
    // path this only matters for pass_through; tool_split is arity-1 with
    // a multiset output, which `combinators.run` cannot express.
    if owned.owner == PASS_THROUGH_OWNER && owned.handler.bare == PASS_THROUGH_HANDLER {
        // Echo input verbatim (arity 1 only).
        let output = req.inputs.first().cloned().unwrap_or(Value::Null);
        send_event(out_tx, caller_result_body(caller, &req.caller_id, output)).await?;
        return Ok(());
    }

    let internal_id: InternalId = uuid::Uuid::new_v4().to_string();
    let (tx, rx) = oneshot::channel::<PendingOutcome>();
    pending.lock().await.insert(
        internal_id.clone(),
        PendingSlot {
            tx,
            expect_multi: false,
        },
    );

    let dispatch_body = handler_dispatch_body(&owned.handler, &internal_id, &req.inputs);
    send_event(out_tx, dispatch_body).await?;

    spawn_await_legacy_reply(
        caller.to_owned(),
        req.caller_id,
        internal_id,
        rx,
        pending.clone(),
        out_tx.clone(),
    );
    Ok(())
}

async fn handle_query(
    registry: &Arc<Mutex<Registry>>,
    out_tx: &mpsc::Sender<PluginOutgoing>,
    caller: &str,
    body: &Map<String, Value>,
) -> Result<(), CombinatorsError> {
    let req = match parse_query_body(body) {
        Ok(r) => r,
        Err(e) => {
            let (code, message, caller_id) = match &e {
                CombinatorsError::QueryRejected { code, message } => {
                    let id = body
                        .get("id")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                    (*code, message.clone(), id)
                }
                other => (ErrorCode::MalformedQuery, other.to_string(), None),
            };
            tracing::warn!(caller = caller, code = %code, message = %message, "query rejected");
            send_event(
                out_tx,
                caller_error_body(caller, caller_id.as_deref(), code, &message),
            )
            .await?;
            return Ok(());
        }
    };

    let resolutions: Vec<QueryResolution> = {
        let guard = registry.lock().await;
        req.signatures
            .iter()
            .map(|sig| {
                let id = Identity::new(sig.arity, sig.in_type.clone(), sig.out_multiset.clone());
                match guard.lookup_or_pass_through(&id) {
                    Some(owned) => QueryResolution::Resolved { owner: owned.owner },
                    None => QueryResolution::Missing,
                }
            })
            .collect()
    };
    let body_out = query_result_body(caller, &req.caller_id, &req.signatures, &resolutions);
    send_event(out_tx, body_out).await
}

async fn handle_invoke(
    registry: &Arc<Mutex<Registry>>,
    pending: &Arc<Mutex<HashMap<InternalId, PendingSlot>>>,
    out_tx: &mpsc::Sender<PluginOutgoing>,
    caller: &str,
    body: &Map<String, Value>,
) -> Result<(), CombinatorsError> {
    let req = match parse_invoke_body(body) {
        Ok(r) => r,
        Err(e) => {
            let (code, message, caller_id) = match &e {
                CombinatorsError::InvokeRejected { code, message } => {
                    let id = body
                        .get("id")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                    (*code, message.clone(), id)
                }
                other => (ErrorCode::MalformedEntry, other.to_string(), None),
            };
            tracing::warn!(caller = caller, code = %code, message = %message, "invoke rejected");
            send_event(
                out_tx,
                caller_error_body(caller, caller_id.as_deref(), code, &message),
            )
            .await?;
            return Ok(());
        }
    };

    let owned = {
        let guard = registry.lock().await;
        guard.lookup_or_pass_through(&req.identity)
    };
    let owned = match owned {
        Some(o) => o,
        None => {
            send_event(
                out_tx,
                caller_error_body(
                    caller,
                    Some(&req.caller_id),
                    ErrorCode::NoHandlerRegistered,
                    &format!(
                        "no handler for signature in=`{}` out={:?}",
                        req.identity.input_type.to_wire(),
                        req.identity
                            .output_multiset
                            .iter()
                            .map(|t| t.to_wire())
                            .collect::<Vec<_>>()
                    ),
                ),
            )
            .await?;
            return Ok(());
        }
    };

    // In-process synthesis: pass_through and tool_split are owned by us.
    if owned.owner == PASS_THROUGH_OWNER {
        let outputs = match owned.handler.bare.as_str() {
            PASS_THROUGH_HANDLER => synthesise_pass_through(&req),
            TOOL_SPLIT_HANDLER => match synthesise_tool_split(&req) {
                Ok(o) => o,
                Err(msg) => {
                    send_event(
                        out_tx,
                        caller_error_body(
                            caller,
                            Some(&req.caller_id),
                            ErrorCode::HandlerError,
                            &msg,
                        ),
                    )
                    .await?;
                    return Ok(());
                }
            },
            RETRY_SPLIT_HANDLER => match synthesise_retry_split(&req) {
                Ok(o) => o,
                Err(msg) => {
                    send_event(
                        out_tx,
                        caller_error_body(
                            caller,
                            Some(&req.caller_id),
                            ErrorCode::HandlerError,
                            &msg,
                        ),
                    )
                    .await?;
                    return Ok(());
                }
            },
            // Future built-ins land here; for now any other bare name owned
            // by us is a bug.
            other => {
                send_event(
                    out_tx,
                    caller_error_body(
                        caller,
                        Some(&req.caller_id),
                        ErrorCode::NoHandlerRegistered,
                        &format!("internal: unknown built-in handler `{other}`"),
                    ),
                )
                .await?;
                return Ok(());
            }
        };
        // Validate built-in output multiset before forwarding.
        if let Err(msg) = validate_output_multiset(&req.identity.output_multiset, &outputs) {
            send_event(
                out_tx,
                caller_error_body(
                    caller,
                    Some(&req.caller_id),
                    ErrorCode::HandlerOutputMismatch,
                    &msg,
                ),
            )
            .await?;
            return Ok(());
        }
        send_event(out_tx, invoke_result_body(caller, &req.caller_id, outputs)).await?;
        return Ok(());
    }

    // External owner: dispatch via the bus.
    let internal_id: InternalId = uuid::Uuid::new_v4().to_string();
    let (tx, rx) = oneshot::channel::<PendingOutcome>();
    pending.lock().await.insert(
        internal_id.clone(),
        PendingSlot {
            tx,
            expect_multi: true,
        },
    );

    let dispatch_body =
        invoke_dispatch_body(&owned.handler, &internal_id, &req.identity, &req.inputs);
    send_event(out_tx, dispatch_body).await?;

    spawn_await_invoke_reply(
        caller.to_owned(),
        req.caller_id,
        req.identity,
        internal_id,
        rx,
        pending.clone(),
        out_tx.clone(),
    );
    Ok(())
}

/// `pass_through :: T -> {T}` — echo the single input as the single output.
fn synthesise_pass_through(req: &InvokeRequest) -> Vec<TypedOutput> {
    let value = req.inputs.first().cloned().unwrap_or(Value::Null);
    let type_ = req.identity.input_type.clone();
    vec![TypedOutput { type_, value }]
}

// ---- Built-in `tool_split` -------------------------------------------------

/// Bare handler name for the `tool_split` built-in.
const TOOL_SPLIT_HANDLER: &str = "tool_split";

/// Plugin namespace owning `ProviderOut` and `FinalAnswer`.
const GENERIC_PROVIDER_NS: &str = "generic-provider";
/// Plugin namespace owning `ToolCalls`.
const GENERIC_TOOL_NS: &str = "generic-tool";
/// Bare type name: input to `tool_split`.
const PROVIDER_OUT_NAME: &str = "ProviderOut";
/// Bare type name: tool-execution branch output.
const TOOL_CALLS_NAME: &str = "ToolCalls";
/// Bare type name: terminal-text branch output.
const FINAL_ANSWER_NAME: &str = "FinalAnswer";

/// Install the built-in `tool_split` registration at startup.
///
/// Per parent spec §6.2: the signature is
/// `generic-provider.ProviderOut -> { generic-tool.ToolCalls,
/// generic-provider.FinalAnswer }`. None of these tags belong to the
/// combinators-plugin namespace, so we use [`Registry::install_builtin`]
/// which bypasses the wire-side namespace-ownership check. The handler
/// itself stays in this plugin's namespace
/// (`nefor-combinators.tool_split`) — that's where the implementation
/// runs.
async fn install_builtin_tool_split(registry: &Arc<Mutex<Registry>>) {
    let provider_out = FullyQualifiedType {
        plugin: GENERIC_PROVIDER_NS.to_owned(),
        name: PROVIDER_OUT_NAME.to_owned(),
    };
    let tool_calls = FullyQualifiedType {
        plugin: GENERIC_TOOL_NS.to_owned(),
        name: TOOL_CALLS_NAME.to_owned(),
    };
    let final_answer = FullyQualifiedType {
        plugin: GENERIC_PROVIDER_NS.to_owned(),
        name: FINAL_ANSWER_NAME.to_owned(),
    };
    let handler = FullyQualifiedKind {
        plugin: PASS_THROUGH_OWNER.to_owned(),
        bare: TOOL_SPLIT_HANDLER.to_owned(),
    };
    let mut guard = registry.lock().await;
    if let Err(e) = guard.install_builtin(
        PASS_THROUGH_OWNER,
        vec![TraitImpl::Fanout {
            in_: provider_out,
            outs: vec![tool_calls, final_answer],
            handler,
        }],
    ) {
        tracing::error!(error = %e, "failed to install built-in tool_split");
    }
}

/// `tool_split` runtime logic. Inspects the input value's JSON shape:
/// non-empty `tool_calls` array → emit ToolCalls slot; else → emit
/// FinalAnswer slot. The unselected slot carries `null` (Maybe semantics).
///
/// Type matching is exact on the canonical tags: input is
/// `generic-provider.ProviderOut`, outputs are `generic-tool.ToolCalls`
/// and `generic-provider.FinalAnswer`. (Suffix-matching the previous
/// placeholder-types code used was a smell flagged in T4's writeup; we
/// now compare against the real tags directly.)
fn synthesise_tool_split(req: &InvokeRequest) -> Result<Vec<TypedOutput>, String> {
    let input = req.inputs.first().cloned().unwrap_or(Value::Null);
    let mut tool_calls_type: Option<FullyQualifiedType> = None;
    let mut final_answer_type: Option<FullyQualifiedType> = None;
    for t in &req.identity.output_multiset {
        if t.plugin == GENERIC_TOOL_NS && t.name == TOOL_CALLS_NAME {
            tool_calls_type = Some(t.clone());
        } else if t.plugin == GENERIC_PROVIDER_NS && t.name == FINAL_ANSWER_NAME {
            final_answer_type = Some(t.clone());
        }
    }
    let tool_calls_type = tool_calls_type.ok_or_else(|| {
        format!("tool_split output multiset missing `{GENERIC_TOOL_NS}.{TOOL_CALLS_NAME}` slot")
    })?;
    let final_answer_type = final_answer_type.ok_or_else(|| {
        format!(
            "tool_split output multiset missing `{GENERIC_PROVIDER_NS}.{FINAL_ANSWER_NAME}` slot"
        )
    })?;

    let has_tool_calls = input
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|a| !a.is_empty())
        .unwrap_or(false);

    if has_tool_calls {
        let calls = input.get("tool_calls").cloned().unwrap_or(Value::Null);
        Ok(vec![
            TypedOutput {
                type_: tool_calls_type,
                value: calls,
            },
            TypedOutput {
                type_: final_answer_type,
                value: Value::Null,
            },
        ])
    } else {
        let text = input.get("text").cloned().unwrap_or_else(|| input.clone());
        let mut answer = Map::new();
        answer.insert("text".into(), text);
        Ok(vec![
            TypedOutput {
                type_: tool_calls_type,
                value: Value::Null,
            },
            TypedOutput {
                type_: final_answer_type,
                value: Value::Object(answer),
            },
        ])
    }
}

// ---- Built-in `retry_split` ------------------------------------------------

/// Bare handler name for the `retry_split` built-in.
const RETRY_SPLIT_HANDLER: &str = "retry_split";

const GENERIC_CONTROL_NS: &str = "generic-control";
const RETRY_DECISION_NAME: &str = "RetryDecision";
const RETRY_BRANCH_NAME: &str = "Retry";
const PASS_BRANCH_NAME: &str = "Pass";
const EXHAUSTED_BRANCH_NAME: &str = "Exhausted";

async fn install_builtin_retry_split(registry: &Arc<Mutex<Registry>>) {
    let decision = FullyQualifiedType {
        plugin: GENERIC_CONTROL_NS.to_owned(),
        name: RETRY_DECISION_NAME.to_owned(),
    };
    let retry = FullyQualifiedType {
        plugin: GENERIC_CONTROL_NS.to_owned(),
        name: RETRY_BRANCH_NAME.to_owned(),
    };
    let pass = FullyQualifiedType {
        plugin: GENERIC_CONTROL_NS.to_owned(),
        name: PASS_BRANCH_NAME.to_owned(),
    };
    let exhausted = FullyQualifiedType {
        plugin: GENERIC_CONTROL_NS.to_owned(),
        name: EXHAUSTED_BRANCH_NAME.to_owned(),
    };
    let handler = FullyQualifiedKind {
        plugin: PASS_THROUGH_OWNER.to_owned(),
        bare: RETRY_SPLIT_HANDLER.to_owned(),
    };
    let mut guard = registry.lock().await;
    if let Err(e) = guard.install_builtin(
        PASS_THROUGH_OWNER,
        vec![TraitImpl::Fanout {
            in_: decision,
            outs: vec![retry, pass, exhausted],
            handler,
        }],
    ) {
        tracing::error!(error = %e, "failed to install built-in retry_split");
    }
}

fn synthesise_retry_split(req: &InvokeRequest) -> Result<Vec<TypedOutput>, String> {
    let input = req.inputs.first().cloned().unwrap_or(Value::Null);
    let route = input
        .get("route")
        .and_then(Value::as_str)
        .ok_or_else(|| "retry_split input missing string `route`".to_owned())?;

    let branch_value = input
        .get("passthrough")
        .cloned()
        .or_else(|| input.get("input").cloned())
        .unwrap_or_else(|| input.clone());

    let mut retry_type = None;
    let mut pass_type = None;
    let mut exhausted_type = None;
    for t in &req.identity.output_multiset {
        if t.plugin == GENERIC_CONTROL_NS && t.name == RETRY_BRANCH_NAME {
            retry_type = Some(t.clone());
        } else if t.plugin == GENERIC_CONTROL_NS && t.name == PASS_BRANCH_NAME {
            pass_type = Some(t.clone());
        } else if t.plugin == GENERIC_CONTROL_NS && t.name == EXHAUSTED_BRANCH_NAME {
            exhausted_type = Some(t.clone());
        }
    }
    let retry_type =
        retry_type.ok_or_else(|| "retry_split output missing Retry slot".to_owned())?;
    let pass_type = pass_type.ok_or_else(|| "retry_split output missing Pass slot".to_owned())?;
    let exhausted_type =
        exhausted_type.ok_or_else(|| "retry_split output missing Exhausted slot".to_owned())?;

    let retry_value = if route == "retry" {
        branch_value.clone()
    } else {
        Value::Null
    };
    let pass_value = if route == "pass" {
        branch_value.clone()
    } else {
        Value::Null
    };
    let exhausted_value = if route == "exhausted" {
        branch_value
    } else {
        Value::Null
    };

    Ok(vec![
        TypedOutput {
            type_: retry_type,
            value: retry_value,
        },
        TypedOutput {
            type_: pass_type,
            value: pass_value,
        },
        TypedOutput {
            type_: exhausted_type,
            value: exhausted_value,
        },
    ])
}

// ---- Reply forwarding -----------------------------------------------------

/// Spawn a task that waits for a legacy `combinators.run` reply (single
/// `output`) and forwards `combinators.result` / `combinators.error` to
/// the caller.
fn spawn_await_legacy_reply(
    caller: String,
    caller_id: String,
    internal_id: InternalId,
    rx: oneshot::Receiver<PendingOutcome>,
    pending: Arc<Mutex<HashMap<InternalId, PendingSlot>>>,
    out_tx: mpsc::Sender<PluginOutgoing>,
) {
    tokio::spawn(async move {
        let reply_body = match tokio::time::timeout(HANDLER_TIMEOUT, rx).await {
            Ok(Ok(Ok(HandlerOutcome::Single(output)))) => {
                caller_result_body(&caller, &caller_id, output)
            }
            Ok(Ok(Ok(HandlerOutcome::Multi(outputs)))) => {
                // Owner replied with multi-shape but caller used legacy run;
                // collapse on a 1-output multiset, surface a mismatch otherwise.
                if outputs.len() == 1 {
                    caller_result_body(
                        &caller,
                        &caller_id,
                        outputs.into_iter().next().expect("len 1").value,
                    )
                } else {
                    caller_error_body(
                        &caller,
                        Some(&caller_id),
                        ErrorCode::HandlerOutputMismatch,
                        "legacy `combinators.run` caller cannot consume multiset reply",
                    )
                }
            }
            Ok(Ok(Ok(HandlerOutcome::Error(msg)))) => {
                caller_error_body(&caller, Some(&caller_id), ErrorCode::HandlerError, &msg)
            }
            Ok(Ok(Err(other))) => caller_error_body(
                &caller,
                Some(&caller_id),
                ErrorCode::HandlerError,
                &other.to_string(),
            ),
            Ok(Err(_)) => caller_error_body(
                &caller,
                Some(&caller_id),
                ErrorCode::HandlerError,
                "handler reply channel closed",
            ),
            Err(_) => {
                let _ = pending.lock().await.remove(&internal_id);
                caller_error_body(
                    &caller,
                    Some(&caller_id),
                    ErrorCode::HandlerTimeout,
                    &format!(
                        "handler did not reply within {}ms",
                        HANDLER_TIMEOUT.as_millis()
                    ),
                )
            }
        };
        let _ = out_tx.send(PluginOutgoing::event(reply_body)).await;
    });
}

/// Spawn a task that waits for a `combinators.invoke` reply (multiset
/// `outputs[]`) and forwards `combinators.invoke.result` /
/// `combinators.error` to the caller.
fn spawn_await_invoke_reply(
    caller: String,
    caller_id: String,
    identity: Identity,
    internal_id: InternalId,
    rx: oneshot::Receiver<PendingOutcome>,
    pending: Arc<Mutex<HashMap<InternalId, PendingSlot>>>,
    out_tx: mpsc::Sender<PluginOutgoing>,
) {
    tokio::spawn(async move {
        let reply_body = match tokio::time::timeout(HANDLER_TIMEOUT, rx).await {
            Ok(Ok(Ok(HandlerOutcome::Multi(outputs)))) => {
                match validate_output_multiset(&identity.output_multiset, &outputs) {
                    Ok(()) => invoke_result_body(&caller, &caller_id, outputs),
                    Err(msg) => caller_error_body(
                        &caller,
                        Some(&caller_id),
                        ErrorCode::HandlerOutputMismatch,
                        &msg,
                    ),
                }
            }
            Ok(Ok(Ok(HandlerOutcome::Single(value)))) => {
                // Owner replied legacy single-output to a new-shape invoke.
                // Wrap into the registered single-element multiset when
                // possible; otherwise mismatch.
                if identity.output_multiset.len() == 1 {
                    let outputs = vec![TypedOutput {
                        type_: identity.output_multiset[0].clone(),
                        value,
                    }];
                    invoke_result_body(&caller, &caller_id, outputs)
                } else {
                    caller_error_body(
                        &caller,
                        Some(&caller_id),
                        ErrorCode::HandlerOutputMismatch,
                        "owner replied with single `output` to a multi-output signature",
                    )
                }
            }
            Ok(Ok(Ok(HandlerOutcome::Error(msg)))) => {
                caller_error_body(&caller, Some(&caller_id), ErrorCode::HandlerError, &msg)
            }
            Ok(Ok(Err(other))) => caller_error_body(
                &caller,
                Some(&caller_id),
                ErrorCode::HandlerError,
                &other.to_string(),
            ),
            Ok(Err(_)) => caller_error_body(
                &caller,
                Some(&caller_id),
                ErrorCode::HandlerError,
                "handler reply channel closed",
            ),
            Err(_) => {
                let _ = pending.lock().await.remove(&internal_id);
                caller_error_body(
                    &caller,
                    Some(&caller_id),
                    ErrorCode::HandlerTimeout,
                    &format!(
                        "handler did not reply within {}ms",
                        HANDLER_TIMEOUT.as_millis()
                    ),
                )
            }
        };
        let _ = out_tx.send(PluginOutgoing::event(reply_body)).await;
    });
}

// ---- static body constructors ----------------------------------------------

fn hello_body() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("combinators.hello".into()));
    m.insert("version".into(), Value::String(PLUGIN_VERSION.into()));
    m
}

fn ready_body() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("combinators.ready".into()));
    m
}

fn goodbye_body() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("combinators.goodbye".into()));
    m.insert("reason".into(), Value::String("stream closed".into()));
    m
}

async fn send_event(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: Map<String, Value>,
) -> Result<(), CombinatorsError> {
    out_tx
        .send(PluginOutgoing::event(body))
        .await
        .map_err(|_| CombinatorsError::Transport(TransportError::WriterClosed))
}

async fn send_ready(out_tx: &mpsc::Sender<PluginOutgoing>) -> Result<(), CombinatorsError> {
    out_tx
        .send(PluginOutgoing::system(SystemBody::Ready {
            protocol_version: PROTOCOL_VERSION.into(),
        }))
        .await
        .map_err(|_| CombinatorsError::Transport(TransportError::WriterClosed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::TraitImpl;
    use serde_json::json;

    fn fqt(plugin: &str, name: &str) -> FullyQualifiedType {
        FullyQualifiedType {
            plugin: plugin.into(),
            name: name.into(),
        }
    }

    fn fqk(plugin: &str, bare: &str) -> FullyQualifiedKind {
        FullyQualifiedKind {
            plugin: plugin.into(),
            bare: bare.into(),
        }
    }

    #[test]
    fn hello_body_advertises_plugin_version() {
        let b = hello_body();
        assert_eq!(
            b.get("kind").and_then(Value::as_str),
            Some("combinators.hello")
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
            Some("combinators.ready")
        );
        assert_eq!(b.len(), 1);
    }

    #[test]
    fn goodbye_body_carries_reason() {
        let b = goodbye_body();
        assert_eq!(
            b.get("kind").and_then(Value::as_str),
            Some("combinators.goodbye")
        );
        assert!(b.get("reason").and_then(Value::as_str).is_some());
    }

    #[test]
    fn synthesise_pass_through_echoes_input() {
        let req = InvokeRequest {
            caller_id: "c".into(),
            identity: Identity::new(1, fqt("p", "T"), vec![fqt("p", "T")]),
            inputs: vec![json!({"x": 7})],
        };
        let outs = synthesise_pass_through(&req);
        assert_eq!(outs.len(), 1);
        assert_eq!(outs[0].type_.to_wire(), "p.T");
        assert_eq!(outs[0].value, json!({"x": 7}));
    }

    #[test]
    fn tool_split_with_real_types_emits_correct_synthesis_for_tool_call_path() {
        let req = InvokeRequest {
            caller_id: "c".into(),
            identity: Identity::new(
                1,
                fqt("generic-provider", "ProviderOut"),
                vec![
                    fqt("generic-provider", "FinalAnswer"),
                    fqt("generic-tool", "ToolCalls"),
                ],
            ),
            inputs: vec![json!({
                "text": "I'll call a tool.",
                "tool_calls": [{"name": "search", "args": {}}],
            })],
        };
        let outs = synthesise_tool_split(&req).expect("ok");
        assert_eq!(outs.len(), 2);
        let tool = outs
            .iter()
            .find(|o| o.type_.to_wire() == "generic-tool.ToolCalls")
            .expect("tool slot");
        let answer = outs
            .iter()
            .find(|o| o.type_.to_wire() == "generic-provider.FinalAnswer")
            .expect("answer slot");
        assert!(tool.value.as_array().expect("array").len() == 1);
        assert_eq!(answer.value, Value::Null);
    }

    #[test]
    fn tool_split_with_real_types_emits_correct_synthesis_for_text_path() {
        let req = InvokeRequest {
            caller_id: "c".into(),
            identity: Identity::new(
                1,
                fqt("generic-provider", "ProviderOut"),
                vec![
                    fqt("generic-provider", "FinalAnswer"),
                    fqt("generic-tool", "ToolCalls"),
                ],
            ),
            inputs: vec![json!({"text": "Hi there."})],
        };
        let outs = synthesise_tool_split(&req).expect("ok");
        let tool = outs
            .iter()
            .find(|o| o.type_.to_wire() == "generic-tool.ToolCalls")
            .expect("tool slot");
        let answer = outs
            .iter()
            .find(|o| o.type_.to_wire() == "generic-provider.FinalAnswer")
            .expect("answer slot");
        assert_eq!(tool.value, Value::Null);
        assert_eq!(
            answer.value.get("text").and_then(Value::as_str),
            Some("Hi there.")
        );
    }

    #[test]
    fn retry_split_emits_only_selected_branch() {
        let req = InvokeRequest {
            caller_id: "c".into(),
            identity: Identity::new(
                1,
                fqt("generic-control", "RetryDecision"),
                vec![
                    fqt("generic-control", "Retry"),
                    fqt("generic-control", "Pass"),
                    fqt("generic-control", "Exhausted"),
                ],
            ),
            inputs: vec![json!({
                "route": "retry",
                "passthrough": { "text": "try again" },
            })],
        };
        let outs = synthesise_retry_split(&req).expect("ok");
        let retry = outs
            .iter()
            .find(|o| o.type_.to_wire() == "generic-control.Retry")
            .expect("retry slot");
        let pass = outs
            .iter()
            .find(|o| o.type_.to_wire() == "generic-control.Pass")
            .expect("pass slot");
        let exhausted = outs
            .iter()
            .find(|o| o.type_.to_wire() == "generic-control.Exhausted")
            .expect("exhausted slot");
        assert_eq!(retry.value, json!({"text": "try again"}));
        assert_eq!(pass.value, Value::Null);
        assert_eq!(exhausted.value, Value::Null);
    }

    #[tokio::test]
    async fn install_retry_split_succeeds_at_startup() {
        let r: Arc<Mutex<Registry>> = Arc::new(Mutex::new(Registry::new()));
        install_builtin_retry_split(&r).await;
        let id = Identity::new(
            1,
            fqt("generic-control", "RetryDecision"),
            vec![
                fqt("generic-control", "Retry"),
                fqt("generic-control", "Pass"),
                fqt("generic-control", "Exhausted"),
            ],
        );
        let guard = r.lock().await;
        let owned = guard.lookup(&id).expect("retry_split registered");
        assert_eq!(owned.handler.to_wire(), "nefor-combinators.retry_split");
    }

    #[tokio::test]
    async fn install_tool_split_succeeds_at_startup() {
        let r: Arc<Mutex<Registry>> = Arc::new(Mutex::new(Registry::new()));
        install_builtin_tool_split(&r).await;
        let id = Identity::new(
            1,
            fqt("generic-provider", "ProviderOut"),
            vec![
                fqt("generic-tool", "ToolCalls"),
                fqt("generic-provider", "FinalAnswer"),
            ],
        );
        let guard = r.lock().await;
        let owned = guard.lookup(&id).expect("tool_split registered");
        assert_eq!(owned.handler.to_wire(), "nefor-combinators.tool_split");
    }

    #[tokio::test]
    async fn query_round_trip_resolves_pass_through_via_synthesis() {
        // Drives the full query path: registry has only an Into entry; a
        // query for one resolved + one missing + one pass-through-synthesised
        // signature returns the right partition.
        let mut registry = Registry::new();
        registry
            .install(
                "p",
                vec![fqt("p", "A")],
                vec![TraitImpl::Into {
                    in_: fqt("p", "A"),
                    out: fqt("other", "B"),
                    handler: fqk("p", "a_to_b"),
                }],
            )
            .expect("install");

        // Build the query body manually and walk the resolution code path.
        let req = parse_query_body(
            json!({
                "id": "q-1",
                "signatures": [
                    { "in": "p.A", "out": ["other.B"] },
                    { "in": "p.NotThere", "out": ["p.AlsoNot"] },
                    { "in": "x.T", "out": ["x.T"] }    // pass_through synthesis
                ]
            })
            .as_object()
            .expect("obj"),
        )
        .expect("parse");

        let resolutions: Vec<QueryResolution> = {
            let r = registry;
            req.signatures
                .iter()
                .map(|sig| {
                    let id =
                        Identity::new(sig.arity, sig.in_type.clone(), sig.out_multiset.clone());
                    match r.lookup_or_pass_through(&id) {
                        Some(owned) => QueryResolution::Resolved { owner: owned.owner },
                        None => QueryResolution::Missing,
                    }
                })
                .collect()
        };
        assert!(matches!(
            resolutions[0],
            QueryResolution::Resolved { ref owner } if owner == "p"
        ));
        assert!(matches!(resolutions[1], QueryResolution::Missing));
        assert!(matches!(
            resolutions[2],
            QueryResolution::Resolved { ref owner } if owner == "nefor-combinators"
        ));
    }

    #[tokio::test]
    async fn legacy_merge_dispatch_round_trip_via_pending() {
        // Wires the dispatch loop's reader path: parse a Merge run, look up
        // the registered handler, simulate an owner reply through the
        // pending-map, and verify the fulfilled outcome.
        let mut registry = Registry::new();
        registry
            .install(
                "mock-plugin",
                vec![fqt("mock-plugin", "Message")],
                vec![TraitImpl::Merge {
                    type_: fqt("mock-plugin", "Message"),
                    handler: fqk("mock-plugin", "message.concat"),
                }],
            )
            .expect("install");

        let body = json!({
            "id": "caller-1",
            "op": "Merge",
            "type": "mock-plugin.Message",
            "inputs": [{"text": "hi "}, {"text": "there"}]
        });
        let req = parse_run_body(body.as_object().expect("obj")).expect("parse");
        let identity = req.identity();
        let owned = registry
            .lookup_or_pass_through(&identity)
            .expect("handler registered");
        assert_eq!(owned.handler.to_wire(), "mock-plugin.message.concat");

        let (tx, rx) = oneshot::channel::<PendingOutcome>();
        let pending: Arc<Mutex<HashMap<InternalId, PendingSlot>>> =
            Arc::new(Mutex::new(HashMap::new()));
        pending.lock().await.insert(
            "internal-1".into(),
            PendingSlot {
                tx,
                expect_multi: false,
            },
        );

        // Dispatch body shape check.
        let dispatch = handler_dispatch_body(&owned.handler, "internal-1", &req.inputs);
        assert_eq!(
            dispatch.get("kind").and_then(Value::as_str),
            Some("mock-plugin.message.concat")
        );

        // Owner "replies" — drive the oneshot.
        let slot = pending.lock().await.remove("internal-1").expect("pending");
        let _ = slot
            .tx
            .send(Ok(HandlerOutcome::Single(json!({"text": "hi there"}))));

        let received = rx.await.expect("oneshot delivered").expect("ok");
        match received {
            HandlerOutcome::Single(v) => {
                assert_eq!(v, json!({"text": "hi there"}));
            }
            other => panic!("unexpected outcome shape: {other:?}"),
        }
    }
}
