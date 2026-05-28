//! HTTP + SSE client for the OpenAI Responses endpoint
//! (`https://chatgpt.com/backend-api/codex/responses`).
//!
//! Phase 3 surface area is **library code only**: types for the
//! request body, a header builder, a streaming HTTP client, and a
//! typed SSE event enum. NCP wiring and the chat event-loop land in
//! Phase 4.
//!
//! ```ignore
//! use chatgpt_provider::responses::{ResponsesClient, ResponsesApiRequest};
//!
//! let client = ResponsesClient::new(
//!     "https://chatgpt.com/backend-api/codex".into(),
//!     installation_id,
//!     "nefor_cli_rs".into(),
//! );
//! let mut stream = client.stream(&request, &auth_snapshot).await?;
//! while let Some(event) = stream.next().await { ... }
//! ```

pub mod headers;
pub mod request;
pub mod stream;

use std::error::Error as _;
use std::pin::Pin;
use std::time::{Duration, Instant};

use futures::stream::StreamExt;
use rand::Rng;

pub use headers::{build_headers, default_user_agent};
pub use nefor_sse::SseBuffer;
pub use request::{
    MessageContent, Reasoning, ReasoningEffort, ReasoningSummary, ReasoningSummaryPart,
    ResponseItem, ResponsesApiRequest, TextControls, Verbosity,
};
pub use stream::{parse_sse_frame, ResponseEvent, ResponseStream};

use serde::Deserialize;

use crate::auth::AuthSnapshot;
use crate::error::ChatgptError;

/// Minimal subset of the model metadata returned by
/// `GET /models`. The real `ModelInfo` codex defines has 30+ fields;
/// nefor only needs the user-facing identity, ordering hints, and the
/// capability bits we actually act on (reasoning today; more later).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ModelEntry {
    pub slug: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Lower values rank earlier in codex's UI. Optional because the
    /// backend may omit it for some tiers.
    #[serde(default)]
    pub priority: Option<i32>,
    /// Whether this model accepts the `reasoning.summary` parameter on
    /// the Responses endpoint. The Codex backend is the authoritative
    /// source — some gpt-5 family slugs (`gpt-5.3-codex-spark`) reject
    /// it even though the slug prefix would suggest support. Default
    /// `false` so an unknown / missing field never tries to send the
    /// param: the worst case is a model that COULD reason without a
    /// summary visible, which is strictly safer than a 400.
    #[serde(default)]
    pub supports_reasoning_summaries: bool,
    /// Context window size if the backend reports it.
    #[serde(default, alias = "max_input_tokens", alias = "context_window")]
    pub context_length: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ModelsResponse {
    models: Vec<ModelEntry>,
}

/// Default base URL for the ChatGPT-subscription Responses endpoint.
/// The full URL is `{base}/responses`. Pulled into a constant so tests
/// can override.
pub const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

/// Originator string emitted on the `originator` header. Codex uses
/// `codex_cli_rs`; nefor uses its own value so server-side analytics
/// can distinguish callers.
pub const DEFAULT_ORIGINATOR: &str = "nefor_cli_rs";

/// `client_version` query param sent on `/models`. The backend gates
/// each model on a `minimal_client_version`, so sending our actual
/// crate version (`0.1.0`) gets back only the oldest model. We instead
/// claim parity with a current codex CLI release so the model list
/// matches what `codex --model` shows on the same account. Bump this
/// alongside codex's releases.
pub const CODEX_COMPAT_CLIENT_VERSION: &str = "0.130.0";

