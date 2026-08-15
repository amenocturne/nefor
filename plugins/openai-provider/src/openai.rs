//! OpenAI-compatible chat-completions request/response shapes and SSE
//! parser.
//!
//! Wire shape notes:
//!
//! - Request body is the standard `{model, messages, stream}` object. We
//!   don't expose temperature/top-p in v1 — Ollama defaults are fine and
//!   each provider accepts a different superset of fields.
//! - Streaming responses come back as Server-Sent Events: each frame is a
//!   `data: {…}\n\n` block, terminated by `data: [DONE]`. Each JSON frame
//!   carries one `choices[0].delta.content` chunk. The final frame
//!   (before `[DONE]`) typically carries `finish_reason` and may carry
//!   `usage` (Ollama does include it; OpenAI requires
//!   `stream_options.include_usage`).
//! - Tool-calling responses interleave `choices[0].delta.tool_calls[*]`
//!   chunks: the first chunk per tool-call carries `function.name` + `id`,
//!   subsequent chunks carry incremental `function.arguments` string
//!   fragments. The terminating chunk's `finish_reason` is `"tool_calls"`.

use serde::{Deserialize, Serialize};

/// One assistant tool call as the model returned it. Used both in the
/// outgoing assistant message (when feeding the model's prior call back
/// in to a follow-up turn) and as the assembled output of the SSE
/// accumulator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    /// Always `"function"` in v1 — OpenAI's only tool type today. Hard-
    /// coded on the wire so the field shape matches the API exactly.
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ToolCallFunction,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    /// Argument JSON as a single string. Provider-bound serialization
    /// preserves valid JSON verbatim and wraps malformed model output as
    /// a JSON string, so every request satisfies the OpenAI wire contract.
    pub arguments: String,
}

impl Serialize for ToolCallFunction {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;

        let mut state = serializer.serialize_struct("ToolCallFunction", 2)?;
        state.serialize_field("name", &self.name)?;
        state.serialize_field("arguments", &provider_arguments(&self.arguments))?;
        state.end()
    }
}

/// Return an OpenAI-compatible `function.arguments` string without double
/// encoding already-valid JSON. Malformed source remains available to the
/// model as the value of a JSON string.
pub fn provider_arguments(arguments: &str) -> String {
    if serde_json::from_str::<serde_json::Value>(arguments).is_ok() {
        arguments.to_owned()
    } else {
        serde_json::to_string(arguments).unwrap_or_else(|_| "null".to_owned())
    }
}

/// Single chat message in the conversation.
///
/// The OpenAI chat schema has four roles, each with a different field
/// set. This enum encodes that shape directly so invalid combinations
/// (e.g. a user message with tool_calls) are unrepresentable.
///
/// Serde shape: internally tagged on `"role"`, variant names lowercased
/// to match the wire (`{"role": "user", "content": "…"}`).
///
/// Wire serialization quirk: Ollama's `/api/chat` validator rejects
/// `{"role": "assistant", "content": null, "tool_calls": [...]}`
/// with `invalid message content type: <nil>`. The OpenAI spec says
/// null is correct on a tool-calls-only assistant turn, but Ollama's
/// JSON unmarshal trips before reaching the spec-defined branch. We
/// `skip_serializing_if = Option::is_none` on the assistant's content
/// so the field is omitted entirely on that shape — both OpenAI and
/// Ollama accept the missing-field form.
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
    },
}

impl Message {
    // --- convenience accessors (minimise diff at call sites) -----------

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

    // --- factory methods (associated functions) ------------------------

    pub fn user<S: Into<String>>(text: S) -> Self {
        Message::User {
            content: text.into(),
        }
    }

    pub fn system<S: Into<String>>(text: S) -> Self {
        Message::System {
            content: text.into(),
        }
    }

    pub fn assistant<S: Into<String>>(text: S) -> Self {
        Message::Assistant {
            content: Some(text.into()),
            tool_calls: Vec::new(),
        }
    }

    /// Assistant message that only emitted tool calls (no prose). The
    /// OpenAI API requires `content: null` rather than `""` on this
    /// shape.
    pub fn assistant_tool_calls(tool_calls: Vec<ToolCall>) -> Self {
        Message::Assistant {
            content: None,
            tool_calls,
        }
    }

