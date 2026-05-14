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

use std::pin::Pin;

use futures::stream::StreamExt;

pub use headers::{build_headers, default_user_agent};
pub use request::{
    MessageContent, Reasoning, ReasoningEffort, ReasoningSummary, ReasoningSummaryPart,
    ResponseItem, ResponsesApiRequest, TextControls, Verbosity,
};
pub use stream::{parse_sse_frame, ResponseEvent, ResponseStream, SseBuffer};

use serde::Deserialize;

use crate::auth::AuthSnapshot;
use crate::error::ChatgptError;

/// Minimal subset of the model metadata returned by
/// `GET /models`. The real `ModelInfo` codex defines has 30+ fields;
/// nefor only needs the user-facing identity and ordering hints.
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
        Self {
            http: reqwest::Client::new(),
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
    pub async fn stream(
        &self,
        request: &ResponsesApiRequest,
        auth: &AuthSnapshot,
    ) -> Result<ResponseStream, ChatgptError> {
        let headers = headers::build_headers(auth, &self.installation_id, &self.originator)?;
        let url = format!("{}/responses", self.base_url.trim_end_matches('/'));

        let response = self
            .http
            .post(&url)
            .headers(headers)
            .json(request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable response body>".to_string());
            return Err(ChatgptError::ResponsesEndpoint { status, body });
        }

        let byte_stream = response.bytes_stream();
        let parsed = parse_byte_stream(byte_stream);
        Ok(ResponseStream::new(Box::pin(parsed)))
    }

    /// GET `{base_url}/models` and return the list available to the
    /// authenticated account. Codex's CLI uses the same endpoint for
    /// its `/model` picker. The response shape is `{ "models": [...] }`
    /// with rich metadata per entry; we only need slug + display fields.
    pub async fn list_models(&self, auth: &AuthSnapshot) -> Result<Vec<ModelEntry>, ChatgptError> {
        let headers = headers::build_headers(auth, &self.installation_id, &self.originator)?;
        let url = format!(
            "{}/models?client_version={}",
            self.base_url.trim_end_matches('/'),
            CODEX_COMPAT_CLIENT_VERSION,
        );

        let response = self.http.get(&url).headers(headers).send().await?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable response body>".to_string());
            return Err(ChatgptError::ResponsesEndpoint { status, body });
        }

        let parsed: ModelsResponse = response
            .json()
            .await
            .map_err(|e| ChatgptError::ResponsesStream(format!("decode /models response: {e}")))?;
        Ok(parsed.models)
    }
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
                            if let Some(parsed) = parse_sse_frame(&frame) {
                                pending.push(parsed);
                            }
                        }
                    }
                    Some(Err(err)) => {
                        return Some((
                            Err(ChatgptError::ResponsesStream(err.to_string())),
                            (byte_stream, buffer, pending),
                        ));
                    }
                    None => return None,
                }
            }
        },
    )
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
