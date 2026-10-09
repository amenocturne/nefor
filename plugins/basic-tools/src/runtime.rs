// basic-tools — NCP v0.1 plugin: file, image, and process capabilities.
//
// This plugin executes capabilities; it does not define model-facing tools.
// The Lua tool definitions in `lua/libs/basic-tools` (wrapped around this
// process by `libs.compositors.tools`) own names, descriptions, parameter
// schemas, display contracts, argument validation, and gate advertisement.
//
// Wire contract:
//
// - Inbound `basic-tools.capability.invoke { id, capability, args }` runs one
//   capability. `basic-tools.capability.cancel { id }` kills an in-flight
//   process capability's process group.
// - Outbound `capability.result { id, output }` on success or
//   `capability.result { id, error }` on failure; process capabilities also
//   emit `capability.stream { id, stream, text }` while running.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use nefor_plugin_sdk::{await_ready_ok, spawn_stdin_reader, spawn_stdout_writer, TransportError};
use nefor_protocol::{Body, Envelope, PluginOutgoing, SystemBody};
use serde_json::{Map, Value};
use tokio::sync::{mpsc, oneshot};

use crate::tools::{process, process_exec, run_capability, shell_script, CAPABILITIES};

const CHANNEL_CAP: usize = 256;

/// In-flight cancellable invocations, keyed by invoke id. Only process
/// capabilities register here because they hold killable OS process groups; a
/// `basic-tools.capability.cancel { id }` fires the matching sender, which trips the
/// running task's cancel arm and kills the child's process group.
type Cancels = Arc<Mutex<HashMap<String, oneshot::Sender<()>>>>;

/// NCP version this plugin speaks.
const PROTOCOL_VERSION: &str = "0.1";

/// Plugin name on the bus. Must match the `name` the starter spawns us under:
/// capability requests are addressed as `<name>.capability.invoke`.
/// Hard-coded here rather than parsed from CLI: the kind-prefix is part of the
/// wire contract, not a per-spawn detail.
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

    parse_args();

    if let Err(e) = run().await {
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

/// Parse `--read-file-max-bytes` and configure the read_file cap.
fn parse_args() {
    use clap::{Arg, Command};
    let matches = Command::new("basic-tools")
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
}

async fn run() -> Result<(), TransportError> {
    let (out_tx, _writer_handle) = spawn_stdout_writer(CHANNEL_CAP);
    let (in_tx, mut in_rx) = mpsc::channel::<Result<Envelope, TransportError>>(CHANNEL_CAP);
    let _reader_handle = spawn_stdin_reader(in_tx);

    send_ready(&out_tx).await?;
    let engine_version = await_ready_ok(&mut in_rx).await?;
    tracing::info!(engine_version = %engine_version, capabilities = CAPABILITIES.len(), "ready");

    send_event(&out_tx, hello_body()).await?;

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
                            if is_kind(map, INVOKE_SUFFIX) {
                                tasks.retain(|task: &tokio::task::JoinHandle<()>| !task.is_finished());
                                tasks.push(spawn_invoke(out_tx, map, &cancels));
                            } else if is_kind(map, CANCEL_SUFFIX) {
                                handle_cancel(&cancels, map);
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

const INVOKE_SUFFIX: &str = "capability.invoke";
const CANCEL_SUFFIX: &str = "capability.cancel";

fn is_kind(body: &Map<String, Value>, suffix: &str) -> bool {
    body.get("kind")
        .and_then(Value::as_str)
        .and_then(|kind| kind.strip_prefix(PLUGIN_NAME))
        .and_then(|rest| rest.strip_prefix('.'))
        == Some(suffix)
}

/// Handle a `basic-tools.capability.cancel { id }` (the Lua tool wrapper
/// forwards the gate's cancel for the in-flight invocation id). Fire the matching sender so
/// the running process task kills its process group and returns
/// [`ToolError::ProcessCancelled`]. No entry means the invocation already
/// finished (or was never a process capability) — a clean no-op.
fn handle_cancel(cancels: &Cancels, body: &Map<String, Value>) {
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
            tracing::info!(id = %id, "capability.cancel: signalled in-flight process to terminate");
        }
        None => {
            tracing::info!(id = %id, "capability.cancel: no in-flight cancellable invocation; no-op");
        }
    }
}

fn spawn_invoke(
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
        .get("capability")
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
        if let Err(e) = handle_invoke(&out_tx, &body, cancel_rx).await {
            tracing::error!(error = %e, "capability.invoke task failed");
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

async fn handle_invoke(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
    cancel: Option<oneshot::Receiver<()>>,
) -> Result<(), TransportError> {
    // `id` is the caller's correlation token; without it there is nobody to
    // answer.
    let id = match body.get("id").and_then(Value::as_str) {
        Some(s) => s.to_owned(),
        None => {
            tracing::warn!("capability.invoke missing required string field `id`; dropping");
            return Ok(());
        }
    };

    let name = match body
        .get("capability")
        .and_then(Value::as_str)
        .filter(|name| CAPABILITIES.contains(name))
    {
        Some(s) => s.to_owned(),
        None => {
            send_event(
                out_tx,
                result_error_body(
                    &id,
                    "capability.invoke requires `capability` naming a basic-tools capability",
                ),
            )
            .await?;
            return Ok(());
        }
    };

    let args = body
        .get("args")
        .cloned()
        .unwrap_or(Value::Object(Map::new()));

    // Process capabilities share streaming and cancellation mechanics.
    let outcome = if name == process_exec::NAME || name == shell_script::NAME {
        let (stream_tx, mut stream_rx) = mpsc::unbounded_channel();
        let stream_out = out_tx.clone();
        let stream_id = id.clone();
        let forward = tokio::spawn(async move {
            while let Some(chunk) = stream_rx.recv().await {
                let (stream, bytes) = match chunk {
                    process::StreamChunk::Stdout(bytes) => ("stdout", bytes),
                    process::StreamChunk::Stderr(bytes) => ("stderr", bytes),
                };
                let text = String::from_utf8_lossy(&bytes);
                let _ = send_event(&stream_out, stream_body(&stream_id, stream, &text)).await;
            }
        });
        let result = if name == process_exec::NAME {
            process_exec::run_cancellable_streaming(&args, cancel, Some(stream_tx)).await
        } else {
            shell_script::run_cancellable_streaming(&args, cancel, Some(stream_tx)).await
        };
        let _ = forward.await;
        result
    } else {
        run_capability(&name, &args).await
    };
    match outcome {
        Ok(output) => {
            send_event(out_tx, result_ok_body(&id, output)).await?;
        }
        Err(e) => {
            send_event(out_tx, result_error_body(&id, &e.to_string())).await?;
        }
    }
    Ok(())
}

// ---- static body constructors ----------------------------------------------

fn hello_body() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String(format!("{PLUGIN_NAME}.hello")));
    m.insert("version".into(), Value::String(PLUGIN_VERSION.into()));
    m
}

fn stream_body(id: &str, stream: &str, text: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("capability.stream".into()));
    m.insert("id".into(), Value::String(id.to_owned()));
    m.insert("stream".into(), Value::String(stream.to_owned()));
    m.insert("text".into(), Value::String(text.to_owned()));
    m
}

fn result_ok_body(id: &str, output: Value) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("capability.result".into()));
    m.insert("id".into(), Value::String(id.to_owned()));
    m.insert("output".into(), output);
    m
}

fn result_error_body(id: &str, message: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("capability.result".into()));
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
