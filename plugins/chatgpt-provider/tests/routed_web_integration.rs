//! Deterministic routed-web dispatcher integration.

use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::thread;
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
use serde_json::{json, Map, Value};
use tokio::sync::mpsc;

fn event_env_from(origin: &str, kind: &str, fields: Map<String, Value>) -> Envelope {
    let mut body = fields;
    body.insert("kind".into(), Value::String(kind.to_owned()));
    let origin = if origin == "engine" {
        PluginName::engine()
    } else {
        PluginName::new(origin).expect("plugin name")
    };
    Envelope::event(origin, Timestamp::now(), body)
}

fn event_env(kind: &str, fields: Map<String, Value>) -> Envelope {
    event_env_from("engine", kind, fields)
}

fn event_body(message: PluginOutgoing) -> Option<Map<String, Value>> {
    match message.body {
        Body::Event(body) => Some(body),
        Body::System(_) => None,
    }
}

async fn wait_for_web_result(
    rx: &mut mpsc::Receiver<PluginOutgoing>,
    id: &str,
) -> Map<String, Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let message = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("web result timeout")
            .expect("output channel");
        let Some(body) = event_body(message) else {
            continue;
        };
        if body.get("kind").and_then(Value::as_str) == Some("chatgpt.web.result")
            && body.get("id").and_then(Value::as_str) == Some(id)
        {
            return body;
        }
    }
}

fn invocation() -> Value {
    json!({
        "provider": "chatgpt",
        "model": "gpt-test",
        "actor_id": "worker.run-tool",
        "capability_id": "r1/capability-1",
        "session_id": "session-1",
        "run_id": "run-1",
        "run_scope": "r1",
        "conversation_id": "conversation-stable",
        "root_conversation_id": "root"
    })
}

fn web_request(id: &str, name: &str, command: Value) -> Envelope {
    event_env(
        "chatgpt.web.request",
        Map::from_iter([
            ("id".into(), Value::String(id.into())),
            ("caller_id".into(), Value::String("r1/capability-1".into())),
            (
                "invoking_from".into(),
                Value::String("worker.run-tool".into()),
            ),
            ("name".into(), Value::String(name.into())),
            ("model".into(), Value::String("gpt-test".into())),
            ("invocation".into(), invocation()),
            ("commands".into(), command),
        ]),
    )
}

fn web_cancel(id: &str) -> Envelope {
    event_env(
        "chatgpt.web.cancel",
        Map::from_iter([
            ("id".into(), Value::String(id.into())),
            ("caller_id".into(), Value::String("r1/capability-1".into())),
            (
                "invoking_from".into(),
                Value::String("worker.run-tool".into()),
            ),
            ("model".into(), Value::String("gpt-test".into())),
            ("invocation".into(), invocation()),
        ]),
    )
}

