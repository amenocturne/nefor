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
use reqwest::header::{ACCEPT, ACCEPT_ENCODING, CONTENT_TYPE, RETRY_AFTER};
use tokio_util::sync::CancellationToken;

use crate::openai::{
    parse_models_response, parse_sse_chunk, ChatRequest, Message, ModelInfo, ReasoningContinuation,
    ReasoningDetails, SseEvent, StreamOptions, ToolCall, ToolCallFunction, Usage,
};
use crate::wire::WireTrace;

use crate::config::RetryPolicy;

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
    pub completion_id: Option<String>,
    pub full_text: String,
    pub finish_reason: Option<String>,
    pub usage: Option<Usage>,
    pub interrupted: bool,
    pub tool_calls: Vec<ToolCall>,
    pub reasoning_text: String,
    pub reasoning_continuation: Option<ReasoningContinuation>,
}

/// Errors that can come out of the HTTP/SSE pipeline. All of them lower
/// to a single `<prefix>.turn.error` body for the caller; the variant is
/// preserved here for tracing/diagnostics.
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error("{retry}")]
    Exhausted {
        retry: RetryExhaustion,
        source: Box<StreamError>,
    },
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

/// Quota and billing denials carried by HTTP 429 are terminal rather than
/// transient rate limits. Replaying them cannot succeed without an external
/// account change and may multiply billed or budget-capped requests.
fn response_signals_terminal_quota_denial(status: u16, body: &str) -> bool {
    if status != 429 {
        return false;
    }
    let normalized = body.to_ascii_lowercase();
    let names_account_resource = ["quota", "credit", "balance", "budget"]
        .iter()
        .any(|resource| normalized.contains(resource));
    let names_terminal_state = ["exhausted", "depleted", "insufficient"]
        .iter()
        .any(|state| normalized.contains(state));

    (names_account_resource && names_terminal_state)
        || normalized.contains("billing_hard_limit")
        || normalized.contains("billing hard limit")
        || normalized.contains("payment required")
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryPhase {
    Attempt,
    Round,
}

impl RetryPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Attempt => "attempt",
            Self::Round => "round",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryCategory {
    RateLimit,
    Server,
    Transport,
    StreamTransport,
}

impl RetryCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RateLimit => "rate_limit",
            Self::Server => "server",
            Self::Transport => "transport",
            Self::StreamTransport => "stream_transport",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryExhaustionReason {
    AttemptsExhausted,
    RoundsExhausted,
}

impl RetryExhaustionReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AttemptsExhausted => "attempts_exhausted",
            Self::RoundsExhausted => "rounds_exhausted",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryProgress {
    pub phase: RetryPhase,
    pub round: usize,
    pub max_rounds: usize,
    pub next_round: usize,
    pub next_attempt: usize,
    pub total_attempts: usize,
    pub category: RetryCategory,
    pub status: Option<u16>,
    pub failed_attempt: usize,
    pub max_attempts: usize,
    pub next_delay: Duration,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RetryExhaustion {
    pub reason: RetryExhaustionReason,
    pub round: usize,
    pub max_rounds: usize,
    pub attempt: usize,
    pub max_attempts: usize,
    pub total_attempts: usize,
    pub category: RetryCategory,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
}

impl std::fmt::Display for RetryExhaustion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: round {}/{} attempt {}/{}; {}{}; {} sends",
            self.reason.as_str(),
            self.round,
            self.max_rounds,
            self.attempt,
            self.max_attempts,
            self.category.as_str(),
            self.status
                .map(|status| format!(" HTTP {status}"))
                .unwrap_or_default(),
            self.total_attempts
        )
    }
}

