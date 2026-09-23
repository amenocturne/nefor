    #[test]
    fn completion_events_are_namespaced_and_request_correlated() {
        let event = completion_event_body(
            "ollama.",
            "req-42",
            "tool_call",
            [
                ("id", Value::String("call-1".into())),
                ("name", Value::String("read_file".into())),
                ("arguments", serde_json::json!({"path": "x"})),
            ],
        );
        assert_eq!(
            event.get("kind").and_then(Value::as_str),
            Some("ollama.completion.event")
        );
        assert_eq!(
            event.get("request_id").and_then(Value::as_str),
            Some("req-42")
        );
        assert_eq!(
            event.get("event").and_then(Value::as_str),
            Some("tool_call")
        );
        assert_eq!(event.get("id").and_then(Value::as_str), Some("call-1"));
    }

    #[test]
    fn completion_errors_are_terminal_and_request_correlated() {
        let config = cfg("ollama");
        let event = completion_error_body(&config, "req-err", "upstream failed");
        assert_eq!(
            event.get("kind").and_then(Value::as_str),
            Some("ollama.completion.event")
        );
        assert_eq!(
            event.get("request_id").and_then(Value::as_str),
            Some("req-err")
        );
        assert_eq!(event.get("event").and_then(Value::as_str), Some("error"));
        assert_eq!(
            event.get("message").and_then(Value::as_str),
            Some("upstream failed")
        );
    }

    #[test]
    fn direct_completion_prepends_top_level_system_once_before_request_messages() {
        let body = make_event_body(
            "ollama.completion.request",
            &[("system", Value::String("top-level system".into()))],
        );
        let messages = vec![Message::system("message system"), Message::user("question")];

        let request_messages = completion_request_messages(&body, messages);

        assert_eq!(request_messages.len(), 3);
        assert_eq!(request_messages[0].role(), "system");
        assert_eq!(request_messages[0].content(), Some("top-level system"));
        assert_eq!(request_messages[1].content(), Some("message system"));
        assert_eq!(request_messages[2].content(), Some("question"));
        assert_eq!(
            request_messages
                .iter()
                .filter(|message| message.content() == Some("top-level system"))
                .count(),
            1
        );
    }

    #[test]
    fn direct_completion_model_input_accepts_exact_empty_tool_results() {
        assert!(!completion_request_has_model_input(&[
            Message::system("instructions"),
            Message::assistant("prior answer"),
        ]));
        assert!(!completion_request_has_model_input(&[Message::user("  ")]));
        assert!(completion_request_has_model_input(&[Message::user(
            "question"
        )]));
        assert!(completion_request_has_model_input(&[Message::Tool {
            content: String::new(),
            tool_call_id: "call-1".into(),
        }]));
        assert!(completion_request_has_model_input(&[Message::Tool {
            content: " \n\t".into(),
            tool_call_id: "call-2".into(),
        }]));
    }


    #[test]
    fn hello_body_carries_version_provider_model_base_url() {
        let c = cfg("ollama");
        let b = hello_body(&c);
        assert_eq!(b.get("kind").unwrap().as_str(), Some("ollama.hello"));
        assert_eq!(b.get("version").unwrap().as_str(), Some(PLUGIN_VERSION));
        assert_eq!(b.get("provider").unwrap().as_str(), Some("ollama"));
        assert_eq!(b.get("model").unwrap().as_str(), Some("qwen2.5-coder:7b"));
        assert_eq!(
            b.get("base_url").unwrap().as_str(),
            Some("http://localhost:11434")
        );
    }

    #[test]
    fn hello_body_uses_groq_prefix_when_configured() {
        let c = cfg("groq");
        let b = hello_body(&c);
        assert_eq!(b.get("kind").unwrap().as_str(), Some("groq.hello"));
        assert_eq!(b.get("provider").unwrap().as_str(), Some("groq"));
    }

    #[test]
    fn ready_body_kind_only_with_prefix() {
        let b = ready_body(&cfg("ollama"));
        assert_eq!(b.get("kind").unwrap().as_str(), Some("ollama.ready"));
        assert_eq!(b.len(), 1);

        let b2 = ready_body(&cfg("openrouter"));
        assert_eq!(b2.get("kind").unwrap().as_str(), Some("openrouter.ready"));
    }

    #[test]
    fn turn_error_body_has_message_and_prefix() {
        let b = turn_error_body(&cfg("ollama"), "busy");
        assert_eq!(b.get("kind").unwrap().as_str(), Some("ollama.turn.error"));
        assert_eq!(b.get("message").unwrap().as_str(), Some("busy"));

        let b2 = turn_error_body(&cfg("groq"), "boom");
        assert_eq!(b2.get("kind").unwrap().as_str(), Some("groq.turn.error"));
    }

    #[test]
    fn stream_delta_body_carries_text_id_chat_id_and_prefix() {
        let cid = ChatId::new("c1");
        let b = stream_delta_body("ollama.", "turn-1", &cid, "hi");
        assert_eq!(b.get("kind").unwrap().as_str(), Some("ollama.stream.delta"));
        assert_eq!(b.get("id").unwrap().as_str(), Some("turn-1"));
        assert_eq!(b.get("chat_id").unwrap().as_str(), Some("c1"));
        assert_eq!(b.get("text").unwrap().as_str(), Some("hi"));

        let b2 = stream_delta_body("groq.", "turn-1", &cid, "hi");
        assert_eq!(b2.get("kind").unwrap().as_str(), Some("groq.stream.delta"));
    }

    #[test]
    fn stream_end_body_includes_chat_id_model_duration_finish_with_prefix() {
        let cid = ChatId::new("c1");
        let b = stream_end_body(
            &cfg("ollama"),
            "turn-2",
            &cid,
            "Hello.",
            "qwen",
            1200,
            Some("stop"),
        );
        assert_eq!(b.get("kind").unwrap().as_str(), Some("ollama.stream.end"));
        assert_eq!(b.get("chat_id").unwrap().as_str(), Some("c1"));
        assert_eq!(b.get("text").unwrap().as_str(), Some("Hello."));
        assert_eq!(b.get("model").unwrap().as_str(), Some("qwen"));
        assert_eq!(b.get("duration_ms").unwrap().as_u64(), Some(1200));
        assert_eq!(b.get("finish_reason").unwrap().as_str(), Some("stop"));
    }

    #[test]
    fn stream_end_body_omits_finish_reason_when_absent() {
        let cid = ChatId::new("c");
        let b = stream_end_body(&cfg("ollama"), "turn-3", &cid, "", "qwen", 0, None);
        assert!(!b.contains_key("finish_reason"));
    }

    #[test]
    fn session_stats_body_shape_with_prefix_and_chat_id() {
        let stats = ChatStats {
            model: Some("qwen".into()),
            turns_completed: 2,
            cumulative_input_tokens: 100,
            cumulative_output_tokens: 50,
            last_turn_input_tokens: 60,
            last_turn_output_tokens: 25,
            last_turn_context_tokens: 60,
            last_turn_duration_ms: Some(1234),
        };
        let cid = ChatId::new("c1");
        let b = session_stats_body(&cfg("ollama"), &cid, &stats);
        assert_eq!(
            b.get("kind").unwrap().as_str(),
            Some("ollama.session.stats")
        );
        assert_eq!(b.get("chat_id").unwrap().as_str(), Some("c1"));
        assert_eq!(b.get("model").unwrap().as_str(), Some("qwen"));
        assert_eq!(b.get("turns").unwrap().as_u64(), Some(2));
        assert_eq!(
            b.get("cumulative_input_tokens").unwrap().as_u64(),
            Some(100)
        );
        assert_eq!(
            b.get("last_turn_context_tokens").unwrap().as_u64(),
            Some(60)
        );
        assert_eq!(b.get("last_turn_duration_ms").unwrap().as_u64(), Some(1234));
    }

    #[test]
    fn snippet_truncates_long_strings() {
        let long = "a".repeat(500);
        let s = snippet(&long);
        assert!(s.ends_with('…'));
        assert!(s.len() < long.len());
    }

    #[test]
    fn auth_status_body_connected_omits_message() {
        let snap = AuthSnapshot {
            token: Some("tok".into()),
            state: AuthState::Connected,
            source: None,
        };
        let b = auth_status_body(&cfg("ollama"), &snap);
        assert_eq!(b.get("kind").unwrap().as_str(), Some("ollama.auth.status"));
        assert_eq!(b.get("state").unwrap().as_str(), Some("connected"));
        assert!(!b.contains_key("message"));
    }

    #[test]
    fn auth_status_body_login_required_omits_message() {
        let snap = AuthSnapshot {
            token: None,
            state: AuthState::LoginRequired,
            source: None,
        };
        let b = auth_status_body(&cfg("ollama"), &snap);
        assert_eq!(b.get("state").unwrap().as_str(), Some("login_required"));
        assert!(!b.contains_key("message"));
    }

    #[test]
    fn auth_status_body_error_includes_message() {
        let snap = AuthSnapshot {
            token: None,
            state: AuthState::Error("nope".into()),
            source: None,
        };
        let b = auth_status_body(&cfg("groq"), &snap);
        assert_eq!(b.get("kind").unwrap().as_str(), Some("groq.auth.status"));
        assert_eq!(b.get("state").unwrap().as_str(), Some("error"));
        assert_eq!(b.get("message").unwrap().as_str(), Some("nope"));
    }

    #[tokio::test]
    async fn auth_state_starts_connected_when_env_key_present() {
        let auth = AuthStore::from_env_key(Some("envkey".into()));
        let snap = auth.snapshot().await;
        assert_eq!(snap.state, AuthState::Connected);
        let body = auth_status_body(&cfg("ollama"), &snap);
        assert_eq!(body.get("state").unwrap().as_str(), Some("connected"));
    }

    #[tokio::test]
    async fn auth_state_starts_login_required_when_no_env_key() {
        let auth = AuthStore::from_env_key(None);
        let snap = auth.snapshot().await;
        assert_eq!(snap.state, AuthState::LoginRequired);
        let body = auth_status_body(&cfg("ollama"), &snap);
        assert_eq!(body.get("state").unwrap().as_str(), Some("login_required"));
        assert!(!body.contains_key("message"));
    }

    #[tokio::test]
    async fn auth_set_event_updates_token_and_emits_connected_status() {
        let (auth, tx, mut rx) = auth_test_rig(None);
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        let body = make_event_body(
            "ollama.auth.set",
            &[("token", Value::String("new-tok".into()))],
        );
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

        let emitted = drain(&mut rx).await;
        assert_eq!(emitted.len(), 1);
        assert_eq!(
            emitted[0].get("kind").unwrap().as_str(),
            Some("ollama.auth.status")
        );
        assert_eq!(emitted[0].get("state").unwrap().as_str(), Some("connected"));
        assert_eq!(auth.token().await.as_deref(), Some("new-tok"));
    }

    #[tokio::test]
    async fn login_requested_emits_error_status_for_openai_provider() {
        let (auth, tx, mut rx) = auth_test_rig(None);
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        let body = make_event_body("ollama.login_requested", &[]);
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

        let emitted = drain(&mut rx).await;
        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0].get("state").unwrap().as_str(), Some("error"));
        let msg = emitted[0].get("message").unwrap().as_str().unwrap();
        assert!(msg.contains("no built-in login flow"), "message was: {msg}");
    }

    #[tokio::test]
    async fn logout_requested_with_env_token_emits_error_status_no_clear() {
        let (auth, tx, mut rx) = auth_test_rig(Some("envkey"));
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        let body = make_event_body("ollama.logout_requested", &[]);
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

        let emitted = drain(&mut rx).await;
        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0].get("state").unwrap().as_str(), Some("error"));
        let msg = emitted[0].get("message").unwrap().as_str().unwrap();
        assert!(msg.contains("--api-key"), "message was: {msg}");

        // Token must remain.
        assert_eq!(auth.token().await.as_deref(), Some("envkey"));
        let snap = auth.snapshot().await;
        // Stored state still Connected — the error went out on the wire
        // but didn't mutate the in-memory state.
        assert_eq!(snap.state, AuthState::Connected);
    }

    #[tokio::test]
    async fn logout_requested_after_auth_set_clears_token_emits_login_required() {
        let (auth, tx, mut rx) = auth_test_rig(None);
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        // First, auth.set so the token source is AuthSet.
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("nefor-chat"),
            &make_event_body(
                "ollama.auth.set",
                &[("token", Value::String("acquired".into()))],
            ),
        )
        .await
        .expect("dispatch ok");
        // Drain the connected status.
        let _ = drain(&mut rx).await;

        // Now logout.
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("nefor-chat"),
            &make_event_body("ollama.logout_requested", &[]),
        )
        .await
        .expect("dispatch ok");

        let emitted = drain(&mut rx).await;
        assert_eq!(emitted.len(), 1);
        assert_eq!(
            emitted[0].get("state").unwrap().as_str(),
            Some("login_required")
        );
        assert!(auth.token().await.is_none());
    }

    #[tokio::test]
    async fn http_401_response_transitions_to_error_state() {
        // We can't drive a real HTTP request from a unit test cleanly,
        // but mark_auth_error is the same path the dispatcher takes on
        // StreamError::Unauthorized.
        let (auth, _tx, _rx) = auth_test_rig(Some("badkey"));
        let snap = auth.mark_auth_error(HTTP_401_MESSAGE.to_owned()).await;
        assert_eq!(snap.state.wire_str(), "error");
        let body = auth_status_body(&cfg("ollama"), &snap);
        assert_eq!(body.get("state").unwrap().as_str(), Some("error"));
        let msg = body.get("message").unwrap().as_str().unwrap();
        assert!(msg.contains("401"), "message was: {msg}");
    }

    #[tokio::test]
    async fn model_set_updates_default_and_emits_ack() {
        let (auth, tx, mut rx) = auth_test_rig(None);
        let chats = fresh_chats("initial-model");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        let body = make_event_body(
            "ollama.model.set",
            &[("model", Value::String("new-model".into()))],
        );
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

        let emitted = drain(&mut rx).await;
        assert_eq!(emitted.len(), 1);
        assert_eq!(
            emitted[0].get("kind").unwrap().as_str(),
            Some("ollama.model.set_ack")
        );
        assert_eq!(emitted[0].get("model").unwrap().as_str(), Some("new-model"));
        assert_eq!(chats.default_model().await.as_deref(), Some("new-model"));
    }

    #[tokio::test]
    async fn model_set_with_empty_model_is_ignored() {
        let (auth, tx, mut rx) = auth_test_rig(None);
        let chats = fresh_chats("seed");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        let body = make_event_body("ollama.model.set", &[("model", Value::String("".into()))]);
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

        let emitted = drain(&mut rx).await;
        assert!(emitted.is_empty());
        assert_eq!(chats.default_model().await.as_deref(), Some("seed"));
    }

    #[test]
    fn models_listed_body_carries_models_with_prefix() {
        let models = vec![
            ModelInfo {
                id: "a".into(),
                context_window: None,
                reasoning_efforts: Vec::new(),
                default_reasoning_effort: None,
            },
            ModelInfo {
                id: "b".into(),
                context_window: Some(128000),
                reasoning_efforts: vec!["low".into(), "high".into()],
                default_reasoning_effort: Some("low".into()),
            },
        ];
        let b = models_listed_body(&cfg("ollama"), &models);
        assert_eq!(
            b.get("kind").unwrap().as_str(),
            Some("ollama.models.listed")
        );
        let arr = b.get("models").unwrap().as_array().expect("array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0].as_str(), Some("a"));
        assert_eq!(arr[1].as_str(), Some("b"));
        let cw = b.get("context_windows").unwrap().as_object().expect("map");
        assert_eq!(cw.len(), 1);
        assert_eq!(cw.get("b").unwrap().as_u64(), Some(128000));
        let caps = b
            .get("model_capabilities")
            .unwrap()
            .as_object()
            .expect("map");
        let reasoning = caps
            .get("b")
            .and_then(|v| v.get("reasoning"))
            .and_then(Value::as_object)
            .expect("reasoning capabilities");
        assert_eq!(
            reasoning
                .get("levels")
                .and_then(Value::as_array)
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            reasoning.get("default").and_then(Value::as_str),
            Some("low")
        );
    }

    #[test]
    fn model_set_ack_body_carries_model_with_prefix() {
        let b = model_set_ack_body(&cfg("groq"), "llama-3.3");
        assert_eq!(b.get("kind").unwrap().as_str(), Some("groq.model.set_ack"));
        assert_eq!(b.get("model").unwrap().as_str(), Some("llama-3.3"));
    }

    #[tokio::test]
    async fn auth_set_with_empty_token_is_ignored() {
        let (auth, tx, mut rx) = auth_test_rig(None);
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        let body = make_event_body("ollama.auth.set", &[("token", Value::String("".into()))]);
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

        let emitted = drain(&mut rx).await;
        assert!(emitted.is_empty(), "no status should be emitted");
        assert!(auth.token().await.is_none());
    }

    // --- Tool-calling event shapes -----------------------------------

    #[test]
    fn chat_tool_start_body_uses_input_field_per_chat_contract() {
        let args = serde_json::json!({"path": "/tmp/x"});
        let b = chat_tool_start_body("call_1", "read_file", &args);
        assert_eq!(
            b.get("kind").and_then(Value::as_str),
            Some("chat.tool.start")
        );
        assert_eq!(b.get("id").and_then(Value::as_str), Some("call_1"));
        assert_eq!(b.get("name").and_then(Value::as_str), Some("read_file"));
        assert_eq!(b.get("input"), Some(&args));
    }

    #[test]
    fn chat_tool_end_body_carries_output_and_error_bool() {
        let b = chat_tool_end_body("call_1", "result text", false);
        assert_eq!(b.get("kind").and_then(Value::as_str), Some("chat.tool.end"));
        assert_eq!(b.get("id").and_then(Value::as_str), Some("call_1"));
        assert_eq!(b.get("output").and_then(Value::as_str), Some("result text"));
        assert_eq!(b.get("error").and_then(Value::as_bool), Some(false));

        let err_body = chat_tool_end_body("call_2", "boom", true);
        assert_eq!(err_body.get("error").and_then(Value::as_bool), Some(true));
    }

    #[test]
    fn tool_invoke_body_uses_plugin_prefix_routing() {
        let args = serde_json::json!({"path": "/tmp/x"});
        let b = tool_invoke_body("basic-tools", "call_1", "read_file", args.clone());
        assert_eq!(
            b.get("kind").and_then(Value::as_str),
            Some("basic-tools.tool.invoke")
        );
        assert_eq!(b.get("id").and_then(Value::as_str), Some("call_1"));
        assert_eq!(b.get("name").and_then(Value::as_str), Some("read_file"));
        assert_eq!(b.get("args"), Some(&args));
    }

    // --- Catalog wiring through dispatch ----------------------------

    #[tokio::test]
    async fn dispatch_tool_register_populates_catalog_for_sender() {
        let (auth, tx, _rx) = auth_test_rig(None);
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        let body = make_event_body(
            "tool.register",
            &[(
                "tools",
                serde_json::json!([{
                    "name": "read_file",
                    "description": "Read a file.",
                    "parameters": {"type": "object"}
                }]),
            )],
        );
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("basic-tools"),
            &body,
        )
        .await
        .expect("dispatch ok");

        let tools = catalog.to_openai_tools().await;
        assert_eq!(tools.len(), 1);
        assert_eq!(
            catalog.owner_of("read_file").await.as_deref(),
            Some("basic-tools")
        );
    }

    #[tokio::test]
    async fn dispatch_tool_result_delivers_to_pending_invocation() {
        let (auth, tx, _rx) = auth_test_rig(None);
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        // Register a pending invocation, then deliver a tool.result via
        // dispatch_event.
        let rx_pending = broker.register("call_xyz".into()).await;
        let body = make_event_body(
            "tool.result",
            &[
                ("id", Value::String("call_xyz".into())),
                ("output", Value::String("file contents".into())),
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
            &from_plugin("basic-tools"),
            &body,
        )
        .await
        .expect("dispatch ok");

        let result = rx_pending.await.expect("oneshot resolved");
        assert_eq!(result.id, "call_xyz");
        assert_eq!(result.output.as_deref(), Some("file contents"));
        assert!(result.error.is_none());
    }

    #[test]
    fn media_tool_output_becomes_user_visible_error_for_text_model() {
        let output = serde_json::json!({
            "type": "media",
            "media_type": "image/png",
            "filename": "diagram.png",
            "data": "abc"
        });
        let text = tool_output_for_text_model(&output);
        assert_eq!(
            text,
            "ERROR: Cannot read \"diagram.png\" (this model does not support image input). Inform the user."
        );
    }

    #[tokio::test]
    async fn dispatch_tool_result_with_error_string_routes_through() {
        let (auth, tx, _rx) = auth_test_rig(None);
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        let rx_pending = broker.register("call_err".into()).await;
        let body = make_event_body(
            "tool.result",
            &[
                ("id", Value::String("call_err".into())),
                ("error", Value::String("file not found".into())),
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
            &from_plugin("basic-tools"),
            &body,
        )
        .await
        .expect("dispatch ok");

        let result = rx_pending.await.expect("resolved");
        assert!(result.output.is_none());
        assert_eq!(result.error.as_deref(), Some("file not found"));
    }

    #[tokio::test]
    async fn dispatch_tool_result_for_unknown_id_is_silently_dropped() {
        let (auth, tx, _rx) = auth_test_rig(None);
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        let body = make_event_body(
            "tool.result",
            &[
                ("id", Value::String("never-registered".into())),
                ("output", Value::String("x".into())),
            ],
        );
        // Just must not error. There's no caller to address.
        dispatch_event(
            &chats,
            &auth,
            &catalog,
            &broker,
            &config,
            &client,
            &tx,
            &from_plugin("basic-tools"),
            &body,
        )
        .await
        .expect("dispatch ok");
    }

    #[tokio::test]
    async fn run_one_tool_call_emits_error_end_on_unknown_tool() {
        let catalog = Arc::new(ToolCatalog::new()); // empty
        let broker = Arc::new(ToolBroker::new());
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(16);
        let cancel = tokio_util::sync::CancellationToken::new();

        let tc = ToolCall {
            id: "call_1".into(),
            kind: "function".into(),
            function: ToolCallFunction {
                name: "nonexistent".into(),
                arguments: "{}".into(),
            },
        };
        let outcome = run_one_tool_call(&catalog, &broker, &tx, &cancel, tc).await;
        match outcome {
            ToolStepOutcome::Result { id, content } => {
                assert_eq!(id, "call_1");
                assert!(content.contains("nonexistent"));
            }
            ToolStepOutcome::Cancelled { .. } => panic!("unexpected cancel"),
        }
        // chat.tool.start, then chat.tool.end with error=true. No
        // tool.invoke (no owner).
        let mut events = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            let line = msg.to_line();
            let v: Value = serde_json::from_str(&line).expect("json");
            if v.get("type").and_then(Value::as_str) == Some("event") {
                events.push(v.get("body").unwrap().clone());
            }
        }
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].get("kind").and_then(Value::as_str),
            Some("chat.tool.start")
        );
        assert_eq!(
            events[1].get("kind").and_then(Value::as_str),
            Some("chat.tool.end")
        );
        assert_eq!(events[1].get("error").and_then(Value::as_bool), Some(true));
    }

    #[test]
    fn legacy_history_normalizes_malformed_tool_arguments_but_execution_keeps_raw_feedback() {
        let raw = r#"{"path":"x""#;
        let execution_call = ToolCall {
            id: "call-bad".into(),
            kind: "function".into(),
            function: ToolCallFunction {
                name: "read_file".into(),
                arguments: raw.into(),
            },
        };
        assert_eq!(execution_call.function.arguments, raw);
        let request = serde_json::to_value(Message::assistant_tool_calls(vec![execution_call]))
            .expect("serialize provider history");
        let arguments = request["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .expect("arguments string");
        assert_eq!(serde_json::from_str::<Value>(arguments).unwrap(), raw);
    }

    #[tokio::test]
    async fn run_one_tool_call_reports_malformed_arguments_to_model() {
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
            id: "call_bad".into(),
            kind: "function".into(),
            function: ToolCallFunction {
                name: "read_file".into(),
                arguments: "{\"path\":".into(),
            },
        };

        let outcome = run_one_tool_call(&catalog, &broker, &tx, &cancel, tc).await;
        match outcome {
            ToolStepOutcome::Result { id, content } => {
                assert_eq!(id, "call_bad");
                assert!(content.contains("not valid JSON"), "{content}");
                assert!(content.contains("Raw arguments: {\"path\":"), "{content}");
            }
            ToolStepOutcome::Cancelled { .. } => panic!("unexpected cancel"),
        }

        let mut events = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            let line = msg.to_line();
            let v: Value = serde_json::from_str(&line).expect("json");
            if v.get("type").and_then(Value::as_str) == Some("event") {
                events.push(v.get("body").unwrap().clone());
            }
        }
        assert_eq!(events.len(), 2, "start + error end, no tool.invoke");
        assert_eq!(
            events[0].get("kind").and_then(Value::as_str),
            Some("chat.tool.start")
        );
        assert_eq!(
            events[1].get("kind").and_then(Value::as_str),
            Some("chat.tool.end")
        );
        assert_eq!(events[1].get("error").and_then(Value::as_bool), Some(true));
        assert!(events[1]
            .get("output")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .contains("not valid JSON"));
    }

    // --- New explicit chat.* API -------------------------------------

    #[tokio::test]
    async fn chat_create_emits_chat_created_then_state_holds() {
        let (auth, tx, mut rx) = auth_test_rig(None);
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        let body = make_event_body(
            "ollama.chat.create",
            &[
                ("chat_id", Value::String("c-1".into())),
                ("system", Value::String("durable instructions".into())),
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
            &body,
        )
        .await
        .expect("dispatch ok");

        let emitted = drain(&mut rx).await;
        assert_eq!(emitted.len(), 1);
        assert_eq!(
            emitted[0].get("kind").and_then(Value::as_str),
            Some("ollama.chat.created")
        );
        assert_eq!(
            emitted[0].get("chat_id").and_then(Value::as_str),
            Some("c-1")
        );
        assert!(chats.exists(&ChatId::new("c-1")).await);
        let request_history = chats
            .request_history_snapshot(&ChatId::new("c-1"))
            .await
            .expect("request history");
        assert_eq!(request_history.len(), 1);
        assert_eq!(request_history[0].role(), "system");
        assert_eq!(request_history[0].content(), Some("durable instructions"));
    }

    #[tokio::test]
    async fn chat_create_duplicate_emits_chat_error() {
        let (auth, tx, mut rx) = auth_test_rig(None);
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        chats
            .create(ChatId::new("c-1"), None, None, None, None, None)
            .await
            .expect("seed");

        let body = make_event_body(
            "ollama.chat.create",
            &[("chat_id", Value::String("c-1".into()))],
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
            &body,
        )
        .await
        .expect("dispatch ok");

        let emitted = drain(&mut rx).await;
        assert_eq!(emitted.len(), 1);
        assert_eq!(
            emitted[0].get("kind").and_then(Value::as_str),
            Some("ollama.chat.error")
        );
        assert_eq!(
            emitted[0].get("chat_id").and_then(Value::as_str),
            Some("c-1")
        );
    }

    #[tokio::test]
    async fn chat_append_appends_message_to_chat_history() {
        let (auth, tx, mut rx) = auth_test_rig(None);
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        chats
            .create(ChatId::new("c-1"), None, None, None, None, None)
            .await
            .expect("seed");

        let msg = serde_json::json!({"role": "user", "content": "hello"});
        let body = make_event_body(
            "ollama.chat.append",
            &[("chat_id", Value::String("c-1".into())), ("message", msg)],
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
            &body,
        )
        .await
        .expect("dispatch ok");

        let emitted = drain(&mut rx).await;
        assert_eq!(emitted.len(), 1);
        assert_eq!(
            emitted[0].get("kind").and_then(Value::as_str),
            Some("ollama.chat.appended")
        );

        let history = chats
            .history_snapshot(&ChatId::new("c-1"))
            .await
            .expect("snap");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].role(), "user");
        assert_eq!(history[0].content(), Some("hello"));
    }

    #[tokio::test]
    async fn chat_append_preserves_empty_and_whitespace_tool_results() {
        let (auth, tx, mut rx) = auth_test_rig(None);
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");
        let chat_id = ChatId::new("empty-tool-results");

        chats
            .create(chat_id.clone(), None, None, None, None, None)
            .await
            .expect("seed");
        let messages = [
            serde_json::json!({
                "role": "assistant",
                "tool_calls": [
                    {
                        "id": "call-empty",
                        "type": "function",
                        "function": {"name": "read_file", "arguments": "{\"path\":\"empty.txt\"}"}
                    },
                    {
                        "id": "call-whitespace",
                        "type": "function",
                        "function": {"name": "read_file", "arguments": "{\"path\":\"whitespace.txt\"}"}
                    }
                ]
            }),
            serde_json::json!({
                "role": "tool",
                "tool_call_id": "call-empty",
                "content": ""
            }),
            serde_json::json!({
                "role": "tool",
                "tool_call_id": "call-whitespace",
                "content": " \n\t"
            }),
        ];

        for message in messages {
            let body = make_event_body(
                "ollama.chat.append",
                &[
                    ("chat_id", Value::String(chat_id.to_string())),
                    ("message", message),
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
                &body,
            )
            .await
            .expect("append");
        }

        let emitted = drain(&mut rx).await;
        assert_eq!(emitted.len(), 3);
        assert!(emitted.iter().all(|body| body["kind"] == "ollama.chat.appended"));
        let history = chats
            .request_history_snapshot(&chat_id)
            .await
            .expect("request history");
        assert_eq!(history[1].content(), Some(""));
        assert_eq!(history[2].content(), Some(" \n\t"));
        assert_eq!(
            serde_json::to_value(&history[1]).expect("serialize empty result"),
            serde_json::json!({
                "role": "tool",
                "content": "",
                "tool_call_id": "call-empty"
            })
        );
        assert_eq!(
            serde_json::to_value(&history[2]).expect("serialize whitespace result"),
            serde_json::json!({
                "role": "tool",
                "content": " \n\t",
                "tool_call_id": "call-whitespace"
            })
        );
    }

    #[tokio::test]
    async fn chat_restore_preserves_whitespace_only_tool_result_for_requests() {
        let (auth, tx, mut rx) = auth_test_rig(None);
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");
        let chat_id = ChatId::new("restored-whitespace-tool-result");
        let restore = make_event_body(
            "ollama.chat.restore",
            &[
                ("chat_id", Value::String(chat_id.to_string())),
                (
                    "history",
                    serde_json::json!([
                        {
                            "role": "assistant",
                            "tool_calls": [{
                                "id": "call-1",
                                "type": "function",
                                "function": {"name": "read_file", "arguments": "{\"path\":\"whitespace.txt\"}"}
                            }]
                        },
                        {"role": "tool", "tool_call_id": "call-1", "content": " \n\t"}
                    ]),
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
            &restore,
        )
        .await
        .expect("restore");

        let emitted = drain(&mut rx).await;
        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0]["kind"], "ollama.chat.appended");
        let history = chats
            .request_history_snapshot(&chat_id)
            .await
            .expect("request history");
        assert_eq!(history[1].content(), Some(" \n\t"));
        assert!(request_history_has_model_input(&history));
        assert_eq!(
            serde_json::to_value(&history[1]).expect("serialize restored result"),
            serde_json::json!({
                "role": "tool",
                "content": " \n\t",
                "tool_call_id": "call-1"
            })
        );
    }

    #[tokio::test]
    async fn chat_restore_uses_the_selected_default_for_reasoning_ownership() {
        let (auth, tx, mut rx) = auth_test_rig(None);
        let chats = fresh_chats("initial-model");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let mut config = cfg("ollama");
        config.model = Some("initial-model".into());
        let client = reqwest::Client::new();
        let sender = from_plugin("reasoner-graph");
        let switch = make_event_body(
            "ollama.model.set",
            &[("model", Value::String("selected-model".into()))],
        );
        dispatch_event(
            &chats, &auth, &catalog, &broker, &config, &client, &tx, &sender, &switch,
        )
        .await
        .expect("switch default model");
        let assistant = |model: &str| serde_json::json!({
            "role":"assistant", "content":model,
            "provider_context":{
                "provider":"ollama", "base_url":config.base_url,
                "model":model, "format":REASONING_CONTEXT_FORMAT,
                "artifact":{"reasoning_content":format!("{model} thought")}
            }
        });
        let restore = make_event_body(
            "ollama.chat.restore",
            &[
                ("chat_id", Value::String("restored".into())),
                ("history", serde_json::json!([
                    {"role":"user", "content":"prior input"},
                    assistant("initial-model"), assistant("selected-model")
                ])),
            ],
        );
        dispatch_event(
            &chats, &auth, &catalog, &broker, &config, &client, &tx, &sender, &restore,
        )
        .await
        .expect("restore");
        let emitted = drain(&mut rx).await;
        assert_eq!(
            emitted.last().expect("restore reply")["kind"],
            "ollama.chat.appended"
        );
        let chat_id = ChatId::new("restored");
        assert_eq!(chats.model(&chat_id).await.expect("model"), "selected-model");
        let history = chats.request_history_snapshot(&chat_id).await.expect("history");
        assert!(
            history[1].reasoning().is_none(),
            "the startup model cannot lend its artifact"
        );
        assert_eq!(
            history[2].reasoning(),
            Some(&ReasoningContinuation::Content {
                reasoning_content: "selected-model thought".into(),
            })
        );
    }

    #[tokio::test]
    async fn chat_append_rejects_malformed_native_tool_arguments() {
        let (auth, tx, mut rx) = auth_test_rig(None);
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");
        let chat_id = ChatId::new("append-malformed");
        chats
            .create(chat_id.clone(), None, None, None, None, None)
            .await
            .expect("seed");
        let raw = r#"{"path":"x""#;
        let body = make_event_body(
            "ollama.chat.append",
            &[
                ("chat_id", Value::String(chat_id.to_string())),
                (
                    "message",
                    serde_json::json!({
                        "role": "assistant",
                        "tool_calls": [{
                            "id": "call-bad", "type": "function",
                            "function": {"name": "read_file", "arguments": raw}
                        }]
                    }),
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
            &body,
        )
        .await
        .expect("dispatch");
        let emitted = drain(&mut rx).await;
        assert_eq!(emitted[0]["kind"], "ollama.chat.error");

        let history = chats.history_snapshot(&chat_id).await.expect("history");
        assert!(history.is_empty(), "invalid assistant call is quarantined");
    }

    #[tokio::test]
    async fn chat_restore_rejects_malformed_native_tool_arguments() {
        let (auth, tx, mut rx) = auth_test_rig(None);
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");
        let raw = r#"{"path":"x""#;
        let body = make_event_body(
            "ollama.chat.restore",
            &[
                ("chat_id", Value::String("restore-malformed".into())),
                (
                    "history",
                    serde_json::json!([{
                        "role": "assistant",
                        "tool_calls": [{
                            "id": "call-bad", "type": "function",
                            "function": {"name": "read_file", "arguments": raw}
                        }]
                    }]),
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
            &body,
        )
        .await
        .expect("dispatch");
        let emitted = drain(&mut rx).await;
        assert_eq!(emitted[0]["kind"], "ollama.chat.error");
        assert!(
            !chats.exists(&ChatId::new("restore-malformed")).await,
            "invalid restored history cannot create a chat"
        );
    }

    #[tokio::test]
    async fn chat_complete_empty_history_emits_chat_error_to_unblock_reasoner() {
        let (auth, tx, mut rx) = auth_test_rig(None);
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        chats
            .create(ChatId::new("c-1"), None, None, None, None, None)
            .await
            .expect("seed");

        let body = make_event_body(
            "ollama.chat.complete",
            &[("chat_id", Value::String("c-1".into()))],
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
            &body,
        )
        .await
        .expect("dispatch ok");

        let emitted = drain(&mut rx).await;
        assert_eq!(emitted.len(), 1);
        assert_eq!(
            emitted[0].get("kind").and_then(Value::as_str),
            Some("ollama.chat.error")
        );
        assert_eq!(
            emitted[0].get("chat_id").and_then(Value::as_str),
            Some("c-1")
        );
        assert_eq!(
            emitted[0].get("message").and_then(Value::as_str),
            Some(
                "openai-provider: chat.complete needs at least one non-empty user message or tool result"
            )
        );
    }

    #[tokio::test]
    async fn chat_append_to_unknown_chat_emits_chat_error() {
        let (auth, tx, mut rx) = auth_test_rig(None);
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        let msg = serde_json::json!({"role": "user", "content": "hello"});
        let body = make_event_body(
            "ollama.chat.append",
            &[("chat_id", Value::String("ghost".into())), ("message", msg)],
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
            &body,
        )
        .await
        .expect("dispatch ok");

        let emitted = drain(&mut rx).await;
        assert_eq!(emitted.len(), 1);
        assert_eq!(
            emitted[0].get("kind").and_then(Value::as_str),
            Some("ollama.chat.error")
        );
    }

    #[tokio::test]
    async fn chat_delete_removes_chat_and_emits_deleted() {
        let (auth, tx, mut rx) = auth_test_rig(None);
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        chats
            .create(ChatId::new("c-1"), None, None, None, None, None)
            .await
            .expect("seed");

        let body = make_event_body(
            "ollama.chat.delete",
            &[("chat_id", Value::String("c-1".into()))],
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
            &body,
        )
        .await
        .expect("dispatch ok");

        let emitted = drain(&mut rx).await;
        assert_eq!(emitted.len(), 1);
        assert_eq!(
            emitted[0].get("kind").and_then(Value::as_str),
            Some("ollama.chat.deleted")
        );
        assert!(!chats.exists(&ChatId::new("c-1")).await);
    }

    #[tokio::test]
    async fn legacy_prompt_creates_default_chat_lazily() {
        // The chat-app flow: nefor-chat sends `<prefix>.prompt`. Before
        // any chat exists, this must seed the per-prefix default chat
        // and append the user's text to it. We can't drive a real HTTP
        // turn from a unit test (no upstream), so we observe the
        // pre-turn state mutations by creating the chat ourselves with
        // a non-default model and asserting the prompt's text landed in
        // the default-chat history. Here we just check that after
        // dispatch the default chat exists.
        let (auth, tx, _rx) = auth_test_rig(Some("envkey"));
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        // Point at a localhost address that won't accept — the spawned
        // turn task hits a request error and exits cleanly. The
        // dispatcher itself still returns Ok, which is what we assert.
        let client = reqwest::Client::builder().build().expect("client");

        let body = make_event_body("ollama.prompt", &[("text", Value::String("hi".into()))]);
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

        let default_id = ChatId::default_for_prefix(&config.event_prefix());
        assert!(chats.exists(&default_id).await);
        let history = chats.history_snapshot(&default_id).await.expect("h");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].role(), "user");
        assert_eq!(history[0].content(), Some("hi"));
    }

    #[tokio::test]
    async fn two_concurrent_chats_have_independent_histories() {
        let chats = fresh_chats("m");
        chats
            .create(ChatId::new("a"), None, None, None, None, None)
            .await
            .expect("a");
        chats
            .create(ChatId::new("b"), None, None, None, None, None)
            .await
            .expect("b");
        chats
            .push_user(&ChatId::new("a"), "alpha".into())
            .await
            .unwrap();
        chats
            .push_user(&ChatId::new("b"), "beta".into())
            .await
            .unwrap();
        let ha = chats.history_snapshot(&ChatId::new("a")).await.unwrap();
        let hb = chats.history_snapshot(&ChatId::new("b")).await.unwrap();
        assert_eq!(ha.len(), 1);
        assert_eq!(hb.len(), 1);
        assert_eq!(ha[0].content(), Some("alpha"));
        assert_eq!(hb[0].content(), Some("beta"));
    }

    // --- combinators.register on startup -----------------------------

    #[test]
    fn register_body_kind_is_combinators_register() {
        let b = register_body();
        assert_eq!(
            b.get("kind").and_then(Value::as_str),
            Some("combinators.register")
        );
    }

    #[test]
    fn register_body_declares_raw_request_and_raw_response_bare() {
        let b = register_body();
        let types = b
            .get("types")
            .and_then(Value::as_array)
            .expect("types array");
        let names: Vec<&str> = types.iter().filter_map(Value::as_str).collect();
        assert!(names.contains(&"RawRequest"));
        assert!(names.contains(&"RawResponse"));
        // Bare names — no dots.
        for n in &names {
            assert!(!n.contains('.'), "type entry `{n}` must be bare");
        }
    }

    #[test]
    fn register_body_carries_two_into_entries_against_generic_provider() {
        let b = register_body();
        let impls = b
            .get("implementations")
            .and_then(Value::as_array)
            .expect("impls array");
        assert_eq!(impls.len(), 2);

        // Entry 0: Into<generic-provider.ProviderIn -> RawRequest>
        let e0 = impls[0].as_object().expect("obj");
        assert_eq!(e0.get("trait").and_then(Value::as_str), Some("Into"));
        assert_eq!(
            e0.get("in").and_then(Value::as_str),
            Some("generic-provider.ProviderIn")
        );
        assert_eq!(e0.get("out").and_then(Value::as_str), Some("RawRequest"));
        assert!(e0
            .get("handler")
            .and_then(Value::as_str)
            .map(|s| !s.is_empty())
            .unwrap_or(false));

        // Entry 1: Into<RawResponse -> generic-provider.ProviderOut>
        let e1 = impls[1].as_object().expect("obj");
        assert_eq!(e1.get("trait").and_then(Value::as_str), Some("Into"));
        assert_eq!(e1.get("in").and_then(Value::as_str), Some("RawResponse"));
        assert_eq!(
            e1.get("out").and_then(Value::as_str),
            Some("generic-provider.ProviderOut")
        );
    }

    #[test]
    fn parse_provider_message_accepts_role_content_object() {
        let v = serde_json::json!({"role": "user", "content": "hi"});
        let parsed = parse_provider_message(Some(&v), "fixture", "https://fixture.invalid", Some("fixture-model")).expect("ok");
        assert_eq!(parsed.message.role(), "user");
        assert_eq!(parsed.message.content(), Some("hi"));
        assert!(parsed.tool_call_failures.is_empty());
    }

    #[test]
    fn parse_provider_message_rejects_empty_user_content() {
        let v = serde_json::json!({"role": "user", "content": ""});
        let err = match parse_provider_message(Some(&v), "fixture", "https://fixture.invalid", Some("fixture-model")) {
            Ok(_) => panic!("empty content should fail"),
            Err(err) => err,
        };
        assert_eq!(err, "user message `content` must be non-empty");
    }

    #[test]
    fn parse_provider_message_preserves_tool_content_including_structured_json() {
        for (content, expected) in [
            (serde_json::json!(""), ""),
            (serde_json::json!(" \n\t"), " \n\t"),
            (serde_json::json!({"lines": []}), "{\"lines\":[]}"),
        ] {
            let value = serde_json::json!({
                "role": "tool",
                "tool_call_id": "call-1",
                "content": content
            });
            let parsed = parse_provider_message(
                Some(&value),
                "fixture",
                "https://fixture.invalid",
                Some("fixture-model"),
            )
            .expect("valid tool message");
            assert_eq!(parsed.message.content(), Some(expected));
        }
    }

    #[test]
    fn parse_provider_message_rejects_missing_or_null_tool_content_and_missing_id() {
        for value in [
            serde_json::json!({"role": "tool", "tool_call_id": "call-1"}),
            serde_json::json!({"role": "tool", "tool_call_id": "call-1", "content": null}),
        ] {
            let err = match parse_provider_message(
                Some(&value),
                "fixture",
                "https://fixture.invalid",
                Some("fixture-model"),
            ) {
                Ok(_) => panic!("missing or null tool content should fail"),
                Err(err) => err,
            };
            assert_eq!(err, "tool message missing `content`");
        }

        let missing_id = serde_json::json!({"role": "tool", "content": ""});
        let err = match parse_provider_message(
            Some(&missing_id),
            "fixture",
            "https://fixture.invalid",
            Some("fixture-model"),
        ) {
            Ok(_) => panic!("missing tool id should fail"),
            Err(err) => err,
        };
        assert_eq!(err, "tool message missing `tool_call_id`");
    }

    #[test]
    fn parse_provider_message_rejects_assistant_without_content_or_tools() {
        let v = serde_json::json!({"role": "assistant", "content": ""});
        let err = match parse_provider_message(Some(&v), "fixture", "https://fixture.invalid", Some("fixture-model")) {
            Ok(_) => panic!("empty assistant should fail"),
            Err(err) => err,
        };
        assert_eq!(
            err,
            "assistant message must have non-empty `content` or `tool_calls`"
        );
    }

    #[test]
    fn parse_provider_message_round_trips_assistant_with_tool_calls() {
        let v = serde_json::json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {"name": "read_file", "arguments": "{\"path\":\"/x\"}"}
            }]
        });
        let parsed = parse_provider_message(Some(&v), "fixture", "https://fixture.invalid", Some("fixture-model")).expect("ok");
        assert_eq!(parsed.message.role(), "assistant");
        assert!(parsed.message.content().is_none());
        assert_eq!(parsed.message.tool_calls().len(), 1);
        assert_eq!(parsed.message.tool_calls()[0].id, "call_1");
        assert!(parsed.tool_call_failures.is_empty());
    }

    #[test]
    fn parse_provider_message_surfaces_malformed_tool_call_with_id() {
        let v = serde_json::json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [
                {
                    "id": "call_good",
                    "type": "function",
                    "function": {"name": "read_file", "arguments": "{\"path\":\"/x\"}"}
                },
                {
                    "id": "call_bad",
                    "garbage": true
                }
            ]
        });
        let parsed = parse_provider_message(Some(&v), "fixture", "https://fixture.invalid", Some("fixture-model")).expect("ok");
        assert_eq!(parsed.message.tool_calls().len(), 1);
        assert_eq!(parsed.message.tool_calls()[0].id, "call_good");
        assert_eq!(parsed.tool_call_failures.len(), 1);
        assert_eq!(parsed.tool_call_failures[0].id.as_deref(), Some("call_bad"));
        assert!(!parsed.tool_call_failures[0].error.is_empty());
    }

    #[test]
    fn parse_provider_message_surfaces_malformed_tool_call_without_id() {
        let v = serde_json::json!({
            "role": "assistant",
            "content": "hi",
            "tool_calls": [{"no_id": true}]
        });
        let parsed = parse_provider_message(Some(&v), "fixture", "https://fixture.invalid", Some("fixture-model")).expect("ok");
        assert!(parsed.message.tool_calls().is_empty());
        assert_eq!(parsed.tool_call_failures.len(), 1);
        assert!(parsed.tool_call_failures[0].id.is_none());
    }

    #[test]
    fn chat_complete_result_omits_unmeasured_usage() {
        let body = chat_complete_result_body(
            &cfg("ollama"),
            &ChatId::new("no-usage"),
            "Done.",
            &[],
            Some("stop"),
            None,
            "qwen",
            "",
            None,
        );
        let output = body["output"].as_object().expect("output");
        assert!(output.get("usage").is_none());
    }

    #[test]
    fn chat_complete_result_body_carries_completion_event_shaped_output() {
        let cid = ChatId::new("c-1");
        let calls = vec![ToolCall {
            id: "call_1".into(),
            kind: "function".into(),
            function: ToolCallFunction {
                name: "read_file".into(),
                arguments: "{\"path\":\"/x\"}".into(),
            },
        }];
        let b = chat_complete_result_body(
            &cfg("ollama"),
            &cid,
            "Done.",
            &calls,
            Some("tool_calls"),
            Some((10, 5)),
            "qwen",
            "",
            None,
        );
        assert_eq!(
            b.get("kind").and_then(Value::as_str),
            Some("ollama.chat.complete.result")
        );
        assert_eq!(b.get("chat_id").and_then(Value::as_str), Some("c-1"));
        let out = b.get("output").and_then(Value::as_object).expect("output");
        assert_eq!(out.get("text").and_then(Value::as_str), Some("Done."));
        // Empty reasoning is dropped from the wire shape (back-compat).
        assert!(out.get("reasoning").is_none());
        assert_eq!(
            out.get("finish_reason").and_then(Value::as_str),
            Some("tool_calls")
        );
        let tcs = out
            .get("tool_calls")
            .and_then(Value::as_array)
            .expect("tool_calls");
        assert_eq!(tcs.len(), 1);
        let entry = tcs[0].as_object().expect("entry");
        assert_eq!(entry.get("id").and_then(Value::as_str), Some("call_1"));
        assert_eq!(entry.get("name").and_then(Value::as_str), Some("read_file"));
        let usage = out.get("usage").and_then(Value::as_object).expect("usage");
        assert_eq!(usage.get("prompt_tokens").and_then(Value::as_u64), Some(10));
        assert_eq!(
            usage.get("completion_tokens").and_then(Value::as_u64),
            Some(5)
        );
        assert_eq!(usage.get("model").and_then(Value::as_str), Some("qwen"));
    }

    /// Per-chat interrupt fix (companion to `ebea3b8`): `<prefix>.interrupt`
    /// with a `chat_id` MUST cancel only that chat's in-flight turn.
    /// Pre-fix the handler called `chats.interrupt_all()` regardless,
    /// so the agent reasoner's per-firing fanout up-converted into a
    /// global cancel that nuked the lead's chat too.
    #[tokio::test]
    async fn interrupt_with_chat_id_targets_only_that_chat() {
        let (auth, tx, _rx) = auth_test_rig(Some("envkey"));
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        let id_a = ChatId::new("chat-1");
        let id_b = ChatId::new("chat-2");
        chats
            .create(id_a.clone(), None, None, None, None, None)
            .await
            .expect("a");
        chats
            .create(id_b.clone(), None, None, None, None, None)
            .await
            .expect("b");
        let tok_a = chats.begin_turn(&id_a).await.expect("begin a");
        let tok_b = chats.begin_turn(&id_b).await.expect("begin b");

        let body = make_event_body(
            "ollama.interrupt",
            &[("chat_id", Value::String("chat-1".into()))],
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
            &body,
        )
        .await
        .expect("dispatch ok");

        assert!(
            tok_a.is_cancelled(),
            "chat-1 cancel token must fire for the targeted interrupt",
        );
        assert!(
            !tok_b.is_cancelled(),
            "chat-2 cancel token MUST NOT fire when interrupt targets chat-1; \
             pre-fix this would be true (interrupt_all up-converted the fanout)",
        );
    }

    /// Backwards-compat: bare `<prefix>.interrupt` (no chat_id) keeps
    /// the original `ef260cd` shape — cancel every in-flight turn. The
    /// chat-side `/cancel` path emits the bare envelope.
    #[tokio::test]
    async fn interrupt_without_chat_id_falls_back_to_interrupt_all() {
        let (auth, tx, _rx) = auth_test_rig(Some("envkey"));
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        let id_a = ChatId::new("chat-1");
        let id_b = ChatId::new("chat-2");
        chats
            .create(id_a.clone(), None, None, None, None, None)
            .await
            .expect("a");
        chats
            .create(id_b.clone(), None, None, None, None, None)
            .await
            .expect("b");
        let tok_a = chats.begin_turn(&id_a).await.expect("begin a");
        let tok_b = chats.begin_turn(&id_b).await.expect("begin b");

        let body = make_event_body("ollama.interrupt", &[]);
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

        assert!(tok_a.is_cancelled(), "bare interrupt cancels chat-1");
        assert!(tok_b.is_cancelled(), "bare interrupt cancels chat-2");
    }

    /// Unknown chat_id is a no-op: neither `interrupt_all` (would punish
    /// every live chat for a misrouted envelope) nor an error (the
    /// firing might already have closed and de-registered).
    #[tokio::test]
    async fn interrupt_with_unknown_chat_id_is_noop() {
        let (auth, tx, _rx) = auth_test_rig(Some("envkey"));
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        let id_a = ChatId::new("chat-1");
        let id_b = ChatId::new("chat-2");
        chats
            .create(id_a.clone(), None, None, None, None, None)
            .await
            .expect("a");
        chats
            .create(id_b.clone(), None, None, None, None, None)
            .await
            .expect("b");
        let tok_a = chats.begin_turn(&id_a).await.expect("begin a");
        let tok_b = chats.begin_turn(&id_b).await.expect("begin b");

        let body = make_event_body(
            "ollama.interrupt",
            &[("chat_id", Value::String("chat-nonexistent".into()))],
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
            &body,
        )
        .await
        .expect("dispatch ok");

        assert!(
            !tok_a.is_cancelled(),
            "unknown chat_id MUST NOT cancel chat-1 (would imply interrupt_all fallback)",
        );
        assert!(
            !tok_b.is_cancelled(),
            "unknown chat_id MUST NOT cancel chat-2",
        );
    }

    /// `<prefix>.chat.cancel { chat_id }` is the hard-cancel receive
    /// side of the kernel's kill flush: it must cancel AND suppress the
    /// named chat's in-flight turn, and leave every other chat alone.
    #[tokio::test]
    async fn chat_cancel_suppresses_only_named_chat() {
        let (auth, tx, _rx) = auth_test_rig(Some("envkey"));
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        let id_a = ChatId::new("chat-1");
        let id_b = ChatId::new("chat-2");
        chats
            .create(id_a.clone(), None, None, None, None, None)
            .await
            .expect("a");
        chats
            .create(id_b.clone(), None, None, None, None, None)
            .await
            .expect("b");
        let tok_a = chats.begin_turn(&id_a).await.expect("begin a");
        let tok_b = chats.begin_turn(&id_b).await.expect("begin b");

        let body = make_event_body(
            "ollama.chat.cancel",
            &[("chat_id", Value::String("chat-1".into()))],
        );
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

        assert!(tok_a.is_cancelled(), "chat.cancel must abort chat-1");
        assert!(
            tok_a.is_suppressed(),
            "chat.cancel must suppress chat-1's terminal result",
        );
        assert!(
            !tok_b.is_cancelled() && !tok_b.is_suppressed(),
            "chat.cancel for chat-1 must not touch chat-2",
        );
    }

    /// Cancel for an unknown request id is a no-op — never an error,
    /// never a fallback to cancel-all. The kill may race the turn's own
    /// completion, so "unknown or already finished" is a normal case.
    #[tokio::test]
    async fn chat_cancel_with_unknown_or_missing_chat_id_is_noop() {
        let (auth, tx, _rx) = auth_test_rig(Some("envkey"));
        let chats = fresh_chats("m");
        let catalog = Arc::new(ToolCatalog::new());
        let broker = Arc::new(ToolBroker::new());
        let config = cfg("ollama");
        let client = reqwest::Client::builder().build().expect("client");

        let id_a = ChatId::new("chat-1");
        chats
            .create(id_a.clone(), None, None, None, None, None)
            .await
            .expect("a");
        let tok_a = chats.begin_turn(&id_a).await.expect("begin a");

        // Unknown chat_id.
        let body = make_event_body(
            "ollama.chat.cancel",
            &[("chat_id", Value::String("ghost".into()))],
        );
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
        .expect("unknown chat_id dispatch ok");

        // Missing chat_id entirely.
        let body = make_event_body("ollama.chat.cancel", &[]);
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
        .expect("missing chat_id dispatch ok");

        assert!(
            !tok_a.is_cancelled() && !tok_a.is_suppressed(),
            "no-op cancels must not touch the live chat",
        );
    }
