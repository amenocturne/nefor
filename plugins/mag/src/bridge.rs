//! Provider capability bridge — adapts the kernel's single-shot capability
//! request/response contract onto a provider plugin's multi-message `chat.*`
//! conversation.
//!
//! ## The gap this closes
//!
//! The kernel is provider-agnostic. An `llm` actor emits one `capability.invoke`;
//! routing.lua mints a correlation id and puts one
//! `tool.invoke { id, name = <provider>, args = <request> }` on the bus, then
//! awaits one correlated `tool.result` (routing.lua `on_capability_invoke` /
//! `bus_response`). But a real provider plugin (chatgpt-provider, mock-provider)
//! speaks a conversation — `<provider>.chat.create` → `.chat.append`× →
//! `.chat.complete` → a streamed result (`<provider>.chat.complete.result`) — and
//! advertises no `tool.invoke` surface. Nothing bridged the two, so a real
//! kernel-path graph stalled at the first `llm` call.
//!
//! ## What the bridge does
//!
//! Option (a): a host-side adapter now (the clean fix is a pure single-shot
//! `complete(messages, config)` provider API that deletes this — see
//! task-nefor-mag-plugin-state-cleanup). It sits at the mag plugin's bus
//! boundary (main.rs), between the kernel's emit queue and the wire:
//!
//!   * Outbound: a provider-class `tool.invoke` (its `args` carry a `chat_id`
//!     handle — the discriminant, flagged below) is NOT forwarded bare. Instead
//!     the bridge drives the conversation for that request — emit `chat.create`
//!     (threading model/system/tools from the request, keyed by the factory's
//!     own `chat_id` so the `llm`'s cancel handle stays valid), one `chat.append`
//!     per turn message, then `chat.complete` — and records `chat_id →
//!     request_id` so the eventual result correlates back.
//!   * Inbound: `<provider>.chat.complete.result { chat_id, output }` (or
//!     `.chat.error { chat_id, message }`) resolves the request_id by chat_id;
//!     the host feeds the single final `output` back through
//!     `kernel.bus_response` as the correlated reply, then a `chat.delete` cleans
//!     up the provider-side chat.
//!   * Deltas (`<provider>.stream.delta` / `.stream.end`) are the provider's own
//!     bus emissions; the TUI reads them straight off the bus and the kernel
//!     stays delta-blind — the bridge neither produces nor consumes them.
//!
//! ## Discrimination (FLAGGED)
//!
//! A `tool.invoke` is provider-class iff its `args.chat_id` is a non-empty
//! string. Rationale: the `llm` factory mints a `chat_id` as the provider request
//! handle and puts it in the request (factories/llm.lua `build_request`); a
//! `run-tool` invocation's request is `{ name, args, allowlist, da-policy }` and
//! never carries one. Keying on the request field rather than a provider-name
//! allowlist keeps the host free of cross-plugin knowledge (no hard-coded list of
//! provider plugins) and is forward-compatible: the pure provider API will still
//! carry a per-conversation handle.
//!
//! ## Concurrency / interleaving (FLAGGED)
//!
//! All bridge state is keyed by `chat_id`, and each `chat_id` is
//! `<actor_id>@r<seq>` (factories/llm.lua), so two in-flight `llm` actors hold two
//! independent chats with disjoint ids. A result routes back to exactly the actor
//! whose chat_id it names; responses may arrive in any order (reverse of request
//! order included) and never cross.

use std::collections::HashMap;

use serde_json::{Map, Value};

const TOOL_INVOKE: &str = "tool.invoke";
const RESULT_SUFFIX: &str = ".chat.complete.result";
const ERROR_SUFFIX: &str = ".chat.error";

/// A driven provider conversation awaiting its final result.
struct PendingChat {
    /// The kernel-minted correlation id from the intercepted `tool.invoke` — the
    /// id `kernel.bus_response` answers.
    request_id: String,
    /// The provider capability name (the `tool.invoke` `name`): the bus prefix
    /// for the chat.* envelopes and the cleanup `chat.delete`.
    provider: String,
}

