//! AuthStore state-machine tests. The OAuth network paths are not
//! exercised here — wiremock is out of scope for Phase 2.

use base64::Engine;
use chrono::{Duration, Utc};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tempfile::tempdir;

use chatgpt_provider::auth::store::{
    parse_chatgpt_jwt_claims, save, AccessToken, AuthDotJson, ChatgptAccountId, RefreshToken,
    TokenData,
};
use chatgpt_provider::auth::{AuthState, AuthStore, LoginLease, LoginStartOutcome};

fn dummy_tokens() -> TokenData {
    TokenData {
        id_token: "header.payload.sig".into(),
        access_token: AccessToken("acc".into()),
        refresh_token: RefreshToken("ref".into()),
        account_id: Some(ChatgptAccountId("acct-123".into())),
    }
}

async fn begin_login(store: &AuthStore) -> LoginLease {
    match store.begin_login().await {
        LoginStartOutcome::Started(lease) => lease,
        other => panic!("expected login start, got {other:?}"),
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
    let lease = begin_login(&store).await;

    store
        .apply_login_result(lease, dummy_tokens())
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

#[tokio::test]
async fn old_auth_set_token_never_enters_oauth_refresh() {
    let dir = tempdir().unwrap();
    let store = AuthStore::load_from_disk_with_refresh_url(
        &dir.path().join("auth.json"),
        "http://127.0.0.1:1/should-not-run".into(),
    )
    .await
    .expect("load");
    store
        .apply_auth_set_at("static-token".into(), Utc::now() - Duration::days(30))
        .await;
    assert_eq!(
        store.current_access_token().await.expect("token"),
        AccessToken("static-token".into())
    );
}

#[tokio::test]
async fn concurrent_refresh_rotates_once() {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("server");
    let addr = server.server_addr().to_ip().expect("ip");
    let calls = Arc::new(AtomicUsize::new(0));
    let server_calls = calls.clone();
    let thread = std::thread::spawn(move || {
        if let Ok(Some(request)) = server.recv_timeout(std::time::Duration::from_secs(2)) {
            server_calls.fetch_add(1, Ordering::SeqCst);
            let response = tiny_http::Response::from_string(
                r#"{"id_token":"h.e30.s","access_token":"fresh","refresh_token":"rotated"}"#,
            )
            .with_header(
                "content-type: application/json"
                    .parse::<tiny_http::Header>()
                    .expect("header"),
            );
            request.respond(response).expect("respond");
        }
        if let Ok(Some(request)) = server.recv_timeout(std::time::Duration::from_millis(250)) {
            server_calls.fetch_add(1, Ordering::SeqCst);
            let _ = request.respond(tiny_http::Response::empty(500));
        }
    });

    let dir = tempdir().unwrap();
    let path = dir.path().join("auth.json");
    let auth = AuthDotJson {
        tokens: dummy_tokens(),
        last_refresh: Utc::now() - Duration::days(9),
    };
    save(&path, &auth).expect("save");
    let store = Arc::new(
        AuthStore::load_from_disk_with_refresh_url(&path, format!("http://{addr}/oauth/token"))
            .await
            .expect("load"),
    );
    let (first, second) = tokio::join!(store.current_access_token(), store.current_access_token());
    assert_eq!(first.expect("first"), AccessToken("fresh".into()));
    assert_eq!(second.expect("second"), AccessToken("fresh".into()));
    thread.join().expect("server thread");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn permanent_refresh_failure_adopts_concurrent_same_account_disk_login() {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("server");
    let addr = server.server_addr().to_ip().expect("ip");
    let (request_started_tx, request_started_rx) = std::sync::mpsc::channel();
    let (continue_tx, continue_rx) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        let request = server
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("receive")
            .expect("request");
        request_started_tx.send(()).expect("started");
        continue_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("continue");
        request
            .respond(tiny_http::Response::from_string("invalid_grant").with_status_code(401))
            .expect("respond");
    });

    let dir = tempdir().unwrap();
    let path = dir.path().join("auth.json");
    let auth = AuthDotJson {
        tokens: dummy_tokens(),
        last_refresh: Utc::now() - Duration::days(9),
    };
    save(&path, &auth).expect("save");
    let store = Arc::new(
        AuthStore::load_from_disk_with_refresh_url(&path, format!("http://{addr}/oauth/token"))
            .await
            .expect("load"),
    );
    let refreshing = {
        let store = store.clone();
        tokio::spawn(async move { store.current_access_token().await })
    };
    tokio::task::spawn_blocking(move || {
        request_started_rx.recv_timeout(std::time::Duration::from_secs(2))
    })
    .await
    .expect("receiver task")
    .expect("refresh request");
    let mut external = auth;
    external.tokens.access_token = AccessToken("standalone-login".into());
    save(&path, &external).expect("save external login");
    continue_tx.send(()).expect("continue response");

    assert_eq!(
        refreshing.await.expect("task").expect("recovered token"),
        AccessToken("standalone-login".into())
    );
    thread.join().expect("server thread");
}

#[tokio::test]
async fn changed_same_account_disk_credentials_are_adopted() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("auth.json");
    let auth = AuthDotJson {
        tokens: dummy_tokens(),
        last_refresh: Utc::now(),
    };
    save(&path, &auth).expect("save");
    let store = AuthStore::load_from_disk(&path).await.expect("load");
    let mut changed = auth;
    changed.tokens.access_token = AccessToken("external-login".into());
    save(&path, &changed).expect("save changed");
    assert!(store.adopt_disk_credentials().await.expect("adopt"));
    assert_eq!(
        store
            .snapshot()
            .await
            .tokens
            .map(|tokens| tokens.access_token),
        Some(AccessToken("external-login".into()))
    );
}

