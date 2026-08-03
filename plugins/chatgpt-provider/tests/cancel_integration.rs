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
    let responses_client = Arc::new(ResponsesClient::new(
        format!("http://{addr}"),
        "test-installation".into(),
        "nefor_test".into(),
    ));

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

    // Clean shutdown: drop the sender so the loop returns.
    drop(in_tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), loop_handle).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
}

#[tokio::test]
async fn invalid_direct_completion_tools_fail_once_before_http() {
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
        Arc::new(ResponsesClient::new(
            format!("http://{addr}"),
            "test-installation".into(),
            "nefor_test".into(),
        )),
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
        ("unknown", serde_json::json!(["missing"])),
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
async fn concurrent_direct_completions_keep_request_local_tool_allowlists() {
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
                {"name":"alpha","description":"Alpha","input_schema":{"type":"object","properties":{"a":{"type":"string"}}}},
                {"name":"beta","description":"Beta","input_schema":{"type":"object","properties":{"b":{"type":"integer"}}}}
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
        Arc::new(ResponsesClient::new(
            format!("http://{addr}"),
            "test-installation".into(),
            "nefor_test".into(),
        )),
        out_tx,
    );
    let (in_tx, in_rx) = mpsc::channel::<Result<Envelope, TransportError>>(64);
    let loop_handle = tokio::spawn(run_dispatch_loop(ctx, in_rx));

    for (request_id, tool) in [("request-alpha", "alpha"), ("request-beta", "beta")] {
        in_tx
            .send(Ok(event_env(
                &kind("completion.request"),
                &[
                    ("request_id", Value::String(request_id.into())),
                    ("model", Value::String("test-model".into())),
                    ("tools", serde_json::json!([tool])),
                    (
                        "messages",
                        serde_json::json!([{"role":"user","content":request_id}]),
                    ),
                ],
            )))
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
            assert_eq!(tools.len(), 1, "each request keeps exactly its allowlist");
            let tool = tools[0]["name"].as_str().expect("tool name").to_owned();
            assert_eq!(tools[0]["parameters"]["type"], "object");
            (prompt, tool)
        })
        .collect::<Vec<_>>();
    observed.sort();
    assert_eq!(
        observed,
        vec![
            ("request-alpha".into(), "alpha".into()),
            ("request-beta".into(), "beta".into())
        ]
    );

    for request_id in ["request-alpha", "request-beta"] {
        let completed =
            wait_for_completion_event(&mut out_rx, request_id, "completed", Duration::from_secs(5))
                .await
                .expect("completion settles");
        assert_eq!(completed["text"], "done");
    }

    drop(in_tx);
    server.await.expect("server");
    let _ = tokio::time::timeout(Duration::from_secs(2), loop_handle).await;
}

#[tokio::test]
async fn clean_eof_requires_terminal_event_and_next_submissions_settle() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_srv = hits.clone();

    let server = tokio::spawn(async move {
        let responses = [
            // Before output: retryable because nothing user-visible escaped.
            String::new(),
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"retried\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\"}}\n\n".into(),
            // After partial output: terminal error, never replay the visible prefix.
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n".into(),
            // A semantic terminal event makes the following transport EOF valid.
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"after-error\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r3\"}}\n\n".into(),
        ];
        for body in responses {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let _ = read_request(&mut stream).await;
            hits_srv.fetch_add(1, Ordering::SeqCst);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(), body
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
    });

    let args = Arc::new(ServeArgs {
        provider_name: PROVIDER.into(),
        base_url: format!("http://{addr}"),
    });
    let chats = Arc::new(Chats::with_default_model(None));
    let dir = tempfile::tempdir().expect("tempdir");
    let auth = Arc::new(
        AuthStore::load_from_disk(&dir.path().join("auth.json"))
            .await
            .expect("auth store"),
    );
    let _ = auth.apply_auth_set("test-token".into()).await;
    let (out_tx, mut out_rx) = mpsc::channel::<PluginOutgoing>(256);
    let ctx = DispatcherContext::new(
        args,
        chats,
        auth,
        Arc::new(ToolCatalog::new()),
        Arc::new(ToolBroker::new()),
        Arc::new(ResponsesClient::new(
            format!("http://{addr}"),
            "test-installation".into(),
            "nefor_test".into(),
        )),
        out_tx,
    );
    let (in_tx, in_rx) = mpsc::channel::<Result<Envelope, TransportError>>(64);
    let loop_handle = tokio::spawn(run_dispatch_loop(ctx, in_rx));

    async fn submit(in_tx: &mpsc::Sender<Result<Envelope, TransportError>>, id: &str) {
        in_tx
            .send(Ok(event_env(
                &kind("completion.request"),
                &[
                    ("request_id", Value::String(id.into())),
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

    submit(&in_tx, "before-output").await;
    let retried = wait_for_completion_event(
        &mut out_rx,
        "before-output",
        "completed",
        Duration::from_secs(5),
    )
    .await
    .expect("pre-output EOF retries and settles");
    assert_eq!(retried["request_id"], "before-output");
    assert_eq!(retried["event"], "completed");
    assert_eq!(retried["text"], "retried");
    assert_eq!(hits.load(Ordering::SeqCst), 2);

    submit(&in_tx, "after-partial").await;
    let failed = wait_for_completion_event(
        &mut out_rx,
        "after-partial",
        "error",
        Duration::from_secs(3),
    )
    .await
    .expect("partial EOF settles as an error");
    assert_eq!(failed["request_id"], "after-partial");
    assert_eq!(failed["event"], "error");
    assert!(failed["message"]
        .as_str()
        .is_some_and(|error| error.contains("before a terminal event")));
    assert_eq!(
        hits.load(Ordering::SeqCst),
        3,
        "visible output disables retry"
    );

    submit(&in_tx, "after-terminal").await;
    let completed = wait_for_completion_event(
        &mut out_rx,
        "after-terminal",
        "completed",
        Duration::from_secs(3),
    )
    .await
    .expect("provider remains usable after scoped EOF error");
    assert_eq!(completed["request_id"], "after-terminal");
    assert_eq!(completed["event"], "completed");
    assert_eq!(completed["text"], "after-error");
    assert_eq!(hits.load(Ordering::SeqCst), 4);

    drop(in_tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), loop_handle).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
}
