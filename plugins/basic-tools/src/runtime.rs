// basic-tools — NCP v0.1 plugin: file, search, and process primitives.
//
// Mutating and process capabilities are intended to be composed behind
// permission gating (see plugins/tool-gate).
//
// Wire contract (see `docs/chat-contract.md` → "Tool calling (v1)"):
//
// - On ready, advertise every tool this plugin owns. Two modes:
//     * Standalone: broadcast `tool.register { tools: [...] }`. Providers
//       see basic-tools as the canonical owner and route invocations
//       directly via `basic-tools.tool.invoke`.
//     * Gated (`--gate <name>` flag): emit
//       `<gate>.tools.advertise { tools, source: "basic-tools" }` instead.
//       The gate aggregates and re-emits `tool.register` under its own
//       identity, so providers route invocations to the gate, which
//       applies its policy and forwards back to us.
// - Listen for `basic-tools.tool.invoke { id, name, args }` (kind is
//   prefixed with our plugin name so the engine's `<peer>.<rest>` routing
//   delivers it directly to us — see `examples/nefor-agent/ncp.lua` `handle_event`).
// - Reply with a broadcast `tool.result { id, output }` on success or
//   `tool.result { id, error }` on failure. Caller correlates by `id`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use nefor_plugin_sdk::{await_ready_ok, spawn_stdin_reader, spawn_stdout_writer, TransportError};
use nefor_protocol::{Body, Envelope, PluginOutgoing, SystemBody};
use serde_json::{Map, Value};
use tokio::sync::{mpsc, oneshot};

use crate::error::ToolError;
use crate::tools::{process, process_exec, run_tool, shell_script, TOOLS};

const CHANNEL_CAP: usize = 256;

/// In-flight cancellable invocations, keyed by invoke id. Only process
/// capabilities register here because they hold killable OS process groups; a
/// `basic-tools.tool.cancel { id }` fires the matching sender, which trips the
/// running task's cancel arm and kills the child's process group.
type Cancels = Arc<Mutex<HashMap<String, oneshot::Sender<()>>>>;

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

    let gate = parse_args();

    if let Err(e) = run(gate.gate).await {
        tracing::error!(error = %e, "basic-tools exited with error");
        eprintln!("basic-tools: {e}");
        std::process::exit(1);
    }
    // Force exit: `tokio::io::stdin()` parks a non-cancellable blocking
    // reader thread; letting the runtime drop naturally would hang the
    // process and keep the engine's `child.wait()` pending. Same fix as
    // mock-plugin.
    std::process::exit(0);
}

/// Parse `--gate <name>` (optional). When set, registration is routed
/// through the named gate plugin via `<gate>.tools.advertise` rather than
/// a public `tool.register` broadcast.
struct Args {
    gate: Option<String>,
}

fn parse_args() -> Args {
    use clap::{Arg, Command};
    let matches = Command::new("basic-tools")
        .arg(
            Arg::new("gate")
                .long("gate")
                .help("Tool-gate plugin name to advertise to (suppresses public tool.register)."),
        )
        .arg(
            Arg::new("read-file-max-bytes")
                .long("read-file-max-bytes")
                .value_parser(clap::value_parser!(u64).range(1..))
                .required(true)
                .help("Composition-supplied maximum bytes returned by read_file."),
        )
        .get_matches();
    let max_read_bytes = *matches
        .get_one::<u64>("read-file-max-bytes")
        .expect("clap requires read-file-max-bytes");
    crate::tools::read_file::configure_max_bytes(max_read_bytes)
        .expect("read_file maximum configured once at startup");
    Args {
        gate: matches.get_one::<String>("gate").cloned(),
    }
}

