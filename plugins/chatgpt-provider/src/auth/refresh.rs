//! Refresh-token exchange. Distinct from the initial code exchange:
//! the refresh endpoint takes a JSON body, not form-urlencoded.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::auth::oauth::{CLIENT_ID, REVOKE_URL, TOKEN_URL};
use crate::auth::store::{
    parse_chatgpt_jwt_claims, AccessToken, AuthDotJson, RefreshToken, TokenData,
};
use crate::error::ChatgptError;

/// Hard cap on the revoke HTTP call. The endpoint should respond
/// quickly; if it's down or blocked we don't want logout to hang.
const REVOKE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Serialize)]
struct RevokeRequest<'a> {
    token: &'a str,
    token_type_hint: &'a str,
    client_id: &'a str,
}

/// POST `/oauth/revoke` with the refresh-token grant. Revoking the
/// refresh token invalidates the whole token tree on OpenAI's side
/// (access tokens derived from it stop working at their next 401).
/// Non-success responses are returned as errors but callers typically
/// log + ignore them: the local-side cleanup happens regardless.
pub async fn revoke_tokens(refresh_token: &RefreshToken) -> Result<(), ChatgptError> {
    let body = RevokeRequest {
        token: &refresh_token.0,
        token_type_hint: "refresh_token",
        client_id: CLIENT_ID,
    };
    let client = reqwest::Client::new();
    let resp = client
        .post(REVOKE_URL)
        .header("Content-Type", "application/json")
        .timeout(REVOKE_TIMEOUT)
        .json(&body)
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(ChatgptError::RefreshFailed(format!(
            "revoke {status}: {text}"
        )));
    }
    Ok(())
}

/// Refresh if the persisted token is older than `max_age_secs - leeway`.
///
/// We don't have a hard expiry timestamp on disk (the access token's
/// JWT `exp` claim *does* have one, but reading it pulls in an extra
/// parsing path; not worth it for Phase 2). Instead, treat
/// `last_refresh + max_age - leeway` as the trigger — codex's refresh
/// interval is 8 minutes, well inside the access-token lifetime.
pub const TOKEN_REFRESH_LEEWAY_SECS: i64 = 60;

#[derive(Debug, Serialize)]
struct RefreshRequest<'a> {
    client_id: &'a str,
    grant_type: &'a str,
    refresh_token: &'a str,
}

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    id_token: String,
    access_token: String,
    refresh_token: String,
}

/// Hit `/oauth/token` with the refresh-token grant; return a fresh
/// `TokenData`. The id_token's `chatgpt_account_id` claim is re-extracted
/// because the auth service may rotate it (workspace switches).
pub async fn refresh_tokens(refresh_token: &RefreshToken) -> Result<TokenData, ChatgptError> {
    let body = RefreshRequest {
        client_id: CLIENT_ID,
        grant_type: "refresh_token",
        refresh_token: &refresh_token.0,
    };

    let client = reqwest::Client::new();
    let resp = client
        .post(TOKEN_URL)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(ChatgptError::RefreshFailed(format!("{status}: {text}")));
    }
    let resp: RefreshResponse = resp.json().await?;

    let claims = parse_chatgpt_jwt_claims(&resp.id_token).unwrap_or_default();
    Ok(TokenData {
        id_token: resp.id_token,
        access_token: AccessToken(resp.access_token),
        refresh_token: RefreshToken(resp.refresh_token),
        account_id: claims.chatgpt_account_id,
    })
}

/// True when `last_refresh` is older than `max_age_secs - leeway` from
/// `now`. Callers decide what max-age window they want — Phase 2 uses
/// 28 minutes (codex defaults to 8min, but Responses access tokens are
/// usually good for ~60min and we want some slack).
pub fn is_expired(auth: &AuthDotJson, now: DateTime<Utc>, max_age_secs: i64) -> bool {
    let age = now.signed_duration_since(auth.last_refresh).num_seconds();
    age >= (max_age_secs - TOKEN_REFRESH_LEEWAY_SECS).max(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::store::{AccessToken, RefreshToken, TokenData};
    use chrono::Duration;

    fn auth(last_refresh: DateTime<Utc>) -> AuthDotJson {
        AuthDotJson {
            tokens: TokenData {
                id_token: "x".into(),
                access_token: AccessToken("a".into()),
                refresh_token: RefreshToken("r".into()),
                account_id: None,
            },
            last_refresh,
        }
    }

    #[test]
    fn fresh_token_not_expired() {
        let now = Utc::now();
        let a = auth(now);
        assert!(!is_expired(&a, now, 600));
    }

    #[test]
    fn old_token_expired() {
        let now = Utc::now();
        let a = auth(now - Duration::seconds(700));
        assert!(is_expired(&a, now, 600));
    }

    #[test]
    fn leeway_triggers_early_refresh() {
        let now = Utc::now();
        // 600 - 60 leeway = trigger at 540s. At 550s old, should be expired.
        let a = auth(now - Duration::seconds(550));
        assert!(is_expired(&a, now, 600));
    }
}