impl RetryProgress {
    pub fn fields(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut fields = serde_json::json!({
            "phase": self.phase, "round": self.round, "max_rounds": self.max_rounds,
            "attempt": self.failed_attempt, "max_attempts": self.max_attempts,
            "next_round": self.next_round, "next_attempt": self.next_attempt,
            "total_attempts": self.total_attempts, "delay_ms": self.next_delay.as_millis() as u64,
            "category": self.category,
            "message": format!("{}{}; round {}/{} attempt {}/{}; next round {} attempt {} in {}ms",
                self.category.as_str(), self.status.map(|status| format!(" HTTP {status}")).unwrap_or_default(),
                self.round, self.max_rounds, self.failed_attempt, self.max_attempts,
                self.next_round, self.next_attempt, self.next_delay.as_millis())
        }).as_object().cloned().unwrap_or_default();
        if let Some(status) = self.status {
            fields.insert("status".into(), status.into());
        }
        fields
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
    retry_policy: &RetryPolicy,
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
        retry_policy,
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
    retry_policy: &RetryPolicy,
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
        retry_policy,
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
    retry_policy: &RetryPolicy,
    endpoint: &str,
    api_key: Option<&str>,
    auth_header: &str,
    model: &str,
    messages: &[Message],
    tools: Option<&[serde_json::Value]>,
    reasoning_effort: Option<&str>,
    response_format: Option<&serde_json::Value>,
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
    run_chat_stream_with_retry_progress_and_format_and_additions(
        client,
        retry_policy,
        endpoint,
        api_key,
        auth_header,
        model,
        messages,
        tools,
        reasoning_effort,
        response_format,
        None,
        cancel,
        on_delta,
        on_reasoning,
        on_retry_progress,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn run_chat_stream_with_retry_progress_and_format_and_additions<F, R, P>(
    client: &reqwest::Client,
    retry_policy: &RetryPolicy,
    endpoint: &str,
    api_key: Option<&str>,
    auth_header: &str,
    model: &str,
    messages: &[Message],
    tools: Option<&[serde_json::Value]>,
    reasoning_effort: Option<&str>,
    response_format: Option<&serde_json::Value>,
    request_additions: Option<&serde_json::Map<String, serde_json::Value>>,
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
    run_chat_stream_with_retry_progress_and_format_and_additions_and_wire(
        client,
        retry_policy,
        endpoint,
        api_key,
        auth_header,
        model,
        messages,
        tools,
        reasoning_effort,
        response_format,
        request_additions,
        cancel,
        on_delta,
        on_reasoning,
        on_retry_progress,
        WireTrace::from_env(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn run_chat_stream_with_retry_progress_and_format_and_wire<F, R, P>(
    client: &reqwest::Client,
    retry_policy: &RetryPolicy,
    endpoint: &str,
    api_key: Option<&str>,
    auth_header: &str,
    model: &str,
    messages: &[Message],
    tools: Option<&[serde_json::Value]>,
    reasoning_effort: Option<&str>,
    response_format: Option<&serde_json::Value>,
    cancel: CancellationToken,
    on_delta: F,
    on_reasoning: R,
    on_retry_progress: P,
    wire: WireTrace,
) -> Result<StreamOutcome, StreamError>
where
    F: FnMut(&str),
    R: FnMut(ReasoningEvent<'_>),
    P: FnMut(RetryProgress),
{
    run_chat_stream_with_retry_progress_and_format_and_additions_and_wire(
        client,
        retry_policy,
        endpoint,
        api_key,
        auth_header,
        model,
        messages,
        tools,
        reasoning_effort,
        response_format,
        None,
        cancel,
        on_delta,
        on_reasoning,
        on_retry_progress,
        wire,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn run_chat_stream_with_retry_progress_and_format_and_additions_and_wire<F, R, P>(
    client: &reqwest::Client,
    retry_policy: &RetryPolicy,
    endpoint: &str,
    api_key: Option<&str>,
    auth_header: &str,
    model: &str,
    messages: &[Message],
    tools: Option<&[serde_json::Value]>,
    reasoning_effort: Option<&str>,
    response_format: Option<&serde_json::Value>,
    request_additions: Option<&serde_json::Map<String, serde_json::Value>>,
    cancel: CancellationToken,
    mut on_delta: F,
    mut on_reasoning: R,
    mut on_retry_progress: P,
    mut wire: WireTrace,
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

    let request_value = req
        .into_value_with_additions(request_additions)
        .map_err(|error| StreamError::Request(error.to_string()))?;
    let body_json = serde_json::to_string(&request_value)
        .map_err(|error| StreamError::Request(error.to_string()))?;
    if tracing::enabled!(tracing::Level::INFO) {
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
    wire.begin("POST", endpoint);

    let mut total_attempts = 0;
    for round in 1..=retry_policy.max_rounds {
        let mut saw_transient_429 = false;
        let mut server_not_before: Option<tokio::time::Instant> = None;
        for attempt in 1..=retry_policy.max_attempts {
            if cancel.is_cancelled() {
                return Ok(interrupted(&mut wire, total_attempts));
            }
            total_attempts += 1;
            wire.request(total_attempts, "POST", endpoint, &body_json);
            let mut builder = client
                .post(endpoint)
                .header(ACCEPT, "text/event-stream")
                .header(ACCEPT_ENCODING, "identity")
                .json(&request_value);
            if let Some(key) = api_key {
                builder = apply_auth(builder, auth_header, key);
            }
            let sent = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Ok(interrupted(&mut wire, total_attempts)),
                sent = tokio::time::timeout(Duration::from_millis(retry_policy.response_headers_timeout_ms), builder.send()) => match sent {
                    Ok(Ok(response)) => Ok(response),
                    Ok(Err(error)) => Err(StreamError::Request(reqwest_error_detail(&error))),
                    Err(_) => Err(StreamError::Request(format!("timed out waiting for response headers after {}ms", retry_policy.response_headers_timeout_ms))),
                }
            };
            let mut received_retry_after = None;
            let (error, category, status) = match sent {
                Err(error) => (error, RetryCategory::Transport, None),
                Ok(response) => {
                    let status = response.status().as_u16();
                    let received_at = tokio::time::Instant::now();
                    received_retry_after =
                        retry_after_delay(response.headers(), std::time::SystemTime::now())
                            .map(|delay| (received_at, delay));
                    wire.response(total_attempts, status, response.headers());
                    let content_type = response
                        .headers()
                        .get(CONTENT_TYPE)
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("<missing>")
                        .to_owned();
                    if response.status().is_success()
                        && content_type.split(';').next().is_some_and(|media| {
                            media.trim().eq_ignore_ascii_case("text/event-stream")
                        })
                    {
                        match consume_response_stream(
                            response,
                            total_attempts,
                            &cancel,
                            &mut on_delta,
                            &mut on_reasoning,
                            &mut wire,
                        )
                        .await
                        {
                            Ok(outcome) => return Ok(outcome),
                            Err(failure) if failure.replay_safe => {
                                (failure.error, RetryCategory::StreamTransport, None)
                            }
                            Err(failure) => {
                                wire.error(Some(total_attempts), failure.error.to_string());
                                return Err(failure.error);
                            }
                        }
                    } else {
                        let success = response.status().is_success();
                        let body_bytes = tokio::select! {
                            biased;
                            _ = cancel.cancelled() => return Ok(interrupted(&mut wire, total_attempts)),
                            bytes = response.bytes() => bytes.map(|bytes| bytes.to_vec()).unwrap_or_else(|_| b"<unreadable response body>".to_vec()),
                        };
                        wire.response_body(total_attempts, &body_bytes);
                        let body = String::from_utf8_lossy(&body_bytes).into_owned();
                        if success {
                            let error = StreamError::Malformed(format!(
                                "expected streaming SSE response with Content-Type text/event-stream, got {content_type:?}: {}",
                                response_snippet(&body)
                            ));
                            wire.error(Some(total_attempts), error.to_string());
                            return Err(error);
                        }
                        if status == 401 {
                            wire.error(Some(total_attempts), "HTTP 401".into());
                            return Err(StreamError::Unauthorized { body });
                        }
                        if status == 400 && tools.is_some() && body_signals_tools_unsupported(&body)
                        {
                            wire.error(Some(total_attempts), "HTTP 400 tools unsupported".into());
                            return Err(StreamError::ToolsUnsupported { body });
                        }
                        if response_signals_terminal_quota_denial(status, &body)
                            || !(status == 429 || (500..600).contains(&status))
                        {
                            wire.error(Some(total_attempts), format!("HTTP {status}"));
                            return Err(StreamError::Http { status, body });
                        }
                        saw_transient_429 |= status == 429;
                        (
                            StreamError::Http { status, body },
                            if status == 429 {
                                RetryCategory::RateLimit
                            } else {
                                RetryCategory::Server
                            },
                            Some(status),
                        )
                    }
                }
            };
            if cancel.is_cancelled() {
                return Ok(interrupted(&mut wire, total_attempts));
            }
            wire.error(Some(total_attempts), error.to_string());
            let exhausted = attempt == retry_policy.max_attempts;
            let phase = if exhausted {
                RetryPhase::Round
            } else {
                RetryPhase::Attempt
            };
            let metadata = RetryExhaustion {
                reason: if exhausted && saw_transient_429 {
                    RetryExhaustionReason::RoundsExhausted
                } else {
                    RetryExhaustionReason::AttemptsExhausted
                },
                round,
                max_rounds: retry_policy.max_rounds,
                attempt,
                max_attempts: retry_policy.max_attempts,
                total_attempts,
                category,
                status,
            };
            if exhausted && (!saw_transient_429 || round == retry_policy.max_rounds) {
                return Err(StreamError::Exhausted {
                    retry: metadata,
                    source: Box::new(error),
                });
            }
            if let Some((received_at, delay)) = received_retry_after {
                // A valid but unrepresentable deadline must fail closed,
                // not silently permit a send before the server's deadline.
                let deadline = received_at
                    .checked_add(delay)
                    .filter(|_| delay.as_millis() <= u128::from(u64::MAX))
                    .ok_or_else(|| {
                        StreamError::Malformed("Retry-After deadline is not representable".into())
                    })?;
                server_not_before =
                    Some(server_not_before.map_or(deadline, |old| old.max(deadline)));
            }
            let local_delay = retry_policy.local_delay(attempt, exhausted, random_sample());
            let remaining = server_not_before
                .map(|deadline| deadline.saturating_duration_since(tokio::time::Instant::now()))
                .unwrap_or_default();
            let delay = local_delay.max(remaining);
            on_retry_progress(RetryProgress {
                phase,
                round,
                max_rounds: retry_policy.max_rounds,
                failed_attempt: attempt,
                max_attempts: retry_policy.max_attempts,
                next_round: if exhausted { round + 1 } else { round },
                next_attempt: if exhausted { 1 } else { attempt + 1 },
                total_attempts,
                category,
                status,
                next_delay: delay,
            });
            wire.retry(
                total_attempts,
                format!("phase={} delay_ms={}", phase.as_str(), delay.as_millis()),
            );
            if !sleep_before_retry(&cancel, delay).await {
                return Ok(interrupted(&mut wire, total_attempts));
            }
        }
    }
    unreachable!("validated nonzero retry rounds always return a result")
}

fn interrupted(wire: &mut WireTrace, attempt: usize) -> StreamOutcome {
    wire.cancel((attempt > 0).then_some(attempt));
    StreamOutcome {
        interrupted: true,
        ..Default::default()
    }
}

fn random_sample() -> u64 {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[10], bytes[11],
    ])
}

impl RetryPolicy {
    fn exponential_delay_ms(&self, attempt: usize) -> u64 {
        if self.initial_delay_ms == 0 {
            return 0;
        }
        let cap = self.max_delay_ms as f64;
        // Capped exponentiation by squaring bounds work even for usize::MAX attempts.
        // Check before multiplication so neither large factors nor exponents overflow.
        let multiply = |left: f64, right: f64| {
            if left >= cap / right {
                cap
            } else {
                left * right
            }
        };
        let mut delay = self.initial_delay_ms as f64;
        let mut factor = self.backoff_multiplier.min(cap);
        let mut exponent = attempt.saturating_sub(1);
        while exponent > 0 {
            if exponent % 2 == 1 {
                delay = multiply(delay, factor);
            }
            exponent /= 2;
            if exponent > 0 {
                factor = multiply(factor, factor);
            }
        }
        // Millisecond timers truncate fractional results; retain the exact integer cap.
        (delay as u64).min(self.max_delay_ms)
    }

    fn local_delay(&self, attempt: usize, outer: bool, sample: u64) -> Duration {
        let base = if outer {
            self.round_cooldown_ms
        } else {
            self.exponential_delay_ms(attempt)
        };
        // Divide before multiplying the sample so even the largest admitted timer cannot overflow.
        let scaled = u128::from(base) * u128::from(self.jitter_bps);
        let sample = u128::from(sample);
        let denominator = u128::from(u64::MAX);
        let product = (scaled / 10000) * sample;
        let jitter = product / denominator
            + ((product % denominator) * 10000 + (scaled % 10000) * sample) / (denominator * 10000);
        let delay = u128::from(base) + jitter;
        Duration::from_millis(if outer {
            delay as u64
        } else {
            delay.min(u128::from(self.max_delay_ms)) as u64
        })
    }
}

struct StreamAttemptFailure {
    error: StreamError,
    replay_safe: bool,
}

impl StreamAttemptFailure {
    fn new(error: StreamError, outcome: &StreamOutcome, tool_calls: &ToolCallAccumulator) -> Self {
        let replay_safe = matches!(error, StreamError::Body(_))
            && outcome.full_text.is_empty()
            && outcome.reasoning_text.is_empty()
            && outcome.reasoning_continuation.is_none()
            && tool_calls.is_empty();
        Self { error, replay_safe }
    }
}

async fn consume_response_stream<F, R>(
    response: reqwest::Response,
    successful_attempt: usize,
    cancel: &CancellationToken,
    on_delta: &mut F,
    on_reasoning: &mut R,
    wire: &mut WireTrace,
) -> Result<StreamOutcome, StreamAttemptFailure>
where
    F: FnMut(&str),
    R: FnMut(ReasoningEvent<'_>),
{
    let mut outcome = StreamOutcome::default();
    let mut buffer = SseBuffer::new();
    let mut tool_calls = ToolCallAccumulator::new();
    let mut reasoning_ended = false;
    let mut byte_stream = response.bytes_stream();

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                wire.cancel(Some(successful_attempt));
                outcome.interrupted = true;
                outcome.tool_calls = tool_calls.finalize().unwrap_or_default();
                maybe_end_reasoning(&outcome, &mut reasoning_ended, on_reasoning);
                return Ok(outcome);
            }
            next = byte_stream.next() => {
                match next {
                    None => break,
                    Some(Err(error)) => {
                        if stream_semantically_complete(&outcome) && buffer.is_empty() {
                            break;
                        }
                        return Err(StreamAttemptFailure::new(
                            StreamError::Body(reqwest_error_detail(&error)),
                            &outcome,
                            &tool_calls,
                        ));
                    }
                    Some(Ok(bytes)) => {
                        wire.stream_data(successful_attempt, &bytes);
                        buffer.push(&bytes);
                        if let Err(error) = drain_complete_frames(
                            &mut buffer,
                            &mut outcome,
                            &mut tool_calls,
                            &mut reasoning_ended,
                            on_delta,
                            on_reasoning,
                        ) {
                            return Err(StreamAttemptFailure::new(error, &outcome, &tool_calls));
                        }
                    }
                }
            }
        }
    }

    if let Err(error) = drain_complete_frames(
        &mut buffer,
        &mut outcome,
        &mut tool_calls,
        &mut reasoning_ended,
        on_delta,
        on_reasoning,
    ) {
        return Err(StreamAttemptFailure::new(error, &outcome, &tool_calls));
    }
    if outcome.reasoning_continuation.is_some() && !stream_semantically_complete(&outcome) {
        return Err(StreamAttemptFailure::new(
            StreamError::Body("native reasoning stream ended before finish_reason".to_owned()),
            &outcome,
            &tool_calls,
        ));
    }
    outcome.tool_calls = match tool_calls.finalize() {
        Ok(tool_calls) => tool_calls,
        Err(error) => {
            return Err(StreamAttemptFailure {
                error,
                replay_safe: false,
            });
        }
    };
    maybe_end_reasoning(&outcome, &mut reasoning_ended, on_reasoning);
    wire.end(Some(successful_attempt), "stream_complete");
    Ok(outcome)
}

fn stream_semantically_complete(outcome: &StreamOutcome) -> bool {
    outcome.finish_reason.is_some()
}

fn response_snippet(body: &str) -> String {
    const LIMIT: usize = 240;
    let mut snippet = body.chars().take(LIMIT).collect::<String>();
    if body.chars().count() > LIMIT {
        snippet.push('…');
    }
    snippet
}

fn retry_after_delay(
    headers: &reqwest::header::HeaderMap,
    now: std::time::SystemTime,
) -> Option<Duration> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
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
    let mut wire = WireTrace::from_env();
    wire.begin("GET", &endpoint);
    wire.request(1, "GET", &endpoint, "");
    let mut builder = client.get(&endpoint).timeout(Duration::from_secs(30));
    if let Some(k) = api_key {
        builder = apply_auth(builder, auth_header, k);
    }
    let response = match builder.send().await {
        Ok(response) => response,
        Err(error) => {
            let error = StreamError::Request(error.to_string());
            wire.error(Some(1), error.to_string());
            return Err(error);
        }
    };
    let status = response.status().as_u16();
    wire.response(1, status, response.headers());

    if !response.status().is_success() {
        let body_bytes = match response.bytes().await {
            Ok(bytes) => bytes.to_vec(),
            Err(_) => b"<unreadable response body>".to_vec(),
        };
        wire.response_body(1, &body_bytes);
        let body = String::from_utf8_lossy(&body_bytes).into_owned();
        let error = if status == 401 {
            StreamError::Unauthorized { body }
        } else {
            StreamError::Http { status, body }
        };
        wire.error(Some(1), error.to_string());
        return Err(error);
    }
    let body_bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(error) => {
            let error = StreamError::Body(error.to_string());
            wire.error(Some(1), error.to_string());
            return Err(error);
        }
    };
    wire.response_body(1, &body_bytes);
    wire.end(Some(1), "response_complete");
    Ok(parse_models_response(&String::from_utf8_lossy(&body_bytes)))
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
        SseEvent::CompletionId(id) => match &outcome.completion_id {
            Some(existing) if existing != &id => {
                return Err(StreamError::Malformed(format!(
                    "provider completion id changed from {existing:?} to {id:?}"
                )));
            }
            Some(_) => {}
            None => outcome.completion_id = Some(id),
        },
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
            match &mut outcome.reasoning_continuation {
                Some(ReasoningContinuation::Details { .. }) => {}
                Some(ReasoningContinuation::Content { reasoning_content })
                | Some(ReasoningContinuation::Reasoning {
                    reasoning: reasoning_content,
                }) => {
                    reasoning_content.push_str(&text);
                }
                None => {
                    outcome.reasoning_continuation = Some(ReasoningContinuation::Reasoning {
                        reasoning: text.clone(),
                    });
                }
            }
            on_reasoning(ReasoningEvent::Delta(&text));
        }
        SseEvent::ReasoningContentDelta(text) => {
            outcome.reasoning_text.push_str(&text);
            if !matches!(
                outcome.reasoning_continuation,
                Some(ReasoningContinuation::Details { .. })
            ) {
                let mut accumulated = match outcome.reasoning_continuation.take() {
                    Some(ReasoningContinuation::Content { reasoning_content })
                    | Some(ReasoningContinuation::Reasoning {
                        reasoning: reasoning_content,
                    }) => reasoning_content,
                    _ => String::new(),
                };
                accumulated.push_str(&text);
                outcome.reasoning_continuation = Some(ReasoningContinuation::Content {
                    reasoning_content: accumulated,
                });
            }
            on_reasoning(ReasoningEvent::Delta(&text));
        }
        SseEvent::ReasoningDetails(details) => {
            if let Some(ReasoningContinuation::Details { reasoning_details }) =
                &mut outcome.reasoning_continuation
            {
                reasoning_details.append_chunks(details);
            } else {
                outcome.reasoning_continuation = ReasoningDetails::from_chunks(details)
                    .map(|reasoning_details| ReasoningContinuation::Details { reasoning_details });
            }
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
        SseEvent::Usage(usage) => match &outcome.usage {
            Some(existing) if existing != &usage => {
                return Err(StreamError::Malformed(
                    "provider emitted conflicting usage payloads".to_owned(),
                ));
            }
            Some(_) => {}
            None => outcome.usage = Some(usage),
        },
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

    fn is_empty(&self) -> bool {
        self.by_index.is_empty()
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

    fn selected_policy() -> RetryPolicy {
        RetryPolicy::from_json(r#"{"max_attempts":8,"max_rounds":6,"backoff_multiplier":2,"initial_delay_ms":500,"max_delay_ms":30000,"round_cooldown_ms":60000,"jitter_bps":1000,"response_headers_timeout_ms":120000}"#).unwrap()
    }

    #[test]
    fn configured_exponential_multiplier_and_saturation() {
        let mut policy = selected_policy();
        policy.jitter_bps = 0;
        for (factor, expected) in [
            (1.0, [500, 500, 500, 500]),
            (1.5, [500, 750, 1125, 1687]),
            (3.0, [500, 1500, 4500, 13500]),
        ] {
            policy.backoff_multiplier = factor;
            for (index, delay) in expected.into_iter().enumerate() {
                assert_eq!(
                    policy.local_delay(index + 1, false, 0),
                    Duration::from_millis(delay)
                );
            }
        }
        for factor in [1.5, 3.0, f64::MAX] {
            policy.backoff_multiplier = factor;
            assert_eq!(policy.exponential_delay_ms(usize::MAX), 30000);
        }
        policy.initial_delay_ms = 0;
        assert_eq!(policy.exponential_delay_ms(usize::MAX), 0);
        policy.backoff_multiplier = 1.0;
        policy.initial_delay_ms = 500;
        assert_eq!(policy.exponential_delay_ms(usize::MAX), 500);
    }

    #[test]
    fn local_delay_has_additive_jitter_caps_and_independent_outer_cooldown() {
        let policy = selected_policy();
        assert_eq!(policy.local_delay(1, false, 0), Duration::from_millis(500));
        assert_eq!(
            policy.local_delay(1, false, u64::MAX),
            Duration::from_millis(550)
        );
        assert_eq!(
            policy.local_delay(2, false, u64::MAX),
            Duration::from_millis(1100)
        );
        assert_eq!(
            policy.local_delay(8, false, u64::MAX),
            Duration::from_millis(30000)
        );
        assert_eq!(
            policy.local_delay(usize::MAX, false, u64::MAX),
            Duration::from_millis(30000)
        );
        assert_eq!(policy.local_delay(8, true, 0), Duration::from_millis(60000));
        assert_eq!(
            policy.local_delay(8, true, u64::MAX),
            Duration::from_millis(66000)
        );
        let mut no_jitter = policy.clone();
        no_jitter.jitter_bps = 0;
        assert_eq!(
            no_jitter.local_delay(1, false, u64::MAX),
            Duration::from_millis(500)
        );
        assert_eq!(
            no_jitter.local_delay(8, true, u64::MAX),
            Duration::from_millis(60000)
        );
        // Fractional basis-point products are rounded only after sampling.
        let mut fractional = policy;
        fractional.initial_delay_ms = 19;
        fractional.jitter_bps = 1000;
        assert_eq!(
            fractional.local_delay(1, false, u64::MAX / 4 * 3),
            Duration::from_millis(20)
        );
    }

    #[test]
    fn retry_after_accepts_seconds_and_dates_but_does_not_define_local_delay() {
        let mut headers = reqwest::header::HeaderMap::new();
        let now = std::time::UNIX_EPOCH + Duration::from_secs(1700000000);
        for (text, expected) in [
            ("0", Some(Duration::ZERO)),
            ("120", Some(Duration::from_secs(120))),
            ("bad", None),
            ("-1", None),
            ("Wed, 21 Oct 2015 07:28:00 GMT", Some(Duration::ZERO)),
        ] {
            headers.insert(RETRY_AFTER, text.parse().unwrap());
            assert_eq!(retry_after_delay(&headers, now), expected);
        }
        let future = now + Duration::from_secs(120);
        headers.insert(
            RETRY_AFTER,
            httpdate::fmt_http_date(future).parse().unwrap(),
        );
        let delay = retry_after_delay(&headers, now).unwrap();
        assert_eq!(delay, Duration::from_secs(120));
    }

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
    fn terminal_quota_denial_is_distinct_from_transient_retries() {
        for body in [
            r#"{"error":{"type":"insufficient_quota"}}"#,
            r#"{"error":"Your budget has been exhausted"}"#,
            r#"{"error":"Credit balance depleted"}"#,
            r#"{"error":"Insufficient balance"}"#,
            r#"{"error":"Billing hard limit reached"}"#,
            r#"{"error":"Payment required"}"#,
        ] {
            assert!(
                response_signals_terminal_quota_denial(429, body),
                "expected terminal denial for {body}"
            );
            assert!(
                !response_signals_terminal_quota_denial(500, body),
                "5xx must remain retriable for {body}"
            );
        }
        assert!(!response_signals_terminal_quota_denial(
            429,
            r#"{"error":"rate limited"}"#
        ));
    }

    #[test]
    fn sse_transport_replay_requires_an_empty_attempt() {
        let body_error = || StreamError::Body("connection reset".to_owned());
        let empty = StreamOutcome::default();
        let no_tools = ToolCallAccumulator::new();
        assert!(StreamAttemptFailure::new(body_error(), &empty, &no_tools).replay_safe);

        let with_text = StreamOutcome {
            full_text: "partial".to_owned(),
            ..Default::default()
        };
        assert!(!StreamAttemptFailure::new(body_error(), &with_text, &no_tools).replay_safe);

        let with_reasoning = StreamOutcome {
            reasoning_text: "partial".to_owned(),
            ..Default::default()
        };
        assert!(!StreamAttemptFailure::new(body_error(), &with_reasoning, &no_tools).replay_safe);

        let mut with_tools = ToolCallAccumulator::new();
        with_tools.apply(0, Some("call".to_owned()), None, None, None);
        assert!(!StreamAttemptFailure::new(body_error(), &empty, &with_tools).replay_safe);
        assert!(
            !StreamAttemptFailure::new(
                StreamError::Malformed("bad frame".to_owned()),
                &empty,
                &no_tools,
            )
            .replay_safe
        );
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
        assert_eq!(u.prompt_tokens, Some(3));
        assert_eq!(u.completion_tokens, Some(2));
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

    // The source a model sent with a `mag-apply` call in a recorded session.
    // It names a `.mag` suffix inside a shell quote, next to the call's own
    // `file` argument.
    const MAG_APPLY_CONTENT: &str = include_str!("../tests/fixtures/mag_apply_locate_content.mag");

    fn mag_apply_arguments(file_first: bool) -> String {
        let file = serde_json::to_string("locate.mag").unwrap();
        let content = serde_json::to_string(MAG_APPLY_CONTENT).unwrap();
        if file_first {
            format!(r#"{{"file": {file}, "content": {content}}}"#)
        } else {
            format!(r#"{{"content": {content}, "file": {file}}}"#)
        }
    }

    fn frame(chunk: serde_json::Value) -> String {
        format!("data: {chunk}\n\n")
    }

    fn argument_frame(index: usize, arguments: &str) -> String {
        frame(
            serde_json::json!({"id": "gen-1", "provider": "Upstream", "choices": [{"index": 0,
            "delta": {"role": "assistant", "content": "", "tool_calls": [
                {"index": index, "function": {"arguments": arguments}}]},
            "finish_reason": null}]}),
        )
    }

    fn start_frame(index: usize, id: &str, arguments: &str) -> String {
        frame(serde_json::json!({"id": "gen-1", "choices": [{"index": 0,
            "delta": {"role": "assistant", "content": null, "tool_calls": [
                {"index": index, "id": id, "type": "function",
                 "function": {"name": "mag-apply", "arguments": arguments}}]},
            "finish_reason": null}]}))
    }

    fn finish_frame() -> String {
        frame(serde_json::json!({"id": "gen-1",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}))
    }

    /// Feed a whole SSE body to the parser in transport pieces of `piece`
    /// bytes and return the assembled calls.
    fn assemble(sse: &str, piece: usize) -> Vec<ToolCall> {
        let mut buffer = SseBuffer::new();
        let mut outcome = StreamOutcome::default();
        let mut tc = ToolCallAccumulator::new();
        let mut ended = false;
        for part in sse.as_bytes().chunks(piece.max(1)) {
            buffer.push_slice(part);
            drain_complete_frames(
                &mut buffer,
                &mut outcome,
                &mut tc,
                &mut ended,
                &mut |_| {},
                &mut |_| {},
            )
            .expect("drain ok");
        }
        assert_eq!(outcome.finish_reason.as_deref(), Some("tool_calls"));
        tc.finalize().expect("complete calls")
    }

    fn assert_locate(call: &ToolCall, expected: &str) {
        assert_eq!(call.function.name, "mag-apply");
        assert_eq!(call.function.arguments, expected);
        let parsed: serde_json::Value = serde_json::from_str(&call.function.arguments).unwrap();
        assert_eq!(parsed["file"], "locate.mag");
        assert_eq!(parsed["content"], MAG_APPLY_CONTENT);
    }

    #[test]
    fn tool_call_arguments_equal_their_fragments_in_one_chunk() {
        for file_first in [true, false] {
            let arguments = mag_apply_arguments(file_first);
            let sse = start_frame(0, "call_1", &arguments) + &finish_frame() + "data: [DONE]\n\n";
            for piece in [usize::MAX, 1, 7, 64] {
                assert_locate(&assemble(&sse, piece)[0], &arguments);
            }
        }
    }

    #[test]
    fn tool_call_arguments_equal_their_fragments_split_at_every_position() {
        for file_first in [true, false] {
            let arguments = mag_apply_arguments(file_first);
            for at in (0..=arguments.len()).filter(|at| arguments.is_char_boundary(*at)) {
                let (head, tail) = arguments.split_at(at);
                // The name and id arrive first with empty arguments, as
                // OpenRouter sends them, then two argument fragments.
                let sse = start_frame(0, "call_1", "")
                    + &argument_frame(0, head)
                    + &argument_frame(0, tail)
                    + &finish_frame();
                assert_locate(&assemble(&sse, usize::MAX)[0], &arguments);
                // The whole arguments in the first chunk's own delta, then the rest.
                let sse =
                    start_frame(0, "call_1", head) + &argument_frame(0, tail) + &finish_frame();
                assert_locate(&assemble(&sse, 13)[0], &arguments);
            }
        }
    }

    #[test]
    fn tool_call_arguments_keep_fragments_that_repeat_earlier_text() {
        // One character per fragment makes many fragments equal to an earlier
        // one, and `.mag` itself arrives twice as a fragment of its own.
        let arguments = mag_apply_arguments(false);
        let mut sse = start_frame(0, "call_1", "");
        for character in arguments.chars() {
            sse += &argument_frame(0, &character.to_string());
        }
        sse += &finish_frame();
        assert_locate(&assemble(&sse, usize::MAX)[0], &arguments);

        let mut sse = start_frame(0, "call_1", "");
        let mut rest = arguments.as_str();
        while let Some(at) = rest.find(".mag") {
            sse += &argument_frame(0, &rest[..at]);
            sse += &argument_frame(0, ".mag");
            rest = &rest[at + ".mag".len()..];
        }
        sse += &argument_frame(0, rest);
        sse += &finish_frame();
        assert_locate(&assemble(&sse, usize::MAX)[0], &arguments);
    }

    #[test]
    fn tool_call_arguments_survive_interleaved_reasoning_usage_and_empty_deltas() {
        let arguments = mag_apply_arguments(true);
        let (head, tail) = arguments.split_at(arguments.find("locate").unwrap());
        let (name, tail) = tail.split_at("locate".len());
        let reasoning = frame(serde_json::json!({"id": "gen-1", "choices": [{"index": 0,
            "delta": {"reasoning": "Let me use locate.mag.", "reasoning_details": [
                {"type": "reasoning.text", "text": "Let me use locate.mag.", "index": 0}]}}]}));
        let null_usage = frame(serde_json::json!({"id": "gen-1", "choices": [], "usage": null}));
        // A fragment that carries reasoning and an empty-name tool-call delta
        // beside the argument text.
        let mixed = frame(serde_json::json!({"id": "gen-1", "choices": [{"index": 0,
            "delta": {"content": "", "reasoning": "", "reasoning_details": [], "tool_calls": [
                {"index": 0, "id": "call_1", "type": "function",
                 "function": {"name": "mag-apply", "arguments": name}}]}}]}));
        let sse = reasoning.clone()
            + &start_frame(0, "call_1", "")
            + ": OPENROUTER PROCESSING\n\n"
            + &argument_frame(0, head)
            + &null_usage
            + &mixed
            + &argument_frame(0, "")
            + &argument_frame(0, tail)
            + &finish_frame()
            + &frame(serde_json::json!({"id": "gen-1", "choices": [],
                "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3}}))
            + "data: [DONE]\n\n";
        for piece in [usize::MAX, 1, 5] {
            assert_locate(&assemble(&sse, piece)[0], &arguments);
        }
    }

    #[test]
    fn two_tool_calls_keep_their_own_interleaved_fragments() {
        let first = mag_apply_arguments(true);
        let second = mag_apply_arguments(false);
        let mut sse = start_frame(0, "call_1", "") + &start_frame(1, "call_2", "");
        let (mut left, mut right) = (first.as_str(), second.as_str());
        while !left.is_empty() || !right.is_empty() {
            let take = |text: &mut &str, count: usize| {
                let at = (count..=text.len())
                    .find(|at| text.is_char_boundary(*at))
                    .unwrap_or(text.len())
                    .min(text.len());
                let (head, tail) = text.split_at(at);
                *text = tail;
                head.to_owned()
            };
            sse += &argument_frame(0, &take(&mut left, 3));
            sse += &argument_frame(1, &take(&mut right, 5));
        }
        sse += &finish_frame();
        let calls = assemble(&sse, usize::MAX);
        assert_eq!(calls.len(), 2);
        assert_eq!(
            (calls[0].id.as_str(), calls[1].id.as_str()),
            ("call_1", "call_2")
        );
        assert_locate(&calls[0], &first);
        assert_locate(&calls[1], &second);
    }

    #[test]
    fn the_finish_chunk_may_carry_the_last_argument_fragment() {
        let arguments = mag_apply_arguments(true);
        let (head, tail) = arguments.split_at(arguments.find(".mag").unwrap());
        let last = frame(serde_json::json!({"id": "gen-1", "choices": [{"index": 0,
            "delta": {"tool_calls": [{"index": 0, "function": {"arguments": tail}}]},
            "finish_reason": "tool_calls"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3}}));
        let sse = start_frame(0, "call_1", head) + &last + "data: [DONE]\n\n";
        assert_locate(&assemble(&sse, usize::MAX)[0], &arguments);
    }
}
