//! Integration tests for the streaming HTTP path.
//!
//! Spins up a hand-rolled `tokio::net::TcpListener` that speaks just
//! enough HTTP/1.1 to satisfy reqwest's streaming reader. Avoids
//! pulling in axum/hyper-server for one test file.

use nefor_mag::schema::{SchemaField, SchemaType, TypeSchema, SCHEMA_VERSION};
use openai_provider::openai::{Message, ReasoningContinuation};
use openai_provider::state::{ChatId, Chats};
use openai_provider::stream::{
    list_models, run_chat_stream, run_chat_stream_with_retry_progress,
    run_chat_stream_with_retry_progress_and_format, RetryProgress, StreamError,
};
use std::net::SocketAddr;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// Bind a fresh ephemeral port and return both the listener and its
/// addr. Tests drive the listener directly so we control framing.
async fn bind_local() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    (listener, addr)
}

#[tokio::test]
async fn successful_non_sse_json_response_fails_clearly() {
    let (listener, addr) = bind_local().await;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_request(&mut stream).await;
        let body = r#"{"id":"nonstream-completion","choices":[{"message":{"content":"done"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(), body
        );
        stream
            .write_all(response.as_bytes())
            .await
            .expect("response");
    });

    let error = run_chat_stream(
        &reqwest::Client::new(),
        &retry_policy(),
        &format!("http://{addr}/v1/chat/completions"),
        None,
        "Authorization",
        "fixture-model",
        &[Message::user("fixture")],
        None,
        None,
        CancellationToken::new(),
        |_| {},
        |_| {},
    )
    .await
    .expect_err("non-SSE success response must not be accepted as an empty stream");
    server.await.expect("server");
    assert!(
        matches!(error, StreamError::Malformed(message) if message.contains("expected streaming SSE response") && message.contains("application/json"))
    );
}

/// Read the request headers off a single connection and return what
/// followed (the body, if any). We only need enough to drain the
/// request before sending our scripted response.
async fn read_request(stream: &mut tokio::net::TcpStream) -> String {
    let mut buf = vec![0u8; 4096];
    let mut acc = String::new();
    while !acc.contains("\r\n\r\n") {
        let n = stream.read(&mut buf).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        acc.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    acc
}

#[tokio::test]
async fn structured_request_has_object_root_and_decodes_successful_response() {
    let (listener, addr) = bind_local().await;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let request = read_request(&mut stream).await;
        let body = request.split("\r\n\r\n").nth(1).expect("request body");
        let request: serde_json::Value = serde_json::from_str(body).expect("request JSON");
        let schema = &request["response_format"]["json_schema"]["schema"];
        assert_eq!(
            schema["type"], "object",
            "provider rejects absent root type"
        );
        assert_eq!(schema["required"], serde_json::json!(["value"]));
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(
            schema["properties"]["value"]["required"],
            serde_json::json!(["content"])
        );

        let events = "data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"value\\\":{\\\"content\\\":\\\"done\\\"}}\"}}]}\n\n\
                      data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n\
                      data: [DONE]\n\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            events.len(), events
        );
        stream
            .write_all(response.as_bytes())
            .await
            .expect("response");
    });

    let mag_schema = TypeSchema {
        version: SCHEMA_VERSION,
        root: SchemaType::Named {
            name: "nefor.contracts.TextAnswer".into(),
            body: Box::new(SchemaType::Fields {
                fields: vec![SchemaField {
                    name: "content".into(),
                    schema: SchemaType::String,
                }],
            }),
        },
    };
    let provider_schema = mag_schema
        .to_provider_schema()
        .expect("supported MAG schema");
    let response_format = serde_json::json!({
        "type": "json_schema",
        "json_schema": {
            "name": "mag_output",
            "strict": true,
            "schema": provider_schema.schema
        }
    });
    let client = reqwest::Client::new();
    let outcome = run_chat_stream_with_retry_progress_and_format(
        &client,
        &retry_policy(),
        &format!("http://{addr}/v1/chat/completions"),
        None,
        "Authorization",
        "fake-model",
        &[Message::user("answer")],
        None,
        None,
        Some(&response_format),
        CancellationToken::new(),
        |_| {},
        |_| {},
        |_| {},
    )
    .await
    .expect("structured response");
    server.await.expect("server");
    assert_eq!(outcome.full_text, r#"{"value":{"content":"done"}}"#);
    let decoded = mag_schema.validate_provider_json(&outcome.full_text);
    assert!(decoded.ok, "{:?}", decoded.violations);
    assert_eq!(decoded.value, Some(serde_json::json!({"content": "done"})));
}

#[tokio::test]
async fn chat_stream_retries_transient_500_then_succeeds() {
    let (listener, addr) = bind_local().await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_server = attempts.clone();

    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut s, _) = listener.accept().await.expect("accept");
            let _ = read_request(&mut s).await;
            let n = attempts_server.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 1 {
                let body = "{\"error\":\"temporary upstream failure\"}";
                let response = format!(
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = s.write_all(response.as_bytes()).await;
            } else {
                let body = "data: {\"choices\":[{\"delta\":{\"content\":\"Recovered\"}}]}\n\n\
                            data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n\
                            data: [DONE]\n\n";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = s.write_all(response.as_bytes()).await;
            }
            let _ = s.shutdown().await;
        }
    });

    let client = reqwest::Client::builder().build().expect("client");
    let endpoint = format!("http://{}/v1/chat/completions", addr);
    let messages = vec![Message::user("hello")];

    let mut deltas: Vec<String> = Vec::new();
    let outcome = run_chat_stream(
        &client,
        &retry_policy(),
        &endpoint,
        None,
        "Authorization",
        "test-model",
        &messages,
        None,
        None,
        CancellationToken::new(),
        |d| deltas.push(d.to_owned()),
        |_| {},
    )
    .await
    .expect("second attempt succeeds");

    let _ = server.await;

    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(deltas, vec!["Recovered"]);
    assert_eq!(outcome.full_text, "Recovered");
    assert_eq!(outcome.finish_reason.as_deref(), Some("stop"));
}

#[tokio::test]
async fn chat_stream_retries_sse_transport_loss_before_output() {
    let (listener, addr) = bind_local().await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_server = attempts.clone();

    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let _ = read_request(&mut stream).await;
            let attempt = attempts_server.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt == 1 {
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .expect("truncated response headers");
            } else {
                let body = "data: {\"choices\":[{\"delta\":{\"content\":\"Recovered\"}}]}\n\n\
                            data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n\
                            data: [DONE]\n\n";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("recovery response");
            }
            stream.shutdown().await.expect("shutdown");
        }
    });

    let mut deltas = Vec::new();
    let mut progress = Vec::new();
    let outcome = run_chat_stream_with_retry_progress(
        &reqwest::Client::new(),
        &retry_policy(),
        &format!("http://{addr}/v1/chat/completions"),
        None,
        "Authorization",
        "fixture-model",
        &[Message::user("fixture")],
        None,
        None,
        CancellationToken::new(),
        |text| deltas.push(text.to_owned()),
        |_| {},
        |retry| progress.push(retry),
    )
    .await
    .expect("pre-output stream failure is replayed");
    server.await.expect("server");

    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(deltas, vec!["Recovered"]);
    assert_eq!(outcome.full_text, "Recovered");
    assert_eq!(outcome.finish_reason.as_deref(), Some("stop"));
    assert_eq!(progress.len(), 1);
    assert_eq!(progress[0].status, None);
    assert_eq!(progress[0].failed_attempt, 1);
}

#[tokio::test]
async fn chat_stream_retries_429_then_succeeds_and_reports_progress() {
    let (listener, addr) = bind_local().await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_server = attempts.clone();

    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut s, _) = listener.accept().await.expect("accept");
            let _ = read_request(&mut s).await;
            let n = attempts_server.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 1 {
                let body = "{\"error\":\"rate limited\"}";
                let response = format!(
                    "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = s.write_all(response.as_bytes()).await;
            } else {
                let body = "data: {\"choices\":[{\"delta\":{\"content\":\"Recovered\"}}]}\n\n\
                            data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n\
                            data: [DONE]\n\n";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = s.write_all(response.as_bytes()).await;
            }
            let _ = s.shutdown().await;
        }
    });

    let client = reqwest::Client::builder().build().expect("client");
    let endpoint = format!("http://{}/v1/chat/completions", addr);
    let messages = vec![Message::user("hello")];

    let mut progress = Vec::new();
    let outcome = run_chat_stream_with_retry_progress(
        &client,
        &retry_policy(),
        &endpoint,
        None,
        "Authorization",
        "test-model",
        &messages,
        None,
        None,
        CancellationToken::new(),
        |_| {},
        |_| {},
        |p| progress.push(p),
    )
    .await
    .expect("second attempt succeeds");

    let _ = server.await;

    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(outcome.full_text, "Recovered");
    assert_eq!(progress.len(), 1);
    assert_eq!(progress[0].status, Some(429));
    assert_eq!(progress[0].failed_attempt, 1);
    assert_eq!(progress[0].max_attempts, 8);
    assert_eq!(
        progress[0].next_delay,
        std::time::Duration::from_millis(500)
    );
}

