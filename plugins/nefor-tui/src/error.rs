//! Domain errors for nefor-tui.

#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    #[error("lua error: {0}")]
    Lua(#[from] mlua::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("invalid widget description: {0}")]
    InvalidDesc(String),

    #[error("tui.start has not been called yet; cannot drive engine")]
    NotStarted,

    /// NCP transport failure (handshake, parse, writer closed).
    #[error(transparent)]
    Transport(#[from] nefor_plugin_sdk::TransportError),
}
