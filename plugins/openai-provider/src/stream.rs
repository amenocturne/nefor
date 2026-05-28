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
use std::error::Error as _;
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::header::{ACCEPT, ACCEPT_ENCODING};
use tokio_util::sync::CancellationToken;

use crate::openai::{
    parse_models_response, parse_sse_chunk, ChatRequest, Message, ModelInfo, SseEvent,
    StreamOptions, ToolCall, ToolCallFunction, Usage,
};

/// Outcome of `run_chat_stream`. Carries everything the caller needs to
/// either finalize the turn (`tool_calls` empty, `finish_reason ==
/// "stop"`) or run the tool loop (`tool_calls` non-empty,
/// `finish_reason == "tool_calls"`).
///
/// `reasoning_text` accumulates `delta.reasoning` chunks (Ollama's
/// thinking trace for Gemma 3 / Qwen 3). It is intentionally NOT
/// concatenated into `full_text`: the stored assistant message must
/// stay clean (no reasoning) so it doesn't feed back into the next
/// request's history. Callers that want to relay the trace use the
/// dedicated reasoning callback or read this field at end-of-turn.
#[derive(Debug, Clone, Default)]
pub struct StreamOutcome {
    pub full_text: String,
    pub finish_reason: Option<String>,
    pub usage: Option<Usage>,
    pub interrupted: bool,
    pub tool_calls: Vec<ToolCall>,
    pub reasoning_text: String,
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
    /// 400 specifically signalling the model doesn't support tool-calling
    /// (Ollama: `<model> does not support tools`). Surfaced as its own
    /// variant so the dispatcher can transparently retry the same turn
    /// without the `tools` array — the user's mental model is "I sent a
    /// message, the model should reply", not "raw HTTP error". The
    /// dispatcher also caches the model as tools-incapable so subsequent
    /// turns skip the round-trip cost.
    #[error("model does not support tools: {body}")]
    ToolsUnsupported { body: String },
    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("read error mid-stream: {0}")]
    Body(String),
}

/// Heuristic: does this error body match the "model does not support
/// tools" signature Ollama emits when the active model lacks the `tools`
/// capability? Match the substring rather than parsing the JSON shape so
/// future minor wording changes (model name, surrounding quotes) don't
/// break the detection. Case-insensitive on the keyword phrase.
pub(crate) fn body_signals_tools_unsupported(body: &str) -> bool {
    body.to_ascii_lowercase().contains("does not support tools")
}

