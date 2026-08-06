//! Capability bridge between the kernel's generic capability protocol and the
//! canonical provider/tool protocols on the bus.
//!
//! Provider calls are single-shot: one `<provider>.completion.request` carries
//! the complete request and one kernel correlation id. Provider events carry
//! that same `request_id`; only terminal events settle the kernel capability.
//! Tool calls retain the existing stateless tool-gate rewrite.

use std::collections::HashMap;

use serde_json::{Map, Value};

const TOOL_INVOKE: &str = "tool.invoke";
const TOOL_CANCEL: &str = "tool.cancel";
const PROVIDER_EVENT: &str = "completion.event";

struct PendingProvider {
    provider: String,
    structured_output: bool,
    tool_calls: Vec<Value>,
}

#[derive(Debug, Clone)]
struct AdvertisedTool {
    owner: String,
    spec: Value,
}

/// A terminal provider response ready for `kernel.bus_response`.
pub struct ProviderReply {
    pub request_id: String,
    pub result: Option<Value>,
    pub error: Option<String>,
}

/// Provider request ownership keyed by canonical correlation id, plus the
/// composition-owned tool-gate target.
pub struct CapabilityBridge {
    pending_providers: HashMap<String, PendingProvider>,
    advertised_tools: Option<HashMap<String, AdvertisedTool>>,
    gate: String,
}

impl CapabilityBridge {
    pub fn new(gate: impl Into<String>) -> Self {
        Self {
            pending_providers: HashMap::new(),
            advertised_tools: None,
            gate: gate.into(),
        }
    }

    /// Replace the runtime tool snapshot from the composition's canonical gate.
    ///
    /// The gate has already rejected malformed schemas and owner collisions.
    /// MAG therefore treats its flattened `tool.register` event as immutable
    /// descriptor data for subsequent provider requests, rather than asking
    /// each provider to independently resolve an authored name list later.
    pub fn observe_tool_register(&mut self, source: &str, body: &Map<String, Value>) -> bool {
        if source != self.gate || body.get("kind").and_then(Value::as_str) != Some("tool.register")
        {
            return false;
        }

        let Some(tools) = body.get("tools").and_then(Value::as_array) else {
            return false;
        };
        let mut snapshot = HashMap::with_capacity(tools.len());
        for spec in tools {
            let Some(name) = spec.get("name").and_then(Value::as_str) else {
                return false;
            };
            let Some(owner) = spec.get("owner").and_then(Value::as_str) else {
                return false;
            };
            if name.is_empty() || owner.is_empty() || snapshot.contains_key(name) {
                return false;
            }
            snapshot.insert(
                name.to_owned(),
                AdvertisedTool {
                    owner: owner.to_owned(),
                    spec: spec.clone(),
                },
            );
        }
        self.advertised_tools = Some(snapshot);
        true
    }

    pub fn translate_emit(&mut self, body: Map<String, Value>) -> Vec<Map<String, Value>> {
        match body.get("kind").and_then(Value::as_str) {
            Some(TOOL_CANCEL) => self.translate_cancel(&body),
            Some(TOOL_INVOKE) if is_provider_invoke(&body) => self.translate_provider_invoke(body),
            Some(TOOL_INVOKE) => vec![gate_invoke(&self.gate, &body)],
            _ => vec![body],
        }
    }