#[tokio::test]
async fn chat_stream_does_not_retry_quota_denial() {
    let (listener, addr) = bind_local().await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_server = attempts.clone();

    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_request(&mut stream).await;
        attempts_server.fetch_add(1, Ordering::SeqCst);
        let body = r#"{"error":{"message":"quota exhausted","type":"insufficient_quota"}}"#;
        let response = format!(
            "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream
            .write_all(response.as_bytes())
            .await
            .expect("quota response");
        stream.shutdown().await.expect("shutdown");
    });

    let mut progress = Vec::new();
    let error = run_chat_stream_with_retry_progress(
        &reqwest::Client::new(),
        &retry_policy(),
        &format!("http://{addr}/v1/chat/completions"),
        None,
        "Authorization",
        "fixture-model",
        &[Message::user("fixture")],
        None,
        None,
        CancellationToken::new(),
        |_| {},
        |_| {},
        |retry| progress.push(retry),
    )
    .await
    .expect_err("quota denial must be terminal");
    server.await.expect("server");

    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert!(progress.is_empty());
    assert!(matches!(error, StreamError::Http { status: 429, .. }));
}

#[tokio::test]
async fn chat_stream_429_retry_respects_retry_after_seconds() {
    let (listener, addr) = bind_local().await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_server = attempts.clone();

    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut s, _) = listener.accept().await.expect("accept");
            let _ = read_request(&mut s).await;
            let n = attempts_server.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 1 {
                let body = "{\"error\":\"rate limited\"}";
                let response = format!(
                    "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nRetry-After: 2\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = s.write_all(response.as_bytes()).await;
            } else {
                let body = "data: {\"choices\":[{\"delta\":{\"content\":\"OK\"}}]}\n\n\
                            data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n\
                            data: [DONE]\n\n";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = s.write_all(response.as_bytes()).await;
            }
            let _ = s.shutdown().await;
        }
    });

    let client = reqwest::Client::builder().build().expect("client");
    let endpoint = format!("http://{}/v1/chat/completions", addr);
    let messages = vec![Message::user("hello")];

    let mut progress: Vec<RetryProgress> = Vec::new();
    let started = std::time::Instant::now();
    let outcome = run_chat_stream_with_retry_progress(
        &client,
        &retry_policy(),
        &endpoint,
        None,
        "Authorization",
        "test-model",
        &messages,
        None,
        None,
        CancellationToken::new(),
        |_| {},
        |_| {},
        |p| progress.push(p),
    )
    .await
    .expect("second attempt succeeds");

    let _ = server.await;

    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(outcome.full_text, "OK");
    assert_eq!(progress.len(), 1);
    assert_eq!(progress[0].status, Some(429));
    assert!(progress[0].next_delay <= std::time::Duration::from_secs(2));
    assert!(progress[0].next_delay >= std::time::Duration::from_millis(500));
    assert!(started.elapsed() >= std::time::Duration::from_secs(2));
}

#[tokio::test]
async fn chat_stream_429_past_retry_after_does_not_shorten_local_backoff() {
    let (listener, addr) = bind_local().await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_server = attempts.clone();

    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut s, _) = listener.accept().await.expect("accept");
            let _ = read_request(&mut s).await;
            let n = attempts_server.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 1 {
                let body = "{\"error\":\"rate limited\"}";
                let response = format!(
                    "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nRetry-After: Sun, 06 Nov 1994 08:49:37 GMT\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = s.write_all(response.as_bytes()).await;
            } else {
                let body = "data: {\"choices\":[{\"delta\":{\"content\":\"OK\"}}]}\n\n\
                            data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n\
                            data: [DONE]\n\n";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = s.write_all(response.as_bytes()).await;
            }
            let _ = s.shutdown().await;
        }
    });

    let client = reqwest::Client::builder().build().expect("client");
    let endpoint = format!("http://{}/v1/chat/completions", addr);
    let messages = vec![Message::user("hello")];

    let mut progress: Vec<RetryProgress> = Vec::new();
    let outcome = run_chat_stream_with_retry_progress(
        &client,
        &retry_policy(),
        &endpoint,
        None,
        "Authorization",
        "test-model",
        &messages,
        None,
        None,
        CancellationToken::new(),
        |_| {},
        |_| {},
        |p| progress.push(p),
    )
    .await
    .expect("second attempt succeeds");

    let _ = server.await;

    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(outcome.full_text, "OK");
    assert_eq!(progress.len(), 1);
    assert_eq!(progress[0].status, Some(429));
    assert_eq!(
        progress[0].next_delay,
        std::time::Duration::from_millis(500)
    );
}

#[tokio::test]
async fn chat_stream_cancellation_during_backoff_interrupts_without_second_request() {
    let (listener, addr) = bind_local().await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_server = attempts.clone();

    let server = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.expect("accept");
        let _ = read_request(&mut s).await;
        attempts_server.fetch_add(1, Ordering::SeqCst);
        let body = "{\"error\":\"rate limited\"}";
        let response = format!(
            "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nRetry-After: 60\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = s.write_all(response.as_bytes()).await;
        let _ = s.shutdown().await;
    });

    let client = reqwest::Client::builder().build().expect("client");
    let endpoint = format!("http://{}/v1/chat/completions", addr);
    let messages = vec![Message::user("hello")];
    let cancel = CancellationToken::new();
    let cancel_from_progress = cancel.clone();

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        run_chat_stream_with_retry_progress(
            &client,
            &retry_policy(),
            &endpoint,
            None,
            "Authorization",
            "test-model",
            &messages,
            None,
            None,
            cancel,
            |_| {},
            |_| {},
            move |_| cancel_from_progress.cancel(),
        ),
    )
    .await
    .expect("cancellation should interrupt retry sleep promptly")
    .expect("cancellation returns an interrupted outcome");

    let _ = server.await;

    assert!(outcome.interrupted);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn streaming_emits_deltas_then_end() {
    let (listener, addr) = bind_local().await;

    let server = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.expect("accept");
        let _ = read_request(&mut s).await;
        // Streamed SSE response with two deltas, a finish, a usage frame,
        // and the [DONE] terminator. Content-Type is the usual SSE one.
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n\
                    data: {\"choices\":[{\"delta\":{\"content\":\" there\"}}]}\n\n\
                    data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n\
                    data: {\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2,\"total_tokens\":9}}\n\n\
                    data: [DONE]\n\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = s.write_all(response.as_bytes()).await;
        let _ = s.shutdown().await;
    });

    let client = reqwest::Client::builder().build().expect("client");
    let endpoint = format!("http://{}/v1/chat/completions", addr);
    let messages = vec![Message::user("hello")];

    let mut deltas: Vec<String> = Vec::new();
    let outcome = run_chat_stream(
        &client,
        &retry_policy(),
        &endpoint,
        None,
        "Authorization",
        "test-model",
        &messages,
        None,
        None,
        CancellationToken::new(),
        |d| deltas.push(d.to_owned()),
        |_| {},
    )
    .await
    .expect("ok");

    let _ = server.await;

    assert_eq!(deltas, vec!["Hi", " there"]);
    assert_eq!(outcome.full_text, "Hi there");
    assert_eq!(outcome.finish_reason.as_deref(), Some("stop"));
    let u = outcome.usage.expect("usage");
    assert_eq!(u.prompt_tokens, Some(7));
    assert_eq!(u.completion_tokens, Some(2));
    assert!(!outcome.interrupted);
}

#[tokio::test]
async fn completed_chunked_stream_tolerates_missing_http_terminator() {
    let (listener, addr) = bind_local().await;

    let server = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.expect("accept");
        let _ = read_request(&mut s).await;
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n\
                    data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n\
                    data: [DONE]\n\n";
        let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
        let chunk = format!("{:x}\r\n{}\r\n", body.len(), body);
        let _ = s.write_all(headers.as_bytes()).await;
        let _ = s.write_all(chunk.as_bytes()).await;
        let _ = s.shutdown().await;
    });

    let client = reqwest::Client::builder().build().expect("client");
    let endpoint = format!("http://{}/v1/chat/completions", addr);
    let messages = vec![Message::user("hello")];

    let mut deltas: Vec<String> = Vec::new();
    let outcome = run_chat_stream(
        &client,
        &retry_policy(),
        &endpoint,
        None,
        "Authorization",
        "test-model",
        &messages,
        None,
        None,
        CancellationToken::new(),
        |d| deltas.push(d.to_owned()),
        |_| {},
    )
    .await
    .expect("semantic SSE completion wins over missing chunk terminator");

    let _ = server.await;

    assert_eq!(deltas, vec!["Hi"]);
    assert_eq!(outcome.full_text, "Hi");
    assert_eq!(outcome.finish_reason.as_deref(), Some("stop"));
}

#[tokio::test]
async fn incomplete_chunked_stream_still_errors_without_finish_reason() {
    let (listener, addr) = bind_local().await;

    let server = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.expect("accept");
        let _ = read_request(&mut s).await;
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n";
        let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
        let chunk = format!("{:x}\r\n{}\r\n", body.len(), body);
        let _ = s.write_all(headers.as_bytes()).await;
        let _ = s.write_all(chunk.as_bytes()).await;
        let _ = s.shutdown().await;
    });

    let client = reqwest::Client::builder().build().expect("client");
    let endpoint = format!("http://{}/v1/chat/completions", addr);
    let messages = vec![Message::user("hello")];

    let err = run_chat_stream(
        &client,
        &retry_policy(),
        &endpoint,
        None,
        "Authorization",
        "test-model",
        &messages,
        None,
        None,
        CancellationToken::new(),
        |_| {},
        |_| {},
    )
    .await
    .expect_err("stream without finish_reason is still truncated");

    let _ = server.await;

    match err {
        StreamError::Body(body) => assert!(
            body.contains("unexpected EOF") || body.contains("end of file"),
            "body error carried: {body}"
        ),
        other => panic!("expected body read error, got {other:?}"),
    }
}

