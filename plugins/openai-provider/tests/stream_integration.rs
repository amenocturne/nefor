//! Integration tests for the streaming HTTP path.
//!
//! Spins up a hand-rolled `tokio::net::TcpListener` that speaks just
//! enough HTTP/1.1 to satisfy reqwest's streaming reader. Avoids
//! pulling in axum/hyper-server for one test file.

use openai_provider::openai::Message;
use openai_provider::state::{ChatId, Chats};
use openai_provider::stream::{
    list_models, run_chat_stream, run_chat_stream_with_retry_progress, RetryProgress, StreamError,
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
    assert_eq!(progress[0].retry_index(), 1);
    assert_eq!(progress[0].max_retries(), 2);
    assert_eq!(
        progress[0].next_delay,
        std::time::Duration::from_millis(250)
    );
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
    let outcome = run_chat_stream_with_retry_progress(
        &client,
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
    assert_eq!(progress[0].next_delay, std::time::Duration::from_secs(2));
}

#[tokio::test]
async fn chat_stream_429_retry_respects_past_retry_after_http_date_as_zero_delay() {
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
    assert_eq!(progress[0].next_delay, std::time::Duration::ZERO);
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
    assert_eq!(u.prompt_tokens, 7);
    assert_eq!(u.completion_tokens, 2);
    assert!(!outcome.interrupted);
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
