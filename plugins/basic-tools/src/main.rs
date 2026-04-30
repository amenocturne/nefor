//! basic-tools — NCP v0.1 plugin: file/bash/etc. tool primitives.
//!
//! v1 ships a single tool — `read_file`. Future tools (`write_file`, `bash`,
//! …) land here behind permission gating (see plugins/tool-gate).
//!
//! Wire contract (see `docs/chat-contract.md` → "Tool calling (v1)"):
//!
//! - On ready, advertise every tool this plugin owns. Two modes:
//!     * Standalone: broadcast `tool.register { tools: [...] }`. Providers
//!       see basic-tools as the canonical owner and route invocations
//!       directly via `basic-tools.tool.invoke`.
//!     * Gated (`--gate <name>` flag): emit
//!       `<gate>.tools.advertise { tools, source: "basic-tools" }` instead.
//!       The gate aggregates and re-emits `tool.register` under its own
//!       identity, so providers route invocations to the gate, which
//!       applies its policy and forwards back to us.
//! - Listen for `basic-tools.tool.invoke { id, name, args }` (kind is
//!   prefixed with our plugin name so the engine's `<peer>.<rest>` routing
//!   delivers it directly to us — see `starter/ncp.lua` `handle_event`).
//! - Reply with a broadcast `tool.result { id, output }` on success or
//!   `tool.result { id, error }` on failure. Caller correlates by `id`.

mod error;
mod ncp;
mod tools;

use nefor_protocol::{Body, Envelope, PluginOutgoing, SystemBody};
use serde_json::{Map, Value};
use tokio::sync::mpsc;

use crate::error::{BasicToolsError, ToolError};
use crate::tools::{run_tool, TOOLS};

/// NCP version this plugin speaks.
const PROTOCOL_VERSION: &str = "0.1";

/// Plugin name on the bus. Must match the `name` the starter spawns us under,
/// because the engine's prefix-routing keys off it (`<name>.tool.invoke`
/// delivers only to us). Hard-coded here rather than parsed from CLI: the
/// kind-prefix is part of the wire contract, not a per-spawn detail.
pub(crate) const PLUGIN_NAME: &str = "basic-tools";

/// Plugin version, advertised in `basic-tools.hello`.
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

    let gate = parse_gate_arg();

    if let Err(e) = run(gate).await {
        tracing::error!(error = %e, "basic-tools exited with error");
        eprintln!("basic-tools: {e}");
        std::process::exit(1);
    }
    // Force exit: `tokio::io::stdin()` parks a non-cancellable blocking
    // reader thread; letting the runtime drop naturally would hang the
    // process and keep the engine's `child.wait()` pending. Same fix as
    // nefor-combinators / mock-plugin.
    std::process::exit(0);
}

/// Parse `--gate <name>` (optional). When set, registration is routed
/// through the named gate plugin via `<gate>.tools.advertise` rather than
/// a public `tool.register` broadcast.
fn parse_gate_arg() -> Option<String> {
    use clap::{Arg, Command};
    Command::new("basic-tools")
        .arg(
            Arg::new("gate")
                .long("gate")
                .help("Tool-gate plugin name to advertise to (suppresses public tool.register)."),
        )
        .get_matches()
        .get_one::<String>("gate")
        .cloned()
}

async fn run(gate: Option<String>) -> Result<(), BasicToolsError> {
    let (out_tx, _writer_handle) = ncp::spawn_stdout_writer();
    let (in_tx, mut in_rx) = mpsc::channel::<Result<Envelope, BasicToolsError>>(ncp::CHANNEL_CAP);
    let _reader_handle = ncp::spawn_stdin_reader(in_tx);

    send_ready(&out_tx).await?;
    let engine_version = ncp::await_ready_ok(&mut in_rx).await?;
    tracing::info!(engine_version = %engine_version, tools = TOOLS.len(), gate = ?gate, "ready");

    send_event(&out_tx, hello_body()).await?;
    match gate.as_deref() {
        Some(g) => send_event(&out_tx, tools_advertise_body(g)).await?,
        None => send_event(&out_tx, tool_register_body()).await?,
    }

    run_dispatch_loop(&out_tx, &mut in_rx).await?;

    let _ = out_tx.send(PluginOutgoing::event(goodbye_body())).await;
    Ok(())
}

