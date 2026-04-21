//! Plugin transport — the bytes-level interface between the broker and a
//! connection.
//!
//! v0.1 only supports stdio (§2). The broker consumes transports through
//! trait-object halves so tests can plug in in-memory channels without
//! requiring actual subprocesses.

use std::pin::Pin;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout};

/// A boxed `AsyncRead + Send + Unpin`. This is the broker's read half of a
/// plugin connection — stdout for stdio transports, or a
/// `tokio::io::DuplexStream` half in unit tests.
pub type BoxedReader = Pin<Box<dyn AsyncRead + Send + Unpin>>;

/// A boxed `AsyncWrite + Send + Unpin`. Write half paired with [`BoxedReader`].
pub type BoxedWriter = Pin<Box<dyn AsyncWrite + Send + Unpin>>;

/// A transport's stderr channel. `None` for transports that don't carry one
/// (e.g. in-memory test transports). Stderr is piped to `tracing` at INFO
/// level with a plugin-name prefix; it's not part of NCP.
pub type BoxedStderr = Option<Pin<Box<dyn AsyncRead + Send + Unpin>>>;

/// A plugin transport handed to the broker. Splits into three byte channels
/// plus an optional kill handle.
pub struct Transport {
    /// Read half — the broker parses one JSON line at a time.
    pub reader: BoxedReader,
    /// Write half — the broker serializes one JSON line at a time.
    pub writer: BoxedWriter,
    /// Stderr channel (optional). For stdio transports this is the child's
    /// stderr; broker pipes it to `tracing` and never forwards to plugins.
    pub stderr: BoxedStderr,
    /// On stdio transports this drives the subprocess to completion and
    /// reports its exit status. `None` for in-memory transports that have
    /// no process to wait on.
    pub exit: Option<ExitWatcher>,
}

/// Resolves to the process's exit outcome. For stdio transports this wraps
/// `tokio::process::Child::wait`.
pub type ExitWatcher = Pin<Box<dyn std::future::Future<Output = ExitOutcome> + Send>>;

/// How the plugin process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ExitOutcome {
    /// Clean exit with code 0.
    CleanExit,
    /// Non-zero exit code or signal. Broker logs the abnormal
    /// termination.
    Crash,
    /// Engine-initiated close (we killed it because it didn't honour the
    /// shutdown grace, etc.).
    Evicted,
    /// The broker couldn't observe the exit (wait failed). Treated as
    /// crash for safety.
    Unknown,
}

/// Build a [`Transport`] from a `tokio::process::Child`'s stdio halves. Caller
/// must have configured `stdin(piped()).stdout(piped()).stderr(piped())`.
pub fn stdio_transport(
    stdin: ChildStdin,
    stdout: ChildStdout,
    stderr: ChildStderr,
    exit: ExitWatcher,
) -> Transport {
    Transport {
        reader: Box::pin(stdout),
        writer: Box::pin(stdin),
        stderr: Some(Box::pin(stderr)),
        exit: Some(exit),
    }
}
