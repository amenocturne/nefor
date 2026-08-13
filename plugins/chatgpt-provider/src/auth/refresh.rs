//! Refresh-token exchange. Distinct from the initial code exchange:
//! the refresh endpoint takes a JSON body, not form-urlencoded.

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use rand::Rng;
use serde::{Deserialize, Serialize};

use crate::auth::oauth::{CLIENT_ID, REVOKE_URL, TOKEN_URL};
use crate::auth::store::{
    parse_chatgpt_jwt_claims, AccessToken, AuthDotJson, RefreshToken, TokenData,
};
use crate::error::ChatgptError;

/// Hard cap on the revoke HTTP call. The endpoint should respond
/// quickly; if it's down or blocked we don't want logout to hang.
const REVOKE_TIMEOUT: Duration = Duration::from_secs(10);

const REFRESH_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REFRESH_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const REFRESH_RETRY_BUDGET_MS: u64 = 60_000;
const REFRESH_RETRY_BASE_DELAY_MS: u64 = 500;
const REFRESH_RETRY_MAX_DELAY_MS: u64 = 8_000;
const REFRESH_RETRY_JITTER_MS: i64 = 250;

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
    let client = nefor_provider_http::client()?;
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

pub const TOKEN_REFRESH_LEEWAY_SECS: i64 = 5 * 60;
pub const TOKEN_FALLBACK_MAX_AGE_SECS: i64 = 8 * 24 * 60 * 60;

#[derive(Debug, Serialize)]
struct RefreshRequest<'a> {
    client_id: &'a str,
    grant_type: &'a str,
    refresh_token: &'a str,
}

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
}

/// Hit `/oauth/token` with the refresh-token grant; return a fresh
/// `TokenData`. The id_token's `chatgpt_account_id` claim is re-extracted
/// because the auth service may rotate it (workspace switches).
pub async fn refresh_tokens(tokens: &TokenData) -> Result<TokenData, ChatgptError> {
    refresh_tokens_at(tokens, TOKEN_URL).await
}

pub async fn refresh_tokens_at(
    tokens: &TokenData,
    token_url: &str,
) -> Result<TokenData, ChatgptError> {
    let body = RefreshRequest {
        client_id: CLIENT_ID,
        grant_type: "refresh_token",
        refresh_token: &tokens.refresh_token.0,
    };

    let builder = if token_url.starts_with("http://") {
        // Local/test endpoints do not enter the TLS trust boundary.
        reqwest::Client::builder()
    } else {
        nefor_provider_http::client_builder()?.0
    };
    let client = builder.connect_timeout(REFRESH_CONNECT_TIMEOUT).build()?;
    let resp = post_refresh_with_retry(&client, &body, token_url).await?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        let reason = format!("{status}: {text}");
        if is_terminal_refresh_failure(status.as_u16(), &text) {
            return Err(ChatgptError::RefreshFailed(reason));
        }
        return Err(ChatgptError::RefreshTransient(reason));
    }
    let resp: RefreshResponse = resp.json().await?;
    Ok(merge_refresh_response(tokens, resp))
}

fn merge_refresh_response(current: &TokenData, response: RefreshResponse) -> TokenData {
    let account_id = match response.id_token.as_deref() {
        Some(id_token) => {
            parse_chatgpt_jwt_claims(id_token)
                .unwrap_or_default()
                .chatgpt_account_id
        }
        None => current.account_id.clone(),
    };
    TokenData {
        id_token: response
            .id_token
            .unwrap_or_else(|| current.id_token.clone()),
        access_token: response
            .access_token
            .map(AccessToken)
            .unwrap_or_else(|| current.access_token.clone()),
        refresh_token: response
            .refresh_token
            .map(RefreshToken)
            .unwrap_or_else(|| current.refresh_token.clone()),
        account_id,
    }
}

