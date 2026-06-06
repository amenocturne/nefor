//! Per-process state for openai-provider.
//!
//! Stage 1 reshape: replaces the singleton `SessionState` (one history,
//! one in-flight slot) with a chat-id-keyed map. The provider can hold N
//! concurrent chats; each chat is `(model, message_history, in_flight,
//! cancel, stats)`. This is the wire-level view from the parent
//! agent-and-reasoner-types spec §2 — a provider is a "dumb runner that
//! manages chats, where each chat is a `(model_id, message_history)`
//! pair."
//!
//! No persistence in v1 — restarting the plugin drops every chat.
//!
//! ## Default chat (legacy compat)
//!
//! The legacy `<prefix>.prompt` path uses a synthetic default chat id
//! (`<prefix>:default`) so existing chat→provider wiring keeps working
//! during the coexistence window mandated by D-15. The new
//! `<prefix>.chat.create / chat.append / chat.complete` API is what
//! reasoner-graph (T5) drives directly.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::openai::{Message, ToolCall};

/// Newtype wrapper for chat ids so we can't accidentally pass a model
/// name where a chat id is expected.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChatId(String);

impl ChatId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Stable id used by the legacy `<prefix>.prompt` path for the
    /// implicit default chat. Per-prefix so two openai-provider spawns
    /// (`ollama:default` vs `groq:default`) don't collide.
    pub fn default_for_prefix(prefix: &str) -> Self {
        // `prefix` carries the trailing dot already (e.g. `ollama.`); we
        // strip it so the chat id reads `ollama:default` rather than
        // `ollama.:default`.
        let trimmed = prefix.trim_end_matches('.');
        Self(format!("{trimmed}:default"))
    }
}

impl fmt::Display for ChatId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Per-chat telemetry. Tracks the same shape `SessionStats` did before;
/// renamed because it's no longer "the session" — there can be many.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChatStats {
    pub model: Option<String>,
    pub turns_completed: u64,
    pub cumulative_input_tokens: u64,
    pub cumulative_output_tokens: u64,
    pub last_turn_input_tokens: u64,
    pub last_turn_output_tokens: u64,
    /// Context-window size of the most recently completed turn — for
    /// raw chat-completions this is identical to `last_turn_input_tokens`
    /// (no prompt caching).
    pub last_turn_context_tokens: u64,
    pub last_turn_duration_ms: Option<u64>,
}

pub struct ChatRestore {
    pub id: ChatId,
    pub model: Option<String>,
    pub tools_enabled: Option<bool>,
    pub tool_allowlist: Option<Vec<String>>,
    pub reasoning_effort: Option<String>,
    pub system: Option<String>,
    pub history: Vec<Message>,
}

/// Whether a chat's turn slot is idle or occupied.
enum TurnState {
    Idle,
    InFlight(CancellationToken),
}

/// Chat state stored under a `ChatId`.
struct ChatState {
    model: String,
    /// Durable per-chat system instructions. Kept outside appendable
    /// history so reset/compaction can replace history without losing
    /// the chat's behavioral contract.
    system: Option<String>,
    history: Arc<Vec<Message>>,
    turn: TurnState,
    stats: ChatStats,
    /// Per-chat tool surface control. Three states:
    ///
    /// - `None` — all tools (no filter). Default for new chats.
    /// - `Some(vec![])` — no tools (disabled). Set when the upstream
    ///   rejects this model's tools or when the creator explicitly
    ///   disabled tools (`tools_enabled: false` on the wire).
    /// - `Some(names)` — filtered to those tool names. The catalog
    ///   stays process-wide; this is per-chat scoping.
    tool_allowlist: Option<Vec<String>>,
    reasoning_effort: Option<String>,
}

impl ChatState {
    fn new(model: String) -> Self {
        Self {
            model,
            system: None,
            history: Arc::new(Vec::new()),
            turn: TurnState::Idle,
            stats: ChatStats::default(),
            tool_allowlist: None,
            reasoning_effort: None,
        }
    }
}

