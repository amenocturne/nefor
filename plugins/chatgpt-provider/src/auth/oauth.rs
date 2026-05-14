//! Interactive OAuth PKCE flow against `auth.openai.com`.
//!
//! Mirrors the codex CLI flow: spin up a tiny loopback HTTP server on
//! port 1455 (fallback 1457), open the user's browser at the authorize
//! URL, capture `?code=&state=` on `/auth/callback`, then POST the code
//! to `/oauth/token` for the token bundle.

use std::collections::HashMap;
use std::io::Cursor;
use std::net::SocketAddr;
use std::path::Path;
use std::thread;

use chrono::Utc;
use serde::Deserialize;
use tiny_http::{Response, Server};
use tokio::sync::oneshot;
use tracing::{info, warn};

use crate::auth::pkce::{generate_pkce, generate_state, PkceCodes};
use crate::auth::store::{
    parse_chatgpt_jwt_claims, save, AccessToken, AuthDotJson, RefreshToken, TokenData,
};
use crate::error::ChatgptError;

pub const ISSUER: &str = "https://auth.openai.com";
pub const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const REVOKE_URL: &str = "https://auth.openai.com/oauth/revoke";
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const DEFAULT_PORT: u16 = 1455;
pub const FALLBACK_PORT: u16 = 1457;
pub const SCOPE: &str =
    "openid profile email offline_access api.connectors.read api.connectors.invoke";
pub const ORIGINATOR: &str = "nefor_cli_rs";

/// Run the full login flow and persist tokens to `auth_path`.
///
/// `open_browser` is `false` in CI / SSH scenarios where the URL is
/// copied manually; we print it to stderr in that case.
pub async fn run_login(open_browser: bool, auth_path: &Path) -> Result<TokenData, ChatgptError> {
    let pkce = generate_pkce();
    let state = generate_state();

    let (server, actual_port) = bind_server()?;
    let redirect_uri = format!("http://localhost:{actual_port}/auth/callback");
    let auth_url = build_authorize_url(&redirect_uri, &pkce, &state);

    if open_browser {
        if webbrowser::open(&auth_url).is_err() {
            warn!("failed to open browser; copy this URL manually:\n{auth_url}");
            eprintln!("Open this URL to continue login:\n{auth_url}");
        }
    } else {
        eprintln!("Open this URL to continue login:\n{auth_url}");
    }

    let code = wait_for_callback(server, &state).await?;
    let tokens = exchange_code_for_tokens(&redirect_uri, &pkce, &code).await?;

    let claims = parse_chatgpt_jwt_claims(&tokens.id_token).unwrap_or_default();
    let token_data = TokenData {
        id_token: tokens.id_token,
        access_token: AccessToken(tokens.access_token),
        refresh_token: RefreshToken(tokens.refresh_token),
        account_id: claims.chatgpt_account_id,
    };

    let auth = AuthDotJson {
        tokens: token_data.clone(),
        last_refresh: Utc::now(),
    };
    save(auth_path, &auth)?;
    info!(path = %auth_path.display(), "persisted ChatGPT OAuth tokens");

    Ok(token_data)
}

/// Try the preferred port first, fall back if it's already bound.
/// Returns the bound server plus the port it actually got (the caller
/// needs the port to build `redirect_uri`, since the auth service
/// validates `redirect_uri` against the exact value sent in the
/// authorize URL).
fn bind_server() -> Result<(Server, u16), ChatgptError> {
    for port in [DEFAULT_PORT, FALLBACK_PORT] {
        let addr = format!("127.0.0.1:{port}");
        match Server::http(&addr) {
            Ok(server) => {
                let actual = match server.server_addr().to_ip() {
                    Some(SocketAddr::V4(a)) => a.port(),
                    Some(SocketAddr::V6(a)) => a.port(),
                    None => port,
                };
                return Ok((server, actual));
            }
            Err(err) => {
                warn!(%port, error = %err, "failed to bind loopback server, trying next port");
            }
        }
    }
    Err(ChatgptError::BindFailed(format!(
        "both {DEFAULT_PORT} and {FALLBACK_PORT} are unavailable"
    )))
}

pub fn build_authorize_url(redirect_uri: &str, pkce: &PkceCodes, state: &str) -> String {
    let query = [
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", redirect_uri),
        ("scope", SCOPE),
        ("code_challenge", &pkce.code_challenge),
        ("code_challenge_method", "S256"),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("state", state),
        ("originator", ORIGINATOR),
    ];
    let qs = query
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{AUTHORIZE_URL}?{qs}")
}

