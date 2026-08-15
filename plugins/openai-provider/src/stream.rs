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
use nefor_sse::SseBuffer;
use reqwest::header::{ACCEPT, ACCEPT_ENCODING, RETRY_AFTER};
use tokio_util::sync::CancellationToken;

use crate::openai::{
    parse_models_response, parse_sse_chunk, ChatRequest, Message, ModelInfo, SseEvent,
    StreamOptions, ToolCall, ToolCallFunction, Usage,
};

// Retry budget sized for rate-limited internal gateways (429 storms that
// last tens of seconds): 8 attempts = 7 retries at 0.5s, 1s, 2s, 4s, 8s,
// 16s, 30s (doubling, capped) ≈ 60s of patience before the turn fails.
// A `Retry-After` header always overrides the computed delay, and every
// retry surfaces through RetryProgress so the chat shows what's happening.
const CHAT_STREAM_MAX_ATTEMPTS: usize = 8;
const CHAT_STREAM_INITIAL_RETRY_DELAY: Duration = Duration::from_millis(500);
const CHAT_STREAM_MAX_RETRY_DELAY: Duration = Duration::from_secs(30);

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
    #[error("malformed streamed response: {0}")]
    Malformed(String),
    #[error("provider stream error: {message}")]
    Provider {
        message: String,
        kind: Option<String>,
        code: Option<String>,
    },
    #[error("provider refused the request: {0}")]
    Refusal(String),
    #[error("incomplete streamed tool call at index {index}: missing {missing}")]
    IncompleteToolCall { index: usize, missing: String },
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryProgress {
    pub status: Option<u16>,
    pub failed_attempt: usize,
    pub max_attempts: usize,
    pub next_delay: Duration,
}

impl RetryProgress {
    pub fn retry_index(&self) -> usize {
        self.failed_attempt
    }

