//! A single plugin connection: read loop, write queue, ready state.
//!
//! The connection is a value the broker owns in its central state. Each has
//! a bounded MPSC receive queue (default 1024 per §6) the broker pushes
//! messages into; a dedicated writer task drains that queue onto the wire.
//!
//! A dedicated reader task parses one JSON line at a time from the transport
//! reader and forwards a [`ConnectionInbound`] to the broker via the
//! broker-wide inbound channel. Parse errors are converted to
//! [`nefor_protocol::ParseError`] and let the broker decide whether to send
//! back an `error` and whether to close.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};

use nefor_protocol::{
    Envelope, ErrorCode, Offending, ParseError, PluginName, PluginOutgoing, SystemBody, Timestamp,
};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::ncp::transport::{ExitOutcome, ExitWatcher};

/// Unique, broker-local identifier for a connection. Monotonic from 1.
/// `0` is reserved and never issued — distinguishes a default-initialized
/// handle from a live connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnectionId(u64);

impl ConnectionId {
    /// Construct from the atomic generator below.
    pub fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }

    /// Inner value — useful for log tagging only.
    #[allow(dead_code)]
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "conn#{}", self.0)
    }
}

/// Outbound message destined for one connection's write queue.
///
/// `Close` is special: the writer task drains any preceding `Send`s,
/// flushes the writer, then exits. It's how the broker sequences
/// "send error, then close" atomically per §8.
#[derive(Debug)]
pub enum ConnectionOutbound {
    /// Send this envelope (already stamped with `from = engine` or a peer's
    /// name + `ts`).
    Send(Envelope),
    /// Close the connection after draining the preceding sends.
    Close,
}

/// Inbound signal a connection's reader sends to the broker.
#[derive(Debug)]
pub enum ConnectionInbound {
    /// Successfully-parsed line from the plugin.
    Message(PluginOutgoing),
    /// Parse failure — broker maps to `error` code per §8.
    ParseError(ParseError),
    /// Reader loop ended — EOF or fatal IO error. Broker logs the
    /// departure; peers that care about peer liveness rely on plugin-
    /// authored conventions (see `docs/plugin-authoring.md`).
    Closed {
        /// Whether the reader hit EOF cleanly or a transport error.
        reason: ReaderEnd,
    },
}

/// Why a reader loop stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderEnd {
    /// Clean EOF.
    Eof,
    /// Transport-level IO error.
    IoError,
    /// Line exceeded the 16 MiB bound (§2). Treated as a framing error.
    LineTooLong,
}

/// Maximum line length in bytes (§2: 16 MiB default).
pub const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// Default per-connection receive queue capacity (§6).
pub const DEFAULT_QUEUE_CAPACITY: usize = 1024;

/// Spawn the reader task. Reads one line at a time, parses via
/// [`PluginOutgoing::parse_line`], and forwards [`ConnectionInbound`] to
/// `tx`. Returns when EOF or the broker closes `tx`.
pub async fn run_reader(
    id: ConnectionId,
    reader: Pin<Box<dyn AsyncRead + Send + Unpin>>,
    tx: mpsc::Sender<(ConnectionId, ConnectionInbound)>,
) {
    let mut buf = BufReader::with_capacity(64 * 1024, reader);
    let mut line = String::new();
    loop {
        line.clear();
        // Use a manual read-line with a byte cap so a pathological giant
        // line doesn't balloon memory. read_line() has no built-in cap.
        let end = match read_line_capped(&mut buf, &mut line, MAX_LINE_BYTES).await {
            Ok(ReadLine::Ok(n)) => {
                if n == 0 {
                    ReaderEnd::Eof
                } else {
                    // Strip trailing newline(s).
                    while line.ends_with('\n') || line.ends_with('\r') {
                        line.pop();
                    }
                    // Empty line after trim — ignore and continue.
                    if line.is_empty() {
                        continue;
                    }
                    match PluginOutgoing::parse_line(&line) {
                        Ok(msg) => {
                            if tx
                                .send((id, ConnectionInbound::Message(msg)))
                                .await
                                .is_err()
                            {
                                return;
                            }
                            continue;
                        }
                        Err(e) => {
                            let _ = tx.send((id, ConnectionInbound::ParseError(e))).await;
                            continue;
                        }
                    }
                }
            }
            Ok(ReadLine::TooLong) => ReaderEnd::LineTooLong,
            Err(_) => ReaderEnd::IoError,
        };
        let _ = tx
            .send((id, ConnectionInbound::Closed { reason: end }))
            .await;
        return;
    }
}

