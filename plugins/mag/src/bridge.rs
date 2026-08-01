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
    tool_calls: Vec<Value>,
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
    gate: String,
}

impl CapabilityBridge {
    pub fn new(gate: impl Into<String>) -> Self {
        Self {
            pending_providers: HashMap::new(),
            gate: gate.into(),
        }
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
        request.remove("chat_id");
        request.insert(
            "kind".into(),
            Value::String(format!("{provider}.completion.request")),
        );
        request.insert("request_id".into(), Value::String(request_id.clone()));

        self.pending_providers.insert(
            request_id,
            PendingProvider {
                provider,
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
        if !matches!(event, "completed" | "result" | "failed" | "error") {
            return None;
        }

        let pending = self.pending_providers.remove(&request_id)?;
        if matches!(event, "failed" | "error") {
            let error = body
                .get("error")
                .or_else(|| body.get("message"))
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_else(|| "provider completion error".to_owned());
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
                "chat_id": format!("{request_id}-legacy-handle"),
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

    #[test]
    fn provider_invoke_emits_one_request_with_full_history_once() {
        let mut bridge = CapabilityBridge::new("tool-gate");
        let messages = json!([
            {"role": "user", "content": "first"},
            {"role": "assistant", "content": "second"},
            {"role": "user", "content": "third"}
        ]);
        let out = bridge.translate_emit(provider_invoke("req-1", "provider-a", messages.clone()));

        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["kind"], "provider-a.completion.request");
        assert_eq!(out[0]["request_id"], "req-1");
        assert_eq!(out[0]["messages"], messages);
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
