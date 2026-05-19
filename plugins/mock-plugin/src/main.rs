//! mock-plugin — Lua-scriptable NCP v0.1 peer plugin.
//!
//! A test/dev plugin that speaks [NCP v0.1](../../../protocol/v0.1/spec.md)
//! over stdio and drives its behaviour from a user-supplied Lua script.
//! Useful for exercising the engine and other plugins without having to
//! ship real functionality.

mod error;
mod lua_host;
mod state;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use nefor_plugin_sdk::{spawn_stdin_reader, spawn_stdout_writer, await_ready_ok, TransportError};
use nefor_protocol::{Body, Envelope, PluginOutgoing, SystemBody};
use tokio::sync::mpsc;

use crate::error::MockError;
use crate::lua_host::LuaHost;

const CHANNEL_CAP: usize = 128;

/// Hardcoded plugin identity. Exposed to Lua as `nefor.name`. If callers
/// need multiple `mock-plugin` instances under distinct wire identities,
/// extend this with a `--name` flag.
pub const PLUGIN_NAME: &str = "mock-plugin";

/// NCP version this plugin speaks.
pub const PROTOCOL_VERSION: &str = "0.1";

/// Lua-scriptable NCP v0.1 peer plugin for test/dev.
#[derive(Debug, Parser)]
#[command(name = "mock-plugin", version, about, long_about)]
struct Args {
    /// Path to the Lua script that defines this plugin's handlers.
    #[arg(long, value_name = "PATH")]
    script: PathBuf,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "mock-plugin exited with error");
            eprintln!("mock-plugin: {e}");
            ExitCode::from(1)
        }
    }
}

async fn run() -> Result<(), MockError> {
    let args = Args::parse();

    if !args.script.exists() {
        return Err(MockError::ScriptNotFound(args.script));
    }
    let script_src = std::fs::read_to_string(&args.script).map_err(MockError::ScriptRead)?;
    let script_name = args
        .script
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("script.lua")
        .to_string();

    // Transport tasks.
    let (out_tx, _writer_handle) = spawn_stdout_writer(CHANNEL_CAP);
    let (in_tx, mut in_rx) = mpsc::channel::<Result<Envelope, TransportError>>(CHANNEL_CAP);
    let _reader_handle = spawn_stdin_reader(in_tx);

    // Lua host. Load the script before the handshake so we surface
    // syntax errors immediately and never talk to the engine with a
    // broken script.
    let host = LuaHost::new(PLUGIN_NAME, out_tx.clone())?;
    host.exec_script(&script_name, &script_src).await?;

    // Handshake.
    send_ready(&out_tx).await?;
    let engine_version = await_ready_ok(&mut in_rx).await?;
    tracing::info!(engine_version = %engine_version, "ready");
    host.on_ready_ok().await?;

    // Main dispatch loop.
    run_dispatch_loop(&host, &mut in_rx).await?;

    // Shutdown path: fire the Lua handler, then let tasks drop.
    host.on_shutdown().await?;
    Ok(())
}

async fn run_dispatch_loop(
    host: &LuaHost,
    in_rx: &mut mpsc::Receiver<Result<Envelope, TransportError>>,
) -> Result<(), MockError> {
    loop {
        tokio::select! {
            maybe = in_rx.recv() => {
                match maybe {
                    Some(Ok(env)) => {
                        match &env.body {
                            Body::System(SystemBody::Shutdown { .. }) => {
                                tracing::info!("shutdown received; running on_shutdown");
                                return Ok(());
                            }
                            Body::System(_) => {
                                // Unexpected system messages (ready_ok after
                                // we already handshook, stray errors) —
                                // log and keep going.
                                tracing::warn!(?env, "unexpected system envelope after handshake");
                            }
                            Body::Event(map) => {
                                // Streaming dispatch (`<prefix>.chat.complete`)
                                // gets its own tokio task because the
                                // handler awaits `nefor.sleep` between
                                // chunks; awaiting inline would block the
                                // dispatch loop for the full stream and
                                // prevent a follow-up `<prefix>.interrupt`
                                // from landing in time. Spawning lets the
                                // runtime poll other dispatch tasks during
                                // the streaming handler's yields; mlua's
                                // `send` feature serialises real Lua work
                                // on a single VM mutex so the interrupt
                                // handler interleaves cleanly at the
                                // streaming handler's `await` suspension
                                // points (sets a Lua-side cancel flag the
                                // streaming loop checks on each chunk
                                // boundary).
                                //
                                // Non-streaming envelopes (`chat.create`,
                                // `chat.append`, every other event with no
                                // internal `await`) dispatch inline. Prior
                                // shape spawned every event uniformly, but
                                // tokio::spawn doesn't guarantee tasks
                                // start in spawn order, and the order in
                                // which spawned tasks acquire mlua's VM
                                // mutex isn't guaranteed either — under
                                // post batch-protocol refactor's batched
                                // delivery the engine hands the binary
                                // [chat.create, chat.append, chat.complete]
                                // back-to-back and a later `chat.complete`
                                // task could win the VM mutex ahead of an
                                // earlier `chat.append`, leaving the
                                // complete handler with no user message in
                                // history. Awaiting non-streaming events
                                // inline removes the race for everything
                                // that doesn't yield.
                                //
                                // Custom scripts whose handlers do yield
                                // (e.g. tests that emit `nefor.sleep` from
                                // a non-chat.complete kind) need to opt in
                                // by using a `*.chat.complete` kind for the
                                // streaming handler — same shape the real
                                // provider uses.
                                let kind_is_complete = map
                                    .get("kind")
                                    .and_then(|v| v.as_str())
                                    .is_some_and(|k| k.ends_with(".chat.complete"));
                                if kind_is_complete {
                                    let host_clone = host.clone();
                                    tokio::spawn(async move {
                                        if let Err(e) = host_clone.dispatch_event(&env).await {
                                            tracing::error!(error = %e, "dispatch task failed");
                                        }
                                    });
                                } else if let Err(e) = host.dispatch_event(&env).await {
                                    tracing::error!(error = %e, "dispatch task failed");
                                }
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

async fn send_ready(out_tx: &mpsc::Sender<PluginOutgoing>) -> Result<(), MockError> {
    out_tx
        .send(PluginOutgoing::system(SystemBody::Ready {
            protocol_version: PROTOCOL_VERSION.into(),
        }))
        .await
        .map_err(|_| MockError::Transport(TransportError::WriterClosed))
}