/// Retry policy for the initial POST to `/responses` and `/models`.
/// Codex's cloud edge (Envoy) does its own short internal retries and
/// then returns 503 — without a second layer of retries on our side a
/// single momentary backend hiccup propagates straight to the user as
/// `chat.error` and ends the turn.
///
/// Only the *initial* HTTP exchange is retried. Once the SSE stream
/// has yielded bytes, mid-stream failures route through a different
/// code path (`ResponsesStreamRead`) and are only recoverable by the
/// dispatcher before any visible output has been emitted.
///
/// Budget-driven (not attempt-count-driven): keep retrying as long as
/// the next backoff would still fit inside the 5-minute window. Backoff
/// doubles (1s → 2s → 4s → … → 30s cap), so the schedule is roughly
/// `[1, 2, 4, 8, 16, 30, 30, 30, ...]` and gives up after ~10 attempts
/// once the cap dominates. Cancellation is handled by the caller
/// dropping the task — `tokio::time::sleep` is cancellation-safe.
const RETRY_BUDGET_MS: u64 = 5 * 60 * 1_000;
const RETRY_BASE_DELAY_MS: u64 = 1_000;
const RETRY_MAX_DELAY_MS: u64 = 30_000;
const RETRY_JITTER_MS: i64 = 500;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Time-to-first-byte cap per attempt. The SSE stream is unbounded once
/// headers arrive — this only covers the window from "request sent" to
/// "response headers received". A 16-minute hang (observed in production
/// via a proxy/VPN layer) is the failure mode this prevents.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// HTTP client for the Responses endpoint.
///
/// One instance is reused across requests — `reqwest::Client` is
/// already an `Arc` under the hood, so cloning is cheap if a caller
/// needs to share it.
#[derive(Debug, Clone)]
pub struct ResponsesClient {
    http: reqwest::Client,
    base_url: String,
    installation_id: String,
    originator: String,
}

