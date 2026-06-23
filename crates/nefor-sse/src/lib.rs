//! Server-Sent Events frame decoding.
//!
//! This crate deliberately stops at the SSE boundary: it turns raw bytes
//! into complete frames with joined `data:` payloads and optional SSE
//! metadata. Provider-specific JSON parsing stays in provider crates.

use bytes::Bytes;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    pub data: String,
    pub event: Option<String>,
    pub id: Option<String>,
    pub retry: Option<u64>,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum SseError {
    #[error("SSE frame was not valid UTF-8: {0}")]
    InvalidUtf8(String),
}

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

    pub fn push_slice(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn drain(&mut self) -> Vec<Result<SseFrame, SseError>> {
        let mut out = Vec::new();
        while let Some((end, sep_len)) = find_frame_end(&self.buf) {
            let drained: Vec<u8> = self.buf.drain(..end + sep_len).collect();
            let frame_bytes = &drained[..end];
            let frame = match std::str::from_utf8(frame_bytes) {
                Ok(frame) => frame,
                Err(err) => {
                    out.push(Err(SseError::InvalidUtf8(err.to_string())));
                    continue;
                }
            };
            if let Some(frame) = parse_frame(frame) {
                out.push(Ok(frame));
            }
        }
        out
    }
}

fn parse_frame(frame: &str) -> Option<SseFrame> {
    let mut data = Vec::new();
    let mut event = None;
    let mut id = None;
    let mut retry = None;

    for raw_line in frame.lines() {
        let line = raw_line.trim_end_matches('\r');
        if line.starts_with(':') {
            continue;
        }
        let Some((field, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "data" => data.push(value.to_owned()),
            "event" => event = Some(value.to_owned()),
            "id" => id = Some(value.to_owned()),
            "retry" => {
                if let Ok(parsed) = value.parse::<u64>() {
                    retry = Some(parsed);
                }
            }
            _ => {}
        }
    }

    if data.is_empty() {
        return None;
    }

    Some(SseFrame {
        data: data.join("\n"),
        event,
        id,
        retry,
    })
}

fn find_frame_end(buf: &[u8]) -> Option<(usize, usize)> {
    for i in 0..buf.len().saturating_sub(1) {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some((i, 2));
        }
        if buf[i] == b'\r' && buf[i + 1] == b'\r' {
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
    fn yields_complete_frames_and_keeps_partial() {
        let mut b = SseBuffer::new();
        b.push_slice(
            b"data: one\n\n\
              data: two\n\n\
              data: three",
        );
        let frames = b.drain();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].as_ref().expect("valid").data, "one");
        assert_eq!(frames[1].as_ref().expect("valid").data, "two");
    }

    #[test]
    fn handles_crlf_and_cr_frame_boundaries() {
        let mut b = SseBuffer::new();
        b.push_slice(b"data: one\r\n\r\ndata: two\r\r");
        let frames = b.drain();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].as_ref().expect("valid").data, "one");
        assert_eq!(frames[1].as_ref().expect("valid").data, "two");
    }

    #[test]
    fn joins_multiline_data_fields() {
        let mut b = SseBuffer::new();
        b.push_slice(b"event: chunk\ndata: {\"a\":\ndata: 1}\n\n");
        let frame = b.drain().remove(0).expect("valid");
        assert_eq!(frame.event.as_deref(), Some("chunk"));
        assert_eq!(frame.data, "{\"a\":\n1}");
    }

    #[test]
    fn preserves_utf8_split_across_chunks() {
        let mut b = SseBuffer::new();
        let raw = "data: é\n\n".as_bytes();
        let split = raw
            .windows("é".len())
            .position(|w| w == "é".as_bytes())
            .expect("contains e acute")
            + 1;
        b.push_slice(&raw[..split]);
        assert!(b.drain().is_empty());
        b.push_slice(&raw[split..]);
        let frame = b.drain().remove(0).expect("valid");
        assert_eq!(frame.data, "é");
    }

    #[test]
    fn captures_id_and_retry_metadata() {
        let mut b = SseBuffer::new();
        b.push_slice(b"id: abc\nretry: 1500\ndata: hello\n\n");
        let frame = b.drain().remove(0).expect("valid");
        assert_eq!(frame.id.as_deref(), Some("abc"));
        assert_eq!(frame.retry, Some(1500));
        assert_eq!(frame.data, "hello");
    }
}
