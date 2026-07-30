//! Capability bridge — adapts the kernel's single-shot capability
//! request/response contract onto the bus dialects real capability plugins
//! actually speak: a provider's multi-message `chat.*` conversation, and the
//! tool gate's prefix-routed `<gate>.tool.invoke` surface.
//!
//! ## The gap this closes
//!
//! The kernel is capability-agnostic. An actor emits one `capability.invoke`;
//! routing.lua mints a correlation id and puts one
//! `tool.invoke { id, name = <capability>, args = <request> }` on the bus, then
//! awaits one correlated `tool.result` (routing.lua `on_capability_invoke` /
//! `bus_response`). But nothing on the bus subscribes to a bare `tool.invoke`:
//!
//! * A real provider plugin (chatgpt-provider, mock-provider) speaks a
//!   conversation — `<provider>.chat.create` → `.chat.append`× →
//!   `.chat.complete` → a streamed result (`<provider>.chat.complete.result`) —
//!   and advertises no `tool.invoke` surface.
//! * Tool invocations are owned by the tool gate, which prefix-routes on
//!   `<gate>.tool.invoke { id, name, args }` (plugins/tool-gate/src/main.rs)
//!   and answers with a broadcast `tool.result { id, output | error }` keyed by
//!   the caller's id.
//!
//! Nothing bridged either dialect, so a real kernel-path graph stalled at its
//! first `llm` call (pre-provider-bridge) and, once that landed, at its first
//! tool call.
//!
//! ## The tool leg
//!
//! A tool-class `tool.invoke` (no `args.chat_id` — see Discrimination) is
//! rewritten to `<gate>.tool.invoke` with the payload unwrapped to the gate's
//! contract. The kernel's request is double-wrapped —
//! `{ id, name, args = { name, args = <tool args>, allowlist, da-policy } }`
//! (factories/run-tool.lua carries per-node gating alongside the call) — while
//! the gate reads `{ id, name, args = <tool args> }` and forwards `args`
//! verbatim to the owning tool plugin. So the inner `args` is lifted out, and
//! `allowlist` / `da-policy` ride as top-level siblings. The gate enforces
//! allowlist membership and forwards the same inventory to approval
//! classification.
//!
//! The kernel correlation id is kept as the gate's outer id: the gate echoes it
//! on its `tool.result`, which the plugin's existing tool.result path feeds to
//! `kernel.bus_response` (main.rs handle_tool_result). The tool leg therefore
//! needs no bridge state — it is a pure envelope rewrite.
//!
//! The gate's bus name is composition-owned (the starter names the gate when it
//! spawns it — starter/init.lua `tools.gate_spec("tool-gate", …)`), so it is
//! threaded here through the plugin's spawn config (`--tool-gate`), not
//! hard-coded.
//!
//! ## What the provider leg does
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
//! All bridge state is keyed by `chat_id`. The kernel's run-scoped bus seam
//! prefixes every chat handle with the run's scope token before it reaches
//! this bridge — the wire shape is `r<K>/<actor_id>@r<seq>`
//! (plugins/mag/lua/mag-kernel/init.lua bus_emit; the unscoped `<actor_id>@r<seq>` is
//! minted in factories/llm.lua) — so two in-flight `llm` actors hold disjoint
//! chats even across CONCURRENT RUNS of the same program, where actor ids and
//! round counters coincide. The bridge never parses a chat_id: it is an
//! opaque exact-match key. A result routes back to exactly the actor whose
//! chat_id it names; responses may arrive in any order (reverse of request
//! order included) and never cross.

use std::collections::HashMap;

use serde_json::{Map, Value};

const TOOL_INVOKE: &str = "tool.invoke";
const TOOL_CANCEL: &str = "tool.cancel";
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

/// Capability-invoke translator: correlation table for the driven provider
/// conversations (keyed by chat_id) plus the stateless tool-leg rewrite onto
/// the composition-named gate.
pub struct CapabilityBridge {
    pending: HashMap<String, PendingChat>,
    /// The tool gate's bus name — the prefix tool-class invokes are rewritten
    /// to. Composition-owned; threaded from the plugin's spawn config.
    gate: String,
}

impl CapabilityBridge {
    pub fn new(gate: impl Into<String>) -> Self {
        Self {
            pending: HashMap::new(),
            gate: gate.into(),
        }
    }

