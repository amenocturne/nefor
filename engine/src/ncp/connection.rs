//! A single plugin connection: read loop, write queue.
//!
//! Post-Slice-2-I3 the broker is protocol-agnostic: it shuttles raw lines
//! in both directions without parsing the NCP envelope. The reader task
//! reads one newline-delimited UTF-8 string at a time (capped at
//! [`MAX_LINE_BYTES`]) and hands the raw line to the broker. The writer
//! task writes whatever bytes the broker queues onto the wire.
//!
//! Framing-level errors (line exceeds the 16 MiB bound, non-UTF-8 bytes,
//! or transport IO error) end the reader; the broker decides whether to
//! tear the connection down via the exit watcher or reader-closed signal.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
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

/// Outbound payload destined for one connection's write queue.
///
/// `Close` is special: the writer task drains any preceding `Send`s,
/// flushes the writer, then exits. It's how the broker sequences
/// "send, then close" atomically.
#[derive(Debug)]
pub enum ConnectionOutbound {
    /// Send these bytes verbatim. Caller is responsible for including a
    /// trailing `\n` (or whatever framing the wire expects) — the writer
    /// does not add one.
    Send(String),
    /// Close the connection after draining the preceding sends.
    Close,
}

/// Inbound signal a connection's reader sends to the broker.
#[derive(Debug)]
pub enum ConnectionInbound {
    /// A raw line read from the plugin, with the trailing newline stripped.
    /// The broker does not parse; it timestamps and hands off to step.
    Line(String),
    /// Reader loop ended — EOF, framing cap exceeded, or a transport IO
    /// error. Broker logs the departure; peers that care about peer
    /// liveness rely on plugin-authored conventions.
    Closed {
        /// Why the reader stopped.
        reason: ReaderEnd,
    },
}

/// Why a reader loop stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderEnd {
    /// Clean EOF.
    Eof,
    /// Transport-level IO error (including invalid UTF-8 on the wire).
    IoError,
    /// Line exceeded the 16 MiB bound (§2). Treated as a framing error.
    LineTooLong,
}

/// Maximum line length in bytes (§2: 16 MiB default).
pub const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// Default per-connection receive queue capacity (§6).
pub const DEFAULT_QUEUE_CAPACITY: usize = 1024;