impl ResponsesClient {
    pub fn new(base_url: String, installation_id: String, originator: String) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            http,
            base_url,
            installation_id,
            originator,
        }
    }

    /// Variant that lets the caller bring their own `reqwest::Client`
    /// (useful for shared connection pools and middleware).
    pub fn with_http(
        http: reqwest::Client,
        base_url: String,
        installation_id: String,
        originator: String,
    ) -> Self {
        Self {
            http,
            base_url,
            installation_id,
            originator,
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn installation_id(&self) -> &str {
        &self.installation_id
    }

    pub fn originator(&self) -> &str {
        &self.originator
    }

    /// POST the request and return a typed SSE stream.
    ///
    /// On any non-2xx response, drains the body once and surfaces it
    /// as [`ChatgptError::ResponsesEndpoint`] *before* yielding any
    /// stream items — callers can pattern-match on the first error
    /// rather than juggling state across `Option<Result>`s.
    ///
    /// Transient failures (502/503/504 + connection/timeout errors) are
    /// retried with exponential backoff inside a 5-minute budget; see
    /// the `RETRY_*` constants above for the policy.
    pub async fn stream(
        &self,
        request: &ResponsesApiRequest,
        auth: &AuthSnapshot,
    ) -> Result<ResponseStream, ChatgptError> {
        let url = format!("{}/responses", self.base_url.trim_end_matches('/'));

        let response = self
            .post_with_retry(&url, |builder| builder.json(request), auth, "responses")
            .await?;

        let byte_stream = response.bytes_stream();
        let parsed = parse_byte_stream(byte_stream);
        Ok(ResponseStream::new(Box::pin(parsed)))
    }

    /// POST a request to `url` with retry-on-transient-failure. Rebuilds
    /// the request from scratch on each attempt because `RequestBuilder`
    /// is consumed by `.send()` and serialised bodies aren't trivially
    /// clonable. `build_body` lets callers inject `.json(...)` or any
    /// other body shape per attempt.
    async fn post_with_retry<F>(
        &self,
        url: &str,
        build_body: F,
        auth: &AuthSnapshot,
        op: &str,
    ) -> Result<reqwest::Response, ChatgptError>
    where
        F: Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
    {
        let started = Instant::now();
        let mut attempt: u32 = 0;
        loop {
            let headers = headers::build_headers(auth, &self.installation_id, &self.originator)?;
            let builder = self.http.post(url).headers(headers);
            let send_result =
                tokio::time::timeout(REQUEST_TIMEOUT, build_body(builder).send()).await;

            match send_result {
                Ok(Ok(resp)) if resp.status().is_success() => return Ok(resp),
                Ok(Ok(resp)) => {
                    let status = resp.status().as_u16();
                    if !is_transient_status(status) {
                        return Err(http_error_from_response(resp, status).await);
                    }
                    let retry_after = retry_after_seconds(resp.headers());
                    let next_delay = retry_delay(attempt, retry_after);
                    if !budget_allows(started, next_delay) {
                        tracing::warn!(
                            op,
                            attempt = attempt + 1,
                            status,
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            budget_ms = RETRY_BUDGET_MS,
                            "retry budget exhausted; surfacing error",
                        );
                        return Err(http_error_from_response(resp, status).await);
                    }
                    drop(resp);
                    tracing::warn!(
                        op,
                        attempt = attempt + 1,
                        status,
                        delay_ms = next_delay.as_millis() as u64,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "transient HTTP failure; retrying after backoff",
                    );
                    tokio::time::sleep(next_delay).await;
                    attempt += 1;
                }
                Ok(Err(e)) => {
                    if !is_transient_transport(&e) {
                        return Err(e.into());
                    }
                    let next_delay = retry_delay(attempt, None);
                    if !budget_allows(started, next_delay) {
                        tracing::warn!(
                            op, attempt = attempt + 1,
                            error = %e,
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            budget_ms = RETRY_BUDGET_MS,
                            "retry budget exhausted; surfacing error",
                        );
                        return Err(e.into());
                    }
                    tracing::warn!(
                        op, attempt = attempt + 1,
                        error = %e,
                        delay_ms = next_delay.as_millis() as u64,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "network error; retrying after backoff",
                    );
                    tokio::time::sleep(next_delay).await;
                    attempt += 1;
                }
                Err(_) => {
                    let next_delay = retry_delay(attempt, None);
                    if !budget_allows(started, next_delay) {
                        tracing::warn!(
                            op,
                            attempt = attempt + 1,
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            budget_ms = RETRY_BUDGET_MS,
                            "retry budget exhausted after response-header timeout; surfacing error",
                        );
                        return Err(ChatgptError::ResponsesStreamRead(format!(
                            "timed out waiting for response headers after {}s",
                            REQUEST_TIMEOUT.as_secs()
                        )));
                    }
                    tracing::warn!(
                        op,
                        attempt = attempt + 1,
                        delay_ms = next_delay.as_millis() as u64,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "response-header timeout; retrying after backoff",
                    );
                    tokio::time::sleep(next_delay).await;
                    attempt += 1;
                }
            }
        }
    }

    /// GET `{base_url}/models` and return the list available to the
    /// authenticated account. Codex's CLI uses the same endpoint for
    /// its `/model` picker. The response shape is `{ "models": [...] }`
    /// with rich metadata per entry; we only need slug + display fields.
    ///
    /// Retries transient failures with the same policy as [`Self::stream`]
    /// — the model picker shouldn't block on a single backend hiccup.
    pub async fn list_models(&self, auth: &AuthSnapshot) -> Result<Vec<ModelEntry>, ChatgptError> {
        let url = format!(
            "{}/models?client_version={}",
            self.base_url.trim_end_matches('/'),
            CODEX_COMPAT_CLIENT_VERSION,
        );

        let started = Instant::now();
        let mut attempt: u32 = 0;
        let response = loop {
            let headers = headers::build_headers(auth, &self.installation_id, &self.originator)?;
            let send_result = self
                .http
                .get(&url)
                .headers(headers)
                .timeout(REQUEST_TIMEOUT)
                .send()
                .await;
            match send_result {
                Ok(resp) if resp.status().is_success() => break resp,
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    if !is_transient_status(status) {
                        return Err(http_error_from_response(resp, status).await);
                    }
                    let retry_after = retry_after_seconds(resp.headers());
                    let next_delay = retry_delay(attempt, retry_after);
                    if !budget_allows(started, next_delay) {
                        tracing::warn!(
                            op = "list_models",
                            attempt = attempt + 1,
                            status,
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            budget_ms = RETRY_BUDGET_MS,
                            "retry budget exhausted; surfacing error",
                        );
                        return Err(http_error_from_response(resp, status).await);
                    }
                    drop(resp);
                    tracing::warn!(
                        op = "list_models",
                        attempt = attempt + 1,
                        status,
                        delay_ms = next_delay.as_millis() as u64,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "transient HTTP failure; retrying after backoff",
                    );
                    tokio::time::sleep(next_delay).await;
                    attempt += 1;
                }
                Err(e) => {
                    if !is_transient_transport(&e) {
                        return Err(e.into());
                    }
                    let next_delay = retry_delay(attempt, None);
                    if !budget_allows(started, next_delay) {
                        tracing::warn!(
                            op = "list_models", attempt = attempt + 1,
                            error = %e,
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            budget_ms = RETRY_BUDGET_MS,
                            "retry budget exhausted; surfacing error",
                        );
                        return Err(e.into());
                    }
                    tracing::warn!(
                        op = "list_models", attempt = attempt + 1,
                        error = %e,
                        delay_ms = next_delay.as_millis() as u64,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "network error; retrying after backoff",
                    );
                    tokio::time::sleep(next_delay).await;
                    attempt += 1;
                }
            }
        };

        let parsed: ModelsResponse = response.json().await.map_err(|e| {
            ChatgptError::ResponsesStreamParse(format!("decode /models response: {e}"))
        })?;
        Ok(parsed.models)
    }
}

