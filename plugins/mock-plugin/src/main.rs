//! mock-plugin — Lua-scriptable NCP peer plugin.
//!
//! A test/dev plugin that speaks the [current NCP behavior](../../../docs/protocol.md)
//! over stdio and drives its behaviour from a user-supplied Lua script.
//! Useful for exercising the engine and other plugins without having to
//! ship real functionality.

mod error;
mod lua_host;
mod state;

use std::path::PathBuf;

use clap::Parser;
use nefor_plugin_sdk::{
    await_ready_ok, checked_send, spawn_stdin_reader, spawn_stdout_writer, TransportError,
};
use nefor_protocol::{Body, Envelope, PluginOutgoing, SystemBody};
use tokio::sync::mpsc;

use crate::error::MockError;
use crate::lua_host::LuaHost;

const CHANNEL_CAP: usize = 128;

/// Hardcoded plugin identity. Exposed to Lua as `nefor.name`.
pub const PLUGIN_NAME: &str = "mock-plugin";

/// NCP version this plugin speaks.
pub const PROTOCOL_VERSION: &str = "0.1";

/// Lua-scriptable NCP peer plugin for test/dev.
#[derive(Debug, Parser)]
#[command(name = "mock-plugin", version, about, long_about)]
struct Args {
    /// Path to the Lua script that defines this plugin's handlers.
    #[arg(long, value_name = "PATH")]
    script: PathBuf,

    /// Shared Lua tree prepended to `package.path` (`<dir>/?.lua`,
    /// `<dir>/?/init.lua`), so the script can `require("libs.mock-provider")`.
    #[arg(long, value_name = "DIR")]
    lua_root: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let code = match run().await {
        Ok(()) => 0,
        Err(e) => {
            tracing::error!(error = %e, "mock-plugin exited with error");
            eprintln!("mock-plugin: {e}");
            1
        }
    };
    // Tokio stdin uses a non-cancellable blocking read. Tasks have been
    // reclaimed by run; do not let that read defer process-loss observation.
    std::process::exit(code);
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
    let (out_tx, writer_handle) = spawn_stdout_writer(CHANNEL_CAP);
    let (in_tx, mut in_rx) = mpsc::channel::<Result<Envelope, TransportError>>(CHANNEL_CAP);
    let mut reader_handle = spawn_stdin_reader(in_tx);

    // Lua host. Load the script before the handshake so we surface
    // syntax errors immediately and never talk to the engine with a
    // broken script.
    let host = LuaHost::new(PLUGIN_NAME, out_tx.clone())?;
    if let Some(lua_root) = &args.lua_root {
        host.prepend_package_path(lua_root)?;
    }
    tokio::select! {
        result = host.exec_script(&script_name, &script_src) => result?,
        _ = &mut reader_handle => return Err(MockError::InputEnded),
        _ = out_tx.closed() => return Err(TransportError::WriterClosed.into()),
    }

    // Handshake.
    send_ready(&out_tx).await?;
    let engine_version = tokio::select! {
        result = await_ready_ok(&mut in_rx) => result?,
        _ = out_tx.closed() => return Err(TransportError::WriterClosed.into()),
    };
    tracing::info!(engine_version = %engine_version, "ready");
    tokio::select! {
        result = host.on_ready_ok() => result?,
        _ = &mut reader_handle => return Err(MockError::InputEnded),
        _ = out_tx.closed() => return Err(TransportError::WriterClosed.into()),
    }

    // Main dispatch loop.
    run_dispatch_loop(&host, &mut in_rx, &out_tx, &mut reader_handle).await?;

    // Shutdown path: fire the Lua handler, then let tasks drop.
    tokio::select! {
        result = host.on_shutdown() => result?,
        _ = out_tx.closed() => return Err(TransportError::WriterClosed.into()),
    }
    host.release_script()?;
    drop(host);
    drop(out_tx);
    tokio::time::timeout(std::time::Duration::from_secs(2), writer_handle)
        .await
        .map_err(|_| MockError::WriterDrain)?
        .map_err(MockError::Task)?;
    Ok(())
}

