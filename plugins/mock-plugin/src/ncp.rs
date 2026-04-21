//! NCP stdio transport for mock-plugin.
//!
//! Two tokio tasks own stdin and stdout:
//!
//! - [`spawn_stdin_reader`] reads `\n`-terminated JSON lines, parses each as
//!   a [`nefor_protocol::Envelope`], and forwards `Result<Envelope, _>` on
//!   an mpsc. Parse failures become `Err` variants so the main loop can
//!   log and keep reading.
//! - [`spawn_stdout_writer`] owns `tokio::io::stdout()` and serializes any
//!   [`nefor_protocol::PluginOutgoing`] arriving on its mpsc, one JSON
//!   line per message. Single owner means writes never interleave.
//!
//! [`await_ready_ok`] is the half of the handshake the main loop blocks on
//! between sending `ready` and turning over dispatch to the Lua host.

use nefor_protocol::{Envelope, PluginOutgoing};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::error::MockError;

/// Capacity for both mpsc channels. 128 is far more than typical NCP
/// traffic needs and small enough that backpressure surfaces faults
/// rather than hiding them.
pub const CHANNEL_CAP: usize = 128;

/// Spawn the stdin reader task.
///
/// Lines that fail to parse are forwarded as `Err(MockError::Parse(_))` so
/// the main loop decides what to log. Returns when stdin EOFs or the
/// receiver is dropped.
pub fn spawn_stdin_reader(
    tx: mpsc::Sender<Result<Envelope, MockError>>,
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
                    let parsed = Envelope::parse_line(trimmed).map_err(MockError::from);
                    if tx.send(parsed).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = tx.send(Err(MockError::Io(e))).await;
                    break;
                }
            }
        }
    })
}

/// Spawn the stdout writer task. Returns the sender — the single producer
/// interface used by both the handshake code and Lua's `nefor.emit`.
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
/// message returns [`MockError::ReadyFailed`] carrying a code+message
/// summary. Anything else (stray events, other system kinds) is logged at
/// `warn` and the wait continues — spec §5 only allows `ready_ok` or
/// `error` from the engine in this window.
pub async fn await_ready_ok(
    rx: &mut mpsc::Receiver<Result<Envelope, MockError>>,
) -> Result<String, MockError> {
    use nefor_protocol::{Body, SystemBody};
    loop {
        let env = match rx.recv().await {
            Some(Ok(env)) => env,
            Some(Err(e)) => {
                // Parse errors during handshake: log and keep trying.
                // The engine might have sent a malformed line (unlikely)
                // or we got garbage before the real reply.
                tracing::warn!(error = %e, "parse error while awaiting ready_ok; ignoring");
                continue;
            }
            None => return Err(MockError::ReadyClosed),
        };
        match env.body {
            Body::System(SystemBody::ReadyOk { engine_version }) => {
                return Ok(engine_version);
            }
            Body::System(SystemBody::Error { code, message, .. }) => {
                return Err(MockError::ReadyFailed(format!("{code:?}: {message}")));
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
        let (tx, mut rx) = mpsc::channel::<Result<Envelope, MockError>>(4);
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
        let (tx, mut rx) = mpsc::channel::<Result<Envelope, MockError>>(4);
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
        assert!(matches!(err, MockError::ReadyFailed(_)));
    }

    #[tokio::test]
    async fn await_ready_ok_detects_closed_stream() {
        let (_tx, mut rx) = mpsc::channel::<Result<Envelope, MockError>>(1);
        drop(_tx);
        let err = await_ready_ok(&mut rx).await.unwrap_err();
        assert!(matches!(err, MockError::ReadyClosed));
    }

    #[tokio::test]
    async fn await_ready_ok_skips_stray_event() {
        let (tx, mut rx) = mpsc::channel::<Result<Envelope, MockError>>(4);
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