/// 5xx range that's worth retrying. 500 is excluded because it usually
/// means the request itself was bad (model rejected our shape) and a
/// retry won't help.
fn is_transient_status(status: u16) -> bool {
    matches!(status, 502..=504)
}

/// Whether a `reqwest::Error` reflects a transient transport-level
/// failure worth retrying. `is_connect()` alone misses the broader
/// "couldn't even get the request out" cases (DNS, TLS handshake,
/// dropped sockets during write) which reqwest surfaces under
/// `is_request()` — those have the user-visible "error sending request
/// for url …" string. `is_timeout()` covers the read-timeout class.
/// Anything else (builder errors, decode errors, redirect loops) is
/// permanent and surfaces immediately.
fn is_transient_transport(e: &reqwest::Error) -> bool {
    e.is_connect() || e.is_timeout() || e.is_request()
}

/// Parse a numeric `Retry-After: <seconds>` header. RFC 7231 also
/// allows an HTTP-date form; ChatGPT's edge emits the numeric form, so
/// we only handle that variant. Returns `None` on missing/unparseable.
fn retry_after_seconds(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
}

/// Drain a non-2xx response body and wrap it as a
/// [`ChatgptError::ResponsesEndpoint`]. Pulled out so the retry loop's
/// "give up" and "no-retry, surface immediately" arms share one shape
/// for body capture.
async fn http_error_from_response(resp: reqwest::Response, status: u16) -> ChatgptError {
    let body = resp
        .text()
        .await
        .unwrap_or_else(|_| "<unreadable response body>".to_string());
    ChatgptError::ResponsesEndpoint { status, body }
}

/// Whether the retry budget can absorb one more `next_delay` wait
/// starting from `started`. Conservative: returns true only when the
/// projected elapsed time after the sleep is strictly less than the
/// budget, so a wait that lands exactly on the budget boundary still
/// proceeds — the model-side cost is dominated by the eventual call,
/// not the sleep.
fn budget_allows(started: Instant, next_delay: Duration) -> bool {
    let projected = started.elapsed().saturating_add(next_delay);
    projected < Duration::from_millis(RETRY_BUDGET_MS)
}