#[tokio::test]
async fn request_failure_emits_turn_error() {
    let (listener, addr) = bind_local().await;

    let server = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.expect("accept");
        let _ = read_request(&mut s).await;
        let body = "{\"error\":\"model not found\"}";
        let response = format!(
            "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = s.write_all(response.as_bytes()).await;
        let _ = s.shutdown().await;
    });

    let client = reqwest::Client::builder().build().expect("client");
    let endpoint = format!("http://{}/v1/chat/completions", addr);
    let messages = vec![Message::user("hi")];

    let err = run_chat_stream(
        &client,
        &retry_policy(),
        &endpoint,
        None,
        "Authorization",
        "nonexistent",
        &messages,
        None,
        None,
        CancellationToken::new(),
        |_| {},
        |_| {},
    )
    .await
    .expect_err("should fail");

    let _ = server.await;

    match err {
        StreamError::Http { status, body } => {
            assert_eq!(status, 404);
            assert!(body.contains("model not found"), "body carried: {body}");
        }
        other => panic!("expected HTTP error, got {other:?}"),
    }
}

#[tokio::test]
async fn chat_completion_uses_passed_in_model_not_a_default() {
    // run_chat_stream takes `model` as a parameter; this regression test
    // confirms it lands in the wire body verbatim (so swapping
    // session.active_model -> arg in main.rs's spawn_turn really moves the
    // model through to the HTTP request).
    let (listener, addr) = bind_local().await;
    let captured: std::sync::Arc<tokio::sync::Mutex<String>> =
        std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
    let captured_clone = captured.clone();

    let server = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.expect("accept");
        let mut buf = vec![0u8; 8192];
        let mut acc = String::new();
        // Read until end of headers, then get content-length and body.
        loop {
            let n = s.read(&mut buf).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            acc.push_str(&String::from_utf8_lossy(&buf[..n]));
            if let Some(headers_end) = acc.find("\r\n\r\n") {
                let cl: usize = acc
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("content-length:")
                            .or_else(|| l.strip_prefix("Content-Length:"))
                    })
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                let body_so_far = acc.len() - (headers_end + 4);
                if body_so_far >= cl {
                    break;
                }
            }
        }
        if let Some(idx) = acc.find("\r\n\r\n") {
            *captured_clone.lock().await = acc[idx + 4..].to_owned();
        }
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n\
                    data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n\
                    data: [DONE]\n\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = s.write_all(response.as_bytes()).await;
        let _ = s.shutdown().await;
    });

    let client = reqwest::Client::builder().build().expect("client");
    let endpoint = format!("http://{}/v1/chat/completions", addr);
    let messages = vec![Message::user("hi")];
    let _ = run_chat_stream(
        &client,
        &retry_policy(),
        &endpoint,
        None,
        "Authorization",
        "the-active-model",
        &messages,
        None,
        None,
        CancellationToken::new(),
        |_| {},
        |_| {},
    )
    .await
    .expect("ok");

    let _ = server.await;

    let body = captured.lock().await.clone();
    assert!(
        body.contains("\"model\":\"the-active-model\""),
        "request body did not carry the active model: {body}"
    );
}

#[tokio::test]
async fn list_models_returns_sorted_ids() {
    let (listener, addr) = bind_local().await;

    let server = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.expect("accept");
        let _ = read_request(&mut s).await;
        let body = r#"{"object":"list","data":[{"id":"zebra"},{"id":"apple"},{"id":"mango"}]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = s.write_all(response.as_bytes()).await;
        let _ = s.shutdown().await;
    });

    let client = reqwest::Client::builder().build().expect("client");
    let base_url = format!("http://{}", addr);
    let models = list_models(&client, &base_url, None, "Authorization")
        .await
        .expect("ok");

    let _ = server.await;

    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec!["apple", "mango", "zebra"]);
}

#[tokio::test]
async fn list_models_propagates_unauthorized() {
    let (listener, addr) = bind_local().await;

    let server = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.expect("accept");
        let _ = read_request(&mut s).await;
        let body = "{\"error\":\"invalid api key\"}";
        let response = format!(
            "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = s.write_all(response.as_bytes()).await;
        let _ = s.shutdown().await;
    });

    let client = reqwest::Client::builder().build().expect("client");
    let base_url = format!("http://{}", addr);
    let err = list_models(&client, &base_url, Some("badkey"), "Authorization")
        .await
        .expect_err("should fail");

    let _ = server.await;

    assert!(matches!(err, StreamError::Unauthorized { .. }));
}

#[tokio::test]
async fn unauthorized_response_yields_unauthorized_variant() {
    let (listener, addr) = bind_local().await;

    let server = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.expect("accept");
        let _ = read_request(&mut s).await;
        let body = "{\"error\":\"invalid api key\"}";
        let response = format!(
            "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = s.write_all(response.as_bytes()).await;
        let _ = s.shutdown().await;
    });

    let client = reqwest::Client::builder().build().expect("client");
    let endpoint = format!("http://{}/v1/chat/completions", addr);
    let messages = vec![Message::user("hi")];

    let err = run_chat_stream(
        &client,
        &retry_policy(),
        &endpoint,
        Some("badkey"),
        "Authorization",
        "any-model",
        &messages,
        None,
        None,
        CancellationToken::new(),
        |_| {},
        |_| {},
    )
    .await
    .expect_err("should fail");

    let _ = server.await;

    match err {
        StreamError::Unauthorized { body } => {
            assert!(body.contains("invalid api key"), "body carried: {body}");
        }
        other => panic!("expected Unauthorized, got {other:?}"),
    }
}

/// End-to-end of the tool-call streaming path: the server returns
/// fragmented `tool_calls` deltas and a `finish_reason: "tool_calls"`,
/// and `run_chat_stream` should surface the assembled `ToolCall` list
/// in `outcome.tool_calls`.
#[tokio::test]
async fn streaming_assembles_tool_call_from_fragmented_deltas() {
    let (listener, addr) = bind_local().await;

    let server = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.expect("accept");
        let _ = read_request(&mut s).await;
        let body = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_abc\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"\"}}]}}]}\n\n\
                    data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n\
                    data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"/tmp/foo.txt\\\"}\"}}]}}]}\n\n\
                    data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
                    data: [DONE]\n\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = s.write_all(response.as_bytes()).await;
        let _ = s.shutdown().await;
    });

    let client = reqwest::Client::builder().build().expect("client");
    let endpoint = format!("http://{}/v1/chat/completions", addr);
    let messages = vec![Message::user("read /tmp/foo.txt")];

    let outcome = run_chat_stream(
        &client,
        &retry_policy(),
        &endpoint,
        None,
        "Authorization",
        "test-model",
        &messages,
        None,
        None,
        CancellationToken::new(),
        |_| {},
        |_| {},
    )
    .await
    .expect("ok");

    let _ = server.await;

    assert_eq!(outcome.finish_reason.as_deref(), Some("tool_calls"));
    assert_eq!(outcome.tool_calls.len(), 1);
    let tc = &outcome.tool_calls[0];
    assert_eq!(tc.id, "call_abc");
    assert_eq!(tc.kind, "function");
    assert_eq!(tc.function.name, "read_file");
    assert_eq!(tc.function.arguments, "{\"path\":\"/tmp/foo.txt\"}");
    assert!(
        outcome.full_text.is_empty(),
        "no prose in a pure tool-call turn"
    );
}

/// When tools are passed in, the request body must carry them in the
/// OpenAI-spec `tools: [{ "type": "function", "function": {...} }]`
/// array.
#[tokio::test]
async fn request_body_carries_tools_array_when_present() {
    let (listener, addr) = bind_local().await;
    let captured: std::sync::Arc<tokio::sync::Mutex<String>> =
        std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
    let captured_clone = captured.clone();

    let server = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.expect("accept");
        let mut buf = vec![0u8; 8192];
        let mut acc = String::new();
        loop {
            let n = s.read(&mut buf).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            acc.push_str(&String::from_utf8_lossy(&buf[..n]));
            if let Some(headers_end) = acc.find("\r\n\r\n") {
                let cl: usize = acc
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("content-length:")
                            .or_else(|| l.strip_prefix("Content-Length:"))
                    })
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                let body_so_far = acc.len() - (headers_end + 4);
                if body_so_far >= cl {
                    break;
                }
            }
        }
        if let Some(idx) = acc.find("\r\n\r\n") {
            *captured_clone.lock().await = acc[idx + 4..].to_owned();
        }
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n\
                    data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n\
                    data: [DONE]\n\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = s.write_all(response.as_bytes()).await;
        let _ = s.shutdown().await;
    });

    let client = reqwest::Client::builder().build().expect("client");
    let endpoint = format!("http://{}/v1/chat/completions", addr);
    let messages = vec![Message::user("hi")];

    let tools_array = vec![serde_json::json!({
        "type": "function",
        "function": {
            "name": "read_file",
            "description": "Read.",
            "parameters": {"type": "object"}
        }
    })];

    let _ = run_chat_stream(
        &client,
        &retry_policy(),
        &endpoint,
        None,
        "Authorization",
        "any-model",
        &messages,
        Some(&tools_array),
        None,
        CancellationToken::new(),
        |_| {},
        |_| {},
    )
    .await
    .expect("ok");

    let _ = server.await;

    let body = captured.lock().await.clone();
    let v: serde_json::Value = serde_json::from_str(&body).expect("body json");
    let tools = v
        .get("tools")
        .and_then(|t| t.as_array())
        .expect("tools array");
    assert_eq!(tools.len(), 1);
    assert_eq!(
        tools[0].get("type").and_then(serde_json::Value::as_str),
        Some("function")
    );
    let function = tools[0].get("function").expect("function wrapper");
    assert_eq!(
        function.get("name").and_then(serde_json::Value::as_str),
        Some("read_file")
    );
}

