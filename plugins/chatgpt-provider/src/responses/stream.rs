//! SSE wire parsing for the Responses endpoint.
//!
//! The server sends `data: {json}\n\n` frames with no `event:`
//! discriminator — the JSON payload's `"type"` field is the
//! discriminator. We buffer bytes, split on blank-line frame boundaries,
//! concatenate `data:` lines per frame, and deserialize into
//! [`ResponseEvent`].
//!
//! Unknown event types deserialize to [`ResponseEvent::Other`] rather
//! than erroring, so the stream stays alive when the server adds new
//! variants. JSON parse failures are logged and skipped.

use bytes::Bytes;
use futures::stream::BoxStream;
use serde::Deserialize;

use crate::error::ChatgptError;
use crate::responses::request::ResponseItem;

/// Typed SSE event. Variant names map to the Responses-API `type`
/// strings via `#[serde(rename = "...")]`. Numeric index fields are
/// `Option<u32>` because the server omits them on some event shapes.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ResponseEvent {
    #[serde(rename = "response.created")]
    Created { response: serde_json::Value },

    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta {
        delta: String,
        #[serde(default)]
        item_id: Option<String>,
        #[serde(default)]
        output_index: Option<u32>,
    },

    #[serde(rename = "response.output_item.added")]
    OutputItemAdded {
        item: ResponseItem,
        #[serde(default)]
        output_index: Option<u32>,
    },

    #[serde(rename = "response.output_item.done")]
    OutputItemDone {
        item: ResponseItem,
        #[serde(default)]
        output_index: Option<u32>,
    },

    #[serde(rename = "response.reasoning_summary_text.delta")]
    ReasoningSummaryDelta {
        delta: String,
        #[serde(default)]
        summary_index: Option<u32>,
        #[serde(default)]
        item_id: Option<String>,
    },

    #[serde(rename = "response.reasoning_summary_part.added")]
    ReasoningSummaryPartAdded {
        #[serde(default)]
        summary_index: Option<u32>,
        #[serde(default)]
        item_id: Option<String>,
    },

    #[serde(rename = "response.reasoning_text.delta")]
    ReasoningContentDelta {
        delta: String,
        #[serde(default)]
        content_index: Option<u32>,
        #[serde(default)]
        item_id: Option<String>,
    },

    #[serde(rename = "response.function_call_arguments.delta")]
    FunctionCallArgumentsDelta {
        delta: String,
        #[serde(default)]
        item_id: Option<String>,
    },

    /// Terminal event for streaming tool-call arguments. Carries the
    /// fully-assembled argument JSON. The Responses API emits this
    /// before the matching `response.output_item.done`; dispatcher can
    /// finalize on either, but using this one short-circuits the case
    /// where deltas + done arrive keyed differently from one another.
    #[serde(rename = "response.function_call_arguments.done")]
    FunctionCallArgumentsDone {
        arguments: String,
        #[serde(default)]
        item_id: Option<String>,
    },

    #[serde(rename = "response.completed")]
    Completed { response: serde_json::Value },

    #[serde(rename = "response.failed")]
    Failed { response: serde_json::Value },

    #[serde(rename = "response.incomplete")]
    Incomplete { response: serde_json::Value },

    #[serde(other)]
    Other,
}

/// Stream returned by [`ResponsesClient::stream`]. Wraps a boxed stream
/// of typed events; the underlying transport is a `reqwest` byte
/// stream that this module parses lazily as bytes arrive.
///
/// [`ResponsesClient::stream`]: super::ResponsesClient::stream
pub struct ResponseStream {
    inner: BoxStream<'static, Result<ResponseEvent, ChatgptError>>,
}

impl ResponseStream {
    pub fn new(inner: BoxStream<'static, Result<ResponseEvent, ChatgptError>>) -> Self {
        Self { inner }
    }

    pub fn into_inner(self) -> BoxStream<'static, Result<ResponseEvent, ChatgptError>> {
        self.inner
    }
}

impl futures::Stream for ResponseStream {
    type Item = Result<ResponseEvent, ChatgptError>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        // SAFETY-ish: `inner` is the only field, projecting it is sound
        // because we own the storage and don't move it elsewhere.
        let this = self.get_mut();
        std::pin::Pin::new(&mut this.inner).poll_next(cx)
    }
}

/// Parse a single SSE frame payload (the concatenated `data:` lines
/// for one event).
///
/// Returns:
///   * `None` when the payload is the SSE end-of-stream sentinel
///     (`[DONE]`) or the line is empty — caller should close.
///   * `Some(Ok(event))` on successful parse.
///   * `Some(Err(_))` on JSON parse failure with the offending text in
///     the error message.
pub fn parse_sse_frame(payload: &str) -> Option<Result<ResponseEvent, ChatgptError>> {
    let trimmed = payload.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed == "[DONE]" {
        return None;
    }
    match serde_json::from_str::<ResponseEvent>(trimmed) {
        Ok(event) => Some(Ok(event)),
        Err(err) => {
            // Don't tear down the stream on a single bad frame — the
            // caller can drop the error and keep polling.
            tracing::warn!(
                target = "chatgpt_provider::responses",
                error = %err,
                snippet = %truncate_for_log(trimmed),
                "failed to parse SSE frame",
            );
            Some(Err(ChatgptError::ResponsesStreamParse(format!(
                "failed to parse SSE event: {err}"
            ))))
        }
    }
}

