//! On-disk token persistence and JWT claim extraction.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use base64::Engine;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::ChatgptError;

/// Bearer access token for the Responses API.
///
/// Wrapped so callers can't accidentally swap an access token, refresh
/// token, or id_token around — all three are JWTs and look identical at
/// the type level otherwise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AccessToken(pub String);

impl std::fmt::Display for AccessToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Long-lived token used to mint new access tokens via
/// `/oauth/token` (grant_type=refresh_token).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RefreshToken(pub String);

impl std::fmt::Display for RefreshToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// OpenAI workspace/account identifier extracted from the id_token's
/// `https://api.openai.com/auth.chatgpt_account_id` claim. Required by
/// the Responses API on the `chatgpt-account-id` header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChatgptAccountId(pub String);

impl std::fmt::Display for ChatgptAccountId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What we persist on disk after a successful login or refresh.
///
/// `id_token` is kept raw so future code can re-extract claims (e.g.
/// plan type, fedramp flag) without storing every possible field in
/// this struct.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenData {
    pub id_token: String,
    pub access_token: AccessToken,
    pub refresh_token: RefreshToken,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<ChatgptAccountId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthDotJson {
    pub tokens: TokenData,
    pub last_refresh: DateTime<Utc>,
}

/// Subset of id_token claims relevant to this plugin. Codex's
/// `IdTokenInfo` has more fields (plan type, fedramp, email) — we
/// extract only what Phase 2 needs and leave room to grow.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IdTokenInfo {
    pub chatgpt_account_id: Option<ChatgptAccountId>,
}

/// Resolve the on-disk path for the auth file.
///
/// Priority: `$NEFOR_DATA_DIR/chatgpt-auth.json` →
/// `dirs::data_dir()/nefor/chatgpt-auth.json`.
pub fn default_auth_path() -> Result<PathBuf, ChatgptError> {
    if let Ok(dir) = std::env::var("NEFOR_DATA_DIR") {
        return Ok(PathBuf::from(dir).join("chatgpt-auth.json"));
    }
    let base = dirs::data_dir().ok_or(ChatgptError::DataDirUnavailable)?;
    Ok(base.join("nefor").join("chatgpt-auth.json"))
}

/// Read and deserialize the auth file. `None` if the file does not
/// exist — first-run state, not an error.
pub fn load(path: &Path) -> Result<Option<AuthDotJson>, ChatgptError> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let parsed: AuthDotJson = serde_json::from_slice(&bytes)?;
            Ok(Some(parsed))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

/// Write the auth file atomically-ish (truncate + write). The parent
/// directory is created if missing. On Unix the mode is `0600` so other
/// users can't read tokens; the cfg-guard keeps Windows builds
/// compiling (mode bits there are no-ops at this API level).
pub fn save(path: &Path, auth: &AuthDotJson) -> Result<(), ChatgptError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }

    let mut file = opts.open(path)?;
    let bytes = serde_json::to_vec_pretty(auth)?;
    file.write_all(&bytes)?;
    Ok(())
}

/// Pull the middle JWT segment and base64url-decode it to JSON. JWTs
/// produced by auth.openai.com are unpadded, so URL_SAFE_NO_PAD is the
/// right engine here.
pub fn decode_jwt_payload(jwt: &str) -> Result<serde_json::Value, ChatgptError> {
    let mut parts = jwt.split('.');
    let (Some(_h), Some(payload), Some(_s)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(ChatgptError::MalformedJwt(
            "expected three dot-separated segments",
        ));
    };
    if payload.is_empty() {
        return Err(ChatgptError::MalformedJwt("empty payload segment"));
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload)?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    Ok(value)
}

/// Walk the OpenAI auth claim namespace and pull
/// `chatgpt_account_id`. Returns an `IdTokenInfo` even when the claim
/// is absent — that's a legitimate state (free-tier accounts may not
/// have one) and we don't want to fail the whole login on it.
pub fn parse_chatgpt_jwt_claims(jwt: &str) -> Result<IdTokenInfo, ChatgptError> {
    let value = decode_jwt_payload(jwt)?;
    let account_id = value
        .get("https://api.openai.com/auth")
        .and_then(|v| v.get("chatgpt_account_id"))
        .and_then(|v| v.as_str())
        .map(|s| ChatgptAccountId(s.to_string()));
    Ok(IdTokenInfo {
        chatgpt_account_id: account_id,
    })
}