async fn run_dispatch_loop(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    in_rx: &mut mpsc::Receiver<Result<Envelope, BasicToolsError>>,
) -> Result<(), BasicToolsError> {
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
                        Body::Event(map) => {
                            dispatch_event(out_tx, map).await?;
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

/// Route a bus event body based on its `kind`. The engine's prefix-routing
/// delivers `basic-tools.*` events only to us, but other plugins' events
/// (registers, broadcasts) are also delivered — we filter here.
async fn dispatch_event(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
) -> Result<(), BasicToolsError> {
    let kind = match body.get("kind").and_then(Value::as_str) {
        Some(k) => k,
        None => return Ok(()),
    };
    let invoke_kind = format!("{PLUGIN_NAME}.tool.invoke");
    if kind == invoke_kind {
        handle_tool_invoke(out_tx, body).await?;
    }
    Ok(())
}

async fn handle_tool_invoke(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
) -> Result<(), BasicToolsError> {
    // `id` is the caller's correlation token. If it's missing we can't
    // reply usefully — log and drop. (A v2 protocol could surface this as
    // a generic error event, but there's no caller to address it to.)
    let id = match body.get("id").and_then(Value::as_str) {
        Some(s) => s.to_owned(),
        None => {
            tracing::warn!("tool.invoke missing required string field `id`; dropping");
            return Ok(());
        }
    };

    let name = match body.get("name").and_then(Value::as_str) {
        Some(s) => s.to_owned(),
        None => {
            send_event(
                out_tx,
                tool_result_error_body(&id, "tool.invoke missing required string field `name`"),
            )
            .await?;
            return Ok(());
        }
    };

    // `args` is optional on the wire — a tool that takes no parameters
    // shouldn't require an empty `{}`. Default to an empty object so the
    // tool's parser sees a valid JSON object.
    let args = body.get("args").cloned().unwrap_or(Value::Object(Map::new()));

    // Reject names this plugin doesn't own up front — keeps the error
    // surface small (BadArgs is the closest-fitting variant) and avoids
    // calling `run_tool`'s defensive fallback.
    let owns_tool = TOOLS.iter().any(|t| t.name == name);
    if !owns_tool {
        // Silently ignore: the invoke might be addressed at a different
        // tool-providing plugin via shared bus traffic. With prefix
        // routing this branch is unreachable in practice — the engine
        // only delivers `basic-tools.tool.invoke` to us — but we keep
        // the guard for forward compatibility (other tool plugins might
        // share the prefix scheme one day).
        tracing::debug!(name = %name, "tool.invoke for unowned tool; ignoring");
        return Ok(());
    }

    match run_tool(&name, &args).await {
        Ok(output) => {
            send_event(out_tx, tool_result_ok_body(&id, &output)).await?;
        }
        Err(e) => {
            let message = render_tool_error(&e);
            send_event(out_tx, tool_result_error_body(&id, &message)).await?;
        }
    }
    Ok(())
}

fn render_tool_error(e: &ToolError) -> String {
    // Tool error messages are user-facing (the LLM sees them via the
    // provider). The Display impls are already shaped for that audience.
    e.to_string()
}

// ---- static body constructors ----------------------------------------------

fn hello_body() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{PLUGIN_NAME}.hello")),
    );
    m.insert("version".into(), Value::String(PLUGIN_VERSION.into()));
    m
}

fn tool_register_body() -> Map<String, Value> {
    let tools: Vec<Value> = TOOLS
        .iter()
        .map(|t| {
            let mut m = Map::new();
            m.insert("name".into(), Value::String(t.name.into()));
            m.insert("description".into(), Value::String(t.description.into()));
            m.insert("parameters".into(), (t.schema)());
            Value::Object(m)
        })
        .collect();
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("tool.register".into()));
    m.insert("tools".into(), Value::Array(tools));
    m
}

