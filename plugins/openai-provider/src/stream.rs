//! HTTP request → SSE stream → parsed events.
//!
//! `run_chat_stream` issues a single streaming chat-completions request
//! and drives the response, calling the supplied callbacks for each
//! delta / finish / usage chunk. Cancellation is cooperative: the
//! provided `CancellationToken` is observed between SSE frames so a
//! `<prefix>.interrupt` aborts in-flight reads quickly.
//!
//! The tool-call accumulator lives here too: as `ToolCallStart` /
//! `ToolCallArgsDelta` events arrive across many chunks, we rebuild
//! the per-`index` `(id, name, arguments)` triples so the dispatcher
//! sees a clean list of finished tool calls in `StreamOutcome`.

use std::collections::BTreeMap;

use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::openai::{
    parse_models_response, parse_sse_chunk, ChatRequest, Message, SseEvent, StreamOptions,
    ToolCall, ToolCallFunction, Usage,
};

/// Outcome of `run_chat_stream`. Carries everything the caller needs to
/// either finalize the turn (`tool_calls` empty, `finish_reason ==
/// "stop"`) or run the tool loop (`tool_calls` non-empty,
/// `finish_reason == "tool_calls"`).
#[derive(Debug, Clone, Default)]
pub struct StreamOutcome {
    pub full_text: String,
    pub finish_reason: Option<String>,
    pub usage: Option<Usage>,
    pub interrupted: bool,
    pub tool_calls: Vec<ToolCall>,
}

/// Errors that can come out of the HTTP/SSE pipeline. All of them lower
/// to a single `<prefix>.turn.error` body for the caller; the variant is
/// preserved here for tracing/diagnostics.
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error("request failed: {0}")]
    Request(String),
    /// 401 specifically — the server rejected our credentials. Surfaced
    /// separately so the dispatcher can transition auth state.
    #[error("HTTP 401: {body}")]
    Unauthorized { body: String },
    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("read error mid-stream: {0}")]
    Body(String),
}

/// Drive a single chat-completions streaming call.
///
/// `on_delta` is invoked synchronously for every text chunk; the caller
/// emits `<prefix>.stream.delta` from inside that callback. Tool calls
/// are accumulated silently and exposed via the returned
/// `StreamOutcome.tool_calls` — they don't fire a callback because the
/// dispatcher's tool-loop logic needs the assembled list, not deltas.
#[allow(clippy::too_many_arguments)]
pub async fn run_chat_stream<F>(
    client: &reqwest::Client,
    endpoint: &str,
    api_key: Option<&str>,
    model: &str,
    messages: &[Message],
    tools: Option<&[serde_json::Value]>,
    cancel: CancellationToken,
    mut on_delta: F,
) -> Result<StreamOutcome, StreamError>
where
    F: FnMut(&str),
{
    let req = ChatRequest {
        model,
        messages,
        stream: true,
        stream_options: Some(StreamOptions {
            include_usage: true,
        }),
        tools,
    };

    if tracing::enabled!(tracing::Level::INFO) {
        let body_json = serde_json::to_string(&req)
            .unwrap_or_else(|e| format!("<serialize-error: {e}>"));
        tracing::info!(
            target: "openai_provider::http",
            endpoint = endpoint,
            model = model,
            messages_len = messages.len(),
            tools_len = tools.map(|t| t.len()).unwrap_or(0),
            body = %body_json,
            "POST chat completion",
        );
    }

    let mut builder = client.post(endpoint).json(&req);
    if let Some(k) = api_key {
        builder = builder.bearer_auth(k);
    }

    let response = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            return Ok(StreamOutcome {
                interrupted: true,
                ..Default::default()
            });
        }
        r = builder.send() => match r {
            Ok(r) => r,
            Err(e) => return Err(StreamError::Request(e.to_string())),
        },
    };

    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "<unreadable response body>".into());
        if status == 401 {
            return Err(StreamError::Unauthorized { body });
        }
        return Err(StreamError::Http { status, body });
    }

    let mut outcome = StreamOutcome::default();
    let mut buffer = String::new();
    let mut tc_acc = ToolCallAccumulator::new();
    let mut byte_stream = response.bytes_stream();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                outcome.interrupted = true;
                outcome.tool_calls = tc_acc.finalize();
                return Ok(outcome);
            }
            next = byte_stream.next() => {
                match next {
                    None => break,
                    Some(Err(e)) => return Err(StreamError::Body(e.to_string())),
                    Some(Ok(bytes)) => {
                        buffer.push_str(&String::from_utf8_lossy(&bytes));
                        drain_complete_frames(&mut buffer, &mut outcome, &mut tc_acc, &mut on_delta);
                    }
                }
            }
        }
    }
    // Drain any leftover frame the server didn't terminate with `\n\n`.
    drain_complete_frames(&mut buffer, &mut outcome, &mut tc_acc, &mut on_delta);
    outcome.tool_calls = tc_acc.finalize();
    Ok(outcome)
}

