//! NCP stdio transport: line-framed JSON reader and writer.
//!
//! Same pattern as `plugins/mock-plugin/src/ncp.rs` and
//! `plugins/nefor-tui/src/transport.rs` — two tokio tasks own stdin and
//! stdout, communicating with the main loop through mpsc channels.
//!
//! - [`spawn_stdin_reader`] reads `\n`-delimited JSON lines and parses each
//!   into a [`nefor_protocol::Envelope`]. Parse failures become `Err`
//!   variants so the main loop can log and keep reading.
//! - [`spawn_stdout_writer`] owns `tokio::io::stdout()` and writes each
//!   incoming [`nefor_protocol::PluginOutgoing`] as one JSON line. Single
//!   owner ensures writes never interleave mid-line.
//! - [`await_ready_ok`] blocks between the plugin's `ready` and the engine's
//!   reply so the main loop knows when to start emitting chat events.

use nefor_protocol::{Envelope, PluginOutgoing};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::error::ChatError;

/// Capacity for both mpsc channels. Matches mock-plugin / nefor-tui.
pub const CHANNEL_CAP: usize = 128;

/// Spawn the stdin reader task. Terminates on EOF or when the receiver
/// drops.
pub fn spawn_stdin_reader(
    tx: mpsc::Sender<Result<Envelope, ChatError>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin).lines();
        loop {
            match reader.next_line().await {
                Ok(Some(line)) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let parsed = Envelope::parse_line(trimmed).map_err(ChatError::from);
                    if tx.send(parsed).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = tx.send(Err(ChatError::Io(e))).await;
                    break;
                }
            }
        }
    })
}

/// Spawn the stdout writer task. Returns the sender used by the handshake
/// code and the render loop.
pub fn spawn_stdout_writer() -> (mpsc::Sender<PluginOutgoing>, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(CHANNEL_CAP);
    let handle = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(msg) = rx.recv().await {
            let line = msg.to_line();
            if let Err(e) = write_line(&mut stdout, &line).await {
                tracing::error!(error = %e, "stdout write failed; chat output disabled");
                break;
            }
        }
    });
    (tx, handle)
}

async fn write_line<W>(w: &mut W, line: &str) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    w.write_all(line.as_bytes()).await?;
    w.write_all(b"\n").await?;
    w.flush().await
}

/// Block until the engine replies to our `ready`. Returns the engine
/// version on success. Stray events before `ready_ok` are logged and
/// skipped — per §5 only `ready_ok` or `error` are valid replies here.
pub async fn await_ready_ok(
    rx: &mut mpsc::Receiver<Result<Envelope, ChatError>>,
) -> Result<String, ChatError> {
    use nefor_protocol::{Body, SystemBody};
    loop {
        let env = match rx.recv().await {
            Some(Ok(env)) => env,
            Some(Err(e)) => {
                tracing::warn!(error = %e, "parse error while awaiting ready_ok; ignoring");
                continue;
            }
            None => return Err(ChatError::ReadyClosed),
        };
        match env.body {
            Body::System(SystemBody::ReadyOk { engine_version }) => {
                return Ok(engine_version);
            }
            Body::System(SystemBody::Error { code, message, .. }) => {
                return Err(ChatError::ReadyFailed(format!("{code:?}: {message}")));
            }
            other => {
                tracing::warn!(?other, "unexpected pre-ready_ok envelope; ignoring");
                continue;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nefor_protocol::{Envelope, PluginName, SystemBody, Timestamp};

    #[tokio::test]
    async fn await_ready_ok_accepts_ready_ok() {
        let (tx, mut rx) = mpsc::channel::<Result<Envelope, ChatError>>(4);
        let env = Envelope::system(
            PluginName::engine(),
            Timestamp::parse("2026-04-21T00:00:00.000Z").expect("valid"),
            SystemBody::ReadyOk {
                engine_version: "0.1.0".into(),
            },
        );
        tx.send(Ok(env)).await.expect("send");
        drop(tx);
        let v = await_ready_ok(&mut rx).await.expect("ready ok");
        assert_eq!(v, "0.1.0");
    }

    #[tokio::test]
    async fn await_ready_ok_surfaces_error_reply() {
        use nefor_protocol::ErrorCode;
        let (tx, mut rx) = mpsc::channel::<Result<Envelope, ChatError>>(4);
        let env = Envelope::system(
            PluginName::engine(),
            Timestamp::parse("2026-04-21T00:00:00.000Z").expect("valid"),
            SystemBody::Error {
                code: ErrorCode::ProtocolVersionMismatch,
                message: "nope".into(),
                offending: None,
            },
        );
        tx.send(Ok(env)).await.expect("send");
        drop(tx);
        let err = await_ready_ok(&mut rx).await.unwrap_err();
        assert!(matches!(err, ChatError::ReadyFailed(_)));
    }

    #[tokio::test]
    async fn await_ready_ok_detects_closed_stream() {
        let (_tx, mut rx) = mpsc::channel::<Result<Envelope, ChatError>>(1);
        drop(_tx);
        let err = await_ready_ok(&mut rx).await.unwrap_err();
        assert!(matches!(err, ChatError::ReadyClosed));
    }
}
