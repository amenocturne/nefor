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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    /// Argument JSON as a single string — the API contract is that the
    /// model emits a JSON-encoded string here. We do not parse it; it
    /// rides through to the next request's `tool` message verbatim, and
    /// the tool plugin parses it on receipt.
    pub arguments: String,
}

/// Single chat message in the conversation.
///
/// The OpenAI chat schema overloads this shape across four roles:
///
/// - `user` / `system` — `content` is a string.
/// - `assistant` — `content` may be a string OR null (when the assistant
///   only emitted tool calls). When tool calls are present, `tool_calls`
///   carries them.
/// - `tool` — `content` is the tool's output string and `tool_call_id`
///   correlates back to the assistant's original `tool_calls[i].id`.
///
/// `content` is `Option<String>` so the `null` case round-trips cleanly.
/// Skip-serializing on `tool_calls` / `tool_call_id` keeps the wire
/// minimal for the common user/assistant text case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn user<S: Into<String>>(text: S) -> Self {
        Self {
            role: "user".into(),
            content: Some(text.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn assistant<S: Into<String>>(text: S) -> Self {
        Self {
            role: "assistant".into(),
            content: Some(text.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// Assistant message that only emitted tool calls (no prose). The
    /// OpenAI API requires `content: null` rather than `""` on this
    /// shape.
    pub fn assistant_tool_calls(tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant".into(),
            content: None,
            tool_calls,
            tool_call_id: None,
        }
    }

    /// Assistant message that combined prose + tool calls. Used when the
    /// model interleaves text deltas with tool-call deltas in the same
    /// turn.
    pub fn assistant_with_tool_calls<S: Into<String>>(
        text: S,
        tool_calls: Vec<ToolCall>,
    ) -> Self {
        Self {
            role: "assistant".into(),
            content: Some(text.into()),
            tool_calls,
            tool_call_id: None,
        }
    }

    /// Tool result message. `content` carries either the tool's output
    /// string OR an error message (the model doesn't distinguish on the
    /// wire — both are just "what the tool said"). `tool_call_id` MUST
    /// match the corresponding assistant tool_calls entry's `id`.
    pub fn tool_result<S: Into<String>>(tool_call_id: String, content: S) -> Self {
        Self {
            role: "tool".into(),
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id),
        }
    }
}

/// Body of a streaming chat-completions request.
#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest<'a> {
    pub model: &'a str,
    pub messages: &'a [Message],
    pub stream: bool,
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
}

#[derive(Debug, Clone, Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

/// One parsed SSE chunk from the stream.
///
/// The variants line up 1:1 with what the dispatcher needs to act on; a
/// single SSE chunk maps to exactly one `SseEvent`. Tool-call deltas are
/// split across `ToolCallStart` (when the model first names the call)
/// and `ToolCallArgsDelta` (each subsequent chunk that grows the
/// arguments string).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseEvent {
    /// Incremental token text from `choices[0].delta.content`.
    Delta(String),
    /// `choices[0].finish_reason` arrived; the assistant message is done.
    /// Some providers emit this on a chunk that also carries a final
    /// content delta — callers should treat both fields as independent.
    Finish(String),
    /// Final usage report (input/output token totals).
    Usage(Usage),
    /// `data: [DONE]` sentinel.
    Done,
    /// First chunk of a tool call — carries the `id` and function `name`.
    /// `index` distinguishes parallel calls within the same assistant
    /// turn (the model can request several tools at once).
    ToolCallStart {
        index: usize,
        id: String,
        name: String,
    },
    /// Subsequent chunk of a tool call — carries an incremental fragment
    /// of the JSON-encoded arguments string. The accumulator concatenates
    /// every fragment for the same `index` and parses the result at
    /// finish-time.
    ToolCallArgsDelta { index: usize, delta: String },
    /// Empty / informational frame we can ignore.
    Empty,
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

/// Parse the `data` array from a `GET /v1/models` response into a sorted
/// alphabetical list of model IDs. Skips entries without a string `id`.
pub fn parse_models_response(payload: &str) -> Vec<String> {
    let value: serde_json::Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let Some(arr) = value.get("data").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = arr
        .iter()
        .filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(str::to_owned))
        .collect();
    ids.sort();
    ids
}

