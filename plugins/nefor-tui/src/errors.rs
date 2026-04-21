//! Error types for the nefor-tui plugin.
//!
//! `anyhow` is reserved for `main.rs`; everything else returns a typed
//! [`TuiError`].

use nefor_protocol::ParseError;

/// Failure modes inside the plugin.
#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    /// The engine rejected our attach or disconnected before sending
    /// `attach_ok`.
    #[error("attach failed: {0}")]
    AttachFailed(String),

    /// Stdin closed before we got `attach_ok`.
    #[error("engine closed the stream before attach_ok")]
    AttachClosed,

    /// I/O error talking to stdio or the terminal.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// Wire-format decoding failure.
    #[error("protocol parse error: {0}")]
    Parse(#[from] ParseError),

    /// JSON encoding failure when building an outbound event body.
    #[error("json encode error: {0}")]
    Json(#[from] serde_json::Error),
}