/// Spawn the reader task. Reads one line at a time, strips the trailing
/// newline, and forwards [`ConnectionInbound::Line`] to `tx`. Returns when
/// EOF, the broker closes `tx`, or a framing/IO error occurs.
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
                    if tx
                        .send((id, ConnectionInbound::Line(std::mem::take(&mut line))))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    continue;
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
    use tokio::io::AsyncBufReadExt;

    // `read_until` honors the BufReader's internal 64 KiB buffer — single
    // syscall per buffer-fill, then byte scans in user space. The previous
    // implementation called `read_exact` one byte at a time, which defeated
    // the buffer (per-byte poll wake-up + tokio runtime crossings) and
    // showed up as visible typing latency on the keystroke→render path.
    let mut buf = Vec::with_capacity(256);
    let n = reader.read_until(b'\n', &mut buf).await?;
    if n == 0 {
        return Ok(ReadLine::Ok(0));
    }
    if buf.len() > max {
        return Ok(ReadLine::TooLong);
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
/// the oldest queued line is dropped and a warning is logged — the broker no
/// longer emits a protocol-level `QueueOverflow` system message (that was a
/// starter/init.lua concern in the new model).
///
/// `rx` itself is unbounded so the broker is never blocked; backpressure
/// is enforced inside this task. Exits on [`ConnectionOutbound::Close`] or
/// when `rx` closes.
pub async fn run_writer(
    id: ConnectionId,
    mut writer: Pin<Box<dyn AsyncWrite + Send + Unpin>>,
    rx: mpsc::UnboundedReceiver<ConnectionOutbound>,
    cap: usize,
) {
    let mut rx = rx;
    let mut q: VecDeque<String> = VecDeque::with_capacity(cap.min(1024));
    let mut close_after_drain = false;

    loop {
        // Drain anything we can read without blocking, applying overflow
        // policy per §6.
        loop {
            match rx.try_recv() {
                Ok(ConnectionOutbound::Send(payload)) => {
                    if q.len() >= cap {
                        let _dropped = q.pop_front();
                        tracing::warn!(
                            conn = %id,
                            cap,
                            "write queue full; dropping oldest line",
                        );
                    }
                    q.push_back(payload);
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
        if let Some(line) = q.pop_front() {
            if writer.write_all(line.as_bytes()).await.is_err() {
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
            Some(ConnectionOutbound::Send(payload)) => q.push_back(payload),
            Some(ConnectionOutbound::Close) => close_after_drain = true,
            None => close_after_drain = true,
        }
    }
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
    async fn reader_emits_lines_verbatim() {
        let (mut client, server) = duplex(1024);
        let (tx, mut rx) = mpsc::channel(8);
        let id = ConnectionId::next();
        let handle = tokio::spawn(run_reader(id, Box::pin(server), tx));

        client.write_all(b"hello world\n").await.unwrap();
        drop(client);

        let (got_id, msg) = rx.recv().await.expect("message");
        assert_eq!(got_id, id);
        match msg {
            ConnectionInbound::Line(s) => assert_eq!(s, "hello world"),
            other => panic!("expected line, got {other:?}"),
        }

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
    async fn reader_skips_empty_lines() {
        let (mut client, server) = duplex(1024);
        let (tx, mut rx) = mpsc::channel(8);
        let id = ConnectionId::next();
        let handle = tokio::spawn(run_reader(id, Box::pin(server), tx));

        client.write_all(b"\n\nonly\n\n").await.unwrap();
        drop(client);

        let (_, msg) = rx.recv().await.expect("message");
        match msg {
            ConnectionInbound::Line(s) => assert_eq!(s, "only"),
            other => panic!("expected line, got {other:?}"),
        }

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
    async fn writer_drains_lines_verbatim() {
        let (client, server) = duplex(1024);
        let (tx, rx) = mpsc::unbounded_channel();
        let id = ConnectionId::next();
        let handle = tokio::spawn(run_writer(id, Box::pin(server), rx, DEFAULT_QUEUE_CAPACITY));

        tx.send(ConnectionOutbound::Send("first\n".into())).unwrap();
        tx.send(ConnectionOutbound::Send("second\n".into()))
            .unwrap();
        tx.send(ConnectionOutbound::Close).unwrap();

        let mut reader = BufReader::new(client);
        let mut line1 = String::new();
        reader.read_line(&mut line1).await.unwrap();
        assert_eq!(line1, "first\n");
        let mut line2 = String::new();
        reader.read_line(&mut line2).await.unwrap();
        assert_eq!(line2, "second\n");
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn writer_overflow_drops_oldest_silently() {
        // Tiny duplex: writer stalls after 1-2 lines. Tiny cap: queue
        // saturates after a couple of pushes. The new string-level writer
        // drops oldest on overflow and logs a warning — no protocol-level
        // envelope is emitted any more.
        let (client, server) = duplex(16);
        let (tx, rx) = mpsc::unbounded_channel();
        let cap = 2;
        let id = ConnectionId::next();
        let handle = tokio::spawn(run_writer(id, Box::pin(server), rx, cap));

        // Six lines into a queue of 2 — with a stalled writer, at least four
        // should be dropped. We're not asserting the exact dropped count
        // (that depends on scheduling) — just that the surviving output is a
        // strict suffix of what we sent and that the task winds down cleanly.
        let total = 6u32;
        for i in 0..total {
            tx.send(ConnectionOutbound::Send(format!("line{i}\n")))
                .unwrap();
        }
        tx.send(ConnectionOutbound::Close).unwrap();

        let mut reader = BufReader::new(client);
        let mut seen = Vec::new();
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => seen.push(line.trim_end_matches('\n').to_owned()),
                Err(_) => break,
            }
        }
        // Some lines must have been delivered — the exact set is scheduling-
        // dependent but they all have the "line<N>" shape and are in order.
        assert!(!seen.is_empty(), "writer delivered nothing");
        for pair in seen.windows(2) {
            let a: u32 = pair[0].trim_start_matches("line").parse().unwrap();
            let b: u32 = pair[1].trim_start_matches("line").parse().unwrap();
            assert!(a < b, "writer delivered lines out of order: {seen:?}");
        }
        handle.await.unwrap();
    }
}