/// When no tools are attached, the `tools` field MUST NOT be on the
/// wire — keeping the request shape unchanged from v1 for setups that
/// don't load any tool plugins.
#[tokio::test]
async fn request_body_omits_tools_when_none_attached() {
    let (listener, addr) = bind_local().await;
    let captured: std::sync::Arc<tokio::sync::Mutex<String>> =
        std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
    let captured_clone = captured.clone();

    let server = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.expect("accept");
        let mut buf = vec![0u8; 8192];
        let mut acc = String::new();
        loop {
            let n = s.read(&mut buf).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            acc.push_str(&String::from_utf8_lossy(&buf[..n]));
            if let Some(headers_end) = acc.find("\r\n\r\n") {
                let cl: usize = acc
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("content-length:")
                            .or_else(|| l.strip_prefix("Content-Length:"))
                    })
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                let body_so_far = acc.len() - (headers_end + 4);
                if body_so_far >= cl {
                    break;
                }
            }
        }
        if let Some(idx) = acc.find("\r\n\r\n") {
            *captured_clone.lock().await = acc[idx + 4..].to_owned();
        }
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n\
                    data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n\
                    data: [DONE]\n\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = s.write_all(response.as_bytes()).await;
        let _ = s.shutdown().await;
    });

    let client = reqwest::Client::builder().build().expect("client");
    let endpoint = format!("http://{}/v1/chat/completions", addr);
    let messages = vec![Message::user("hi")];

    let _ = run_chat_stream(
        &client,
        &retry_policy(),
        &endpoint,
        None,
        "Authorization",
        "any-model",
        &messages,
        None,
        None,
        CancellationToken::new(),
        |_| {},
        |_| {},
    )
    .await
    .expect("ok");

    let _ = server.await;

    let body = captured.lock().await.clone();
    let v: serde_json::Value = serde_json::from_str(&body).expect("body json");
    assert!(v.get("tools").is_none(), "no tools field when None: {body}");
}

/// Reactive fallback contract: when the upstream returns 400 with the
/// "model does not support tools" signature AND we sent tools in the
/// request, `run_chat_stream` surfaces the dedicated
/// `StreamError::ToolsUnsupported` variant. The turn loop in main.rs
/// pattern-matches on this variant to mark the model + flip the chat's
/// `tool_allowlist` to empty and retry the iteration.
#[tokio::test]
async fn http_400_with_tools_unsupported_signature_yields_dedicated_variant() {
    let (listener, addr) = bind_local().await;

    let server = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.expect("accept");
        let _ = read_request(&mut s).await;
        // Verbatim body shape the user reported (translategemma).
        let body = r#"{"error":{"message":"registry.ollama.ai/library/translategemma:latest does not support tools"}}"#;
        let response = format!(
            "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = s.write_all(response.as_bytes()).await;
        let _ = s.shutdown().await;
    });

    let client = reqwest::Client::builder().build().expect("client");
    let endpoint = format!("http://{}/v1/chat/completions", addr);
    let messages = vec![Message::user("hi")];
    let tools_array = vec![serde_json::json!({
        "type": "function",
        "function": {
            "name": "read_file",
            "description": "Read.",
            "parameters": {"type": "object"}
        }
    })];

    let err = run_chat_stream(
        &client,
        &retry_policy(),
        &endpoint,
        None,
        "Authorization",
        "translategemma",
        &messages,
        Some(&tools_array),
        None,
        CancellationToken::new(),
        |_| {},
        |_| {},
    )
    .await
    .expect_err("400 should fail");

    let _ = server.await;

    match err {
        StreamError::ToolsUnsupported { body } => {
            assert!(
                body.contains("does not support tools"),
                "body carried: {body}"
            );
        }
        other => panic!("expected ToolsUnsupported, got {other:?}"),
    }
}

/// Disambiguator: same 400-with-signature body but the request did NOT
/// carry tools. The dedicated variant must NOT fire — we only treat
/// "tools" as the cause when we actually sent them. Falls through to the
/// generic `Http` variant so the user sees the underlying error.
#[tokio::test]
async fn http_400_without_tools_in_request_falls_through_to_http_error() {
    let (listener, addr) = bind_local().await;

    let server = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.expect("accept");
        let _ = read_request(&mut s).await;
        let body = r#"{"error":{"message":"some-model does not support tools"}}"#;
        let response = format!(
            "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = s.write_all(response.as_bytes()).await;
        let _ = s.shutdown().await;
    });

    let client = reqwest::Client::builder().build().expect("client");
    let endpoint = format!("http://{}/v1/chat/completions", addr);
    let messages = vec![Message::user("hi")];

    let err = run_chat_stream(
        &client,
        &retry_policy(),
        &endpoint,
        None,
        "Authorization",
        "some-model",
        &messages,
        None, // <-- the discriminator
        None,
        CancellationToken::new(),
        |_| {},
        |_| {},
    )
    .await
    .expect_err("400 should fail");

    let _ = server.await;

    match err {
        StreamError::Http { status, .. } => assert_eq!(status, 400),
        other => panic!("expected generic Http, got {other:?}"),
    }
}

/// End-to-end retry contract: when the upstream rejects tools on the
/// first call, the turn loop's reactive fallback (mark model + flip chat
/// flag → omit tools on retry) succeeds on the second call. Mirrors the
/// exact logic main.rs runs in the turn loop's `Err(ToolsUnsupported)`
/// arm. Two server requests on a single endpoint:
///   1) tools present → 400 with the signature.
///   2) tools omitted → 200 streaming response.
/// Asserts: both calls land on the same endpoint, the second omits the
/// `tools` field, and the cache correctly suppresses tools on retry.
#[tokio::test]
async fn reactive_fallback_retries_without_tools_after_signature_400() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    // Captured request bodies from each connection so we can assert the
    // wire shape changes between attempts.
    let req1: std::sync::Arc<tokio::sync::Mutex<String>> =
        std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
    let req2: std::sync::Arc<tokio::sync::Mutex<String>> =
        std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
    let req1_c = req1.clone();
    let req2_c = req2.clone();

    let server = tokio::spawn(async move {
        // First connection: respond with 400 + signature.
        let (mut s, _) = listener.accept().await.expect("accept 1");
        let mut buf = vec![0u8; 8192];
        let mut acc = String::new();
        loop {
            let n = s.read(&mut buf).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            acc.push_str(&String::from_utf8_lossy(&buf[..n]));
            if let Some(headers_end) = acc.find("\r\n\r\n") {
                let cl: usize = acc
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("content-length:")
                            .or_else(|| l.strip_prefix("Content-Length:"))
                    })
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                let body_so_far = acc.len() - (headers_end + 4);
                if body_so_far >= cl {
                    break;
                }
            }
        }
        if let Some(idx) = acc.find("\r\n\r\n") {
            *req1_c.lock().await = acc[idx + 4..].to_owned();
        }
        let body = r#"{"error":{"message":"translategemma:latest does not support tools"}}"#;
        let response = format!(
            "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = s.write_all(response.as_bytes()).await;
        let _ = s.shutdown().await;

        // Second connection: respond with the streaming success body.
        let (mut s, _) = listener.accept().await.expect("accept 2");
        let mut buf = vec![0u8; 8192];
        let mut acc = String::new();
        loop {
            let n = s.read(&mut buf).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            acc.push_str(&String::from_utf8_lossy(&buf[..n]));
            if let Some(headers_end) = acc.find("\r\n\r\n") {
                let cl: usize = acc
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("content-length:")
                            .or_else(|| l.strip_prefix("Content-Length:"))
                    })
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                let body_so_far = acc.len() - (headers_end + 4);
                if body_so_far >= cl {
                    break;
                }
            }
        }
        if let Some(idx) = acc.find("\r\n\r\n") {
            *req2_c.lock().await = acc[idx + 4..].to_owned();
        }
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
                    data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n\
                    data: [DONE]\n\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = s.write_all(response.as_bytes()).await;
        let _ = s.shutdown().await;
    });

    let chats = Chats::with_default_model(Some("translategemma".into()));
    let chat_id = ChatId::new("c1");
    chats
        .create(
            chat_id.clone(),
            Some("translategemma".into()),
            None,
            None,
            None,
            None,
        )
        .await
        .expect("create");

    let client = reqwest::Client::builder().build().expect("client");
    let endpoint = format!("http://{}/v1/chat/completions", addr);
    let messages = vec![Message::user("just chatting")];
    let tools_array = vec![serde_json::json!({
        "type": "function",
        "function": {
            "name": "read_file",
            "description": "Read.",
            "parameters": {"type": "object"}
        }
    })];

    // Mirror the turn-loop contract: build tools-array conditional on
    // both per-chat allowlist AND per-model cache.
    let chat_allowlist = chats.tool_allowlist(&chat_id).await.expect("allowlist");
    let model_on = chats.model_supports_tools("translategemma").await;
    let tools_disabled = !model_on || matches!(&chat_allowlist, Some(names) if names.is_empty());
    let tools_for_first: Option<&[serde_json::Value]> = if !tools_disabled {
        Some(&tools_array)
    } else {
        None
    };
    assert!(tools_for_first.is_some(), "first call must include tools");

    let first = run_chat_stream(
        &client,
        &retry_policy(),
        &endpoint,
        None,
        "Authorization",
        "translategemma",
        &messages,
        tools_for_first,
        None,
        CancellationToken::new(),
        |_| {},
        |_| {},
    )
    .await;

    // Assert the dedicated variant — exactly what main.rs's turn-loop
    // pattern-matches on.
    match first {
        Err(StreamError::ToolsUnsupported { body }) => {
            assert!(body.contains("does not support tools"));
        }
        other => panic!("expected ToolsUnsupported on first call, got {other:?}"),
    }

    // Reactive fallback: mirror the turn-loop's response — mark model +
    // disable tools on this chat via empty allowlist.
    chats.mark_model_tools_unsupported("translategemma").await;
    chats
        .set_tool_allowlist(&chat_id, Some(vec![]))
        .await
        .expect("disable tools");

    // Re-evaluate: the cache must now suppress tools.
    let chat_allowlist = chats.tool_allowlist(&chat_id).await.expect("allowlist");
    let model_on = chats.model_supports_tools("translategemma").await;
    let tools_disabled = !model_on || matches!(&chat_allowlist, Some(names) if names.is_empty());
    let tools_for_retry: Option<&[serde_json::Value]> = if !tools_disabled {
        Some(&tools_array)
    } else {
        None
    };
    assert!(
        tools_for_retry.is_none(),
        "retry must omit tools after marking model + chat as incapable"
    );

    let outcome = run_chat_stream(
        &client,
        &retry_policy(),
        &endpoint,
        None,
        "Authorization",
        "translategemma",
        &messages,
        tools_for_retry,
        None,
        CancellationToken::new(),
        |_| {},
        |_| {},
    )
    .await
    .expect("retry succeeds");

    let _ = server.await;

    assert_eq!(outcome.full_text, "hi");
    assert_eq!(outcome.finish_reason.as_deref(), Some("stop"));

    // Pin the wire shape: first request carried tools, second omitted them.
    let body1 = req1.lock().await.clone();
    let v1: serde_json::Value = serde_json::from_str(&body1).expect("body1 json");
    assert!(
        v1.get("tools").is_some(),
        "first request must carry tools field, body: {body1}"
    );
    let body2 = req2.lock().await.clone();
    let v2: serde_json::Value = serde_json::from_str(&body2).expect("body2 json");
    assert!(
        v2.get("tools").is_none(),
        "retry request must omit tools field, body: {body2}"
    );
}

