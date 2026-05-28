#![deny(unsafe_code)]

//! Shared NCP stdio transport for nefor plugins.
//!
//! Every NCP plugin needs the same three primitives:
//!
//! - [`spawn_stdin_reader`] — background task that reads `\n`-terminated
//!   JSON lines from stdin, parses each as an [`Envelope`], and forwards
//!   `Result<Envelope, TransportError>` on an mpsc.
//! - [`spawn_stdout_writer`] — background task that owns stdout and
//!   serializes [`PluginOutgoing`] values one JSON line per message.
//! - [`await_ready_ok`] — blocks on the NCP ready handshake, returning
//!   the engine version string on success.
//!
//! Plugins use `#[error(transparent)] Transport(#[from] TransportError)`
//! in their own error enums to integrate these into their error hierarchy.

use nefor_protocol::{Body, Envelope, PluginOutgoing, SystemBody};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

/// Transport-level errors shared by all NCP plugins.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// I/O error on stdio or inside a transport task.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Engine rejected our ready handshake.
    #[error("ready handshake rejected: {0}")]
    ReadyFailed(String),

    /// Stdin closed before we saw `ready_ok`.
    #[error("stdin closed before ready_ok")]
    ReadyClosed,

    /// Wire-format decode failure.
    #[error("protocol parse error: {0}")]
    Parse(#[from] nefor_protocol::ParseError),

    /// The stdout writer task exited before the outgoing message was delivered.
    #[error("stdout writer closed")]
    WriterClosed,
}

/// Spawn the stdin reader task.
///
/// Lines that fail to parse are forwarded as `Err(TransportError::Parse(_))`
/// so the caller decides what to log. Returns when stdin EOFs or the
/// receiver is dropped.
pub fn spawn_stdin_reader(
    tx: mpsc::Sender<Result<Envelope, TransportError>>,
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
                    let parsed = Envelope::parse_line(trimmed).map_err(TransportError::from);
                    if tx.send(parsed).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = tx.send(Err(TransportError::Io(e))).await;
                    break;
                }
            }
        }
    })
}

/// Spawn the stdout writer task.
///
/// Returns a sender for outgoing messages and the task handle. The task
/// owns `tokio::io::stdout()` exclusively so writes never interleave.
/// `cap` sets the mpsc channel capacity — pass the plugin's preferred
/// value (typically 64, 128, or 256).
pub fn spawn_stdout_writer(
    cap: usize,
) -> (mpsc::Sender<PluginOutgoing>, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(cap);
    let handle = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(msg) = rx.recv().await {
            let line = msg.to_line();
            if let Err(e) = write_line(&mut stdout, &line).await {
                tracing::error!(error = %e, "stdout write failed; giving up");
                break;
            }
        }
    });
    (tx, handle)
}

/// Write a single `\n`-terminated line to an async writer.
async fn write_line<W>(w: &mut W, line: &str) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    w.write_all(line.as_bytes()).await?;
    w.write_all(b"\n").await?;
    w.flush().await
}

/// Block until the engine replies to our `ready` with `ready_ok`.
///
/// On success returns the engine version string. On `error` system
/// message returns [`TransportError::ReadyFailed`]. Anything else
/// (stray events, other system kinds) is logged at `warn` and the
/// wait continues.
pub async fn await_ready_ok(
    rx: &mut mpsc::Receiver<Result<Envelope, TransportError>>,
) -> Result<String, TransportError> {
    loop {
        let env = match rx.recv().await {
            Some(Ok(env)) => env,
            Some(Err(e)) => {
                tracing::warn!(error = %e, "parse error while awaiting ready_ok; ignoring");
                continue;
            }
            None => return Err(TransportError::ReadyClosed),
        };
        match env.body {
            Body::System(SystemBody::ReadyOk { engine_version }) => {
                return Ok(engine_version);
            }
            Body::System(SystemBody::Error { code, message, .. }) => {
                return Err(TransportError::ReadyFailed(format!("{code:?}: {message}")));
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
    use nefor_protocol::{Envelope, ErrorCode, PluginName, SystemBody, Timestamp};

    #[tokio::test]
    async fn await_ready_ok_accepts_ready_ok() {
        let (tx, mut rx) = mpsc::channel::<Result<Envelope, TransportError>>(4);
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
        let (tx, mut rx) = mpsc::channel::<Result<Envelope, TransportError>>(4);
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
        assert!(matches!(err, TransportError::ReadyFailed(_)));
    }

    #[tokio::test]
    async fn await_ready_ok_detects_closed_stream() {
        let (_tx, mut rx) = mpsc::channel::<Result<Envelope, TransportError>>(1);
        drop(_tx);
        let err = await_ready_ok(&mut rx).await.unwrap_err();
        assert!(matches!(err, TransportError::ReadyClosed));
    }

    #[tokio::test]
    async fn await_ready_ok_skips_stray_event() {
        let (tx, mut rx) = mpsc::channel::<Result<Envelope, TransportError>>(4);
        let stray = Envelope::event(
            PluginName::new("other").expect("valid"),
            Timestamp::parse("2026-04-21T00:00:00.000Z").expect("valid"),
            serde_json::Map::new(),
        );
        let ok = Envelope::system(
            PluginName::engine(),
            Timestamp::parse("2026-04-21T00:00:00.001Z").expect("valid"),
            SystemBody::ReadyOk {
                engine_version: "0.1.0".into(),
            },
        );
        tx.send(Ok(stray)).await.expect("send");
        tx.send(Ok(ok)).await.expect("send");
        drop(tx);
        let v = await_ready_ok(&mut rx).await.expect("ready ok after stray");
        assert_eq!(v, "0.1.0");
    }
}