    fn translate_provider_invoke(&mut self, body: Map<String, Value>) -> Vec<Map<String, Value>> {
        let Some(request_id) = body.get("id").and_then(Value::as_str) else {
            return vec![body];
        };
        let Some(provider) = body.get("name").and_then(Value::as_str) else {
            return vec![body];
        };
        let request_id = request_id.to_owned();
        let provider = provider.to_owned();
        let mut request = body
            .get("args")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        // The kernel's ProviderInput wrapper is an internal activation shape;
        // the provider protocol owns one canonical top-level message history.
        if let Some(input) = request.remove("input") {
            if let Some(messages) = input
                .as_object()
                .and_then(|input| input.get("messages"))
                .cloned()
            {
                request.insert("messages".into(), messages);
            }
        }
        if let (Some(catalog), Some(names)) = (
            self.advertised_tools.as_ref(),
            request.get("tools").and_then(Value::as_array),
        ) {
            let requested = names.iter().filter_map(Value::as_str).collect::<Vec<_>>();
            if requested.len() == names.len() {
                let mut specs = Vec::with_capacity(requested.len());
                for name in requested {
                    if let Some(tool) = catalog.get(name) {
                        debug_assert!(!tool.owner.is_empty());
                        specs.push(tool.spec.clone());
                    }
                }
                request.insert("tool_specs".into(), Value::Array(specs));
            }
        }
        request.insert(
            "kind".into(),
            Value::String(format!("{provider}.completion.request")),
        );
        request.insert("request_id".into(), Value::String(request_id.clone()));
        if let Some(invocation @ Value::Object(_)) = body.get("invocation") {
            request.insert("invocation".into(), invocation.clone());
        }

        let structured_output = request
            .get("output_schema")
            .is_some_and(|value| !value.is_null());
        self.pending_providers.insert(
            request_id,
            PendingProvider {
                provider,
                structured_output,
                tool_calls: Vec::new(),
            },
        );
        vec![request]
    }

    fn translate_cancel(&mut self, body: &Map<String, Value>) -> Vec<Map<String, Value>> {
        let Some(request_id) = body.get("id").and_then(Value::as_str) else {
            return Vec::new();
        };
        if let Some(pending) = self.pending_providers.remove(request_id) {
            let mut out = Map::new();
            out.insert(
                "kind".into(),
                Value::String(format!("{}.completion.cancel", pending.provider)),
            );
            out.insert("request_id".into(), Value::String(request_id.to_owned()));
            return vec![out];
        }
        vec![gate_cancel(&self.gate, request_id)]
    }

    /// Identify an event belonging to an open provider request. This uses the
    /// exact event channel derived from request ownership, not provider-specific
    /// event suffixes.
    pub fn provider_request_id<'a>(
        &'a self,
        kind: &str,
        body: &'a Map<String, Value>,
    ) -> Option<&'a str> {
        let request_id = body.get("request_id").and_then(Value::as_str)?;
        let pending = self.pending_providers.get(request_id)?;
        (kind == format!("{}.{}", pending.provider, PROVIDER_EVENT)).then_some(request_id)
    }

    pub fn is_structured_request(&self, request_id: &str) -> bool {
        self.pending_providers
            .get(request_id)
            .is_some_and(|pending| pending.structured_output)
    }

    /// Record non-terminal provider data or consume a terminal event. Unknown,
    /// late, and cross-provider events are ignored.
    pub fn take_reply(&mut self, kind: &str, body: &Map<String, Value>) -> Option<ProviderReply> {
        let request_id = self.provider_request_id(kind, body)?.to_owned();
        let event = body.get("event").and_then(Value::as_str)?;

        if event == "tool_call" {
            let call = provider_tool_call(body);
            self.pending_providers
                .get_mut(&request_id)?
                .tool_calls
                .push(call);
            return None;
        }
        if !matches!(
            event,
            "completed" | "result" | "failed" | "error" | "interrupted"
        ) {
            return None;
        }

        let pending = self.pending_providers.remove(&request_id)?;
        if matches!(event, "failed" | "error" | "interrupted") {
            let error = body
                .get("error")
                .or_else(|| body.get("message"))
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_else(|| {
                    if event == "interrupted" {
                        "provider completion interrupted".to_owned()
                    } else {
                        "provider completion error".to_owned()
                    }
                });
            return Some(ProviderReply {
                request_id,
                result: None,
                error: Some(error),
            });
        }

        let mut result = body
            .get("result")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_else(|| provider_result(body));
        if !pending.tool_calls.is_empty() && !result.contains_key("tool_calls") {
            result.insert("tool_calls".into(), Value::Array(pending.tool_calls));
        }
        Some(ProviderReply {
            request_id,
            result: Some(Value::Object(result)),
            error: None,
        })
    }
}

