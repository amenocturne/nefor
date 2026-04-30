//! Domain errors for openai-provider.

use nefor_protocol::ParseError;

use openai_provider::state::ChatsError;

/// All failure modes inside openai-provider.
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
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

    /// Bubbled out of a chats-map operation. The dispatcher catches and
    /// translates these into wire-level error events; surfacing them as
    /// a top-level error variant means we don't have to `unwrap` in
    /// production code paths.
    #[error("chat operation failed: {0}")]
    Chats(#[from] ChatsError),
}
