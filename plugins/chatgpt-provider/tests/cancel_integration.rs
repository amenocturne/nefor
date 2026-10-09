//! Cancellable-completion integration test.
//!
//! Drives the real dispatch loop (`run_dispatch_loop`) against a
//! hand-rolled local TCP server — the same style as
//! openai-provider's `stream_integration.rs`, which speaks just enough
//! HTTP/1.1 to satisfy reqwest's streaming reader.
//!
//! Contract under test (the honor side of `graph.cancel`):
//!   1. Start a completion, cancel it → NO `chat.complete.result` is
//!      delivered (the in-flight HTTP stream is aborted and the terminal
//!      result suppressed).
//!   2. The provider serves a subsequent completion normally.
//!   3. Cancel for an unknown request id is a no-op (never errors, never
//!      wedges the loop).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chatgpt_provider::auth::AuthStore;
use chatgpt_provider::broker::ToolBroker;
use chatgpt_provider::catalog::ToolCatalog;
use chatgpt_provider::config::ServeArgs;
use chatgpt_provider::dispatcher::{run_dispatch_loop, DispatcherContext};
use chatgpt_provider::responses::ResponsesClient;
use chatgpt_provider::state::Chats;

use nefor_plugin_sdk::TransportError;
use nefor_protocol::{Body, Envelope, PluginName, PluginOutgoing, Timestamp};

use serde_json::{Map, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

const PROVIDER: &str = "chatgpt";

fn test_responses_client(base_url: String) -> ResponsesClient {
    ResponsesClient::with_http(
        reqwest::Client::builder().build().expect("HTTP client"),
        base_url,
        "test-installation".into(),
        "nefor_test".into(),
    )
}

fn kind(suffix: &str) -> String {
    format!("{PROVIDER}.{suffix}")
}

/// Build an event envelope from `test-caller` carrying `kind` + fields.
fn event_env(k: &str, fields: &[(&str, Value)]) -> Envelope {
    let mut body = Map::new();
    body.insert("kind".into(), Value::String(k.to_owned()));
    for (name, v) in fields {
        body.insert((*name).into(), v.clone());
    }
    Envelope::event(
        PluginName::new("test-caller").expect("valid name"),
        Timestamp::now(),
        body,
    )
}

fn event_body(msg: &PluginOutgoing) -> Option<&Map<String, Value>> {
    match &msg.body {
        Body::Event(m) => Some(m),
        _ => None,
    }
}

/// Drain everything available within `dur`, returning the event bodies.
async fn drain_for(
    rx: &mut mpsc::Receiver<PluginOutgoing>,
    dur: Duration,
) -> Vec<Map<String, Value>> {
    let deadline = tokio::time::Instant::now() + dur;
    let mut out = Vec::new();
    while let Ok(Some(msg)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        if let Some(m) = event_body(&msg) {
            out.push(m.clone());
        }
    }
    out
}

/// Await the next event whose `kind` matches, up to `dur`.
async fn wait_for_kind(
    rx: &mut mpsc::Receiver<PluginOutgoing>,
    want: &str,
    dur: Duration,
) -> Option<Map<String, Value>> {
    let deadline = tokio::time::Instant::now() + dur;
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(msg)) => {
                if let Some(m) = event_body(&msg) {
                    if m.get("kind").and_then(Value::as_str) == Some(want) {
                        return Some(m.clone());
                    }
                }
            }
            Ok(None) | Err(_) => return None,
        }
    }
}

async fn wait_for_completion_event(
    rx: &mut mpsc::Receiver<PluginOutgoing>,
    request_id: &str,
    terminal_event: &str,
    dur: Duration,
) -> Option<Map<String, Value>> {
    let deadline = tokio::time::Instant::now() + dur;
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(msg)) => {
                if let Some(body) = event_body(&msg) {
                    if body.get("kind").and_then(Value::as_str) == Some(&kind("completion.event"))
                        && body.get("request_id").and_then(Value::as_str) == Some(request_id)
                        && body.get("event").and_then(Value::as_str) == Some(terminal_event)
                    {
                        return Some(body.clone());
                    }
                }
            }
            Ok(None) | Err(_) => return None,
        }
    }
}

/// Read a request off the socket until the header terminator. Enough to
/// confirm the POST arrived before we script the response.
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

async fn read_request_json(stream: &mut tokio::net::TcpStream) -> Value {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let count = stream.read(&mut buffer).await.expect("read request");
        assert!(count > 0, "request closed before body");
        bytes.extend_from_slice(&buffer[..count]);
        let Some(header_end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length: ")
                    .and_then(|value| value.parse::<usize>().ok())
            })
            .unwrap_or(0);
        let body_start = header_end + 4;
        if bytes.len() >= body_start + length {
            return serde_json::from_slice(&bytes[body_start..body_start + length])
                .expect("request JSON");
        }
    }
}