    /// Translate one kernel-drained emit body into the bus envelope(s) that go on
    /// the wire. A provider-class `tool.invoke` becomes the
    /// create/append/complete sequence (and records the correlation); a
    /// tool-class `tool.invoke` is rewritten to the gate's
    /// `<gate>.tool.invoke` contract; everything else — lifecycle events, the
    /// raw kill-time cancel envelope — passes through untouched.
    pub fn translate_emit(&mut self, body: Map<String, Value>) -> Vec<Map<String, Value>> {
        let kind = body.get("kind").and_then(Value::as_str);
        if kind == Some(TOOL_CANCEL) {
            return self.translate_cancel(&body);
        }
        if kind != Some(TOOL_INVOKE) {
            return vec![body];
        }
        let chat_id = body
            .get("args")
            .and_then(Value::as_object)
            .and_then(|a| a.get("chat_id"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        // Not provider-class (no chat_id handle) → a tool invocation; rewrite it
        // onto the gate. Nothing on the bus subscribes to a bare `tool.invoke`,
        // so forwarding it verbatim would strand the kernel's open correlation.
        let chat_id = match chat_id {
            Some(c) => c.to_owned(),
            None => return vec![gate_invoke(&self.gate, &body)],
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

        let mut out = Vec::with_capacity(4);
        out.push(create_envelope(&provider, &chat_id, &args));
        if let Some(artifact) = args.get("context_artifact").filter(|v| !v.is_null()) {
            out.push(compaction_restore_envelope(&provider, &chat_id, artifact));
        }
        for message in append_messages(&args) {
            out.push(append_envelope(&provider, &chat_id, message));
        }
        out.push(complete_envelope(&provider, &chat_id, &args));

        self.pending.insert(
            chat_id,
            PendingChat {
                request_id,
                provider,
            },
        );
        out
    }

    /// Translate the kernel's interrupt `tool.cancel { id }` (routing.lua
    /// interrupt emits one per open capability correlation) onto the dialect
    /// the owning capability actually speaks — symmetric with the invoke path:
    ///
    /// * A correlation that names a driven PROVIDER chat (its `request_id` is
    ///   in `pending`) becomes `<provider>.chat.cancel { chat_id }`, the abort
    ///   the llm factory's own kill handler emits (§2.4). We do NOT drop the
    ///   pending chat: its result/error still arrives and resolves the (now
    ///   kernel-settled) correlation to nothing, one request one reply.
    /// * Otherwise it is a TOOL correlation → `<gate>.tool.cancel { id }`, which
    ///   the gate forwards to the owning source under its inner id.
    fn translate_cancel(&self, body: &Map<String, Value>) -> Vec<Map<String, Value>> {
        let id = match body.get("id").and_then(Value::as_str) {
            Some(id) => id,
            None => return Vec::new(),
        };
        for (chat_id, pending) in &self.pending {
            if pending.request_id == id {
                return vec![chat_cancel_envelope(&pending.provider, chat_id)];
            }
        }
        vec![gate_cancel(&self.gate, id)]
    }

    /// Resolve a driven provider chat to its kernel correlation without consuming it.
    /// Streaming telemetry uses this while the final reply still owns settlement.
    pub fn request_id_for_chat(&self, chat_id: &str) -> Option<&str> {
        self.pending
            .get(chat_id)
            .map(|pending| pending.request_id.as_str())
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

/// Rewrite a tool-class `tool.invoke` onto the gate's contract
/// (plugins/tool-gate: `<gate>.tool.invoke { id, name, args }`, answered by a
/// broadcast `tool.result` keyed by the same id).
///
/// The kernel's request is double-wrapped
/// (`args = { name, args = <tool args>, allowlist, da-policy }` —
/// factories/run-tool.lua), so the inner `args` is lifted out as the gate's
/// `args`; a request without the wrap (a hand-authored capability request) is
/// forwarded as the args whole. The kernel correlation id rides through as the
/// gate's outer id, so the gate's `tool.result` correlates straight back to the
/// kernel's open request. Per-node gating (`allowlist` / `da-policy`) rides
/// top-level for runtime capability enforcement and approval classification.
fn gate_invoke(gate: &str, body: &Map<String, Value>) -> Map<String, Value> {
    let request = body
        .get("args")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut m = Map::new();
    m.insert("kind".into(), Value::String(format!("{gate}.tool.invoke")));
    if let Some(id) = body.get("id").and_then(Value::as_str) {
        m.insert("id".into(), Value::String(id.to_owned()));
    }
    // The emitting actor's plain address (routing.lua on_capability_invoke
    // stamps `from` on the kernel's envelope): threaded through so bus
    // observers can attribute the invoke to an actor without parsing the
    // scoped correlation id.
    if let Some(from) = body.get("from").and_then(Value::as_str) {
        m.insert("from".into(), Value::String(from.to_owned()));
    }
    // Structured invocation provenance is stamped by the authoritative run
    // context and preserved verbatim. Downstream diagnostics must not recover
    // run/actor identity by parsing the opaque correlation id.
    if let Some(invocation @ Value::Object(_)) = body.get("invocation") {
        m.insert("invocation".into(), invocation.clone());
    }
    // The tool name: routing.lua stamps the capability name on the envelope;
    // the wrapped request carries the same name (run-tool sets both).
    if let Some(name) = body
        .get("name")
        .or_else(|| request.get("name"))
        .and_then(Value::as_str)
    {
        m.insert("name".into(), Value::String(name.to_owned()));
    }
    let args = match request.get("args") {
        Some(inner @ Value::Object(_)) => inner.clone(),
        _ => Value::Object(request.clone()),
    };
    m.insert("args".into(), args);
    for key in ["allowlist", "da-policy"] {
        match request.get(key) {
            Some(v) if !v.is_null() => {
                m.insert(key.into(), v.clone());
            }
            _ => {}
        }
    }
    m
}

/// Rewrite a tool-leg `tool.cancel` onto the gate's cancel contract
/// (`<gate>.tool.cancel { id }`), the id being the kernel correlation the gate
/// echoes as the forward's outer id.
fn gate_cancel(gate: &str, id: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String(format!("{gate}.tool.cancel")));
    m.insert("id".into(), Value::String(id.to_owned()));
    m
}

/// The provider abort envelope, keyed by the chat handle — the same shape the
/// llm factory's kill handler inlines (`<provider>.chat.cancel { chat_id }`).
fn chat_cancel_envelope(provider: &str, chat_id: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{provider}.chat.cancel")),
    );
    m.insert("chat_id".into(), Value::String(chat_id.to_owned()));
    m
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
    // `reasoning_effort` is the control-plane-resolved profile depth
    // (factories/llm.lua build_request); providers parse it off chat.create.
    for key in [
        "model",
        "system",
        "tools",
        "reasoning_effort",
        "routing_session_id",
    ] {
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

fn compaction_restore_envelope(
    provider: &str,
    chat_id: &str,
    artifact: &Value,
) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{provider}.chat.compaction.restore")),
    );
    m.insert("chat_id".into(), Value::String(chat_id.to_owned()));
    m.insert("model_context_artifact".into(), artifact.clone());
    m
}

fn complete_envelope(
    provider: &str,
    chat_id: &str,
    args: &Map<String, Value>,
) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String(format!("{provider}.chat.complete")),
    );
    m.insert("chat_id".into(), Value::String(chat_id.to_owned()));
    if let Some(descriptor) = args.get("output_schema").filter(|value| !value.is_null()) {
        if let Ok(descriptor) =
            serde_json::from_value::<nefor_mag::schema::TypeSchema>(descriptor.clone())
        {
            m.insert("output_schema".into(), descriptor.to_json_schema());
        }
    }
    m
}

/// The turn messages to append from a provider request. The `llm` factory hands
/// its FULL transcript through as `input.messages` — each round runs on a fresh
/// provider chat, so the whole conversation replays (factories/llm.lua, "The
/// transcript"); the array is role-tagged turn messages. Append each. A bare
/// single message (`{ role, … }`) or a `{ text }` shape are tolerated as
/// fallbacks so a hand-authored request still drives.
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