/// Errors callers can hit when manipulating chats. No `unwrap`s in the
/// dispatcher — every state mutation that could conflict returns one of
/// these.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChatsError {
    #[error("chat `{0}` already exists")]
    AlreadyExists(ChatId),
    #[error("chat `{0}` not found")]
    NotFound(ChatId),
    #[error("chat `{0}` is busy")]
    Busy(ChatId),
    #[error("no model configured: pass `--model` to openai-provider, set PROVIDER_MODEL in init.lua, or include `model` in the chat.create body")]
    NoModelConfigured,
}

/// Concurrent-safe map of `ChatId → ChatState`. The full map is wrapped
/// in a single `Mutex` rather than per-chat locks because operations are
/// short (push, snapshot) and the per-turn HTTP call holds no lock — the
/// dispatcher takes a snapshot, releases the lock, runs the streaming
/// request, then re-acquires to write the assistant message back.
#[derive(Default)]
pub struct Chats {
    /// Default model used to seed new chats whose `chat.create` body
    /// omitted a `model` field. None when the user hasn't set a model
    /// (no `--model` arg, no per-chat override). The dispatcher errors
    /// instead of guessing.
    default_model: Mutex<Option<String>>,
    inner: Mutex<HashMap<ChatId, ChatState>>,
    /// Models the upstream rejected with the "does not support tools"
    /// signature. Process-wide (a model's tool capability is a property
    /// of the model, not the chat), so a fresh chat against a known-
    /// incapable model skips the round-trip cost of the first failed
    /// turn. Cleared only by process restart — sufficient because the
    /// model's capabilities don't change mid-process.
    tools_unsupported_models: Mutex<HashSet<String>>,
}

impl Chats {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a Chats with an optional default model. Production passes
    /// the env-resolved `Config::model` (`Option<String>`); tests can
    /// pass `Some("any-model")`.
    pub fn with_default_model(model: Option<String>) -> Self {
        Self {
            default_model: Mutex::new(model),
            inner: Mutex::new(HashMap::new()),
            tools_unsupported_models: Mutex::new(HashSet::new()),
        }
    }

    pub async fn default_model(&self) -> Option<String> {
        self.default_model.lock().await.clone()
    }

    /// Update the default model used to seed *future* chats. Existing
    /// chats keep whatever model they were created with (per-chat
    /// model.set is via `set_chat_model`).
    pub async fn set_default_model(&self, model: String) {
        *self.default_model.lock().await = Some(model);
    }

    /// Create a chat. Errors if a chat with this id already exists, or
    /// when neither a per-chat `model` nor the plugin-default is set
    /// (`ChatsError::NoModelConfigured`).
    ///
    /// Tool surface is controlled by a single `tool_allowlist`:
    /// - `None` → all tools (no filter). Default.
    /// - `Some(vec![])` → no tools (disabled).
    /// - `Some(names)` → filtered to those tool names.
    ///
    /// The `tools_enabled` wire field (bool) is converted here:
    /// `false` → `Some(vec![])`, `true` without an explicit allowlist
    /// → `None`. An explicit array wins over the bool.
    pub async fn create(
        &self,
        id: ChatId,
        model: Option<String>,
        tools_enabled: Option<bool>,
        tool_allowlist: Option<Vec<String>>,
        reasoning_effort: Option<String>,
        system: Option<String>,
    ) -> Result<(), ChatsError> {
        let resolved_model = match model {
            Some(m) => m,
            None => self
                .default_model
                .lock()
                .await
                .clone()
                .ok_or(ChatsError::NoModelConfigured)?,
        };
        let mut g = self.inner.lock().await;
        if g.contains_key(&id) {
            return Err(ChatsError::AlreadyExists(id));
        }
        let mut chat = ChatState::new(resolved_model);
        chat.system = system;
        // Explicit array wins over the bool.
        if let Some(names) = tool_allowlist {
            chat.tool_allowlist = Some(names);
        } else if let Some(false) = tools_enabled {
            chat.tool_allowlist = Some(Vec::new());
        }
        chat.reasoning_effort = reasoning_effort;
        // else: None (all tools) — the default from ChatState::new().
        g.insert(id, chat);
        Ok(())
    }

