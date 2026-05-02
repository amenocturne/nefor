//! Domain errors for nefor-tui-decl.

#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    #[error("lua error: {0}")]
    Lua(#[from] mlua::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("invalid widget description: {0}")]
    InvalidDesc(String),
}