enum ReadLine {
    Ok(usize),
    TooLong,
}

async fn read_line_capped<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    out: &mut String,
    max: usize,
) -> std::io::Result<ReadLine> {
    let mut buf = Vec::with_capacity(256);
    loop {
        let mut byte = [0u8; 1];
        let n = reader.read_exact(&mut byte).await.map(|_| 1).or_else(|e| {
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                Ok(0)
            } else {
                Err(e)
            }
        })?;
        if n == 0 {
            if buf.is_empty() {
                return Ok(ReadLine::Ok(0));
            }
            break;
        }
        if byte[0] == b'\n' {
            buf.push(byte[0]);
            break;
        }
        if buf.len() + 1 > max {
            return Ok(ReadLine::TooLong);
        }
        buf.push(byte[0]);
    }
    match String::from_utf8(buf) {
        Ok(s) => {
            out.push_str(&s);
            Ok(ReadLine::Ok(out.len()))
        }
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid UTF-8 in NCP line",
        )),
    }
}

/// Spawn the writer task. Implements the bounded receive queue from §6: an
/// internal [`VecDeque`] of capacity `cap` (default
/// [`DEFAULT_QUEUE_CAPACITY`]). When a Send arrives and the queue is full,
/// the oldest queued envelope is dropped and a `QueueOverflow` system error
/// is enqueued in its place — the receiver MUST see the error per spec §6.
///
/// `rx` itself is unbounded so the broker is never blocked; backpressure
/// is enforced inside this task. Exits on [`ConnectionOutbound::Close`] or
/// when `rx` closes.
pub async fn run_writer(
    mut writer: Pin<Box<dyn AsyncWrite + Send + Unpin>>,
    rx: mpsc::UnboundedReceiver<ConnectionOutbound>,
    cap: usize,
) {
    let mut rx = rx;
    let mut q: VecDeque<Envelope> = VecDeque::with_capacity(cap.min(1024));
    let mut close_after_drain = false;

    loop {
        // Drain anything we can read without blocking, applying overflow
        // policy per §6.
        loop {
            match rx.try_recv() {
                Ok(ConnectionOutbound::Send(env)) => {
                    if q.len() >= cap {
                        let dropped = q.pop_front();
                        let overflow = make_queue_overflow(dropped.as_ref());
                        q.push_back(overflow);
                    }
                    q.push_back(env);
                }
                Ok(ConnectionOutbound::Close) => {
                    close_after_drain = true;
                    break;
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    close_after_drain = true;
                    break;
                }
            }
        }

        // Try to write the head of the queue.
        if let Some(env) = q.pop_front() {
            let line = env.to_line();
            if writer.write_all(line.as_bytes()).await.is_err() {
                return;
            }
            if writer.write_all(b"\n").await.is_err() {
                return;
            }
            if writer.flush().await.is_err() {
                return;
            }
            continue;
        }

        if close_after_drain {
            let _ = writer.flush().await;
            let _ = writer.shutdown().await;
            return;
        }

        // Nothing to do — wait for at least one new message.
        match rx.recv().await {
            Some(ConnectionOutbound::Send(env)) => q.push_back(env),
            Some(ConnectionOutbound::Close) => close_after_drain = true,
            None => close_after_drain = true,
        }
    }
}

fn make_queue_overflow(dropped: Option<&Envelope>) -> Envelope {
    Envelope::system(
        PluginName::engine(),
        Timestamp::now(),
        SystemBody::Error {
            code: ErrorCode::QueueOverflow,
            message: "per-connection receive queue full; oldest message dropped".into(),
            offending: dropped.map(|e| Offending {
                from: e.from.clone(),
                ts: e.ts,
            }),
        },
    )
}

/// Pipe a stderr byte stream to tracing, one line per log record, prefixed
/// with the plugin name. Exits on EOF or IO error.
pub async fn run_stderr_pump(plugin_name: String, reader: Pin<Box<dyn AsyncRead + Send + Unpin>>) {
    let mut buf = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        match buf.read_line(&mut line).await {
            Ok(0) => return,
            Ok(_) => {
                let trimmed = line.trim_end_matches(['\n', '\r']);
                if !trimmed.is_empty() {
                    tracing::info!(plugin = %plugin_name, "{}", trimmed);
                }
            }
            Err(_) => return,
        }
    }
}