/// Fetch the model catalog from `<base_url>/v1/models`. Returns an
/// alphabetically-sorted list of model IDs on success.
pub async fn list_models(
    client: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
) -> Result<Vec<String>, StreamError> {
    let endpoint = format!("{}/v1/models", base_url.trim_end_matches('/'));
    let mut builder = client.get(&endpoint);
    if let Some(k) = api_key {
        builder = builder.bearer_auth(k);
    }
    let response = builder
        .send()
        .await
        .map_err(|e| StreamError::Request(e.to_string()))?;

    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "<unreadable response body>".into());
        if status == 401 {
            return Err(StreamError::Unauthorized { body });
        }
        return Err(StreamError::Http { status, body });
    }
    let body = response
        .text()
        .await
        .map_err(|e| StreamError::Body(e.to_string()))?;
    Ok(parse_models_response(&body))
}

/// Pull every `\n\n`-delimited SSE frame out of `buffer`, parse the
/// `data:` lines inside each, and apply them to `outcome` / `tc_acc` /
/// `on_delta`. Trailing partial frames stay in the buffer for the next
/// read.
fn drain_complete_frames<F>(
    buffer: &mut String,
    outcome: &mut StreamOutcome,
    tc_acc: &mut ToolCallAccumulator,
    on_delta: &mut F,
) where
    F: FnMut(&str),
{
    while let Some(end) = buffer.find("\n\n") {
        let frame: String = buffer.drain(..end + 2).collect();
        for line in frame.lines() {
            let line = line.trim_end_matches('\r');
            let Some(payload) = line.strip_prefix("data:") else {
                continue;
            };
            let payload = payload.trim_start();
            match parse_sse_chunk(payload) {
                SseEvent::Delta(text) => {
                    on_delta(&text);
                    outcome.full_text.push_str(&text);
                }
                SseEvent::Finish(reason) => {
                    outcome.finish_reason = Some(reason);
                }
                SseEvent::Usage(u) => {
                    outcome.usage = Some(u);
                }
                SseEvent::ToolCallStart { index, id, name } => {
                    tc_acc.start(index, id, name);
                }
                SseEvent::ToolCallArgsDelta { index, delta } => {
                    tc_acc.append_args(index, &delta);
                }
                SseEvent::Done | SseEvent::Empty => {}
            }
        }
    }
}

/// Per-stream accumulator for tool-call deltas. Indexed by the model's
/// `tool_calls[*].index` so parallel calls don't interleave.
///
/// `BTreeMap` so `finalize` returns calls in stable index order — the
/// model occasionally inserts call 1 before call 0 in the SSE stream
/// and we want the final list to read 0,1,2,…
#[derive(Debug, Default)]
struct ToolCallAccumulator {
    by_index: BTreeMap<usize, ToolCallBuilder>,
}

#[derive(Debug)]
struct ToolCallBuilder {
    id: String,
    name: String,
    arguments: String,
}

impl ToolCallAccumulator {
    fn new() -> Self {
        Self::default()
    }

    fn start(&mut self, index: usize, id: String, name: String) {
        self.by_index.insert(
            index,
            ToolCallBuilder {
                id,
                name,
                arguments: String::new(),
            },
        );
    }

    fn append_args(&mut self, index: usize, delta: &str) {
        if let Some(b) = self.by_index.get_mut(&index) {
            b.arguments.push_str(delta);
        } else {
            // Args delta arrived before start — providers we've seen
            // never do this, but be defensive: stash a partial entry
            // so the args don't get lost. id/name will be empty; the
            // dispatcher will skip empty-id entries.
            self.by_index.insert(
                index,
                ToolCallBuilder {
                    id: String::new(),
                    name: String::new(),
                    arguments: delta.to_owned(),
                },
            );
        }
    }