async fn spin_until(counter: &AtomicUsize, at_least: usize, dur: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + dur;
    while counter.load(Ordering::SeqCst) < at_least {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    true
}

#[tokio::test]
async fn cancel_aborts_inflight_completion_and_provider_serves_next() {
    // --- local HTTP server: conn1 hangs open (cancel target), conn2
    //     streams a full completion. ---
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_srv = hits.clone();
    let (request_tx, mut request_rx) = mpsc::channel::<String>(2);

    let server = tokio::spawn(async move {
        // conn1: send only the response headers, then hold the byte
        // stream open until the client aborts the connection (cancel
        // drops the reqwest stream → EOF here).
        let (mut s1, _) = listener.accept().await.expect("accept 1");
        let request = read_request(&mut s1).await;
        let _ = request_tx.send(request).await;
        hits_srv.fetch_add(1, Ordering::SeqCst);
        let headers =
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
        let _ = s1.write_all(headers.as_bytes()).await;
        let _ = s1.flush().await;
        let mut buf = [0u8; 256];
        while let Ok(n) = s1.read(&mut buf).await {
            if n == 0 {
                break;
            }
        }

        // conn2: full streaming response with a delta + completion.
        let (mut s2, _) = listener.accept().await.expect("accept 2");
        let request = read_request(&mut s2).await;
        let _ = request_tx.send(request).await;
        hits_srv.fetch_add(1, Ordering::SeqCst);
        let body = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n\
                    data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n\
                    data: [DONE]\n\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = s2.write_all(response.as_bytes()).await;
        let _ = s2.shutdown().await;
    });

    // --- dispatcher context wired to the local server ---
    let args = Arc::new(ServeArgs {
        provider_name: PROVIDER.into(),
        base_url: format!("http://{addr}"),
        stream_retry_timeout_seconds: None,
    });
    let chats = Arc::new(Chats::with_default_model(None));
    let dir = tempfile::tempdir().expect("tempdir");
    let auth = Arc::new(
        AuthStore::load_from_disk(&dir.path().join("auth.json"))
            .await
            .expect("auth store"),
    );
    // Static token → Connected without any network refresh.
    let _ = auth.apply_auth_set("test-token".into()).await;
    let catalog = Arc::new(ToolCatalog::new());
    let broker = Arc::new(ToolBroker::new());
    let responses_client = Arc::new(test_responses_client(format!("http://{addr}")));

    let (out_tx, mut out_rx) = mpsc::channel::<PluginOutgoing>(256);
    let ctx = DispatcherContext::new(args, chats, auth, catalog, broker, responses_client, out_tx);

    let (in_tx, in_rx) = mpsc::channel::<Result<Envelope, TransportError>>(64);
    let loop_handle = tokio::spawn(run_dispatch_loop(ctx, in_rx));

    // --- 1. start one canonical single-shot completion ---
    in_tx
        .send(Ok(event_env(
            &kind("completion.request"),
            &[
                ("request_id", Value::String("shared-id".into())),
                ("system", Value::String("top-level system".into())),
                ("model", Value::String("test-model".into())),
                (
                    "messages",
                    serde_json::json!([
                        {"role":"system","content":"be concise"},
                        {"role":"user","content":"hi"}
                    ]),
                ),
            ],
        )))
        .await
        .expect("send completion c1");

    // The turn must genuinely reach the server before we cancel.
    assert!(
        spin_until(&hits, 1, Duration::from_secs(5)).await,
        "c1 completion should POST to the backend",
    );
    let request = request_rx.recv().await.expect("captured request");
    assert_eq!(
        request.matches("top-level system").count(),
        1,
        "top-level system prompt must occur exactly once: {request}"
    );
    assert_eq!(
        request.matches("be concise").count(),
        1,
        "inline system prompt must occur exactly once: {request}"
    );
    assert_eq!(request.matches(r#""type":"web_search""#).count(), 0);

    // A persistent chat may use the same id without replacing or owning
    // the request-local completion state.
    in_tx
        .send(Ok(event_env(
            &kind("chat.create"),
            &[
                ("chat_id", Value::String("shared-id".into())),
                ("model", Value::String("chat-model".into())),
                ("system", Value::String("persistent system".into())),
            ],
        )))
        .await
        .expect("create colliding persistent chat");
    let created = wait_for_kind(&mut out_rx, &kind("chat.created"), Duration::from_secs(1))
        .await
        .expect("colliding chat id remains available");
    assert_eq!(created["chat_id"], "shared-id");

    // --- 2. cancel c1, plus an unknown-id cancel (must be a no-op) ---
    in_tx
        .send(Ok(event_env(
            &kind("completion.cancel"),
            &[("request_id", Value::String("shared-id".into()))],
        )))
        .await
        .expect("send cancel c1");
    in_tx
        .send(Ok(event_env(
            &kind("completion.cancel"),
            &[("request_id", Value::String("ghost".into()))],
        )))
        .await
        .expect("send cancel ghost");

    // No terminal event for the cancelled request within a settle window.
    let drained = drain_for(&mut out_rx, Duration::from_millis(600)).await;
    let leaked_terminal = drained.iter().any(|m| {
        m.get("kind").and_then(Value::as_str) == Some(&kind("completion.event"))
            && m.get("request_id").and_then(Value::as_str) == Some("shared-id")
            && matches!(
                m.get("event").and_then(Value::as_str),
                Some("completed" | "error")
            )
    });
    assert!(
        !leaked_terminal,
        "cancelled completion must not deliver a terminal completion.event: {drained:?}"
    );
    assert!(
        drained.iter().all(|m| {
            m.get("kind").and_then(Value::as_str) != Some(&kind("chat.complete.result"))
                || m.get("chat_id").and_then(Value::as_str) != Some("shared-id")
        }),
        "direct completion must never use the persistent chat result API: {drained:?}"
    );

    // --- 3. the provider serves the next completion normally ---
    in_tx
        .send(Ok(event_env(
            &kind("chat.create"),
            &[
                ("chat_id", Value::String("c2".into())),
                ("model", Value::String("test-model".into())),
            ],
        )))
        .await
        .expect("send create c2");
    in_tx
        .send(Ok(event_env(
            &kind("chat.append"),
            &[
                ("chat_id", Value::String("c2".into())),
                (
                    "message",
                    serde_json::json!({"role": "user", "content": "again"}),
                ),
            ],
        )))
        .await
        .expect("send append c2");
    in_tx
        .send(Ok(event_env(
            &kind("chat.complete"),
            &[("chat_id", Value::String("c2".into()))],
        )))
        .await
        .expect("send complete c2");

    let result = wait_for_kind(
        &mut out_rx,
        &kind("chat.complete.result"),
        Duration::from_secs(5),
    )
    .await
    .expect("c2 must deliver a chat.complete.result");

    assert_eq!(
        result.get("chat_id").and_then(Value::as_str),
        Some("c2"),
        "result is for the second request",
    );
    let text = result
        .get("output")
        .and_then(|o| o.get("text"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert_eq!(text, "hello", "second completion streamed its output");
    let chat_request = request_rx.recv().await.expect("captured chat request");
    assert_eq!(
        chat_request.matches(r#""type":"web_search""#).count(),
        0,
        "persistent chat requests must not receive hosted web search: {chat_request}"
    );

    // Clean shutdown: drop the sender so the loop returns.
    drop(in_tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), loop_handle).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
}

#[tokio::test]
async fn invalid_direct_completion_options_fail_once_before_http() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let hits = Arc::new(AtomicUsize::new(0));
    let server_hits = hits.clone();
    let server = tokio::spawn(async move {
        if let Ok(Ok((_stream, _))) =
            tokio::time::timeout(Duration::from_millis(750), listener.accept()).await
        {
            server_hits.fetch_add(1, Ordering::SeqCst);
        }
    });

    let args = Arc::new(ServeArgs {
        provider_name: PROVIDER.into(),
        base_url: format!("http://{addr}"),
        stream_retry_timeout_seconds: None,
    });
    let chats = Arc::new(Chats::with_default_model(None));
    let dir = tempfile::tempdir().expect("tempdir");
    let auth = Arc::new(
        AuthStore::load_from_disk(&dir.path().join("auth.json"))
            .await
            .expect("auth store"),
    );
    let _ = auth.apply_auth_set("test-token".into()).await;
    let catalog = Arc::new(ToolCatalog::new());
    catalog
        .register_from(
            "tool-gate",
            ToolCatalog::parse_tools(&serde_json::json!([{
                "name": "alpha", "description": "Alpha", "input_schema": {"type": "object"}
            }])),
        )
        .await;
    let (out_tx, mut out_rx) = mpsc::channel::<PluginOutgoing>(256);
    let ctx = DispatcherContext::new(
        args,
        chats,
        auth,
        catalog,
        Arc::new(ToolBroker::new()),
        Arc::new(test_responses_client(format!("http://{addr}"))),
        out_tx,
    );
    let (in_tx, in_rx) = mpsc::channel::<Result<Envelope, TransportError>>(64);
    let loop_handle = tokio::spawn(run_dispatch_loop(ctx, in_rx));

    let invalid = [
        ("true", serde_json::json!(true)),
        ("null", serde_json::json!(null)),
        ("number", serde_json::json!(7)),
        ("string", serde_json::json!("alpha")),
        ("object", serde_json::json!({"name": "alpha"})),
        ("mixed", serde_json::json!(["alpha", 7])),
        ("inline", serde_json::json!([{"name": "alpha"}])),
        ("mixed-spec", serde_json::json!(["alpha", {"name": "beta"}])),
        ("empty-name", serde_json::json!([" "])),
        ("duplicate", serde_json::json!(["alpha", "alpha"])),
    ];
    for (request_id, tools) in invalid {
        in_tx
            .send(Ok(event_env(
                &kind("completion.request"),
                &[
                    ("request_id", Value::String(request_id.into())),
                    ("model", Value::String("test-model".into())),
                    ("tools", tools),
                    (
                        "messages",
                        serde_json::json!([{"role":"user","content":"hello"}]),
                    ),
                ],
            )))
            .await
            .expect("submit invalid completion");
        wait_for_completion_event(&mut out_rx, request_id, "failed", Duration::from_secs(2))
            .await
            .expect("one correlated failure");
    }

    let invalid_provider_options = [
        ("provider-options-null", serde_json::json!(null)),
        ("provider-options-array", serde_json::json!([])),
        (
            "provider-options-null-tier",
            serde_json::json!({"service_tier": null}),
        ),
        (
            "provider-options-other-tier",
            serde_json::json!({"service_tier": "standard"}),
        ),
        (
            "provider-options-unknown-field",
            serde_json::json!({"service_tier": "fast", "unknown": true}),
        ),
    ];
    for (request_id, provider_options) in invalid_provider_options {
        in_tx
            .send(Ok(event_env(
                &kind("completion.request"),
                &[
                    ("request_id", Value::String(request_id.into())),
                    ("model", Value::String("test-model".into())),
                    ("provider_options", provider_options),
                    (
                        "messages",
                        serde_json::json!([{"role":"user","content":"hello"}]),
                    ),
                ],
            )))
            .await
            .expect("submit invalid completion");
        wait_for_completion_event(&mut out_rx, request_id, "failed", Duration::from_secs(2))
            .await
            .expect("one correlated failure");
    }

    let extra = drain_for(&mut out_rx, Duration::from_millis(100)).await;
    assert!(
        extra.iter().all(|event| {
            event.get("kind").and_then(Value::as_str) != Some(&kind("completion.event"))
        }),
        "invalid requests emitted extra completion events: {extra:?}"
    );
    server.await.expect("server");
    assert_eq!(hits.load(Ordering::SeqCst), 0, "rejection reached HTTP");

    drop(in_tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), loop_handle).await;
}

#[tokio::test]
async fn direct_completions_keep_independent_local_allowlists() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (request_tx, mut request_rx) = mpsc::channel::<Value>(2);
    let server = tokio::spawn(async move {
        let mut tasks = Vec::new();
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let request_tx = request_tx.clone();
            tasks.push(tokio::spawn(async move {
                let request = read_request_json(&mut stream).await;
                request_tx.send(request).await.expect("capture request");
                let body = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"done\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\"}}\n\ndata: [DONE]\n\n";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(), body
                );
                stream.write_all(response.as_bytes()).await.expect("response");
            }));
        }
        for task in tasks {
            task.await.expect("connection task");
        }
    });

    let args = Arc::new(ServeArgs {
        provider_name: PROVIDER.into(),
        base_url: format!("http://{addr}"),
        stream_retry_timeout_seconds: None,
    });
    let chats = Arc::new(Chats::with_default_model(None));
    let dir = tempfile::tempdir().expect("tempdir");
    let auth = Arc::new(
        AuthStore::load_from_disk(&dir.path().join("auth.json"))
            .await
            .expect("auth store"),
    );
    let _ = auth.apply_auth_set("test-token".into()).await;
    let catalog = Arc::new(ToolCatalog::new());
    catalog
        .register_from(
            "tool-gate",
            ToolCatalog::parse_tools(&serde_json::json!([
                {"name":"alpha","owner":"tool-gate","description":"Alpha","input_schema":{"type":"object","properties":{"a":{"type":"string"}}}},
                {"name":"beta","owner":"tool-gate","description":"Beta","input_schema":{"type":"object","properties":{"b":{"type":"integer"}}}}
            ])),
        )
        .await;
    let (out_tx, mut out_rx) = mpsc::channel::<PluginOutgoing>(256);
    let ctx = DispatcherContext::new(
        args,
        chats,
        auth,
        catalog,
        Arc::new(ToolBroker::new()),
        Arc::new(test_responses_client(format!("http://{addr}"))),
        out_tx,
    );
    let (in_tx, in_rx) = mpsc::channel::<Result<Envelope, TransportError>>(64);
    let loop_handle = tokio::spawn(run_dispatch_loop(ctx, in_rx));

    for (request_id, tool, provider_options) in [
        (
            "request-alpha",
            "alpha",
            Some(serde_json::json!({"service_tier": "fast"})),
        ),
        ("request-beta", "beta", None),
    ] {
        let mut fields = vec![
            ("request_id", Value::String(request_id.into())),
            ("model", Value::String("test-model".into())),
            ("tools", serde_json::json!([tool])),
            (
                "messages",
                serde_json::json!([{"role":"user","content":request_id}]),
            ),
        ];
        if let Some(provider_options) = provider_options {
            fields.push(("provider_options", provider_options));
        }
        in_tx
            .send(Ok(event_env(&kind("completion.request"), &fields)))
            .await
            .expect("submit completion");
    }

    let first = request_rx.recv().await.expect("first HTTP request");
    let second = request_rx.recv().await.expect("second HTTP request");
    let mut observed = [first, second]
        .into_iter()
        .map(|request| {
            let prompt = request["input"][0]["content"][0]["text"]
                .as_str()
                .expect("prompt")
                .to_owned();
            let tools = request["tools"].as_array().expect("tools");
            assert_eq!(
                tools.len(),
                1,
                "each request keeps only its selected routed tool"
            );
            let tool = tools[0]["name"].as_str().expect("tool name").to_owned();
            assert_eq!(tools[0]["parameters"]["type"], "object");
            let service_tier = request
                .get("service_tier")
                .and_then(Value::as_str)
                .map(str::to_owned);
            (prompt, tool, service_tier)
        })
        .collect::<Vec<_>>();
    observed.sort();
    assert_eq!(
        observed,
        vec![
            (
                "request-alpha".into(),
                "alpha".into(),
                Some("priority".into())
            ),
            ("request-beta".into(), "beta".into(), None)
        ]
    );

    for request_id in ["request-alpha", "request-beta"] {
        let completed =
            wait_for_completion_event(&mut out_rx, request_id, "completed", Duration::from_secs(5))
                .await
                .expect("completion settles");
        assert_eq!(completed["text"], "done");
        assert_eq!(completed["model"], "test-model");
        assert!(completed["duration_ms"].as_u64().is_some());
    }

    drop(in_tx);
    server.await.expect("server");
    let _ = tokio::time::timeout(Duration::from_secs(2), loop_handle).await;
}