/// Wait on the process exit watcher and report the outcome on `tx`. No-op
/// if no watcher was supplied.
pub async fn run_exit_watcher(
    id: ConnectionId,
    watcher: Option<ExitWatcher>,
    tx: mpsc::Sender<(ConnectionId, ExitOutcome)>,
) {
    let watcher = match watcher {
        Some(w) => w,
        None => return,
    };
    let outcome = watcher.await;
    let _ = tx.send((id, outcome)).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn reader_parses_one_valid_line() {
        let (mut client, server) = duplex(1024);
        let (tx, mut rx) = mpsc::channel(8);
        let id = ConnectionId::next();
        let handle = tokio::spawn(run_reader(id, Box::pin(server), tx));

        // Send a valid ready outgoing envelope.
        let out = PluginOutgoing::system(nefor_protocol::SystemBody::Ready {
            protocol_version: "0.1".into(),
        });
        let line = format!("{}\n", out.to_line());
        client.write_all(line.as_bytes()).await.unwrap();
        drop(client);

        let (got_id, msg) = rx.recv().await.expect("message");
        assert_eq!(got_id, id);
        assert!(matches!(msg, ConnectionInbound::Message(_)));

        let (_, closed) = rx.recv().await.expect("close");
        assert!(
            matches!(
                closed,
                ConnectionInbound::Closed {
                    reason: ReaderEnd::Eof
                }
            ),
            "got {closed:?}"
        );
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn reader_reports_parse_error_on_invalid_json() {
        let (mut client, server) = duplex(1024);
        let (tx, mut rx) = mpsc::channel(8);
        let id = ConnectionId::next();
        let handle = tokio::spawn(run_reader(id, Box::pin(server), tx));

        client.write_all(b"this is not json\n").await.unwrap();
        drop(client);

        let (_, msg) = rx.recv().await.expect("message");
        assert!(
            matches!(
                msg,
                ConnectionInbound::ParseError(ParseError::InvalidJson(_))
            ),
            "got {msg:?}"
        );

        let (_, closed) = rx.recv().await.expect("close");
        assert!(matches!(
            closed,
            ConnectionInbound::Closed {
                reason: ReaderEnd::Eof
            }
        ));
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn writer_drains_envelopes_as_one_line_each() {
        let (client, server) = duplex(1024);
        let (tx, rx) = mpsc::unbounded_channel();
        let handle = tokio::spawn(run_writer(Box::pin(server), rx, DEFAULT_QUEUE_CAPACITY));

        let env = Envelope::system(
            PluginName::engine(),
            Timestamp::now(),
            SystemBody::ReadyOk {
                engine_version: "0.1.0".into(),
            },
        );
        tx.send(ConnectionOutbound::Send(env.clone())).unwrap();
        tx.send(ConnectionOutbound::Close).unwrap();

        let mut reader = BufReader::new(client);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert!(line.ends_with('\n'));
        assert!(line.contains("ready_ok"));
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn writer_overflow_drops_oldest_and_emits_queue_overflow() {
        // Tiny duplex: writer stalls after 1-2 lines. Tiny cap: queue
        // saturates after a couple of pushes. Then we push a third —
        // expect the oldest to be dropped and a QueueOverflow envelope
        // to appear in the stream.
        let (client, server) = duplex(64);
        let (tx, rx) = mpsc::unbounded_channel();
        let cap = 2;
        let handle = tokio::spawn(run_writer(Box::pin(server), rx, cap));

        // Three small events. Once the duplex fills the writer stalls;
        // queue accumulates; on the third push the oldest is dropped.
        for i in 0..6u32 {
            let mut body = serde_json::Map::new();
            body.insert("i".into(), serde_json::json!(i));
            let env = Envelope::event(PluginName::new("p").unwrap(), Timestamp::now(), body);
            tx.send(ConnectionOutbound::Send(env)).unwrap();
        }
        tx.send(ConnectionOutbound::Close).unwrap();

        let mut reader = BufReader::new(client);
        let mut saw_overflow = false;
        let mut total = 0;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {
                    total += 1;
                    if line.contains("queue_overflow") {
                        saw_overflow = true;
                    }
                }
                Err(_) => break,
            }
        }
        assert!(total > 0);
        assert!(
            saw_overflow,
            "expected at least one queue_overflow line; got {total} lines total"
        );
        handle.await.unwrap();
    }
}