/// Exponential backoff with jitter, capped at [`RETRY_MAX_DELAY_MS`].
/// `attempt` is 0-indexed (0 for the first retry after the original
/// failure, 1 for the next, …). When a server-supplied `Retry-After` is
/// available it wins over the computed value.
fn retry_delay(attempt: u32, retry_after_sec: Option<u64>) -> Duration {
    if let Some(s) = retry_after_sec {
        let ms = s.saturating_mul(1_000).min(RETRY_MAX_DELAY_MS);
        return Duration::from_millis(ms);
    }
    // 2^attempt with saturation: attempt=0→1×, 1→2×, 2→4×, … Clamps at
    // u32::MAX worth of shift so a hypothetical large `attempt` doesn't
    // wrap; the result is then clamped to RETRY_MAX_DELAY_MS anyway.
    let shift = attempt.min(32);
    let factor = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
    let base = RETRY_BASE_DELAY_MS.saturating_mul(factor);
    let capped = base.min(RETRY_MAX_DELAY_MS);
    let jitter = rand::thread_rng().gen_range(-RETRY_JITTER_MS..=RETRY_JITTER_MS);
    let with_jitter = (capped as i64 + jitter).max(0) as u64;
    Duration::from_millis(with_jitter)
}

/// Glue: take a reqwest byte-stream and yield typed events as the
/// bytes are consumed. Buffers across chunk boundaries; carries a
/// `[DONE]` sentinel through as a clean stream end.
fn parse_byte_stream<S>(
    byte_stream: S,
) -> impl futures::Stream<Item = Result<ResponseEvent, ChatgptError>> + Send + 'static
where
    S: futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
{
    let buffer = SseBuffer::new();
    futures::stream::unfold(
        (Box::pin(byte_stream) as Pin<Box<S>>, buffer, Vec::new()),
        move |(mut byte_stream, mut buffer, mut pending)| async move {
            loop {
                // Drain any frames produced by previous reads before
                // pulling more bytes from the wire.
                if let Some(event) = pop_pending(&mut pending) {
                    return Some((event, (byte_stream, buffer, pending)));
                }
                match byte_stream.next().await {
                    Some(Ok(chunk)) => {
                        buffer.push(&chunk);
                        for frame in buffer.drain() {
                            match frame {
                                Ok(frame) => {
                                    if let Some(parsed) = parse_sse_frame(&frame.data) {
                                        pending.push(parsed);
                                    }
                                }
                                Err(err) => pending
                                    .push(Err(ChatgptError::ResponsesStreamParse(err.to_string()))),
                            }
                        }
                    }
                    Some(Err(err)) => {
                        return Some((
                            Err(ChatgptError::ResponsesStreamRead(reqwest_error_detail(
                                &err,
                            ))),
                            (byte_stream, buffer, pending),
                        ));
                    }
                    None => return None,
                }
            }
        },
    )
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

fn pop_pending(
    pending: &mut Vec<Result<ResponseEvent, ChatgptError>>,
) -> Option<Result<ResponseEvent, ChatgptError>> {
    if pending.is_empty() {
        None
    } else {
        Some(pending.remove(0))
    }
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};

    #[test]
    fn is_transient_status_only_5xx_recoverable() {
        assert!(is_transient_status(502));
        assert!(is_transient_status(503));
        assert!(is_transient_status(504));
        assert!(!is_transient_status(500));
        assert!(!is_transient_status(400));
        assert!(!is_transient_status(429));
        assert!(!is_transient_status(200));
    }

    #[test]
    fn retry_after_seconds_parses_numeric_header() {
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, HeaderValue::from_static("12"));
        assert_eq!(retry_after_seconds(&h), Some(12));
    }

    #[test]
    fn retry_after_seconds_handles_missing_and_garbage() {
        assert_eq!(retry_after_seconds(&HeaderMap::new()), None);

        let mut h = HeaderMap::new();
        h.insert(
            RETRY_AFTER,
            HeaderValue::from_static("Wed, 21 Oct 2026 07:28:00 GMT"),
        );
        assert_eq!(retry_after_seconds(&h), None);
    }

    #[test]
    fn retry_delay_doubles_with_attempt() {
        // Compare midpoints (jitter spans ±RETRY_JITTER_MS, capped at
        // RETRY_MAX_DELAY_MS). attempt=0 → ~1s, 1 → ~2s, 2 → ~4s.
        let d0 = retry_delay(0, None);
        let d1 = retry_delay(1, None);
        let d2 = retry_delay(2, None);
        // Lower-bound assertions: each step ≥ prior step's mid - jitter.
        assert!(d0 <= Duration::from_millis(1_000 + RETRY_JITTER_MS as u64));
        assert!(d1 >= Duration::from_millis(2_000 - RETRY_JITTER_MS as u64));
        assert!(d2 >= Duration::from_millis(4_000 - RETRY_JITTER_MS as u64));
    }

    #[test]
    fn retry_delay_caps_at_max() {
        // attempt=10 would be 2^10 × 1s = 1024s — must clamp.
        let d = retry_delay(10, None);
        assert!(d <= Duration::from_millis(RETRY_MAX_DELAY_MS + RETRY_JITTER_MS as u64));
    }

    #[test]
    fn retry_delay_honours_retry_after_over_backoff() {
        // Retry-After: 5s — must use 5s exactly, no jitter, regardless
        // of attempt. The server's hint wins.
        let d = retry_delay(0, Some(5));
        assert_eq!(d, Duration::from_millis(5_000));
        let d = retry_delay(3, Some(5));
        assert_eq!(d, Duration::from_millis(5_000));
    }

    #[test]
    fn retry_delay_caps_retry_after_too() {
        // A pathological Retry-After: 999999s still clamps to our cap.
        let d = retry_delay(0, Some(999_999));
        assert_eq!(d, Duration::from_millis(RETRY_MAX_DELAY_MS));
    }

    #[test]
    fn budget_allows_admits_fresh_start() {
        // A small next-delay against a budget that's barely been
        // touched must always be allowed.
        let started = Instant::now();
        assert!(budget_allows(started, Duration::from_secs(1)));
        assert!(budget_allows(started, Duration::from_secs(30)));
    }

    #[test]
    fn budget_allows_rejects_when_projected_exceeds_window() {
        // Simulate "started long ago" by manually projecting: an Instant
        // earlier than now-(budget) means the next sleep MUST be
        // rejected. We can't time-travel an Instant directly, so check
        // the boundary: a next-delay equal to RETRY_BUDGET_MS from a
        // fresh start projects past the window and is rejected.
        let started = Instant::now();
        assert!(!budget_allows(
            started,
            Duration::from_millis(RETRY_BUDGET_MS),
        ));
        assert!(!budget_allows(
            started,
            Duration::from_millis(RETRY_BUDGET_MS + 1_000),
        ));
    }
}