#[tokio::test]
async fn chat_compaction_preserves_fast_service_tier() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (request_tx, mut request_rx) = mpsc::channel::<Value>(1);
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        request_tx
            .send(read_request_json(&mut stream).await)
            .await
            .expect("capture request");
        let body = concat!(
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"sealed\"},\"output_index\":0}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
            "data: [DONE]\n\n"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream
            .write_all(response.as_bytes())
            .await
            .expect("response");
    });

    let (in_tx, mut out_rx, loop_handle) = start_direct_harness_with_budget(
        format!("http://{addr}"),
        test_responses_client(format!("http://{addr}")),
        None,
    )
    .await;
    in_tx
        .send(Ok(event_env(
            &kind("chat.create"),
            &[
                ("chat_id", Value::String("compact-fast".into())),
                ("model", Value::String("test-model".into())),
                (
                    "provider_options",
                    serde_json::json!({"service_tier": "fast"}),
                ),
            ],
        )))
        .await
        .expect("create chat");
    wait_for_kind(&mut out_rx, &kind("chat.created"), Duration::from_secs(2))
        .await
        .expect("chat created");
    in_tx
        .send(Ok(event_env(
            &kind("chat.append"),
            &[
                ("chat_id", Value::String("compact-fast".into())),
                (
                    "message",
                    serde_json::json!({"role":"user","content":"remember this"}),
                ),
            ],
        )))
        .await
        .expect("append message");
    wait_for_kind(&mut out_rx, &kind("chat.appended"), Duration::from_secs(2))
        .await
        .expect("message appended");
    in_tx
        .send(Ok(event_env(
            &kind("chat.compact"),
            &[("chat_id", Value::String("compact-fast".into()))],
        )))
        .await
        .expect("compact chat");

    let request = request_rx.recv().await.expect("compaction request");
    assert_eq!(request["service_tier"], "priority");
    assert_eq!(request["input"][1]["type"], "compaction_trigger");
    assert_eq!(request["tools"].as_array().map(Vec::len), Some(0));
    wait_for_kind(
        &mut out_rx,
        &kind("chat.compaction.commit"),
        Duration::from_secs(5),
    )
    .await
    .expect("compaction committed");

    finish_harness(in_tx, loop_handle).await;
    server.await.expect("server");
}

