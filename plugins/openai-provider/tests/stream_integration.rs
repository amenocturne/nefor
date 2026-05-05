//! Integration tests for the streaming HTTP path.
//!
//! Spins up a hand-rolled `tokio::net::TcpListener` that speaks just
//! enough HTTP/1.1 to satisfy reqwest's streaming reader. Avoids
//! pulling in axum/hyper-server for one test file.

use openai_provider::openai::Message;
use openai_provider::stream::{list_models, run_chat_stream, StreamError};
use std::net::SocketAddr;
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
    let models = list_models(&client, &base_url, None, "Authorization").await.expect("ok");

    let _ = server.await;

    assert_eq!(models, vec!["apple", "mango", "zebra"]);
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