/// Boundary signal passed to the reasoning callback. The dispatcher
/// uses this to drive `<prefix>.stream.reasoning_delta` (per-chunk) and
/// `<prefix>.stream.reasoning_end` (one-shot, synthesised at the moment
/// reasoning stops streaming).
///
/// `End` fires exactly once per turn, at whichever of these comes first:
///   * the first `delta.content` chunk (model transitioned thinking →
///     output);
///   * `finish_reason` arrives without any prior content (reasoning-only
///     turn — typical for Gemma 3's reasoning-only edge case);
///   * the body stream ends (defensive — providers we've seen always
///     close with finish_reason, but don't rely on it).
///
/// `End` carries the full accumulated reasoning text so the chat plugin
/// can render the collapsed row without holding its own buffer; the
/// dispatcher can also stamp it onto `chat.complete.result` for
/// non-streaming consumers.
pub enum ReasoningEvent<'a> {
    Delta(&'a str),
    End { text: &'a str },
}

/// Drive a single chat-completions streaming call.
///
/// `on_delta` is invoked synchronously for every content chunk; the
/// caller emits `<prefix>.stream.delta` from inside that callback.
/// `on_reasoning` is invoked for every reasoning chunk and once with
/// `End` when reasoning is done — the chat plugin uses these to live-
/// stream the thinking trace then collapse it. Tool calls are
/// accumulated silently and exposed via the returned
/// `StreamOutcome.tool_calls` — they don't fire a callback because the
/// dispatcher's tool-loop logic needs the assembled list, not deltas.
#[allow(clippy::too_many_arguments)]
pub async fn run_chat_stream<F, R>(
    client: &reqwest::Client,
    endpoint: &str,
    api_key: Option<&str>,
    auth_header: &str,
    model: &str,
    messages: &[Message],
    tools: Option<&[serde_json::Value]>,
    cancel: CancellationToken,
    mut on_delta: F,
    mut on_reasoning: R,
) -> Result<StreamOutcome, StreamError>
where
    F: FnMut(&str),
    R: FnMut(ReasoningEvent<'_>),
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
        let body_json =
            serde_json::to_string(&req).unwrap_or_else(|e| format!("<serialize-error: {e}>"));
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

    let mut builder = client
        .post(endpoint)
        .header(ACCEPT, "text/event-stream")
        .header(ACCEPT_ENCODING, "identity")
        .json(&req)
        .timeout(Duration::from_secs(120));
    if let Some(k) = api_key {
        builder = apply_auth(builder, auth_header, k);
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
            Err(e) => return Err(StreamError::Request(reqwest_error_detail(&e))),
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
        // Reactive fallback: only meaningful when the request actually
        // carried tools. If we sent no tools and still got the signature,
        // the server is telling us something else — fall through to Http.
        if status == 400 && tools.is_some() && body_signals_tools_unsupported(&body) {
            return Err(StreamError::ToolsUnsupported { body });
        }
        return Err(StreamError::Http { status, body });
    }

    let mut outcome = StreamOutcome::default();
    let mut buffer = Vec::new();
    let mut tc_acc = ToolCallAccumulator::new();
    // Latch flipped once we've fired ReasoningEvent::End — at the
    // boundary where reasoning stops and content/finish/usage takes
    // over. Prevents duplicate end events if frames arrive interleaved.
    let mut reasoning_ended = false;
    let mut byte_stream = response.bytes_stream();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                outcome.interrupted = true;
                outcome.tool_calls = tc_acc.finalize();
                maybe_end_reasoning(&outcome, &mut reasoning_ended, &mut on_reasoning);
                return Ok(outcome);
            }
            next = byte_stream.next() => {
                match next {
                    None => break,
                    Some(Err(e)) => return Err(StreamError::Body(reqwest_error_detail(&e))),
                    Some(Ok(bytes)) => {
                        buffer.extend_from_slice(&bytes);
                        drain_complete_frames(
                            &mut buffer,
                            &mut outcome,
                            &mut tc_acc,
                            &mut reasoning_ended,
                            &mut on_delta,
                            &mut on_reasoning,
                        )?;
                    }
                }
            }
        }
    }
    // Drain any leftover frame the server didn't terminate with `\n\n`.
    drain_complete_frames(
        &mut buffer,
        &mut outcome,
        &mut tc_acc,
        &mut reasoning_ended,
        &mut on_delta,
        &mut on_reasoning,
    )?;
    outcome.tool_calls = tc_acc.finalize();
    maybe_end_reasoning(&outcome, &mut reasoning_ended, &mut on_reasoning);
    Ok(outcome)
}

/// Apply the API key to the request builder under the configured
/// header. `Authorization` (the default) takes the standard
/// `Authorization: Bearer <key>` shape via reqwest's `bearer_auth`,
/// preserving compatibility with Ollama / OpenAI / Groq / etc. Any
/// other header name sends the key raw — `<header>: <key>` — for
/// backends like the corp Nestor gateway that gate on a non-standard
/// header. Comparison is case-insensitive so users can write
/// `--auth-header authorization` without surprise.
fn apply_auth(
    builder: reqwest::RequestBuilder,
    auth_header: &str,
    key: &str,
) -> reqwest::RequestBuilder {
    if auth_header.eq_ignore_ascii_case("Authorization") {
        builder.bearer_auth(key)
    } else {
        builder.header(auth_header, key)
    }
}

