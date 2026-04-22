//! Domain errors for the nefor-chat plugin.
//!
//! `anyhow` is reserved for `main.rs`'s top boundary; everything else
//! returns a typed [`ChatError`].

use nefor_protocol::ParseError;

/// Failure modes inside the plugin.
#[derive(Debug, thiserror::Error)]
pub enum ChatError {
    /// The engine rejected our ready handshake.
    #[error("ready failed: {0}")]
    ReadyFailed(String),

    /// Stdin closed before `ready_ok` arrived.
    #[error("engine closed the stream before ready_ok")]
    ReadyClosed,

    /// I/O error on stdio.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// Wire-format decode failure.
    #[error("protocol parse error: {0}")]
    Parse(#[from] ParseError),

    /// The stdout writer task exited before the outgoing channel drained.
    #[error("stdout writer closed before outgoing message was delivered")]
    WriterClosed,
}
