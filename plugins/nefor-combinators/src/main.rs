//! nefor-combinators — NCP v0.1 plugin: combinator registry + executor.
//!
//! Plugins on the bus register their type-aware combinator implementations
//! (`Merge`, `Into`) via `combinators.register`. Callers ask this plugin to
//! perform an op via `combinators.run`; the plugin looks up the registered
//! handler, dispatches to it, and echoes the handler's reply back to the
//! caller as `combinators.result` / `combinators.error`.
//!
//! Wire schema: see the plugin architecture doc
//! (`nefor-reasoner-architecture.md`, section "The combinator library").
//!
//! Slice 1 scope:
//! - `Merge` end-to-end.
//! - `Into` parsed and stubbed (replies `no_handler_registered`).
//! - Type-agnostic combinators (`Chain`, `Identity`, `Map<Option<T>>`, …)
//!   are library-only and land when a consumer needs them — they are NOT
//!   registered through this plugin.

mod dispatch;
mod error;
mod ncp;
mod registry;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use nefor_protocol::{Body, Envelope, PluginOutgoing, SystemBody};
use serde_json::{Map, Value};
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::dispatch::{
    caller_error_body, caller_result_body, classify_reply_kind, handler_dispatch_body,
    parse_run_body, HandlerReplyKind, InternalId, Op, PendingMap,
};
use crate::error::{CombinatorsError, ErrorCode};
use crate::registry::{parse_register_body, Registry};

/// NCP version this plugin speaks.
const PROTOCOL_VERSION: &str = "0.1";

/// Plugin version, advertised in `combinators.hello`.
const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Handler timeout bound (v1: 30s, per spec).
const HANDLER_TIMEOUT: Duration = Duration::from_secs(30);

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
    let (out_tx, _writer_handle) = ncp::spawn_stdout_writer();
    let (in_tx, mut in_rx) = mpsc::channel::<Result<Envelope, CombinatorsError>>(ncp::CHANNEL_CAP);
    let _reader_handle = ncp::spawn_stdin_reader(in_tx);

    send_ready(&out_tx).await?;
    let engine_version = ncp::await_ready_ok(&mut in_rx).await?;
    tracing::info!(engine_version = %engine_version, "ready");

    send_event(&out_tx, hello_body()).await?;
    send_event(&out_tx, ready_body()).await?;

    let registry: Arc<Mutex<Registry>> = Arc::new(Mutex::new(Registry::new()));
    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));

    run_dispatch_loop(&registry, &pending, &out_tx, &mut in_rx).await?;

    let _ = out_tx.send(PluginOutgoing::event(goodbye_body())).await;
    Ok(())
}

