//! Per-request header construction for the Responses endpoint.
//!
//! `Content-Type: application/json` is set by reqwest when we call
//! `.json(...)` on the request builder — we don't insert it here.

use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, ACCEPT_ENCODING, AUTHORIZATION, USER_AGENT};

use crate::auth::AuthSnapshot;
use crate::error::ChatgptError;

/// Header name for codex-style installation tracking. Codex normalizes
/// to all-lowercase on the wire (see
/// `codex-rs/core/src/client.rs::X_CODEX_INSTALLATION_ID_HEADER`); HTTP
/// headers are case-insensitive but we match exactly for parity.
pub const X_CODEX_INSTALLATION_ID: &str = "x-codex-installation-id";

/// Stable conversation routing identifier. Both Codex and OpenCode send
/// this on every Responses request.
pub const SESSION_ID: &str = "session-id";

/// Stable logical thread identifier. Codex sends this alongside
/// `session-id` and uses the same thread identity for prompt caching.
pub const THREAD_ID: &str = "thread-id";

/// Server-issued sticky-routing token, replayed only within the turn that
/// received it.
pub const X_CODEX_TURN_STATE: &str = "x-codex-turn-state";

/// ChatGPT-side account routing. Only present when the JWT id_token
/// carried an `auth.chatgpt_account_id` claim (Plus / Team / Enterprise).
pub const CHATGPT_ACCOUNT_ID: &str = "chatgpt-account-id";

/// Originator identifier — codex sends `codex_cli_rs`; nefor sends
/// `nefor_cli_rs` so server-side traffic shaping can tell us apart.
pub const ORIGINATOR: &str = "originator";

/// Default User-Agent. Cargo picks up the crate version at compile
/// time; bumping the package version automatically bumps this string.
pub fn default_user_agent() -> String {
    format!("nefor-chatgpt-provider/{}", env!("CARGO_PKG_VERSION"))
}

/// Build the header set for a Responses POST.
///
/// Returns `ChatgptError::NoTokens` when `auth` has no tokens — caller
/// should drive the login flow first. Returns `InvalidHeader` only if
/// the token bytes are themselves invalid for an HTTP header value,
/// which should be impossible with real OAuth tokens but we don't
/// `unwrap()`.
pub fn build_headers(
    auth: &AuthSnapshot,
    installation_id: &str,
    originator: &str,
) -> Result<HeaderMap, ChatgptError> {
    let tokens = auth.tokens.as_ref().ok_or(ChatgptError::NoTokens)?;

    let mut headers = HeaderMap::new();

    let bearer = format!("Bearer {}", tokens.access_token);
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&bearer).map_err(|e| ChatgptError::InvalidHeader(e.to_string()))?,
    );

    if let Some(account_id) = &tokens.account_id {
        headers.insert(
            CHATGPT_ACCOUNT_ID,
            HeaderValue::from_str(&account_id.0)
                .map_err(|e| ChatgptError::InvalidHeader(e.to_string()))?,
        );
    }

    headers.insert(
        ORIGINATOR,
        HeaderValue::from_str(originator)
            .map_err(|e| ChatgptError::InvalidHeader(e.to_string()))?,
    );

    headers.insert(
        X_CODEX_INSTALLATION_ID,
        HeaderValue::from_str(installation_id)
            .map_err(|e| ChatgptError::InvalidHeader(e.to_string()))?,
    );

    headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
    headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));

    headers.insert(
        USER_AGENT,
        HeaderValue::from_str(&default_user_agent())
            .map_err(|e| ChatgptError::InvalidHeader(e.to_string()))?,
    );

    Ok(headers)
}

pub fn add_turn_headers(
    headers: &mut HeaderMap,
    session_id: &str,
    thread_id: &str,
    turn_state: Option<&str>,
) -> Result<(), ChatgptError> {
    headers.insert(
        SESSION_ID,
        HeaderValue::from_str(session_id)
            .map_err(|e| ChatgptError::InvalidHeader(e.to_string()))?,
    );
    headers.insert(
        THREAD_ID,
        HeaderValue::from_str(thread_id).map_err(|e| ChatgptError::InvalidHeader(e.to_string()))?,
    );
    if let Some(turn_state) = turn_state {
        headers.insert(
            X_CODEX_TURN_STATE,
            HeaderValue::from_str(turn_state)
                .map_err(|e| ChatgptError::InvalidHeader(e.to_string()))?,
        );
    }
    Ok(())
}
