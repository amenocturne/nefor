use std::sync::{Arc, LazyLock};

use reqwest::header::HeaderMap;
use serde::Serialize;
use uuid::Uuid;

pub const WIRE_LOG_ENV: &str = "NEFOR_OPENAI_WIRE_LOG";

static PROCESS_SESSION_ID: LazyLock<String> = LazyLock::new(|| Uuid::new_v4().to_string());

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WireRecord {
    pub session_id: String,
    pub request_id: String,
    pub attempt: Option<usize>,
    pub sequence: u64,
    pub event: &'static str,
    pub method: Option<String>,
    pub url: Option<String>,
    pub status: Option<u16>,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    pub body_hex: Option<String>,
    pub detail: Option<String>,
}

pub trait WireSink: Send + Sync {
    fn emit(&self, record: WireRecord);
}

#[derive(Debug)]
struct TracingWireSink;

impl WireSink for TracingWireSink {
    fn emit(&self, record: WireRecord) {
        let encoded = serde_json::to_string(&record).unwrap_or_else(|error| {
            format!(r#"{{"event":"wire_log_encode_error","detail":{error:?}}}"#)
        });
        tracing::info!(target: "openai_provider::wire", record = %encoded, "OpenAI wire");
    }
}

#[derive(Clone)]
pub struct WireTrace {
    sink: Option<Arc<dyn WireSink>>,
    session_id: String,
    request_id: String,
    sequence: u64,
}

impl WireTrace {
    pub fn from_env() -> Self {
        if env_enabled(std::env::var(WIRE_LOG_ENV).ok().as_deref()) {
            Self::enabled_with_ids(
                Arc::new(TracingWireSink),
                PROCESS_SESSION_ID.clone(),
                Uuid::new_v4().to_string(),
            )
        } else {
            Self::disabled()
        }
    }

    pub fn disabled() -> Self {
        Self {
            sink: None,
            session_id: String::new(),
            request_id: String::new(),
            sequence: 0,
        }
    }

    pub fn enabled_with_ids(
        sink: Arc<dyn WireSink>,
        session_id: impl Into<String>,
        request_id: impl Into<String>,
    ) -> Self {
        Self {
            sink: Some(sink),
            session_id: session_id.into(),
            request_id: request_id.into(),
            sequence: 0,
        }
    }

    pub fn begin(&mut self, method: &str, url: &str) {
        self.emit(
            "begin",
            None,
            Some(method),
            Some(url),
            None,
            Vec::new(),
            None,
            None,
            None,
        );
    }

    pub fn request(&mut self, attempt: usize, method: &str, url: &str, body: &str) {
        self.emit(
            "request",
            Some(attempt),
            Some(method),
            Some(url),
            None,
            if method == "POST" {
                vec![
                    ("accept".into(), "text/event-stream".into()),
                    ("accept-encoding".into(), "identity".into()),
                    ("content-type".into(), "application/json".into()),
                ]
            } else {
                Vec::new()
            },
            (!body.is_empty()).then(|| body.to_owned()),
            None,
            None,
        );
    }

    pub fn response(&mut self, attempt: usize, status: u16, headers: &HeaderMap) {
        self.emit(
            "response",
            Some(attempt),
            None,
            None,
            Some(status),
            safe_response_headers(headers),
            None,
            None,
            None,
        );
    }

    pub fn response_body(&mut self, attempt: usize, body: &[u8]) {
        self.emit(
            "response_body",
            Some(attempt),
            None,
            None,
            None,
            Vec::new(),
            Some(String::from_utf8_lossy(body).into_owned()),
            Some(hex(body)),
            None,
        );
    }

    pub fn stream_data(&mut self, attempt: usize, bytes: &[u8]) {
        self.emit(
            "stream_data",
            Some(attempt),
            None,
            None,
            None,
            Vec::new(),
            Some(String::from_utf8_lossy(bytes).into_owned()),
            Some(hex(bytes)),
            None,
        );
    }