#[tokio::test]
async fn legacy_native_web_context_replays_without_hosted_activity() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_server = hits.clone();
    let (request_tx, mut request_rx) = mpsc::channel::<Value>(2);
    let server = tokio::spawn(async move {
        let replies = [
            concat!(
                "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"web_search_call\",\"id\":\"ws_1\",\"status\":\"in_progress\"}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"found\"}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"found\"}]}}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"web_search_call\",\"id\":\"ws_1\",\"status\":\"completed\",\"action\":{\"type\":\"search\",\"query\":\"current rust\"}}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\"}}\n\n",
                "data: [DONE]\n\n"
            ),
            concat!(
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"continued\"}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"continued\"}]}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r2\"}}\n\n",
                "data: [DONE]\n\n"
            ),
        ];
        for body in replies {
            let (mut stream, _) = listener.accept().await.expect("accept");
            request_tx
                .send(read_request_json(&mut stream).await)
                .await
                .expect("capture request");
            hits_server.fetch_add(1, Ordering::SeqCst);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(), body
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("response");
        }
    });

    let base_url = format!("http://{addr}");
    let (in_tx, mut out_rx, loop_handle) =
        start_direct_harness(base_url.clone(), test_responses_client(base_url)).await;
    submit_direct_completion(&in_tx, "native-search-1").await;
    let completed = wait_for_completion_event(
        &mut out_rx,
        "native-search-1",
        "completed",
        Duration::from_secs(5),
    )
    .await
    .expect("first completion");
    assert_eq!(completed["text"], "found");
    assert!(completed.get("tool_calls").is_none());
    let provider_context = completed["provider_context"].clone();
    let items = provider_context["artifact"]["items"]
        .as_array()
        .expect("native items");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["type"], "web_search_call");
    assert_eq!(items[0]["id"], "ws_1");
    assert_eq!(items[1]["type"], "message");

    in_tx
        .send(Ok(event_env(
            &kind("completion.request"),
            &[
                ("request_id", Value::String("native-search-2".into())),
                ("model", Value::String("test-model".into())),
                (
                    "messages",
                    serde_json::json!([
                        {"role":"user","content":"find it"},
                        {"role":"assistant","content":"found","provider_context":provider_context},
                        {"role":"user","content":"continue"}
                    ]),
                ),
            ],
        )))
        .await
        .expect("second completion request");
    let second = wait_for_completion_event(
        &mut out_rx,
        "native-search-2",
        "completed",
        Duration::from_secs(5),
    )
    .await
    .expect("second completion");
    assert_eq!(second["text"], "continued");

    let _first_request = request_rx.recv().await.expect("first request");
    let second_request = request_rx.recv().await.expect("second request");
    let replay = second_request["input"].as_array().expect("request input");
    let replay_types = replay
        .iter()
        .filter_map(|item| item.get("type").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert_eq!(
        replay_types,
        ["message", "web_search_call", "message", "message"]
    );
    let web_search = replay
        .iter()
        .find(|item| item["type"] == "web_search_call")
        .expect("replayed web search");
    assert!(web_search.get("id").is_none());
    assert_eq!(web_search["action"]["query"], "current rust");
    assert_eq!(hits.load(Ordering::SeqCst), 2, "no local tool iteration");
    let trailing = drain_for(&mut out_rx, Duration::from_millis(100)).await;
    assert!(trailing
        .iter()
        .all(|body| body.get("event").and_then(Value::as_str) != Some("tool_call")));

    finish_harness(in_tx, loop_handle).await;
    server.await.expect("server");
}

struct ScriptedSseReply {
    body: String,
    declared_length: usize,
    linger: Duration,
}

impl ScriptedSseReply {
    fn complete(body: impl Into<String>) -> Self {
        let body = body.into();
        Self {
            declared_length: body.len(),
            body,
            linger: Duration::ZERO,
        }
    }

    fn truncated(body: impl Into<String>) -> Self {
        let body = body.into();
        Self {
            declared_length: body.len() + 64,
            body,
            linger: Duration::ZERO,
        }
    }

    fn stalled(linger: Duration) -> Self {
        Self {
            body: String::new(),
            declared_length: 64,
            linger,
        }
    }
}

async fn serve_scripted_sse(
    listener: TcpListener,
    replies: Vec<ScriptedSseReply>,
    hits: Arc<AtomicUsize>,
) {
    for reply in replies {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_request_json(&mut stream).await;
        hits.fetch_add(1, Ordering::SeqCst);
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            reply.declared_length,
        );
        stream
            .write_all(headers.as_bytes())
            .await
            .expect("write headers");
        stream
            .write_all(reply.body.as_bytes())
            .await
            .expect("write body");
        if !reply.linger.is_zero() {
            tokio::time::sleep(reply.linger).await;
        }
        stream.shutdown().await.expect("close response");
    }
}

async fn start_direct_harness(
    base_url: String,
    client: ResponsesClient,
) -> (
    mpsc::Sender<Result<Envelope, TransportError>>,
    mpsc::Receiver<PluginOutgoing>,
    tokio::task::JoinHandle<Result<(), chatgpt_provider::error::ChatgptError>>,
) {
    start_direct_harness_with_budget(base_url, client, None).await
}

async fn start_direct_harness_with_budget(
    base_url: String,
    client: ResponsesClient,
    retry_budget: Option<Duration>,
) -> (
    mpsc::Sender<Result<Envelope, TransportError>>,
    mpsc::Receiver<PluginOutgoing>,
    tokio::task::JoinHandle<Result<(), chatgpt_provider::error::ChatgptError>>,
) {
    let args = Arc::new(ServeArgs {
        provider_name: PROVIDER.into(),
        base_url,
        stream_retry_timeout_seconds: None,
    });
    let chats = Arc::new(Chats::with_default_model(None));
    let dir = tempfile::tempdir().expect("tempdir");
    let auth = Arc::new(
        AuthStore::load_from_disk(&dir.path().join("auth.json"))
            .await
            .expect("auth store"),
    );
    let _ = auth.apply_auth_set("test-token".into()).await;
    let (out_tx, out_rx) = mpsc::channel::<PluginOutgoing>(256);
    let mut ctx = DispatcherContext::new(
        args,
        chats,
        auth,
        Arc::new(ToolCatalog::new()),
        Arc::new(ToolBroker::new()),
        Arc::new(client),
        out_tx,
    )
    .with_pre_output_retry_max_attempts(3);
    if let Some(retry_budget) = retry_budget {
        ctx = ctx.with_pre_output_retry_budget(retry_budget);
    }
    let (in_tx, in_rx) = mpsc::channel::<Result<Envelope, TransportError>>(64);
    let loop_handle = tokio::spawn(run_dispatch_loop(ctx, in_rx));
    (in_tx, out_rx, loop_handle)
}

async fn submit_direct_completion(
    in_tx: &mpsc::Sender<Result<Envelope, TransportError>>,
    request_id: &str,
) {
    in_tx
        .send(Ok(event_env(
            &kind("completion.request"),
            &[
                ("request_id", Value::String(request_id.into())),
                ("model", Value::String("test-model".into())),
                (
                    "messages",
                    serde_json::json!([
                        {"role":"system","content":"be concise"},
                        {"role":"user","content":"hi"}
                    ]),
                ),
            ],
        )))
        .await
        .expect("completion request");
}

async fn finish_harness(
    in_tx: mpsc::Sender<Result<Envelope, TransportError>>,
    loop_handle: tokio::task::JoinHandle<Result<(), chatgpt_provider::error::ChatgptError>>,
) {
    drop(in_tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), loop_handle).await;
}

