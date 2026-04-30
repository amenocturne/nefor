//! Domain errors for the basic-tools plugin.
//!
//! Two layers:
//!
//! - [`BasicToolsError`] — plugin-level failures (transport, handshake, parse).
//!   These are fatal: they halt or short-circuit dispatch.
//! - [`ToolError`] — tool-call failures surfaced on the wire as
//!   `tool.result { error }`. These are *not* fatal — they're a tool's
//!   normal error channel and the plugin keeps serving the bus.

use nefor_protocol::ParseError;

/// Plugin-level failure modes (transport, handshake, parse). Any of these
/// short-circuits the dispatch loop.
#[derive(Debug, thiserror::Error)]
pub enum BasicToolsError {
    /// I/O error on stdio or inside a transport task.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// Engine rejected our ready handshake, or closed before replying.
    #[error("ready failed: {0}")]
    ReadyFailed(String),

    /// Stdin closed before we saw `ready_ok`.
    #[error("engine closed stdio before ready_ok")]
    ReadyClosed,

    /// Wire-format decode failure we could not recover from.
    #[error("protocol parse error: {0}")]
    Parse(#[from] ParseError),

    /// The writer task exited before the outgoing channel drained.
    #[error("stdout writer closed before outgoing message was delivered")]
    WriterClosed,
}

/// Tool-call failure modes. These surface on the wire as
/// `tool.result { id, error: "<message>" }`. The variant carries enough
/// context to format a useful diagnostic without leaking internal types.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// The path didn't resolve to a file.
    #[error("file not found: {path}")]
    NotFound {
        /// Path the caller asked for.
        path: String,
    },

    /// The path resolved, but it's a directory.
    #[error("path is a directory: {path}")]
    IsDirectory {
        /// Path the caller asked for.
        path: String,
    },

    /// Heuristic: the first 8 KiB of the file contains a NUL byte.
    #[error("file appears to be binary: {path}")]
    BinaryContent {
        /// Path the caller asked for.
        path: String,
    },

    /// The file is larger than the 1 MiB cap.
    #[error("file too large ({size} bytes; cap is 1 MiB): {path}")]
    TooLarge {
        /// Actual size in bytes.
        size: u64,
        /// Path the caller asked for.
        path: String,
    },

    /// The file contents weren't valid UTF-8.
    #[error("file is not valid UTF-8: {path}")]
    NotUtf8 {
        /// Path the caller asked for.
        path: String,
    },

    /// Generic IO error opening / reading the file.
    #[error("io error reading {path}: {message}")]
    Io {
        /// Path the caller asked for.
        path: String,
        /// Underlying error message.
        message: String,
    },

    /// `args` payload was structurally invalid for the named tool.
    #[error("invalid args for tool `{tool}`: {message}")]
    BadArgs {
        /// Tool name from the `tool.invoke` event.
        tool: String,
        /// Diagnostic for the caller.
        message: String,
    },

    /// `bash` exceeded its wall-clock timeout. Output captured up to the
    /// kill point is included so the caller can see what ran.
    #[error("bash timed out after {timeout_ms}ms; partial output:\n{output}")]
    BashTimeout {
        /// Configured timeout in milliseconds.
        timeout_ms: u64,
        /// Combined stdout+stderr captured before the kill.
        output: String,
    },
}
