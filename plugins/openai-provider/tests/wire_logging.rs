use std::sync::{Arc, Mutex};

use openai_provider::openai::Message;
use openai_provider::stream::{
    run_chat_stream_with_retry_progress_and_format_and_wire, StreamError,
};
use openai_provider::wire::{WireRecord, WireSink, WireTrace};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Capture(Mutex<Vec<WireRecord>>);
impl WireSink for Capture {
    fn emit(&self, record: WireRecord) {
        self.0.lock().unwrap().push(record);
    }
}

async fn server(response: &'static [u8]) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = vec![0; 8192];
        let _ = stream.read(&mut request).await;
        stream.write_all(response).await.unwrap();
    });
    format!("http://{addr}/v1/chat/completions")
}

async fn run(endpoint: &str, trace: WireTrace) -> Result<String, StreamError> {
    let mut deltas = String::new();
    let outcome = run_chat_stream_with_retry_progress_and_format_and_wire(
        &reqwest::Client::new(),
        endpoint,
        Some("never-log-this-token"),
        "X-Secret-Auth",
        "model",
        &[Message::user("sensitive prompt")],
        None,
        None,
        None,
        CancellationToken::new(),
        |delta| deltas.push_str(delta),
        |_| {},
        |_| {},
        trace,
    )
    .await?;
    assert_eq!(deltas, outcome.full_text);
    Ok(outcome.full_text)
}

#[tokio::test]
async fn enabled_trace_preserves_raw_sse_arrival_order_and_stream_outcome() {
    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: {malformed}\n\n";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nX-Request-ID: corr-1\r\nSet-Cookie: secret=cookie\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let endpoint = server(Box::leak(response.into_boxed_str()).as_bytes()).await;
    let capture = Arc::new(Capture::default());
    let error = run(
        &endpoint,
        WireTrace::enabled_with_ids(capture.clone(), "session", "request"),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, StreamError::Malformed(_)));

    let records = capture.0.lock().unwrap();
    let events: Vec<_> = records.iter().map(|record| record.event).collect();
    assert_eq!(
        events,
        ["begin", "request", "response", "stream_data", "error"]
    );
    assert!(records[1]
        .body
        .as_deref()
        .unwrap()
        .contains("sensitive prompt"));
    assert!(records[3]
        .body
        .as_deref()
        .unwrap()
        .contains("data: {malformed}"));
    let encoded = serde_json::to_string(&*records).unwrap();
    assert!(encoded.contains("corr-1"));
    assert!(!encoded.contains("never-log-this-token"));
    assert!(!encoded.contains("secret=cookie"));
}

#[tokio::test]
async fn non_success_body_is_logged_before_terminal_error() {
    let body = r#"{"error":{"message":"bad shape"}}"#;
    let response = format!(
        "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nX-Correlation-ID: corr-2\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let endpoint = server(Box::leak(response.into_boxed_str()).as_bytes()).await;
    let capture = Arc::new(Capture::default());
    let error = run(
        &endpoint,
        WireTrace::enabled_with_ids(capture.clone(), "session", "request"),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, StreamError::Http { status: 400, .. }));
    let records = capture.0.lock().unwrap();
    assert_eq!(
        records
            .iter()
            .map(|record| record.event)
            .collect::<Vec<_>>(),
        ["begin", "request", "response", "response_body", "error"]
    );
    assert_eq!(records[3].body.as_deref(), Some(body));
}

#[tokio::test]
async fn disabled_trace_leaves_successful_stream_outcome_unchanged() {
    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"same\"}}]}\n\ndata: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let endpoint = server(Box::leak(response.into_boxed_str()).as_bytes()).await;
    assert_eq!(run(&endpoint, WireTrace::disabled()).await.unwrap(), "same");
}