/// Cache test: a fresh chat against a model already marked
/// tools-unsupported skips the round-trip — the very first request
/// against this chat omits tools without paying the 400 cost. This is
/// what makes the model-level cache valuable beyond the per-chat flag:
/// every chat against the incapable model is fast on the first turn.
#[tokio::test]
async fn marked_model_skips_tools_on_first_turn_of_a_brand_new_chat() {
    let chats = Chats::with_default_model(Some("translategemma".into()));
    chats.mark_model_tools_unsupported("translategemma").await;

    let chat_id = ChatId::new("fresh-chat");
    chats
        .create(
            chat_id.clone(),
            Some("translategemma".into()),
            None,
            None,
            None,
            None,
        )
        .await
        .expect("create");

    // Per-chat allowlist is still default (None = all tools), since the
    // chat never paid the round-trip. But the model cache vetoes tools.
    assert_eq!(
        chats.tool_allowlist(&chat_id).await.expect("allowlist"),
        None,
        "per-chat allowlist stays default (all tools)"
    );
    assert!(
        !chats.model_supports_tools("translategemma").await,
        "model cache says no"
    );

    // Combined gate (mirrors the turn-loop logic) → tools omitted.
    let model_on = chats.model_supports_tools("translategemma").await;
    assert!(
        !model_on,
        "combined gate should suppress tools on first turn for a known-incapable model"
    );

    // A different model is unaffected.
    assert!(chats.model_supports_tools("qwen3").await);
}

/// Disambiguator: an unrelated 400 (not the "does not support tools"
/// signature) must still surface as the generic `Http` variant even when
/// tools were sent. The tools-fallback logic only fires on the exact
/// signature, never on arbitrary 400s — otherwise a transient bad-request
/// would silently disable tools for the rest of the process.
#[tokio::test]
async fn http_400_with_unrelated_message_falls_through_to_http_error() {
    let (listener, addr) = bind_local().await;

    let server = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.expect("accept");
        let _ = read_request(&mut s).await;
        let body = r#"{"error":{"message":"context length exceeded"}}"#;
        let response = format!(
            "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = s.write_all(response.as_bytes()).await;
        let _ = s.shutdown().await;
    });

    let client = reqwest::Client::builder().build().expect("client");
    let endpoint = format!("http://{}/v1/chat/completions", addr);
    let messages = vec![Message::user("hi")];
    let tools_array = vec![serde_json::json!({
        "type": "function",
        "function": {
            "name": "read_file",
            "description": "Read.",
            "parameters": {"type": "object"}
        }
    })];

    let err = run_chat_stream(
        &client,
        &retry_policy(),
        &endpoint,
        None,
        "Authorization",
        "any-model",
        &messages,
        Some(&tools_array),
        None,
        CancellationToken::new(),
        |_| {},
        |_| {},
    )
    .await
    .expect_err("400 should fail");

    let _ = server.await;

    match err {
        StreamError::Http { status, body } => {
            assert_eq!(status, 400);
            assert!(body.contains("context length exceeded"));
        }
        other => panic!("expected generic Http, got {other:?}"),
    }
}

async fn run_scripted_stream(
    body: String,
) -> (
    Result<openai_provider::stream::StreamOutcome, StreamError>,
    Vec<String>,
    Vec<String>,
) {
    let (listener, addr) = bind_local().await;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_request(&mut stream).await;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(), body
        );
        stream
            .write_all(response.as_bytes())
            .await
            .expect("response");
    });
    let mut deltas = Vec::new();
    let mut reasoning = Vec::new();
    let result = run_chat_stream(
        &reqwest::Client::new(),
        &retry_policy(),
        &format!("http://{addr}/v1/chat/completions"),
        None,
        "Authorization",
        "fixture-model",
        &[Message::user("fixture")],
        None,
        None,
        CancellationToken::new(),
        |text| deltas.push(text.to_owned()),
        |event| {
            if let openai_provider::stream::ReasoningEvent::Delta(text) = event {
                reasoning.push(text.to_owned());
            }
        },
    )
    .await;
    server.await.expect("server");
    (result, deltas, reasoning)
}

#[tokio::test]
async fn repeated_identical_usage_payloads_dedupe_but_conflicts_fail() {
    let identical = concat!(
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1}}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1}}\n\n",
        "data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    )
    .to_owned();
    let (result, _, _) = run_scripted_stream(identical).await;
    assert_eq!(
        result
            .expect("identical usage dedupes")
            .usage
            .expect("usage")
            .prompt_tokens,
        Some(1)
    );

    let conflicting = concat!(
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1}}\n\n",
        "data: {\"choices\":[],\"usage\":{\"completion_tokens\":2}}\n\n"
    )
    .to_owned();
    let (result, _, _) = run_scripted_stream(conflicting).await;
    assert!(
        matches!(result, Err(StreamError::Malformed(message)) if message.contains("conflicting usage"))
    );
}

#[tokio::test]
async fn conflicting_completion_ids_are_rejected() {
    let body = concat!(
        "data: {\"id\":\"first\",\"choices\":[{\"delta\":{\"content\":\"part\"}}]}\n\n",
        "data: {\"id\":\"second\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n"
    )
    .to_owned();
    let (result, _, _) = run_scripted_stream(body).await;
    assert!(matches!(result, Err(StreamError::Malformed(_))));
}

#[tokio::test]
async fn cancellation_before_usage_does_not_fabricate_accounting() {
    let (listener, addr) = bind_local().await;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_request(&mut stream).await;
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("headers");
        stream.write_all(
            b"data: {\"id\":\"completion-before\",\"choices\":[{\"delta\":{\"content\":\"part\"}}]}\n\n",
        ).await.expect("delta");
        stream.flush().await.expect("flush");
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    });
    let cancel = CancellationToken::new();
    let cancel_after_delta = cancel.clone();
    let outcome = run_chat_stream(
        &reqwest::Client::new(),
        &retry_policy(),
        &format!("http://{addr}/v1/chat/completions"),
        None,
        "Authorization",
        "fixture-model",
        &[Message::user("fixture")],
        None,
        None,
        cancel,
        move |_| cancel_after_delta.cancel(),
        |_| {},
    )
    .await
    .expect("cancelled stream outcome");
    server.abort();
    assert!(outcome.interrupted);
    assert!(outcome.usage.is_none());
}

#[tokio::test]
async fn terminal_usage_and_completion_id_survive_cancellation_before_done() {
    let (listener, addr) = bind_local().await;
    let (usage_sent, usage_visible) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_request(&mut stream).await;
        let headers =
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
        stream.write_all(headers.as_bytes()).await.expect("headers");
        stream
            .write_all(concat!(
                "data: {\"id\":\"completion-cancel\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                "data: {\"id\":\"completion-cancel\",\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"provider_marker\":{\"retained\":true}}}\n\n"
            ).as_bytes())
            .await
            .expect("terminal frames");
        stream.flush().await.expect("flush");
        let _ = usage_sent.send(());
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    });
    let cancel = CancellationToken::new();
    let cancel_after_usage = cancel.clone();
    tokio::spawn(async move {
        usage_visible.await.expect("usage sent");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cancel_after_usage.cancel();
    });
    let outcome = run_chat_stream(
        &reqwest::Client::new(),
        &retry_policy(),
        &format!("http://{addr}/v1/chat/completions"),
        None,
        "Authorization",
        "fixture-model",
        &[Message::user("fixture")],
        None,
        None,
        cancel,
        |_| {},
        |_| {},
    )
    .await
    .expect("cancelled stream outcome");
    server.abort();

    assert!(outcome.interrupted);
    assert_eq!(outcome.completion_id.as_deref(), Some("completion-cancel"));
    let usage = outcome
        .usage
        .expect("authoritative usage survives cancellation");
    assert_eq!(usage.prompt_tokens, Some(7));
    assert_eq!(usage.extensions["provider_marker"]["retained"], true);
}

