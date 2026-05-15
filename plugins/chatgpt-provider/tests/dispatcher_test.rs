//! Dispatcher-adjacent integration tests.
//!
//! Skips the full happy-path HTTP-streaming flow (requires wiremock,
//! out-of-scope for Phase 4 per spec). Instead exercises the
//! state/broker/catalog interactions the dispatcher composes, plus the
//! auth state-machine transitions wired through the dispatch path.

use std::sync::Arc;
use tokio::sync::mpsc;

use chatgpt_provider::auth::store::{AccessToken, ChatgptAccountId, RefreshToken, TokenData};
use chatgpt_provider::auth::{AuthSnapshot, AuthState, AuthStore, LogoutOutcome, TokenSource};
use chatgpt_provider::broker::{ToolBroker, ToolResult};
use chatgpt_provider::catalog::{ToolCatalog, ToolSpec};
use chatgpt_provider::state::{ChatId, Chats, Message};

fn dummy_tokens() -> TokenData {
    TokenData {
        id_token: "h.p.s".into(),
        access_token: AccessToken("acc".into()),
        refresh_token: RefreshToken("ref".into()),
        account_id: Some(ChatgptAccountId("acct".into())),
    }
}

#[tokio::test]
async fn chats_and_catalog_and_broker_compose_for_a_tool_turn_shape() {
    // No baked-in default model — production startup ships with None.
    // The test passes an explicit per-chat model into `create()` instead.
    let chats = Arc::new(Chats::with_default_model(None));
    let catalog = Arc::new(ToolCatalog::new());
    let broker = Arc::new(ToolBroker::new());

    // 1. tool plugin registers a tool.
    catalog
        .register_from(
            "basic-tools",
            vec![ToolSpec {
                name: "read_file".into(),
                description: "Read a file".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }),
            }],
        )
        .await;
    assert_eq!(
        catalog.owner_of("read_file").await.as_deref(),
        Some("basic-tools")
    );

    // 2. chat is created and a user message is appended.
    let chat_id = ChatId::new("c1");
    chats
        .create(chat_id.clone(), Some("test-model".into()), None, None, None)
        .await
        .expect("create");
    chats
        .push_user(&chat_id, "open /etc/hostname".into())
        .await
        .expect("push user");

    // 3. begin_turn reserves the slot.
    let _cancel = chats.begin_turn(&chat_id).await.expect("begin turn");
    assert!(
        chats.begin_turn(&chat_id).await.is_err(),
        "second begin_turn must error with Busy"
    );

    // 4. broker registration + delivery oneshot pattern.
    let rx = broker.register("call_1".into()).await;
    let delivered = broker
        .deliver(ToolResult {
            id: "call_1".into(),
            output: Some("darwin".into()),
            error: None,
        })
        .await;
    assert!(delivered);
    let r = rx.await.expect("oneshot");
    assert_eq!(r.output.as_deref(), Some("darwin"));

    // 5. end_turn releases the slot.
    chats.end_turn(&chat_id).await;
    let _again = chats.begin_turn(&chat_id).await.expect("post-end_turn");
}

#[tokio::test]
async fn auth_store_apply_auth_set_then_logout_clears() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("auth.json");
    let store = AuthStore::load_from_disk(&path).await.expect("load");
    let initial = store.snapshot().await;
    assert_eq!(initial.state, AuthState::LoginRequired);
    assert!(initial.source.is_none());

    let snap = store.apply_auth_set("raw-token".into()).await;
    assert_eq!(snap.state, AuthState::Connected);
    assert_eq!(snap.source, Some(TokenSource::AuthSet));
    assert!(snap
        .tokens
        .as_ref()
        .map(|t| t.access_token == AccessToken("raw-token".into()))
        .unwrap_or(false));

    let outcome = store.apply_logout().await;
    assert_eq!(outcome, LogoutOutcome::Cleared);
    let after = store.snapshot().await;
    assert_eq!(after.state, AuthState::LoginRequired);
    assert!(after.tokens.is_none());
    assert!(after.source.is_none());
}

#[tokio::test]
async fn auth_store_apply_login_result_persists_and_transitions() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("auth.json");
    let store = AuthStore::load_from_disk(&path).await.expect("load");
    store
        .apply_login_result(dummy_tokens())
        .await
        .expect("apply");
    let snap = store.snapshot().await;
    assert_eq!(snap.state, AuthState::Connected);
    assert_eq!(snap.source, Some(TokenSource::Oauth));
}

#[tokio::test]
async fn auth_store_apply_error_marks_error_state() {
    let store = AuthStore::load_from_disk(&tempfile::tempdir().unwrap().path().join("a"))
        .await
        .expect("load");
    let snap = store.apply_error("HTTP 401".into()).await;
    match &snap.state {
        AuthState::Error(msg) => assert_eq!(msg, "HTTP 401"),
        _ => panic!("expected Error"),
    }
}