async fn run(gate: Option<String>) -> Result<(), TransportError> {
    let (out_tx, _writer_handle) = spawn_stdout_writer(CHANNEL_CAP);
    let (in_tx, mut in_rx) = mpsc::channel::<Result<Envelope, TransportError>>(CHANNEL_CAP);
    let _reader_handle = spawn_stdin_reader(in_tx);

    send_ready(&out_tx).await?;
    let engine_version = await_ready_ok(&mut in_rx).await?;
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
    in_rx: &mut mpsc::Receiver<Result<Envelope, TransportError>>,
) -> Result<(), TransportError> {
    let cancels: Cancels = Arc::new(Mutex::new(HashMap::new()));
    let mut tasks = Vec::new();
    let termination = termination_signal();
    tokio::pin!(termination);
    loop {
        tokio::select! {
            maybe = in_rx.recv() => {
                match maybe {
                    Some(Ok(env)) => match &env.body {
                        Body::System(SystemBody::Shutdown { .. }) => {
                            tracing::info!("shutdown received");
                            break;
                        }
                        Body::System(_) => {
                            tracing::warn!(?env, "unexpected system envelope after handshake");
                        }
                        Body::Event(map) => {
                            if is_tool_invoke_event(map) {
                                tasks.retain(|task: &tokio::task::JoinHandle<()>| !task.is_finished());
                                tasks.push(spawn_tool_invoke(out_tx, map, &cancels));
                            } else if is_tool_cancel_event(map) {
                                handle_tool_cancel(&cancels, map);
                            } else {
                                dispatch_event(out_tx, map).await?;
                            }
                        }
                    },
                    Some(Err(e)) => {
                        tracing::error!(error = %e, "stdin parse error; dropping line");
                    }
                    None => {
                        tracing::info!("stdin closed; exiting");
                        break;
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("ctrl-c; exiting");
                break;
            }
            _ = &mut termination => {
                tracing::info!("termination signal; cancelling active tools");
                break;
            }
        }
    }
    cancel_and_wait(&cancels, tasks).await;
    Ok(())
}

async fn termination_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut signal) => { signal.recv().await; }
            Err(error) => {
                tracing::error!(%error, "failed to install termination signal handler");
                std::future::pending::<()>().await;
            }
        }
    }
    #[cfg(not(unix))]
    std::future::pending::<()>().await;
}

async fn cancel_and_wait(cancels: &Cancels, tasks: Vec<tokio::task::JoinHandle<()>>) {
    let pending = match cancels.lock() {
        Ok(mut registrations) => registrations.drain().map(|(_, tx)| tx).collect::<Vec<_>>(),
        Err(error) => {
            tracing::error!(%error, "process cancellation registry poisoned during shutdown");
            return;
        }
    };
    for cancel in pending { let _ = cancel.send(()); }
    // Cancellation drives each process group through kill + reap. Joining the
    // invocation owners keeps plugin exit from racing that accounting.
    for task in tasks { let _ = task.await; }
}

/// Route a bus event body based on its `kind`. The engine's prefix-routing
/// delivers `basic-tools.*` events only to us, but other plugins' events
/// (registers, broadcasts) are also delivered — we filter here.
async fn dispatch_event(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
) -> Result<(), TransportError> {
    let kind = match body.get("kind").and_then(Value::as_str) {
        Some(k) => k,
        None => return Ok(()),
    };
    if is_tool_invoke_kind(kind) {
        handle_tool_invoke(out_tx, body, None).await?;
    }
    Ok(())
}

fn is_tool_invoke_event(body: &Map<String, Value>) -> bool {
    body.get("kind")
        .and_then(Value::as_str)
        .is_some_and(is_tool_invoke_kind)
}

fn is_tool_invoke_kind(kind: &str) -> bool {
    kind == format!("{PLUGIN_NAME}.tool.invoke")
}

fn is_tool_cancel_event(body: &Map<String, Value>) -> bool {
    body.get("kind").and_then(Value::as_str) == Some(format!("{PLUGIN_NAME}.tool.cancel").as_str())
}

/// Handle a `basic-tools.tool.cancel { id }` (the gate forwards the kernel's
/// interrupt cancel for the in-flight firing id). Fire the matching sender so
/// the running process task kills its process group and returns
/// [`ToolError::ProcessCancelled`]. No entry means the invocation already
/// finished (or was never a process capability) — a clean no-op.
fn handle_tool_cancel(cancels: &Cancels, body: &Map<String, Value>) {
    let id = match body.get("id").and_then(Value::as_str) {
        Some(i) => i,
        None => return,
    };
    let taken = match cancels.lock() {
        Ok(mut registrations) => registrations.remove(id),
        Err(error) => {
            tracing::error!(%error, "process cancellation registry poisoned");
            return;
        }
    };
    match taken {
        Some(tx) => {
            // Err means the task already completed and dropped its receiver —
            // nothing left to cancel.
            let _ = tx.send(());
            tracing::info!(id = %id, "tool.cancel: signalled in-flight process to terminate");
        }
        None => {
            tracing::info!(id = %id, "tool.cancel: no in-flight cancellable invocation; no-op");
        }
    }
}

