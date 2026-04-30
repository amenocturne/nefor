//! Domain errors for the generic-tool plugin.
//!
//! Tiny surface: this plugin is a passive type-registry hub. The only
//! failures are stdio / handshake faults; everything else is silent.

use nefor_protocol::ParseError;

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
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