/// Private gate-addressed advertisement. Same `tools` shape as
/// `tool_register_body`, but the kind is prefixed with `<gate>.` so
/// engine prefix-routing delivers it only to the gate, and a `source`
/// field tags us as the underlying owner so the gate's reverse map
/// knows where to forward invocations.
fn tools_advertise_body(gate: &str) -> Map<String, Value> {
    let tools: Vec<Value> = TOOLS
        .iter()
        .map(|t| {
            let mut m = Map::new();
            m.insert("name".into(), Value::String(t.name.into()));
            m.insert("description".into(), Value::String(t.description.into()));
            m.insert("parameters".into(), (t.schema)());
            Value::Object(m)
        })
        .collect();
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{gate}.tools.advertise")),
    );
    m.insert("source".into(), Value::String(PLUGIN_NAME.into()));
    m.insert("tools".into(), Value::Array(tools));
    m
}

fn tool_result_ok_body(id: &str, output: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("tool.result".into()));
    m.insert("id".into(), Value::String(id.to_owned()));
    m.insert("output".into(), Value::String(output.to_owned()));
    m
}

fn tool_result_error_body(id: &str, message: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("tool.result".into()));
    m.insert("id".into(), Value::String(id.to_owned()));
    m.insert("error".into(), Value::String(message.to_owned()));
    m
}

fn goodbye_body() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{PLUGIN_NAME}.goodbye")),
    );
    m.insert("reason".into(), Value::String("stream closed".into()));
    m
}

async fn send_event(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: Map<String, Value>,
) -> Result<(), BasicToolsError> {
    out_tx
        .send(PluginOutgoing::event(body))
        .await
        .map_err(|_| BasicToolsError::WriterClosed)
}