#[tokio::test]
async fn mixed_frames_preserve_content_reasoning_tools_finish_and_usage() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"answer\",\"reasoning\":\"thought\",\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":3,\"total_tokens\":5}}\n\n",
        "data: [DONE]\n\n"
    ).to_owned();
    let (result, deltas, reasoning) = run_scripted_stream(body).await;
    let outcome = result.expect("valid mixed stream");
    assert_eq!(deltas, ["answer"]);
    assert_eq!(reasoning, ["thought"]);
    assert_eq!(outcome.full_text, "answer");
    assert_eq!(outcome.reasoning_text, "thought");
    assert_eq!(
        outcome.reasoning_continuation,
        Some(ReasoningContinuation::Reasoning {
            reasoning: "thought".into(),
        })
    );
    assert_eq!(outcome.finish_reason.as_deref(), Some("tool_calls"));
    assert_eq!(outcome.usage.expect("usage").total_tokens, Some(5));
    assert_eq!(outcome.tool_calls.len(), 1);
    assert_eq!(outcome.tool_calls[0].function.arguments, "{}");
}

#[tokio::test]
async fn reasoning_content_reaches_the_reasoning_stream_accumulator() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thought\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    )
    .to_owned();
    let (result, deltas, reasoning) = run_scripted_stream(body).await;
    let outcome = result.expect("valid reasoning_content stream");
    assert_eq!(deltas, ["answer"]);
    assert_eq!(reasoning, ["thought"]);
    assert_eq!(outcome.full_text, "answer");
    assert_eq!(outcome.reasoning_text, "thought");
    assert_eq!(
        outcome.reasoning_continuation,
        Some(ReasoningContinuation::Content {
            reasoning_content: "thought".into()
        })
    );
}

#[tokio::test]
async fn streamed_reasoning_details_preserve_opaque_blocks_and_unknown_fields() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"reasoning_details\":[{\"type\":\"reasoning.text\",\"text\":\"first\",\"index\":0,\"future\":true}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"reasoning_details\":[{\"type\":\"reasoning.encrypted\",\"data\":\"sealed\",\"index\":1}],\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n"
    )
    .to_owned();
    let (result, _, _) = run_scripted_stream(body).await;
    let outcome = result.expect("valid structured reasoning stream");
    assert_eq!(outcome.tool_calls[0].id, "call_1");
    assert_eq!(
        serde_json::to_value(outcome.reasoning_continuation.expect("continuation"))
            .expect("serialize continuation"),
        serde_json::json!({
            "reasoning_details": [
                {"type":"reasoning.text","text":"first","index":0,"future":true},
                {"type":"reasoning.encrypted","data":"sealed","index":1}
            ]
        })
    );
}

#[tokio::test]
async fn clean_eof_does_not_complete_partial_native_reasoning() {
    for native in [
        serde_json::json!({"reasoning":"unfinished thought"}),
        serde_json::json!({"reasoning_content":"unfinished thought"}),
        serde_json::json!({"reasoning_details":[{"type":"reasoning.encrypted","data":"partial"}]}),
    ] {
        let body = format!(
            "data: {}\n\ndata: {{\"choices\":[{{\"delta\":{{\"content\":\"partial answer\"}}}}]}}\n\n",
            serde_json::json!({"choices":[{"delta":native}]}),
        );
        let (result, deltas, _) = run_scripted_stream(body).await;
        assert_eq!(
            deltas,
            ["partial answer"],
            "displayed text remains partial output"
        );
        assert!(matches!(result, Err(StreamError::Body(message))
            if message == "stream ended before a complete semantic response"));
    }
}

#[tokio::test]
async fn native_reasoning_with_finish_reason_survives_clean_eof_without_done() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"complete thought\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"},\"finish_reason\":\"stop\"}]}\n\n",
    )
    .to_owned();
    let (result, _, _) = run_scripted_stream(body).await;
    let outcome = result.expect("semantic completion is sufficient");
    assert_eq!(
        outcome.reasoning_continuation,
        Some(ReasoningContinuation::Content {
            reasoning_content: "complete thought".into(),
        })
    );
}

#[tokio::test]
async fn structured_reasoning_takes_precedence_over_both_plaintext_aliases() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"reasoning\":\"raw\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\" content\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"reasoning_details\":[{\"type\":\"reasoning.encrypted\",\"data\":\"sealed\"}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"reasoning\":\" after details\"},\"finish_reason\":\"stop\"}]}\n\n",
    ).to_owned();
    let (result, _, _) = run_scripted_stream(body).await;
    let outcome = result.expect("completed reasoning");
    assert_eq!(outcome.reasoning_text, "raw content after details");
    assert_eq!(
        outcome
            .reasoning_continuation
            .expect("native details")
            .artifact(),
        serde_json::json!({"reasoning_details":[{"type":"reasoning.encrypted","data":"sealed"}]})
    );
}

#[tokio::test]
async fn pairwise_mixed_frames_reach_downstream_accumulators() {
    let frames = [
        (
            r#"{"choices":[{"delta":{"content":"c"},"finish_reason":"stop"}]}"#,
            "content_finish",
        ),
        (
            r#"{"choices":[{"delta":{"content":"c","tool_calls":[{"index":0,"id":"x","type":"function","function":{"name":"f","arguments":"{}"}}]}}]}"#,
            "content_tool",
        ),
        (
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"x","type":"function","function":{"name":"f","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
            "tool_finish",
        ),
        (
            r#"{"choices":[{"delta":{"reasoning":"r","tool_calls":[{"index":0,"id":"x","type":"function","function":{"name":"f","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
            "reasoning_tool",
        ),
        (
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}"#,
            "finish_usage",
        ),
        (
            r#"{"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}"#,
            "usage_only",
        ),
    ];
    for (json, name) in frames {
        let terminal = match name {
            "content_tool" => "data: {\"choices\":[{\"finish_reason\":\"tool_calls\"}]}\n\n",
            "usage_only" => "data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n",
            _ => "",
        };
        let body = format!("data: {json}\n\n{terminal}");
        let (result, deltas, reasoning) = run_scripted_stream(body).await;
        let outcome = result.expect(name);
        match name {
            "content_finish" => {
                assert_eq!(deltas, ["c"]);
                assert_eq!(outcome.finish_reason.as_deref(), Some("stop"));
            }
            "content_tool" => {
                assert_eq!(deltas, ["c"]);
                assert_eq!(outcome.tool_calls.len(), 1);
            }
            "tool_finish" => {
                assert_eq!(outcome.tool_calls.len(), 1);
                assert_eq!(outcome.finish_reason.as_deref(), Some("tool_calls"));
            }
            "reasoning_tool" => {
                assert_eq!(reasoning, ["r"]);
                assert_eq!(outcome.tool_calls.len(), 1);
            }
            "finish_usage" => {
                assert_eq!(outcome.finish_reason.as_deref(), Some("stop"));
                assert_eq!(outcome.usage.expect("usage").total_tokens, Some(3));
            }
            "usage_only" => assert_eq!(outcome.usage.expect("usage").total_tokens, Some(3)),
            _ => unreachable!(),
        }
    }
}

