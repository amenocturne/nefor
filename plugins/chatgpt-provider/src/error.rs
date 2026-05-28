//! Domain errors for chatgpt-provider.

#[derive(Debug, thiserror::Error)]
pub enum ChatgptError {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("HTTP transport error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),

    /// JWT claim payload could not be base64url-decoded.
    #[error("invalid base64 in JWT: {0}")]
    Base64(#[from] base64::DecodeError),

    /// JWT did not have three dot-separated segments, or the payload
    /// segment was missing.
    #[error("malformed JWT: {0}")]
    MalformedJwt(&'static str),

    /// Loopback callback server bind failed on both the preferred and
    /// fallback ports.
    #[error("could not bind loopback OAuth callback server: {0}")]
    BindFailed(String),

    /// The OAuth `state` parameter on the callback did not match the
    /// value we sent in the authorize URL — treat as a possible CSRF
    /// attempt and abort.
    #[error("OAuth state parameter mismatch on /auth/callback")]
    StateMismatch,

    /// Server returned the `error` query param on the callback (e.g.
    /// `access_denied`).
    #[error("OAuth callback returned error: {0}")]
    CallbackError(String),

    /// Token endpoint returned a non-2xx response.
    #[error("token endpoint returned {status}: {body}")]
    TokenEndpoint { status: u16, body: String },

    /// `data_dir` could not be resolved — neither `$NEFOR_DATA_DIR` is
    /// set nor does the platform expose a user data directory.
    #[error("could not resolve $NEFOR_DATA_DIR or platform data dir")]
    DataDirUnavailable,

    /// `current_access_token()` was called but the store is in
    /// `LoginRequired` — caller must drive the OAuth flow first.
    #[error("no tokens on disk; run `chatgpt-provider login` first")]
    NoTokens,

    /// Refresh-token exchange failed (likely refresh token expired or
    /// revoked).
    #[error("refresh failed: {0}")]
    RefreshFailed(String),

    /// The Responses endpoint returned a non-2xx status with the body
    /// captured for diagnostics. Surfaced before any SSE frame is
    /// yielded.
    #[error("responses endpoint returned {status}: {body}")]
    ResponsesEndpoint { status: u16, body: String },

    /// Mid-stream transport read failure (TCP reset, idle timeout,
    /// chunked decoder error). Safe to retry only before an attempt has
    /// emitted any user-visible output.
    #[error("responses SSE stream read error: {0}")]
    ResponsesStreamRead(String),

    /// A complete SSE frame arrived, but did not match the schema this
    /// provider understands. Retrying the same request is unlikely to
    /// change the payload shape, so callers should surface this.
    #[error("responses SSE stream parse error: {0}")]
    ResponsesStreamParse(String),

    /// `Authorization: Bearer ...` could not be constructed because the
    /// access token contained bytes that aren't valid in an HTTP header
    /// value. Shouldn't happen with real OAuth tokens — defensive.
    #[error("could not build Authorization header: {0}")]
    InvalidHeader(String),

    /// NCP transport failure (handshake, parse, writer closed).
    #[error(transparent)]
    Transport(#[from] nefor_plugin_sdk::TransportError),

    /// Bubbled out of a chats-map operation. The dispatcher catches and
    /// translates these into wire-level error events; surfacing it as a
    /// top-level error variant means production code paths don't have
    /// to `unwrap` a Result<_, ChatsError>.
    #[error("chat operation failed: {0}")]
    Chats(#[from] crate::state::ChatsError),
}
