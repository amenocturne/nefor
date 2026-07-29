//! Header-builder coverage. No HTTP traffic — we inspect the
//! `HeaderMap` directly.

use chatgpt_provider::auth::store::{AccessToken, ChatgptAccountId, RefreshToken, TokenData};
use chatgpt_provider::auth::{AuthSnapshot, AuthState};
use chatgpt_provider::responses::headers::{
    add_turn_headers, build_headers, CHATGPT_ACCOUNT_ID, ORIGINATOR, SESSION_ID,
    X_CODEX_INSTALLATION_ID, X_CODEX_TURN_STATE,
};
use reqwest::header::{ACCEPT, ACCEPT_ENCODING, AUTHORIZATION, USER_AGENT};

fn snapshot_with_account() -> AuthSnapshot {
    AuthSnapshot {
        tokens: Some(TokenData {
            id_token: "h.p.s".into(),
            access_token: AccessToken("acc-token".into()),
            refresh_token: RefreshToken("ref-token".into()),
            account_id: Some(ChatgptAccountId("acct-42".into())),
        }),
        state: AuthState::Connected,
        source: None,
    }
}

fn snapshot_without_account() -> AuthSnapshot {
    AuthSnapshot {
        tokens: Some(TokenData {
            id_token: "h.p.s".into(),
            access_token: AccessToken("acc-token".into()),
            refresh_token: RefreshToken("ref-token".into()),
            account_id: None,
        }),
        state: AuthState::Connected,
        source: None,
    }
}

#[test]
fn includes_authorization_bearer_when_tokens_present() {
    let snap = snapshot_with_account();
    let headers = build_headers(&snap, "install-1", "nefor_cli_rs").expect("headers");
    let auth = headers.get(AUTHORIZATION).expect("Authorization header");
    assert_eq!(auth.to_str().unwrap(), "Bearer acc-token");
}

#[test]
fn includes_account_id_header_when_present() {
    let snap = snapshot_with_account();
    let headers = build_headers(&snap, "install-1", "nefor_cli_rs").expect("headers");
    let v = headers
        .get(CHATGPT_ACCOUNT_ID)
        .expect("ChatGPT-Account-Id header");
    assert_eq!(v.to_str().unwrap(), "acct-42");
}

#[test]
fn omits_account_id_header_when_absent() {
    let snap = snapshot_without_account();
    let headers = build_headers(&snap, "install-1", "nefor_cli_rs").expect("headers");
    assert!(
        headers.get(CHATGPT_ACCOUNT_ID).is_none(),
        "ChatGPT-Account-Id must be absent when token has no account_id",
    );
}

#[test]
fn sets_originator_to_provided_value() {
    let snap = snapshot_with_account();
    let headers = build_headers(&snap, "install-1", "my-originator").expect("headers");
    let v = headers.get(ORIGINATOR).expect("originator header");
    assert_eq!(v.to_str().unwrap(), "my-originator");
}

#[test]
fn sets_installation_id_header() {
    let snap = snapshot_with_account();
    let headers = build_headers(&snap, "uuid-abc-123", "nefor_cli_rs").expect("headers");
    let v = headers
        .get(X_CODEX_INSTALLATION_ID)
        .expect("x-codex-installation-id header");
    assert_eq!(v.to_str().unwrap(), "uuid-abc-123");
}

#[test]
fn sets_accept_text_event_stream() {
    let snap = snapshot_with_account();
    let headers = build_headers(&snap, "install-1", "nefor_cli_rs").expect("headers");
    let v = headers.get(ACCEPT).expect("Accept header");
    assert_eq!(v.to_str().unwrap(), "text/event-stream");
}

#[test]
fn disables_response_compression_for_sse() {
    let snap = snapshot_with_account();
    let headers = build_headers(&snap, "install-1", "nefor_cli_rs").expect("headers");
    let v = headers
        .get(ACCEPT_ENCODING)
        .expect("Accept-Encoding header");
    assert_eq!(v.to_str().unwrap(), "identity");
}

#[test]
fn sets_versioned_user_agent() {
    let snap = snapshot_with_account();
    let headers = build_headers(&snap, "install-1", "nefor_cli_rs").expect("headers");
    let v = headers.get(USER_AGENT).expect("User-Agent header");
    let ua = v.to_str().unwrap();
    assert!(ua.starts_with("nefor-chatgpt-provider/"));
    assert!(ua.len() > "nefor-chatgpt-provider/".len());
}

#[test]
fn adds_stable_session_routing_headers() {
    let snap = snapshot_with_account();
    let mut headers = build_headers(&snap, "install-1", "nefor_cli_rs").expect("headers");
    add_turn_headers(&mut headers, "chat-42", Some("sticky-7")).expect("turn headers");

    assert_eq!(headers.get(SESSION_ID).unwrap(), "chat-42");
    assert_eq!(headers.get(X_CODEX_TURN_STATE).unwrap(), "sticky-7");
}

#[test]
fn omits_turn_state_before_server_assigns_it() {
    let snap = snapshot_with_account();
    let mut headers = build_headers(&snap, "install-1", "nefor_cli_rs").expect("headers");
    add_turn_headers(&mut headers, "chat-42", None).expect("turn headers");

    assert_eq!(headers.get(SESSION_ID).unwrap(), "chat-42");
    assert!(headers.get(X_CODEX_TURN_STATE).is_none());
}

#[test]
fn fails_with_no_tokens_when_snapshot_is_login_required() {
    let snap = AuthSnapshot {
        tokens: None,
        state: AuthState::LoginRequired,
        source: None,
    };
    let err =
        build_headers(&snap, "install-1", "nefor_cli_rs").expect_err("expected NoTokens error");
    assert!(
        matches!(err, chatgpt_provider::error::ChatgptError::NoTokens),
        "expected NoTokens, got {err:?}"
    );
}