#[tokio::test]
async fn tool_fields_accumulate_independently_by_interleaved_index() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"function\":{\"arguments\":\"{\\\"b\\\":\"}},{\"index\":0,\"id\":\"a\"}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{}\"}},{\"index\":1,\"id\":\"b\",\"type\":\"function\"}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"function\":{\"name\":\"second\",\"arguments\":\"2}\"}},{\"index\":0,\"type\":\"function\",\"function\":{\"name\":\"first\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n"
    ).to_owned();
    let (result, _, _) = run_scripted_stream(body).await;
    let calls = result.expect("complete calls").tool_calls;
    assert_eq!(
        calls
            .iter()
            .map(|call| call.id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert_eq!(calls[0].function.name, "first");
    assert_eq!(calls[1].function.arguments, r#"{"b":2}"#);
}

#[tokio::test]
async fn incomplete_tool_refusal_malformed_and_in_band_error_are_typed_failures() {
    let cases = [
        ("data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n", "incomplete"),
        ("data: {\"choices\":[{\"delta\":{\"refusal\":\"cannot comply\"},\"finish_reason\":\"stop\"}]}\n\n", "refusal"),
        ("data: {bad\n\n", "malformed"),
        ("data: {\"error\":{\"message\":\"quota exhausted\",\"type\":\"insufficient_quota\"}}\n\n", "provider"),
    ];
    for (body, expected) in cases {
        let (result, _, _) = run_scripted_stream(body.to_owned()).await;
        let error = result.expect_err(expected);
        match (expected, error) {
            ("incomplete", StreamError::IncompleteToolCall { index: 0, .. })
            | ("refusal", StreamError::Refusal(_))
            | ("malformed", StreamError::Malformed(_))
            | ("provider", StreamError::Provider { .. }) => {}
            (_, other) => panic!("unexpected {expected} error: {other:?}"),
        }
    }
}

fn retry_policy() -> openai_provider::config::RetryPolicy {
    openai_provider::config::RetryPolicy::from_json(r#"{"max_attempts":8,"max_rounds":1,"backoff_multiplier":2,"initial_delay_ms":500,"max_delay_ms":30000,"round_cooldown_ms":0,"jitter_bps":0,"response_headers_timeout_ms":120000}"#).expect("explicit fixture retry policy")
}

fn rounds_policy(attempts: usize, rounds: usize) -> openai_provider::config::RetryPolicy {
    openai_provider::config::RetryPolicy::from_json(
        &serde_json::json!({
            "max_attempts": attempts, "max_rounds": rounds,
            "backoff_multiplier":2,"initial_delay_ms": 500, "max_delay_ms": 30000,
            "round_cooldown_ms": 60000, "jitter_bps": 0,
            "response_headers_timeout_ms": 120000
        })
        .to_string(),
    )
    .expect("explicit rounds fixture policy")
}

// Additional reply scripts for the shared HTTP ledger. Zero already denotes
// a connection loss before headers; real HTTP statuses remain unchanged.
const EMPTY_STREAM_LOSS: u16 = 1;
const IN_BAND_ERROR: u16 = 2;
const MALFORMED_STREAM: u16 = 3;
const QUOTA_DENIAL: u16 = 4;
const POST_TEXT_STREAM_LOSS: u16 = 5;
const CLEAN_EMPTY_EOF: u16 = 6;

/// Keep a runnable task while real localhost I/O is pending: Tokio's automatic
/// paused-clock advancement must not mistake socket latency for a header timeout.
/// Only an observed retry notice authorizes advancing the clock here.
async fn scripted_retry_ledger(
    statuses: Vec<u16>,
    attempts_per_round: usize,
    rounds: usize,
) -> (
    Result<openai_provider::stream::StreamOutcome, StreamError>,
    Vec<RetryProgress>,
    Vec<String>,
    std::time::Duration,
) {
    let (listener, addr) = bind_local().await;
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let server = tokio::spawn(async move {
        let mut index = 0;
        loop {
            let (mut socket, _) = listener.accept().await.expect("accept scripted request");
            let mut request = read_request(&mut socket).await;
            let (headers, body) = request.split_once("\r\n\r\n").expect("HTTP headers");
            let expected_length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")?
                        .parse::<usize>()
                        .ok()
                })
                .expect("request content length");
            let mut body_length = body.len();
            while body_length < expected_length {
                let mut bytes = [0; 4096];
                let count = socket.read(&mut bytes).await.expect("request body");
                assert!(count > 0, "complete request body");
                body_length += count;
                request.push_str(std::str::from_utf8(&bytes[..count]).expect("UTF-8 fixture body"));
            }
            recorded.lock().unwrap().push(request);
            let status = statuses.get(index).copied().unwrap_or(429);
            index += 1;
            if status == 0 {
                // Dropping without headers is a replay-safe request transport error.
                continue;
            }
            let (http_status, content_type, body, truncate) = match status {
                EMPTY_STREAM_LOSS => (200, "text/event-stream", "", true),
                CLEAN_EMPTY_EOF => (200, "text/event-stream", ": keepalive\n\ndata: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n", false),
                IN_BAND_ERROR => (200, "text/event-stream", "data: {\"error\":{\"message\":\"rate limited\"}}\n\n", false),
                MALFORMED_STREAM => (200, "text/event-stream", "data: {bad\n\n", false),
                QUOTA_DENIAL => (429, "application/json", "{\"error\":{\"type\":\"insufficient_quota\"}}", false),
                POST_TEXT_STREAM_LOSS => (200, "text/event-stream", "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n", true),
                200 => (200, "text/event-stream", "data: {\"choices\":[{\"delta\":{\"content\":\"recovered\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n", false),
                status => (status, "application/json", "{\"error\":\"fixture response\"}", false),
            };
            let length = body.len() + usize::from(truncate);
            let response = format!("HTTP/1.1 {http_status} Fixture\r\nContent-Type: {content_type}\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n{body}");
            socket
                .write_all(response.as_bytes())
                .await
                .expect("scripted response");
        }
    });
    let keep_clock_still = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let start = tokio::time::Instant::now();
    let mut completion = tokio::spawn(async move {
        run_chat_stream_with_retry_progress(
            &reqwest::Client::new(),
            &rounds_policy(attempts_per_round, rounds),
            &format!("http://{addr}/v1/chat/completions"),
            None,
            "Authorization",
            "fixture-model",
            &[Message::user("same logical request")],
            None,
            None,
            CancellationToken::new(),
            |_| {},
            |_| {},
            move |notice| {
                progress_tx.send(notice).expect("progress receiver");
            },
        )
        .await
    });
    let mut ledger = Vec::new();
    let outcome = loop {
        tokio::select! {
            biased;
            Some(notice) = progress_rx.recv() => {
                ledger.push(notice);
                tokio::time::advance(notice.next_delay).await;
            }
            result = &mut completion => break result.expect("completion task"),
        }
    };
    let elapsed = start.elapsed();
    server.abort();
    keep_clock_still.abort();
    let requests = requests.lock().unwrap().clone();
    (outcome, ledger, requests, elapsed)
}

#[tokio::test(start_paused = true)]
async fn persistent_429_consumes_exactly_eight_attempts_in_each_of_six_rounds() {
    let (result, progress, requests, elapsed) = scripted_retry_ledger(vec![429; 48], 8, 6).await;
    let StreamError::Exhausted { retry, source } = result.expect_err("finite rate-limit budget")
    else {
        panic!("expected exhaustion")
    };
    assert_eq!(requests.len(), 48, "no 49th send");
    assert_eq!(
        (
            retry.reason.as_str(),
            retry.round,
            retry.attempt,
            retry.total_attempts
        ),
        ("rounds_exhausted", 6, 8, 48)
    );
    assert_eq!(
        (retry.category.as_str(), retry.status),
        ("rate_limit", Some(429))
    );
    assert!(matches!(*source, StreamError::Http { status: 429, .. }));
    assert_eq!(progress.len(), 47, "no wait after final send");
    assert_eq!(
        progress
            .iter()
            .filter(|p| p.phase.as_str() == "attempt")
            .count(),
        42
    );
    assert_eq!(
        progress
            .iter()
            .filter(|p| p.phase.as_str() == "round")
            .count(),
        5
    );
    let inner_ms = [500, 1000, 2000, 4000, 8000, 16000, 30000];
    for (index, notice) in progress.iter().enumerate() {
        let round = index / 8 + 1;
        let attempt = index % 8 + 1;
        assert_eq!(
            (
                notice.round,
                notice.max_rounds,
                notice.failed_attempt,
                notice.max_attempts
            ),
            (round, 6, attempt, 8)
        );
        assert_eq!(notice.total_attempts, index + 1);
        assert_eq!(
            (notice.category.as_str(), notice.status),
            ("rate_limit", Some(429))
        );
        if attempt == 8 {
            assert_eq!(
                (
                    notice.phase.as_str(),
                    notice.next_round,
                    notice.next_attempt
                ),
                ("round", round + 1, 1)
            );
            assert_eq!(notice.next_delay, std::time::Duration::from_secs(60));
        } else {
            assert_eq!(
                (
                    notice.phase.as_str(),
                    notice.next_round,
                    notice.next_attempt
                ),
                ("attempt", round, attempt + 1)
            );
            assert_eq!(
                notice.next_delay,
                std::time::Duration::from_millis(inner_ms[attempt - 1])
            );
        }
    }
    assert_eq!(
        elapsed,
        std::time::Duration::from_millis(6 * inner_ms.iter().sum::<u64>() + 5 * 60000)
    );
    let bodies: Vec<_> = requests
        .iter()
        .map(|request| request.split_once("\r\n\r\n").expect("HTTP body").1)
        .collect();
    assert!(
        bodies.iter().all(|body| *body == bodies[0]),
        "rounds reuse the prepared conversation and request body"
    );
}

#[tokio::test(start_paused = true)]
async fn last_attempt_can_succeed_in_first_or_last_round() {
    for total in [8, 48] {
        let mut statuses = vec![429; total - 1];
        statuses.push(200);
        let (result, progress, requests, _) = scripted_retry_ledger(statuses, 8, 6).await;
        assert_eq!(
            result.expect("last attempt succeeds").full_text,
            "recovered"
        );
        assert_eq!(requests.len(), total);
        assert_eq!(progress.len(), total - 1);
    }
}

#[tokio::test(start_paused = true)]
async fn explicit_small_budgets_change_sends_and_reset_attempts() {
    let (result, progress, requests, elapsed) = scripted_retry_ledger(vec![429], 1, 1).await;
    assert!(matches!(result, Err(StreamError::Exhausted { .. })));
    assert_eq!(requests.len(), 1);
    assert!(progress.is_empty());
    assert_eq!(elapsed, std::time::Duration::ZERO);
    let (result, progress, requests, _) =
        scripted_retry_ledger(vec![429, 429, 429, 200], 2, 2).await;
    assert!(result.is_ok());
    assert_eq!(requests.len(), 4);
    assert_eq!(
        progress
            .iter()
            .map(|p| (p.round, p.failed_attempt, p.next_round, p.next_attempt))
            .collect::<Vec<_>>(),
        [(1, 1, 1, 2), (1, 2, 2, 1), (2, 1, 2, 2)]
    );
}

#[tokio::test(start_paused = true)]
async fn outer_eligibility_retains_current_round_429_but_not_previous_round_429() {
    for final_status in [503, 0] {
        let (result, progress, requests, _) =
            scripted_retry_ledger(vec![429, final_status, 503, 503], 2, 6).await;
        let StreamError::Exhausted { retry, .. } =
            result.expect_err("second round lacks backpressure")
        else {
            panic!("expected exhaustion")
        };
        assert_eq!(requests.len(), 4);
        assert_eq!(
            (
                retry.reason.as_str(),
                retry.round,
                retry.attempt,
                retry.total_attempts
            ),
            ("attempts_exhausted", 2, 2, 4)
        );
        assert_eq!(progress[1].phase.as_str(), "round");
        assert_eq!(
            progress[1].status,
            if final_status == 0 {
                None
            } else {
                Some(final_status)
            }
        );
        assert_eq!(
            progress[1].category.as_str(),
            if final_status == 0 {
                "transport"
            } else {
                "server"
            }
        );
        assert_eq!(
            progress
                .iter()
                .filter(|p| p.phase.as_str() == "round")
                .count(),
            1
        );
    }
    for status in [503, 0] {
        let (result, progress, requests, _) = scripted_retry_ledger(vec![status; 8], 8, 6).await;
        let StreamError::Exhausted { retry, .. } = result.expect_err("no 429 means one round")
        else {
            panic!("expected exhaustion")
        };
        assert_eq!(requests.len(), 8);
        assert_eq!(retry.reason.as_str(), "attempts_exhausted");
        assert!(progress.iter().all(|p| p.phase.as_str() == "attempt"));
    }
}

#[tokio::test(start_paused = true)]
async fn terminal_status_after_429_wins_without_exhaustion_or_outer_retry() {
    for status in [400, 401] {
        let (result, progress, requests, _) = scripted_retry_ledger(vec![429, status], 2, 6).await;
        let error = result.expect_err("terminal status");
        if status == 401 {
            assert!(matches!(error, StreamError::Unauthorized { .. }));
        } else {
            assert!(matches!(error, StreamError::Http { status: 400, .. }));
        }
        assert_eq!(requests.len(), 2);
        assert_eq!(progress.len(), 1);
    }
}

#[tokio::test]
async fn cancellation_interrupts_stalled_headers_error_body_and_wrong_content_type_body() {
    for headers in [
        "",
        "HTTP/1.1 429 Fixture\r\nContent-Type: application/json\r\nContent-Length: 1000\r\n\r\n{",
        "HTTP/1.1 200 Fixture\r\nContent-Type: application/json\r\nContent-Length: 1000\r\n\r\n{",
    ] {
        let (listener, addr) = bind_local().await;
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("request");
            let _ = read_request(&mut socket).await;
            socket
                .write_all(headers.as_bytes())
                .await
                .expect("partial response");
            ready_tx.send(()).expect("ready receiver");
            // Hold the connection open after cancellation to distinguish it from
            // a body EOF; still observe whether an unauthorized resend arrives.
            match tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
                .await
            {
                Err(_) => {}
                Ok(_) => panic!("cancellation must not send another request"),
            }
        });
        let completion = tokio::spawn(async move {
            run_chat_stream_with_retry_progress(
                &reqwest::Client::new(),
                &rounds_policy(2, 2),
                &format!("http://{addr}/v1/chat/completions"),
                None,
                "Authorization",
                "fixture-model",
                &[Message::user("cancel stalled request")],
                None,
                None,
                token,
                |_| panic!("no semantic output"),
                |_| {},
                |_| panic!("no retry after cancellation"),
            )
            .await
        });
        ready_rx.await.expect("server holds response open");
        cancel.cancel();
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), completion)
            .await
            .expect("cancel must interrupt header/body acquisition")
            .expect("completion task")
            .expect("cancel is not exhaustion");
        assert!(outcome.interrupted);
        server.await.expect("no replay");
    }
}