/// Parse a single SSE `data:` payload (the JSON between `data: ` and the
/// blank-line terminator, or the literal `[DONE]` sentinel).
///
/// Errors: returns `Empty` for unparseable JSON rather than failing —
/// some providers emit keepalives or comments and we don't want one bad
/// frame to abort a turn.
///
/// Precedence when a single chunk carries multiple shapes (rare but
/// possible): text delta first, then tool-call deltas, then
/// `finish_reason`, then `usage`. Per-chunk we only ever return one
/// variant; if a chunk packs e.g. a text delta AND a tool-call start,
/// the text wins and the tool-call shape is dropped — we have not
/// observed that combination from OpenAI/Ollama in practice. If it
/// emerges this function would need to grow into a variant-list return.
pub fn parse_sse_chunk(payload: &str) -> SseEvent {
    let payload = payload.trim();
    if payload.is_empty() {
        return SseEvent::Empty;
    }
    if payload == "[DONE]" {
        return SseEvent::Done;
    }
    let value: serde_json::Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(_) => return SseEvent::Empty,
    };

    let first_choice = value
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first());

    if let Some(content) = first_choice
        .and_then(|c| c.get("delta"))
        .and_then(|d| d.get("content"))
        .and_then(|t| t.as_str())
    {
        if !content.is_empty() {
            return SseEvent::Delta(content.to_owned());
        }
    }

    // Tool-call deltas live alongside the regular `delta.content` field.
    // We only look at the first entry in `tool_calls` per chunk: providers
    // we've seen emit one tool-call shape per chunk even when several
    // calls are in flight (different `index` values per chunk).
    if let Some(tc_array) = first_choice
        .and_then(|c| c.get("delta"))
        .and_then(|d| d.get("tool_calls"))
        .and_then(|tc| tc.as_array())
    {
        if let Some(tc) = tc_array.first() {
            let index = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            // Chunk 1 carries id + function.name (and possibly an empty
            // arguments string). Detect by name presence — id alone is
            // not enough since some providers stream id-only setup
            // chunks but for OpenAI/Ollama the first chunk carries name.
            let function = tc.get("function");
            let name = function
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str());
            let id = tc.get("id").and_then(|v| v.as_str());
            if let (Some(id), Some(name)) = (id, name) {
                return SseEvent::ToolCallStart {
                    index,
                    id: id.to_owned(),
                    name: name.to_owned(),
                };
            }
            // Subsequent chunks carry only `function.arguments` as a
            // partial string — accumulate.
            if let Some(args) = function
                .and_then(|f| f.get("arguments"))
                .and_then(|a| a.as_str())
            {
                if !args.is_empty() {
                    return SseEvent::ToolCallArgsDelta {
                        index,
                        delta: args.to_owned(),
                    };
                }
            }
        }
    }

    if let Some(reason) = first_choice
        .and_then(|c| c.get("finish_reason"))
        .and_then(|r| r.as_str())
    {
        return SseEvent::Finish(reason.to_owned());
    }
    if let Some(usage) = value.get("usage") {
        if let Ok(u) = serde_json::from_value::<Usage>(usage.clone()) {
            return SseEvent::Usage(u);
        }
    }
    SseEvent::Empty
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sse_chunk_extracts_delta_content() {
        let payload = r#"{"choices":[{"delta":{"content":"Hello"},"index":0}]}"#;
        assert_eq!(parse_sse_chunk(payload), SseEvent::Delta("Hello".into()));
    }

    #[test]
    fn parse_sse_chunk_handles_finish_reason() {
        let payload = r#"{"choices":[{"delta":{},"finish_reason":"stop","index":0}]}"#;
        assert_eq!(parse_sse_chunk(payload), SseEvent::Finish("stop".into()));
    }

    #[test]
    fn parse_sse_chunk_extracts_usage() {
        let payload = r#"{"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":34,"total_tokens":46}}"#;
        let ev = parse_sse_chunk(payload);
        match ev {
            SseEvent::Usage(u) => {
                assert_eq!(u.prompt_tokens, 12);
                assert_eq!(u.completion_tokens, 34);
                assert_eq!(u.total_tokens, 46);
            }
            other => panic!("expected Usage, got {other:?}"),
        }
    }

    #[test]
    fn parse_sse_chunk_handles_done_marker() {
        assert_eq!(parse_sse_chunk("[DONE]"), SseEvent::Done);
        assert_eq!(parse_sse_chunk(" [DONE] "), SseEvent::Done);
    }

    #[test]
    fn parse_sse_chunk_empty_payload_is_empty_event() {
        assert_eq!(parse_sse_chunk(""), SseEvent::Empty);
        assert_eq!(parse_sse_chunk("   "), SseEvent::Empty);
    }

    #[test]
    fn parse_sse_chunk_garbage_json_is_empty_event() {
        assert_eq!(parse_sse_chunk("{not json"), SseEvent::Empty);
    }

    #[test]
    fn parse_sse_chunk_empty_delta_string_is_not_a_delta() {
        // OpenAI sometimes sends an empty content string on the first or
        // last frame. Don't propagate empty deltas — they'd render as
        // no-op events on the bus.
        let payload = r#"{"choices":[{"delta":{"content":""},"index":0}]}"#;
        assert_eq!(parse_sse_chunk(payload), SseEvent::Empty);
    }

    #[test]
    fn message_helpers_set_role() {
        assert_eq!(Message::user("hi").role, "user");
        assert_eq!(Message::user("hi").content.as_deref(), Some("hi"));
        assert_eq!(Message::assistant("yo").role, "assistant");
        assert_eq!(Message::assistant("yo").content.as_deref(), Some("yo"));
    }

    #[test]
    fn message_assistant_tool_calls_has_null_content_on_wire() {
        let calls = vec![ToolCall {
            id: "call_1".into(),
            kind: "function".into(),
            function: ToolCallFunction {
                name: "read_file".into(),
                arguments: "{\"path\":\"/x\"}".into(),
            },
        }];
        let msg = Message::assistant_tool_calls(calls);
        assert_eq!(msg.role, "assistant");
        assert!(msg.content.is_none());
        let v = serde_json::to_value(&msg).expect("ser");
        assert!(v.get("content").is_some(), "explicit null serialized");
        assert_eq!(v.get("content").unwrap(), &serde_json::Value::Null);
        assert_eq!(
            v.get("tool_calls").and_then(|c| c.as_array()).map(|a| a.len()),
            Some(1)
        );
    }

    #[test]
    fn message_tool_result_carries_tool_call_id() {
        let m = Message::tool_result("call_1".into(), "file contents");
        assert_eq!(m.role, "tool");
        assert_eq!(m.tool_call_id.as_deref(), Some("call_1"));
        let v = serde_json::to_value(&m).expect("ser");
        assert_eq!(
            v.get("tool_call_id").and_then(|s| s.as_str()),
            Some("call_1")
        );
        assert_eq!(v.get("content").and_then(|s| s.as_str()), Some("file contents"));
    }

    #[test]
    fn message_user_serializes_without_tool_fields() {
        let m = Message::user("hi");
        let v = serde_json::to_value(&m).expect("ser");
        assert!(v.get("tool_calls").is_none(), "skip-serialized");
        assert!(v.get("tool_call_id").is_none(), "skip-serialized");
    }

    #[test]
    fn list_models_parses_data_array() {
        let payload = r#"{"data":[{"id":"gpt-4"},{"id":"gpt-3.5"}]}"#;
        let ids = parse_models_response(payload);
        assert_eq!(ids, vec!["gpt-3.5".to_string(), "gpt-4".to_string()]);
    }

    #[test]
    fn list_models_sorts_alphabetically() {
        let payload = r#"{"data":[{"id":"zebra"},{"id":"apple"},{"id":"mango"}]}"#;
        let ids = parse_models_response(payload);
        assert_eq!(ids, vec!["apple", "mango", "zebra"]);
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
        let ids = parse_models_response(payload);
        assert_eq!(ids, vec!["a", "b"]);
    }

    // --- Tool-call SSE delta parser tests --------------------------------

    #[test]
    fn parse_sse_chunk_tool_call_start_carries_id_name_index() {
        let payload = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_abc","type":"function","function":{"name":"read_file","arguments":""}}]}}]}"#;
        match parse_sse_chunk(payload) {
            SseEvent::ToolCallStart { index, id, name } => {
                assert_eq!(index, 0);
                assert_eq!(id, "call_abc");
                assert_eq!(name, "read_file");
            }
            other => panic!("expected ToolCallStart, got {other:?}"),
        }
    }

    #[test]
    fn parse_sse_chunk_tool_call_args_delta() {
        let payload = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}}]}"#;
        match parse_sse_chunk(payload) {
            SseEvent::ToolCallArgsDelta { index, delta } => {
                assert_eq!(index, 0);
                assert_eq!(delta, "{\"path\":");
            }
            other => panic!("expected ToolCallArgsDelta, got {other:?}"),
        }
    }

    #[test]
    fn parse_sse_chunk_tool_calls_finish_reason() {
        let payload = r#"{"choices":[{"delta":{},"finish_reason":"tool_calls","index":0}]}"#;
        match parse_sse_chunk(payload) {
            SseEvent::Finish(reason) => assert_eq!(reason, "tool_calls"),
            other => panic!("expected Finish, got {other:?}"),
        }
    }

    #[test]
    fn parse_sse_chunk_tool_call_with_index_other_than_zero() {
        // Parallel tool calls — second call's index is 1.
        let payload = r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call_2","type":"function","function":{"name":"write_file","arguments":""}}]}}]}"#;
        match parse_sse_chunk(payload) {
            SseEvent::ToolCallStart { index, id, name } => {
                assert_eq!(index, 1);
                assert_eq!(id, "call_2");
                assert_eq!(name, "write_file");
            }
            other => panic!("expected ToolCallStart, got {other:?}"),
        }
    }

    #[test]
    fn parse_sse_chunk_tool_call_empty_args_is_empty() {
        // Some providers stream a no-op chunk with empty arguments —
        // treat as Empty so the accumulator doesn't see noise.
        let payload = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":""}}]}}]}"#;
        assert_eq!(parse_sse_chunk(payload), SseEvent::Empty);
    }
}
