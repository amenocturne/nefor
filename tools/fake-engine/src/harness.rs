//! The harness itself: spawn the plugin, do the ready handshake, then
//! concurrently pump messages in both directions.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use nefor_protocol::{
    Body, Envelope, MessageKind, PluginName, PluginOutgoing, SystemBody, Timestamp,
};

// Plugin-outgoing parser is used on the handshake line (plugin sends
// {type, body} — no from/ts); the full-envelope parser is used for the
// logging reader (where the plugin might be sending replies).
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, Mutex};

use crate::log;
use crate::script::ScriptStep;

/// Version string reported in `ready_ok.engine_version`.
pub const ENGINE_VERSION: &str = "fake-0.1.0";
/// Grace window for the ctrl-c triggered shutdown.
const CTRL_C_GRACE: Duration = Duration::from_millis(2000);

/// High-level entry point. Spawns the plugin, performs the handshake,
/// plays back the script (if any), and relays messages until the plugin
/// exits or ctrl-c fires.
///
/// Returns the plugin's exit status code on normal exit; `anyhow::Result`
/// wraps unrecoverable harness-side failures (spawn failed, handshake
/// rejected).
pub async fn run(plugin_path: &Path, script: Option<Vec<ScriptStep>>) -> Result<i32> {
    let mut child = spawn_plugin(plugin_path)?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("plugin stdin was not piped"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("plugin stdout was not piped"))?;

    // Stdin is shared: the handshake writes to it, then the optional
    // script playback task and the ctrl-c path both need access. Wrapping
    // in Arc<Mutex<>> keeps the ownership simple — writes are short and
    // coarse-grained so contention is negligible.
    let stdin = Arc::new(Mutex::new(stdin));

    let mut reader = BufReader::new(stdout).lines();

    // --- Handshake: read first line, expect ready, respond with ready_ok.
    let ready_line = reader
        .next_line()
        .await
        .context("reading ready line from plugin")?
        .ok_or_else(|| anyhow!("plugin closed stdout before sending ready"))?;

    let plugin_label = plugin_name_from_command(plugin_path);
    {
        let mut s = stdin.lock().await;
        handle_ready(&ready_line, &mut s).await?;
    }
    eprintln!("-- fake-engine: ready plugin {plugin_label:?}");

    // --- Reader task: logs every line the plugin emits to our stderr.
    // Plugins send `{type, body}` (PluginOutgoing), not fully-stamped
    // envelopes. We stamp the plugin's label and a fresh timestamp so
    // the log format stays uniform; this stamping is display-only, not
    // a claim of authority (the real engine stamps from spawn-config).
    let (reader_done_tx, mut reader_done_rx) = mpsc::channel::<()>(1);
    let label_for_reader = plugin_label.clone();
    let reader_task = tokio::spawn(async move {
        let mut lines = reader;
        while let Ok(Some(raw)) = lines.next_line().await {
            match PluginOutgoing::parse_line(&raw) {
                Ok(out) => {
                    let env = stamp_for_display(&label_for_reader, out);
                    eprintln!("{}", log::format_envelope(&env));
                }
                Err(e) => eprintln!(
                    "{}",
                    log::format_unparseable(&raw, &format!("parse error: {e}"))
                ),
            }
        }
        let _ = reader_done_tx.send(()).await;
    });

    // --- Optional script playback task.
    let mut script_done_rx = if let Some(steps) = script {
        let (tx, rx) = mpsc::channel::<()>(1);
        let stdin_for_script = Arc::clone(&stdin);
        tokio::spawn(async move {
            if let Err(e) = play_script(steps, &stdin_for_script).await {
                eprintln!("-- fake-engine: script playback error: {e}");
            }
            let _ = tx.send(()).await;
        });
        Some(rx)
    } else {
        None
    };

    // --- Main select loop: wait for stdout-close, process exit, ctrl-c,
    // or script completion.
    let exit_code = loop {
        tokio::select! {
            _ = reader_done_rx.recv() => {
                break wait_child(&mut child).await;
            }
            status = child.wait() => {
                break status.ok().and_then(|s| s.code()).unwrap_or(0);
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!(
                    "-- fake-engine: ctrl-c; sending shutdown with grace {}ms",
                    CTRL_C_GRACE.as_millis()
                );
                graceful_shutdown(&stdin, &mut child).await;
                break wait_child(&mut child).await;
            }
            _ = option_recv(script_done_rx.as_mut()) => {
                eprintln!("-- fake-engine: script playback complete");
                script_done_rx = None;
            }
        }
    };

    reader_task.abort();
    eprintln!("-- fake-engine: plugin exited with code {exit_code}");
    Ok(exit_code)
}

/// Select-compatible helper: if the receiver is Some, await it; if None,
/// never resolve.
async fn option_recv(rx: Option<&mut mpsc::Receiver<()>>) -> Option<()> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

async fn graceful_shutdown(stdin: &Arc<Mutex<ChildStdin>>, child: &mut Child) {
    {
        let mut s = stdin.lock().await;
        if let Err(e) = send_shutdown(&mut s, CTRL_C_GRACE).await {
            eprintln!("-- fake-engine: failed to send shutdown: {e}");
        }
    }
    tokio::select! {
        _ = tokio::time::sleep(CTRL_C_GRACE) => {
            eprintln!("-- fake-engine: grace window expired; killing plugin");
            let _ = child.kill().await;
        }
        status = child.wait() => {
            eprintln!("-- fake-engine: plugin exited during grace window: {status:?}");
        }
    }
}

async fn wait_child(child: &mut Child) -> i32 {
    match child.wait().await {
        Ok(status) => status.code().unwrap_or(0),
        Err(_) => 0,
    }
}