fn truncate_for_log(s: &str) -> String {
    const MAX: usize = 200;
    if s.len() <= MAX {
        s.to_string()
    } else {
        format!("{}…", &s[..MAX])
    }
}

/// Frame-level buffer that turns a byte stream into a sequence of
/// `data:` payload strings.
///
/// SSE frames are separated by a blank line. We keep bytes until a full
/// frame arrives, then validate UTF-8 once for the whole frame. That
/// avoids corrupting split multi-byte codepoints across network reads.
/// Inside a frame, every line that starts with `data:` contributes to
/// the payload; other line prefixes (`event:`, `id:`, `:` comment,
/// `retry:`) are ignored per the spec.
///
/// `push` appends raw bytes; `drain` returns every complete frame's
/// concatenated `data` payload as a `String`. Trailing partial frames
/// stay in the buffer for the next call.
#[derive(Debug, Default)]
pub struct SseBuffer {
    buf: Vec<u8>,
}

impl SseBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &Bytes) {
        self.buf.extend_from_slice(bytes);
    }

    /// Pop every complete frame, returning each one's joined `data`
    /// payload. Partial trailing frames remain in `self.buf`.
    pub fn drain(&mut self) -> Vec<Result<String, ChatgptError>> {
        let mut out = Vec::new();
        while let Some((end, sep_len)) = find_frame_end(&self.buf) {
            let drained: Vec<u8> = self.buf.drain(..end + sep_len).collect();
            let frame_bytes = &drained[..end];
            let frame = match std::str::from_utf8(frame_bytes) {
                Ok(frame) => frame,
                Err(err) => {
                    out.push(Err(ChatgptError::ResponsesStreamParse(format!(
                        "SSE frame was not valid UTF-8: {err}"
                    ))));
                    continue;
                }
            };
            let mut payload = String::new();
            for line in frame.lines() {
                let line = line.trim_end_matches('\r');
                let Some(rest) = line.strip_prefix("data:") else {
                    continue;
                };
                let rest = rest.strip_prefix(' ').unwrap_or(rest);
                // Multi-line `data` payloads in SSE are joined with a
                // literal newline per the spec. Single-line frames stay
                // unchanged.
                if !payload.is_empty() {
                    payload.push('\n');
                }
                payload.push_str(rest);
            }
            if !payload.is_empty() {
                out.push(Ok(payload));
            }
        }
        out
    }
}

fn find_frame_end(buf: &[u8]) -> Option<(usize, usize)> {
    for i in 0..buf.len().saturating_sub(1) {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some((i, 2));
        }
        if i + 3 < buf.len() && &buf[i..i + 4] == b"\r\n\r\n" {
            return Some((i, 4));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_yields_two_complete_frames_keeps_partial() {
        let mut b = SseBuffer::new();
        b.push(&Bytes::from(
            "data: {\"type\":\"response.created\",\"response\":{}}\n\n\
             data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\n\n\
             data: {\"type\":\"response.completed\"",
        ));
        let frames = b.drain();
        assert_eq!(frames.len(), 2);
        assert!(frames[0]
            .as_ref()
            .expect("valid")
            .contains("response.created"));
        assert!(frames[1].as_ref().expect("valid").contains("Hi"));
    }

    #[test]
    fn parse_sse_frame_handles_done_sentinel() {
        assert!(parse_sse_frame("[DONE]").is_none());
        assert!(parse_sse_frame("   ").is_none());
    }

    #[test]
    fn parse_sse_frame_unknown_type_is_other() {
        let raw = r#"{"type":"response.brand_new_event","payload":42}"#;
        let parsed = parse_sse_frame(raw).expect("Some").expect("Ok");
        assert_eq!(parsed, ResponseEvent::Other);
    }

    #[test]
    fn buffer_handles_crlf_frame_boundaries() {
        let mut b = SseBuffer::new();
        b.push(&Bytes::from(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\r\n\r\n",
        ));
        let frames = b.drain();
        assert_eq!(frames.len(), 1);
        assert!(frames[0].as_ref().expect("valid").contains("Hi"));
    }

    #[test]
    fn buffer_preserves_utf8_split_across_chunks() {
        let mut b = SseBuffer::new();
        let raw = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"é\"}\n\n";
        let bytes = raw.as_bytes();
        let split = bytes
            .windows("é".len())
            .position(|w| w == "é".as_bytes())
            .expect("contains e acute")
            + 1;
        b.push(&Bytes::copy_from_slice(&bytes[..split]));
        assert!(b.drain().is_empty());
        b.push(&Bytes::copy_from_slice(&bytes[split..]));
        let frames = b.drain();
        assert_eq!(frames.len(), 1);
        assert!(frames[0].as_ref().expect("valid").contains("é"));
    }
}