async fn assert_truncated_observation_is_discarded_and_replayed(
    request_id: &str,
    event_body: &str,
) -> Value {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let hits = Arc::new(AtomicUsize::new(0));
    let server = tokio::spawn(serve_scripted_sse(
        listener,
        vec![
            ScriptedSseReply::truncated(event_body),
            ScriptedSseReply::complete(concat!(
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"replacement\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r2\"}}\n\n"
            )),
        ],
        hits.clone(),
    ));
    let base_url = format!("http://{addr}");
    let (in_tx, mut out_rx, loop_handle) =
        start_direct_harness(base_url.clone(), test_responses_client(base_url)).await;

    submit_direct_completion(&in_tx, request_id).await;
    let discarded = wait_for_completion_event(
        &mut out_rx,
        request_id,
        "attempt_discarded",
        Duration::from_secs(3),
    )
    .await
    .expect("discarded attempt");
    let decision = wait_for_completion_event(
        &mut out_rx,
        request_id,
        "retry_decision",
        Duration::from_secs(3),
    )
    .await
    .expect("retry decision");
    assert_eq!(decision["retry"], true);
    assert_eq!(decision["retry_reason"], "retryable_provisional_failure");
    wait_for_completion_event(&mut out_rx, request_id, "completed", Duration::from_secs(3))
        .await
        .expect("replacement completion");
    assert_eq!(hits.load(Ordering::SeqCst), 2, "request was replayed once");

    finish_harness(in_tx, loop_handle).await;
    server.await.expect("server");
    Value::Object(discarded)
}

#[tokio::test]
async fn truncated_body_before_output_replays_once_then_succeeds() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let hits = Arc::new(AtomicUsize::new(0));
    let completed = concat!(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"retried\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\"}}\n\n"
    );
    let server = tokio::spawn(serve_scripted_sse(
        listener,
        vec![
            ScriptedSseReply::truncated(""),
            ScriptedSseReply::complete(completed),
        ],
        hits.clone(),
    ));
    let base_url = format!("http://{addr}");
    let (in_tx, mut out_rx, loop_handle) =
        start_direct_harness(base_url.clone(), test_responses_client(base_url)).await;

    submit_direct_completion(&in_tx, "truncated-replay").await;
    let decision = wait_for_completion_event(
        &mut out_rx,
        "truncated-replay",
        "retry_decision",
        Duration::from_secs(3),
    )
    .await
    .expect("retry decision");
    assert_eq!(decision["attempt"], 1);
    assert_eq!(decision["retry"], true);
    assert_eq!(decision["failure_kind"], "body_read");
    assert_eq!(decision["terminal_event_seen"], false);
    assert_eq!(decision["visible_state_observed"], false);
    assert_eq!(decision["tool_state_observed"], false);
    assert!(decision["error"]
        .as_str()
        .is_some_and(|error| error.contains("source:")));

    let completed = wait_for_completion_event(
        &mut out_rx,
        "truncated-replay",
        "completed",
        Duration::from_secs(3),
    )
    .await
    .expect("retried completion");
    assert_eq!(completed["text"], "retried");
    assert_eq!(completed["stream_attempts"], 2);
    assert_eq!(completed["terminal_event_seen"], true);
    assert_eq!(hits.load(Ordering::SeqCst), 2, "exactly one replay");

    finish_harness(in_tx, loop_handle).await;
    server.await.expect("server");
}

#[tokio::test]
async fn replay_body_read_stops_at_the_absolute_recovery_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let hits = Arc::new(AtomicUsize::new(0));
    let server = tokio::spawn(serve_scripted_sse(
        listener,
        vec![
            ScriptedSseReply::truncated(""),
            ScriptedSseReply::stalled(Duration::from_millis(1_200)),
        ],
        hits.clone(),
    ));
    let base_url = format!("http://{addr}");
    let retry_budget = Duration::from_millis(600);
    let (in_tx, mut out_rx, loop_handle) = start_direct_harness_with_budget(
        base_url.clone(),
        test_responses_client(base_url),
        Some(retry_budget),
    )
    .await;

    let started = tokio::time::Instant::now();
    submit_direct_completion(&in_tx, "bounded-replay").await;
    let first = wait_for_completion_event(
        &mut out_rx,
        "bounded-replay",
        "retry_decision",
        Duration::from_secs(2),
    )
    .await
    .expect("first retry decision");
    assert_eq!(first["retry"], true);
    let final_decision = wait_for_completion_event(
        &mut out_rx,
        "bounded-replay",
        "retry_decision",
        Duration::from_secs(2),
    )
    .await
    .expect("deadline decision");
    assert_eq!(final_decision["retry"], false);
    assert_eq!(final_decision["retry_reason"], "elapsed_budget_exhausted");
    assert_eq!(final_decision["failure_kind"], "recovery_timeout");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "replay must stop at its short recovery budget, not the 300s SSE idle timeout"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 2);

    let failed = wait_for_completion_event(
        &mut out_rx,
        "bounded-replay",
        "error",
        Duration::from_secs(2),
    )
    .await
    .expect("bounded replay failure");
    assert_eq!(failed["stream_attempts"], 2);

    finish_harness(in_tx, loop_handle).await;
    server.await.expect("server");
}

#[tokio::test]
async fn pre_output_transport_retries_stop_at_attempt_limit() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let hits = Arc::new(AtomicUsize::new(0));
    let server = tokio::spawn(serve_scripted_sse(
        listener,
        (0..3).map(|_| ScriptedSseReply::truncated("")).collect(),
        hits.clone(),
    ));
    let base_url = format!("http://{addr}");
    let (in_tx, mut out_rx, loop_handle) =
        start_direct_harness(base_url.clone(), test_responses_client(base_url)).await;

    submit_direct_completion(&in_tx, "exhausted").await;
    for attempt in 1..=3 {
        let decision = wait_for_completion_event(
            &mut out_rx,
            "exhausted",
            "retry_decision",
            Duration::from_secs(3),
        )
        .await
        .expect("retry decision");
        assert_eq!(decision["attempt"], attempt);
        assert_eq!(decision["retry"], attempt < 3);
        if attempt == 3 {
            assert_eq!(decision["retry_reason"], "attempt_limit_reached");
        }
    }
    let failed =
        wait_for_completion_event(&mut out_rx, "exhausted", "error", Duration::from_secs(3))
            .await
            .expect("exhausted completion");
    assert!(failed["message"]
        .as_str()
        .is_some_and(|message| message.contains("retrying the turn is safe")));
    assert_eq!(failed["stream_attempts"], 3);
    assert_eq!(hits.load(Ordering::SeqCst), 3);

    finish_harness(in_tx, loop_handle).await;
    server.await.expect("server");
}

#[tokio::test]
async fn out_of_order_function_argument_events_are_provisional() {
    let cases = [
        (
            "function-args-delta",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"delta\":\"{\",\"item_id\":\"fc_unknown\"}\n\n",
        ),
        (
            "function-args-done",
            "data: {\"type\":\"response.function_call_arguments.done\",\"arguments\":\"{}\",\"item_id\":\"fc_unknown\"}\n\n",
        ),
    ];
    for (request_id, event_body) in cases {
        let discarded =
            assert_truncated_observation_is_discarded_and_replayed(request_id, event_body).await;
        assert_eq!(discarded["tool_state_observed"], true);
    }
}

#[tokio::test]
async fn reasoning_output_item_observation_is_provisional() {
    let discarded = assert_truncated_observation_is_discarded_and_replayed(
        "reasoning-item-added",
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"summary\":[]}}\n\n",
    )
    .await;
    assert_eq!(discarded["native_output_state_observed"], true);
    assert_eq!(discarded["visible_state_observed"], false);
}

#[tokio::test]
async fn empty_text_and_reasoning_observations_are_provisional() {
    let cases = [
        (
            "empty-text-delta",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"\"}\n\n",
        ),
        (
            "empty-reasoning-delta",
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"\",\"item_id\":\"rs_1\"}\n\n",
        ),
        (
            "empty-reasoning-part",
            "data: {\"type\":\"response.reasoning_summary_part.added\",\"summary_index\":0,\"item_id\":\"rs_1\"}\n\n",
        ),
    ];
    for (request_id, event_body) in cases {
        let discarded =
            assert_truncated_observation_is_discarded_and_replayed(request_id, event_body).await;
        assert_eq!(discarded["visible_state_observed"], true);
    }
}