    pub async fn restore(&self, restore: ChatRestore) -> Result<(), ChatsError> {
        let resolved_model = match restore.model {
            Some(m) => m,
            None => self
                .default_model
                .lock()
                .await
                .clone()
                .ok_or(ChatsError::NoModelConfigured)?,
        };
        let mut chat = ChatState::new(resolved_model);
        chat.system = restore.system;
        if let Some(names) = restore.tool_allowlist {
            chat.tool_allowlist = Some(names);
        } else if let Some(false) = restore.tools_enabled {
            chat.tool_allowlist = Some(Vec::new());
        }
        chat.reasoning_effort = restore.reasoning_effort;
        chat.history = Arc::new(repair_tool_call_history(restore.history));

        let mut g = self.inner.lock().await;
        g.insert(restore.id, chat);
        Ok(())
    }

    /// Snapshot the chat's tool-name allowlist. `Ok(None)` means no
    /// filter (the chat sees the full catalog); `Ok(Some(names))` means
    /// restrict per-turn `tools` to entries whose name is in `names`
    /// (empty Vec → no tools advertised). Errors if the chat doesn't
    /// exist.
    pub async fn tool_allowlist(&self, id: &ChatId) -> Result<Option<Vec<String>>, ChatsError> {
        let g = self.inner.lock().await;
        g.get(id)
            .map(|c| c.tool_allowlist.clone())
            .ok_or_else(|| ChatsError::NotFound(id.clone()))
    }

    /// Set a chat's tool allowlist. Called after the reactive
    /// "model doesn't support tools" 400 lands (with `Some(vec![])`)
    /// so the same chat's next turn skips the round-trip.
    pub async fn set_tool_allowlist(
        &self,
        id: &ChatId,
        allowlist: Option<Vec<String>>,
    ) -> Result<(), ChatsError> {
        let mut g = self.inner.lock().await;
        let chat = g
            .get_mut(id)
            .ok_or_else(|| ChatsError::NotFound(id.clone()))?;
        chat.tool_allowlist = allowlist;
        Ok(())
    }

    pub async fn set_chat_reasoning_effort(
        &self,
        id: &ChatId,
        effort: String,
    ) -> Result<(), ChatsError> {
        let mut g = self.inner.lock().await;
        let chat = g
            .get_mut(id)
            .ok_or_else(|| ChatsError::NotFound(id.clone()))?;
        chat.reasoning_effort = Some(effort);
        Ok(())
    }

    pub async fn reasoning_effort(&self, id: &ChatId) -> Result<Option<String>, ChatsError> {
        let g = self.inner.lock().await;
        g.get(id)
            .map(|c| c.reasoning_effort.clone())
            .ok_or_else(|| ChatsError::NotFound(id.clone()))
    }

    /// Mark a model as known-tools-incapable. Subsequent
    /// `model_supports_tools` checks return false for this exact name.
    /// Comparison is exact: a per-spawn model.set with a slight rename
    /// (e.g. tag suffix) is treated as a separate model and pays the
    /// round-trip once.
    pub async fn mark_model_tools_unsupported(&self, model: &str) {
        self.tools_unsupported_models
            .lock()
            .await
            .insert(model.to_owned());
    }

    /// True unless we've seen the upstream reject this exact model's
    /// chat-completions call with the "does not support tools" signature.
    pub async fn model_supports_tools(&self, model: &str) -> bool {
        !self.tools_unsupported_models.lock().await.contains(model)
    }

    /// Idempotent variant for the default-chat compat path: creates the
    /// chat if absent, no-op if present. Used by `<prefix>.prompt`. Errors
    /// `NoModelConfigured` if the chat has to be created and no default
    /// is set — matches `create()` semantics.
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
            .ok_or(ChatsError::NoModelConfigured)?;
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

    pub async fn history_snapshot(&self, id: &ChatId) -> Result<Arc<Vec<Message>>, ChatsError> {
        let g = self.inner.lock().await;
        g.get(id)
            .map(|c| Arc::clone(&c.history))
            .ok_or_else(|| ChatsError::NotFound(id.clone()))
    }

