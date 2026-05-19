//! Domain errors for mock-plugin.
//!
//! `thiserror` for typed variants; no `anyhow` anywhere. Callers branch on
//! the variant, not on the `Display` string.

use std::path::PathBuf;

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

    /// NCP transport failure (I/O, handshake, parse, writer closed).
    #[error(transparent)]
    Transport(#[from] nefor_plugin_sdk::TransportError),
}