#[tokio::test]
async fn interruption_after_text_discards_and_replays() {
    let body = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n";
    let discarded =
        assert_truncated_observation_is_discarded_and_replayed("partial-text", body).await;
    assert_eq!(discarded["visible_state_observed"], true);
}

#[tokio::test]
async fn interruption_after_buffered_reasoning_discards_and_replays() {
    let body = "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"**Considering recovery\",\"item_id\":\"rs_1\"}\n\n";
    let discarded =
        assert_truncated_observation_is_discarded_and_replayed("partial-reasoning", body).await;
    assert_eq!(discarded["visible_state_observed"], true);
}

#[tokio::test]
async fn interruption_after_tool_call_state_discards_and_replays() {
    let body = "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"inspect\",\"arguments\":\"\"}}\n\n";
    let discarded =
        assert_truncated_observation_is_discarded_and_replayed("partial-tool", body).await;
    assert_eq!(discarded["tool_state_observed"], true);
}

#[tokio::test]
async fn clean_eof_before_completion_is_not_semantic_success() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let hits = Arc::new(AtomicUsize::new(0));
    let server = tokio::spawn(serve_scripted_sse(
        listener,
        (0..3).map(|_| ScriptedSseReply::complete("")).collect(),
        hits.clone(),
    ));
    let base_url = format!("http://{addr}");
    let (in_tx, mut out_rx, loop_handle) =
        start_direct_harness(base_url.clone(), test_responses_client(base_url)).await;

    submit_direct_completion(&in_tx, "clean-eof").await;
    let mut last = None;
    for _ in 0..3 {
        last = wait_for_completion_event(
            &mut out_rx,
            "clean-eof",
            "retry_decision",
            Duration::from_secs(3),
        )
        .await;
    }
    let last = last.expect("final EOF decision");
    assert_eq!(last["failure_kind"], "clean_eof");
    assert_eq!(last["terminal_event_seen"], false);
    assert_eq!(last["retry"], false);
    let failed =
        wait_for_completion_event(&mut out_rx, "clean-eof", "error", Duration::from_secs(3))
            .await
            .expect("EOF terminal error");
    assert!(failed["message"]
        .as_str()
        .is_some_and(|message| message.contains("before a terminal event")));
    assert_eq!(hits.load(Ordering::SeqCst), 3);

    finish_harness(in_tx, loop_handle).await;
    server.await.expect("server");
}

#[tokio::test]
async fn response_completed_wins_over_following_connection_failure() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let hits = Arc::new(AtomicUsize::new(0));
    let body = concat!(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"done\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\"}}\n\n"
    );
    let server = tokio::spawn(serve_scripted_sse(
        listener,
        vec![ScriptedSseReply::truncated(body)],
        hits.clone(),
    ));
    let base_url = format!("http://{addr}");
    let (in_tx, mut out_rx, loop_handle) =
        start_direct_harness(base_url.clone(), test_responses_client(base_url)).await;

    submit_direct_completion(&in_tx, "completed-before-reset").await;
    let completed = wait_for_completion_event(
        &mut out_rx,
        "completed-before-reset",
        "completed",
        Duration::from_secs(3),
    )
    .await
    .expect("semantic completion");
    assert_eq!(completed["text"], "done");
    assert_eq!(completed["terminal_event_seen"], true);
    assert_eq!(completed["stream_attempts"], 1);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    let trailing = drain_for(&mut out_rx, Duration::from_millis(100)).await;
    assert!(!trailing
        .iter()
        .any(|body| { body.get("event").and_then(Value::as_str) == Some("retry_decision") }));

    finish_harness(in_tx, loop_handle).await;
    server.await.expect("server");
}

#[tokio::test]
async fn direct_completion_replays_encrypted_reasoning_before_tool_output() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (request_tx, mut request_rx) = mpsc::channel::<Value>(2);
    let server = tokio::spawn(async move {
        let first_body = concat!(
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"encrypted_content\":\"sealed-plan\",\"summary\":[]}}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"inspect\",\"arguments\":\"{}\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\"}}\n\n",
            "data: [DONE]\n\n"
        );
        let second_body = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"done\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"done\"}]}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r2\"}}\n\n",
            "data: [DONE]\n\n"
        );
        for body in [first_body, second_body] {
            let (mut stream, _) = listener.accept().await.expect("accept");
            request_tx
                .send(read_request_json(&mut stream).await)
                .await
                .expect("capture request");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(), body
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("response");
        }
    });

    let args = Arc::new(ServeArgs {
        provider_name: PROVIDER.into(),
        base_url: format!("http://{addr}"),
        stream_retry_timeout_seconds: None,
    });
    let chats = Arc::new(Chats::with_default_model(None));
    let dir = tempfile::tempdir().expect("tempdir");
    let auth = Arc::new(
        AuthStore::load_from_disk(&dir.path().join("auth.json"))
            .await
            .expect("auth store"),
    );
    let _ = auth.apply_auth_set("test-token".into()).await;
    let catalog = Arc::new(ToolCatalog::new());
    catalog
        .register_from(
            "tool-gate",
            ToolCatalog::parse_tools(&serde_json::json!([{
                "name": "inspect", "description": "Inspect", "input_schema": {"type": "object"}
            }])),
        )
        .await;
    let (out_tx, mut out_rx) = mpsc::channel::<PluginOutgoing>(256);
    let ctx = DispatcherContext::new(
        args,
        chats,
        auth,
        catalog,
        Arc::new(ToolBroker::new()),
        Arc::new(test_responses_client(format!("http://{addr}"))),
        out_tx,
    );
    let (in_tx, in_rx) = mpsc::channel::<Result<Envelope, TransportError>>(64);
    let loop_handle = tokio::spawn(run_dispatch_loop(ctx, in_rx));

    let user = serde_json::json!({"role":"user","content":"inspect the project"});
    in_tx
        .send(Ok(event_env(
            &kind("completion.request"),
            &[
                ("request_id", Value::String("reasoning-1".into())),
                ("model", Value::String("gpt-5.6-sol".into())),
                ("tools", serde_json::json!(["inspect"])),
                ("messages", serde_json::json!([user.clone()])),
            ],
        )))
        .await
        .expect("first completion");
    let first = wait_for_completion_event(
        &mut out_rx,
        "reasoning-1",
        "completed",
        Duration::from_secs(5),
    )
    .await
    .expect("first completion settles");
    let provider_context = first
        .get("provider_context")
        .cloned()
        .expect("native output artifact");
    assert_eq!(
        provider_context["artifact"]["items"][0]["encrypted_content"],
        "sealed-plan"
    );

    in_tx
        .send(Ok(event_env(
            &kind("completion.request"),
            &[
                ("request_id", Value::String("reasoning-2".into())),
                ("model", Value::String("gpt-5.6-sol".into())),
                ("tools", serde_json::json!(["inspect"])),
                (
                    "messages",
                    serde_json::json!([
                        user,
                        {
                            "role": "assistant",
                            "content": "",
                            "tool_calls": [{
                                "id": "call_1",
                                "name": "inspect",
                                "arguments": {}
                            }],
                            "provider_context": provider_context
                        },
                        {"role":"tool","tool_call_id":"call_1","content":"project details"}
                    ]),
                ),
            ],
        )))
        .await
        .expect("second completion");
    wait_for_completion_event(
        &mut out_rx,
        "reasoning-2",
        "completed",
        Duration::from_secs(5),
    )
    .await
    .expect("second completion settles");

    let _first_request = request_rx.recv().await.expect("first request");
    let second_request = request_rx.recv().await.expect("second request");
    let input = second_request["input"].as_array().expect("Responses input");
    assert_eq!(input.len(), 4);
    assert_eq!(input[0]["type"], "message");
    assert_eq!(input[1]["type"], "reasoning");
    assert_eq!(input[1]["encrypted_content"], "sealed-plan");
    assert_eq!(input[2]["type"], "function_call");
    assert_eq!(input[2]["call_id"], "call_1");
    assert_eq!(input[3]["type"], "function_call_output");
    assert_eq!(input[3]["call_id"], "call_1");

    drop(in_tx);
    server.await.expect("server");
    let _ = tokio::time::timeout(Duration::from_secs(2), loop_handle).await;
}