#[tokio::test]
async fn changed_different_account_disk_credentials_are_rejected() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("auth.json");
    let auth = AuthDotJson {
        tokens: dummy_tokens(),
        last_refresh: Utc::now(),
    };
    save(&path, &auth).expect("save");
    let store = AuthStore::load_from_disk(&path).await.expect("load");
    let mut changed = auth;
    changed.tokens.access_token = AccessToken("other".into());
    changed.tokens.account_id = Some(ChatgptAccountId("other-account".into()));
    save(&path, &changed).expect("save changed");
    assert!(!store.adopt_disk_credentials().await.expect("reject"));
    assert_eq!(
        store
            .snapshot()
            .await
            .tokens
            .map(|tokens| tokens.access_token),
        Some(AccessToken("acc".into()))
    );
}

#[tokio::test]
async fn begin_login_is_atomic_and_rejects_duplicates() {
    let dir = tempdir().unwrap();
    let store = AuthStore::load_from_disk(&dir.path().join("auth.json"))
        .await
        .expect("load");
    let first = store.begin_login().await;
    assert!(matches!(first, LoginStartOutcome::Started(_)));
    assert_eq!(
        store.begin_login().await,
        LoginStartOutcome::AlreadyInProgress
    );
    assert_eq!(store.snapshot().await.state, AuthState::LoginInProgress);
}

#[tokio::test]
async fn logout_invalidates_late_login_success() {
    let dir = tempdir().unwrap();
    let store = AuthStore::load_from_disk(&dir.path().join("auth.json"))
        .await
        .expect("load");
    let lease = begin_login(&store).await;
    assert_eq!(
        store.apply_logout().await,
        chatgpt_provider::auth::LogoutOutcome::Cleared
    );
    assert!(!store
        .apply_login_result(lease, dummy_tokens())
        .await
        .expect("late result"));
    assert_eq!(store.snapshot().await.state, AuthState::LoginRequired);
}

#[tokio::test]
async fn auth_set_invalidates_late_login_success() {
    let dir = tempdir().unwrap();
    let store = AuthStore::load_from_disk(&dir.path().join("auth.json"))
        .await
        .expect("load");
    let lease = begin_login(&store).await;
    store.apply_auth_set("manual".into()).await;
    assert!(!store
        .apply_login_result(lease, dummy_tokens())
        .await
        .expect("late result"));
    assert_eq!(
        store
            .snapshot()
            .await
            .tokens
            .map(|tokens| tokens.access_token),
        Some(AccessToken("manual".into()))
    );
}

#[tokio::test]
async fn auth_set_never_adopts_oauth_disk_credentials() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("auth.json");
    save(
        &path,
        &AuthDotJson {
            tokens: dummy_tokens(),
            last_refresh: Utc::now(),
        },
    )
    .expect("save");
    let store = AuthStore::load_from_disk(&path).await.expect("load");
    store.apply_auth_set("manual".into()).await;
    assert!(!store.adopt_disk_credentials().await.expect("adopt"));
}

#[tokio::test]
async fn oauth_credentials_without_account_ids_are_not_adopted() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("auth.json");
    let mut auth = AuthDotJson {
        tokens: dummy_tokens(),
        last_refresh: Utc::now(),
    };
    auth.tokens.account_id = None;
    save(&path, &auth).expect("save");
    let store = AuthStore::load_from_disk(&path).await.expect("load");
    auth.tokens.access_token = AccessToken("changed".into());
    save(&path, &auth).expect("save changed");
    assert!(!store.adopt_disk_credentials().await.expect("adopt"));
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