fn is_provider_invoke(body: &Map<String, Value>) -> bool {
    let Some(args) = body.get("args").and_then(Value::as_object) else {
        return false;
    };
    args.get("messages").and_then(Value::as_array).is_some()
        || args
            .get("input")
            .and_then(Value::as_object)
            .and_then(|input| input.get("messages"))
            .and_then(Value::as_array)
            .is_some()
}

fn provider_tool_call(body: &Map<String, Value>) -> Value {
    let mut call = Map::new();
    for key in ["id", "name", "arguments"] {
        if let Some(value) = body.get(key) {
            call.insert(key.into(), value.clone());
        }
    }
    Value::Object(call)
}

fn provider_result(body: &Map<String, Value>) -> Map<String, Value> {
    let mut result = Map::new();
    for key in ["text", "reasoning", "finish_reason", "tool_calls"] {
        if let Some(value) = body.get(key) {
            result.insert(key.into(), value.clone());
        }
    }
    result
}

/// Preserve the existing tool-gate invocation translation.
fn gate_invoke(gate: &str, body: &Map<String, Value>) -> Map<String, Value> {
    let request = body
        .get("args")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut out = Map::new();
    out.insert("kind".into(), Value::String(format!("{gate}.tool.invoke")));
    if let Some(id) = body.get("id").and_then(Value::as_str) {
        out.insert("id".into(), Value::String(id.to_owned()));
    }
    if let Some(from) = body.get("from").and_then(Value::as_str) {
        out.insert("from".into(), Value::String(from.to_owned()));
    }
    if let Some(invocation @ Value::Object(_)) = body.get("invocation") {
        out.insert("invocation".into(), invocation.clone());
    }
    if let Some(name) = body
        .get("name")
        .or_else(|| request.get("name"))
        .and_then(Value::as_str)
    {
        out.insert("name".into(), Value::String(name.to_owned()));
    }
    let args = match request.get("args") {
        Some(inner @ Value::Object(_)) => inner.clone(),
        _ => Value::Object(request.clone()),
    };
    out.insert("args".into(), args);
    for key in ["allowlist", "da-policy"] {
        if let Some(value) = request.get(key).filter(|value| !value.is_null()) {
            out.insert(key.into(), value.clone());
        }
    }
    out
}

