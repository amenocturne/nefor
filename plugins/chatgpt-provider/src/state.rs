//! Per-process state for chatgpt-provider.
//!
//! Mirror of openai-provider's `Chats`: chat-id-keyed map where each
//! chat carries `(model, message_history, in_flight slot, stats)`. The
//! provider can hold N concurrent chats; each turn is per-chat
//! exclusive. The full map is wrapped in a single mutex — operations
//! are short and the per-turn HTTP call holds no lock (dispatcher
//! snapshots history, releases, streams, re-acquires to append).
//!
//! No persistence in v1 — restarting the plugin drops every chat.

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------
// Message shape — internal history representation.
//
// Mirrors openai-provider's `Message` field-for-field so the same shape
// flows across the bus on `chat.append`. The translator (translator.rs)
// converts `Vec<Message>` into the Responses API's
// `(instructions, input: Vec<ResponseItem>)` at request time; chat
// history never sees the Responses-API typing.
// ---------------------------------------------------------------------

/// One assistant tool call as the model returned it. Used both in the
/// outgoing assistant message and the in-memory history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub function: ToolCallFunction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    /// JSON-encoded string. The Responses API does not pre-parse tool
    /// arguments; we mirror that on the wire too.
    pub arguments: String,
}

/// Single chat message in the conversation. Overloaded across four
/// roles (`user`, `assistant`, `system`, `tool`) — see openai-provider's
/// Message docstring for the full taxonomy.
///
/// `content` is `Option<String>` so the `null` case (assistant emitted
/// only tool calls) round-trips cleanly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tool_call_id: Option<String>,
    /// `name` field used by the `tool` role per the Responses API spec.
    /// We pass it through if a chat.append carries it but don't require
    /// it.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub name: Option<String>,
}

impl Message {
    pub fn user<S: Into<String>>(text: S) -> Self {
        Self {
            role: "user".into(),
            content: Some(text.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
        }
    }

    pub fn assistant<S: Into<String>>(text: S) -> Self {
        Self {
            role: "assistant".into(),
            content: Some(text.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
        }
    }

    pub fn system<S: Into<String>>(text: S) -> Self {
        Self {
            role: "system".into(),
            content: Some(text.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
        }
    }

    pub fn assistant_tool_calls(tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant".into(),
            content: None,
            tool_calls,
            tool_call_id: None,
            name: None,
        }
    }

    pub fn assistant_with_tool_calls<S: Into<String>>(text: S, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant".into(),
            content: Some(text.into()),
            tool_calls,
            tool_call_id: None,
            name: None,
        }
    }

    pub fn tool_result<S: Into<String>>(tool_call_id: String, content: S) -> Self {
        Self {
            role: "tool".into(),
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id),
            name: None,
        }
    }
}

// ---------------------------------------------------------------------
// ChatId newtype — keeps a model name from being passed where a chat
// id is expected.
// ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChatId(String);

impl ChatId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Stable id used by the legacy `<prefix>.prompt` compat path.
    /// Per-prefix so two chatgpt-provider spawns don't collide.
    pub fn default_for_prefix(prefix: &str) -> Self {
        let trimmed = prefix.trim_end_matches('.');
        Self(format!("{trimmed}:default"))
    }
}

impl fmt::Display for ChatId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------
// Per-chat telemetry.
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChatStats {
    pub model: Option<String>,
    pub turns_completed: u64,
    pub cumulative_input_tokens: u64,
    pub cumulative_output_tokens: u64,
    pub last_turn_input_tokens: u64,
    pub last_turn_output_tokens: u64,
    pub last_turn_duration_ms: Option<u64>,
}

// ---------------------------------------------------------------------
// Chat state.
// ---------------------------------------------------------------------

struct ChatState {
    model: String,
    /// Optional system prompt (set on chat.create). Translator pulls
    /// this out into the Responses-API `instructions` field; never goes
    /// into the `input` array.
    system: Option<String>,
    history: Vec<Message>,
    in_flight: bool,
    cancel: Option<CancellationToken>,
    stats: ChatStats,
    /// Optional per-chat allowlist of tool names; when `Some(names)`,
    /// the per-turn `tools` array is filtered to entries whose name is
    /// in `names`. Empty vec → zero tools. `None` → no filter.
    tool_allowlist: Option<Vec<String>>,
    /// Optional per-chat tool spec list. When `Some`, this overrides
    /// the catalog entirely (used by callers that want chat-specific
    /// tools rather than the global union). When `None`, the catalog
    /// is consulted.
    tool_overrides: Option<Vec<crate::catalog::ToolSpec>>,
}