    /// Snapshot the message list used for upstream model requests.
    /// Durable `chat.create.system` instructions are prepended here,
    /// but are not part of normal appendable history.
    pub async fn request_history_snapshot(
        &self,
        id: &ChatId,
    ) -> Result<Arc<Vec<Message>>, ChatsError> {
        let g = self.inner.lock().await;
        let chat = g.get(id).ok_or_else(|| ChatsError::NotFound(id.clone()))?;
        match &chat.system {
            Some(system) => {
                let mut messages = Vec::with_capacity(chat.history.len() + 1);
                messages.push(Message::system(system.clone()));
                messages.extend(chat.history.iter().cloned());
                Ok(Arc::new(messages))
            }
            None => Ok(Arc::clone(&chat.history)),
        }
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
        Arc::make_mut(&mut chat.history).push(message);
        Ok(())
    }

    pub async fn push_user(&self, id: &ChatId, text: String) -> Result<(), ChatsError> {
        self.append(id, Message::user(text)).await
    }

    pub async fn push_assistant(&self, id: &ChatId, text: String) -> Result<(), ChatsError> {
        self.append(id, Message::assistant(text)).await
    }

    /// Append the assistant message that emitted tool calls. When `text`
    /// is non-empty it rides alongside the calls in the same message
    /// (interleaved prose + tools); when empty the message has
    /// `content: null` per the OpenAI spec.
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

    /// Append a tool result message. `tool_call_id` MUST match the
    /// corresponding assistant tool_calls entry's `id`.
    pub async fn push_tool_result(
        &self,
        id: &ChatId,
        tool_call_id: String,
        content: String,
    ) -> Result<(), ChatsError> {
        self.append(id, Message::tool_result(tool_call_id, content))
            .await
    }

    /// Wipe the history for one chat without dropping the chat itself.
    /// Used by the legacy `<prefix>.reset` event so the chat plugin's
    /// existing "/reset" command keeps working.
    pub async fn reset(&self, id: &ChatId) -> Result<(), ChatsError> {
        let mut g = self.inner.lock().await;
        let chat = g
            .get_mut(id)
            .ok_or_else(|| ChatsError::NotFound(id.clone()))?;
        chat.history = Arc::new(Vec::new());
        Ok(())
    }

    /// Wipe history on every chat. Same shape as `reset` for the legacy
    /// "no chat id" code path — the previous singleton's `reset()`
    /// cleared everything; we preserve that behaviour for backwards
    /// compat.
    pub async fn reset_all(&self) {
        let mut g = self.inner.lock().await;
        for chat in g.values_mut() {
            chat.history = Arc::new(Vec::new());
        }
    }

    /// Begin a turn on `id`. Returns `Ok(token)` if the caller now owns
    /// the chat's turn slot; `Err(Busy)` if the chat is already running
    /// a turn; `Err(NotFound)` if no such chat exists.
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

    /// Cancel the in-flight turn for `id` if one is running.
    /// Returns `true` if a turn was running, `false` otherwise.
    /// Returns `false` (not an error) when the chat doesn't exist —
    /// matches the previous singleton's "interrupt is best-effort"
    /// shape; the dispatcher's caller doesn't care which case.
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

    /// Cancel every in-flight turn across every chat. Used at process
    /// shutdown / ctrl-c.
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
        chat.stats.last_turn_context_tokens = prompt_tokens;
        chat.stats.last_turn_duration_ms = Some(duration_ms);
        Ok(())
    }

    pub async fn stats_snapshot(&self, id: &ChatId) -> Result<ChatStats, ChatsError> {
        let g = self.inner.lock().await;
        g.get(id)
            .map(|c| c.stats.clone())
            .ok_or_else(|| ChatsError::NotFound(id.clone()))
    }

    /// Return all live chat ids. Used for diagnostics / shutdown
    /// bookkeeping (e.g. emitting one final `chat.deleted` per chat).
    pub async fn ids(&self) -> Vec<ChatId> {
        self.inner.lock().await.keys().cloned().collect()
    }
}