    /// Assistant message that combined prose + tool calls. Used when the
    /// model interleaves text deltas with tool-call deltas in the same
    /// turn.
    pub fn assistant_with_tool_calls<S: Into<String>>(text: S, tool_calls: Vec<ToolCall>) -> Self {
        Message::Assistant {
            content: Some(text.into()),
            tool_calls,
        }
    }

    /// Tool result message. `content` carries either the tool's output
    /// string OR an error message (the model doesn't distinguish on the
    /// wire — both are just "what the tool said"). `tool_call_id` MUST
    /// match the corresponding assistant tool_calls entry's `id`.
    pub fn tool_result<S: Into<String>>(tool_call_id: String, content: S) -> Self {
        Message::Tool {
            content: content.into(),
            tool_call_id,
        }
    }
}

/// Body of a streaming chat-completions request.
#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest<'a> {
    pub model: &'a str,
    pub messages: &'a [Message],
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<&'a str>,
    /// `{"include_usage": true}` so the final frame carries `usage`. Ollama
    /// includes it unconditionally; OpenAI honours this opt-in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    /// Tool catalog in OpenAI format — each entry is
    /// `{"type":"function","function":{"name":..,"description":..,"parameters":..}}`.
    /// Skip-serialized when empty so the request shape stays identical
    /// to v1 when no tool plugins are attached.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<&'a [serde_json::Value]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<&'a serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

/// One semantic item carried by an SSE data frame. A frame may contain
/// several items; `Batch` preserves their deterministic wire-semantic order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseEvent {
    Delta(String),
    Refusal(String),
    ReasoningDelta(String),
    ToolCallFragment {
        index: usize,
        id: Option<String>,
        kind: Option<String>,
        name: Option<String>,
        arguments: Option<String>,
    },
    Finish(FinishReason),
    Usage(Usage),
    Error {
        message: String,
        kind: Option<String>,
        code: Option<String>,
    },
    Malformed(String),
    Done,
    Batch(Vec<SseEvent>),
    Empty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    FunctionCall,
}

impl FinishReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Length => "length",
            Self::ToolCalls => "tool_calls",
            Self::ContentFilter => "content_filter",
            Self::FunctionCall => "function_call",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "stop" => Some(Self::Stop),
            "length" => Some(Self::Length),
            "tool_calls" => Some(Self::ToolCalls),
            "content_filter" => Some(Self::ContentFilter),
            "function_call" => Some(Self::FunctionCall),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

/// Parsed model info from `/v1/models`.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelInfo {
    pub id: String,
    /// Context window size if the backend reports it (vLLM, LiteLLM, etc.).
    pub context_window: Option<u64>,
    pub reasoning_efforts: Vec<String>,
    pub default_reasoning_effort: Option<String>,
}

/// Parse the `data` array from a `GET /v1/models` response into a sorted
/// alphabetical list of model info. Skips entries without a string `id`.
/// Opportunistically extracts context window from common extension fields
/// (`max_model_len`, `max_input_tokens`, `context_length`, `context_window`).
pub fn parse_models_response(payload: &str) -> Vec<ModelInfo> {
    let value: serde_json::Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let Some(arr) = value.get("data").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut models: Vec<ModelInfo> = arr
        .iter()
        .filter_map(|m| {
            let id = m.get("id").and_then(|v| v.as_str())?.to_owned();
            let context_window = m
                .get("max_model_len")
                .or_else(|| m.get("max_input_tokens"))
                .or_else(|| m.get("context_length"))
                .or_else(|| m.get("context_window"))
                .and_then(|v| v.as_u64());
            let reasoning_efforts = parse_string_array(
                m.get("reasoning_efforts")
                    .or_else(|| m.get("supported_reasoning_efforts"))
                    .or_else(|| m.get("reasoning_effort_levels"))
                    .or_else(|| m.get("thinking_levels")),
            );
            let default_reasoning_effort = m
                .get("default_reasoning_effort")
                .or_else(|| m.get("default_thinking_level"))
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            Some(ModelInfo {
                id,
                context_window,
                reasoning_efforts,
                default_reasoning_effort,
            })
        })
        .collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    models
}

