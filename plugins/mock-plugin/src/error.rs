//! Domain errors for mock-plugin.
//!
//! `thiserror` for typed variants; no `anyhow` anywhere. Callers branch on
//! the variant, not on the `Display` string.

use std::path::PathBuf;

use nefor_protocol::ParseError;

/// All failure modes inside mock-plugin.
#[derive(Debug, thiserror::Error)]
pub enum MockError {
    /// User passed `--script <path>` but the file doesn't exist.
    #[error("script file not found: {0}")]
    ScriptNotFound(PathBuf),

    /// Script file existed but couldn't be read.
    #[error("failed to read script file: {0}")]
    ScriptRead(#[source] std::io::Error),

    /// Script parsed / executed but raised a Lua error.
    #[error("lua error: {0}")]
    Lua(#[from] mlua::Error),

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