#[tokio::test]
async fn giant_live_delta_rejects_once_cancels_reader_and_reuses_request_id() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("address"));
    let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("first request");
        read_request_json(&mut stream).await;
        let delta = serde_json::json!({"type":"response.output_text.delta", "delta":"x".repeat(16 * 1024 * 1024)});
        let body = format!("data: {delta}\n\n");
        let header = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len() + 1024);
        stream.write_all(header.as_bytes()).await.expect("headers");
        stream.write_all(body.as_bytes()).await.expect("delta");
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(90), stream.read(&mut byte))
                .await
                .expect("reader cancelled")
                .expect("read"),
            0
        );
        closed_tx.send(()).expect("reader observation");
        serve_scripted_sse(
            listener,
            vec![ScriptedSseReply::complete(concat!(
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"later\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"second\"}}\n\n"
            ))],
            Arc::new(AtomicUsize::new(0)),
        )
        .await;
    });
    let (in_tx, mut out_rx, handle) =
        start_direct_harness(base.clone(), test_responses_client(base)).await;
    submit_direct_completion(&in_tx, "reuse").await;
    let error = wait_for_completion_event(&mut out_rx, "reuse", "error", Duration::from_secs(90))
        .await
        .expect("small terminal");
    assert_eq!(error["error_category"], "output_admission_failed");
    assert_eq!(error["effects"], "unknown");
    assert!(serde_json::to_vec(&error).expect("encode").len() < 1024);
    closed_rx.await.expect("stream was dropped before terminal");
    assert!(drain_for(&mut out_rx, Duration::from_millis(30))
        .await
        .iter()
        .all(|b| b.get("event") != Some(&Value::String("completed".into()))));
    submit_direct_completion(&in_tx, "reuse").await;
    let result =
        wait_for_completion_event(&mut out_rx, "reuse", "completed", Duration::from_secs(5))
            .await
            .expect("released request id");
    assert_eq!(result["text"], "later");
    finish_harness(in_tx, handle).await;
    server.await.expect("mock server");
}

#[tokio::test]
async fn giant_final_tool_call_is_not_published_as_success() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("address"));
    let item = serde_json::json!({"type":"response.output_item.done", "output_index":0, "item":{
        "type":"function_call", "id":"item-1", "call_id":"call-1", "name":"read_file", "arguments":"x".repeat(16 * 1024 * 1024)
    }});
    let body = format!("data: {item}\n\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"r\"}}}}\n\n");
    let server = tokio::spawn(serve_scripted_sse(
        listener,
        vec![ScriptedSseReply::complete(body)],
        Arc::new(AtomicUsize::new(0)),
    ));
    let (in_tx, mut out_rx, handle) =
        start_direct_harness(base.clone(), test_responses_client(base)).await;
    in_tx.send(Ok(event_env("tool.register", &[("tools", serde_json::json!([{"name":"read_file", "description":"read", "input_schema":{"type":"object"}}]))]))).await.expect("advertise");
    in_tx
        .send(Ok(event_env(
            &kind("completion.request"),
            &[
                ("request_id", Value::String("giant-call".into())),
                ("model", Value::String("test-model".into())),
                ("tools", serde_json::json!(["read_file"])),
                (
                    "messages",
                    serde_json::json!([{"role":"user","content":"read"}]),
                ),
            ],
        )))
        .await
        .expect("request");
    let error =
        wait_for_completion_event(&mut out_rx, "giant-call", "error", Duration::from_secs(90))
            .await
            .expect("terminal admission error");
    assert_eq!(error["error_category"], "output_admission_failed");
    assert!(drain_for(&mut out_rx, Duration::from_millis(30))
        .await
        .iter()
        .all(|b| b.get("event") != Some(&Value::String("completed".into()))));
    finish_harness(in_tx, handle).await;
    server.await.expect("mock server");
}

#[tokio::test]
async fn writer_loss_joins_active_http_task_without_input_traffic() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("address"));
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("request");
        read_request_json(&mut stream).await;
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 1000\r\n\r\n").await.expect("headers");
        started_tx.send(()).expect("started");
        let mut byte = [0];
        let closed = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut byte))
            .await
            .expect("cancelled read");
        assert!(matches!(closed, Ok(0)) || closed.is_err());
    });
    let (in_tx, out_rx, handle) =
        start_direct_harness(base.clone(), test_responses_client(base)).await;
    submit_direct_completion(&in_tx, "writer-loss").await;
    started_rx.await.expect("active HTTP");
    drop(out_rx);
    let result = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("supervisor exits")
        .expect("joins");
    assert!(matches!(
        result,
        Err(chatgpt_provider::error::ChatgptError::Transport(
            TransportError::WriterClosed
        ))
    ));
    drop(in_tx);
    server.await.expect("reader cancelled and joined");
}

#[tokio::test]
async fn giant_accumulated_chat_result_releases_same_chat_slot() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("address"));
    let delta =
        serde_json::json!({"type":"response.output_text.delta", "delta":"x".repeat(1024 * 1024)});
    let mut body = format!("data: {delta}\n\n").repeat(17);
    body.push_str("data: {\"type\":\"response.completed\",\"response\":{\"id\":\"first\"}}\n\n");
    let server = tokio::spawn(serve_scripted_sse(listener, vec![
        ScriptedSseReply::complete(body),
        ScriptedSseReply::complete("data: {\"type\":\"response.output_text.delta\",\"delta\":\"healthy\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"second\"}}\n\n")
    ], Arc::new(AtomicUsize::new(0))));
    let (in_tx, mut out_rx, handle) =
        start_direct_harness(base.clone(), test_responses_client(base)).await;
    in_tx
        .send(Ok(event_env(
            &kind("chat.create"),
            &[
                ("chat_id", Value::String("same-chat".into())),
                ("model", Value::String("test-model".into())),
            ],
        )))
        .await
        .expect("create");
    wait_for_kind(&mut out_rx, &kind("chat.created"), Duration::from_secs(5))
        .await
        .expect("created");
    in_tx
        .send(Ok(event_env(
            &kind("chat.append"),
            &[
                ("chat_id", Value::String("same-chat".into())),
                (
                    "message",
                    serde_json::json!({"role":"user", "content":"hi"}),
                ),
            ],
        )))
        .await
        .expect("append");
    let complete = event_env(
        &kind("chat.complete"),
        &[("chat_id", Value::String("same-chat".into()))],
    );
    in_tx.send(Ok(complete.clone())).await.expect("first turn");
    let error = wait_for_kind(&mut out_rx, &kind("chat.error"), Duration::from_secs(90))
        .await
        .expect("rejected aggregate terminal");
    assert_eq!(error["chat_id"], "same-chat");
    assert_eq!(error["effects"], "unknown");
    assert_eq!(error["error_category"], "output_admission_failed");
    in_tx.send(Ok(complete)).await.expect("subsequent turn");
    let result = wait_for_kind(
        &mut out_rx,
        &kind("chat.complete.result"),
        Duration::from_secs(10),
    )
    .await
    .expect("slot released");
    assert_eq!(result["output"]["text"], "healthy");
    finish_harness(in_tx, handle).await;
    server.await.expect("mock server");
}