async fn run_dispatch_loop(
    registry: &Arc<Mutex<Registry>>,
    pending: &PendingMap,
    out_tx: &mpsc::Sender<PluginOutgoing>,
    in_rx: &mut mpsc::Receiver<Result<Envelope, CombinatorsError>>,
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
    pending: &PendingMap,
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
            let maybe_tx = pending.lock().await.remove(internal_id);
            if let Some(tx) = maybe_tx {
                let outcome = match shape {
                    HandlerReplyKind::Result => match body.get("output").cloned() {
                        Some(v) => Ok(v),
                        None => Err(CombinatorsError::Handler(
                            "handler reply missing `output`".into(),
                        )),
                    },
                    HandlerReplyKind::Error => {
                        let msg = body
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("<no message>")
                            .to_owned();
                        Err(CombinatorsError::Handler(msg))
                    }
                };
                // Receiver may be gone if the per-run task already timed
                // out — that's fine, nothing to do.
                let _ = tx.send(outcome);
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
    pending: &PendingMap,
    out_tx: &mpsc::Sender<PluginOutgoing>,
    caller: &str,
    body: &Map<String, Value>,
) -> Result<(), CombinatorsError> {
    let req = match parse_run_body(body) {
        Ok(r) => r,
        Err(e) => {
            let (code, message, caller_id) = match &e {
                CombinatorsError::RunRejected { code, message } => {
                    // Best-effort pull of `id` for the reply. Parse errors
                    // may have failed BEFORE id, hence Option.
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

    // Slice 1: Into is stubbed — dispatch replies `no_handler_registered`.
    // Full Into lands with the first consumer that needs it.
    match req.op {
        Op::Into => {
            send_event(
                out_tx,
                caller_error_body(
                    caller,
                    Some(&req.caller_id),
                    ErrorCode::NoHandlerRegistered,
                    "Into is not implemented in Slice 1",
                ),
            )
            .await?;
            return Ok(());
        }
        Op::Merge => {}
    }

    // Look up the Merge handler.
    let handler = {
        let guard = registry.lock().await;
        guard.merge_handler(&req.type_).cloned()
    };
    let handler = match handler {
        Some(h) => h,
        None => {
            send_event(
                out_tx,
                caller_error_body(
                    caller,
                    Some(&req.caller_id),
                    ErrorCode::NoHandlerRegistered,
                    &format!("no Merge handler for type `{}`", req.type_.to_wire()),
                ),
            )
            .await?;
            return Ok(());
        }
    };

    // Reserve a pending slot, emit the dispatch, then spawn a task that
    // awaits the oneshot + timeout and forwards the outcome to the caller.
    let internal_id: InternalId = uuid::Uuid::new_v4().to_string();
    let (tx, rx) = oneshot::channel::<Result<Value, CombinatorsError>>();
    pending.lock().await.insert(internal_id.clone(), tx);

    let dispatch_body = handler_dispatch_body(&handler, &internal_id, &req.inputs);
    send_event(out_tx, dispatch_body).await?;

    spawn_await_handler_reply(
        caller.to_owned(),
        req.caller_id.clone(),
        internal_id,
        rx,
        pending.clone(),
        out_tx.clone(),
    );

    // `req` still owned here — passing ownership to the spawned task would
    // require cloning inputs we no longer use. Drop implicitly at fn end.
    let _ = req;
    Ok(())
}

/// Spawn a task that waits for the handler's reply (or the timeout), then
/// forwards a `combinators.result` / `combinators.error` to the caller.
fn spawn_await_handler_reply(
    caller: String,
    caller_id: String,
    internal_id: InternalId,
    rx: oneshot::Receiver<Result<Value, CombinatorsError>>,
    pending: PendingMap,
    out_tx: mpsc::Sender<PluginOutgoing>,
) {
    tokio::spawn(async move {
        let reply_body = match tokio::time::timeout(HANDLER_TIMEOUT, rx).await {
            Ok(Ok(Ok(output))) => caller_result_body(&caller, &caller_id, output),
            Ok(Ok(Err(CombinatorsError::Handler(msg)))) => {
                caller_error_body(&caller, Some(&caller_id), ErrorCode::HandlerError, &msg)
            }
            Ok(Ok(Err(other))) => caller_error_body(
                &caller,
                Some(&caller_id),
                ErrorCode::HandlerError,
                &other.to_string(),
            ),
            // Oneshot dropped without sending — treat as handler error.
            Ok(Err(_)) => caller_error_body(
                &caller,
                Some(&caller_id),
                ErrorCode::HandlerError,
                "handler reply channel closed",
            ),
            Err(_) => {
                // Timeout: evict our pending entry so a late reply is
                // ignored rather than silently dropped later.
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
        .map_err(|_| CombinatorsError::WriterClosed)
}

async fn send_ready(out_tx: &mpsc::Sender<PluginOutgoing>) -> Result<(), CombinatorsError> {
    out_tx
        .send(PluginOutgoing::system(SystemBody::Ready {
            protocol_version: PROTOCOL_VERSION.into(),
        }))
        .await
        .map_err(|_| CombinatorsError::WriterClosed)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