async fn send_ready(out_tx: &mpsc::Sender<PluginOutgoing>) -> Result<(), BasicToolsError> {
    out_tx
        .send(PluginOutgoing::system(SystemBody::Ready {
            protocol_version: PROTOCOL_VERSION.into(),
        }))
        .await
        .map_err(|_| BasicToolsError::WriterClosed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hello_body_advertises_plugin_version() {
        let b = hello_body();
        assert_eq!(
            b.get("kind").and_then(Value::as_str),
            Some("basic-tools.hello")
        );
        assert_eq!(
            b.get("version").and_then(Value::as_str),
            Some(PLUGIN_VERSION)
        );
    }

    #[test]
    fn tool_register_body_lists_every_tool_with_schema() {
        let b = tool_register_body();
        assert_eq!(
            b.get("kind").and_then(Value::as_str),
            Some("tool.register")
        );
        let arr = b.get("tools").and_then(Value::as_array).expect("tools");
        assert_eq!(arr.len(), TOOLS.len());
        let read_file = arr
            .iter()
            .find(|v| v.get("name").and_then(Value::as_str) == Some("read_file"))
            .expect("read_file in tools");
        assert!(read_file.get("description").and_then(Value::as_str).is_some());
        let params = read_file.get("parameters").expect("parameters");
        assert_eq!(params.get("type").and_then(Value::as_str), Some("object"));
    }

    #[test]
    fn tool_result_ok_body_carries_id_and_output() {
        let b = tool_result_ok_body("call-1", "hello");
        assert_eq!(b.get("kind").and_then(Value::as_str), Some("tool.result"));
        assert_eq!(b.get("id").and_then(Value::as_str), Some("call-1"));
        assert_eq!(b.get("output").and_then(Value::as_str), Some("hello"));
        assert!(!b.contains_key("error"));
    }

    #[test]
    fn tool_result_error_body_carries_id_and_error() {
        let b = tool_result_error_body("call-2", "boom");
        assert_eq!(b.get("kind").and_then(Value::as_str), Some("tool.result"));
        assert_eq!(b.get("id").and_then(Value::as_str), Some("call-2"));
        assert_eq!(b.get("error").and_then(Value::as_str), Some("boom"));
        assert!(!b.contains_key("output"));
    }

    #[test]
    fn goodbye_body_uses_plugin_prefix() {
        let b = goodbye_body();
        assert_eq!(
            b.get("kind").and_then(Value::as_str),
            Some("basic-tools.goodbye")
        );
        assert!(b.get("reason").and_then(Value::as_str).is_some());
    }

    /// End-to-end dispatch: feed a `basic-tools.tool.invoke` event into
    /// `dispatch_event` and verify a matching `tool.result { output }`
    /// emerges on the writer channel.
    #[tokio::test]
    async fn dispatch_invoke_read_file_emits_result() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut f = NamedTempFile::new().expect("tempfile");
        write!(f, "abc").expect("write");
        let path = f.path().to_str().expect("utf8 path").to_owned();

        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let body = json!({
            "kind": "basic-tools.tool.invoke",
            "id": "call-7",
            "name": "read_file",
            "args": { "path": path }
        })
        .as_object()
        .expect("obj")
        .clone();

        dispatch_event(&tx, &body).await.expect("dispatch ok");

        let msg = rx.recv().await.expect("got reply");
        let line = msg.to_line();
        let v: Value = serde_json::from_str(&line).expect("json");
        let body = v.get("body").expect("body");
        assert_eq!(body.get("kind").and_then(Value::as_str), Some("tool.result"));
        assert_eq!(body.get("id").and_then(Value::as_str), Some("call-7"));
        assert_eq!(body.get("output").and_then(Value::as_str), Some("abc"));
    }

    /// Invoke with a non-existent path produces a `tool.result { error }`.
    #[tokio::test]
    async fn dispatch_invoke_missing_file_emits_error() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let body = json!({
            "kind": "basic-tools.tool.invoke",
            "id": "call-8",
            "name": "read_file",
            "args": { "path": "/definitely/does/not/exist/qzx" }
        })
        .as_object()
        .expect("obj")
        .clone();

        dispatch_event(&tx, &body).await.expect("dispatch ok");

        let msg = rx.recv().await.expect("got reply");
        let line = msg.to_line();
        let v: Value = serde_json::from_str(&line).expect("json");
        let body = v.get("body").expect("body");
        assert_eq!(body.get("kind").and_then(Value::as_str), Some("tool.result"));
        assert_eq!(body.get("id").and_then(Value::as_str), Some("call-8"));
        let err = body.get("error").and_then(Value::as_str).expect("error");
        assert!(err.contains("file not found"), "got: {err}");
        assert!(body.get("output").is_none());
    }

    /// Missing `id` is dropped silently (no caller to address) — verify no
    /// reply is emitted.
    #[tokio::test]
    async fn dispatch_invoke_missing_id_is_dropped() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let body = json!({
            "kind": "basic-tools.tool.invoke",
            "name": "read_file",
            "args": { "path": "/tmp/whatever" }
        })
        .as_object()
        .expect("obj")
        .clone();
        dispatch_event(&tx, &body).await.expect("dispatch ok");
        // Drop the sender so try_recv returns Disconnected (or Empty if
        // we got here too fast).
        drop(tx);
        match rx.recv().await {
            None => {}
            Some(unexpected) => panic!("expected no reply, got {}", unexpected.to_line()),
        }
    }

    /// Missing `name` — but with a valid `id` — produces a tool.result with
    /// an error so the caller can correlate.
    #[tokio::test]
    async fn dispatch_invoke_missing_name_emits_error() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let body = json!({
            "kind": "basic-tools.tool.invoke",
            "id": "call-9"
        })
        .as_object()
        .expect("obj")
        .clone();
        dispatch_event(&tx, &body).await.expect("dispatch ok");
        let msg = rx.recv().await.expect("reply");
        let line = msg.to_line();
        let v: Value = serde_json::from_str(&line).expect("json");
        let body = v.get("body").expect("body");
        assert_eq!(body.get("id").and_then(Value::as_str), Some("call-9"));
        assert!(body.get("error").is_some());
    }

    /// Events with a different kind (e.g. another plugin's broadcast) are
    /// ignored — no spurious replies.
    #[tokio::test]
    async fn dispatch_ignores_unrelated_events() {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let body = json!({ "kind": "ollama.stream.delta", "text": "hi" })
            .as_object()
            .expect("obj")
            .clone();
        dispatch_event(&tx, &body).await.expect("dispatch ok");
        drop(tx);
        assert!(rx.recv().await.is_none());
    }
}