#[tokio::test]
async fn oversized_identity_does_not_recursively_fallback_or_stay_alive() {
    let base = "http://127.0.0.1:1".to_owned();
    let (in_tx, _out_rx, handle) =
        start_direct_harness(base.clone(), test_responses_client(base)).await;
    submit_direct_completion(&in_tx, &"i".repeat(16 * 1024 * 1024)).await;
    let result = tokio::time::timeout(Duration::from_secs(15), handle)
        .await
        .expect("fallback rejection exits")
        .expect("joined");
    assert!(result.is_err());
}

#[tokio::test]
async fn terminal_stdin_fault_joins_active_request() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("address"));
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("request");
        read_request_json(&mut stream).await;
        started_tx.send(()).expect("started");
        let mut byte = [0];
        let closed = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut byte))
            .await
            .expect("cancelled read");
        assert!(matches!(closed, Ok(0)) || closed.is_err());
    });
    let (in_tx, _out_rx, handle) =
        start_direct_harness(base.clone(), test_responses_client(base)).await;
    submit_direct_completion(&in_tx, "stdin-fault").await;
    started_rx.await.expect("active request");
    in_tx
        .send(Err(TransportError::InvalidUtf8))
        .await
        .expect("stdin fault");
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("teardown")
            .expect("join"),
        Err(chatgpt_provider::error::ChatgptError::Transport(
            TransportError::InvalidUtf8
        ))
    ));
    server.await.expect("HTTP dropped");
}

#[tokio::test]
async fn full_publisher_queue_backpressures_without_losing_required_deltas() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("address"));
    let mut body =
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n\n".repeat(400);
    body.push_str(
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"backpressure\"}}\n\n",
    );
    let server = tokio::spawn(serve_scripted_sse(
        listener,
        vec![ScriptedSseReply::complete(body)],
        Arc::new(AtomicUsize::new(0)),
    ));
    let (in_tx, mut out_rx, handle) =
        start_direct_harness(base.clone(), test_responses_client(base)).await;
    submit_direct_completion(&in_tx, "backpressure").await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while out_rx.len() < 256 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("wire queue saturated");
    let mut deltas = 0;
    loop {
        let message = tokio::time::timeout(Duration::from_secs(5), out_rx.recv())
            .await
            .expect("output")
            .expect("writer");
        let Some(body) = event_body(&message) else {
            continue;
        };
        match body.get("event").and_then(Value::as_str) {
            Some("text_delta") => deltas += 1,
            Some("completed") => {
                assert_eq!(body["text"], "x".repeat(400));
                break;
            }
            Some("error" | "failed") => panic!("unexpected terminal failure: {body:?}"),
            _ => {}
        }
    }
    assert_eq!(
        deltas, 400,
        "required deltas are not silently dropped by try_send"
    );
    finish_harness(in_tx, handle).await;
    server.await.expect("mock server");
}

/// Writer loss cancels accepted HTTP siblings without further bus input.
/// A parked stdin reader must not delay serve-mode exit after owned work settles.
#[tokio::test]
async fn physical_stdout_loss_cancels_http_and_reaps_with_stdin_open() {
    use nefor_protocol::SystemBody;
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::time::timeout;
    async fn accept_completion(listener: &TcpListener) -> tokio::net::TcpStream {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                let count = stream.read(&mut buffer).await.unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&buffer[..count]);
                let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&bytes[..end]);
                if headers.starts_with("GET ") {
                    assert!(
                        headers.contains("/models") || headers.contains("/usage"),
                        "unexpected local request: {headers}"
                    );
                    let catalog = r#"{"models":[]}"#;
                    stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", catalog.len(), catalog).as_bytes()).await.unwrap();
                    break;
                }
                assert!(headers.starts_with("POST "));
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .and_then(|value| value.parse::<usize>().ok())
                    })
                    .unwrap();
                if bytes.len() >= end + 4 + length {
                    let request: Value =
                        serde_json::from_slice(&bytes[end + 4..end + 4 + length]).unwrap();
                    assert_eq!(request["model"], "gpt-test");
                    return stream;
                }
            }
        }
    }
    let wait = Duration::from_secs(10);
    let data = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_chatgpt-provider"))
        .args([
            "--base-url",
            &format!("http://{}", listener.local_addr().unwrap()),
        ])
        .env("NEFOR_DATA_DIR", data.path())
        .env_remove("OPENAI_API_KEY")
        .env_remove("CHATGPT_ACCESS_TOKEN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    timeout(wait, stdout.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        PluginOutgoing::parse_line(line.trim_end()).unwrap().body,
        Body::System(SystemBody::Ready { .. })
    ));
    let ready = Envelope::system(
        PluginName::engine(),
        Timestamp::now(),
        SystemBody::ReadyOk {
            engine_version: "offline-test".into(),
        },
    );
    stdin
        .write_all(format!("{}\n", ready.to_line()).as_bytes())
        .await
        .unwrap();
    // auth.set uses a synthetic token without refresh/login; endpoint is loopback only.
    let auth = Envelope::event(
        PluginName::engine(),
        Timestamp::now(),
        serde_json::json!({"kind":"chatgpt.auth.set","token":"offline-fixture-token"})
            .as_object()
            .unwrap()
            .clone(),
    );
    stdin
        .write_all(format!("{}\n", auth.to_line()).as_bytes())
        .await
        .unwrap();
    stdin.flush().await.unwrap();
    let (mut models, _) = timeout(wait, listener.accept()).await.unwrap().unwrap();
    let request = timeout(wait, read_request(&mut models)).await.unwrap();
    assert!(
        request.starts_with("GET "),
        "only local catalog fetch: {request}"
    );
    let catalog = r#"{"models":[]}"#;
    models.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", catalog.len(), catalog).as_bytes()).await.unwrap();
    drop(models);
    let body = serde_json::json!({"kind":"chatgpt.completion.request","request_id":"writer-trigger",
        "model":"gpt-test","messages":[{"role":"user","content":"fixture"}]});
    let envelope = Envelope::event(
        PluginName::engine(),
        Timestamp::now(),
        body.as_object().unwrap().clone(),
    );
    stdin
        .write_all(format!("{}\n", envelope.to_line()).as_bytes())
        .await
        .unwrap();
    stdin.flush().await.unwrap();
    let mut trigger = timeout(wait, accept_completion(&listener)).await.unwrap();
    let body = serde_json::json!({"kind":"chatgpt.completion.request","request_id":"pending-sibling",
        "model":"gpt-test","messages":[{"role":"user","content":"fixture"}]});
    let envelope = Envelope::event(
        PluginName::engine(),
        Timestamp::now(),
        body.as_object().unwrap().clone(),
    );
    stdin
        .write_all(format!("{}\n", envelope.to_line()).as_bytes())
        .await
        .unwrap();
    stdin.flush().await.unwrap();
    let mut pending = timeout(wait, accept_completion(&listener)).await.unwrap();
    drop(stdout);
    let sse = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"trigger writer\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"offline-response\"}}\n\ndata: [DONE]\n\n";
    trigger.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}", sse.len(), sse).as_bytes()).await.unwrap();
    let mut cancelled_byte = [0; 1];
    assert_eq!(
        timeout(wait, pending.read(&mut cancelled_byte))
            .await
            .expect("HTTP sibling cancellation before process exit")
            .unwrap(),
        0
    );
    let status = timeout(wait, child.wait())
        .await
        .expect("process exit with stdin held open")
        .unwrap();
    assert!(
        !status.success(),
        "stdout loss cannot be reported as successful shutdown"
    );
    let mut byte = [0; 1];
    assert_eq!(
        timeout(wait, pending.read(&mut byte))
            .await
            .expect("accepted sibling HTTP cancelled")
            .unwrap(),
        0
    );
}