    /// Drain the accumulator into a sorted list of `ToolCall`s. Drops
    /// entries with empty `id` (the dispatcher can't address them).
    fn finalize(self) -> Vec<ToolCall> {
        self.by_index
            .into_values()
            .filter(|b| !b.id.is_empty())
            .map(|b| ToolCall {
                id: b.id,
                kind: "function".into(),
                function: ToolCallFunction {
                    name: b.name,
                    arguments: b.arguments,
                },
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_yields_deltas_then_finish_and_usage() {
        let mut buffer = String::new();
        buffer.push_str("data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n");
        buffer.push_str("data: {\"choices\":[{\"delta\":{\"content\":\" there\"}}]}\n\n");
        buffer.push_str("data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n");
        buffer.push_str("data: {\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n\n");
        buffer.push_str("data: [DONE]\n\n");

        let mut deltas: Vec<String> = Vec::new();
        let mut outcome = StreamOutcome::default();
        let mut tc = ToolCallAccumulator::new();
        drain_complete_frames(&mut buffer, &mut outcome, &mut tc, &mut |s| {
            deltas.push(s.to_owned())
        });
        assert_eq!(deltas, vec!["Hi", " there"]);
        assert_eq!(outcome.full_text, "Hi there");
        assert_eq!(outcome.finish_reason.as_deref(), Some("stop"));
        let u = outcome.usage.expect("usage");
        assert_eq!(u.prompt_tokens, 3);
        assert_eq!(u.completion_tokens, 2);
        assert!(tc.finalize().is_empty(), "no tool calls");
    }

    #[test]
    fn drain_keeps_partial_trailing_frame() {
        // A chunk that arrives split across two reads: the first half
        // doesn't end with a frame terminator. drain must leave it alone.
        let mut buffer = String::from("data: {\"choices\":[{\"delta\":{\"content\":\"par");
        let mut outcome = StreamOutcome::default();
        let mut deltas: Vec<String> = Vec::new();
        let mut tc = ToolCallAccumulator::new();
        drain_complete_frames(&mut buffer, &mut outcome, &mut tc, &mut |s| {
            deltas.push(s.to_owned())
        });
        assert!(deltas.is_empty(), "no complete frames yet");
        assert!(buffer.contains("par"), "buffer retained the partial frame");
    }

    #[test]
    fn drain_ignores_non_data_lines_and_blanks() {
        let mut buffer = String::from(": keepalive\nevent: ping\ndata: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n");
        let mut outcome = StreamOutcome::default();
        let mut deltas: Vec<String> = Vec::new();
        let mut tc = ToolCallAccumulator::new();
        drain_complete_frames(&mut buffer, &mut outcome, &mut tc, &mut |s| {
            deltas.push(s.to_owned())
        });
        assert_eq!(deltas, vec!["x"]);
    }

    #[test]
    fn tool_call_accumulator_assembles_one_call() {
        let mut tc = ToolCallAccumulator::new();
        tc.start(0, "call_a".into(), "read_file".into());
        tc.append_args(0, "{\"path\":");
        tc.append_args(0, "\"/tmp/foo.txt\"}");
        let calls = tc.finalize();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_a");
        assert_eq!(calls[0].kind, "function");
        assert_eq!(calls[0].function.name, "read_file");
        assert_eq!(calls[0].function.arguments, "{\"path\":\"/tmp/foo.txt\"}");
    }

    #[test]
    fn tool_call_accumulator_assembles_parallel_calls_in_index_order() {
        let mut tc = ToolCallAccumulator::new();
        tc.start(0, "call_a".into(), "read_file".into());
        tc.start(1, "call_b".into(), "write_file".into());
        // Args arrive interleaved across indexes — the model emits them
        // in the order it's planning them.
        tc.append_args(1, "{\"path\":\"/x\",");
        tc.append_args(0, "{\"path\":\"/y\"}");
        tc.append_args(1, "\"content\":\"hi\"}");
        let calls = tc.finalize();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "call_a");
        assert_eq!(calls[0].function.arguments, "{\"path\":\"/y\"}");
        assert_eq!(calls[1].id, "call_b");
        assert_eq!(calls[1].function.arguments, "{\"path\":\"/x\",\"content\":\"hi\"}");
    }

    #[test]
    fn drain_assembles_tool_calls_across_chunks() {
        let mut buffer = String::new();
        // Chunk 1: start
        buffer.push_str("data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_x\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"\"}}]}}]}\n\n");
        // Chunks 2/3: argument fragments
        buffer.push_str("data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n");
        buffer.push_str("data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"/tmp/x\\\"}\"}}]}}]}\n\n");
        // Finish
        buffer.push_str("data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n");
        buffer.push_str("data: [DONE]\n\n");

        let mut outcome = StreamOutcome::default();
        let mut deltas: Vec<String> = Vec::new();
        let mut tc = ToolCallAccumulator::new();
        drain_complete_frames(&mut buffer, &mut outcome, &mut tc, &mut |s| {
            deltas.push(s.to_owned())
        });
        assert!(deltas.is_empty(), "no text deltas in a tool-call turn");
        assert_eq!(outcome.finish_reason.as_deref(), Some("tool_calls"));
        let calls = tc.finalize();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_x");
        assert_eq!(calls[0].function.name, "read_file");
        assert_eq!(calls[0].function.arguments, "{\"path\":\"/tmp/x\"}");
    }
}
