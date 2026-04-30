//! NCP stdio transport for basic-tools.
//!
//! Same shape as `nefor-combinators::ncp` — a stdin reader, a stdout writer
//! that owns `tokio::io::stdout()`, and an `await_ready_ok` helper for the
//! §5 handshake. See `plugins/nefor-combinators/src/ncp.rs` for the
//! provenance of the pattern.

use nefor_protocol::{Envelope, PluginOutgoing};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::error::BasicToolsError;

/// Capacity for both mpsc channels. Sized generously enough that a burst
/// of `tool.invoke` events won't backpressure the transport tasks.
pub const CHANNEL_CAP: usize = 256;

/// Spawn the stdin reader task. Lines that fail to parse are forwarded as
/// `Err(BasicToolsError::Parse(_))` so the main loop decides what to log.
pub fn spawn_stdin_reader(
    tx: mpsc::Sender<Result<Envelope, BasicToolsError>>,
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
                    let parsed = Envelope::parse_line(trimmed).map_err(BasicToolsError::from);
                    if tx.send(parsed).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = tx.send(Err(BasicToolsError::Io(e))).await;
                    break;
                }
            }
        }
    })
}

/// Spawn the stdout writer task. Single owner means writes never interleave.
pub fn spawn_stdout_writer() -> (mpsc::Sender<PluginOutgoing>, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(CHANNEL_CAP);
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

async fn write_line<W>(w: &mut W, line: &str) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    w.write_all(line.as_bytes()).await?;
    w.write_all(b"\n").await?;
    w.flush().await
}

/// Block until the engine replies to our `ready`.
///
/// On `ready_ok` returns the engine version string. On `error` system
/// message returns [`BasicToolsError::ReadyFailed`]. Anything else is
/// logged at `warn` and the wait continues.
pub async fn await_ready_ok(
    rx: &mut mpsc::Receiver<Result<Envelope, BasicToolsError>>,
) -> Result<String, BasicToolsError> {
    use nefor_protocol::{Body, SystemBody};
    loop {
        let env = match rx.recv().await {
            Some(Ok(env)) => env,
            Some(Err(e)) => {
                tracing::warn!(error = %e, "parse error while awaiting ready_ok; ignoring");
                continue;
            }
            None => return Err(BasicToolsError::ReadyClosed),
        };
        match env.body {
            Body::System(SystemBody::ReadyOk { engine_version }) => {
                return Ok(engine_version);
            }
            Body::System(SystemBody::Error { code, message, .. }) => {
                return Err(BasicToolsError::ReadyFailed(format!(
                    "{code:?}: {message}"
                )));
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
        let (tx, mut rx) = mpsc::channel::<Result<Envelope, BasicToolsError>>(4);
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
    async fn await_ready_ok_detects_closed_stream() {
        let (_tx, mut rx) = mpsc::channel::<Result<Envelope, BasicToolsError>>(1);
        drop(_tx);
        let err = await_ready_ok(&mut rx).await.unwrap_err();
        assert!(matches!(err, BasicToolsError::ReadyClosed));
    }
}
