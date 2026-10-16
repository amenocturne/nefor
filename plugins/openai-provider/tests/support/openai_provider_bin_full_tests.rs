    #[tokio::test]
    async fn direct_structured_completion_suppresses_text_deltas_but_keeps_terminal_json() {
        let events = dispatch_direct_completion(Some(serde_json::json!({
            "type": "object",
            "properties": {"content": {"type": "string"}},
            "required": ["content"],
        })))
        .await;

        assert!(!events
            .iter()
            .any(|event| { event.get("event").and_then(Value::as_str) == Some("text_delta") }));
        assert!(events.iter().any(|event| {
            event.get("event").and_then(Value::as_str) == Some("reasoning_delta")
        }));
        assert!(events
            .iter()
            .any(|event| event.get("event").and_then(Value::as_str) == Some("usage")));
        let usage = events
            .iter()
            .find(|event| event.get("event").and_then(Value::as_str) == Some("usage"))
            .expect("usage event");
        assert!(usage.get("duration_ms").and_then(Value::as_u64).is_some());
        let reasoning_end = events
            .iter()
            .find(|event| event.get("event").and_then(Value::as_str) == Some("reasoning_end"))
            .expect("reasoning end event");
        assert!(reasoning_end
            .get("duration_ms")
            .and_then(Value::as_u64)
            .is_some());
        let terminal = events.last().expect("terminal event");
        assert_eq!(
            terminal.get("event").and_then(Value::as_str),
            Some("completed")
        );
        assert_eq!(
            terminal.get("text").and_then(Value::as_str),
            Some(r#"{"content":"ok"}"#)
        );
        assert_eq!(
            terminal.get("model").and_then(Value::as_str),
            Some("qwen2.5-coder:7b")
        );
        assert!(terminal
            .get("duration_ms")
            .and_then(Value::as_u64)
            .is_some());
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event.get("event").and_then(Value::as_str),
                    Some("completed" | "error")
                ))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn direct_unstructured_completion_still_emits_text_deltas() {
        let events = dispatch_direct_completion(None).await;
        let deltas: Vec<_> = events
            .iter()
            .filter(|event| event.get("event").and_then(Value::as_str) == Some("text_delta"))
            .filter_map(|event| event.get("text").and_then(Value::as_str))
            .collect();

        assert_eq!(deltas, vec![r#"{"content":"#, r#""ok"}"#]);
        assert_eq!(
            events
                .last()
                .and_then(|event| event.get("text"))
                .and_then(Value::as_str),
            Some(r#"{"content":"ok"}"#)
        );
    }

    #[tokio::test]
    async fn direct_completion_caches_tools_unsupported_per_model() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        #[derive(Clone, Copy)]
        struct DirectTestDeps<'a> {
            chats: &'a Arc<Chats>,
            completions: &'a Arc<CompletionRuns>,
            auth: &'a Arc<AuthStore>,
            catalog: &'a Arc<ToolCatalog>,
            broker: &'a Arc<ToolBroker>,
        }

        async fn read_request(stream: &mut tokio::net::TcpStream) -> Value {
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = stream.read(&mut buffer).await.expect("read request");
                assert!(read > 0, "request closed before its complete body");
                bytes.extend_from_slice(&buffer[..read]);
                let Some(headers_end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&bytes[..headers_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .expect("content length");
                let body_start = headers_end + 4;
                if bytes.len() >= body_start + content_length {
                    return serde_json::from_slice(&bytes[body_start..body_start + content_length])
                        .expect("request json");
                }
            }
        }

        async fn dispatch_and_wait(
            request_id: &str,
            model: &str,
            deps: DirectTestDeps<'_>,
            config: &Config,
            client: &reqwest::Client,
            tx: &mpsc::Sender<PluginOutgoing>,
            rx: &mut mpsc::Receiver<PluginOutgoing>,
        ) {
            let body = make_event_body(
                "ollama.completion.request",
                &[
                    ("request_id", Value::String(request_id.into())),
                    ("model", Value::String(model.into())),
                    (
                        "messages",
                        serde_json::json!([{"role": "user", "content": "answer"}]),
                    ),
                    ("tools", serde_json::json!(["read_file"])),
                    (
                        "tool_specs",
                        serde_json::json!([{
                            "name": "read_file", "description": "Read a file",
                            "parameters": {"type": "object"}
                        }]),
                    ),
                    (
                        "conversation_id",
                        Value::String("stable-provider-context".into()),
                    ),
                ],
            );
            dispatch_event_with_completions(
                deps.chats,
                deps.completions,
                deps.auth,
                deps.catalog,
                deps.broker,
                config,
                client,
                tx,
                &from_plugin("mag"),
                &body,
            )
            .await
            .expect("dispatch completion");

            loop {
                let outgoing = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                    .await
                    .expect("completion timeout")
                    .expect("completion channel");
                let envelope: Value =
                    serde_json::from_str(&outgoing.to_line()).expect("outgoing json");
                let event = envelope["body"]["event"].as_str();
                if matches!(event, Some("completed" | "error")) {
                    assert_eq!(event, Some("completed"));
                    break;
                }
            }
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("address");
        let (request_tx, mut request_rx) = mpsc::channel(4);
        let server = tokio::spawn(async move {
            for index in 0..4 {
                let (mut stream, _) = listener.accept().await.expect("accept");
                request_tx
                    .send(read_request(&mut stream).await)
                    .await
                    .expect("capture request");
                if index == 0 {
                    let body = r#"{"error":"model does not support tools"}"#;
                    let response = format!(
                        "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(), body
                    );
                    stream
                        .write_all(response.as_bytes())
                        .await
                        .expect("write 400");
                } else {
                    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n\
                                data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n\
                                data: [DONE]\n\n";
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(), body
                    );
                    stream
                        .write_all(response.as_bytes())
                        .await
                        .expect("write 200");
                }
            }
        });

        let auth = Arc::new(AuthStore::from_env_key(Some("test-key".into())));
        let chats = fresh_chats("test-model");
        let completions = Arc::new(CompletionRuns::new());
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let deps = DirectTestDeps {
            chats: &chats,
            completions: &completions,
            auth: &auth,
            catalog: &catalog,
            broker: &broker,
        };
        let mut config = cfg("ollama");
        config.base_url = format!("http://{addr}");
        let client = reqwest::Client::builder().build().expect("client");
        let (tx, mut rx) = mpsc::channel(32);

        dispatch_and_wait("first-a", "model-a", deps, &config, &client, &tx, &mut rx).await;
        dispatch_and_wait("second-a", "model-a", deps, &config, &client, &tx, &mut rx).await;
        dispatch_and_wait("first-b", "model-b", deps, &config, &client, &tx, &mut rx).await;

        server.await.expect("server");
        let mut tool_presence = Vec::new();
        while let Some(request) = request_rx.recv().await {
            tool_presence.push(request.get("tools").is_some());
        }
        assert_eq!(tool_presence, vec![true, false, false, true]);
    }

    #[tokio::test]
    async fn models_list_requested_emits_models_listed_event() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");

        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.expect("accept");
            let mut buf = vec![0u8; 4096];
            let mut acc = String::new();
            while !acc.contains("\r\n\r\n") {
                let n = s.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                acc.push_str(&String::from_utf8_lossy(&buf[..n]));
            }
            let body = r#"{"object":"list","data":[{"id":"qwen2.5-coder:7b"},{"id":"llama3:8b"}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = s.write_all(response.as_bytes()).await;
            let _ = s.shutdown().await;
        });

        let (auth, tx, mut rx) = auth_test_rig(Some("envkey"));
        let chats = fresh_chats("any");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let mut config = cfg("ollama");
        config.base_url = format!("http://{}", addr);
        let client = reqwest::Client::builder().build().expect("client");

        let body = make_event_body("ollama.models.list_requested", &[]);
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("nefor-chat"),
            &body,
        )
        .await
        .expect("dispatch ok");

        let _ = server.await;

        let emitted = drain(&mut rx).await;
        assert_eq!(emitted.len(), 1);
        assert_eq!(
            emitted[0].get("kind").unwrap().as_str(),
            Some("ollama.models.listed")
        );
        let arr = emitted[0].get("models").unwrap().as_array().expect("array");
        // Sorted alphabetically.
        assert_eq!(arr[0].as_str(), Some("llama3:8b"));
        assert_eq!(arr[1].as_str(), Some("qwen2.5-coder:7b"));
    }

    /// End-to-end: queue a tool call, simulate a fast tool reply via
    /// the broker, and verify run_one_tool_call emits the right
    /// chat.tool.start / <plugin>.tool.invoke / chat.tool.end sequence.
    #[tokio::test]
    async fn run_one_tool_call_routes_invoke_then_emits_end_on_success() {
        let catalog = Arc::new(ToolCatalog::new());
        catalog
            .register_from(
                "basic-tools",
                vec![openai_provider::catalog::ToolSpec {
                    name: "read_file".into(),
                    owner: "basic-tools".into(),
                    description: "Read a file.".into(),
                    parameters: serde_json::json!({"type": "object"}),
                    execution: openai_provider::catalog::ToolExecution::Routed,
                }],
            )
            .await;
        let broker = Arc::new(ToolBroker::new());
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(16);
        let cancel = tokio_util::sync::CancellationToken::new();

        let tc = ToolCall {
            id: "call_1".into(),
            kind: "function".into(),
            function: ToolCallFunction {
                name: "read_file".into(),
                arguments: "{\"path\":\"/tmp/x\"}".into(),
            },
        };

        // Spawn the tool runner; meanwhile deliver a result via the
        // broker as if the tool plugin had replied.
        let broker_clone = broker.clone();
        let runner = tokio::spawn(async move {
            run_one_tool_call(&catalog, &broker_clone, &tx, &cancel, tc).await
        });

        // Give the runner a tick to register pending + emit invoke.
        tokio::time::sleep(Duration::from_millis(20)).await;
        broker
            .deliver(ToolResult {
                id: "call_1".into(),
                output: Some("file body".into()),
                error: None,
            })
            .await;

        let outcome = runner.await.expect("runner");
        match outcome {
            ToolStepOutcome::Result { id, content } => {
                assert_eq!(id, "call_1");
                assert_eq!(content, "file body");
            }
            ToolStepOutcome::Cancelled { .. } => panic!("unexpected cancel"),
        }

        // Inspect the event sequence.
        let mut events = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            let line = msg.to_line();
            let v: Value = serde_json::from_str(&line).expect("json");
            if v.get("type").and_then(Value::as_str) == Some("event") {
                events.push(v.get("body").unwrap().clone());
            }
        }
        assert_eq!(events.len(), 3, "start + invoke + end");
        assert_eq!(
            events[0].get("kind").and_then(Value::as_str),
            Some("chat.tool.start")
        );
        assert_eq!(
            events[1].get("kind").and_then(Value::as_str),
            Some("basic-tools.tool.invoke")
        );
        assert_eq!(
            events[2].get("kind").and_then(Value::as_str),
            Some("chat.tool.end")
        );
        assert_eq!(
            events[2].get("output").and_then(Value::as_str),
            Some("file body")
        );
        assert_eq!(events[2].get("error").and_then(Value::as_bool), Some(false));
    }

    /// Regression for the cancel-mid-stream contract: when a `/cancel`
    /// fires while the model is mid-response, the partial assistant
    /// text the model has already emitted MUST land in the chat's
    /// history table on the provider binary side. The user-facing
    /// motivation: "you started thinking wrong, reconsider" only works
    /// if the next turn's request includes what the model just said
    /// before being cut off.
    ///
    /// Drive: spin up an SSE server that streams 5 deltas with a 30ms
    /// pause between each (well past the watchdog floor of any path
    /// here, well below the test's 5s timeout); fire chat.create →
    /// chat.append (user) → chat.complete; wait for at least 2 deltas
    /// to reach the writer channel; call chats.interrupt(&chat_id)
    /// directly (the bus-side path is `<prefix>.interrupt` →
    /// `chats.interrupt_all()`, which is functionally identical for
    /// the per-chat case here); wait for chat.complete.result; assert
    /// chats.history_snapshot's last message is an assistant message
    /// with non-empty content equal to a prefix of the deltas the
    /// server actually wrote.
    #[tokio::test]
    async fn chat_complete_persists_partial_assistant_to_history_on_interrupt_midstream() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");

        // Slow-stream SSE: send each delta line, flush, wait 30ms,
        // repeat. Five deltas → ~150ms total before [DONE]; the test
        // interrupts after the second delta lands in the writer
        // channel, so the server may still be mid-write when the
        // cancel fires. That's the production shape — the cancel
        // token lives inside `run_chat_stream`'s `tokio::select!` and
        // races the `byte_stream.next()` arm.
        let _server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.expect("accept");
            // Drain request headers + body before responding.
            let mut buf = vec![0u8; 4096];
            let mut acc = String::new();
            while !acc.contains("\r\n\r\n") {
                let n = s.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                acc.push_str(&String::from_utf8_lossy(&buf[..n]));
            }
            // Headers + chunked transfer (Content-Length unknown for a
            // paced stream).
            let _ = s
                .write_all(
                    b"HTTP/1.1 200 OK\r\n\
                      Content-Type: text/event-stream\r\n\
                      Transfer-Encoding: chunked\r\n\
                      Connection: close\r\n\r\n",
                )
                .await;
            for word in ["alpha", "beta", "gamma", "delta", "epsilon"] {
                let frame = format!(
                    "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{word} \"}}}}]}}\n\n",
                );
                let chunk = format!("{:x}\r\n{}\r\n", frame.len(), frame);
                if s.write_all(chunk.as_bytes()).await.is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
            let finish = "data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n";
            let usage = "data: {\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":5}}\n\n";
            let done = "data: [DONE]\n\n";
            for frame in [finish, usage, done] {
                let chunk = format!("{:x}\r\n{}\r\n", frame.len(), frame);
                if s.write_all(chunk.as_bytes()).await.is_err() {
                    return;
                }
            }
            let _ = s.write_all(b"0\r\n\r\n").await;
            let _ = s.shutdown().await;
        });

        let (auth, tx, mut rx) = auth_test_rig(Some("envkey"));
        let chats = fresh_chats("test-model");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let mut config = cfg("ollama");
        config.base_url = format!("http://{}", addr);
        let client = reqwest::Client::builder().build().expect("client");

        let chat_id = ChatId::new("c-interrupt");

        // 1. chat.create.
        let create_body = make_event_body(
            "ollama.chat.create",
            &[("chat_id", Value::String("c-interrupt".into()))],
        );
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("reasoner-graph"),
            &create_body,
        )
        .await
        .expect("create");
        chats
            .record_turn(&chat_id, Some("test-model"), Some((17, 9)), 123)
            .await
            .expect("seed stats");

        // 2. chat.append { role=user }.
        let append_body = make_event_body(
            "ollama.chat.append",
            &[
                ("chat_id", Value::String("c-interrupt".into())),
                (
                    "message",
                    serde_json::json!({"role": "user", "content": "hi"}),
                ),
            ],
        );
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("reasoner-graph"),
            &append_body,
        )
        .await
        .expect("append");

        // 3. chat.complete — kicks off the spawned turn.
        let complete_body = make_event_body(
            "ollama.chat.complete",
            &[("chat_id", Value::String("c-interrupt".into()))],
        );
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("reasoner-graph"),
            &complete_body,
        )
        .await
        .expect("complete");

        // 4. Wait for at least 2 stream.delta envelopes to reach the
        //    writer channel — confirms we're mid-stream when the
        //    cancel fires.
        let mut delta_count = 0;
        let mut wire_partial = String::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while delta_count < 2 && std::time::Instant::now() < deadline {
            if let Ok(msg) = rx.try_recv() {
                let line = msg.to_line();
                let v: Value = serde_json::from_str(&line).expect("plugin out json");
                if let Some(body) = v.get("body").and_then(Value::as_object) {
                    if body.get("kind").and_then(Value::as_str) == Some("ollama.stream.delta") {
                        delta_count += 1;
                        if let Some(t) = body.get("text").and_then(Value::as_str) {
                            wire_partial.push_str(t);
                        }
                    }
                }
            } else {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
        assert!(
            delta_count >= 2,
            "expected at least 2 stream.delta envelopes before timeout; got {delta_count}",
        );
        assert!(
            !wire_partial.is_empty(),
            "wire_partial must accumulate the delta text",
        );

        // 5. Interrupt the in-flight turn directly. The bus-side path
        //    is `<prefix>.interrupt` → chats.interrupt_all(); the
        //    per-chat shape calls chats.interrupt(&chat_id).
        let was_in_flight = chats.interrupt(&chat_id).await;
        assert!(was_in_flight, "interrupt should land on an in-flight turn");

        // 6. Wait for chat.complete.result on the writer channel —
        //    that's the marker the spawned turn finished its cleanup.
        let mut saw_complete_result = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if let Ok(msg) = rx.try_recv() {
                let line = msg.to_line();
                let v: Value = serde_json::from_str(&line).expect("json");
                if let Some(body) = v.get("body").and_then(Value::as_object) {
                    if body.get("kind").and_then(Value::as_str)
                        == Some("ollama.chat.complete.result")
                    {
                        saw_complete_result = true;
                        break;
                    }
                }
            } else {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
        assert!(
            saw_complete_result,
            "chat.complete.result must arrive after interrupt"
        );

        // 7. The persistence assertion. History should be:
        //    [user="hi", assistant=<partial>]. The partial assistant
        //    content must be non-empty (regression: pre-fix it would
        //    not be pushed when the path was wrong).
        let history = chats
            .history_snapshot(&chat_id)
            .await
            .expect("history snapshot");
        assert_eq!(
            history.len(),
            2,
            "expected [user, assistant] in history, got {history:?}",
        );
        assert_eq!(history[0].role(), "user");
        assert_eq!(history[0].content(), Some("hi"));
        assert_eq!(history[1].role(), "assistant");
        let assistant_content = history[1].content().expect("assistant content set");
        assert!(
            !assistant_content.is_empty(),
            "assistant content must hold the partial text streamed before interrupt; \
             history was {history:?}; wire_partial = {wire_partial:?}",
        );
        // The persisted partial is whatever the stream loop accumulated
        // into `outcome.full_text` at the moment the cancel token fired
        // — usually equal to or a 1-2-chunk superset of what the writer
        // channel had at the moment we called `chats.interrupt`. Either
        // direction (wire is a prefix of stored, or stored is a prefix
        // of wire) is acceptable; both indicate the same underlying
        // delta accumulator.
        assert!(
            assistant_content.starts_with(wire_partial.trim_end())
                || wire_partial.starts_with(assistant_content),
            "stored partial and wire-observed partial must share a prefix; \
             stored={assistant_content:?} wire={wire_partial:?}",
        );
        let stats = chats.stats_snapshot(&chat_id).await.expect("stats");
        assert_eq!(stats.turns_completed, 2);
        assert_eq!(stats.cumulative_input_tokens, 17);
        assert_eq!(stats.cumulative_output_tokens, 9);
        assert_eq!(stats.last_turn_input_tokens, 17);
        assert_eq!(stats.last_turn_output_tokens, 9);
        assert_eq!(stats.last_turn_context_tokens, 17);
        assert!(stats.last_turn_duration_ms.is_some());
    }

    /// Fix #1 regression: when `<prefix>.chat.complete` carries an
    /// `extra_tools` array, every iteration's upstream HTTP request body
    /// MUST include those tools alongside whatever the global
    /// ToolCatalog provided. The agent reasoner relies on this to inject
    /// its synthetic `finalize` schema per-firing without polluting the
    /// catalog. Pre-fix the chat.complete dispatcher ignored
    /// `extra_tools` and the tools array was catalog-only — the model
    /// never saw `finalize` in its advertised set.
    ///
    /// Drive: bind an SSE server that captures the first request body
    /// it receives and replies with a single delta + finish + DONE.
    /// Fire chat.create → chat.append (user) → chat.complete (with
    /// extra_tools = [finalize_schema]). Wait for
    /// chat.complete.result. Assert the captured request body's
    /// `tools` array contains an entry with name == "finalize".
    #[tokio::test]
    async fn chat_complete_extra_tools_lands_in_upstream_request_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");

        let captured: std::sync::Arc<tokio::sync::Mutex<String>> =
            std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
        let captured_clone = captured.clone();

        let _server = tokio::spawn(async move {
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
            // One delta + finish + DONE — terminates the loop cleanly so
            // the test doesn't hang waiting for chat.complete.result.
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

        let (auth, tx, mut rx) = auth_test_rig(Some("envkey"));
        let chats = fresh_chats("test-model");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let mut config = cfg("ollama");
        config.base_url = format!("http://{}", addr);
        let client = reqwest::Client::builder().build().expect("client");

        // 1. chat.create.
        let create_body = make_event_body(
            "ollama.chat.create",
            &[("chat_id", Value::String("c-extra".into()))],
        );
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("reasoner-graph"),
            &create_body,
        )
        .await
        .expect("create");

        // 2. chat.append { role=user }.
        let append_body = make_event_body(
            "ollama.chat.append",
            &[
                ("chat_id", Value::String("c-extra".into())),
                (
                    "message",
                    serde_json::json!({"role": "user", "content": "go"}),
                ),
            ],
        );
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("reasoner-graph"),
            &append_body,
        )
        .await
        .expect("append");

        // 3. chat.complete with extra_tools carrying the finalize
        // schema. Mirrors the agent reasoner's emit shape:
        // `extra_tools = [FINALIZE_SCHEMA]`.
        let finalize_schema = serde_json::json!({
            "type": "function",
            "function": {
                "name": "finalize",
                "description": "Terminate this agent's run.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "answer": {"type": "string"}
                    },
                    "required": ["answer"],
                    "additionalProperties": true,
                },
            },
        });
        let complete_body = make_event_body(
            "ollama.chat.complete",
            &[
                ("chat_id", Value::String("c-extra".into())),
                ("extra_tools", Value::Array(vec![finalize_schema])),
            ],
        );
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("reasoner-graph"),
            &complete_body,
        )
        .await
        .expect("complete");

        // 4. Wait for chat.complete.result on the writer channel —
        //    that's the marker the spawned turn finished its HTTP call.
        let mut saw_complete_result = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if let Ok(msg) = rx.try_recv() {
                let line = msg.to_line();
                let v: Value = serde_json::from_str(&line).expect("json");
                if let Some(body) = v.get("body").and_then(Value::as_object) {
                    if body.get("kind").and_then(Value::as_str)
                        == Some("ollama.chat.complete.result")
                    {
                        saw_complete_result = true;
                        break;
                    }
                }
            } else {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
        assert!(
            saw_complete_result,
            "chat.complete.result must arrive after the SSE server closes",
        );

        // 5. Assert the captured upstream request body included the
        //    finalize tool in its `tools` array.
        let body = captured.lock().await.clone();
        assert!(!body.is_empty(), "server must have captured a request body",);
        let v: serde_json::Value = serde_json::from_str(&body).expect("upstream request body json");
        let tools = v
            .get("tools")
            .and_then(|t| t.as_array())
            .expect("upstream request body must include `tools` when extra_tools provided");
        let saw_finalize = tools.iter().any(|t| {
            t.get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                == Some("finalize")
        });
        assert!(
            saw_finalize,
            "tools array MUST include an entry whose function.name == \"finalize\"; \
             body was {body}",
        );
    }

    #[tokio::test]
    async fn chat_complete_http_error_emits_chat_error_not_empty_result() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");

        let _server = tokio::spawn(async move {
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
            let body = "{\"error\":{\"message\":\"Input is a zero-length, empty document: line 1 column 1 (char 0)\"}}";
            let response = format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = s.write_all(response.as_bytes()).await;
            let _ = s.shutdown().await;
        });

        let (auth, tx, mut rx) = auth_test_rig(Some("envkey"));
        let chats = fresh_chats("test-model");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let mut config = cfg("ollama");
        config.base_url = format!("http://{}", addr);
        let client = reqwest::Client::builder().build().expect("client");

        let create_body = make_event_body(
            "ollama.chat.create",
            &[("chat_id", Value::String("c-http-error".into()))],
        );
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("reasoner-graph"),
            &create_body,
        )
        .await
        .expect("create");

        let append_body = make_event_body(
            "ollama.chat.append",
            &[
                ("chat_id", Value::String("c-http-error".into())),
                (
                    "message",
                    serde_json::json!({"role": "user", "content": "hello"}),
                ),
            ],
        );
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("reasoner-graph"),
            &append_body,
        )
        .await
        .expect("append");
        let _ = drain(&mut rx).await;

        let complete_body = make_event_body(
            "ollama.chat.complete",
            &[("chat_id", Value::String("c-http-error".into()))],
        );
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("reasoner-graph"),
            &complete_body,
        )
        .await
        .expect("complete");

        let mut saw_chat_error = false;
        let mut saw_complete_result = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if let Ok(msg) = rx.try_recv() {
                let line = msg.to_line();
                let v: Value = serde_json::from_str(&line).expect("json");
                if let Some(body) = v.get("body").and_then(Value::as_object) {
                    match body.get("kind").and_then(Value::as_str) {
                        Some("ollama.chat.error") => {
                            saw_chat_error = true;
                            let message = body
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            assert!(
                                message.contains("Input is a zero-length"),
                                "message was: {message}",
                            );
                        }
                        Some("ollama.chat.complete.result") => {
                            saw_complete_result = true;
                        }
                        _ => {}
                    }
                }
            } else {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            if saw_chat_error {
                break;
            }
        }

        assert!(saw_chat_error, "chat.error must arrive for HTTP failures");
        assert!(
            !saw_complete_result,
            "HTTP failure must not be lowered to chat.complete.result"
        );
    }

    /// Per-chat tool-name allowlist regression: when `chat.create.tools`
    /// is an array of strings, the per-turn upstream request body's
    /// `tools` array MUST be filtered to entries whose function.name is
    /// in the list. Catalog entries outside the list are dropped.
    ///
    /// Drives the lead-orchestrator runtime fix — pre-fix the lead's
    /// chat advertised the full catalog (including reasoner-graph
    /// internals like `spawn_graph`), so the model would happily call
    /// `spawn_graph` directly and bottom out in `reasoner '<role>' not
    /// connected`. The allowlist is the substrate that prevents the
    /// model from ever seeing names it shouldn't call.
    ///
    /// Drive: register two tools in the catalog (`read_file`,
    /// `spawn_graph`); chat.create with `tools = ["read_file"]`;
    /// chat.complete. Capture the upstream request body and assert
    /// `read_file` is present and `spawn_graph` is absent.
    #[tokio::test]
    async fn chat_create_tools_string_array_filters_per_turn_tools_array() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");

        let captured: std::sync::Arc<tokio::sync::Mutex<String>> =
            std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
        let captured_clone = captured.clone();

        let _server = tokio::spawn(async move {
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

        let (auth, tx, mut rx) = auth_test_rig(Some("envkey"));
        let chats = fresh_chats("test-model");
        let catalog = Arc::new(ToolCatalog::new());
        // Seed two tools: only `read_file` is in the chat's allowlist;
        // `spawn_graph` is in the catalog but must be filtered out.
        catalog
            .register_from(
                "basic-tools",
                vec![
                    openai_provider::catalog::ToolSpec {
                        name: "read_file".into(),
                        owner: "basic-tools".into(),
                        description: "Read a file.".into(),
                        parameters: serde_json::json!({"type":"object"}),
                        execution: openai_provider::catalog::ToolExecution::Routed,
                    },
                    openai_provider::catalog::ToolSpec {
                        name: "spawn_graph".into(),
                        owner: "basic-tools".into(),
                        description: "Reasoner-graph internal.".into(),
                        parameters: serde_json::json!({"type":"object"}),
                        execution: openai_provider::catalog::ToolExecution::Routed,
                    },
                ],
            )
            .await;
        let broker = Arc::new(ToolBroker::new());
        let mut config = cfg("ollama");
        config.base_url = format!("http://{}", addr);
        let client = reqwest::Client::builder().build().expect("client");

        // 1. chat.create with `tools = ["read_file"]`.
        let create_body = make_event_body(
            "ollama.chat.create",
            &[
                ("chat_id", Value::String("c-allow".into())),
                (
                    "tools",
                    Value::Array(vec![Value::String("read_file".into())]),
                ),
            ],
        );
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("reasoner-graph"),
            &create_body,
        )
        .await
        .expect("create");

        // 2. chat.append { role=user }.
        let append_body = make_event_body(
            "ollama.chat.append",
            &[
                ("chat_id", Value::String("c-allow".into())),
                (
                    "message",
                    serde_json::json!({"role": "user", "content": "go"}),
                ),
            ],
        );
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("reasoner-graph"),
            &append_body,
        )
        .await
        .expect("append");

        // 3. chat.complete (no extra_tools).
        let complete_body = make_event_body(
            "ollama.chat.complete",
            &[("chat_id", Value::String("c-allow".into()))],
        );
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("reasoner-graph"),
            &complete_body,
        )
        .await
        .expect("complete");

        // 4. Wait for chat.complete.result.
        let mut saw_complete_result = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if let Ok(msg) = rx.try_recv() {
                let line = msg.to_line();
                let v: Value = serde_json::from_str(&line).expect("json");
                if let Some(body) = v.get("body").and_then(Value::as_object) {
                    if body.get("kind").and_then(Value::as_str)
                        == Some("ollama.chat.complete.result")
                    {
                        saw_complete_result = true;
                        break;
                    }
                }
            } else {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
        assert!(
            saw_complete_result,
            "chat.complete.result must arrive after the SSE server closes",
        );

        // 5. Inspect the captured upstream body — only `read_file`
        //    should be present in the tools array. `spawn_graph` MUST
        //    have been filtered.
        let body = captured.lock().await.clone();
        assert!(!body.is_empty(), "server must have captured a request body",);
        let v: serde_json::Value = serde_json::from_str(&body).expect("upstream request body json");
        let tools = v
            .get("tools")
            .and_then(|t| t.as_array())
            .expect("upstream request body must include `tools`");
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| {
                t.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
            })
            .collect();
        assert!(
            names.contains(&"read_file"),
            "allowed tool `read_file` must be present in upstream tools; saw {names:?}",
        );
        assert!(
            !names.contains(&"spawn_graph"),
            "filtered tool `spawn_graph` MUST be absent from upstream tools; saw {names:?}",
        );
    }

    /// Hard-cancel mid-stream contract (the receive side of the kernel's
    /// kill flush, mirroring chatgpt-provider's cancel_integration):
    ///
    ///   1. Start a completion, stream some deltas, then
    ///      `<prefix>.chat.cancel { chat_id }` → the in-flight HTTP
    ///      stream is aborted and NO terminal event lands — no
    ///      `chat.complete.result`, no `stream.end`, no `turn.error`,
    ///      no `session.stats`. Unlike a graceful interrupt, the
    ///      partial assistant text is NOT persisted to history.
    ///   2. The chat's turn slot is released (no dangling InFlight
    ///      state), and the same chat serves a subsequent completion
    ///      normally.
    #[tokio::test]
    async fn chat_cancel_midstream_suppresses_terminal_events_and_chat_stays_usable() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");

        async fn drain_headers(s: &mut tokio::net::TcpStream) {
            let mut buf = vec![0u8; 4096];
            let mut acc = String::new();
            while !acc.contains("\r\n\r\n") {
                let n = s.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                acc.push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        }

        let _server = tokio::spawn(async move {
            // conn1: stream two deltas, then hold the socket open until
            // the client aborts (the hard cancel drops the reqwest byte
            // stream → EOF here). Never sends finish/[DONE].
            let (mut s1, _) = listener.accept().await.expect("accept 1");
            drain_headers(&mut s1).await;
            let _ = s1
                .write_all(
                    b"HTTP/1.1 200 OK\r\n\
                      Content-Type: text/event-stream\r\n\
                      Transfer-Encoding: chunked\r\n\
                      Connection: close\r\n\r\n",
                )
                .await;
            for word in ["alpha", "beta"] {
                let frame = format!(
                    "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{word} \"}}}}]}}\n\n",
                );
                let chunk = format!("{:x}\r\n{}\r\n", frame.len(), frame);
                if s1.write_all(chunk.as_bytes()).await.is_err() {
                    return;
                }
            }
            let mut buf = [0u8; 256];
            while let Ok(n) = s1.read(&mut buf).await {
                if n == 0 {
                    break;
                }
            }

            // conn2: full streaming completion for the post-cancel turn.
            let (mut s2, _) = listener.accept().await.expect("accept 2");
            drain_headers(&mut s2).await;
            let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n\
                        data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n\
                        data: {\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1}}\n\n\
                        data: [DONE]\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = s2.write_all(response.as_bytes()).await;
            let _ = s2.shutdown().await;
        });

        let auth = Arc::new(AuthStore::from_env_key(Some("envkey".into())));
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(64);
        let chats = fresh_chats("test-model");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let mut config = cfg("ollama");
        config.base_url = format!("http://{}", addr);
        let client = reqwest::Client::builder().build().expect("client");

        let chat_id = ChatId::new("c-cancel");
        let dispatch = |body: Map<String, Value>| {
            let chats = chats.clone();
            let auth = auth.clone();
            let catalog = catalog.clone();
            let broker = broker.clone();
            let config = config.clone();
            let client = client.clone();
            let tx = tx.clone();
            async move {
                dispatch_event(
                    &chats,
                    &auth,
                    &catalog,
                    &broker,
                    &config,
                    &client,
                    &tx,
                    &from_plugin("mag"),
                    &body,
                )
                .await
                .expect("dispatch ok");
            }
        };

        // 1. chat.create → chat.append(user) → chat.complete.
        dispatch(make_event_body(
            "ollama.chat.create",
            &[("chat_id", Value::String("c-cancel".into()))],
        ))
        .await;
        dispatch(make_event_body(
            "ollama.chat.append",
            &[
                ("chat_id", Value::String("c-cancel".into())),
                (
                    "message",
                    serde_json::json!({"role": "user", "content": "hi"}),
                ),
            ],
        ))
        .await;
        dispatch(make_event_body(
            "ollama.chat.complete",
            &[("chat_id", Value::String("c-cancel".into()))],
        ))
        .await;

        // 2. Wait until we're genuinely mid-stream (both deltas on the
        //    writer channel) before cancelling.
        let mut delta_count = 0;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while delta_count < 2 && std::time::Instant::now() < deadline {
            if let Ok(msg) = rx.try_recv() {
                let line = msg.to_line();
                let v: Value = serde_json::from_str(&line).expect("plugin out json");
                if let Some(body) = v.get("body").and_then(Value::as_object) {
                    if body.get("kind").and_then(Value::as_str) == Some("ollama.stream.delta") {
                        delta_count += 1;
                    }
                }
            } else {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
        assert_eq!(delta_count, 2, "must be mid-stream before the cancel");

        // 3. Hard cancel via the wire kind the kernel flushes at kill.
        dispatch(make_event_body(
            "ollama.chat.cancel",
            &[("chat_id", Value::String("c-cancel".into()))],
        ))
        .await;

        // 4. Settle window: NO terminal event for the cancelled turn.
        let forbidden = [
            "ollama.chat.complete.result",
            "ollama.stream.end",
            "ollama.turn.error",
            "ollama.session.stats",
            "ollama.chat.error",
        ];
        let deadline = std::time::Instant::now() + Duration::from_millis(600);
        while std::time::Instant::now() < deadline {
            if let Ok(msg) = rx.try_recv() {
                let line = msg.to_line();
                let v: Value = serde_json::from_str(&line).expect("json");
                if let Some(body) = v.get("body").and_then(Value::as_object) {
                    let kind = body.get("kind").and_then(Value::as_str).unwrap_or("");
                    assert!(
                        !forbidden.contains(&kind),
                        "cancelled completion must not deliver a terminal event; got {body:?}",
                    );
                }
            } else {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }

        // 5. Unlike graceful interrupt, the partial assistant text is
        //    NOT persisted — history holds only the user message.
        let history = chats
            .history_snapshot(&chat_id)
            .await
            .expect("history snapshot");
        assert_eq!(
            history.len(),
            1,
            "hard cancel must not persist partial assistant text; got {history:?}",
        );
        assert_eq!(history[0].role(), "user");

        // 6. The turn slot was released — the same chat runs a fresh
        //    completion end to end.
        dispatch(make_event_body(
            "ollama.chat.append",
            &[
                ("chat_id", Value::String("c-cancel".into())),
                (
                    "message",
                    serde_json::json!({"role": "user", "content": "again"}),
                ),
            ],
        ))
        .await;
        dispatch(make_event_body(
            "ollama.chat.complete",
            &[("chat_id", Value::String("c-cancel".into()))],
        ))
        .await;

        let mut result: Option<Map<String, Value>> = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while result.is_none() && std::time::Instant::now() < deadline {
            if let Ok(msg) = rx.try_recv() {
                let line = msg.to_line();
                let v: Value = serde_json::from_str(&line).expect("json");
                if let Some(body) = v.get("body").and_then(Value::as_object) {
                    let kind = body.get("kind").and_then(Value::as_str).unwrap_or("");
                    if kind == "ollama.turn.error" {
                        panic!("post-cancel completion must not error: {body:?}");
                    }
                    if kind == "ollama.chat.complete.result" {
                        result = Some(body.clone());
                    }
                }
            } else {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
        let result = result.expect("post-cancel completion must deliver chat.complete.result");
        assert_eq!(
            result.get("chat_id").and_then(Value::as_str),
            Some("c-cancel"),
        );
        let text = result
            .get("output")
            .and_then(|o| o.get("text"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert_eq!(text, "hello", "second completion streamed its output");
    }

    #[tokio::test]
    async fn oversized_completed_response_errors_once_and_same_request_is_reusable() {
        let (base_url, server) = large_response_server().await;
        let mut config = cfg("fixture");
        config.base_url = base_url;
        let client = reqwest::Client::new();
        let completions = Arc::new(CompletionRuns::new());
        let auth = Arc::new(AuthStore::from_env_key(None));
        let catalog = Arc::new(ToolCatalog::new());
        let (wire, mut output) = mpsc::channel(32);
        let tasks = Arc::new(publisher::Tasks::default());
        publisher::TASKS.scope(tasks.clone(), async {
            for expected in ["error", "completed"] {
                let body = make_event_body("fixture.completion.request", &[
                    ("request_id", Value::String("reused-request".into())),
                    ("messages", serde_json::json!([{"role":"user","content":"answer"}])),
                    ("output_schema", serde_json::json!({"type":"string"})),
                ]);
                dispatch_completion_request(&completions, &auth, &catalog, &config, &client, &wire, &body).await.expect("dispatch");
                let terminal = tokio::time::timeout(Duration::from_secs(20), async {
                    loop {
                        let msg = output.recv().await.expect("output");
                        let body = match msg.body { Body::Event(body) => body, _ => panic!("event") };
                        assert_eq!(body["request_id"], "reused-request");
                        if matches!(body.get("event").and_then(Value::as_str), Some("error" | "completed")) { break body; }
                    }
                }).await.expect("terminal");
                assert_eq!(terminal["event"], expected);
                if expected == "error" { assert_eq!(terminal["error_category"], "output_admission"); assert_eq!(terminal["message"], ADMISSION_FAILURE); }
                let probe = tokio::time::timeout(Duration::from_secs(2), async {
                    loop { if let Ok(run) = completions.begin("reused-request".into()).await { break run; } tokio::task::yield_now().await; }
                }).await.expect("registry cleanup");
                completions.finish("reused-request", &probe).await;
                assert!(output.try_recv().is_err(), "one terminal, no later success");
            }
        }).await;
        tasks.shutdown().await.expect("shutdown");
        server.await.expect("server");
    }

    #[tokio::test]
    async fn oversized_chat_result_errors_once_and_same_chat_is_reusable() {
        let (base_url, server) = large_response_server().await;
        let mut config = cfg("fixture"); config.base_url = base_url;
        let chats = fresh_chats("test-model");
        let chat_id = ChatId::new("same-chat");
        chats.ensure(chat_id.clone()).await.expect("create");
        chats.push_user(&chat_id, "answer".into()).await.expect("input");
        let auth = Arc::new(AuthStore::from_env_key(None));
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let client = reqwest::Client::new();
        let (wire, mut output) = mpsc::channel(32);
        let tasks = Arc::new(publisher::Tasks::default());
        publisher::TASKS.scope(tasks.clone(), async {
            for expected in ["fixture.chat.error", "fixture.chat.complete.result"] {
                start_completion_turn(&chats, &auth, &catalog, &broker, &config, &client, &wire, chat_id.clone(), None, Vec::new(), Some(serde_json::json!({"type":"json_object"}))).await.expect("start");
                let terminal = tokio::time::timeout(Duration::from_secs(20), async {
                    loop {
                        let body = match output.recv().await.expect("output").body { Body::Event(body) => body, _ => panic!("event") };
                        if matches!(body.get("kind").and_then(Value::as_str), Some("fixture.chat.error" | "fixture.chat.complete.result")) { break body; }
                    }
                }).await.expect("terminal");
                assert_eq!(terminal["kind"], expected);
                assert_eq!(terminal["chat_id"], "same-chat");
                if expected.ends_with("error") { assert_eq!(terminal["error_category"], "output_admission"); assert_eq!(terminal["message"], ADMISSION_FAILURE); }
                let probe = tokio::time::timeout(Duration::from_secs(2), async {
                    loop { if let Ok(token) = chats.begin_turn(&chat_id).await { break token; } tokio::task::yield_now().await; }
                }).await.expect("busy slot cleanup");
                chats.finish_turn(&chat_id, &probe).await;
                assert!(output.try_recv().is_err(), "no later terminal after rejected result");
            }
        }).await;
        tasks.shutdown().await.expect("shutdown");
        server.await.expect("server");
    }

    #[tokio::test]
    async fn supervisor_shutdown_drops_network_and_releases_completion_and_chat_slots() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let mut config = cfg("fixture"); config.base_url = format!("http://{}", listener.local_addr().expect("addr"));
        let chats = fresh_chats("test-model");
        let chat_id = ChatId::new("shutdown-chat");
        chats.ensure(chat_id.clone()).await.expect("create");
        chats.push_user(&chat_id, "answer".into()).await.expect("input");
        let completions = Arc::new(CompletionRuns::new());
        let auth = Arc::new(AuthStore::from_env_key(None));
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let client = reqwest::Client::new();
        let (wire, mut output) = mpsc::channel(32);
        let tasks = Arc::new(publisher::Tasks::default());
        publisher::TASKS.scope(tasks.clone(), async {
            start_completion_turn(&chats, &auth, &catalog, &broker, &config, &client, &wire, chat_id.clone(), None, Vec::new(), None).await.expect("start");
            dispatch_completion_request(&completions, &auth, &catalog, &config, &client, &wire, &make_event_body("fixture.completion.request", &[("request_id", Value::String("shutdown-request".into())), ("messages", serde_json::json!([{"role":"user", "content":"answer"}]))])).await.expect("completion");
        }).await;
        let (mut first, _) = listener.accept().await.expect("first HTTP request");
        let (mut second, _) = listener.accept().await.expect("second HTTP request");
        read_local_request(&mut first).await; read_local_request(&mut second).await;
        tokio::time::timeout(Duration::from_secs(2), tasks.shutdown()).await.expect("bounded shutdown").expect("joined");
        assert!(chats.begin_turn(&chat_id).await.is_ok());
        assert!(completions.begin("shutdown-request".into()).await.is_ok());
        assert!(output.try_recv().is_err(), "teardown must not invent terminal");
        use tokio::io::AsyncReadExt;
        let mut byte = [0;1];
        assert_eq!(tokio::time::timeout(Duration::from_secs(2), first.read(&mut byte)).await.expect("network closed").expect("read"), 0);
        assert_eq!(tokio::time::timeout(Duration::from_secs(2), second.read(&mut byte)).await.expect("network closed").expect("read"), 0);
    }

    #[tokio::test]
    async fn giant_stream_reasoning_and_tool_call_reject_the_invocation_without_success() {
        for mode in ["reasoning", "tool"] {
            let (base_url, server) = large_response_server_for(mode).await;
            let mut config = cfg("fixture"); config.base_url = base_url;
            let completions = Arc::new(CompletionRuns::new());
            let auth = Arc::new(AuthStore::from_env_key(None));
            let catalog = Arc::new(ToolCatalog::new());
            let (wire, mut output) = mpsc::channel(64);
            let client = reqwest::Client::new();
            let tasks = Arc::new(publisher::Tasks::default());
            publisher::TASKS.scope(tasks.clone(), async {
                for expected in ["error", "completed"] {
                    let body = make_event_body("fixture.completion.request", &[("request_id", Value::String(mode.into())), ("messages", serde_json::json!([{"role":"user","content":"answer"}]))]);
                    dispatch_completion_request(&completions, &auth, &catalog, &config, &client, &wire, &body).await.expect("dispatch");
                    let terminal = tokio::time::timeout(Duration::from_secs(20), async {
                        loop {
                            let body = match output.recv().await.expect("output").body { Body::Event(body) => body, _ => panic!("event") };
                            if matches!(body.get("event").and_then(Value::as_str), Some("completed" | "error")) { break body; }
                        }
                    }).await.expect("terminal");
                    assert_eq!(terminal["event"], expected);
                    if expected == "error" { assert_eq!(terminal["error_category"], "output_admission"); }
                    let probe = tokio::time::timeout(Duration::from_secs(2), async {
                        loop { if let Ok(run) = completions.begin(mode.into()).await { break run; } tokio::task::yield_now().await; }
                    }).await.expect("released");
                    completions.finish(mode, &probe).await;
                    assert!(output.try_recv().is_err());
                }
            }).await;
            tasks.shutdown().await.expect("joined");
            server.await.expect("server");
        }
    }

    #[tokio::test]
    async fn synchronous_stream_callback_full_cancels_network_without_cancelling_request_identity() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let mut config = cfg("fixture"); config.base_url = format!("http://{}", listener.local_addr().expect("addr"));
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            read_local_request(&mut stream).await;
            let body = "data: {\"choices\":[{\"delta\":{\"content\":\"one\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"two\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"three\"}}]}\n\n";
            // Leave the response unfinished so cancellation must close the socket.
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 999999\r\n\r\n{body}").as_bytes()).await.expect("stream");
            let mut byte = [0;1];
            assert_eq!(tokio::time::timeout(Duration::from_secs(2), stream.read(&mut byte)).await.expect("cancelled socket").expect("read"), 0);
        });
        let parent = CancellationToken::new();
        let (publisher, input) = Publisher::for_invocation(1, &parent, &parent);
        let (wire, mut output) = mpsc::channel(8);
        let client = reqwest::Client::new();
        let result = tokio::time::timeout(Duration::from_secs(2), publisher.run(&wire, input, &CancellationToken::new(), async {
            let _ = run_completion_attempt(&client, &config, None, "test-model", &[Message::user("answer")], None, None, None, None, publisher.cancellation_token(), &publisher, "req").await;
            let _ = publisher.send(PluginOutgoing::event(completion_event_body("fixture.", "req", "completed", []))).await;
        })).await.expect("publisher termination");
        assert!(matches!(result, Err(publisher::OutputFailure::Full)));
        assert!(publisher.cancellation_token().is_cancelled());
        assert!(!parent.is_cancelled(), "backpressure is not caller hard-cancel");
        assert!(output.try_recv().is_err(), "no progress or success after queue rejection");
        server.await.expect("server");
    }

    #[tokio::test]
    async fn unavailable_correlated_fallback_reaches_supervisor_after_registry_cleanup() {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let mut config = cfg("fixture"); config.base_url = format!("http://{}", listener.local_addr().expect("addr"));
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            read_local_request(&mut stream).await;
            let body = "data: {\"choices\":[{\"delta\":{\"content\":\"small answer\"}}]}\n\ndata: [DONE]\n\n";
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.expect("response");
        });
        let id = "i".repeat(16 * 1024 * 1024);
        let runs = Arc::new(CompletionRuns::new());
        let auth = Arc::new(AuthStore::from_env_key(None));
        let catalog = Arc::new(ToolCatalog::new());
        let client = reqwest::Client::new();
        let (wire, mut output) = mpsc::channel(8);
        let tasks = Arc::new(publisher::Tasks::default());
        publisher::TASKS.scope(tasks.clone(), async {
            dispatch_completion_request(&runs, &auth, &catalog, &config, &client, &wire, &make_event_body("fixture.completion.request", &[("request_id", Value::String(id.clone())), ("messages", serde_json::json!([{"role":"user","content":"answer"}]))])).await.expect("dispatch");
        }).await;
        assert!(matches!(tokio::time::timeout(Duration::from_secs(10), tasks.monitor()).await.expect("supervisor fault"), Err(LlmError::Transport(TransportError::FrameTooLarge { .. }))));
        tasks.shutdown().await.expect("remaining work joined");
        assert!(runs.begin(id).await.is_ok(), "failed fallback still releases request generation");
        assert!(output.try_recv().is_err(), "no truncated identity or recursive terminal");
        server.await.expect("server");
    }
