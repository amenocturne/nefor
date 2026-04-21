//! NCP stdio transport: line-framed JSON reader and writer.
//!
//! Splits stdin into complete `\n`-delimited envelopes (spec §2) and parses
//! each into a [`nefor_protocol::Envelope`]. Writes are accepted via an mpsc
//! channel so multiple producers (initial ready, stdin-reader-triggered
//! replies, terminal input) can share a single owner of stdout without
//! interleaving mid-line.
//!
//! The `serde_json::Map`-building helpers live in [`crate::grid`] and
//! [`crate::input`]; this module only cares about framing.

use std::io;

use nefor_protocol::{Envelope, PluginOutgoing};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::errors::TuiError;

/// Spawn a task that reads JSON lines from stdin and forwards parsed
/// envelopes to `tx`. Returns when stdin closes.
///
/// Lines that fail to parse are surfaced through `tx` as an `Err` so the
/// main loop can log them uniformly. Writes are accepted via an mpsc
/// channel so multiple producers (initial ready, stdin-reader-triggered
/// replies, terminal input) can share a single owner of stdout without
/// interleaving mid-line.
pub fn spawn_stdin_reader(
    tx: mpsc::Sender<Result<Envelope, TuiError>>,
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
                    let parsed = Envelope::parse_line(trimmed).map_err(TuiError::from);
                    if tx.send(parsed).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break, // EOF
                Err(e) => {
                    let _ = tx.send(Err(TuiError::Io(e))).await;
                    break;
                }
            }
        }
    })
}

/// Spawn the single owner of stdout. Consumers send [`PluginOutgoing`]
/// values through the returned sender; the task writes each one as a
/// newline-terminated JSON line.
pub fn spawn_stdout_writer(
    capacity: usize,
) -> (mpsc::Sender<PluginOutgoing>, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(capacity);
    let handle = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(msg) = rx.recv().await {
            let line = msg.to_line();
            if let Err(e) = write_line(&mut stdout, &line).await {
                tracing::error!(error = %e, "stdout write failed; tui output disabled");
                break;
            }
        }
    });
    (tx, handle)
}

async fn write_line<W>(w: &mut W, line: &str) -> io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    w.write_all(line.as_bytes()).await?;
    w.write_all(b"\n").await?;
    w.flush().await
}

/// Block until stdin yields the engine's `ready_ok` or `error` reply.
///
/// Called by main.rs between sending our own `ready` and entering the
/// main loop. Returns the engine version on success, or an error on
/// rejection / premature close.
pub async fn await_ready_ok(
    rx: &mut mpsc::Receiver<Result<Envelope, TuiError>>,
) -> Result<String, TuiError> {
    use nefor_protocol::{Body, SystemBody};
    loop {
        let env = match rx.recv().await {
            Some(Ok(env)) => env,
            Some(Err(e)) => return Err(e),
            None => return Err(TuiError::ReadyClosed),
        };
        match env.body {
            Body::System(SystemBody::ReadyOk { engine_version }) => {
                return Ok(engine_version);
            }
            Body::System(SystemBody::Error { code, message, .. }) => {
                return Err(TuiError::ReadyFailed(format!("{code:?}: {message}")));
            }
            // Anything else before ready_ok is a protocol violation or a
            // stale stream; surface it as a best-effort diagnostic and
            // keep waiting.
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
        let (tx, mut rx) = mpsc::channel::<Result<Envelope, TuiError>>(4);
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
    async fn await_ready_ok_reports_error() {
        use nefor_protocol::ErrorCode;
        let (tx, mut rx) = mpsc::channel::<Result<Envelope, TuiError>>(4);
        let env = Envelope::system(
            PluginName::engine(),
            Timestamp::parse("2026-04-21T00:00:00.000Z").expect("valid"),
            SystemBody::Error {
                code: ErrorCode::ProtocolVersionMismatch,
                message: "unsupported".into(),
                offending: None,
            },
        );
        tx.send(Ok(env)).await.expect("send");
        drop(tx);
        let err = await_ready_ok(&mut rx).await.unwrap_err();
        match err {
            TuiError::ReadyFailed(m) => assert!(m.contains("ProtocolVersionMismatch")),
            _ => panic!("expected ReadyFailed, got {err:?}"),
        }
    }

    #[tokio::test]
    async fn await_ready_ok_detects_closed_stream() {
        let (_tx, mut rx) = mpsc::channel::<Result<Envelope, TuiError>>(1);
        drop(_tx);
        let err = await_ready_ok(&mut rx).await.unwrap_err();
        assert!(matches!(err, TuiError::ReadyClosed));
    }
}