#[cfg(test)]
mod model_entry_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn model_entry_deserialises_supports_reasoning_summaries() {
        let v = json!({
            "slug": "gpt-5",
            "display_name": "GPT-5",
            "priority": 10,
            "supports_reasoning_summaries": true,
        });
        let m: ModelEntry = serde_json::from_value(v).expect("decode");
        assert_eq!(m.slug, "gpt-5");
        assert!(m.supports_reasoning_summaries);
    }

    #[test]
    fn model_entry_defaults_reasoning_to_false_when_absent() {
        // Backend omitting the field MUST default to false so a missing
        // capability never accidentally activates reasoning.
        let v = json!({ "slug": "mystery-model" });
        let m: ModelEntry = serde_json::from_value(v).expect("decode");
        assert!(!m.supports_reasoning_summaries);
    }

    #[test]
    fn model_entry_tolerates_unknown_fields_alongside_known_ones() {
        // The real ModelInfo has 30+ fields; we deserialise a subset.
        // Unknown fields must not break decoding.
        let v = json!({
            "slug": "gpt-5.3-codex-spark",
            "display_name": "Codex Spark",
            "priority": 5,
            "supports_reasoning_summaries": false,
            "supports_parallel_tool_calls": true,
            "shell_type": "default",
            "visibility": "public",
        });
        let m: ModelEntry = serde_json::from_value(v).expect("decode");
        assert_eq!(m.slug, "gpt-5.3-codex-spark");
        assert!(!m.supports_reasoning_summaries);
    }
}