async fn run_dispatch_loop(
    host: &LuaHost,
    in_rx: &mut mpsc::Receiver<Result<Envelope, TransportError>>,
    out_tx: &mpsc::Sender<PluginOutgoing>,
    reader_handle: &mut tokio::task::JoinHandle<()>,
) -> Result<(), MockError> {
    let mut tasks = tokio::task::JoinSet::new();
    let mut outcome = async {
    loop {
        tokio::select! {
            _ = out_tx.closed() => return Err(TransportError::WriterClosed.into()),
            completed = tasks.join_next(), if !tasks.is_empty() => {
                match completed {
                    Some(Ok(result)) => result?,
                    Some(Err(error)) => return Err(MockError::Task(error)),
                    None => {}
                }
            }
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
                                // Streaming dispatch (`<prefix>.completion.request`)
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
                                // Every other envelope dispatches inline.
                                // Direct completion requests carry their
                                // complete input snapshot, so they do not
                                // depend on earlier create/append mutations.
                                // Spawning only that request kind keeps the
                                // input loop available for its correlated
                                // cancellation while preserving ordered
                                // dispatch for ordinary event handlers.
                                //
                                // Custom scripts whose handlers yield opt in
                                // through the same `*.completion.request`
                                // shape used by `libs.mock-provider`.
                                let kind_is_complete = map
                                    .get("kind")
                                    .and_then(|v| v.as_str())
                                    .is_some_and(|k| k.ends_with(".completion.request"));
                                if kind_is_complete {
                                    let host_clone = host.clone();
                                    tasks.spawn(async move {
                                        host_clone.dispatch_event(&env).await
                                    });
                                } else {
                                    let dispatch = host.dispatch_event(&env);
                                    tokio::pin!(dispatch);
                                    loop {
                                        tokio::select! {
                                            result = &mut dispatch => { result?; break; }
                                            _ = &mut *reader_handle => return Err(MockError::InputEnded),
                                            _ = out_tx.closed() => return Err(TransportError::WriterClosed.into()),
                                            completed = tasks.join_next(), if !tasks.is_empty() => {
                                                match completed {
                                                    Some(Ok(result)) => result?,
                                                    Some(Err(error)) => return Err(MockError::Task(error)),
                                                    None => {}
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Some(Err(TransportError::Parse(e))) => {
                        tracing::error!(error = %e, "stdin parse error; dropping line");
                    }
                    Some(Err(e)) => return Err(e.into()),
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
    }.await;
    tasks.abort_all();
    while let Some(completed) = tasks.join_next().await {
        match completed {
            Ok(Err(error)) if outcome.is_ok() => outcome = Err(error),
            Err(error) if error.is_panic() => return Err(MockError::Task(error)),
            _ => {}
        }
    }
    outcome
}

async fn send_ready(out_tx: &mpsc::Sender<PluginOutgoing>) -> Result<(), MockError> {
    checked_send(
        out_tx,
        PluginOutgoing::system(SystemBody::Ready {
            protocol_version: PROTOCOL_VERSION.into(),
        }),
    )
    .await
    .map_err(MockError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nefor_protocol::{PluginName, Timestamp};

    #[tokio::test]
    async fn writer_closure_reclaims_script_task_without_inbound_traffic() {
        let (out_tx, mut out_rx) = mpsc::channel(4);
        let host = LuaHost::new(PLUGIN_NAME, out_tx.clone()).unwrap();
        host.exec_script(
            "closed",
            r#"
            nefor.on("peer.completion.request", function()
                nefor.emit("accepted")
                nefor.sleep(60000)
                nefor.emit("false-success")
            end)
        "#,
        )
        .await
        .unwrap();
        host.on_ready_ok().await.unwrap();
        let (in_tx, mut in_rx) = mpsc::channel(4);
        let body = serde_json::json!({"kind":"peer.completion.request"})
            .as_object()
            .unwrap()
            .clone();
        in_tx
            .send(Ok(Envelope::event(
                PluginName::new("peer").unwrap(),
                Timestamp::now(),
                body,
            )))
            .await
            .unwrap();
        let mut reader = tokio::spawn(std::future::pending::<()>());
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(
                run_dispatch_loop(&host, &mut in_rx, &out_tx, &mut reader),
                async {
                    out_rx.recv().await.unwrap();
                    drop(out_rx);
                }
            )
            .0
        })
        .await
        .unwrap();
        assert!(matches!(
            result,
            Err(MockError::Transport(TransportError::WriterClosed))
        ));
        // The loop returns only after aborting and joining its owned scripts.
        reader.abort();
        let _ = reader.await;
        drop(in_tx);
    }
}
