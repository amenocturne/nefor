//! Domain errors for nefor-tui.

use nefor_protocol::ParseError;

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

    #[error("ready failed: {0}")]
    ReadyFailed(String),

    #[error("engine closed stdio before ready_ok")]
    ReadyClosed,

    #[error("protocol parse error: {0}")]
    Parse(#[from] ParseError),

    #[error("stdout writer closed before outgoing message was delivered")]
    WriterClosed,
}