async fn post_refresh_with_retry(
    client: &reqwest::Client,
    body: &RefreshRequest<'_>,
    token_url: &str,
) -> Result<reqwest::Response, ChatgptError> {
    let started = Instant::now();
    let mut attempt: u32 = 0;
    loop {
        let result = client
            .post(token_url)
            .header("Content-Type", "application/json")
            .timeout(REFRESH_REQUEST_TIMEOUT)
            .json(body)
            .send()
            .await;

        match result {
            Ok(resp) if resp.status().is_success() => return Ok(resp),
            Ok(resp) => {
                let status = resp.status();
                if !is_transient_status(status.as_u16()) {
                    return Ok(resp);
                }
                let retry_after = retry_after_seconds(resp.headers());
                let next_delay = retry_delay(attempt, retry_after);
                if !budget_allows(started, next_delay) {
                    return Ok(resp);
                }
                tracing::warn!(
                    attempt = attempt + 1,
                    status = status.as_u16(),
                    delay_ms = next_delay.as_millis() as u64,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "refresh token endpoint transient HTTP failure; retrying",
                );
                tokio::time::sleep(next_delay).await;
                attempt += 1;
            }
            Err(e) => {
                if !is_transient_transport(&e) {
                    return Err(e.into());
                }
                let next_delay = retry_delay(attempt, None);
                if !budget_allows(started, next_delay) {
                    return Err(e.into());
                }
                tracing::warn!(
                    attempt = attempt + 1,
                    error = %e,
                    delay_ms = next_delay.as_millis() as u64,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "refresh token transport failure; retrying",
                );
                tokio::time::sleep(next_delay).await;
                attempt += 1;
            }
        }
    }
}

fn is_transient_status(status: u16) -> bool {
    status == 429 || (500..=599).contains(&status)
}

fn is_terminal_refresh_failure(status: u16, body: &str) -> bool {
    if status == 401 {
        return true;
    }
    if status == 429 || status >= 500 {
        return false;
    }
    let body = body.to_ascii_lowercase();
    [
        "invalid_grant",
        "refresh token is expired",
        "refresh token expired",
        "refresh token was revoked",
        "refresh token revoked",
        "refresh token reused",
        "refresh token was already used",
        "refresh token invalidated",
    ]
    .iter()
    .any(|marker| body.contains(marker))
}

fn is_transient_transport(e: &reqwest::Error) -> bool {
    e.is_connect() || e.is_timeout() || e.is_request()
}

fn retry_after_seconds(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
}

fn budget_allows(started: Instant, next_delay: Duration) -> bool {
    started.elapsed().saturating_add(next_delay) < Duration::from_millis(REFRESH_RETRY_BUDGET_MS)
}

fn retry_delay(attempt: u32, retry_after_sec: Option<u64>) -> Duration {
    if let Some(s) = retry_after_sec {
        return Duration::from_millis(s.saturating_mul(1_000).min(REFRESH_RETRY_MAX_DELAY_MS));
    }
    let shift = attempt.min(32);
    let factor = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
    let base = REFRESH_RETRY_BASE_DELAY_MS.saturating_mul(factor);
    let capped = base.min(REFRESH_RETRY_MAX_DELAY_MS);
    let jitter = rand::thread_rng().gen_range(-REFRESH_RETRY_JITTER_MS..=REFRESH_RETRY_JITTER_MS);
    Duration::from_millis((capped as i64 + jitter).max(0) as u64)
}

/// Refresh five minutes before the access JWT expires. Tokens that are
/// opaque or malformed use Codex's conservative eight-day age fallback.
pub fn needs_refresh(auth: &AuthDotJson, now: DateTime<Utc>) -> bool {
    access_token_expiry(&auth.tokens.access_token)
        .map(|exp| now.timestamp() >= exp.saturating_sub(TOKEN_REFRESH_LEEWAY_SECS))
        .unwrap_or_else(|| {
            now.signed_duration_since(auth.last_refresh).num_seconds()
                >= TOKEN_FALLBACK_MAX_AGE_SECS
        })
}