/// A correlated provider reply, resolved from an inbound `chat.complete.result`
/// or `chat.error`, ready to feed to `kernel.bus_response`.
pub struct ProviderReply {
    /// The correlation id to answer.
    pub request_id: String,
    /// The provider bus prefix (for the `chat.delete` cleanup envelope).
    pub provider: String,
    /// The chat handle to delete once the single result is collected.
    pub chat_id: String,
    /// The provider's final `output` (a ProviderOut), when the chat completed.
    pub result: Option<Value>,
    /// The provider's error message, when the chat closed with `chat.error`.
    pub error: Option<String>,
}

/// Correlation table for the driven provider conversations, keyed by chat_id.
#[derive(Default)]
pub struct ProviderBridge {
    pending: HashMap<String, PendingChat>,
}

impl ProviderBridge {
    pub fn new() -> Self {
        Self::default()
    }

    /// Translate one kernel-drained emit body into the bus envelope(s) that go on
    /// the wire. A provider-class `tool.invoke` becomes the
    /// create/append/complete sequence (and records the correlation); everything
    /// else — lifecycle events, tool `tool.invoke`s, the raw kill-time cancel
    /// envelope — passes through untouched.
    pub fn translate_emit(&mut self, body: Map<String, Value>) -> Vec<Map<String, Value>> {
        if body.get("kind").and_then(Value::as_str) != Some(TOOL_INVOKE) {
            return vec![body];
        }
        let chat_id = body
            .get("args")
            .and_then(Value::as_object)
            .and_then(|a| a.get("chat_id"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        // Not provider-class (no chat_id handle) → a tool invocation; forward it.
        let chat_id = match chat_id {
            Some(c) => c.to_owned(),
            None => return vec![body],
        };
        // A provider-class invoke with no correlation id or name can't be driven;
        // forward it and let the bus surface the anomaly rather than swallow it.
        let request_id = match body.get("id").and_then(Value::as_str) {
            Some(id) => id.to_owned(),
            None => return vec![body],
        };
        let provider = match body.get("name").and_then(Value::as_str) {
            Some(n) => n.to_owned(),
            None => return vec![body],
        };
        let args = body
            .get("args")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();

        let mut out = Vec::with_capacity(3);
        out.push(create_envelope(&provider, &chat_id, &args));
        for message in append_messages(&args) {
            out.push(append_envelope(&provider, &chat_id, message));
        }
        out.push(complete_envelope(&provider, &chat_id));

        self.pending.insert(
            chat_id,
            PendingChat {
                request_id,
                provider,
            },
        );
        out
    }

    /// Whether an inbound event kind is a provider reply the bridge may own.
    pub fn is_provider_reply(kind: &str) -> bool {
        kind.ends_with(RESULT_SUFFIX) || kind.ends_with(ERROR_SUFFIX)
    }

    /// Resolve an inbound provider reply against the driven chats. Returns `None`
    /// when the chat_id names no chat we drive (a reply for another consumer —
    /// e.g. a reasoner-graph chat sharing the broadcast bus), so the caller
    /// ignores it. Removes the correlation on a hit: one request, one reply.
    pub fn take_reply(&mut self, kind: &str, body: &Map<String, Value>) -> Option<ProviderReply> {
        let chat_id = body.get("chat_id").and_then(Value::as_str)?;
        let pending = self.pending.remove(chat_id)?;
        let (result, error) = if kind.ends_with(ERROR_SUFFIX) {
            let msg = body
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("provider chat error")
                .to_owned();
            (None, Some(msg))
        } else {
            (body.get("output").cloned(), None)
        };
        Some(ProviderReply {
            request_id: pending.request_id,
            provider: pending.provider,
            chat_id: chat_id.to_owned(),
            result,
            error,
        })
    }
}

/// The provider-side cleanup envelope: delete the chat once its single result is
/// collected. Each `llm` turn mints a fresh chat_id (factories/llm.lua `seq`), so
/// a completed chat is never reused — deleting it frees the provider's per-chat
/// state.
pub fn delete_envelope(provider: &str, chat_id: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{provider}.chat.delete")),
    );
    m.insert("chat_id".into(), Value::String(chat_id.to_owned()));
    m
}

fn create_envelope(provider: &str, chat_id: &str, args: &Map<String, Value>) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{provider}.chat.create")),
    );
    m.insert("chat_id".into(), Value::String(chat_id.to_owned()));
    // Thread the call config the provider needs from the request. Absent/null
    // fields are omitted so the provider falls back to its own defaults.
    for key in ["model", "system", "tools"] {
        match args.get(key) {
            Some(v) if !v.is_null() => {
                m.insert(key.into(), v.clone());
            }
            _ => {}
        }
    }
    m
}