#[tokio::test]
async fn cancellation_during_outer_cooldown_has_no_next_send() {
    let (listener, addr) = bind_local().await;
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("first request");
        let _ = read_request(&mut socket).await;
        socket
            .write_all(b"HTTP/1.1 429 Fixture\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .expect("429");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "no send during cancelled cooldown"
        );
    });
    let cancel = CancellationToken::new();
    let cancel_from_progress = cancel.clone();
    let mut progress = Vec::new();
    let outcome = run_chat_stream_with_retry_progress(
        &reqwest::Client::new(),
        &rounds_policy(1, 6),
        &format!("http://{addr}/v1/chat/completions"),
        None,
        "Authorization",
        "fixture-model",
        &[Message::user("cancel cooldown")],
        None,
        None,
        cancel,
        |_| {},
        |_| {},
        |notice| {
            progress.push(notice);
            cancel_from_progress.cancel();
        },
    )
    .await
    .expect("cancel is not exhausted");
    assert!(outcome.interrupted);
    assert_eq!(progress.len(), 1);
    assert_eq!(
        (
            progress[0].phase.as_str(),
            progress[0].failed_attempt,
            progress[0].next_round,
            progress[0].next_attempt
        ),
        ("round", 1, 2, 1)
    );
    server.await.expect("no next request");
}

#[tokio::test]
async fn semantic_state_blocks_transport_replay_even_when_structured_text_is_hidden() {
    for (delta, structured) in [
        (serde_json::json!({"content":"partial answer"}), false),
        (serde_json::json!({"reasoning":"partial thought"}), false),
        (
            serde_json::json!({"reasoning_details":[{"type":"reasoning.encrypted","data":"sealed"}]}),
            false,
        ),
        (
            serde_json::json!({"tool_calls":[{"index":0,"id":"partial-tool"}]}),
            false,
        ),
        (serde_json::json!({"content":"{\"partial\":"}), true),
    ] {
        let (listener, addr) = bind_local().await;
        let body = format!(
            "data: {}\n\n",
            serde_json::json!({"choices":[{"delta":delta}]})
        );
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("request");
            let _ = read_request(&mut socket).await;
            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len() + 100);
            socket
                .write_all(response.as_bytes())
                .await
                .expect("semantic fragment");
            socket.shutdown().await.expect("truncated body");
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
                    .await
                    .is_err(),
                "semantic state forbids a resend"
            );
        });
        let format = serde_json::json!({"type":"json_object"});
        let mut deltas = Vec::new();
        let result = run_chat_stream_with_retry_progress_and_format(
            &reqwest::Client::new(),
            &rounds_policy(2, 6),
            &format!("http://{addr}/v1/chat/completions"),
            None,
            "Authorization",
            "fixture-model",
            &[Message::user("do not replay partial output")],
            None,
            None,
            structured.then_some(&format),
            CancellationToken::new(),
            |text| {
                if !structured {
                    deltas.push(text.to_owned());
                }
            },
            |_| {},
            |_| panic!("semantic failure cannot retry"),
        )
        .await;
        assert!(
            matches!(result, Err(StreamError::Body(_))),
            "must preserve non-budget body failure: {result:?}"
        );
        if structured {
            assert!(
                deltas.is_empty(),
                "structured text stays hidden but still forbids replay"
            );
        }
        server.await.expect("no replay");
    }
}

#[tokio::test(start_paused = true)]
async fn shared_counter_includes_presemantic_sse_loss_and_terminal_failures_override_prior_429() {
    let (result, progress, requests, _) =
        scripted_retry_ledger(vec![429, 0, EMPTY_STREAM_LOSS, 503, 503, 503], 3, 6).await;
    let StreamError::Exhausted { retry, .. } = result.unwrap_err() else {
        panic!("exhausted")
    };
    assert_eq!(requests.len(), 6);
    assert_eq!(
        (retry.round, retry.attempt, retry.total_attempts),
        (2, 3, 6)
    );
    assert_eq!(retry.reason.as_str(), "attempts_exhausted");
    assert_eq!(progress[2].category.as_str(), "stream_transport");
    assert_eq!(progress[2].phase.as_str(), "round");
    assert_eq!(progress[2].total_attempts, 3);
    for final_reply in [
        IN_BAND_ERROR,
        MALFORMED_STREAM,
        QUOTA_DENIAL,
        POST_TEXT_STREAM_LOSS,
    ] {
        let (result, progress, requests, _) =
            scripted_retry_ledger(vec![429, final_reply], 2, 6).await;
        let error = result.expect_err("terminal result beats earlier rate-limit eligibility");
        assert!(!matches!(error, StreamError::Exhausted { .. }));
        assert_eq!(requests.len(), 2);
        assert_eq!(progress.len(), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn clean_presemantic_eof_uses_the_shared_bounded_ledger() {
    let (result, progress, requests, _) =
        scripted_retry_ledger(vec![CLEAN_EMPTY_EOF, CLEAN_EMPTY_EOF, 200], 3, 1).await;
    assert_eq!(
        result.expect("recovered before output").full_text,
        "recovered"
    );
    assert_eq!(requests.len(), 3);
    assert_eq!(progress.len(), 2);
    assert!(progress
        .iter()
        .all(|notice| notice.category.as_str() == "stream_transport"));

    let (result, _, requests, _) = scripted_retry_ledger(vec![CLEAN_EMPTY_EOF; 3], 3, 6).await;
    assert!(matches!(result, Err(StreamError::Exhausted { .. })));
    assert_eq!(
        requests.len(),
        3,
        "body failure alone cannot open outer rounds"
    );
}

#[tokio::test]
async fn semantic_fragments_never_replay_on_clean_or_chunked_eof() {
    for delta in [
        serde_json::json!({"content":"partial"}),
        serde_json::json!({"reasoning":"partial"}),
        serde_json::json!({"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"effect","arguments":"{}"}}]}),
    ] {
        for framing in ["clean", "chunk-size", "chunk-body"] {
            let (listener, addr) = bind_local().await;
            let body = format!(
                "data: {}\n\n",
                serde_json::json!({"choices":[{"delta":delta}]})
            );
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.expect("request");
                let _ = read_request(&mut socket).await;
                let response = if framing == "clean" {
                    format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
                } else {
                    let tail = if framing == "chunk-size" {
                        "a"
                    } else {
                        "a\r\nxx"
                    };
                    format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{body}\r\n{tail}", body.len())
                };
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("response");
                socket.shutdown().await.expect("EOF");
                assert!(
                    tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
                        .await
                        .is_err(),
                    "no resend after semantic state"
                );
            });
            let result = run_chat_stream_with_retry_progress(
                &reqwest::Client::new(),
                &rounds_policy(2, 6),
                &format!("http://{addr}/v1/chat/completions"),
                None,
                "Authorization",
                "fixture-model",
                &[Message::user("no replay")],
                None,
                None,
                CancellationToken::new(),
                |_| {},
                |_| {},
                |_| panic!("no retry"),
            )
            .await;
            assert!(
                matches!(result, Err(StreamError::Body(_))),
                "{framing}: {result:?}"
            );
            server.await.expect("no replay");
        }
    }
}