fn parse_string_array(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Parse one SSE `data:` payload without discarding co-occurring fields.
/// Semantic items are ordered content, refusal, reasoning, tool-call fragments
/// in array order, finish reason, then usage.
pub fn parse_sse_chunk(payload: &str) -> SseEvent {
    let payload = payload.trim();
    if payload.is_empty() {
        return SseEvent::Empty;
    }
    if payload == "[DONE]" {
        return SseEvent::Done;
    }
    let value: serde_json::Value = match serde_json::from_str(payload) {
        Ok(value) => value,
        Err(error) => return SseEvent::Malformed(error.to_string()),
    };

    if let Some(error) = value.get("error").and_then(|value| value.as_object()) {
        let message = error
            .get("message")
            .and_then(|value| value.as_str())
            .unwrap_or("provider returned an error")
            .to_owned();
        let kind = error
            .get("type")
            .and_then(|value| value.as_str())
            .map(str::to_owned);
        let code = error
            .get("code")
            .and_then(|value| value.as_str())
            .map(str::to_owned);
        return SseEvent::Error {
            message,
            kind,
            code,
        };
    }

    let first_choice = value
        .get("choices")
        .and_then(|choices| choices.as_array())
        .and_then(|choices| choices.first());
    let delta = first_choice.and_then(|choice| choice.get("delta"));
    let mut events = Vec::new();

    if let Some(content) = delta
        .and_then(|delta| delta.get("content"))
        .and_then(|v| v.as_str())
    {
        if !content.is_empty() {
            events.push(SseEvent::Delta(content.to_owned()));
        }
    }
    if let Some(refusal) = delta
        .and_then(|delta| delta.get("refusal"))
        .and_then(|v| v.as_str())
    {
        if !refusal.is_empty() {
            events.push(SseEvent::Refusal(refusal.to_owned()));
        }
    }
    if let Some(reasoning) = delta
        .and_then(|delta| delta.get("reasoning"))
        .and_then(|v| v.as_str())
    {
        if !reasoning.is_empty() {
            events.push(SseEvent::ReasoningDelta(reasoning.to_owned()));
        }
    }
    if let Some(tool_calls) = delta
        .and_then(|delta| delta.get("tool_calls"))
        .and_then(|value| value.as_array())
    {
        for call in tool_calls {
            let Some(index) = call.get("index").and_then(|value| value.as_u64()) else {
                return SseEvent::Malformed("tool-call chunk is missing required index".to_owned());
            };
            let function = call.get("function");
            events.push(SseEvent::ToolCallFragment {
                index: index as usize,
                id: call.get("id").and_then(|v| v.as_str()).map(str::to_owned),
                kind: call.get("type").and_then(|v| v.as_str()).map(str::to_owned),
                name: function
                    .and_then(|v| v.get("name"))
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
                arguments: function
                    .and_then(|v| v.get("arguments"))
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
            });
        }
    }
    if let Some(reason) = first_choice
        .and_then(|choice| choice.get("finish_reason"))
        .and_then(|value| value.as_str())
    {
        let Some(reason) = FinishReason::parse(reason) else {
            return SseEvent::Malformed(format!("unknown finish_reason {reason:?}"));
        };
        events.push(SseEvent::Finish(reason));
    }
    if let Some(usage) = value
        .get("usage")
        .and_then(|usage| serde_json::from_value::<Usage>(usage.clone()).ok())
    {
        events.push(SseEvent::Usage(usage));
    }

    match events.len() {
        0 => SseEvent::Empty,
        1 => events.remove(0),
        _ => SseEvent::Batch(events),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn chat_request_serializes_native_json_schema_response_format() {
        let messages = vec![Message::user("answer")];
        let response_format = json!({
            "type": "json_schema",
            "json_schema": {
                "name": "mag_output",
                "strict": true,
                "schema": {"type": "string"}
            }
        });
        let request = ChatRequest {
            model: "gpt-5",
            messages: &messages,
            stream: true,
            reasoning_effort: None,
            stream_options: None,
            tools: None,
            response_format: Some(&response_format),
        };
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["response_format"]["type"], "json_schema");
        assert_eq!(
            value["response_format"]["json_schema"]["schema"]["type"],
            "string"
        );
    }

    #[test]
    fn parse_sse_chunk_preserves_mixed_fields_in_order() {
        let payload = r#"{"choices":[{"delta":{"content":"hi","refusal":"no","reasoning":"why","tool_calls":[{"index":0,"id":"call","type":"function","function":{"name":"f","arguments":"{}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}"#;
        assert_eq!(
            parse_sse_chunk(payload),
            SseEvent::Batch(vec![
                SseEvent::Delta("hi".into()),
                SseEvent::Refusal("no".into()),
                SseEvent::ReasoningDelta("why".into()),
                SseEvent::ToolCallFragment {
                    index: 0,
                    id: Some("call".into()),
                    kind: Some("function".into()),
                    name: Some("f".into()),
                    arguments: Some("{}".into())
                },
                SseEvent::Finish(FinishReason::ToolCalls),
                SseEvent::Usage(Usage {
                    prompt_tokens: 1,
                    completion_tokens: 2,
                    total_tokens: 3
                }),
            ])
        );
    }

    #[test]
    fn parse_sse_chunk_parses_every_standard_finish_reason() {
        for (wire, expected) in [
            ("stop", FinishReason::Stop),
            ("length", FinishReason::Length),
            ("tool_calls", FinishReason::ToolCalls),
            ("content_filter", FinishReason::ContentFilter),
            ("function_call", FinishReason::FunctionCall),
        ] {
            let payload = format!(r#"{{"choices":[{{"delta":{{}},"finish_reason":"{wire}"}}]}}"#);
            assert_eq!(parse_sse_chunk(&payload), SseEvent::Finish(expected));
        }
    }

    #[test]
    fn parse_sse_chunk_rejects_unknown_finish_and_malformed_json() {
        assert!(matches!(
            parse_sse_chunk(r#"{"choices":[{"finish_reason":"weird"}]}"#),
            SseEvent::Malformed(_)
        ));
        assert!(matches!(parse_sse_chunk("{bad"), SseEvent::Malformed(_)));
    }

    #[test]
    fn parse_sse_chunk_recognizes_standard_error_envelope() {
        assert_eq!(
            parse_sse_chunk(
                r#"{"error":{"message":"bad","type":"invalid_request_error","code":"x"}}"#
            ),
            SseEvent::Error {
                message: "bad".into(),
                kind: Some("invalid_request_error".into()),
                code: Some("x".into())
            }
        );
    }

    #[test]
    fn parse_sse_chunk_handles_done_empty_and_usage_only() {
        assert_eq!(parse_sse_chunk("[DONE]"), SseEvent::Done);
        assert_eq!(parse_sse_chunk(" "), SseEvent::Empty);
        assert!(matches!(
            parse_sse_chunk(r#"{"choices":[],"usage":{"prompt_tokens":1}}"#),
            SseEvent::Usage(_)
        ));
    }

    #[test]
    fn message_helpers_set_role() {
        assert_eq!(Message::user("hi").role(), "user");
        assert_eq!(Message::user("hi").content(), Some("hi"));
        assert_eq!(Message::assistant("yo").role(), "assistant");
        assert_eq!(Message::assistant("yo").content(), Some("yo"));
    }

    #[test]
    fn message_assistant_tool_calls_omits_content_field_on_wire() {
        // Updated from prior null-on-wire shape to absent-field shape.
        // Ollama's `/api/chat` validator rejects `content: null` with
        // `invalid message content type: <nil>`, breaking every
        // multi-tool turn after the lead's first dispatch. Both OpenAI
        // and Ollama accept the field-omitted form (Option::None →
        // skip_serializing_if), so omitting is the safe shape across
        // providers.
        let calls = vec![ToolCall {
            id: "call_1".into(),
            kind: "function".into(),
            function: ToolCallFunction {
                name: "read_file".into(),
                arguments: "{\"path\":\"/x\"}".into(),
            },
        }];
        let msg = Message::assistant_tool_calls(calls);
        assert_eq!(msg.role(), "assistant");
        assert!(msg.content().is_none());
        let v = serde_json::to_value(&msg).expect("ser");
        assert!(
            v.get("content").is_none(),
            "content field must be omitted (not null) for Ollama compatibility"
        );
        assert_eq!(
            v.get("tool_calls")
                .and_then(|c| c.as_array())
                .map(|a| a.len()),
            Some(1)
        );
    }

    #[test]
    fn message_tool_result_carries_tool_call_id() {
        let m = Message::tool_result("call_1".into(), "file contents");
        assert_eq!(m.role(), "tool");
        assert_eq!(m.tool_call_id(), Some("call_1"));
        let v = serde_json::to_value(&m).expect("ser");
        assert_eq!(
            v.get("tool_call_id").and_then(|s| s.as_str()),
            Some("call_1")
        );
        assert_eq!(
            v.get("content").and_then(|s| s.as_str()),
            Some("file contents")
        );
    }

    #[test]
    fn message_user_serializes_without_tool_fields() {
        let m = Message::user("hi");
        let v = serde_json::to_value(&m).expect("ser");
        assert!(v.get("tool_calls").is_none(), "skip-serialized");
        assert!(v.get("tool_call_id").is_none(), "skip-serialized");
    }

    fn model_ids(models: &[ModelInfo]) -> Vec<&str> {
        models.iter().map(|m| m.id.as_str()).collect()
    }

    #[test]
    fn list_models_parses_data_array() {
        let payload = r#"{"data":[{"id":"gpt-4"},{"id":"gpt-3.5"}]}"#;
        let models = parse_models_response(payload);
        assert_eq!(model_ids(&models), vec!["gpt-3.5", "gpt-4"]);
        assert!(models.iter().all(|m| m.context_window.is_none()));
    }

    #[test]
    fn list_models_sorts_alphabetically() {
        let payload = r#"{"data":[{"id":"zebra"},{"id":"apple"},{"id":"mango"}]}"#;
        let models = parse_models_response(payload);
        assert_eq!(model_ids(&models), vec!["apple", "mango", "zebra"]);
    }

    #[test]
    fn list_models_handles_empty_array() {
        assert!(parse_models_response(r#"{"data":[]}"#).is_empty());
        assert!(parse_models_response(r#"{}"#).is_empty());
        assert!(parse_models_response("not json").is_empty());
    }

    #[test]
    fn list_models_skips_entries_without_id() {
        let payload = r#"{"data":[{"id":"a"},{"object":"model"},{"id":"b"}]}"#;
        let models = parse_models_response(payload);
        assert_eq!(model_ids(&models), vec!["a", "b"]);
    }

    #[test]
    fn list_models_extracts_context_window() {
        let payload = r#"{"data":[
            {"id":"vllm-model","max_model_len":32768},
            {"id":"litellm-model","max_input_tokens":128000},
            {"id":"plain-model"}
        ]}"#;
        let models = parse_models_response(payload);
        assert_eq!(models[0].context_window, Some(128000));
        assert_eq!(models[1].context_window, None);
        assert_eq!(models[2].context_window, Some(32768));
    }

    #[test]
    fn provider_tool_arguments_preserve_valid_json_without_double_encoding() {
        let call = ToolCallFunction {
            name: "read_file".into(),
            arguments: r#"{"path":"x"}"#.into(),
        };
        let wire = serde_json::to_value(call).expect("serialize tool call");
        assert_eq!(wire["arguments"], r#"{"path":"x"}"#);
    }

    #[test]
    fn provider_tool_arguments_wrap_malformed_source_once() {
        let raw = r#"{"path":"x""#;
        let call = ToolCallFunction {
            name: "read_file".into(),
            arguments: raw.into(),
        };
        let wire = serde_json::to_value(call).expect("serialize tool call");
        let arguments = wire["arguments"].as_str().expect("arguments string");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(arguments).unwrap(),
            raw
        );
        assert_ne!(
            serde_json::from_str::<serde_json::Value>(arguments).unwrap(),
            serde_json::to_string(raw).unwrap(),
            "malformed source is not double encoded",
        );
    }

    #[test]
    fn parse_sse_chunk_preserves_empty_arguments_fragment() {
        let payload =
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":""}}]}}]}"#;
        assert_eq!(
            parse_sse_chunk(payload),
            SseEvent::ToolCallFragment {
                index: 0,
                id: None,
                kind: None,
                name: None,
                arguments: Some(String::new()),
            }
        );
    }
}