impl ChatState {
    fn new(model: String) -> Self {
        Self {
            model,
            system: None,
            history: Vec::new(),
            in_flight: false,
            cancel: None,
            stats: ChatStats::default(),
            tool_allowlist: None,
            tool_overrides: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChatsError {
    #[error("chat `{0}` already exists")]
    AlreadyExists(ChatId),
    #[error("chat `{0}` not found")]
    NotFound(ChatId),
    #[error("chat `{0}` is busy")]
    Busy(ChatId),
}

/// Snapshot of a chat's state at the moment a turn starts. Carries
/// everything the dispatcher needs to build the Responses request
/// without holding the chats lock across the streaming HTTP call.
#[derive(Debug, Clone)]
pub struct ChatSnapshot {
    pub model: String,
    pub system: Option<String>,
    pub history: Vec<Message>,
    pub tool_allowlist: Option<Vec<String>>,
    pub tool_overrides: Option<Vec<crate::catalog::ToolSpec>>,
}

#[derive(Default)]
pub struct Chats {
    default_model: Mutex<Option<String>>,
    inner: Mutex<HashMap<ChatId, ChatState>>,
}

impl Chats {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_default_model(model: Option<String>) -> Self {
        Self {
            default_model: Mutex::new(model),
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub async fn default_model(&self) -> Option<String> {
        self.default_model.lock().await.clone()
    }

    pub async fn set_default_model(&self, model: String) {
        *self.default_model.lock().await = Some(model);
    }

    /// Create a chat. `model` overrides the plugin default; `system`
    /// becomes the Responses-API `instructions` for every turn; the
    /// two tool fields are optional gates layered over the catalog.
    pub async fn create(
        &self,
        id: ChatId,
        model: Option<String>,
        system: Option<String>,
        tool_overrides: Option<Vec<crate::catalog::ToolSpec>>,
        tool_allowlist: Option<Vec<String>>,
    ) -> Result<(), ChatsError> {
        let resolved_model = match model {
            Some(m) => m,
            None => self
                .default_model
                .lock()
                .await
                .clone()
                // ChatId-bearing errors only — falling back here is the
                // dispatcher's call. A missing default_model is treated
                // as "use the spec default": we plumb something so the
                // chat exists, and per-turn the request can fail
                // visibly if the model name is rejected upstream.
                .unwrap_or_else(|| crate::config::DEFAULT_MODEL.to_string()),
        };
        let mut g = self.inner.lock().await;
        if g.contains_key(&id) {
            return Err(ChatsError::AlreadyExists(id));
        }
        let mut chat = ChatState::new(resolved_model);
        chat.system = system;
        chat.tool_overrides = tool_overrides;
        chat.tool_allowlist = tool_allowlist;
        g.insert(id, chat);
        Ok(())
    }

    /// Idempotent variant used by the legacy default-chat compat path
    /// (`<prefix>.prompt`).
    pub async fn ensure(&self, id: ChatId) -> Result<(), ChatsError> {
        let mut g = self.inner.lock().await;
        if g.contains_key(&id) {
            return Ok(());
        }
        let model = self
            .default_model
            .lock()
            .await
            .clone()
            .unwrap_or_else(|| crate::config::DEFAULT_MODEL.to_string());
        g.insert(id, ChatState::new(model));
        Ok(())
    }

    pub async fn delete(&self, id: &ChatId) -> Result<(), ChatsError> {
        let mut g = self.inner.lock().await;
        if g.remove(id).is_none() {
            return Err(ChatsError::NotFound(id.clone()));
        }
        Ok(())
    }

    pub async fn exists(&self, id: &ChatId) -> bool {
        self.inner.lock().await.contains_key(id)
    }

    pub async fn model(&self, id: &ChatId) -> Result<String, ChatsError> {
        let g = self.inner.lock().await;
        g.get(id)
            .map(|c| c.model.clone())
            .ok_or_else(|| ChatsError::NotFound(id.clone()))
    }

    pub async fn set_chat_model(&self, id: &ChatId, model: String) -> Result<(), ChatsError> {
        let mut g = self.inner.lock().await;
        let chat = g
            .get_mut(id)
            .ok_or_else(|| ChatsError::NotFound(id.clone()))?;
        chat.model = model;
        Ok(())
    }

    /// Snapshot everything the dispatcher needs to build a turn
    /// request, in one lock acquisition. Returns `NotFound` if the
    /// chat is gone.
    pub async fn snapshot(&self, id: &ChatId) -> Result<ChatSnapshot, ChatsError> {
        let g = self.inner.lock().await;
        g.get(id)
            .map(|c| ChatSnapshot {
                model: c.model.clone(),
                system: c.system.clone(),
                history: c.history.clone(),
                tool_allowlist: c.tool_allowlist.clone(),
                tool_overrides: c.tool_overrides.clone(),
            })
            .ok_or_else(|| ChatsError::NotFound(id.clone()))
    }

    pub async fn append(&self, id: &ChatId, message: Message) -> Result<(), ChatsError> {
        let mut g = self.inner.lock().await;
        let chat = g
            .get_mut(id)
            .ok_or_else(|| ChatsError::NotFound(id.clone()))?;
        chat.history.push(message);
        Ok(())
    }

    pub async fn push_user(&self, id: &ChatId, text: String) -> Result<(), ChatsError> {
        self.append(id, Message::user(text)).await
    }

    pub async fn push_assistant(&self, id: &ChatId, text: String) -> Result<(), ChatsError> {
        self.append(id, Message::assistant(text)).await
    }

    pub async fn push_assistant_tool_calls(
        &self,
        id: &ChatId,
        text: String,
        tool_calls: Vec<ToolCall>,
    ) -> Result<(), ChatsError> {
        let msg = if text.is_empty() {
            Message::assistant_tool_calls(tool_calls)
        } else {
            Message::assistant_with_tool_calls(text, tool_calls)
        };
        self.append(id, msg).await
    }

    pub async fn push_tool_result(
        &self,
        id: &ChatId,
        tool_call_id: String,
        content: String,
    ) -> Result<(), ChatsError> {
        self.append(id, Message::tool_result(tool_call_id, content))
            .await
    }

    pub async fn reset(&self, id: &ChatId) -> Result<(), ChatsError> {
        let mut g = self.inner.lock().await;
        let chat = g
            .get_mut(id)
            .ok_or_else(|| ChatsError::NotFound(id.clone()))?;
        chat.history.clear();
        Ok(())
    }

    pub async fn reset_all(&self) {
        let mut g = self.inner.lock().await;
        for chat in g.values_mut() {
            chat.history.clear();
        }
    }

    pub async fn begin_turn(&self, id: &ChatId) -> Result<CancellationToken, ChatsError> {
        let mut g = self.inner.lock().await;
        let chat = g
            .get_mut(id)
            .ok_or_else(|| ChatsError::NotFound(id.clone()))?;
        if chat.in_flight {
            return Err(ChatsError::Busy(id.clone()));
        }
        let token = CancellationToken::new();
        chat.cancel = Some(token.clone());
        chat.in_flight = true;
        Ok(token)
    }

    pub async fn end_turn(&self, id: &ChatId) {
        let mut g = self.inner.lock().await;
        if let Some(chat) = g.get_mut(id) {
            chat.in_flight = false;
            chat.cancel = None;
        }
    }

    /// Cancel the in-flight turn for `id` if any. Best-effort: returns
    /// `false` for unknown chats rather than erroring.
    pub async fn interrupt(&self, id: &ChatId) -> bool {
        let g = self.inner.lock().await;
        match g.get(id).and_then(|c| c.cancel.as_ref()) {
            Some(t) => {
                t.cancel();
                true
            }
            None => false,
        }
    }

    pub async fn interrupt_all(&self) {
        let g = self.inner.lock().await;
        for chat in g.values() {
            if let Some(t) = &chat.cancel {
                t.cancel();
            }
        }
    }

    pub async fn record_turn(
        &self,
        id: &ChatId,
        model: Option<&str>,
        prompt_tokens: u64,
        completion_tokens: u64,
        duration_ms: u64,
    ) -> Result<(), ChatsError> {
        let mut g = self.inner.lock().await;
        let chat = g
            .get_mut(id)
            .ok_or_else(|| ChatsError::NotFound(id.clone()))?;
        if let Some(m) = model {
            chat.stats.model = Some(m.to_owned());
        }
        chat.stats.turns_completed = chat.stats.turns_completed.saturating_add(1);
        chat.stats.cumulative_input_tokens = chat
            .stats
            .cumulative_input_tokens
            .saturating_add(prompt_tokens);
        chat.stats.cumulative_output_tokens = chat
            .stats
            .cumulative_output_tokens
            .saturating_add(completion_tokens);
        chat.stats.last_turn_input_tokens = prompt_tokens;
        chat.stats.last_turn_output_tokens = completion_tokens;
        chat.stats.last_turn_duration_ms = Some(duration_ms);
        Ok(())
    }

    pub async fn stats_snapshot(&self, id: &ChatId) -> Result<ChatStats, ChatsError> {
        let g = self.inner.lock().await;
        g.get(id)
            .map(|c| c.stats.clone())
            .ok_or_else(|| ChatsError::NotFound(id.clone()))
    }

    pub async fn ids(&self) -> Vec<ChatId> {
        self.inner.lock().await.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_then_append_and_snapshot_round_trips() {
        let c = Chats::with_default_model(Some("m".into()));
        let id = ChatId::new("a");
        c.create(id.clone(), None, None, None, None)
            .await
            .expect("create");
        c.push_user(&id, "hello".into()).await.expect("push user");
        c.push_assistant(&id, "hi there".into())
            .await
            .expect("push assistant");
        let snap = c.snapshot(&id).await.expect("snapshot");
        assert_eq!(snap.history.len(), 2);
        assert_eq!(snap.history[0].role, "user");
        assert_eq!(snap.history[1].role, "assistant");
    }

    #[tokio::test]
    async fn create_rejects_duplicate_id() {
        let c = Chats::with_default_model(Some("m".into()));
        let id = ChatId::new("dup");
        c.create(id.clone(), None, None, None, None)
            .await
            .expect("first");
        let err = c
            .create(id.clone(), None, None, None, None)
            .await
            .expect_err("second");
        assert!(matches!(err, ChatsError::AlreadyExists(x) if x == id));
    }

    #[tokio::test]
    async fn delete_removes_chat() {
        let c = Chats::with_default_model(Some("m".into()));
        let id = ChatId::new("x");
        c.create(id.clone(), None, None, None, None)
            .await
            .expect("create");
        c.delete(&id).await.expect("delete");
        let err = c.push_user(&id, "y".into()).await.expect_err("post-delete");
        assert!(matches!(err, ChatsError::NotFound(x) if x == id));
        assert!(!c.exists(&id).await);
    }

    #[tokio::test]
    async fn begin_turn_is_per_chat_exclusive() {
        let c = Chats::with_default_model(Some("m".into()));
        let a = ChatId::new("a");
        let b = ChatId::new("b");
        c.create(a.clone(), None, None, None, None)
            .await
            .expect("a");
        c.create(b.clone(), None, None, None, None)
            .await
            .expect("b");
        let _t1 = c.begin_turn(&a).await.expect("a first");
        let busy = c.begin_turn(&a).await.expect_err("busy");
        assert!(matches!(busy, ChatsError::Busy(x) if x == a));
        let _t2 = c.begin_turn(&b).await.expect("b parallel");
    }

    #[tokio::test]
    async fn interrupt_cancels_only_named_chat() {
        let c = Chats::with_default_model(Some("m".into()));
        let a = ChatId::new("a");
        let b = ChatId::new("b");
        c.create(a.clone(), None, None, None, None)
            .await
            .expect("a");
        c.create(b.clone(), None, None, None, None)
            .await
            .expect("b");
        let ta = c.begin_turn(&a).await.expect("a");
        let tb = c.begin_turn(&b).await.expect("b");
        assert!(c.interrupt(&a).await);
        assert!(ta.is_cancelled());
        assert!(!tb.is_cancelled());
    }

    #[tokio::test]
    async fn snapshot_carries_system_and_tool_fields() {
        let c = Chats::with_default_model(Some("m".into()));
        let id = ChatId::new("a");
        c.create(
            id.clone(),
            Some("custom".into()),
            Some("be brief".into()),
            None,
            Some(vec!["read_file".into()]),
        )
        .await
        .expect("create");
        let snap = c.snapshot(&id).await.expect("snapshot");
        assert_eq!(snap.model, "custom");
        assert_eq!(snap.system.as_deref(), Some("be brief"));
        assert_eq!(
            snap.tool_allowlist.as_deref(),
            Some(&["read_file".to_string()][..])
        );
    }

    #[tokio::test]
    async fn record_turn_accumulates() {
        let c = Chats::with_default_model(Some("m".into()));
        let id = ChatId::new("a");
        c.create(id.clone(), None, None, None, None)
            .await
            .expect("create");
        c.record_turn(&id, Some("gpt-5"), 100, 50, 1234)
            .await
            .expect("first");
        c.record_turn(&id, Some("gpt-5"), 120, 60, 2222)
            .await
            .expect("second");
        let snap = c.stats_snapshot(&id).await.expect("snap");
        assert_eq!(snap.turns_completed, 2);
        assert_eq!(snap.cumulative_input_tokens, 220);
        assert_eq!(snap.cumulative_output_tokens, 110);
        assert_eq!(snap.last_turn_duration_ms, Some(2222));
    }

    #[tokio::test]
    async fn reset_all_clears_every_history() {
        let c = Chats::with_default_model(Some("m".into()));
        let a = ChatId::new("a");
        let b = ChatId::new("b");
        c.create(a.clone(), None, None, None, None)
            .await
            .expect("a");
        c.create(b.clone(), None, None, None, None)
            .await
            .expect("b");
        c.push_user(&a, "x".into()).await.expect("a");
        c.push_user(&b, "y".into()).await.expect("b");
        c.reset_all().await;
        assert!(c.snapshot(&a).await.expect("a").history.is_empty());
        assert!(c.snapshot(&b).await.expect("b").history.is_empty());
    }

    #[test]
    fn chat_id_default_for_prefix_strips_trailing_dot() {
        let id = ChatId::default_for_prefix("chatgpt.");
        assert_eq!(id.as_str(), "chatgpt:default");
    }
}