    pub fn max_retries(&self) -> usize {
        self.max_attempts.saturating_sub(1)
    }
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
    reasoning_effort: Option<&str>,
    cancel: CancellationToken,
    on_delta: F,
    on_reasoning: R,
) -> Result<StreamOutcome, StreamError>
where
    F: FnMut(&str),
    R: FnMut(ReasoningEvent<'_>),
{
    run_chat_stream_with_retry_progress(
        client,
        endpoint,
        api_key,
        auth_header,
        model,
        messages,
        tools,
        reasoning_effort,
        cancel,
        on_delta,
        on_reasoning,
        |_| {},
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn run_chat_stream_with_retry_progress<F, R, P>(
    client: &reqwest::Client,
    endpoint: &str,
    api_key: Option<&str>,
    auth_header: &str,
    model: &str,
    messages: &[Message],
    tools: Option<&[serde_json::Value]>,
    reasoning_effort: Option<&str>,
    cancel: CancellationToken,
    on_delta: F,
    on_reasoning: R,
    on_retry_progress: P,
) -> Result<StreamOutcome, StreamError>
where
    F: FnMut(&str),
    R: FnMut(ReasoningEvent<'_>),
    P: FnMut(RetryProgress),
{
    run_chat_stream_with_retry_progress_and_format(
        client,
        endpoint,
        api_key,
        auth_header,
        model,
        messages,
        tools,
        reasoning_effort,
        None,
        cancel,
        on_delta,
        on_reasoning,
        on_retry_progress,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn run_chat_stream_with_retry_progress_and_format<F, R, P>(
    client: &reqwest::Client,
    endpoint: &str,
    api_key: Option<&str>,
    auth_header: &str,
    model: &str,
    messages: &[Message],
    tools: Option<&[serde_json::Value]>,
    reasoning_effort: Option<&str>,
    response_format: Option<&serde_json::Value>,
    cancel: CancellationToken,
    mut on_delta: F,
    mut on_reasoning: R,
    mut on_retry_progress: P,
) -> Result<StreamOutcome, StreamError>
where
    F: FnMut(&str),
    R: FnMut(ReasoningEvent<'_>),
    P: FnMut(RetryProgress),
{
    let req = ChatRequest {
        model,
        messages,
        stream: true,
        reasoning_effort,
        stream_options: Some(StreamOptions {
            include_usage: true,
        }),
        tools,
        response_format,
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

    let response = {
        let mut attempt = 1usize;
        loop {
            let mut builder = client
                .post(endpoint)
                .header(ACCEPT, "text/event-stream")
                .header(ACCEPT_ENCODING, "identity")
                .json(&req);
            if let Some(k) = api_key {
                builder = apply_auth(builder, auth_header, k);
            }

            let sent = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    return Ok(StreamOutcome {
                        interrupted: true,
                        ..Default::default()
                    });
                }
                r = tokio::time::timeout(Duration::from_secs(120), builder.send()) => match r {
                    Ok(Ok(r)) => Ok(r),
                    Ok(Err(e)) => Err(StreamError::Request(reqwest_error_detail(&e))),
                    Err(_) => Err(StreamError::Request(
                        "timed out waiting for response headers after 120s".to_owned(),
                    )),
                },
            };

            let response = match sent {
                Ok(response) => response,
                Err(e) => {
                    if attempt < CHAT_STREAM_MAX_ATTEMPTS && stream_error_is_retriable(&e) {
                        let delay = retry_delay(attempt, None);
                        tracing::warn!(
                            attempt,
                            max_attempts = CHAT_STREAM_MAX_ATTEMPTS,
                            delay_ms = delay.as_millis(),
                            error = %e,
                            "chat completion request failed; retrying"
                        );
                        on_retry_progress(RetryProgress {
                            status: None,
                            failed_attempt: attempt,
                            max_attempts: CHAT_STREAM_MAX_ATTEMPTS,
                            next_delay: delay,
                        });
                        if !sleep_before_retry(&cancel, delay).await {
                            return Ok(StreamOutcome {
                                interrupted: true,
                                ..Default::default()
                            });
                        }
                        attempt += 1;
                        continue;
                    }
                    return Err(e);
                }
            };

            if response.status().is_success() {
                break response;
            }

            let status = response.status().as_u16();
            let retry_after = retry_after_delay(response.headers());
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
            let err = StreamError::Http { status, body };
            if attempt < CHAT_STREAM_MAX_ATTEMPTS && stream_error_is_retriable(&err) {
                let delay = retry_delay(attempt, retry_after);
                tracing::warn!(
                    attempt,
                    max_attempts = CHAT_STREAM_MAX_ATTEMPTS,
                    delay_ms = delay.as_millis(),
                    error = %err,
                    "chat completion returned retriable HTTP status; retrying"
                );
                on_retry_progress(RetryProgress {
                    status: Some(status),
                    failed_attempt: attempt,
                    max_attempts: CHAT_STREAM_MAX_ATTEMPTS,
                    next_delay: delay,
                });
                if !sleep_before_retry(&cancel, delay).await {
                    return Ok(StreamOutcome {
                        interrupted: true,
                        ..Default::default()
                    });
                }
                attempt += 1;
                continue;
            }
            return Err(err);
        }
    };

    let mut outcome = StreamOutcome::default();
    let mut buffer = SseBuffer::new();
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
                outcome.tool_calls = tc_acc.finalize().unwrap_or_default();
                maybe_end_reasoning(&outcome, &mut reasoning_ended, &mut on_reasoning);
                return Ok(outcome);
            }
            next = byte_stream.next() => {
                match next {
                    None => break,
                    Some(Err(e)) => {
                        if stream_semantically_complete(&outcome) && buffer.is_empty() {
                            break;
                        }
                        return Err(StreamError::Body(reqwest_error_detail(&e)));
                    }
                    Some(Ok(bytes)) => {
                        buffer.push(&bytes);
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
    outcome.tool_calls = tc_acc.finalize()?;
    maybe_end_reasoning(&outcome, &mut reasoning_ended, &mut on_reasoning);
    Ok(outcome)
}

fn stream_semantically_complete(outcome: &StreamOutcome) -> bool {
    outcome.finish_reason.is_some()
}

fn stream_error_is_retriable(err: &StreamError) -> bool {
    match err {
        StreamError::Request(_) => true,
        StreamError::Http { status, .. } => *status == 429 || (500..600).contains(status),
        StreamError::Unauthorized { .. }
        | StreamError::ToolsUnsupported { .. }
        | StreamError::Body(_)
        | StreamError::Malformed(_)
        | StreamError::Provider { .. }
        | StreamError::Refusal(_)
        | StreamError::IncompleteToolCall { .. } => false,
    }
}

fn retry_delay(failed_attempt: usize, retry_after: Option<Duration>) -> Duration {
    retry_after.unwrap_or_else(|| exponential_retry_delay(failed_attempt))
}

fn exponential_retry_delay(failed_attempt: usize) -> Duration {
    let factor = 1u32
        .checked_shl(failed_attempt.saturating_sub(1) as u32)
        .unwrap_or(u32::MAX);
    CHAT_STREAM_INITIAL_RETRY_DELAY
        .saturating_mul(factor)
        .min(CHAT_STREAM_MAX_RETRY_DELAY)
}

fn retry_after_delay(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let now = std::time::SystemTime::now();
    let when = httpdate::parse_http_date(value).ok()?;
    Some(when.duration_since(now).unwrap_or(Duration::ZERO))
}

async fn sleep_before_retry(cancel: &CancellationToken, delay: Duration) -> bool {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => false,
        _ = tokio::time::sleep(delay) => true,
    }
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
    buffer: &mut SseBuffer,
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
    for frame in buffer.drain() {
        let frame = frame.map_err(|err| StreamError::Body(err.to_string()))?;
        apply_sse_event(
            parse_sse_chunk(&frame.data),
            outcome,
            tc_acc,
            reasoning_ended,
            on_delta,
            on_reasoning,
        )?;
    }
    Ok(())
}

fn apply_sse_event<F, R>(
    event: SseEvent,
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
    match event {
        SseEvent::Batch(events) => {
            for event in events {
                apply_sse_event(
                    event,
                    outcome,
                    tc_acc,
                    reasoning_ended,
                    on_delta,
                    on_reasoning,
                )?;
            }
        }
        SseEvent::Delta(text) => {
            if !*reasoning_ended && !outcome.reasoning_text.is_empty() {
                *reasoning_ended = true;
                on_reasoning(ReasoningEvent::End {
                    text: &outcome.reasoning_text,
                });
            }
            let emit_text = if outcome.full_text.is_empty() && !outcome.reasoning_text.is_empty() {
                let trimmed = text.trim_start();
                trimmed
                    .strip_prefix("</think>")
                    .map(|rest| rest.trim_start_matches(|c: char| c == '>' || c.is_whitespace()))
                    .unwrap_or(text.as_str())
            } else {
                text.as_str()
            };
            if !emit_text.is_empty() {
                on_delta(emit_text);
                outcome.full_text.push_str(emit_text);
            }
        }
        SseEvent::Refusal(message) => return Err(StreamError::Refusal(message)),
        SseEvent::ReasoningDelta(text) => {
            outcome.reasoning_text.push_str(&text);
            on_reasoning(ReasoningEvent::Delta(&text));
        }
        SseEvent::ToolCallFragment {
            index,
            id,
            kind,
            name,
            arguments,
        } => {
            tc_acc.apply(index, id, kind, name, arguments);
        }
        SseEvent::Finish(reason) => {
            outcome.finish_reason = Some(reason.as_str().to_owned());
            if !*reasoning_ended && !outcome.reasoning_text.is_empty() {
                *reasoning_ended = true;
                on_reasoning(ReasoningEvent::End {
                    text: &outcome.reasoning_text,
                });
            }
        }
        SseEvent::Usage(usage) => outcome.usage = Some(usage),
        SseEvent::Error {
            message,
            kind,
            code,
        } => {
            return Err(StreamError::Provider {
                message,
                kind,
                code,
            });
        }
        SseEvent::Malformed(message) => return Err(StreamError::Malformed(message)),
        SseEvent::Done | SseEvent::Empty => {}
    }
    Ok(())
}

#[derive(Debug, Default)]
struct ToolCallAccumulator {
    by_index: BTreeMap<usize, ToolCallBuilder>,
}

#[derive(Debug, Default)]
struct ToolCallBuilder {
    id: Option<String>,
    kind: Option<String>,
    name: Option<String>,
    arguments: Option<String>,
}

impl ToolCallAccumulator {
    fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    fn start(&mut self, index: usize, id: String, name: String, arguments: String) {
        self.apply(
            index,
            Some(id),
            Some("function".to_owned()),
            Some(name),
            Some(arguments),
        );
    }

    #[cfg(test)]
    fn append_args(&mut self, index: usize, arguments: &str) {
        self.apply(index, None, None, None, Some(arguments.to_owned()));
    }

    fn apply(
        &mut self,
        index: usize,
        id: Option<String>,
        kind: Option<String>,
        name: Option<String>,
        arguments: Option<String>,
    ) {
        let builder = self.by_index.entry(index).or_default();
        if let Some(id) = id {
            builder.id = Some(id);
        }
        if let Some(kind) = kind {
            builder.kind = Some(kind);
        }
        if let Some(name) = name {
            builder.name = Some(name);
        }
        if let Some(arguments) = arguments {
            builder
                .arguments
                .get_or_insert_with(String::new)
                .push_str(&arguments);
        }
    }

    fn finalize(self) -> Result<Vec<ToolCall>, StreamError> {
        self.by_index
            .into_iter()
            .map(|(index, builder)| {
                let mut missing = Vec::new();
                if builder.id.is_none() {
                    missing.push("id");
                }
                if builder.kind.as_deref() != Some("function") {
                    missing.push("type=function");
                }
                if builder.name.is_none() {
                    missing.push("function.name");
                }
                if builder.arguments.is_none() {
                    missing.push("function.arguments");
                }
                if !missing.is_empty() {
                    return Err(StreamError::IncompleteToolCall {
                        index,
                        missing: missing.join(", "),
                    });
                }
                Ok(ToolCall {
                    id: builder.id.unwrap_or_default(),
                    kind: builder.kind.unwrap_or_else(|| "function".to_owned()),
                    function: ToolCallFunction {
                        name: builder.name.unwrap_or_default(),
                        arguments: builder.arguments.unwrap_or_default(),
                    },
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(s: &str) -> SseBuffer {
        let mut buffer = SseBuffer::new();
        buffer.push_slice(s.as_bytes());
        buffer
    }

    fn push(buffer: &mut SseBuffer, s: &str) {
        buffer.push_slice(s.as_bytes());
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
        let mut buffer = SseBuffer::new();
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
        assert!(
            tc.finalize().expect("complete calls").is_empty(),
            "no tool calls"
        );
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
        assert!(deltas.is_empty(), "buffer retained the partial frame");
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
        let mut buffer = SseBuffer::new();
        buffer.push_slice(&raw_bytes[..split]);
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

        buffer.push_slice(&raw_bytes[split..]);
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
        let calls = tc.finalize().expect("complete calls");
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
        let calls = tc.finalize().expect("complete calls");
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
        let calls = tc.finalize().expect("complete calls");
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
        let mut buffer = SseBuffer::new();
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
        let calls = tc.finalize().expect("complete calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_x");
        assert_eq!(calls[0].function.name, "read_file");
        assert_eq!(calls[0].function.arguments, "{\"path\":\"/tmp/x\"}");
    }

    #[test]
    fn drain_assembles_multiple_tool_calls_from_one_frame() {
        let mut buffer = SseBuffer::new();
        push(&mut buffer, "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"/tmp/a\\\"}\"}},{\"index\":1,\"id\":\"call_b\",\"type\":\"function\",\"function\":{\"name\":\"write_file\",\"arguments\":\"{\\\"path\\\":\\\"/tmp/b\\\"}\"}}]}}]}\n\n");
        push(
            &mut buffer,
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        );

        let mut outcome = StreamOutcome::default();
        let mut tc = ToolCallAccumulator::new();
        let mut ended = false;
        drain_complete_frames(
            &mut buffer,
            &mut outcome,
            &mut tc,
            &mut ended,
            &mut |_| {},
            &mut |_| {},
        )
        .expect("drain ok");
        assert_eq!(outcome.finish_reason.as_deref(), Some("tool_calls"));
        let calls = tc.finalize().expect("complete calls");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "call_a");
        assert_eq!(calls[0].function.name, "read_file");
        assert_eq!(calls[0].function.arguments, r#"{"path":"/tmp/a"}"#);
        assert_eq!(calls[1].id, "call_b");
        assert_eq!(calls[1].function.name, "write_file");
        assert_eq!(calls[1].function.arguments, r#"{"path":"/tmp/b"}"#);
    }

    /// Reasoning-then-content interleave assembles correctly into
    /// separate fields. The reasoning callback fires once per chunk
    /// during the thinking phase, then `End` exactly once when the
    /// first content delta arrives. `full_text` only sees content.
    #[test]
    fn drain_separates_reasoning_from_content_with_boundary_end() {
        let mut buffer = SseBuffer::new();
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
        let mut buffer = SseBuffer::new();
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