    const GATE: &str = "tool-gate";

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().expect("object").clone()
    }

    #[test]
    fn provider_invoke_drives_the_chat_conversation_and_records_correlation() {
        let mut bridge = CapabilityBridge::new(GATE);
        let invoke = obj(json!({
            "kind": "tool.invoke",
            "id": "corr-1",
            "name": "chatgpt-provider",
            "args": {
                "chat_id": "agent@r1",
                "model": "opus",
                "system": "be helpful",
                "tools": ["fs/read"],
                "output_schema": {
                    "version": 1,
                    "root": {"kind": "record", "fields": [
                        {"name": "answer", "schema": {"kind": "string"}}
                    ]}
                },
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
        assert_eq!(
            out[2]["output_schema"]["properties"]["answer"]["type"],
            "string"
        );

        // A completion result correlates back to the minted correlation id.
        let result = obj(json!({
            "kind": "chatgpt-provider.chat.complete.result",
            "chat_id": "agent@r1",
            "output": { "text": "done" }
        }));
        assert!(CapabilityBridge::is_provider_reply(
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
    fn provider_invoke_restores_compacted_context_before_appending_new_messages() {
        let mut bridge = CapabilityBridge::new(GATE);
        let artifact = json!({
            "kind": "responses.compaction",
            "items": [{
                "type": "compaction",
                "encrypted_content": "sealed-history"
            }]
        });
        let out = bridge.translate_emit(obj(json!({
            "kind": "tool.invoke",
            "id": "corr-compact",
            "name": "chatgpt-provider",
            "args": {
                "chat_id": "lead.llm@r2",
                "context_artifact": artifact.clone(),
                "input": {
                    "messages": [{
                        "role": "user",
                        "content": "after compaction"
                    }]
                }
            }
        })));

        let kinds: Vec<&str> = out
            .iter()
            .map(|e| e.get("kind").and_then(Value::as_str).unwrap_or(""))
            .collect();
        assert_eq!(
            kinds,
            vec![
                "chatgpt-provider.chat.create",
                "chatgpt-provider.chat.compaction.restore",
                "chatgpt-provider.chat.append",
                "chatgpt-provider.chat.complete",
            ],
            "fresh provider chats restore the native artifact before post-compaction messages"
        );
        assert_eq!(
            out[1].get("chat_id").and_then(Value::as_str),
            Some("lead.llm@r2")
        );
        assert_eq!(
            out[1].get("model_context_artifact"),
            Some(&artifact),
            "the opaque artifact crosses the bridge without interpretation"
        );
        assert_eq!(
            out[2]["message"]["content"], "after compaction",
            "only the post-compaction transcript is appended after restoration"
        );
    }

    #[test]
    fn tool_invoke_is_rewritten_to_the_gate_with_the_payload_unwrapped() {
        let mut bridge = CapabilityBridge::new(GATE);
        // The kernel's double-wrapped request (routing.lua on_capability_invoke
        // forwarding run-tool's { name, args, allowlist, da-policy } verbatim).
        let invoke = obj(json!({
            "kind": "tool.invoke",
            "id": "cap-4",
            "name": "list_dir",
            "args": {
                "name": "list_dir",
                "args": { "path": "." },
                "allowlist": ["list_dir", "read_file"],
                "da-policy": { "git": "read" }
            }
        }));
        let out = bridge.translate_emit(invoke);
        assert_eq!(out.len(), 1, "one invoke in, one gate envelope out");
        let gate = &out[0];
        assert_eq!(
            gate.get("kind").and_then(Value::as_str),
            Some("tool-gate.tool.invoke"),
            "a tool invocation is rewritten onto the gate's prefix"
        );
        // The kernel correlation id is kept as the gate's outer id, so the
        // gate's tool.result correlates straight back to the open request.
        assert_eq!(gate.get("id").and_then(Value::as_str), Some("cap-4"));
        assert_eq!(gate.get("name").and_then(Value::as_str), Some("list_dir"));
        // The inner args are unwrapped to the gate's contract.
        assert_eq!(
            gate.get("args"),
            Some(&json!({ "path": "." })),
            "the double-wrapped payload is unwrapped for the gate"
        );
        // Per-node gating rides at the gate's top level.
        assert_eq!(
            gate.get("allowlist"),
            Some(&json!(["list_dir", "read_file"]))
        );
        assert_eq!(gate.get("da-policy"), Some(&json!({ "git": "read" })));
    }

    #[test]
    fn tool_invoke_preserves_structured_invocation_provenance() {
        let mut bridge = CapabilityBridge::new(GATE);
        let invocation = json!({
            "session_id": "session-1",
            "run_id": "run-1",
            "run_scope": "r7",
            "actor_id": "scout.run-tool",
            "capability_id": "r7/cap-4",
            "principal": "subagent"
        });
        let out = bridge.translate_emit(obj(json!({
            "kind": "tool.invoke",
            "id": "r7/cap-4",
            "from": "scout.run-tool",
            "name": "read_file",
            "args": { "name": "read_file", "args": { "path": "src/lib.rs" } },
            "invocation": invocation
        })));
        let gate = &out[0];
        assert_eq!(gate.get("id"), Some(&json!("r7/cap-4")));
        assert_eq!(gate.get("from"), Some(&json!("scout.run-tool")));
        assert_eq!(gate.get("invocation"), Some(&invocation));
        assert_eq!(gate.get("args"), Some(&json!({ "path": "src/lib.rs" })));
    }

    #[test]
    fn tool_invoke_gate_target_is_config_threaded_not_hardcoded() {
        let mut bridge = CapabilityBridge::new("custom-gate");
        let out = bridge.translate_emit(obj(json!({
            "kind": "tool.invoke",
            "id": "cap-1",
            "name": "echo",
            "args": { "name": "echo", "args": {} }
        })));
        assert_eq!(
            out[0].get("kind").and_then(Value::as_str),
            Some("custom-gate.tool.invoke")
        );
    }

    #[test]
    fn unwrapped_tool_request_is_forwarded_as_the_args_whole() {
        // A hand-authored capability request without the run-tool wrap: the
        // request itself is the tool args.
        let mut bridge = CapabilityBridge::new(GATE);
        let out = bridge.translate_emit(obj(json!({
            "kind": "tool.invoke",
            "id": "cap-2",
            "name": "read_file",
            "args": { "path": "/etc/hosts" }
        })));
        let gate = &out[0];
        assert_eq!(
            gate.get("kind").and_then(Value::as_str),
            Some("tool-gate.tool.invoke")
        );
        assert_eq!(gate.get("name").and_then(Value::as_str), Some("read_file"));
        assert_eq!(gate.get("args"), Some(&json!({ "path": "/etc/hosts" })));
    }

    #[test]
    fn tool_cancel_for_a_tool_correlation_rewrites_to_the_gate() {
        let mut bridge = CapabilityBridge::new(GATE);
        // No provider chat named this id → a tool correlation.
        let out = bridge.translate_emit(obj(json!({
            "kind": "tool.cancel", "id": "r16/cap-2"
        })));
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].get("kind").and_then(Value::as_str),
            Some("tool-gate.tool.cancel")
        );
        assert_eq!(out[0].get("id").and_then(Value::as_str), Some("r16/cap-2"));
    }

    #[test]
    fn tool_cancel_for_a_provider_correlation_becomes_a_chat_cancel() {
        let mut bridge = CapabilityBridge::new(GATE);
        // Drive a provider chat so the correlation id maps to a chat handle.
        bridge.translate_emit(obj(json!({
            "kind": "tool.invoke", "id": "corr-9", "name": "chatgpt-provider",
            "args": { "chat_id": "lead.llm@r1", "input": { "messages": [] } }
        })));
        let out = bridge.translate_emit(obj(json!({
            "kind": "tool.cancel", "id": "corr-9"
        })));
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].get("kind").and_then(Value::as_str),
            Some("chatgpt-provider.chat.cancel"),
            "a provider-round interrupt aborts the driven chat"
        );
        assert_eq!(
            out[0].get("chat_id").and_then(Value::as_str),
            Some("lead.llm@r1")
        );
    }

    #[test]
    fn non_invoke_emit_forwards_unchanged() {
        let mut bridge = CapabilityBridge::new(GATE);
        let event = obj(json!({ "kind": "mag.actor_ready", "id": "sink" }));
        let out = bridge.translate_emit(event.clone());
        assert_eq!(out, vec![event]);
    }

    #[test]
    fn chat_error_resolves_as_an_error_reply() {
        let mut bridge = CapabilityBridge::new(GATE);
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
        let mut bridge = CapabilityBridge::new(GATE);
        let result = obj(json!({
            "kind": "p.chat.complete.result", "chat_id": "not-ours", "output": {}
        }));
        assert!(bridge
            .take_reply("p.chat.complete.result", &result)
            .is_none());
    }

    #[test]
    fn concurrent_chats_stay_independent_and_resolve_in_any_order() {
        let mut bridge = CapabilityBridge::new(GATE);
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
