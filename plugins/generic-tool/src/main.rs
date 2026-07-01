//! generic-tool — type-registry hub for the canonical tool protocol.
//!
//! Sibling of `generic-provider` for the tool-execution role. Owns two
//! canonical types every tool-shaped reasoner ecosystem agrees on:
//!
//! - `generic-tool.ToolCalls`   — list of tool invocations a provider
//!   asked for (the fanout target slot for `tool_split`).
//! - `generic-tool.ToolResults` — list of tool execution outcomes that
//!   feeds back into the provider on the next firing.
//!
//! On startup the plugin sends a `combinators.register` declaring just
//! these types (no implementations). Concrete tool-source plugins
//! (basic-tools, mock-plugin's tool layer, ...) separately declare
//! `Into<generic-tool.ToolCalls, <them>.RawCalls>` and
//! `Into<<them>.RawResults, generic-tool.ToolResults>` against
//! MAG. Cross-namespace `Into.out` makes the hub-and-spoke
//! shape work without a many-to-many adapter mesh.
//!
//! This plugin is a passive type-registry hub. It does not execute tools,
//! it does not own combinator implementations, it does not gate or prompt.
//! The job of routing a graph node referencing `ToolCalls`/`ToolResults`
//! to a specific concrete tool source belongs to the Lua glue layer.
//!
//! ## Recommended JSON shapes (Schelling-point documentation only)
//!
//! These shapes are NOT enforced by this plugin — the registry only knows
//! the type tags. They exist so concrete tool sources' `Into`/`From`
//! conversions and downstream consumers all coalesce on the same wire
//! shape. Treat them as a community contract, not a spec rule.
//!
//! ```jsonc
//! // generic-tool.ToolCalls — list of tool invocations
//! {
//!   "calls": [
//!     { "id": "<call-id>", "name": "<tool-name>", "arguments": <any-json> }
//!   ]
//! }
//!
//! // generic-tool.ToolResults — list of tool outcomes (parallel to calls)
//! {
//!   "results": [
//!     { "id": "<call-id>",
//!       "ok"?: <any-json>,
//!       "error"?: "<diagnostic>" }
//!   ]
//! }
//! ```

use nefor_plugin_sdk::{await_ready_ok, spawn_stdin_reader, spawn_stdout_writer, TransportError};
use nefor_protocol::{Body, Envelope, PluginOutgoing, SystemBody};
use serde_json::{Map, Value};
use tokio::sync::mpsc;

const CHANNEL_CAP: usize = 64;

/// NCP version this plugin speaks.
const PROTOCOL_VERSION: &str = "0.1";

/// Plugin version, advertised in `generic-tool.hello`.
const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The two canonical type tags this plugin owns.
///
/// Bare names — the registry's `combinators.register` parser prepends our
/// namespace (`generic-tool`) at install time.
const CANONICAL_TYPES: &[&str] = &["ToolCalls", "ToolResults"];

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
        tracing::error!(error = %e, "generic-tool exited with error");
        eprintln!("generic-tool: {e}");
        std::process::exit(1);
    }
    // Force exit: `tokio::io::stdin()` parks a non-cancellable blocking
    // reader thread; same fix as mock-plugin.
    std::process::exit(0);
}

async fn run() -> Result<(), TransportError> {
    let (out_tx, _writer_handle) = spawn_stdout_writer(CHANNEL_CAP);
    let (in_tx, mut in_rx) = mpsc::channel::<Result<Envelope, TransportError>>(CHANNEL_CAP);
    let _reader_handle = spawn_stdin_reader(in_tx);

    send_ready(&out_tx).await?;
    let engine_version = await_ready_ok(&mut in_rx).await?;
    tracing::info!(engine_version = %engine_version, "ready");

    send_event(&out_tx, hello_body()).await?;
    send_event(&out_tx, register_body()).await?;
    send_event(&out_tx, ready_body()).await?;

    idle_until_shutdown(&mut in_rx).await?;

    let _ = out_tx.send(PluginOutgoing::event(goodbye_body())).await;
    Ok(())
}

/// The plugin has no incoming work — it only registers types and waits.
async fn idle_until_shutdown(
    in_rx: &mut mpsc::Receiver<Result<Envelope, TransportError>>,
) -> Result<(), TransportError> {
    loop {
        tokio::select! {
            maybe = in_rx.recv() => {
                match maybe {
                    Some(Ok(env)) => match &env.body {
                        Body::System(SystemBody::Shutdown { .. }) => {
                            tracing::info!("shutdown received");
                            return Ok(());
                        }
                        Body::System(_) => {
                            tracing::warn!(?env, "unexpected system envelope after handshake");
                        }
                        Body::Event(_) => {
                            // Not for us. Passive type registry.
                        }
                    },
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

// ---- static body constructors ----------------------------------------------

fn hello_body() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("generic-tool.hello".into()));
    m.insert("version".into(), Value::String(PLUGIN_VERSION.into()));
    m
}

fn ready_body() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("generic-tool.ready".into()));
    m
}

fn goodbye_body() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("generic-tool.goodbye".into()));
    m.insert("reason".into(), Value::String("stream closed".into()));
    m
}

/// Build the `combinators.register` body announcing our canonical types.
///
/// `implementations` is empty: this plugin owns no combinator handlers.
fn register_body() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("combinators.register".into()));
    let types = CANONICAL_TYPES
        .iter()
        .map(|t| Value::String((*t).to_owned()))
        .collect::<Vec<_>>();
    m.insert("types".into(), Value::Array(types));
    m.insert("implementations".into(), Value::Array(vec![]));
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

    #[test]
    fn hello_body_advertises_plugin_version() {
        let b = hello_body();
        assert_eq!(
            b.get("kind").and_then(Value::as_str),
            Some("generic-tool.hello")
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
            Some("generic-tool.ready")
        );
        assert_eq!(b.len(), 1);
    }

    #[test]
    fn goodbye_body_carries_reason() {
        let b = goodbye_body();
        assert_eq!(
            b.get("kind").and_then(Value::as_str),
            Some("generic-tool.goodbye")
        );
        assert!(b.get("reason").and_then(Value::as_str).is_some());
    }

    #[test]
    fn register_body_announces_all_canonical_types() {
        let b = register_body();
        assert_eq!(
            b.get("kind").and_then(Value::as_str),
            Some("combinators.register")
        );
        let types = b
            .get("types")
            .and_then(Value::as_array)
            .expect("types array");
        let names: Vec<&str> = types.iter().filter_map(Value::as_str).collect();
        assert_eq!(names, vec!["ToolCalls", "ToolResults"]);
    }

    #[test]
    fn register_body_has_no_implementations() {
        let b = register_body();
        let impls = b
            .get("implementations")
            .and_then(Value::as_array)
            .expect("impls array");
        assert!(
            impls.is_empty(),
            "generic-tool must not declare combinator handlers; it is a passive hub"
        );
    }

    #[test]
    fn register_body_uses_bare_type_names() {
        let b = register_body();
        let types = b
            .get("types")
            .and_then(Value::as_array)
            .expect("types array");
        for t in types {
            let s = t.as_str().expect("string");
            assert!(
                !s.contains('.'),
                "type name `{s}` contains a dot; should be bare"
            );
            assert!(!s.is_empty(), "empty type name");
        }
    }

    #[test]
    fn canonical_types_match_spec_set() {
        let expected: std::collections::BTreeSet<&str> =
            ["ToolCalls", "ToolResults"].into_iter().collect();
        let actual: std::collections::BTreeSet<&str> = CANONICAL_TYPES.iter().copied().collect();
        assert_eq!(actual, expected);
    }
}