#[tokio::test]
async fn interrupt_only_cancels_named_chat() {
    let chats = Arc::new(Chats::with_default_model(None));
    let a = ChatId::new("a");
    let b = ChatId::new("b");
    chats
        .create(a.clone(), Some("test-model".into()), None, None, None)
        .await
        .expect("a");
    chats
        .create(b.clone(), Some("test-model".into()), None, None, None)
        .await
        .expect("b");
    let ta = chats.begin_turn(&a).await.expect("a");
    let tb = chats.begin_turn(&b).await.expect("b");
    assert!(chats.interrupt(&a).await);
    assert!(ta.is_cancelled());
    assert!(!tb.is_cancelled());
}

#[tokio::test]
async fn chat_snapshot_carries_history_for_translator_use() {
    let chats = Arc::new(Chats::with_default_model(None));
    let id = ChatId::new("a");
    chats
        .create(
            id.clone(),
            Some("test-model".into()),
            Some("system prompt".into()),
            None,
            None,
        )
        .await
        .expect("create");
    chats.push_user(&id, "hello".into()).await.expect("user");
    chats
        .push_assistant(&id, "hi back".into())
        .await
        .expect("assistant");
    let snap = chats.snapshot(&id).await.expect("snap");
    assert_eq!(snap.history.len(), 2);
    assert_eq!(snap.system.as_deref(), Some("system prompt"));

    // The translator drains this snapshot end-to-end.
    let t = chatgpt_provider::translator::history_to_input(&snap.history, snap.system.as_deref());
    assert_eq!(t.instructions, "system prompt");
    assert_eq!(t.input.len(), 2);
}

#[tokio::test]
async fn writer_channel_send_does_not_panic_on_close() {
    // Smoke test: the dispatcher's mpsc-based output channel must
    // accept body Maps without surprises. We don't drive the stdio
    // writer task here (would require real stdout); just confirm the
    // send pattern works against a manually-constructed channel.
    use nefor_protocol::PluginOutgoing;
    use serde_json::{Map, Value};
    let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
    let mut body = Map::new();
    body.insert("kind".into(), Value::String("chatgpt.hello".into()));
    tx.send(PluginOutgoing::event(body)).await.expect("send");
    let msg = rx.recv().await.expect("recv");
    let line = msg.to_line();
    assert!(line.contains(r#""kind":"chatgpt.hello""#));
}

#[test]
fn auth_snapshot_constructs_without_default() {
    // Compile-time check that AuthSnapshot has the public fields the
    // dispatcher relies on.
    let snap = AuthSnapshot {
        tokens: None,
        state: AuthState::LoginRequired,
        source: None,
    };
    assert_eq!(snap.state, AuthState::LoginRequired);
}

#[tokio::test]
async fn catalog_filters_by_allowlist_at_translator_step() {
    use chatgpt_provider::translator::tools_to_responses_format;

    let cat = ToolCatalog::new();
    cat.register_from(
        "basic-tools",
        vec![
            ToolSpec {
                name: "read_file".into(),
                description: String::new(),
                input_schema: serde_json::json!({}),
            },
            ToolSpec {
                name: "delete_file".into(),
                description: String::new(),
                input_schema: serde_json::json!({}),
            },
        ],
    )
    .await;
    let all = cat.all().await;
    assert_eq!(all.len(), 2);

    // Apply an allowlist post-fetch (this is what the dispatcher does
    // before calling the translator).
    let allowed = ["read_file".to_string()];
    let filtered: Vec<_> = all
        .into_iter()
        .filter(|t| allowed.iter().any(|a| a == &t.name))
        .collect();
    let wire = tools_to_responses_format(&filtered);
    assert_eq!(wire.len(), 1);
    assert_eq!(
        wire[0].get("name").and_then(|v| v.as_str()),
        Some("read_file")
    );
}

#[test]
fn message_round_trips_through_json_appended_shape() {
    // chat.append carries `message: {role, content, tool_calls?,
    // tool_call_id?, name?}`. Serde on our Message struct must accept
    // both the user/assistant text shape and the tool-result shape.
    let user_json = serde_json::json!({"role":"user","content":"hi"});
    let m: Message = serde_json::from_value(user_json).expect("user");
    assert_eq!(m.role, "user");
    assert_eq!(m.content.as_deref(), Some("hi"));

    let tool_json = serde_json::json!({
        "role":"tool",
        "content":"ok",
        "tool_call_id":"call_1",
        "name":"read_file"
    });
    let m: Message = serde_json::from_value(tool_json).expect("tool");
    assert_eq!(m.role, "tool");
    assert_eq!(m.tool_call_id.as_deref(), Some("call_1"));
    assert_eq!(m.name.as_deref(), Some("read_file"));
}
