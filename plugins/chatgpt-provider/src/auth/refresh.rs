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

    let client = reqwest::Client::builder()
        .connect_timeout(REFRESH_CONNECT_TIMEOUT)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let resp = post_refresh_with_retry(&client, &body).await?;

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

async fn post_refresh_with_retry(
    client: &reqwest::Client,
    body: &RefreshRequest<'_>,
) -> Result<reqwest::Response, ChatgptError> {
    let started = Instant::now();
    let mut attempt: u32 = 0;
    loop {
        let result = client
            .post(TOKEN_URL)
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
    matches!(status, 429 | 502..=504)
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
        assert!(!is_transient_status(401));
        assert!(!is_transient_status(400));
    }

    #[test]
    fn retry_delay_caps_retry_after() {
        assert_eq!(
            retry_delay(0, Some(999)).as_millis(),
            REFRESH_RETRY_MAX_DELAY_MS as u128
        );
    }
}