fn append_envelope(provider: &str, chat_id: &str, message: Value) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{provider}.chat.append")),
    );
    m.insert("chat_id".into(), Value::String(chat_id.to_owned()));
    m.insert("message".into(), message);
    m
}

fn complete_envelope(provider: &str, chat_id: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{provider}.chat.complete")),
    );
    m.insert("chat_id".into(), Value::String(chat_id.to_owned()));
    m
}

/// The turn messages to append from a provider request. The `llm` factory hands
/// the whole ProviderOut through as `input`, whose canonical shape carries a
/// `messages` array of role-tagged turn messages (factories/adapter.lua,
/// factories/tool-result.lua). Append each. A bare single message (`{ role, … }`)
/// or a `{ text }` shape are tolerated as fallbacks so a hand-authored request
/// still drives.
fn append_messages(args: &Map<String, Value>) -> Vec<Value> {
    let input = match args.get("input") {
        Some(i) => i,
        None => return Vec::new(),
    };
    if let Some(obj) = input.as_object() {
        if let Some(msgs) = obj.get("messages").and_then(Value::as_array) {
            return msgs.iter().filter(|m| !m.is_null()).cloned().collect();
        }
        if obj.contains_key("role") {
            return vec![input.clone()];
        }
        if let Some(text) = obj.get("text").and_then(Value::as_str) {
            return vec![user_message(text)];
        }
    }
    if let Some(text) = input.as_str() {
        return vec![user_message(text)];
    }
    Vec::new()
}