fn gate_cancel(gate: &str, id: &str) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("kind".into(), Value::String(format!("{gate}.tool.cancel")));
    out.insert("id".into(), Value::String(id.to_owned()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(value: Value) -> Map<String, Value> {
        value.as_object().expect("object").clone()
    }

    fn provider_invoke(request_id: &str, provider: &str, messages: Value) -> Map<String, Value> {
        obj(json!({
            "kind": "tool.invoke",
            "id": request_id,
            "name": provider,
            "args": {
                "conversation_id": "conversation-stable",
                "model": "opus",
                "system": "be helpful",
                "tools": [{"name": "read_file"}],
                "input": {"messages": messages}
            }
        }))
    }

    fn event(request_id: &str, provider: &str, name: &str, fields: Value) -> Map<String, Value> {
        let mut body = fields.as_object().expect("fields").clone();
        body.insert(
            "kind".into(),
            Value::String(format!("{provider}.completion.event")),
        );
        body.insert("request_id".into(), Value::String(request_id.into()));
        body.insert("event".into(), Value::String(name.into()));
        body
    }

    fn register(tools: Value) -> Map<String, Value> {
        obj(json!({"kind": "tool.register", "tools": tools}))
    }

    #[test]
    fn provider_requests_project_authored_names_through_the_gate_snapshot() {
        let mut bridge = CapabilityBridge::new("tool-gate");
        let snapshot = register(json!([
            {
                "name": "read_file",
                "owner": "basic-tools",
                "description": "Read a file.",
                "parameters": {"type": "object"},
                "display": {"label": "Read", "result": {"kind": "content"}}
            },
            {
                "name": "instructions",
                "owner": "read-only-tools",
                "description": "Load instructions.",
                "parameters": {"type": "object"},
                "display": {"label": "Instructions", "result": {"kind": "content"}}
            }
        ]));
        assert!(bridge.observe_tool_register("tool-gate", &snapshot));

        let mut invoke = provider_invoke("req-projected", "provider-a", json!([]));
        invoke
            .get_mut("args")
            .and_then(Value::as_object_mut)
            .unwrap()
            .insert(
                "tools".into(),
                json!(["read_file", "python-read", "instructions"]),
            );
        let out = bridge.translate_emit(invoke);

        assert_eq!(
            out[0]["tools"],
            json!(["read_file", "python-read", "instructions"]),
            "the authored list remains the immutable invocation allowlist"
        );
        assert_eq!(out[0]["tool_specs"].as_array().unwrap().len(), 2);
        assert_eq!(out[0]["tool_specs"][0]["name"], "read_file");
        assert_eq!(out[0]["tool_specs"][0]["owner"], "basic-tools");
        assert_eq!(out[0]["tool_specs"][1]["name"], "instructions");
    }

    #[test]
    fn only_the_named_gate_can_replace_the_tool_snapshot() {
        let mut bridge = CapabilityBridge::new("custom-gate");
        let foreign = register(json!([{
            "name": "stale", "owner": "foreign", "description": "",
            "parameters": {}, "display": {"label": "Stale", "result": {"kind": "content"}}
        }]));
        assert!(!bridge.observe_tool_register("tool-gate", &foreign));

        let canonical = register(json!([{
            "name": "read_file", "owner": "basic-tools", "description": "",
            "parameters": {}, "display": {"label": "Read", "result": {"kind": "content"}}
        }]));
        assert!(bridge.observe_tool_register("custom-gate", &canonical));
        let replacement = register(json!([]));
        assert!(bridge.observe_tool_register("custom-gate", &replacement));

        let mut invoke = provider_invoke("req-empty", "provider-a", json!([]));
        invoke
            .get_mut("args")
            .and_then(Value::as_object_mut)
            .unwrap()
            .insert("tools".into(), json!(["read_file"]));
        let out = bridge.translate_emit(invoke);
        assert_eq!(out[0]["tool_specs"], json!([]));
    }

    #[test]
    fn provider_fallback_remains_available_before_the_first_gate_snapshot() {
        let mut bridge = CapabilityBridge::new("tool-gate");
        let mut invoke = provider_invoke("req-legacy", "provider-a", json!([]));
        invoke
            .get_mut("args")
            .and_then(Value::as_object_mut)
            .unwrap()
            .insert("tools".into(), json!(["read_file"]));
        let out = bridge.translate_emit(invoke);
        assert!(out[0].get("tool_specs").is_none());
    }

    #[test]
    fn provider_invoke_emits_one_request_with_full_history_once() {
        let mut bridge = CapabilityBridge::new("tool-gate");
        let messages = json!([
            {"role": "user", "content": "first"},
            {"role": "assistant", "content": "second"},
            {"role": "user", "content": "third"}
        ]);
        let mut invoke = provider_invoke("req-1", "provider-a", messages.clone());
        let invocation = json!({"run_id": "run-1", "actor_id": "worker"});
        invoke.insert("invocation".into(), invocation.clone());
        let out = bridge.translate_emit(invoke);

        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["kind"], "provider-a.completion.request");
        assert_eq!(out[0]["request_id"], "req-1");
        assert_eq!(out[0]["conversation_id"], "conversation-stable");
        assert_eq!(out[0]["messages"], messages);
        assert_eq!(out[0]["invocation"], invocation);
        assert!(out[0].get("input").is_none());
        assert!(out[0].get("chat_id").is_none());
        let wire = serde_json::to_string(&out).expect("serialize");
        assert_eq!(wire.matches("first").count(), 1);
        assert!(!wire.contains(".chat."));
        assert!(!wire.contains("history"));
    }

    #[test]
    fn provider_results_correlate_out_of_order() {
        let mut bridge = CapabilityBridge::new("tool-gate");
        bridge.translate_emit(provider_invoke("req-a", "provider-a", json!([])));
        bridge.translate_emit(provider_invoke("req-b", "provider-a", json!([])));
        let b = event("req-b", "provider-a", "completed", json!({"text": "B"}));
        let a = event("req-a", "provider-a", "completed", json!({"text": "A"}));
        assert_eq!(
            bridge
                .take_reply(b["kind"].as_str().unwrap(), &b)
                .unwrap()
                .request_id,
            "req-b"
        );
        assert_eq!(
            bridge
                .take_reply(a["kind"].as_str().unwrap(), &a)
                .unwrap()
                .request_id,
            "req-a"
        );
    }

    #[test]
    fn cancellation_uses_request_id_and_late_output_is_ignored() {
        let mut bridge = CapabilityBridge::new("tool-gate");
        bridge.translate_emit(provider_invoke("req-1", "provider-a", json!([])));
        let cancel = bridge.translate_emit(obj(json!({"kind": "tool.cancel", "id": "req-1"})));
        assert_eq!(
            cancel,
            vec![obj(json!({
                "kind": "provider-a.completion.cancel", "request_id": "req-1"
            }))]
        );
        let late = event("req-1", "provider-a", "completed", json!({"text": "late"}));
        assert!(bridge
            .take_reply(late["kind"].as_str().unwrap(), &late)
            .is_none());
    }

    #[test]
    fn canonical_events_are_self_correlated_and_only_terminal_settles() {
        let mut bridge = CapabilityBridge::new("tool-gate");
        bridge.translate_emit(provider_invoke("req-1", "provider-a", json!([])));
        for name in ["text_delta", "reasoning_delta", "retry", "usage"] {
            let body = event("req-1", "provider-a", name, json!({"text": "chunk"}));
            let kind = body["kind"].as_str().unwrap();
            assert_eq!(bridge.provider_request_id(kind, &body), Some("req-1"));
            assert!(bridge.take_reply(kind, &body).is_none());
        }
        let tool = event(
            "req-1",
            "provider-a",
            "tool_call",
            json!({
                "id": "call-1", "name": "read_file", "arguments": {"path": "x"}
            }),
        );
        assert!(bridge
            .take_reply(tool["kind"].as_str().unwrap(), &tool)
            .is_none());
        let done = event("req-1", "provider-a", "completed", json!({"text": ""}));
        let reply = bridge
            .take_reply(done["kind"].as_str().unwrap(), &done)
            .unwrap();
        assert_eq!(reply.result.unwrap()["tool_calls"][0]["name"], "read_file");
    }

    #[test]
    fn error_event_settles_as_kernel_error_and_unknown_is_ignored() {
        let mut bridge = CapabilityBridge::new("tool-gate");
        bridge.translate_emit(provider_invoke("req-1", "provider-a", json!([])));
        let wrong = event("req-1", "provider-b", "completed", json!({"text": "wrong"}));
        assert!(bridge
            .take_reply(wrong["kind"].as_str().unwrap(), &wrong)
            .is_none());
        let failed = event(
            "req-1",
            "provider-a",
            "error",
            json!({"message": "overloaded"}),
        );
        let reply = bridge
            .take_reply(failed["kind"].as_str().unwrap(), &failed)
            .unwrap();
        assert_eq!(reply.error.as_deref(), Some("overloaded"));
        assert!(reply.result.is_none());
    }

    #[test]
    fn canonical_nested_result_settles_text_and_tool_calls_once() {
        let mut bridge = CapabilityBridge::new("tool-gate");
        bridge.translate_emit(provider_invoke("req-result", "chatgpt", json!([])));
        let terminal = event(
            "req-result",
            "chatgpt",
            "completed",
            json!({"result": {
                "text": "typed answer",
                "finish_reason": "tool_calls",
                "tool_calls": [{
                    "id": "call-1", "name": "read_file", "arguments": {"path": "x"}
                }]
            }}),
        );
        let result = bridge
            .take_reply(terminal["kind"].as_str().unwrap(), &terminal)
            .expect("nested output settles")
            .result
            .expect("successful result");
        assert_eq!(result["text"], "typed answer");
        assert_eq!(result["finish_reason"], "tool_calls");
        assert_eq!(result["tool_calls"][0]["name"], "read_file");
        assert!(bridge
            .take_reply(terminal["kind"].as_str().unwrap(), &terminal)
            .is_none());
    }

    #[test]
    fn canonical_nested_result_and_top_level_shapes_settle_once() {
        let mut bridge = CapabilityBridge::new("tool-gate");
        bridge.translate_emit(provider_invoke("req-result", "provider-a", json!([])));
        let result = event(
            "req-result",
            "provider-a",
            "completed",
            json!({"result": {"text": "nested result", "finish_reason": "stop"}}),
        );
        assert_eq!(
            bridge
                .take_reply(result["kind"].as_str().unwrap(), &result)
                .unwrap()
                .result
                .unwrap(),
            json!({"text": "nested result", "finish_reason": "stop"})
        );

        bridge.translate_emit(provider_invoke("req-top", "provider-b", json!([])));
        let top = event(
            "req-top",
            "provider-b",
            "completed",
            json!({"text": "top-level", "finish_reason": "stop"}),
        );
        assert_eq!(
            bridge
                .take_reply(top["kind"].as_str().unwrap(), &top)
                .unwrap()
                .result
                .unwrap(),
            json!({"text": "top-level", "finish_reason": "stop"})
        );
    }

    #[test]
    fn terminal_tool_calls_do_not_duplicate_aggregated_stream_calls() {
        let mut bridge = CapabilityBridge::new("tool-gate");
        bridge.translate_emit(provider_invoke("req-tools", "provider-a", json!([])));
        let split = event(
            "req-tools",
            "provider-a",
            "tool_call",
            json!({"id": "call-1", "name": "read_file", "arguments": {"path": "x"}}),
        );
        assert!(bridge
            .take_reply(split["kind"].as_str().unwrap(), &split)
            .is_none());
        let done = event(
            "req-tools",
            "provider-a",
            "completed",
            json!({"result": {"finish_reason": "tool_calls", "tool_calls": [{
                "id": "call-1", "name": "read_file", "arguments": {"path": "x"}
            }]}}),
        );
        let result = bridge
            .take_reply(done["kind"].as_str().unwrap(), &done)
            .unwrap()
            .result
            .unwrap();
        assert_eq!(result["tool_calls"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn tool_gate_translation_is_unchanged() {
        let mut bridge = CapabilityBridge::new("custom-gate");
        let invocation = json!({"run_id": "run-1", "actor_id": "worker"});
        let out = bridge.translate_emit(obj(json!({
            "kind": "tool.invoke", "id": "cap-1", "from": "worker",
            "name": "read_file", "invocation": invocation,
            "args": {"name": "read_file", "args": {"path": "src/lib.rs"},
                     "allowlist": ["read_file"], "da-policy": {"git": "read"}}
        })));
        assert_eq!(
            out,
            vec![obj(json!({
                "kind": "custom-gate.tool.invoke", "id": "cap-1", "from": "worker",
                "name": "read_file", "invocation": invocation,
                "args": {"path": "src/lib.rs"}, "allowlist": ["read_file"],
                "da-policy": {"git": "read"}
            }))]
        );
        let cancel = bridge.translate_emit(obj(json!({"kind": "tool.cancel", "id": "cap-1"})));
        assert_eq!(cancel[0]["kind"], "custom-gate.tool.cancel");
    }
}
