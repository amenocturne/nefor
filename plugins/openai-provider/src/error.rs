//! Domain errors for openai-provider.

use openai_provider::state::ChatsError;

/// All failure modes inside openai-provider.
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error(transparent)]
    ProviderHttp(#[from] nefor_provider_http::ProviderHttpError),

    #[error("HTTP client build failed: {0}")]
    Http(#[from] reqwest::Error),
    /// NCP transport failure (I/O, handshake, parse, writer closed).
    #[error(transparent)]
    Transport(#[from] nefor_plugin_sdk::TransportError),

    /// Bubbled out of a chats-map operation. The dispatcher catches and
    /// translates these into wire-level error events; surfacing them as
    /// a top-level error variant means we don't have to `unwrap` in
    /// production code paths.
    #[error("chat operation failed: {0}")]
    Chats(#[from] ChatsError),
}