fn has_unanswered_tool_call(history: &[Message], tool_call_id: &str) -> bool {
    let mut seen_call = false;
    for message in history {
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
    use crate::openai::{ToolCall, ToolCallFunction};

    #[tokio::test]
    async fn create_then_append_and_snapshot_round_trips_through_history() {
        let c = Chats::with_default_model(Some("m".into()));
        let id = ChatId::new("a");
        c.create(id.clone(), None, None, None, None, None)
            .await
            .expect("create");
        c.push_user(&id, "hello".into()).await.expect("push user");
        c.push_assistant(&id, "hi there".into())
            .await
            .expect("push assistant");
        let h = c.history_snapshot(&id).await.expect("snapshot");
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].role(), "user");
        assert_eq!(h[0].content(), Some("hello"));
        assert_eq!(h[1].role(), "assistant");
        assert_eq!(h[1].content(), Some("hi there"));
    }

    #[tokio::test]
    async fn create_system_is_durable_request_context_not_append_history() {
        let c = Chats::with_default_model(Some("m".into()));
        let id = ChatId::new("a");
        c.create(
            id.clone(),
            None,
            None,
            None,
            None,
            Some("stay terse".into()),
        )
        .await
        .expect("create");
        c.push_user(&id, "hello".into()).await.expect("push user");

        let display_history = c.history_snapshot(&id).await.expect("history");
        assert_eq!(display_history.len(), 1);
        assert_eq!(display_history[0].role(), "user");

        let request_history = c
            .request_history_snapshot(&id)
            .await
            .expect("request history");
        assert_eq!(request_history.len(), 2);
        assert_eq!(request_history[0].role(), "system");
        assert_eq!(request_history[0].content(), Some("stay terse"));
        assert_eq!(request_history[1].role(), "user");

        c.reset(&id).await.expect("reset");
        let after_reset = c
            .request_history_snapshot(&id)
            .await
            .expect("request history after reset");
        assert_eq!(after_reset.len(), 1);
        assert_eq!(after_reset[0].role(), "system");
        assert_eq!(after_reset[0].content(), Some("stay terse"));
    }

    #[tokio::test]
    async fn create_rejects_duplicate_id() {
        let c = Chats::with_default_model(Some("m".into()));
        let id = ChatId::new("dup");
        c.create(id.clone(), None, None, None, None, None)
            .await
            .expect("first create");
        let err = c
            .create(id.clone(), None, None, None, None, None)
            .await
            .expect_err("second create");
        assert!(matches!(err, ChatsError::AlreadyExists(x) if x == id));
    }

    #[tokio::test]
    async fn delete_removes_chat_and_subsequent_ops_404() {
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
    async fn append_to_unknown_chat_errors() {
        let c = Chats::with_default_model(Some("m".into()));
        let err = c
            .push_user(&ChatId::new("ghost"), "hi".into())
            .await
            .expect_err("missing");
        assert!(matches!(err, ChatsError::NotFound(_)));
    }

    #[tokio::test]
    async fn reset_clears_only_target_chat_history() {
        let c = Chats::with_default_model(Some("m".into()));
        let a = ChatId::new("a");
        let b = ChatId::new("b");
        c.create(a.clone(), None, None, None, None, None)
            .await
            .expect("a");
        c.create(b.clone(), None, None, None, None, None)
            .await
            .expect("b");
        c.push_user(&a, "x".into()).await.expect("a push");
        c.push_user(&b, "y".into()).await.expect("b push");
        c.reset(&a).await.expect("reset a");
        assert!(c.history_snapshot(&a).await.expect("a").is_empty());
        assert_eq!(c.history_snapshot(&b).await.expect("b").len(), 1);
    }

    #[tokio::test]
    async fn reset_all_clears_every_chats_history() {
        let c = Chats::with_default_model(Some("m".into()));
        let a = ChatId::new("a");
        let b = ChatId::new("b");
        c.create(a.clone(), None, None, None, None, None)
            .await
            .expect("a");
        c.create(b.clone(), None, None, None, None, None)
            .await
            .expect("b");
        c.push_user(&a, "x".into()).await.expect("a push");
        c.push_user(&b, "y".into()).await.expect("b push");
        c.reset_all().await;
        assert!(c.history_snapshot(&a).await.expect("a").is_empty());
        assert!(c.history_snapshot(&b).await.expect("b").is_empty());
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
        // Same chat → busy.
        let busy = c.begin_turn(&a).await.expect_err("a busy");
        assert!(matches!(busy, ChatsError::Busy(x) if x == a));
        // Different chat → fine, runs in parallel.
        let _t2 = c.begin_turn(&b).await.expect("b parallel");
    }

    #[tokio::test]
    async fn interrupt_only_cancels_named_chat() {
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
        assert!(!tb.is_cancelled(), "interrupt of `a` must not affect `b`");
    }

    #[tokio::test]
    async fn interrupt_all_cancels_every_in_flight_turn() {
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
        c.interrupt_all().await;
        assert!(ta.is_cancelled());
        assert!(tb.is_cancelled());
    }

    #[tokio::test]
    async fn create_with_explicit_model_overrides_default() {
        let c = Chats::with_default_model(Some("default".into()));
        let id = ChatId::new("a");
        c.create(id.clone(), Some("explicit".into()), None, None, None, None)
            .await
            .expect("create");
        assert_eq!(c.model(&id).await.expect("model"), "explicit");
    }

    #[tokio::test]
    async fn set_chat_model_updates_per_chat_only() {
        let c = Chats::with_default_model(Some("default".into()));
        let a = ChatId::new("a");
        let b = ChatId::new("b");
        c.create(a.clone(), None, None, None, None, None)
            .await
            .expect("a");
        c.create(b.clone(), None, None, None, None, None)
            .await
            .expect("b");
        c.set_chat_model(&a, "new".into()).await.expect("set a");
        assert_eq!(c.model(&a).await.expect("a"), "new");
        assert_eq!(c.model(&b).await.expect("b"), "default");
    }

    #[tokio::test]
    async fn ensure_is_idempotent_for_legacy_default_chat_path() {
        let c = Chats::with_default_model(Some("m".into()));
        let id = ChatId::default_for_prefix("ollama.");
        c.ensure(id.clone()).await.expect("ensure ok");
        c.ensure(id.clone()).await.expect("ensure ok");
        assert!(c.exists(&id).await);
    }

    #[tokio::test]
    async fn default_chat_id_per_prefix_uniqueness() {
        let a = ChatId::default_for_prefix("ollama.");
        let b = ChatId::default_for_prefix("groq.");
        assert_ne!(a, b);
        assert_eq!(a.as_str(), "ollama:default");
        assert_eq!(b.as_str(), "groq:default");
    }

    #[tokio::test]
    async fn record_turn_accumulates_per_chat() {
        let c = Chats::with_default_model(Some("m".into()));
        let id = ChatId::new("a");
        c.create(id.clone(), None, None, None, None, None)
            .await
            .expect("create");
        c.record_turn(&id, Some("qwen"), 100, 50, 1234)
            .await
            .expect("first");
        c.record_turn(&id, Some("qwen"), 120, 60, 2222)
            .await
            .expect("second");
        let snap = c.stats_snapshot(&id).await.expect("snap");
        assert_eq!(snap.model.as_deref(), Some("qwen"));
        assert_eq!(snap.turns_completed, 2);
        assert_eq!(snap.cumulative_input_tokens, 220);
        assert_eq!(snap.cumulative_output_tokens, 110);
        assert_eq!(snap.last_turn_input_tokens, 120);
        assert_eq!(snap.last_turn_output_tokens, 60);
        assert_eq!(snap.last_turn_duration_ms, Some(2222));
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
            kind: "function".into(),
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
        let h = c.history_snapshot(&id).await.expect("h");
        assert_eq!(h.len(), 3);
        assert_eq!(h[1].role(), "assistant");
        assert!(h[1].content().is_none());
        assert_eq!(h[1].tool_calls().len(), 1);
        assert_eq!(h[2].role(), "tool");
        assert_eq!(h[2].tool_call_id(), Some("call_1"));
        assert_eq!(h[2].content(), Some("file contents"));
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

        let h = c.history_snapshot(&id).await.expect("h");
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
            kind: "function".into(),
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

        let h = c.history_snapshot(&id).await.expect("h");
        assert_eq!(h.len(), 2);
        assert_eq!(h[1].content(), Some("first"));
    }

    #[tokio::test]
    async fn restore_repairs_unanswered_tool_calls_before_next_user() {
        let c = Chats::with_default_model(Some("m".into()));
        let id = ChatId::new("a");
        let calls = vec![ToolCall {
            id: "call_1".into(),
            kind: "function".into(),
            function: ToolCallFunction {
                name: "read_file".into(),
                arguments: "{\"path\":\"/x\"}".into(),
            },
        }];
        c.restore(ChatRestore {
            id: id.clone(),
            model: None,
            tools_enabled: None,
            tool_allowlist: None,
            reasoning_effort: None,
            system: None,
            history: vec![
                Message::user("hi"),
                Message::assistant_with_tool_calls("", calls),
                Message::user("continue"),
            ],
        })
        .await
        .expect("restore");

        let h = c.history_snapshot(&id).await.expect("history");
        assert_eq!(h.len(), 4);
        assert_eq!(h[2].role(), "tool");
        assert_eq!(h[2].tool_call_id(), Some("call_1"));
        assert_eq!(
            h[2].content(),
            Some("Tool call was interrupted before producing output.")
        );
        assert_eq!(h[3].role(), "user");
    }

    #[tokio::test]
    async fn ids_returns_every_live_chat() {
        let c = Chats::with_default_model(Some("m".into()));
        c.create(ChatId::new("a"), None, None, None, None, None)
            .await
            .expect("a");
        c.create(ChatId::new("b"), None, None, None, None, None)
            .await
            .expect("b");
        let mut ids = c.ids().await;
        ids.sort();
        assert_eq!(ids, vec![ChatId::new("a"), ChatId::new("b")]);
    }

    #[tokio::test]
    async fn delete_unknown_chat_errors() {
        let c = Chats::with_default_model(Some("m".into()));
        let err = c
            .delete(&ChatId::new("ghost"))
            .await
            .expect_err("delete missing");
        assert!(matches!(err, ChatsError::NotFound(_)));
    }

    // --- tools-unsupported model cache ------------------------------

    #[tokio::test]
    async fn fresh_chats_supports_tools_for_any_model() {
        let c = Chats::with_default_model(Some("m".into()));
        assert!(c.model_supports_tools("translategemma").await);
        assert!(c.model_supports_tools("anything-else").await);
    }

    #[tokio::test]
    async fn marking_a_model_unsupported_persists_across_calls() {
        let c = Chats::with_default_model(Some("m".into()));
        c.mark_model_tools_unsupported("translategemma").await;
        assert!(!c.model_supports_tools("translategemma").await);
        // Other models unaffected.
        assert!(c.model_supports_tools("qwen3").await);
        // Idempotent re-mark.
        c.mark_model_tools_unsupported("translategemma").await;
        assert!(!c.model_supports_tools("translategemma").await);
    }

    #[tokio::test]
    async fn set_tool_allowlist_flips_per_chat_flag() {
        let c = Chats::with_default_model(Some("m".into()));
        let id = ChatId::new("a");
        c.create(id.clone(), None, None, None, None, None)
            .await
            .expect("create");
        // Default: None = all tools.
        assert_eq!(c.tool_allowlist(&id).await.expect("default"), None);
        // Disable tools via empty allowlist.
        c.set_tool_allowlist(&id, Some(vec![]))
            .await
            .expect("disable");
        assert_eq!(c.tool_allowlist(&id).await.expect("disabled"), Some(vec![]));
        // Re-enable all tools.
        c.set_tool_allowlist(&id, None).await.expect("re-enable");
        assert_eq!(c.tool_allowlist(&id).await.expect("re-enabled"), None);
    }

    #[tokio::test]
    async fn set_tool_allowlist_on_unknown_chat_errors() {
        let c = Chats::with_default_model(Some("m".into()));
        let err = c
            .set_tool_allowlist(&ChatId::new("ghost"), Some(vec![]))
            .await
            .expect_err("ghost");
        assert!(matches!(err, ChatsError::NotFound(_)));
    }
}