fn spawn_plugin(plugin_path: &Path) -> Result<Child> {
    Command::new(plugin_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Inherit stderr: plugin's log channel stays visible alongside ours.
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("spawning plugin {plugin_path:?}"))
}

/// Parse the plugin's first line, validate it as a `ready`, and send
/// `ready_ok` in return. The plugin sends a `PluginOutgoing` shape
/// ({type, body} — no `from`/`ts`), so we use the outgoing parser here.
async fn handle_ready(line: &str, stdin: &mut ChildStdin) -> Result<()> {
    let out = PluginOutgoing::parse_line(line)
        .with_context(|| format!("parsing ready line: {line:?}"))?;
    match (&out.kind, &out.body) {
        (MessageKind::System, Body::System(SystemBody::Ready { protocol_version })) => {
            if protocol_version != "0.1" {
                return Err(anyhow!(
                    "plugin reported protocol_version {protocol_version:?}, fake-engine speaks 0.1"
                ));
            }
        }
        _ => {
            return Err(anyhow!(
                "first message was not a system ready: got {:?}",
                out
            ));
        }
    };

    let ok = Envelope::system(
        PluginName::engine(),
        Timestamp::now(),
        SystemBody::ReadyOk {
            engine_version: ENGINE_VERSION.into(),
        },
    );
    write_line(stdin, &ok.to_line()).await?;
    Ok(())
}

/// Derive a display name for the plugin from its binary path. The
/// real engine assigns names from spawn-config; the fake-engine doesn't
/// have that, so it falls back to the binary's file stem.
fn plugin_name_from_command(plugin_path: &Path) -> String {
    plugin_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("plugin")
        .to_owned()
}

/// Stamp a plugin-outgoing message with a display-only identity + timestamp,
/// so the log output shows the same shape the real engine would produce.
fn stamp_for_display(label: &str, out: PluginOutgoing) -> Envelope {
    let from = PluginName::new(label).unwrap_or_else(|_| PluginName::engine());
    Envelope {
        kind: out.kind,
        from,
        ts: Timestamp::now(),
        body: out.body,
    }
}

async fn send_shutdown(stdin: &mut ChildStdin, grace: Duration) -> Result<()> {
    let env = Envelope::system(
        PluginName::engine(),
        Timestamp::now(),
        SystemBody::Shutdown {
            reason: Some("harness ctrl_c".into()),
            grace_ms: Some(grace.as_millis() as u64),
        },
    );
    write_line(stdin, &env.to_line()).await
}

async fn play_script(steps: Vec<ScriptStep>, stdin: &Arc<Mutex<ChildStdin>>) -> Result<()> {
    for step in steps {
        match step {
            ScriptStep::Sleep(d) => tokio::time::sleep(d).await,
            ScriptStep::SendVerbatim(env) => {
                let mut s = stdin.lock().await;
                write_line(&mut s, &env.to_line()).await?;
            }
            ScriptStep::SendStamped(out) => {
                let env = stamp(out);
                let mut s = stdin.lock().await;
                write_line(&mut s, &env.to_line()).await?;
            }
        }
    }
    Ok(())
}

/// Stamp a plugin-outgoing with `from: "engine"` (the reserved identity for
/// the party on the other end of a plugin connection) and a fresh `ts`.
///
/// Exposed for unit tests; callers inside the harness use it via
/// [`play_script`].
pub fn stamp(out: PluginOutgoing) -> Envelope {
    // The fake-engine is impersonating an engine; §3 reserves
    // from:"engine" for engine-authored messages, which is exactly the
    // role we're playing from the plugin's perspective.
    Envelope {
        kind: out.kind,
        from: PluginName::engine(),
        ts: Timestamp::now(),
        body: out.body,
    }
}

async fn write_line(stdin: &mut ChildStdin, line: &str) -> Result<()> {
    stdin
        .write_all(line.as_bytes())
        .await
        .context("writing to plugin stdin")?;
    stdin.write_all(b"\n").await.context("writing newline")?;
    stdin.flush().await.context("flushing plugin stdin")
}

#[cfg(test)]
mod tests {
    use super::*;
    use nefor_protocol::PluginOutgoing;

    #[test]
    fn stamp_sets_from_engine_and_valid_ts() {
        let out = PluginOutgoing::system(SystemBody::Shutdown {
            reason: None,
            grace_ms: Some(500),
        });
        let env = stamp(out);
        assert_eq!(env.from.as_str(), "engine");
        assert_eq!(env.kind, MessageKind::System);
        // `ts` is current wall clock; round-trip through ISO-8601 to
        // confirm the stamp is a valid Timestamp.
        let iso = env.ts.to_iso8601();
        let back = Timestamp::parse(&iso).expect("ts round-trips");
        assert_eq!(back, env.ts);
    }

    #[test]
    fn stamp_preserves_body() {
        let mut body = serde_json::Map::new();
        body.insert("kind".into(), serde_json::json!("nefor-tui.grid.flush"));
        body.insert("grid".into(), serde_json::json!(1));
        let out = PluginOutgoing::event(body.clone());
        let env = stamp(out);
        match env.body {
            Body::Event(m) => assert_eq!(m, body),
            _ => panic!("expected event body"),
        }
    }

    #[test]
    fn stamp_then_serialize_yields_valid_envelope_line() {
        let out = PluginOutgoing::system(SystemBody::Shutdown {
            reason: Some("test".into()),
            grace_ms: Some(1000),
        });
        let env = stamp(out);
        let line = env.to_line();
        let parsed = Envelope::parse_line(&line).expect("round trip");
        assert_eq!(parsed.from.as_str(), "engine");
        assert!(matches!(
            parsed.body,
            Body::System(SystemBody::Shutdown { .. })
        ));
    }
}