fn user_message(text: &str) -> Value {
    let mut m = Map::new();
    m.insert("role".into(), Value::String("user".into()));
    m.insert("content".into(), Value::String(text.to_owned()));
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().expect("object").clone()
    }

    #[test]
    fn provider_invoke_drives_the_chat_conversation_and_records_correlation() {
        let mut bridge = ProviderBridge::new();
        let invoke = obj(json!({
            "kind": "tool.invoke",
            "id": "corr-1",
            "name": "chatgpt-provider",
            "args": {
                "chat_id": "agent@r1",
                "model": "opus",
                "system": "be helpful",
                "tools": ["fs/read"],
                "input": { "kind": "generic-provider.ProviderOut",
                           "messages": [ { "role": "user", "content": "hi" } ] }
            }
        }));
        let out = bridge.translate_emit(invoke);
        let kinds: Vec<&str> = out
            .iter()
            .map(|e| e.get("kind").and_then(Value::as_str).unwrap_or(""))
            .collect();
        assert_eq!(
            kinds,
            vec![
                "chatgpt-provider.chat.create",
                "chatgpt-provider.chat.append",
                "chatgpt-provider.chat.complete",
            ],
            "a provider invoke becomes the create/append/complete sequence"
        );
        // create threads config, keyed by the factory's chat_id.
        let create = &out[0];
        assert_eq!(
            create.get("chat_id").and_then(Value::as_str),
            Some("agent@r1")
        );
        assert_eq!(create.get("model").and_then(Value::as_str), Some("opus"));
        assert_eq!(
            create.get("system").and_then(Value::as_str),
            Some("be helpful")
        );
        assert!(create.get("tools").and_then(Value::as_array).is_some());
        // append carries the turn message verbatim.
        let msg = out[1]
            .get("message")
            .and_then(Value::as_object)
            .expect("message");
        assert_eq!(msg.get("role").and_then(Value::as_str), Some("user"));
        assert_eq!(msg.get("content").and_then(Value::as_str), Some("hi"));

        // A completion result correlates back to the minted correlation id.
        let result = obj(json!({
            "kind": "chatgpt-provider.chat.complete.result",
            "chat_id": "agent@r1",
            "output": { "text": "done" }
        }));
        assert!(ProviderBridge::is_provider_reply(
            result.get("kind").and_then(Value::as_str).unwrap()
        ));
        let reply = bridge
            .take_reply("chatgpt-provider.chat.complete.result", &result)
            .expect("reply resolves");
        assert_eq!(reply.request_id, "corr-1");
        assert_eq!(reply.provider, "chatgpt-provider");
        assert_eq!(reply.chat_id, "agent@r1");
        assert_eq!(
            reply
                .result
                .as_ref()
                .and_then(|r| r.get("text"))
                .and_then(Value::as_str),
            Some("done")
        );
        assert!(reply.error.is_none());
        // One request, one reply: the correlation is consumed.
        assert!(bridge
            .take_reply("chatgpt-provider.chat.complete.result", &result)
            .is_none());
    }

    #[test]
    fn tool_invoke_without_chat_id_forwards_unchanged() {
        let mut bridge = ProviderBridge::new();
        let invoke = obj(json!({
            "kind": "tool.invoke",
            "id": "corr-2",
            "name": "fs/read",
            "args": { "name": "fs/read", "args": { "path": "x" } }
        }));
        let out = bridge.translate_emit(invoke.clone());
        assert_eq!(out, vec![invoke], "a tool invocation passes through bare");
    }

    #[test]
    fn non_invoke_emit_forwards_unchanged() {
        let mut bridge = ProviderBridge::new();
        let event = obj(json!({ "kind": "mag.actor_ready", "id": "sink" }));
        let out = bridge.translate_emit(event.clone());
        assert_eq!(out, vec![event]);
    }

    #[test]
    fn chat_error_resolves_as_an_error_reply() {
        let mut bridge = ProviderBridge::new();
        bridge.translate_emit(obj(json!({
            "kind": "tool.invoke", "id": "corr-3", "name": "p",
            "args": { "chat_id": "a@r1", "input": { "messages": [] } }
        })));
        let err =
            obj(json!({ "kind": "p.chat.error", "chat_id": "a@r1", "message": "interrupted" }));
        let reply = bridge
            .take_reply("p.chat.error", &err)
            .expect("error reply");
        assert_eq!(reply.request_id, "corr-3");
        assert_eq!(reply.error.as_deref(), Some("interrupted"));
        assert!(reply.result.is_none());
    }

    #[test]
    fn reply_for_unknown_chat_is_ignored() {
        let mut bridge = ProviderBridge::new();
        let result = obj(json!({
            "kind": "p.chat.complete.result", "chat_id": "not-ours", "output": {}
        }));
        assert!(bridge
            .take_reply("p.chat.complete.result", &result)
            .is_none());
    }

    #[test]
    fn concurrent_chats_stay_independent_and_resolve_in_any_order() {
        let mut bridge = ProviderBridge::new();
        for (corr, chat) in [("corr-a", "agentA@r1"), ("corr-b", "agentB@r1")] {
            bridge.translate_emit(obj(json!({
                "kind": "tool.invoke", "id": corr, "name": "p",
                "args": { "chat_id": chat, "input": { "messages": [] } }
            })));
        }
        // Resolve in reverse request order: each maps to its own correlation id.
        let rb = bridge
            .take_reply(
                "p.chat.complete.result",
                &obj(json!({ "kind": "p.chat.complete.result", "chat_id": "agentB@r1", "output": {} })),
            )
            .expect("B resolves");
        assert_eq!(rb.request_id, "corr-b");
        let ra = bridge
            .take_reply(
                "p.chat.complete.result",
                &obj(json!({ "kind": "p.chat.complete.result", "chat_id": "agentA@r1", "output": {} })),
            )
            .expect("A resolves");
        assert_eq!(ra.request_id, "corr-a");
    }
}
