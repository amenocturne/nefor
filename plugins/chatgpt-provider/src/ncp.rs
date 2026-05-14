//! NCP stdio transport.
//!
//! Two background tasks: a stdin reader that parses lines into
//! `Envelope`s and forwards them on an mpsc, and a stdout writer that
//! owns `tokio::io::stdout()` and serializes `PluginOutgoing`s with
//! `\n` framing. `await_ready_ok` consumes pre-handshake envelopes
//! until the §5.2 `ready_ok` arrives. Mirror of openai-provider/ncp.rs.

use nefor_protocol::{Body, Envelope, PluginOutgoing, SystemBody};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::error::ChatgptError;

/// Default capacity of both the inbound and outbound channels. Matches
/// the openai-provider value — same fanout characteristics; bus
/// pressure on either side is unlikely.
pub const CHANNEL_CAP: usize = 256;

pub fn spawn_stdin_reader(
    tx: mpsc::Sender<Result<Envelope, ChatgptError>>,
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
                    let parsed = Envelope::parse_line(trimmed).map_err(ChatgptError::from);
                    if tx.send(parsed).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = tx.send(Err(ChatgptError::Io(e))).await;
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

/// Consume the inbound channel until §5.2 `ready_ok` arrives. Returns
/// the engine's reported version string. Aborts on a §5.4 `error` from
/// the engine; parse errors before handshake are logged and skipped
/// (engine spec allows interleaved `error` envelopes that don't close
/// the connection).
pub async fn await_ready_ok(
    rx: &mut mpsc::Receiver<Result<Envelope, ChatgptError>>,
) -> Result<String, ChatgptError> {
    loop {
        let env = match rx.recv().await {
            Some(Ok(env)) => env,
            Some(Err(e)) => {
                tracing::warn!(error = %e, "parse error before ready_ok; ignoring");
                continue;
            }
            None => return Err(ChatgptError::ReadyClosed),
        };
        match env.body {
            Body::System(SystemBody::ReadyOk { engine_version }) => return Ok(engine_version),
            Body::System(SystemBody::Error { code, message, .. }) => {
                return Err(ChatgptError::ReadyFailed(format!("{code:?}: {message}")));
            }
            other => {
                tracing::warn!(?other, "unexpected pre-ready_ok envelope; ignoring");
                continue;
            }
        }
    }
}