fn access_token_expiry(token: &AccessToken) -> Option<i64> {
    crate::auth::store::decode_jwt_payload(&token.0)
        .ok()?
        .get("exp")?
        .as_i64()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::store::{AccessToken, RefreshToken, TokenData};
    use base64::Engine;
    use chrono::Duration;

    fn jwt_with_exp(exp: i64) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{}");
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#));
        format!("{header}.{payload}.sig")
    }

    fn auth(access_token: String, last_refresh: DateTime<Utc>) -> AuthDotJson {
        AuthDotJson {
            tokens: TokenData {
                id_token: "x".into(),
                access_token: AccessToken(access_token),
                refresh_token: RefreshToken("r".into()),
                account_id: None,
            },
            last_refresh,
        }
    }

    #[test]
    fn jwt_expiry_refreshes_five_minutes_early() {
        let now = Utc::now();
        let fresh = auth(jwt_with_exp(now.timestamp() + 301), now);
        let expiring = auth(jwt_with_exp(now.timestamp() + 300), now);
        assert!(!needs_refresh(&fresh, now));
        assert!(needs_refresh(&expiring, now));
    }

    #[test]
    fn malformed_jwt_uses_eight_day_fallback() {
        let now = Utc::now();
        let fresh = auth("opaque".into(), now - Duration::days(7));
        let old = auth("opaque".into(), now - Duration::days(8));
        assert!(!needs_refresh(&fresh, now));
        assert!(needs_refresh(&old, now));
    }

    #[test]
    fn retry_after_seconds_parses_numeric_header() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(reqwest::header::RETRY_AFTER, "12".parse().unwrap());
        assert_eq!(retry_after_seconds(&h), Some(12));
    }

    #[test]
    fn retry_after_seconds_ignores_invalid_header() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(reqwest::header::RETRY_AFTER, "soon".parse().unwrap());
        assert_eq!(retry_after_seconds(&h), None);
    }

    #[test]
    fn transient_status_includes_rate_limit_and_gateway_failures() {
        assert!(is_transient_status(429));
        assert!(is_transient_status(502));
        assert!(is_transient_status(503));
        assert!(is_transient_status(504));
        assert!(is_transient_status(500));
        assert!(is_transient_status(599));
        assert!(!is_transient_status(401));
        assert!(!is_transient_status(400));
    }

    #[test]
    fn terminal_refresh_failure_requires_401_or_known_invalid_token_reason() {
        assert!(is_terminal_refresh_failure(401, "anything"));
        assert!(is_terminal_refresh_failure(400, "invalid_grant"));
        assert!(is_terminal_refresh_failure(
            400,
            "refresh token was already used"
        ));
        assert!(!is_terminal_refresh_failure(
            400,
            "temporary policy failure"
        ));
        assert!(!is_terminal_refresh_failure(403, "forbidden"));
        assert!(!is_terminal_refresh_failure(500, "invalid_grant"));
    }

    #[test]
    fn partial_refresh_response_preserves_omitted_tokens() {
        let current = TokenData {
            id_token: "old-id".into(),
            access_token: AccessToken("old-access".into()),
            refresh_token: RefreshToken("old-refresh".into()),
            account_id: Some(crate::auth::store::ChatgptAccountId("acct".into())),
        };
        let merged = merge_refresh_response(
            &current,
            RefreshResponse {
                id_token: None,
                access_token: Some("new-access".into()),
                refresh_token: None,
            },
        );
        assert_eq!(merged.id_token, "old-id");
        assert_eq!(merged.access_token, AccessToken("new-access".into()));
        assert_eq!(merged.refresh_token, RefreshToken("old-refresh".into()));
        assert_eq!(merged.account_id, current.account_id);
    }

    #[test]
    fn empty_refresh_response_keeps_complete_current_bundle() {
        let current = TokenData {
            id_token: "old-id".into(),
            access_token: AccessToken("old-access".into()),
            refresh_token: RefreshToken("old-refresh".into()),
            account_id: None,
        };
        let merged = merge_refresh_response(
            &current,
            RefreshResponse {
                id_token: None,
                access_token: None,
                refresh_token: None,
            },
        );
        assert_eq!(merged, current);
    }

    #[test]
    fn returned_id_token_without_account_claim_clears_old_account_id() {
        let current = TokenData {
            id_token: "old-id".into(),
            access_token: AccessToken("old-access".into()),
            refresh_token: RefreshToken("old-refresh".into()),
            account_id: Some(crate::auth::store::ChatgptAccountId("old-account".into())),
        };
        let merged = merge_refresh_response(
            &current,
            RefreshResponse {
                id_token: Some("e30.e30.sig".into()),
                access_token: None,
                refresh_token: None,
            },
        );
        assert_eq!(merged.id_token, "e30.e30.sig");
        assert_eq!(merged.account_id, None);
    }

    #[test]
    fn retry_delay_caps_retry_after() {
        assert_eq!(
            retry_delay(0, Some(999)).as_millis(),
            REFRESH_RETRY_MAX_DELAY_MS as u128
        );
    }
}
