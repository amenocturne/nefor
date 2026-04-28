//! NCP stdio transport for generic-provider.
//!
//! Mirrors the pattern used by `nefor-combinators` and `mock-plugin`:
//!
//! - [`spawn_stdin_reader`] reads `\n`-terminated JSON lines, parses each as
//!   an [`nefor_protocol::Envelope`], and forwards `Result<Envelope, _>` on
//!   an mpsc.
//! - [`spawn_stdout_writer`] owns `tokio::io::stdout()` and serializes any
//!   [`nefor_protocol::PluginOutgoing`] one JSON line per message.
//! - [`await_ready_ok`] blocks on the handshake reply between sending
//!   `ready` and turning over to dispatch.

use nefor_protocol::{Envelope, PluginOutgoing};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::error::ProviderError;

/// Capacity for both mpsc channels.
pub const CHANNEL_CAP: usize = 64;

pub fn spawn_stdin_reader(
    tx: mpsc::Sender<Result<Envelope, ProviderError>>,
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
                    let parsed = Envelope::parse_line(trimmed).map_err(ProviderError::from);
                    if tx.send(parsed).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = tx.send(Err(ProviderError::Io(e))).await;
                    break;
                }
            }
        }
    })
}

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

/// Block until the engine replies to our `ready`. Returns the engine
/// version on success.
pub async fn await_ready_ok(
    rx: &mut mpsc::Receiver<Result<Envelope, ProviderError>>,
) -> Result<String, ProviderError> {
    use nefor_protocol::{Body, SystemBody};
    loop {
        let env = match rx.recv().await {
            Some(Ok(env)) => env,
            Some(Err(e)) => {
                tracing::warn!(error = %e, "parse error while awaiting ready_ok; ignoring");
                continue;
            }
            None => return Err(ProviderError::ReadyClosed),
        };
        match env.body {
            Body::System(SystemBody::ReadyOk { engine_version }) => {
                return Ok(engine_version);
            }
            Body::System(SystemBody::Error { code, message, .. }) => {
                return Err(ProviderError::ReadyFailed(format!("{code:?}: {message}")));
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
        let (tx, mut rx) = mpsc::channel::<Result<Envelope, ProviderError>>(4);
        let env = Envelope::system(
            PluginName::engine(),
            Timestamp::parse("2026-04-30T00:00:00.000Z").expect("valid"),
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
        let (_tx, mut rx) = mpsc::channel::<Result<Envelope, ProviderError>>(1);
        drop(_tx);
        let err = await_ready_ok(&mut rx).await.unwrap_err();
        assert!(matches!(err, ProviderError::ReadyClosed));
    }
}