#[tokio::test]
async fn routed_requests_settle_out_of_order_cancel_once_and_preserve_exact_payloads() {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind server");
    let address = server.server_addr().to_ip().expect("server address");
    let (captured_tx, captured_rx) = std_mpsc::channel::<Value>();
    let server_thread = thread::spawn(move || {
        let mut responders = Vec::new();
        for _ in 0..4 {
            let mut request = server.recv().expect("receive request");
            let mut text = String::new();
            request
                .as_reader()
                .read_to_string(&mut text)
                .expect("request body");
            let body: Value = serde_json::from_str(&text).expect("request JSON");
            captured_tx.send(body.clone()).expect("capture request");
            responders.push(thread::spawn(move || {
                let (delay, response) = if body.pointer("/commands/search_query/0/q")
                    == Some(&Value::String("slow".into()))
                {
                    (
                        Duration::from_millis(250),
                        json!({"encrypted_output":null,"output":"late search"}),
                    )
                } else if body.pointer("/commands/image_query/0/q")
                    == Some(&Value::String("moon".into()))
                {
                    (
                        Duration::from_millis(5),
                        json!({
                            "encrypted_output":"opaque-image-state",
                            "output":"exact image output",
                            "results":[{
                                "type":"image_result",
                                "future":{"x":1},
                                "large_opaque_payload":"Z".repeat(60_000)
                            }]
                        }),
                    )
                } else if body.pointer("/commands/screenshot/0/ref_id")
                    == Some(&Value::String("turn10view0".into()))
                {
                    (
                        Duration::from_millis(5),
                        serde_json::from_str(include_str!(
                            "fixtures/web/screenshot-reference-success.json"
                        ))
                        .expect("successful screenshot fixture"),
                    )
                } else {
                    (
                        Duration::from_millis(5),
                        serde_json::from_str(include_str!(
                            "fixtures/web/screenshot-direct-url-failure.json"
                        ))
                        .expect("semantic screenshot failure fixture"),
                    )
                };
                thread::sleep(delay);
                let _ = request.respond(
                    tiny_http::Response::from_string(response.to_string())
                        .with_status_code(200)
                        .with_header(
                            "content-type: application/json"
                                .parse::<tiny_http::Header>()
                                .expect("content type"),
                        ),
                );
            }));
        }
        for responder in responders {
            responder.join().expect("responder");
        }
    });

    let base_url = format!("http://{address}");
    let args = Arc::new(ServeArgs {
        provider_name: "chatgpt".into(),
        base_url: base_url.clone(),
        stream_retry_timeout_seconds: None,
    });
    let chats = Arc::new(Chats::with_default_model(Some(
        "acknowledged-fallback".into(),
    )));
    let auth_dir = tempfile::tempdir().expect("auth dir");
    let auth = Arc::new(
        AuthStore::load_from_disk(&auth_dir.path().join("auth.json"))
            .await
            .expect("auth store"),
    );
    let _ = auth.apply_auth_set("test-token".into()).await;
    let responses_client = Arc::new(ResponsesClient::with_http(
        reqwest::Client::new(),
        base_url,
        "installation-test".into(),
        "originator-test".into(),
    ));
    let (out_tx, mut out_rx) = mpsc::channel(64);
    let ctx = DispatcherContext::new(
        args,
        chats,
        auth,
        Arc::new(ToolCatalog::new()),
        Arc::new(ToolBroker::new()),
        responses_client,
        out_tx,
    );
    let (in_tx, in_rx) = mpsc::channel::<Result<Envelope, TransportError>>(64);
    let loop_handle = tokio::spawn(run_dispatch_loop(ctx, in_rx));

    in_tx
        .send(Ok(web_request(
            "gate-slow",
            "web_search",
            json!({"search_query":[{"q":"slow"}]}),
        )))
        .await
        .expect("slow request");
    in_tx
        .send(Ok(web_request(
            "gate-image",
            "web_image_search",
            json!({"image_query":[{"q":"moon"}]}),
        )))
        .await
        .expect("image request");
    let mut forged_cancel = web_cancel("gate-slow");
    forged_cancel.from = PluginName::new("direct-attacker").expect("plugin name");
    in_tx.send(Ok(forged_cancel)).await.expect("forged cancel");
    let mut mismatched_cancel = web_cancel("gate-slow");
    let Body::Event(body) = &mut mismatched_cancel.body else {
        unreachable!("event")
    };
    body.get_mut("invocation")
        .and_then(Value::as_object_mut)
        .expect("invocation")
        .insert("actor_id".into(), Value::String("other.run-tool".into()));
    in_tx
        .send(Ok(mismatched_cancel))
        .await
        .expect("mismatched cancel");

    let image = wait_for_web_result(&mut out_rx, "gate-image").await;
    assert_eq!(image["output"]["text"], "exact image output");
    assert_eq!(image["output"]["results"][0]["future"]["x"], 1);
    assert_eq!(
        image["output"]["results"][0]["large_opaque_payload"]
            .as_str()
            .map(str::len),
        Some(60_000),
        "the provider boundary must leave large structured output intact for the generic gate dump path",
    );
    assert_eq!(
        image["provider_state"]["encrypted_output"],
        "opaque-image-state"
    );

    in_tx
        .send(Ok(web_cancel("gate-slow")))
        .await
        .expect("cancel slow");
    let canceled = wait_for_web_result(&mut out_rx, "gate-slow").await;
    assert_eq!(canceled["error"], "web request cancelled");

    in_tx
        .send(Ok(web_request(
            "gate-shot",
            "web_screenshot",
            json!({"screenshot":[{"ref_id":"turn10view0","pageno":0}]}),
        )))
        .await
        .expect("screenshot request");
    let screenshot = wait_for_web_result(&mut out_rx, "gate-shot").await;
    assert_eq!(
        screenshot["output"]["text"],
        " (https://www.iana.org/about/informational-booklet.pdf)\nciteturn12view0 "
    );
    assert_eq!(screenshot["output"]["results"], json!([]));
    assert_eq!(
        screenshot["provider_state"]["encrypted_output"],
        "opaque-screenshot-state"
    );
    assert!(screenshot.get("error").is_none());
    assert!(screenshot["output"].get("type").is_none());

    in_tx
        .send(Ok(web_request(
            "gate-shot-failed",
            "web_screenshot",
            json!({"screenshot":[{"ref_id":"https://www.iana.org/about/informational-booklet.pdf","pageno":0}]}),
        )))
        .await
        .expect("direct URL screenshot request");
    let failed_screenshot = wait_for_web_result(&mut out_rx, "gate-shot-failed").await;
    assert_eq!(
        failed_screenshot["error"],
        "web screenshot failed: provider could not resolve the screenshot call"
    );
    assert!(failed_screenshot["output"]["text"]
        .as_str()
        .is_some_and(|text| text.contains("turn11view0")));
    assert_eq!(
        failed_screenshot["output"]["results"][0]["ref_id"],
        "turn11view0"
    );
    assert!(failed_screenshot["output"]["results"][0]["snippet"]
        .as_str()
        .is_some_and(|snippet| snippet.starts_with("Unable to resolve screenshot call:")));
    assert_eq!(
        failed_screenshot["provider_state"]["encrypted_output"],
        "opaque-screenshot-state"
    );
    assert!(failed_screenshot["output"].get("type").is_none());

    let requests = [
        captured_rx.recv().expect("first request"),
        captured_rx.recv().expect("second request"),
        captured_rx.recv().expect("third request"),
        captured_rx.recv().expect("fourth request"),
    ];
    let stable_ids = requests
        .iter()
        .map(|request| request["id"].as_str().expect("request id"))
        .collect::<Vec<_>>();
    assert!(stable_ids.windows(2).all(|pair| pair[0] == pair[1]));
    assert!(requests.iter().any(|request| {
        request.pointer("/commands/image_query/0/q") == Some(&Value::String("moon".into()))
            && request.get("search_query").is_none()
    }));
    assert!(requests.iter().any(|request| {
        request.pointer("/commands/screenshot/0/pageno") == Some(&Value::Number(0.into()))
    }));

    tokio::time::sleep(Duration::from_millis(300)).await;
    while let Ok(message) = out_rx.try_recv() {
        if let Some(body) = event_body(message) {
            assert_ne!(body.get("id").and_then(Value::as_str), Some("gate-slow"));
        }
    }

    drop(in_tx);
    loop_handle
        .await
        .expect("dispatch task")
        .expect("dispatch loop");
    server_thread.join().expect("server thread");
}

