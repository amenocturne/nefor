//! AuthStore state-machine tests. The OAuth network paths are not
//! exercised here — wiremock is out of scope for Phase 2.

use base64::Engine;
use chrono::Utc;
use tempfile::tempdir;

use chatgpt_provider::auth::store::{
    parse_chatgpt_jwt_claims, save, AccessToken, AuthDotJson, ChatgptAccountId, RefreshToken,
    TokenData,
};
use chatgpt_provider::auth::{AuthState, AuthStore};

fn dummy_tokens() -> TokenData {
    TokenData {
        id_token: "header.payload.sig".into(),
        access_token: AccessToken("acc".into()),
        refresh_token: RefreshToken("ref".into()),
        account_id: Some(ChatgptAccountId("acct-123".into())),
    }
}

#[tokio::test]
async fn load_from_missing_file_yields_login_required() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("auth.json");
    let store = AuthStore::load_from_disk(&path).await.expect("load");
    let snap = store.snapshot().await;
    assert_eq!(snap.state, AuthState::LoginRequired);
    assert!(snap.tokens.is_none());
}

#[tokio::test]
async fn load_from_existing_file_yields_connected() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("auth.json");
    let auth = AuthDotJson {
        tokens: dummy_tokens(),
        last_refresh: Utc::now(),
    };
    save(&path, &auth).expect("save");

    let store = AuthStore::load_from_disk(&path).await.expect("load");
    let snap = store.snapshot().await;
    assert_eq!(snap.state, AuthState::Connected);
    assert_eq!(
        snap.tokens.as_ref().map(|t| t.access_token.clone()),
        Some(AccessToken("acc".into()))
    );
}

#[tokio::test]
async fn apply_login_result_persists_and_transitions_to_connected() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("auth.json");
    let store = AuthStore::load_from_disk(&path).await.expect("load");

    store
        .apply_login_result(dummy_tokens())
        .await
        .expect("apply");

    let snap = store.snapshot().await;
    assert_eq!(snap.state, AuthState::Connected);
    assert!(snap.tokens.is_some());

    // Reload from disk in a new store — should be Connected with the
    // same token data, proving persistence round-trips.
    let reloaded = AuthStore::load_from_disk(&path).await.expect("reload");
    let snap2 = reloaded.snapshot().await;
    assert_eq!(snap2.state, AuthState::Connected);
    assert_eq!(snap2.tokens, snap.tokens);
}

#[tokio::test]
async fn current_access_token_fails_when_no_tokens() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("auth.json");
    let store = AuthStore::load_from_disk(&path).await.expect("load");
    let result = store.current_access_token().await;
    assert!(result.is_err(), "expected NoTokens error");
}

#[tokio::test]
async fn current_access_token_returns_fresh_token() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("auth.json");
    let auth = AuthDotJson {
        tokens: dummy_tokens(),
        last_refresh: Utc::now(),
    };
    save(&path, &auth).expect("save");
    let store = AuthStore::load_from_disk(&path).await.expect("load");
    let token = store.current_access_token().await.expect("token");
    assert_eq!(token, AccessToken("acc".into()));
}

/// Hand-craft an unsigned JWT carrying the OpenAI auth claim
/// namespace, verify `parse_chatgpt_jwt_claims` lifts the
/// `chatgpt_account_id` out of it.
#[test]
fn jwt_claim_extraction_pulls_account_id() {
    let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{\"alg\":\"none\"}");
    let payload = serde_json::json!({
        "https://api.openai.com/auth": {
            "chatgpt_account_id": "acct-from-jwt-xyz"
        }
    });
    let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&payload).unwrap());
    let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"sig");
    let jwt = format!("{header}.{payload_b64}.{sig}");

    let claims = parse_chatgpt_jwt_claims(&jwt).expect("parse");
    assert_eq!(
        claims.chatgpt_account_id,
        Some(ChatgptAccountId("acct-from-jwt-xyz".into()))
    );
}

#[test]
fn jwt_claim_extraction_absent_account_id_returns_none() {
    let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{\"alg\":\"none\"}");
    let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{}");
    let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"sig");
    let jwt = format!("{header}.{payload_b64}.{sig}");

    let claims = parse_chatgpt_jwt_claims(&jwt).expect("parse");
    assert_eq!(claims.chatgpt_account_id, None);
}

#[test]
fn malformed_jwt_errors() {
    let result = parse_chatgpt_jwt_claims("not-a-jwt");
    assert!(result.is_err());
}
