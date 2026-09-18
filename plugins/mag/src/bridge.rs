// Capability bridge between the kernel's generic capability protocol and the
// canonical provider/tool protocols on the bus.
//
// Provider calls cross MAG as thin conversation-manager commands. The manager
// owns durable conversation state and relays public provider events; MAG never
// serializes a request transcript or provider-specific protocol onto the bus.
// Tool calls retain the existing stateless tool-gate rewrite.

use std::collections::HashMap;

use serde_json::{Map, Value};

const TOOL_INVOKE: &str = "tool.invoke";
const TOOL_CANCEL: &str = "tool.cancel";
const PROVIDER_INVOKE_REQUEST: &str = "conversation.provider.invoke.request";
const PROVIDER_CANCEL_REQUEST: &str = "conversation.provider.cancel.request";
const PROVIDER_EVENT: &str = "conversation.provider.event";
const CONVERSATION_MANAGER: &str = "conversation-manager";

struct PendingProvider {
    provider: String,
    structured_output: bool,
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
        let args = body
            .get("args")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut request = Map::new();
        request.insert("kind".into(), Value::String(PROVIDER_INVOKE_REQUEST.into()));
        request.insert("provider".into(), Value::String(provider.clone()));
        request.insert("request_id".into(), Value::String(request_id.clone()));
        for key in [
            "conversation_id",
            "model",
            "reasoning_effort",
            "provider_options",
            "tools",
            "output_schema",
            "tool_specs",
        ] {
            if let Some(value) = args.get(key).filter(|value| !value.is_null()) {
                request.insert(key.into(), value.clone());
            }
        }
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
            out.insert("kind".into(), Value::String(PROVIDER_CANCEL_REQUEST.into()));
            out.insert("provider".into(), Value::String(pending.provider));
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
        source: &str,
        kind: &str,
        body: &'a Map<String, Value>,
    ) -> Option<&'a str> {
        let request_id = body.get("request_id").and_then(Value::as_str)?;
        let pending = self.pending_providers.get(request_id)?;
        let provider_matches =
            body.get("provider").and_then(Value::as_str) == Some(pending.provider.as_str());
        (source == CONVERSATION_MANAGER && kind == PROVIDER_EVENT && provider_matches)
            .then_some(request_id)
    }

    pub fn is_structured_request(&self, request_id: &str) -> bool {
        self.pending_providers
            .get(request_id)
            .is_some_and(|pending| pending.structured_output)
    }

    /// Record non-terminal provider data or consume a terminal event. Unknown,
    /// late, and cross-provider events are ignored.
    pub fn take_reply(&mut self, kind: &str, body: &Map<String, Value>) -> Option<ProviderReply> {
        let request_id = self
            .provider_request_id(CONVERSATION_MANAGER, kind, body)?
            .to_owned();
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
    body.get("class").and_then(Value::as_str) == Some("provider")
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
    for key in [
        "text",
        "reasoning",
        "finish_reason",
        "tool_calls",
        "provider_context",
    ] {
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