#[tokio::test]
async fn validation_errors_return_without_http() {
    let responses_client = Arc::new(ResponsesClient::with_http(
        reqwest::Client::new(),
        "http://127.0.0.1:1".into(),
        "installation-test".into(),
        "originator-test".into(),
    ));
    let args = Arc::new(ServeArgs {
        provider_name: "chatgpt".into(),
        base_url: "http://127.0.0.1:1".into(),
        stream_retry_timeout_seconds: None,
    });
    let auth_dir = tempfile::tempdir().expect("auth dir");
    let auth = Arc::new(
        AuthStore::load_from_disk(&auth_dir.path().join("auth.json"))
            .await
            .expect("auth store"),
    );
    let (out_tx, mut out_rx) = mpsc::channel(8);
    let ctx = DispatcherContext::new(
        args,
        Arc::new(Chats::with_default_model(None)),
        auth,
        Arc::new(ToolCatalog::new()),
        Arc::new(ToolBroker::new()),
        responses_client,
        out_tx,
    );
    let (in_tx, in_rx) = mpsc::channel::<Result<Envelope, TransportError>>(8);
    let loop_handle = tokio::spawn(run_dispatch_loop(ctx, in_rx));
    in_tx
        .send(Ok(event_env(
            "chatgpt.web.request",
            Map::from_iter([
                ("id".into(), Value::String("gate-bad".into())),
                ("caller_id".into(), Value::String("r1/capability-1".into())),
                (
                    "invoking_from".into(),
                    Value::String("worker.run-tool".into()),
                ),
                ("model".into(), Value::String("gpt-test".into())),
                ("invocation".into(), invocation()),
            ]),
        )))
        .await
        .expect("validation request");
    let result = wait_for_web_result(&mut out_rx, "gate-bad").await;
    assert_eq!(result["error"], "web request missing `commands`");

    let direct = event_env_from(
        "direct-attacker",
        "chatgpt.web.request",
        Map::from_iter([
            ("id".into(), Value::String("gate-origin".into())),
            ("caller_id".into(), Value::String("r1/capability-1".into())),
            (
                "invoking_from".into(),
                Value::String("worker.run-tool".into()),
            ),
            ("name".into(), Value::String("web_search".into())),
            ("model".into(), Value::String("gpt-test".into())),
            ("invocation".into(), invocation()),
            ("commands".into(), json!({"search_query":[{"q":"blocked"}]})),
        ]),
    );
    in_tx.send(Ok(direct)).await.expect("direct request");
    let result = wait_for_web_result(&mut out_rx, "gate-origin").await;
    assert_eq!(result["error"], "web request requires engine origin");

    let mut missing_provenance = web_request(
        "gate-provenance",
        "web_search",
        json!({"search_query":[{"q":"blocked"}]}),
    );
    let Body::Event(body) = &mut missing_provenance.body else {
        unreachable!("event")
    };
    body.remove("invocation");
    in_tx
        .send(Ok(missing_provenance))
        .await
        .expect("missing provenance request");
    let result = wait_for_web_result(&mut out_rx, "gate-provenance").await;
    assert_eq!(
        result["error"],
        "web request requires invocation provenance"
    );

    for (id, field, replacement, expected) in [
        (
            "gate-provider",
            "provider",
            Value::String("other-provider".into()),
            "web invocation provider `other-provider` does not match `chatgpt`",
        ),
        (
            "gate-actor",
            "actor_id",
            Value::String(String::new()),
            "web invocation provenance requires non-empty `actor_id`",
        ),
        (
            "gate-model",
            "model",
            Value::String("other-model".into()),
            "web request model does not match invocation provenance",
        ),
        (
            "gate-capability",
            "capability_id",
            Value::String("other-scope/capability-1".into()),
            "web request caller correlation does not match capability provenance",
        ),
    ] {
        let mut request = web_request(id, "web_search", json!({"search_query":[{"q":"blocked"}]}));
        let Body::Event(body) = &mut request.body else {
            unreachable!("event")
        };
        body.get_mut("invocation")
            .and_then(Value::as_object_mut)
            .expect("invocation")
            .insert(field.into(), replacement);
        in_tx
            .send(Ok(request))
            .await
            .expect("invalid provenance request");
        let result = wait_for_web_result(&mut out_rx, id).await;
        assert_eq!(result["error"], expected);
    }

    let mut mismatched_actor = web_request(
        "gate-invoking-actor",
        "web_search",
        json!({"search_query":[{"q":"blocked"}]}),
    );
    let Body::Event(body) = &mut mismatched_actor.body else {
        unreachable!("event")
    };
    body.insert(
        "invoking_from".into(),
        Value::String("other.run-tool".into()),
    );
    in_tx
        .send(Ok(mismatched_actor))
        .await
        .expect("mismatched invoking actor request");
    let result = wait_for_web_result(&mut out_rx, "gate-invoking-actor").await;
    assert_eq!(
        result["error"],
        "web request invoking actor does not match invocation provenance"
    );

    for (id, name, commands) in [
        (
            "gate-unknown",
            "web_open",
            json!({"open":[{"ref_id":"ref","unknown":true}]}),
        ),
        (
            "gate-empty-query",
            "web_search",
            json!({"search_query":[{"q":""}]}),
        ),
        (
            "gate-empty-domain",
            "web_search",
            json!({"search_query":[{"q":"ok","domains":[""]}]}),
        ),
        (
            "gate-empty-ref",
            "web_open",
            json!({"open":[{"ref_id":""}]}),
        ),
        (
            "gate-empty-pattern",
            "web_find",
            json!({"find":[{"ref_id":"ref","pattern":""}]}),
        ),
    ] {
        in_tx
            .send(Ok(web_request(id, name, commands)))
            .await
            .expect("malformed command request");
        let result = wait_for_web_result(&mut out_rx, id).await;
        assert!(result.get("error").and_then(Value::as_str).is_some());
    }
    drop(in_tx);
    loop_handle
        .await
        .expect("dispatch task")
        .expect("dispatch loop");
}