/// Block on the loopback server (in a thread) until `/auth/callback`
/// arrives, then return the `code`. State mismatch and OAuth error
/// responses fail fast.
async fn wait_for_callback(server: Server, expected_state: &str) -> Result<String, ChatgptError> {
    let expected_state = expected_state.to_string();
    let (tx, rx) = oneshot::channel::<Result<String, ChatgptError>>();

    // tiny_http is blocking; park it on its own OS thread and bridge to
    // tokio via oneshot. Bounded to a single callback — we exit the
    // loop as soon as we hand a result to the channel.
    thread::spawn(move || {
        let mut tx_slot = Some(tx);
        while let Ok(req) = server.recv() {
            let url = req.url().to_string();
            // Build a fully-qualified URL so url::Url can parse it.
            // Avoid pulling the `url` crate in just for this — split the
            // query off by hand.
            let (path, query) = split_path_query(&url);
            if path != "/auth/callback" {
                let _ = req.respond(Response::from_string("not found").with_status_code(404));
                continue;
            }
            let params = parse_query(query);
            let result = interpret_callback(&params, &expected_state);
            let body = match &result {
                Ok(_) => "Login complete. You can close this tab.",
                Err(_) => "Login failed. Check the terminal for details.",
            };
            let resp = Response::new(
                200.into(),
                vec![],
                Cursor::new(body.as_bytes().to_vec()),
                None,
                None,
            );
            let _ = req.respond(resp);

            if let Some(tx) = tx_slot.take() {
                let _ = tx.send(result);
            }
            break;
        }
    });

    rx.await.map_err(|_| {
        ChatgptError::BindFailed("callback server thread exited before delivering result".into())
    })?
}

fn split_path_query(url: &str) -> (&str, &str) {
    match url.split_once('?') {
        Some((p, q)) => (p, q),
        None => (url, ""),
    }
}

fn parse_query(q: &str) -> HashMap<String, String> {
    q.split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            let kd = urlencoding::decode(k).ok()?.into_owned();
            let vd = urlencoding::decode(v).ok()?.into_owned();
            Some((kd, vd))
        })
        .collect()
}

fn interpret_callback(
    params: &HashMap<String, String>,
    expected_state: &str,
) -> Result<String, ChatgptError> {
    if let Some(err) = params.get("error") {
        let desc = params
            .get("error_description")
            .map(String::as_str)
            .unwrap_or("");
        return Err(ChatgptError::CallbackError(format!("{err}: {desc}")));
    }
    let state = params.get("state").map(String::as_str).unwrap_or("");
    if state != expected_state {
        return Err(ChatgptError::StateMismatch);
    }
    let code = params
        .get("code")
        .filter(|c| !c.is_empty())
        .ok_or_else(|| ChatgptError::CallbackError("missing code parameter".into()))?;
    Ok(code.clone())
}

#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
}

/// POST to `/oauth/token` with the auth code, return the raw token
/// strings. The auth service rejects this exchange unless `redirect_uri`
/// matches the exact value sent in the authorize URL (port included),
/// which is why we plumb the actual bound port through.
pub async fn exchange_code_for_tokens(
    redirect_uri: &str,
    pkce: &PkceCodes,
    code: &str,
) -> Result<TokenResponse, ChatgptError> {
    let body = format!(
        "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
        urlencoding::encode(code),
        urlencoding::encode(redirect_uri),
        urlencoding::encode(CLIENT_ID),
        urlencoding::encode(&pkce.code_verifier),
    );

    let client = reqwest::Client::new();
    let resp = client
        .post(TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(ChatgptError::TokenEndpoint {
            status: status.as_u16(),
            body,
        });
    }
    let parsed: TokenResponse = resp.json().await?;
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::pkce::generate_pkce;

    #[test]
    fn authorize_url_contains_required_params() {
        let pkce = generate_pkce();
        let url = build_authorize_url("http://localhost:1455/auth/callback", &pkce, "deadbeef");
        assert!(url.starts_with(AUTHORIZE_URL));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("originator=nefor_cli_rs"));
        assert!(url.contains("state=deadbeef"));
        // SCOPE has spaces — must be percent-encoded.
        assert!(url.contains("scope=openid%20profile%20email"));
    }

    #[test]
    fn parse_query_handles_url_encoding() {
        let q = "code=abc%20def&state=xyz";
        let map = parse_query(q);
        assert_eq!(map.get("code"), Some(&"abc def".to_string()));
        assert_eq!(map.get("state"), Some(&"xyz".to_string()));
    }

    #[test]
    fn interpret_callback_state_mismatch() {
        let mut p = HashMap::new();
        p.insert("code".into(), "c".into());
        p.insert("state".into(), "wrong".into());
        match interpret_callback(&p, "right") {
            Err(ChatgptError::StateMismatch) => {}
            other => panic!("expected StateMismatch, got {other:?}"),
        }
    }

    #[test]
    fn interpret_callback_returns_oauth_error() {
        let mut p = HashMap::new();
        p.insert("error".into(), "access_denied".into());
        p.insert("state".into(), "x".into());
        match interpret_callback(&p, "x") {
            Err(ChatgptError::CallbackError(msg)) => assert!(msg.contains("access_denied")),
            other => panic!("expected CallbackError, got {other:?}"),
        }
    }

    #[test]
    fn interpret_callback_happy_path() {
        let mut p = HashMap::new();
        p.insert("code".into(), "the-code".into());
        p.insert("state".into(), "match".into());
        let code = interpret_callback(&p, "match").expect("should succeed");
        assert_eq!(code, "the-code");
    }
}
