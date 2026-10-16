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
        &retry_policy(),
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

fn retry_policy() -> openai_provider::config::RetryPolicy {
    openai_provider::config::RetryPolicy::from_json(r#"{"max_attempts":8,"max_rounds":1,"backoff_multiplier":2,"initial_delay_ms":500,"max_delay_ms":30000,"round_cooldown_ms":0,"jitter_bps":0,"response_headers_timeout_ms":120000}"#).expect("explicit fixture retry policy")
}

#[tokio::test(start_paused = true)]
async fn retry_after_deadline_counts_body_time_and_overrides_inner_cap_or_outer_cooldown() {
    struct Observe(tokio::sync::mpsc::UnboundedSender<WireRecord>);
    impl WireSink for Observe {
        fn emit(&self, record: WireRecord) {
            let _ = self.0.send(record);
        }
    }
    for (attempts, rounds, phase) in [(2, 1, "attempt"), (1, 2, "round")] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (release_body, body_ready) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 8192];
            socket.read(&mut request).await.unwrap();
            socket.write_all(b"HTTP/1.1 429 Fixture\r\nRetry-After: 120\r\nContent-Length: 2\r\nConnection: close\r\n\r\n").await.unwrap();
            body_ready.await.unwrap();
            socket.write_all(b"{}").await.unwrap();
            drop(socket);
            let (mut next, _) = listener.accept().await.unwrap();
            next.read(&mut request).await.unwrap();
            let body = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
            next.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        });
        let running = tokio::spawn(async {
            loop {
                tokio::task::yield_now().await;
            }
        });
        let (records_tx, mut records) = tokio::sync::mpsc::unbounded_channel();
        let (progress_tx, mut progress) = tokio::sync::mpsc::unbounded_channel();
        let policy = openai_provider::config::RetryPolicy::from_json(&serde_json::json!({
            "max_attempts":attempts,"max_rounds":rounds,"backoff_multiplier":2,"initial_delay_ms":500,"max_delay_ms":30000,
            "round_cooldown_ms":60000,"jitter_bps":0,"response_headers_timeout_ms":120000
        }).to_string()).unwrap();
        let started = tokio::time::Instant::now();
        let completion = tokio::spawn(async move {
            run_chat_stream_with_retry_progress_and_format_and_wire(
                &reqwest::Client::new(),
                &policy,
                &format!("http://{addr}/v1/chat/completions"),
                None,
                "Authorization",
                "fixture",
                &[Message::user("same request")],
                None,
                None,
                None,
                CancellationToken::new(),
                |_| {},
                |_| {},
                move |notice| {
                    progress_tx.send(notice).unwrap();
                },
                WireTrace::enabled_with_ids(Arc::new(Observe(records_tx)), "session", "deadline"),
            )
            .await
        });
        while records.recv().await.unwrap().event != "response" {}
        tokio::time::advance(std::time::Duration::from_secs(5)).await;
        release_body.send(()).unwrap();
        let notice = progress.recv().await.unwrap();
        assert_eq!(notice.phase.as_str(), phase);
        assert_eq!(
            notice.next_delay,
            std::time::Duration::from_secs(115),
            "server deadline uses header arrival, not body completion"
        );
        tokio::time::advance(std::time::Duration::from_secs(114)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        while let Ok(record) = records.try_recv() {
            assert!(
                !(record.event == "request" && record.attempt == Some(2)),
                "never send before Retry-After"
            );
        }
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        assert_eq!(completion.await.unwrap().unwrap().full_text, "ok");
        assert_eq!(started.elapsed(), std::time::Duration::from_secs(120));
        server.await.unwrap();
        running.abort();
    }
}
