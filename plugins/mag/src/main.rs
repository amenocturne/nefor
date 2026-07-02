//! mag — NCP v0.1 plugin hosting the MAG actor-kernel runtime.
//!
//! Skeleton stage: completes the NCP ready handshake, hosts a Lua VM that
//! loads the (stub) kernel from the config-resolved Lua path, and answers a
//! trivial `mag.ping` so liveness is testable. No kernel behavior and no
//! `nefor-mag` evaluator calls yet — those land with the runtime.
//!
//! Layering mirrors the sibling plugins (`reasoner-graph`, `tool-gate`):
//! - `main.rs` — entry, handshake, dispatch loop, bus body encoding.
//! - `kernel.rs` — the embedded Lua VM and kernel loading.
//! - `error.rs` — `MagError` domain error hierarchy.

mod error;
mod kernel;

use std::path::PathBuf;

use nefor_plugin_sdk::{await_ready_ok, spawn_stdin_reader, spawn_stdout_writer, TransportError};
use nefor_protocol::{Body, Envelope, PluginOutgoing, SystemBody};
use serde_json::{Map, Value};
use tokio::sync::mpsc;

use crate::error::MagError;
use crate::kernel::LuaHost;

/// Outbound/inbound channel capacity for the stdio transport tasks.
const CHANNEL_CAP: usize = 128;

/// NCP version this plugin speaks.
const PROTOCOL_VERSION: &str = "0.1";

/// Plugin name (bus identity is assigned by the engine from spawn-config;
/// this is what we prefix our own event kinds with).
const PLUGIN_NAME: &str = "mag";

/// Plugin version, advertised in `mag.hello`.
const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Liveness ping we answer, and the reply kind.
const PING_KIND: &str = "mag.ping";
const PONG_KIND: &str = "mag.pong";

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
        tracing::error!(error = %e, "mag exited with error");
        eprintln!("mag: {e}");
        std::process::exit(1);
    }
    // Force exit: `tokio::io::stdin()` parks a non-cancellable blocking
    // reader thread; letting the runtime drop naturally would hang the
    // process. Same fix as reasoner-graph / nefor-tui.
    std::process::exit(0);
}

async fn run() -> Result<(), MagError> {
    let (out_tx, _writer_handle) = spawn_stdout_writer(CHANNEL_CAP);
    let (in_tx, mut in_rx) = mpsc::channel::<Result<Envelope, TransportError>>(CHANNEL_CAP);
    let _reader_handle = spawn_stdin_reader(in_tx);

    send_ready(&out_tx).await?;
    let engine_version = await_ready_ok(&mut in_rx).await?;
    tracing::info!(engine_version = %engine_version, "ready");

    // Host the Lua VM and load the kernel before advertising liveness, so
    // `mag.hello` truthfully reports the loaded kernel. `host` is held for
    // the whole session — the VM is the kernel's entire world.
    let kernel_path = resolve_kernel_path()?;
    tracing::info!(path = %kernel_path.display(), "loading mag kernel");
    let host = LuaHost::load_kernel(&kernel_path)?;

    send_event(&out_tx, hello_body(host.kernel_name().as_deref())).await?;

    run_dispatch_loop(&out_tx, &mut in_rx).await?;

    let _ = out_tx.send(PluginOutgoing::event(goodbye_body())).await;
    Ok(())
}

/// Resolve where the kernel entry Lua lives, highest precedence first:
///
/// 1. `--kernel <path>` (or `-k`) argv — how the starter passes it.
/// 2. `NEFOR_DEV_DIR/starter/mag-kernel/init.lua` — in-checkout dev mode.
/// 3. `NEFOR_CONFIG_DIR/mag-kernel/init.lua` — installed-config default.
///
/// This mirrors the ecosystem's `NEFOR_DEV_DIR`-first search convention.
/// The engine exports `NEFOR_CONFIG_DIR` into every spawned plugin's env,
/// so (3) works even when the starter passes no explicit flag.
fn resolve_kernel_path() -> Result<PathBuf, MagError> {
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        if (args[i] == "--kernel" || args[i] == "-k") && i + 1 < args.len() {
            return Ok(PathBuf::from(&args[i + 1]));
        }
        i += 1;
    }

    if let Some(dev) = std::env::var_os("NEFOR_DEV_DIR") {
        let candidate = PathBuf::from(dev).join("starter/mag-kernel/init.lua");
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    if let Some(cfg) = std::env::var_os("NEFOR_CONFIG_DIR") {
        let candidate = PathBuf::from(cfg).join("mag-kernel/init.lua");
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    Err(MagError::NoKernelPath)
}

async fn run_dispatch_loop(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    in_rx: &mut mpsc::Receiver<Result<Envelope, TransportError>>,
) -> Result<(), MagError> {
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
                            handle_event(out_tx, map).await?;
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

/// Handle one inbound event body. The skeleton only answers `mag.ping`;
/// everything else on the broadcast bus is not ours and drops silently.
async fn handle_event(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
) -> Result<(), MagError> {
    let kind = match body.get("kind").and_then(Value::as_str) {
        Some(k) => k,
        None => return Ok(()),
    };
    if kind == PING_KIND {
        let in_reply_to = body.get("id").and_then(Value::as_str);
        send_event(out_tx, pong_body(in_reply_to)).await?;
    }
    Ok(())
}

// ---- static body constructors ----------------------------------------------

fn hello_body(kernel: Option<&str>) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String(format!("{PLUGIN_NAME}.hello")));
    m.insert("version".into(), Value::String(PLUGIN_VERSION.into()));
    if let Some(k) = kernel {
        m.insert("kernel".into(), Value::String(k.to_owned()));
    }
    m
}

fn pong_body(in_reply_to: Option<&str>) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String(PONG_KIND.into()));
    if let Some(id) = in_reply_to {
        m.insert("in_reply_to".into(), Value::String(id.to_owned()));
    }
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
) -> Result<(), MagError> {
    out_tx
        .send(PluginOutgoing::event(body))
        .await
        .map_err(|_| TransportError::WriterClosed)?;
    Ok(())
}

async fn send_ready(out_tx: &mpsc::Sender<PluginOutgoing>) -> Result<(), MagError> {
    out_tx
        .send(PluginOutgoing::system(SystemBody::Ready {
            protocol_version: PROTOCOL_VERSION.into(),
        }))
        .await
        .map_err(|_| TransportError::WriterClosed)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_body_advertises_version_and_kernel() {
        let b = hello_body(Some("mag-kernel"));
        assert_eq!(b.get("kind").and_then(Value::as_str), Some("mag.hello"));
        assert_eq!(
            b.get("version").and_then(Value::as_str),
            Some(PLUGIN_VERSION)
        );
        assert_eq!(b.get("kernel").and_then(Value::as_str), Some("mag-kernel"));
    }

    #[test]
    fn hello_body_omits_kernel_when_absent() {
        let b = hello_body(None);
        assert!(b.get("kernel").is_none());
    }

    #[test]
    fn pong_body_echoes_in_reply_to() {
        let b = pong_body(Some("ping-1"));
        assert_eq!(b.get("kind").and_then(Value::as_str), Some("mag.pong"));
        assert_eq!(b.get("in_reply_to").and_then(Value::as_str), Some("ping-1"));
    }

    #[test]
    fn goodbye_body_carries_reason() {
        let b = goodbye_body();
        assert_eq!(b.get("kind").and_then(Value::as_str), Some("mag.goodbye"));
        assert!(b.get("reason").and_then(Value::as_str).is_some());
    }
}