fn spawn_tool_invoke(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
    cancels: &Cancels,
) -> tokio::task::JoinHandle<()> {
    let out_tx = out_tx.clone();
    let body = body.clone();
    let cancels = Arc::clone(cancels);
    // Register cancellation for either capability backed by an OS process.
    let id = body.get("id").and_then(Value::as_str).map(str::to_owned);
    let is_process = body
        .get("name")
        .and_then(Value::as_str)
        .is_some_and(|name| name == process_exec::NAME || name == shell_script::NAME);
    let cancel_rx = match (&id, is_process) {
        (Some(id), true) => {
            let (tx, rx) = oneshot::channel();
            match cancels.lock() {
                Ok(mut registrations) => {
                    registrations.insert(id.clone(), tx);
                    Some(rx)
                }
                Err(error) => {
                    tracing::error!(%error, "process cancellation registry poisoned");
                    None
                }
            }
        }
        _ => None,
    };
    tokio::spawn(async move {
        if let Err(e) = handle_tool_invoke(&out_tx, &body, cancel_rx).await {
            tracing::error!(error = %e, "tool.invoke task failed");
        }
        // Clear the registration on completion so a late cancel is a no-op and
        // the map doesn't leak finished invocations.
        if let Some(id) = id {
            match cancels.lock() {
                Ok(mut registrations) => {
                    registrations.remove(&id);
                }
                Err(error) => tracing::error!(%error, "process cancellation registry poisoned"),
            }
        }
    })
}

async fn handle_tool_invoke(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
    cancel: Option<oneshot::Receiver<()>>,
) -> Result<(), TransportError> {
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
    let args = body
        .get("args")
        .cloned()
        .unwrap_or(Value::Object(Map::new()));

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

    // Process capabilities share streaming and cancellation mechanics.
    let outcome = if name == process_exec::NAME || name == shell_script::NAME {
        let preview = Arc::new(process::LivePreview::default());
        let (finished_tx, finished_rx) = oneshot::channel();
        let forward = tokio::spawn(forward_preview(
            Arc::clone(&preview),
            out_tx.clone(),
            id.clone(),
            finished_rx,
        ));
        let result = if name == process_exec::NAME {
            process_exec::run_cancellable_streaming(&args, cancel, Some(preview)).await
        } else {
            shell_script::run_cancellable_streaming(&args, cancel, Some(preview)).await
        };
        let _ = finished_tx.send(());
        let _ = forward.await;
        result
    } else {
        run_tool(&name, &args).await
    };
    match outcome {
        Ok(output) => {
            send_event(out_tx, tool_result_ok_body(&id, output)).await?;
        }
        Err(e) => {
            let message = render_tool_error(&e);
            send_event(out_tx, tool_result_error_body(&id, &message)).await?;
        }
    }
    Ok(())
}

/// Live output is a lossy preview; the authoritative output is the
/// `tool.result`. Flushing on a fixed cadence caps `tool.stream` traffic at
/// one event per stream per interval, however fast the child writes.
const PREVIEW_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

async fn forward_preview(
    preview: Arc<process::LivePreview>,
    out_tx: mpsc::Sender<PluginOutgoing>,
    id: String,
    mut finished: oneshot::Receiver<()>,
) {
    let mut ticks = tokio::time::interval(PREVIEW_FLUSH_INTERVAL);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let done = tokio::select! {
            _ = ticks.tick() => false,
            _ = &mut finished => true,
        };
        for chunk in preview.take() {
            let (stream, bytes) = match chunk {
                process::StreamChunk::Stdout(bytes) => ("stdout", bytes),
                process::StreamChunk::Stderr(bytes) => ("stderr", bytes),
            };
            let text = String::from_utf8_lossy(&bytes);
            if send_event(&out_tx, tool_stream_body(&id, stream, &text)).await.is_err() {
                return;
            }
        }
        if done {
            return;
        }
    }
}

fn render_tool_error(e: &ToolError) -> String {
    // Tool error messages are user-facing (the LLM sees them via the
    // provider). The Display impls are already shaped for that audience.
    e.to_string()
}

// ---- static body constructors ----------------------------------------------

fn hello_body() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String(format!("{PLUGIN_NAME}.hello")));
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
            m.insert("display".into(), (t.display)());
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
            m.insert("display".into(), (t.display)());
            m.insert("context".into(), (t.context)());
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

fn tool_stream_body(id: &str, stream: &str, text: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("tool.stream".into()));
    m.insert("id".into(), Value::String(id.to_owned()));
    m.insert("stream".into(), Value::String(stream.to_owned()));
    m.insert("text".into(), Value::String(text.to_owned()));
    m
}

fn tool_result_ok_body(id: &str, output: Value) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("tool.result".into()));
    m.insert("id".into(), Value::String(id.to_owned()));
    m.insert("output".into(), output);
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
