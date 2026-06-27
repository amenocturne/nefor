//! Per-process state for chatgpt-provider.
//!
//! Mirror of openai-provider's `Chats`: chat-id-keyed map where each
//! chat carries `(model, message_history, turn state, stats)`. The
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

use crate::responses::request::{ReasoningEffort, ResponseItem};

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

/// Single chat message in the conversation, keyed by role.
///
/// Internally tagged on `"role"` so the JSON wire shape is identical to
/// the old flat struct: `{"role":"user","content":"hi"}` round-trips
/// through serde unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    User {
        content: String,
    },
    Assistant {
        #[serde(skip_serializing_if = "Option::is_none", default)]
        content: Option<String>,
        #[serde(skip_serializing_if = "Vec::is_empty", default)]
        tool_calls: Vec<ToolCall>,
    },
    System {
        content: String,
    },
    Tool {
        content: String,
        tool_call_id: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        name: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HistoryEntry {
    Message { message: Message },
    Native { item: ResponseItem },
}

impl From<Message> for HistoryEntry {
    fn from(message: Message) -> Self {
        Self::Message { message }
    }
}

impl Message {
    pub fn role(&self) -> &str {
        match self {
            Message::User { .. } => "user",
            Message::Assistant { .. } => "assistant",
            Message::System { .. } => "system",
            Message::Tool { .. } => "tool",
        }
    }

    pub fn content(&self) -> Option<&str> {
        match self {
            Message::User { content, .. }
            | Message::System { content, .. }
            | Message::Tool { content, .. } => Some(content),
            Message::Assistant { content, .. } => content.as_deref(),
        }
    }

    pub fn tool_calls(&self) -> &[ToolCall] {
        match self {
            Message::Assistant { tool_calls, .. } => tool_calls,
            _ => &[],
        }
    }

    pub fn tool_call_id(&self) -> Option<&str> {
        match self {
            Message::Tool { tool_call_id, .. } => Some(tool_call_id),
            _ => None,
        }
    }

    pub fn user<S: Into<String>>(text: S) -> Self {
        Message::User {
            content: text.into(),
        }
    }

    pub fn assistant<S: Into<String>>(text: S) -> Self {
        Message::Assistant {
            content: Some(text.into()),
            tool_calls: Vec::new(),
        }
    }

    pub fn system<S: Into<String>>(text: S) -> Self {
        Message::System {
            content: text.into(),
        }
    }

    pub fn assistant_tool_calls(tool_calls: Vec<ToolCall>) -> Self {
        Message::Assistant {
            content: None,
            tool_calls,
        }
    }

    pub fn assistant_with_tool_calls<S: Into<String>>(text: S, tool_calls: Vec<ToolCall>) -> Self {
        Message::Assistant {
            content: Some(text.into()),
            tool_calls,
        }
    }

    pub fn tool_result<S: Into<String>>(tool_call_id: String, content: S) -> Self {
        Message::Tool {
            content: content.into(),
            tool_call_id,
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

enum TurnState {
    Idle,
    InFlight(CancellationToken),
}

struct ChatState {
    model: String,
    /// Optional system prompt (set on chat.create). Translator pulls
    /// this out into the Responses-API `instructions` field; never goes
    /// into the `input` array.
    system: Option<String>,
    history: Vec<HistoryEntry>,
    turn: TurnState,
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
    reasoning_effort: Option<ReasoningEffort>,
}

impl ChatState {
    fn new(model: String) -> Self {
        Self {
            model,
            system: None,
            history: Vec::new(),
            turn: TurnState::Idle,
            stats: ChatStats::default(),
            tool_allowlist: None,
            tool_overrides: None,
            reasoning_effort: None,
        }
    }

    fn from_create(
        model: String,
        system: Option<String>,
        tool_overrides: Option<Vec<crate::catalog::ToolSpec>>,
        tool_allowlist: Option<Vec<String>>,
        reasoning_effort: Option<ReasoningEffort>,
    ) -> Self {
        let mut chat = Self::new(model);
        chat.system = system;
        chat.tool_overrides = tool_overrides;
        chat.tool_allowlist = tool_allowlist;
        chat.reasoning_effort = reasoning_effort;
        chat
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
    #[error(
        "no model configured: pass `model` in chat.create or set the \
         provider default via model.set (the user picks via `/model` in \
         the chat surface)"
    )]
    NoModelConfigured,
}

/// Snapshot of a chat's state at the moment a turn starts. Carries
/// everything the dispatcher needs to build the Responses request
/// without holding the chats lock across the streaming HTTP call.
#[derive(Debug, Clone)]
pub struct ChatSnapshot {
    pub model: String,
    pub system: Option<String>,
    pub history: Vec<HistoryEntry>,
    pub tool_allowlist: Option<Vec<String>>,
    pub tool_overrides: Option<Vec<crate::catalog::ToolSpec>>,
    pub reasoning_effort: Option<ReasoningEffort>,
}

pub struct MessageRestore {
    pub id: ChatId,
    pub model: Option<String>,
    pub system: Option<String>,
    pub tool_overrides: Option<Vec<crate::catalog::ToolSpec>>,
    pub tool_allowlist: Option<Vec<String>>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub history: Vec<Message>,
}

/// Per-slug capability bits learned from the backend's /models
/// response. Populated by `Chats::record_model_capabilities` whenever
/// the dispatcher fetches /models. Authoritative source for "does
/// this model accept the reasoning.summary parameter on /responses".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelCapabilities {
    pub supports_reasoning_summaries: bool,
    pub supports_parallel_tool_calls: bool,
}

#[derive(Default)]
pub struct Chats {
    default_model: Mutex<Option<String>>,
    inner: Mutex<HashMap<ChatId, ChatState>>,
    /// Per-model capability cache populated from /models. Populated
    /// authoritatively by `record_model_capabilities`. When a model is
    /// absent from this map (we haven't fetched /models yet, or the
    /// model came in via `model.set` without going through /models),
    /// the dispatcher's static heuristic is the fallback.
    capabilities: Mutex<HashMap<String, ModelCapabilities>>,
    /// Per-model "reasoning unsupported" override populated reactively
    /// when a 400 from /responses reports `reasoning.summary` (or any
    /// `reasoning.*`) as an unsupported parameter — defense in depth
    /// for slugs the /models response says CAN reason but the live
    /// endpoint disagrees with. Idempotent insert. Mirrors
    /// openai-provider's `mark_model_tools_unsupported` pattern.
    reasoning_unsupported: Mutex<std::collections::HashSet<String>>,
}

impl Chats {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_default_model(model: Option<String>) -> Self {
        Self {
            default_model: Mutex::new(model),
            inner: Mutex::new(HashMap::new()),
            capabilities: Mutex::new(HashMap::new()),
            reasoning_unsupported: Mutex::new(std::collections::HashSet::new()),
        }
    }

    /// Replace the per-model capability cache with `entries`. Called
    /// after a successful /models fetch. Subsequent capability queries
    /// for slugs not in `entries` fall through to the static-heuristic
    /// path. Replaces rather than merges so a model that was demoted
    /// (`supports_reasoning_summaries` flipped from true to false)
    /// loses the stale "true" on next refresh.
    pub async fn record_model_capabilities<I>(&self, entries: I)
    where
        I: IntoIterator<Item = (String, ModelCapabilities)>,
    {
        let mut g = self.capabilities.lock().await;
        g.clear();
        for (slug, caps) in entries {
            g.insert(slug, caps);
        }
    }

    /// Authoritative reasoning-summary capability for `model` when we
    /// have a /models record for it. `None` means "we don't know,
    /// caller should fall back" (the static heuristic in
    /// `translator::model_supports_reasoning`).
    pub async fn model_capability_reasoning(&self, model: &str) -> Option<bool> {
        self.capabilities
            .lock()
            .await
            .get(model)
            .map(|c| c.supports_reasoning_summaries)
    }

    /// Authoritative parallel-tool-call capability for `model` when we
    /// have a /models record for it. `None` means "unknown"; callers
    /// should default to allowing parallel calls because the rest of the
    /// runtime already dispatches every returned tool call concurrently.
    pub async fn model_capability_parallel_tool_calls(&self, model: &str) -> Option<bool> {
        self.capabilities
            .lock()
            .await
            .get(model)
            .map(|c| c.supports_parallel_tool_calls)
    }

    /// Mark a model as rejecting the `reasoning` request block (e.g. a
    /// non-reasoning member of the gpt-5 family the static heuristic
    /// can't tell from the slug). Subsequent turns on this model omit
    /// the reasoning fields. Idempotent.
    pub async fn mark_model_reasoning_unsupported(&self, model: &str) {
        self.reasoning_unsupported
            .lock()
            .await
            .insert(model.to_owned());
    }

    pub async fn model_reasoning_unsupported(&self, model: &str) -> bool {
        self.reasoning_unsupported.lock().await.contains(model)
    }

    pub async fn default_model(&self) -> Option<String> {
        self.default_model.lock().await.clone()
    }

    pub async fn set_default_model(&self, model: String) {
        *self.default_model.lock().await = Some(model);
    }

    async fn resolve_model(&self, model: Option<String>) -> Result<String, ChatsError> {
        match model {
            Some(m) => Ok(m),
            None => self
                .default_model
                .lock()
                .await
                .clone()
                // No baked-in default. If no per-call `model` was given
                // AND no provider default has been set (via model.set
                // from the user's `/model` picker), surface a clear
                // error rather than guess. The previous code fell back
                // to a hardcoded `gpt-5-codex` which Codex rejects for
                // ChatGPT-subscription accounts.
                .ok_or(ChatsError::NoModelConfigured),
        }
    }

    /// Create the chat state value shared by strict create and
    /// recreate. `model` overrides the plugin default; `system` becomes
    /// the Responses-API `instructions` for every turn; the two tool
    /// fields are optional gates layered over the catalog.
    async fn build_chat(
        &self,
        model: Option<String>,
        system: Option<String>,
        tool_overrides: Option<Vec<crate::catalog::ToolSpec>>,
        tool_allowlist: Option<Vec<String>>,
        reasoning_effort: Option<ReasoningEffort>,
    ) -> Result<ChatState, ChatsError> {
        let resolved_model = self.resolve_model(model).await?;
        Ok(ChatState::from_create(
            resolved_model,
            system,
            tool_overrides,
            tool_allowlist,
            reasoning_effort,
        ))
    }

    /// Strict create used by tests and callers that need duplicate-id
    /// detection.
    pub async fn create(
        &self,
        id: ChatId,
        model: Option<String>,
        system: Option<String>,
        tool_overrides: Option<Vec<crate::catalog::ToolSpec>>,
        tool_allowlist: Option<Vec<String>>,
        reasoning_effort: Option<ReasoningEffort>,
    ) -> Result<(), ChatsError> {
        let chat = self
            .build_chat(
                model,
                system,
                tool_overrides,
                tool_allowlist,
                reasoning_effort,
            )
            .await?;
        let mut g = self.inner.lock().await;
        if g.contains_key(&id) {
            return Err(ChatsError::AlreadyExists(id));
        }
        g.insert(id, chat);
        Ok(())
    }

    /// Create a fresh chat state, replacing any stale in-process state
    /// with the same id. The agentic loop can replay or restart with a
    /// reused chat id; replacement prevents an old poisoned history from
    /// leaking into a nominally new conversation.
    pub async fn recreate(
        &self,
        id: ChatId,
        model: Option<String>,
        system: Option<String>,
        tool_overrides: Option<Vec<crate::catalog::ToolSpec>>,
        tool_allowlist: Option<Vec<String>>,
        reasoning_effort: Option<ReasoningEffort>,
    ) -> Result<(), ChatsError> {
        let chat = self
            .build_chat(
                model,
                system,
                tool_overrides,
                tool_allowlist,
                reasoning_effort,
            )
            .await?;
        let mut g = self.inner.lock().await;
        if g.insert(id.clone(), chat).is_some() {
            tracing::warn!(
                chat_id = %id,
                "replacing existing chat state during chat.create"
            );
        }
        Ok(())
    }

    pub async fn restore_messages(&self, restore: MessageRestore) -> Result<(), ChatsError> {
        let mut chat = self
            .build_chat(
                restore.model,
                restore.system,
                restore.tool_overrides,
                restore.tool_allowlist,
                restore.reasoning_effort,
            )
            .await?;
        chat.history = repair_tool_call_history(restore.history)
            .into_iter()
            .map(HistoryEntry::from)
            .collect();
        let mut g = self.inner.lock().await;
        g.insert(restore.id, chat);
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

    pub async fn set_chat_reasoning_effort(
        &self,
        id: &ChatId,
        effort: ReasoningEffort,
    ) -> Result<(), ChatsError> {
        let mut g = self.inner.lock().await;
        let chat = g
            .get_mut(id)
            .ok_or_else(|| ChatsError::NotFound(id.clone()))?;
        chat.reasoning_effort = Some(effort);
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
                reasoning_effort: c.reasoning_effort,
            })
            .ok_or_else(|| ChatsError::NotFound(id.clone()))
    }

    pub async fn append(&self, id: &ChatId, message: Message) -> Result<(), ChatsError> {
        let mut g = self.inner.lock().await;
        let chat = g
            .get_mut(id)
            .ok_or_else(|| ChatsError::NotFound(id.clone()))?;
        if let Message::Tool { tool_call_id, .. } = &message {
            if !has_unanswered_tool_call(&chat.history, tool_call_id) {
                tracing::warn!(
                    chat_id = %id,
                    tool_call_id = %tool_call_id,
                    "dropping orphan tool result without matching assistant tool call"
                );
                return Ok(());
            }
        }
        chat.history.push(message.into());
        Ok(())
    }

    pub async fn replace_with_native_history(
        &self,
        id: &ChatId,
        items: Vec<ResponseItem>,
    ) -> Result<(), ChatsError> {
        let mut g = self.inner.lock().await;
        let chat = g
            .get_mut(id)
            .ok_or_else(|| ChatsError::NotFound(id.clone()))?;
        chat.history = items
            .into_iter()
            .map(|item| HistoryEntry::Native { item })
            .collect();
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
        if matches!(chat.turn, TurnState::InFlight(_)) {
            return Err(ChatsError::Busy(id.clone()));
        }
        let token = CancellationToken::new();
        chat.turn = TurnState::InFlight(token.clone());
        Ok(token)
    }

    pub async fn end_turn(&self, id: &ChatId) {
        let mut g = self.inner.lock().await;
        if let Some(chat) = g.get_mut(id) {
            chat.turn = TurnState::Idle;
        }
    }

    /// Cancel the in-flight turn for `id` if any. Best-effort: returns
    /// `false` for unknown chats rather than erroring.
    pub async fn interrupt(&self, id: &ChatId) -> bool {
        let g = self.inner.lock().await;
        match g.get(id) {
            Some(ChatState {
                turn: TurnState::InFlight(ref token),
                ..
            }) => {
                token.cancel();
                true
            }
            _ => false,
        }
    }

    pub async fn interrupt_all(&self) {
        let g = self.inner.lock().await;
        for chat in g.values() {
            if let TurnState::InFlight(ref token) = chat.turn {
                token.cancel();
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

fn has_unanswered_tool_call(history: &[HistoryEntry], tool_call_id: &str) -> bool {
    let mut seen_call = false;
    for entry in history {
        let HistoryEntry::Message { message } = entry else {
            continue;
        };
        match message {
            Message::Assistant { tool_calls, .. }
                if tool_calls.iter().any(|tc| tc.id == tool_call_id) =>
            {
                seen_call = true;
            }
            Message::Tool {
                tool_call_id: answered,
                ..
            } if answered == tool_call_id => {
                seen_call = false;
            }
            _ => {}
        }
    }
    seen_call
}

fn repair_tool_call_history(history: Vec<Message>) -> Vec<Message> {
    let mut repaired = Vec::with_capacity(history.len());
    let mut pending: Vec<String> = Vec::new();

    for message in history {
        match &message {
            Message::Assistant { tool_calls, .. } => {
                for tc in tool_calls {
                    if !tc.id.is_empty() {
                        pending.push(tc.id.clone());
                    }
                }
                repaired.push(message);
            }
            Message::Tool { tool_call_id, .. } => {
                if let Some(i) = pending.iter().position(|id| id == tool_call_id) {
                    pending.remove(i);
                    repaired.push(message);
                } else {
                    tracing::warn!(
                        tool_call_id = %tool_call_id,
                        "dropping orphan tool result during chat.restore"
                    );
                }
            }
            Message::User { .. } | Message::System { .. } => {
                close_pending_tool_calls(&mut repaired, &mut pending);
                repaired.push(message);
            }
        }
    }

    close_pending_tool_calls(&mut repaired, &mut pending);
    repaired
}

fn close_pending_tool_calls(repaired: &mut Vec<Message>, pending: &mut Vec<String>) {
    for id in pending.drain(..) {
        repaired.push(Message::tool_result(
            id,
            "Tool call was interrupted before producing output.",
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_then_append_and_snapshot_round_trips() {
        let c = Chats::with_default_model(Some("m".into()));
        let id = ChatId::new("a");
        c.create(id.clone(), None, None, None, None, None)
            .await
            .expect("create");
        c.push_user(&id, "hello".into()).await.expect("push user");
        c.push_assistant(&id, "hi there".into())
            .await
            .expect("push assistant");
        let snap = c.snapshot(&id).await.expect("snapshot");
        assert_eq!(snap.history.len(), 2);
        match &snap.history[0] {
            HistoryEntry::Message { message } => assert_eq!(message.role(), "user"),
            _ => panic!("expected message"),
        }
        match &snap.history[1] {
            HistoryEntry::Message { message } => assert_eq!(message.role(), "assistant"),
            _ => panic!("expected message"),
        }
    }

    #[tokio::test]
    async fn create_rejects_duplicate_id() {
        let c = Chats::with_default_model(Some("m".into()));
        let id = ChatId::new("dup");
        c.create(id.clone(), None, None, None, None, None)
            .await
            .expect("first");
        let err = c
            .create(id.clone(), None, None, None, None, None)
            .await
            .expect_err("second");
        assert!(matches!(err, ChatsError::AlreadyExists(x) if x == id));
    }

    #[tokio::test]
    async fn recreate_replaces_existing_history() {
        let c = Chats::with_default_model(Some("m".into()));
        let id = ChatId::new("dup");
        c.create(id.clone(), None, Some("old".into()), None, None, None)
            .await
            .expect("first");
        c.push_user(&id, "stale".into()).await.expect("push");

        c.recreate(
            id.clone(),
            Some("new-model".into()),
            Some("new".into()),
            None,
            None,
            None,
        )
        .await
        .expect("recreate");

        let snap = c.snapshot(&id).await.expect("snapshot");
        assert!(snap.history.is_empty());
        assert_eq!(snap.model, "new-model");
        assert_eq!(snap.system.as_deref(), Some("new"));
    }

    #[tokio::test]
    async fn delete_removes_chat() {
        let c = Chats::with_default_model(Some("m".into()));
        let id = ChatId::new("x");
        c.create(id.clone(), None, None, None, None, None)
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
        c.create(a.clone(), None, None, None, None, None)
            .await
            .expect("a");
        c.create(b.clone(), None, None, None, None, None)
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
        c.create(a.clone(), None, None, None, None, None)
            .await
            .expect("a");
        c.create(b.clone(), None, None, None, None, None)
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
            None,
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
        c.create(id.clone(), None, None, None, None, None)
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
        c.create(a.clone(), None, None, None, None, None)
            .await
            .expect("a");
        c.create(b.clone(), None, None, None, None, None)
            .await
            .expect("b");
        c.push_user(&a, "x".into()).await.expect("a");
        c.push_user(&b, "y".into()).await.expect("b");
        c.reset_all().await;
        assert!(c.snapshot(&a).await.expect("a").history.is_empty());
        assert!(c.snapshot(&b).await.expect("b").history.is_empty());
    }

    #[tokio::test]
    async fn create_errors_when_no_model_and_no_default() {
        // Production startup ships with `with_default_model(None)`. A
        // `chat.create` without an explicit `model` field MUST error
        // rather than fall back to a hardcoded string — the user picks
        // via `/model` and the picker drives `set_default_model`. The
        // previous code silently filled in `gpt-5-codex`, which the
        // backend rejects for ChatGPT-subscription accounts.
        let c = Chats::with_default_model(None);
        let id = ChatId::new("a");
        let err = c
            .create(id, None, None, None, None, None)
            .await
            .expect_err("must reject");
        assert_eq!(err, ChatsError::NoModelConfigured);
    }

    #[tokio::test]
    async fn create_succeeds_with_explicit_model_when_no_default() {
        let c = Chats::with_default_model(None);
        let id = ChatId::new("a");
        c.create(
            id.clone(),
            Some("test-model".into()),
            None,
            None,
            None,
            None,
        )
        .await
        .expect("explicit model wins");
        let snap = c.snapshot(&id).await.expect("snap");
        assert_eq!(snap.model, "test-model");
    }

    #[tokio::test]
    async fn push_assistant_tool_calls_and_tool_result_round_trip() {
        let c = Chats::with_default_model(Some("m".into()));
        let id = ChatId::new("a");
        c.create(id.clone(), None, None, None, None, None)
            .await
            .expect("create");
        c.push_user(&id, "hi".into()).await.expect("u");
        let calls = vec![ToolCall {
            id: "call_1".into(),
            function: ToolCallFunction {
                name: "read_file".into(),
                arguments: "{\"path\":\"/x\"}".into(),
            },
        }];
        c.push_assistant_tool_calls(&id, String::new(), calls)
            .await
            .expect("a");
        c.push_tool_result(&id, "call_1".into(), "file contents".into())
            .await
            .expect("t");
        let h = c.snapshot(&id).await.expect("h").history;
        assert_eq!(h.len(), 3);
        match &h[1] {
            HistoryEntry::Message { message } => {
                assert_eq!(message.role(), "assistant");
                assert!(message.content().is_none());
                assert_eq!(message.tool_calls().len(), 1);
            }
            _ => panic!("expected assistant message"),
        }
        match &h[2] {
            HistoryEntry::Message { message } => {
                assert_eq!(message.role(), "tool");
                assert_eq!(message.tool_call_id(), Some("call_1"));
                assert_eq!(message.content(), Some("file contents"));
            }
            _ => panic!("expected tool message"),
        }
    }

    #[tokio::test]
    async fn orphan_tool_result_is_dropped_from_history() {
        let c = Chats::with_default_model(Some("m".into()));
        let id = ChatId::new("a");
        c.create(id.clone(), None, None, None, None, None)
            .await
            .expect("create");

        c.push_tool_result(&id, "call_missing".into(), "stale result".into())
            .await
            .expect("orphan is non-fatal");

        let h = c.snapshot(&id).await.expect("h").history;
        assert!(
            h.is_empty(),
            "orphan tool result must not poison future provider requests: {h:?}"
        );
    }

    #[tokio::test]
    async fn duplicate_tool_result_is_dropped_from_history() {
        let c = Chats::with_default_model(Some("m".into()));
        let id = ChatId::new("a");
        c.create(id.clone(), None, None, None, None, None)
            .await
            .expect("create");
        let calls = vec![ToolCall {
            id: "call_1".into(),
            function: ToolCallFunction {
                name: "read_file".into(),
                arguments: "{\"path\":\"/x\"}".into(),
            },
        }];

        c.push_assistant_tool_calls(&id, String::new(), calls)
            .await
            .expect("assistant");
        c.push_tool_result(&id, "call_1".into(), "first".into())
            .await
            .expect("first");
        c.push_tool_result(&id, "call_1".into(), "duplicate".into())
            .await
            .expect("duplicate is non-fatal");

        let h = c.snapshot(&id).await.expect("h").history;
        assert_eq!(h.len(), 2);
        match &h[1] {
            HistoryEntry::Message { message } => assert_eq!(message.content(), Some("first")),
            _ => panic!("expected tool message"),
        }
    }

    #[tokio::test]
    async fn restore_repairs_unanswered_tool_calls_before_next_user() {
        let c = Chats::with_default_model(Some("m".into()));
        let id = ChatId::new("a");
        let calls = vec![ToolCall {
            id: "call_1".into(),
            function: ToolCallFunction {
                name: "read_file".into(),
                arguments: "{\"path\":\"/x\"}".into(),
            },
        }];
        c.restore_messages(MessageRestore {
            id: id.clone(),
            model: None,
            system: None,
            tool_overrides: None,
            tool_allowlist: None,
            reasoning_effort: None,
            history: vec![
                Message::user("hi"),
                Message::assistant_with_tool_calls("", calls),
                Message::user("continue"),
            ],
        })
        .await
        .expect("restore");

        let h = c.snapshot(&id).await.expect("history").history;
        assert_eq!(h.len(), 4);
        match &h[2] {
            HistoryEntry::Message { message } => {
                assert_eq!(message.role(), "tool");
                assert_eq!(message.tool_call_id(), Some("call_1"));
                assert_eq!(
                    message.content(),
                    Some("Tool call was interrupted before producing output.")
                );
            }
            _ => panic!("expected repaired tool result"),
        }
        match &h[3] {
            HistoryEntry::Message { message } => assert_eq!(message.role(), "user"),
            _ => panic!("expected user message"),
        }
    }

    #[tokio::test]
    async fn record_model_capabilities_round_trips_known_flags() {
        let c = Chats::with_default_model(None);
        c.record_model_capabilities([
            (
                "gpt-5".to_string(),
                ModelCapabilities {
                    supports_reasoning_summaries: true,
                    supports_parallel_tool_calls: true,
                },
            ),
            (
                "gpt-5.3-codex-spark".to_string(),
                ModelCapabilities {
                    supports_reasoning_summaries: false,
                    supports_parallel_tool_calls: false,
                },
            ),
        ])
        .await;
        assert_eq!(c.model_capability_reasoning("gpt-5").await, Some(true));
        assert_eq!(
            c.model_capability_parallel_tool_calls("gpt-5").await,
            Some(true)
        );
        assert_eq!(
            c.model_capability_reasoning("gpt-5.3-codex-spark").await,
            Some(false)
        );
        assert_eq!(
            c.model_capability_parallel_tool_calls("gpt-5.3-codex-spark")
                .await,
            Some(false)
        );
        // Unknown model → None (caller falls back to static heuristic).
        assert_eq!(c.model_capability_reasoning("unknown-model").await, None);
        assert_eq!(
            c.model_capability_parallel_tool_calls("unknown-model")
                .await,
            None
        );
    }

    #[tokio::test]
    async fn replace_with_native_history_installs_compaction_items() {
        let c = Chats::with_default_model(Some("m".into()));
        let id = ChatId::new("a");
        c.create(id.clone(), None, None, None, None, None)
            .await
            .expect("create");
        c.push_user(&id, "old".into()).await.expect("push");

        c.replace_with_native_history(
            &id,
            vec![ResponseItem::Compaction {
                encrypted_content: "sealed".into(),
            }],
        )
        .await
        .expect("replace");

        let h = c.snapshot(&id).await.expect("h").history;
        assert_eq!(h.len(), 1);
        assert!(matches!(h[0], HistoryEntry::Native { .. }));
    }

    #[tokio::test]
    async fn record_model_capabilities_replaces_prior_snapshot() {
        let c = Chats::with_default_model(None);
        c.record_model_capabilities([(
            "gpt-5".to_string(),
            ModelCapabilities {
                supports_reasoning_summaries: true,
                supports_parallel_tool_calls: true,
            },
        )])
        .await;
        // Refresh: the model disappears from the new list. Stale "true"
        // must NOT linger.
        c.record_model_capabilities(std::iter::empty()).await;
        assert_eq!(c.model_capability_reasoning("gpt-5").await, None);
    }
}