/// Fire `ReasoningEvent::End` once, idempotently. Called whenever the
/// stream wraps up — either because content has started, the model
/// emitted `finish_reason`, or the body terminated. Skips firing when
/// no reasoning was observed at all (the common content-only path).
fn maybe_end_reasoning<R>(outcome: &StreamOutcome, ended: &mut bool, on_reasoning: &mut R)
where
    R: FnMut(ReasoningEvent<'_>),
{
    if *ended {
        return;
    }
    if outcome.reasoning_text.is_empty() {
        return;
    }
    *ended = true;
    on_reasoning(ReasoningEvent::End {
        text: &outcome.reasoning_text,
    });
}

/// Fetch the model catalog from `<base_url>/v1/models`. Returns an
/// alphabetically-sorted list of model IDs on success.
pub async fn list_models(
    client: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
    auth_header: &str,
) -> Result<Vec<ModelInfo>, StreamError> {
    let endpoint = format!("{}/v1/models", base_url.trim_end_matches('/'));
    let mut builder = client.get(&endpoint).timeout(Duration::from_secs(30));
    if let Some(k) = api_key {
        builder = apply_auth(builder, auth_header, k);
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

fn reqwest_error_detail(err: &reqwest::Error) -> String {
    let mut parts = vec![err.to_string()];
    if err.is_timeout() {
        parts.push("timeout=true".into());
    }
    if err.is_connect() {
        parts.push("connect=true".into());
    }
    if err.is_decode() {
        parts.push("decode=true".into());
    }
    if let Some(status) = err.status() {
        parts.push(format!("status={status}"));
    }

    let mut source = err.source();
    while let Some(err) = source {
        parts.push(format!("source: {err}"));
        source = err.source();
    }
    parts.join("; ")
}

/// Pull every blank-line-delimited SSE frame out of `buffer`, parse the
/// `data:` lines inside each, and apply them to `outcome` / `tc_acc` /
/// `on_delta` / `on_reasoning`. Trailing partial frames stay in the
/// buffer for the next read.
///
/// `reasoning_ended` is the shared latch that gates the one-shot
/// `ReasoningEvent::End`. We synthesise it here at the boundary where
/// the model transitions out of thinking — either the first content
/// delta arrives, or `finish_reason` lands. Subsequent calls are no-ops.
fn drain_complete_frames<F, R>(
    buffer: &mut Vec<u8>,
    outcome: &mut StreamOutcome,
    tc_acc: &mut ToolCallAccumulator,
    reasoning_ended: &mut bool,
    on_delta: &mut F,
    on_reasoning: &mut R,
) -> Result<(), StreamError>
where
    F: FnMut(&str),
    R: FnMut(ReasoningEvent<'_>),
{
    while let Some((end, sep_len)) = find_frame_end(buffer) {
        let drained: Vec<u8> = buffer.drain(..end + sep_len).collect();
        let frame = std::str::from_utf8(&drained[..end])
            .map_err(|err| StreamError::Body(format!("SSE frame was not valid UTF-8: {err}")))?;
        let Some(payload) = frame_data_payload(frame) else {
            continue;
        };
        match parse_sse_chunk(&payload) {
            SseEvent::Delta(text) => {
                // Boundary: first content chunk closes the reasoning
                // stream. The chat plugin uses this to flip the
                // live reasoning preview into its collapsed form.
                if !*reasoning_ended && !outcome.reasoning_text.is_empty() {
                    *reasoning_ended = true;
                    on_reasoning(ReasoningEvent::End {
                        text: &outcome.reasoning_text,
                    });
                }
                // Defensive strip: when Qwen-style chat templates close
                // reasoning on a literal `</think>` written inside the
                // model's monologue, the literal close-tag character
                // sequence frequently leads the first content chunk
                // (Ollama emits the matched tag itself onto the content
                // channel after the reasoning split). The user shouldn't
                // see `</think>` rendered as the start of an answer.
                // Strip a single leading `</think>` (with optional
                // surrounding whitespace) from the content stream.
                // Only fires when reasoning was non-empty AND we have
                // not yet emitted any content — covers the leak shape
                // without disturbing legitimate uses of the literal
                // string later in the answer.
                let emit_text: &str = if outcome.full_text.is_empty() && !outcome.reasoning_text.is_empty() {
                    let trimmed = text.trim_start();
                    if let Some(rest) = trimmed.strip_prefix("</think>") {
                        rest.trim_start_matches(|c: char| c == '>' || c.is_whitespace())
                    } else {
                        text.as_str()
                    }
                } else {
                    text.as_str()
                };
                if !emit_text.is_empty() {
                    on_delta(emit_text);
                    outcome.full_text.push_str(emit_text);
                }
            }
            SseEvent::ReasoningDelta(text) => {
                outcome.reasoning_text.push_str(&text);
                on_reasoning(ReasoningEvent::Delta(&text));
            }
            SseEvent::Finish(reason) => {
                outcome.finish_reason = Some(reason);
                // Reasoning-only turn (Gemma 3 edge case): finish
                // arrives without any content. Still close the
                // reasoning channel so the chat plugin renders the
                // collapsed/expanded row instead of leaving the
                // assistant entry stuck on "streaming".
                if !*reasoning_ended && !outcome.reasoning_text.is_empty() {
                    *reasoning_ended = true;
                    on_reasoning(ReasoningEvent::End {
                        text: &outcome.reasoning_text,
                    });
                }
            }
            SseEvent::Usage(u) => {
                outcome.usage = Some(u);
            }
            SseEvent::ToolCallStart {
                index,
                id,
                name,
                args,
            } => {
                tc_acc.start(index, id, name, args);
            }
            SseEvent::ToolCallArgsDelta { index, delta } => {
                tc_acc.append_args(index, &delta);
            }
            SseEvent::Done | SseEvent::Empty => {}
        }
    }
    Ok(())
}

fn frame_data_payload(frame: &str) -> Option<String> {
    let mut payload = String::new();
    for line in frame.lines() {
        let line = line.trim_end_matches('\r');
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        let rest = rest.strip_prefix(' ').unwrap_or(rest);
        if !payload.is_empty() {
            payload.push('\n');
        }
        payload.push_str(rest);
    }
    (!payload.is_empty()).then_some(payload)
}

fn find_frame_end(buffer: &[u8]) -> Option<(usize, usize)> {
    for i in 0..buffer.len().saturating_sub(1) {
        if buffer[i] == b'\n' && buffer[i + 1] == b'\n' {
            return Some((i, 2));
        }
        if i + 3 < buffer.len() && &buffer[i..i + 4] == b"\r\n\r\n" {
            return Some((i, 4));
        }
    }
    None
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

    fn start(&mut self, index: usize, id: String, name: String, initial_args: String) {
        self.by_index.insert(
            index,
            ToolCallBuilder {
                id,
                name,
                arguments: initial_args,
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

    fn bytes(s: &str) -> Vec<u8> {
        s.as_bytes().to_vec()
    }

    fn push(buffer: &mut Vec<u8>, s: &str) {
        buffer.extend_from_slice(s.as_bytes());
    }

    #[test]
    fn body_signals_tools_unsupported_matches_ollama_exact_phrase() {
        // Ollama 0.x body shape, lifted from the user-reported bug.
        let body = r#"{"error":{"message":"registry.ollama.ai/library/translategemma:latest does not support tools"}}"#;
        assert!(body_signals_tools_unsupported(body));
    }

    #[test]
    fn body_signals_tools_unsupported_is_case_insensitive() {
        assert!(body_signals_tools_unsupported(
            "Model Does Not Support Tools"
        ));
    }

    #[test]
    fn body_signals_tools_unsupported_rejects_unrelated_400() {
        assert!(!body_signals_tools_unsupported(
            r#"{"error":{"message":"model not found"}}"#
        ));
        assert!(!body_signals_tools_unsupported("invalid api key"));
    }

    #[test]
    fn drain_yields_deltas_then_finish_and_usage() {
        let mut buffer = Vec::new();
        push(
            &mut buffer,
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n",
        );
        push(
            &mut buffer,
            "data: {\"choices\":[{\"delta\":{\"content\":\" there\"}}]}\n\n",
        );
        push(
            &mut buffer,
            "data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n",
        );
        push(&mut buffer, "data: {\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n\n");
        push(&mut buffer, "data: [DONE]\n\n");

        let mut deltas: Vec<String> = Vec::new();
        let mut outcome = StreamOutcome::default();
        let mut tc = ToolCallAccumulator::new();
        let mut ended = false;
        drain_complete_frames(
            &mut buffer,
            &mut outcome,
            &mut tc,
            &mut ended,
            &mut |s| deltas.push(s.to_owned()),
            &mut |_| {},
        )
        .expect("drain ok");
        assert_eq!(deltas, vec!["Hi", " there"]);
        assert_eq!(outcome.full_text, "Hi there");
        assert_eq!(outcome.finish_reason.as_deref(), Some("stop"));
        let u = outcome.usage.expect("usage");
        assert_eq!(u.prompt_tokens, 3);
        assert_eq!(u.completion_tokens, 2);
        assert!(tc.finalize().is_empty(), "no tool calls");
        assert!(outcome.reasoning_text.is_empty(), "no reasoning seen");
    }

    #[test]
    fn drain_keeps_partial_trailing_frame() {
        // A chunk that arrives split across two reads: the first half
        // doesn't end with a frame terminator. drain must leave it alone.
        let mut buffer = bytes("data: {\"choices\":[{\"delta\":{\"content\":\"par");
        let mut outcome = StreamOutcome::default();
        let mut deltas: Vec<String> = Vec::new();
        let mut tc = ToolCallAccumulator::new();
        let mut ended = false;
        drain_complete_frames(
            &mut buffer,
            &mut outcome,
            &mut tc,
            &mut ended,
            &mut |s| deltas.push(s.to_owned()),
            &mut |_| {},
        )
        .expect("drain ok");
        assert!(deltas.is_empty(), "no complete frames yet");
        assert!(
            std::str::from_utf8(&buffer).expect("utf8").contains("par"),
            "buffer retained the partial frame"
        );
    }

    #[test]
    fn drain_ignores_non_data_lines_and_blanks() {
        let mut buffer = bytes(
            ": keepalive\nevent: ping\ndata: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n",
        );
        let mut outcome = StreamOutcome::default();
        let mut deltas: Vec<String> = Vec::new();
        let mut tc = ToolCallAccumulator::new();
        let mut ended = false;
        drain_complete_frames(
            &mut buffer,
            &mut outcome,
            &mut tc,
            &mut ended,
            &mut |s| deltas.push(s.to_owned()),
            &mut |_| {},
        )
        .expect("drain ok");
        assert_eq!(deltas, vec!["x"]);
    }

    #[test]
    fn drain_joins_multiline_data_fields() {
        let mut buffer = bytes(
            "event: chunk\n\
             data: {\"choices\":[\n\
             data: {\"delta\":{\"content\":\"x\"}}\n\
             data: ]}\n\n",
        );
        let mut outcome = StreamOutcome::default();
        let mut deltas: Vec<String> = Vec::new();
        let mut tc = ToolCallAccumulator::new();
        let mut ended = false;
        drain_complete_frames(
            &mut buffer,
            &mut outcome,
            &mut tc,
            &mut ended,
            &mut |s| deltas.push(s.to_owned()),
            &mut |_| {},
        )
        .expect("drain ok");
        assert_eq!(deltas, vec!["x"]);
    }

    #[test]
    fn drain_handles_crlf_frame_boundaries() {
        let mut buffer = bytes("data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\r\n\r\n");
        let mut outcome = StreamOutcome::default();
        let mut deltas: Vec<String> = Vec::new();
        let mut tc = ToolCallAccumulator::new();
        let mut ended = false;
        drain_complete_frames(
            &mut buffer,
            &mut outcome,
            &mut tc,
            &mut ended,
            &mut |s| deltas.push(s.to_owned()),
            &mut |_| {},
        )
        .expect("drain ok");
        assert_eq!(deltas, vec!["x"]);
    }

    #[test]
    fn drain_preserves_utf8_split_across_chunks() {
        let raw = "data: {\"choices\":[{\"delta\":{\"content\":\"é\"}}]}\n\n";
        let raw_bytes = raw.as_bytes();
        let split = raw_bytes
            .windows("é".len())
            .position(|w| w == "é".as_bytes())
            .expect("contains e acute")
            + 1;
        let mut buffer = raw_bytes[..split].to_vec();
        let mut outcome = StreamOutcome::default();
        let mut deltas: Vec<String> = Vec::new();
        let mut tc = ToolCallAccumulator::new();
        let mut ended = false;
        drain_complete_frames(
            &mut buffer,
            &mut outcome,
            &mut tc,
            &mut ended,
            &mut |s| deltas.push(s.to_owned()),
            &mut |_| {},
        )
        .expect("partial drain ok");
        assert!(deltas.is_empty());

        buffer.extend_from_slice(&raw_bytes[split..]);
        drain_complete_frames(
            &mut buffer,
            &mut outcome,
            &mut tc,
            &mut ended,
            &mut |s| deltas.push(s.to_owned()),
            &mut |_| {},
        )
        .expect("complete drain ok");
        assert_eq!(deltas, vec!["é"]);
    }

    #[test]
    fn tool_call_accumulator_assembles_one_call() {
        let mut tc = ToolCallAccumulator::new();
        tc.start(0, "call_a".into(), "read_file".into(), String::new());
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
    fn tool_call_accumulator_seeds_args_from_start_chunk() {
        // Ollama-style: args delivered entirely in the start chunk.
        let mut tc = ToolCallAccumulator::new();
        tc.start(
            0,
            "call_a".into(),
            "spawn_graph".into(),
            r#"{"graph":{"nodes":[]}}"#.into(),
        );
        let calls = tc.finalize();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.arguments, r#"{"graph":{"nodes":[]}}"#);
    }

    #[test]
    fn tool_call_accumulator_assembles_parallel_calls_in_index_order() {
        let mut tc = ToolCallAccumulator::new();
        tc.start(0, "call_a".into(), "read_file".into(), String::new());
        tc.start(1, "call_b".into(), "write_file".into(), String::new());
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
        assert_eq!(
            calls[1].function.arguments,
            "{\"path\":\"/x\",\"content\":\"hi\"}"
        );
    }

    #[test]
    fn drain_assembles_tool_calls_across_chunks() {
        let mut buffer = Vec::new();
        // Chunk 1: start
        push(&mut buffer, "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_x\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"\"}}]}}]}\n\n");
        // Chunks 2/3: argument fragments
        push(&mut buffer, "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n");
        push(&mut buffer, "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"/tmp/x\\\"}\"}}]}}]}\n\n");
        // Finish
        push(
            &mut buffer,
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        );
        push(&mut buffer, "data: [DONE]\n\n");

        let mut outcome = StreamOutcome::default();
        let mut deltas: Vec<String> = Vec::new();
        let mut tc = ToolCallAccumulator::new();
        let mut ended = false;
        drain_complete_frames(
            &mut buffer,
            &mut outcome,
            &mut tc,
            &mut ended,
            &mut |s| deltas.push(s.to_owned()),
            &mut |_| {},
        )
        .expect("drain ok");
        assert!(deltas.is_empty(), "no text deltas in a tool-call turn");
        assert_eq!(outcome.finish_reason.as_deref(), Some("tool_calls"));
        let calls = tc.finalize();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_x");
        assert_eq!(calls[0].function.name, "read_file");
        assert_eq!(calls[0].function.arguments, "{\"path\":\"/tmp/x\"}");
    }

    /// Reasoning-then-content interleave assembles correctly into
    /// separate fields. The reasoning callback fires once per chunk
    /// during the thinking phase, then `End` exactly once when the
    /// first content delta arrives. `full_text` only sees content.
    #[test]
    fn drain_separates_reasoning_from_content_with_boundary_end() {
        let mut buffer = Vec::new();
        // Three reasoning chunks first (Ollama's typical Qwen3 shape).
        push(
            &mut buffer,
            "data: {\"choices\":[{\"delta\":{\"reasoning\":\"Let me \"}}]}\n\n",
        );
        push(
            &mut buffer,
            "data: {\"choices\":[{\"delta\":{\"reasoning\":\"think \"}}]}\n\n",
        );
        push(
            &mut buffer,
            "data: {\"choices\":[{\"delta\":{\"reasoning\":\"about it.\"}}]}\n\n",
        );
        // Then content. The first content chunk must trigger
        // ReasoningEvent::End with the full accumulated trace.
        push(
            &mut buffer,
            "data: {\"choices\":[{\"delta\":{\"content\":\"The \"}}]}\n\n",
        );
        push(
            &mut buffer,
            "data: {\"choices\":[{\"delta\":{\"content\":\"answer.\"}}]}\n\n",
        );
        push(
            &mut buffer,
            "data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n",
        );
        push(&mut buffer, "data: [DONE]\n\n");

        let mut outcome = StreamOutcome::default();
        let mut content_deltas: Vec<String> = Vec::new();
        let mut reasoning_deltas: Vec<String> = Vec::new();
        let mut reasoning_ends: Vec<String> = Vec::new();
        let mut tc = ToolCallAccumulator::new();
        let mut ended = false;
        drain_complete_frames(
            &mut buffer,
            &mut outcome,
            &mut tc,
            &mut ended,
            &mut |s| content_deltas.push(s.to_owned()),
            &mut |ev| match ev {
                ReasoningEvent::Delta(s) => reasoning_deltas.push(s.to_owned()),
                ReasoningEvent::End { text } => reasoning_ends.push(text.to_owned()),
            },
        )
        .expect("drain ok");

        assert_eq!(content_deltas, vec!["The ", "answer."]);
        assert_eq!(outcome.full_text, "The answer.");
        assert_eq!(outcome.reasoning_text, "Let me think about it.");
        assert_eq!(reasoning_deltas, vec!["Let me ", "think ", "about it."]);
        // End fires exactly once at the content boundary, carrying the
        // fully accumulated trace.
        assert_eq!(reasoning_ends, vec!["Let me think about it."]);
    }

    /// Reasoning-only turn (Gemma 3 edge case): the model emits
    /// reasoning then `finish_reason: "stop"` with NO content. End must
    /// still fire exactly once so the chat plugin can finalise.
    #[test]
    fn drain_synthesises_reasoning_end_on_finish_when_no_content() {
        let mut buffer = Vec::new();
        push(
            &mut buffer,
            "data: {\"choices\":[{\"delta\":{\"reasoning\":\"thinking only\"}}]}\n\n",
        );
        push(
            &mut buffer,
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        );
        push(&mut buffer, "data: [DONE]\n\n");

        let mut outcome = StreamOutcome::default();
        let mut reasoning_ends: Vec<String> = Vec::new();
        let mut tc = ToolCallAccumulator::new();
        let mut ended = false;
        drain_complete_frames(
            &mut buffer,
            &mut outcome,
            &mut tc,
            &mut ended,
            &mut |_| {},
            &mut |ev| {
                if let ReasoningEvent::End { text } = ev {
                    reasoning_ends.push(text.to_owned());
                }
            },
        )
        .expect("drain ok");

        assert!(outcome.full_text.is_empty(), "no content emitted");
        assert_eq!(outcome.reasoning_text, "thinking only");
        assert_eq!(reasoning_ends, vec!["thinking only"]);
    }
}
