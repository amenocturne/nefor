    use openai_provider::openai::{ToolCall, ToolCallFunction};

    fn cfg(name: &str) -> Config {
        Config {
            retry_policy: openai_provider::config::RetryPolicy::from_json(r#"{"max_attempts":8,"max_rounds":1,"backoff_multiplier":2,"initial_delay_ms":500,"max_delay_ms":30000,"round_cooldown_ms":0,"jitter_bps":0,"response_headers_timeout_ms":120000}"#).expect("fixture policy"),
            provider_name: name.into(),
            base_url: "http://localhost:11434".into(),
            model: Some("qwen2.5-coder:7b".into()),
            api_key: None,
            auth_header: "Authorization".into(),
        }
    }

    fn from_plugin(name: &str) -> PluginName {
        PluginName::new(name).expect("valid plugin name")
    }
    /// Build the harness pieces a unit test needs: an AuthStore, a small
    /// stdout channel, and the matching receiver.
    fn auth_test_rig(
        env_key: Option<&str>,
    ) -> (
        Arc<AuthStore>,
        mpsc::Sender<PluginOutgoing>,
        mpsc::Receiver<PluginOutgoing>,
    ) {
        let auth = Arc::new(AuthStore::from_env_key(env_key.map(|s| s.to_string())));
        let (tx, rx) = mpsc::channel::<PluginOutgoing>(16);
        (auth, tx, rx)
    }

    fn make_event_body(kind: &str, extra: &[(&str, Value)]) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("kind".into(), Value::String(kind.into()));
        for (k, v) in extra {
            m.insert((*k).to_owned(), v.clone());
        }
        m
    }

    /// Drain the writer channel into a vec of bodies (events only).
    async fn drain(rx: &mut mpsc::Receiver<PluginOutgoing>) -> Vec<Map<String, Value>> {
        let mut out = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            // PluginOutgoing serializes through to_line; for tests we can
            // round-trip through JSON to recover the body map.
            let line = msg.to_line();
            let v: Value = serde_json::from_str(&line).expect("plugin outgoing json");
            if v.get("type").and_then(Value::as_str) == Some("event") {
                if let Some(body) = v.get("body").and_then(Value::as_object) {
                    out.push(body.clone());
                }
            }
        }
        out
    }

    fn fresh_chats(default_model: &str) -> Arc<Chats> {
        Arc::new(Chats::with_default_model(Some(default_model.to_owned())))
    }

    async fn run_one_tool_call(
        catalog: &Arc<ToolCatalog>, broker: &Arc<ToolBroker>, wire: &mpsc::Sender<PluginOutgoing>, cancel: &CancellationToken, call: ToolCall,
    ) -> ToolStepOutcome {
        let (publisher, rx) = Publisher::new(256);
        let mut outcome = None;
        publisher.run(wire, rx, &CancellationToken::new(), async {
            outcome = Some(run_one_tool_call_published(catalog, broker, &publisher, cancel, "Default", call).await);
        }).await.expect("tool publication");
        outcome.expect("tool outcome")
    }