    pub fn retry(&mut self, attempt: usize, detail: String) {
        self.emit(
            "retry",
            Some(attempt),
            None,
            None,
            None,
            Vec::new(),
            None,
            None,
            Some(detail),
        );
    }

    pub fn end(&mut self, attempt: Option<usize>, detail: &str) {
        self.emit(
            "end",
            attempt,
            None,
            None,
            None,
            Vec::new(),
            None,
            None,
            Some(detail.to_owned()),
        );
    }

    pub fn error(&mut self, attempt: Option<usize>, detail: String) {
        self.emit(
            "error",
            attempt,
            None,
            None,
            None,
            Vec::new(),
            None,
            None,
            Some(detail),
        );
    }

    pub fn cancel(&mut self, attempt: Option<usize>) {
        self.emit(
            "cancel",
            attempt,
            None,
            None,
            None,
            Vec::new(),
            None,
            None,
            None,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn emit(
        &mut self,
        event: &'static str,
        attempt: Option<usize>,
        method: Option<&str>,
        url: Option<&str>,
        status: Option<u16>,
        headers: Vec<(String, String)>,
        body: Option<String>,
        body_hex: Option<String>,
        detail: Option<String>,
    ) {
        let Some(sink) = &self.sink else { return };
        let record = WireRecord {
            session_id: self.session_id.clone(),
            request_id: self.request_id.clone(),
            attempt,
            sequence: self.sequence,
            event,
            method: method.map(str::to_owned),
            url: url.map(str::to_owned),
            status,
            headers,
            body,
            body_hex,
            detail,
        };
        self.sequence = self.sequence.saturating_add(1);
        sink.emit(record);
    }
}

pub fn env_enabled(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on" | "enabled"
        )
    })
}

fn safe_response_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    const ALLOWED: &[&str] = &[
        "content-type",
        "date",
        "retry-after",
        "server",
        "traceparent",
        "x-correlation-id",
        "x-request-id",
        "request-id",
        "openai-request-id",
        "x-ratelimit-limit-requests",
        "x-ratelimit-remaining-requests",
        "x-ratelimit-reset-requests",
    ];
    headers
        .iter()
        .filter(|(name, _)| ALLOWED.contains(&name.as_str()))
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                value.to_str().unwrap_or("<non-utf8>").to_owned(),
            )
        })
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Capture(Mutex<Vec<WireRecord>>);
    impl WireSink for Capture {
        fn emit(&self, record: WireRecord) {
            self.0.lock().unwrap().push(record);
        }
    }

    #[test]
    fn environment_is_disabled_by_default_and_requires_a_truthy_value() {
        assert!(!env_enabled(None));
        assert!(!env_enabled(Some("")));
        assert!(!env_enabled(Some("0")));
        assert!(!env_enabled(Some("false")));
        assert!(env_enabled(Some("TRUE")));
        assert!(env_enabled(Some("on")));
    }

    #[test]
    fn response_headers_are_allowlisted_and_stream_bytes_are_exact() {
        let capture = Arc::new(Capture::default());
        let mut trace = WireTrace::enabled_with_ids(capture.clone(), "session", "request");
        let mut headers = HeaderMap::new();
        headers.insert("x-request-id", "correlate".parse().unwrap());
        headers.insert("authorization", "Bearer secret".parse().unwrap());
        headers.insert("set-cookie", "credential=secret".parse().unwrap());
        trace.response(2, 200, &headers);
        trace.stream_data(2, b"data: {malformed}\n\n\xff");
        let records = capture.0.lock().unwrap();
        let encoded = serde_json::to_string(&*records).unwrap();
        assert!(encoded.contains("correlate"));
        assert!(!encoded.contains("Bearer secret"));
        assert!(!encoded.contains("credential=secret"));
        assert_eq!(
            records[1].body_hex.as_deref(),
            Some("646174613a207b6d616c666f726d65647d0a0aff")
        );
    }

    #[test]
    fn disabled_trace_emits_nothing() {
        let mut trace = WireTrace::disabled();
        trace.begin("POST", "https://example.invalid");
        trace.request(1, "POST", "https://example.invalid", "{sensitive}");
    }
}
