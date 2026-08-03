//! Round-trip serde coverage for `ResponsesApiRequest` and friends.
//!
//! The Responses server is opinionated about field presence — every
//! assertion here corresponds to a way the wire shape could regress
//! and silently break the streaming endpoint.

use nefor_mag::schema::{SchemaField, SchemaType, TypeSchema, SCHEMA_VERSION};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use chatgpt_provider::auth::store::{AccessToken, RefreshToken, TokenData};
use chatgpt_provider::auth::{AuthSnapshot, AuthState, TokenSource};
use chatgpt_provider::responses::{
    MessageContent, Reasoning, ReasoningEffort, ReasoningSummary, ResponseItem,
    ResponsesApiRequest, ResponsesClient, ResponsesTurnContext, TextControls, Verbosity,
};
use serde_json::json;

fn minimal_request() -> ResponsesApiRequest {
    ResponsesApiRequest {
        model: "gpt-5".into(),
        instructions: String::new(),
        input: vec![ResponseItem::Message {
            role: "user".into(),
            content: vec![MessageContent::InputText { text: "Hi".into() }],
        }],
        tools: vec![],
        tool_choice: "auto".into(),
        parallel_tool_calls: false,
        reasoning: None,
        store: false,
        stream: true,
        include: vec![],
        service_tier: None,
        prompt_cache_key: None,
        text: None,
    }
}

#[test]
fn minimal_request_omits_optional_fields() {
    let req = minimal_request();
    let v = serde_json::to_value(&req).expect("serialize");
    let obj = v.as_object().expect("object");

    // Required fields are always present.
    assert!(obj.contains_key("model"));
    assert!(obj.contains_key("input"));
    assert!(obj.contains_key("tools"));
    assert!(obj.contains_key("tool_choice"));
    assert!(obj.contains_key("parallel_tool_calls"));
    assert!(obj.contains_key("store"));
    assert!(obj.contains_key("stream"));
    assert!(obj.contains_key("include"));

    // Optional fields are skipped when None/empty.
    assert!(!obj.contains_key("instructions"));
    assert!(!obj.contains_key("reasoning"));
    assert!(!obj.contains_key("service_tier"));
    assert!(!obj.contains_key("prompt_cache_key"));
    assert!(!obj.contains_key("text"));
}

#[test]
fn instructions_serialized_when_non_empty() {
    let mut req = minimal_request();
    req.instructions = "You are helpful.".into();
    let v = serde_json::to_value(&req).expect("serialize");
    assert_eq!(v["instructions"], json!("You are helpful."));
}

#[test]
fn reasoning_request_serializes_effort_and_summary() {
    let mut req = minimal_request();
    req.reasoning = Some(Reasoning {
        effort: Some(ReasoningEffort::Medium),
        summary: Some(ReasoningSummary::Auto),
    });
    req.include = vec!["reasoning.encrypted_content".into()];

    let v = serde_json::to_value(&req).expect("serialize");
    assert_eq!(v["reasoning"]["effort"], json!("medium"));
    assert_eq!(v["reasoning"]["summary"], json!("auto"));
    assert_eq!(v["include"], json!(["reasoning.encrypted_content"]));
}

#[test]
fn reasoning_with_only_effort_skips_summary_field() {
    let r = Reasoning {
        effort: Some(ReasoningEffort::High),
        summary: None,
    };
    let v = serde_json::to_value(&r).expect("serialize");
    let obj = v.as_object().expect("object");
    assert!(obj.contains_key("effort"));
    assert!(!obj.contains_key("summary"));
}

#[test]
fn reasoning_effort_xhigh_serializes_as_xhigh() {
    let r = Reasoning {
        effort: Some(ReasoningEffort::XHigh),
        summary: None,
    };
    let v = serde_json::to_value(&r).expect("serialize");
    assert_eq!(v["effort"], json!("xhigh"));
}

#[test]
fn text_controls_serializes_verbosity_and_format() {
    let mut req = minimal_request();
    req.text = Some(TextControls {
        verbosity: Some(Verbosity::Low),
        format: Some(json!({
            "type": "json_schema",
            "name": "MyOutput",
            "strict": true,
            "schema": {"type": "object"},
        })),
    });
    let v = serde_json::to_value(&req).expect("serialize");
    assert_eq!(v["text"]["verbosity"], json!("low"));
    assert_eq!(v["text"]["format"]["type"], json!("json_schema"));
}

#[tokio::test]
async fn structured_request_reaches_local_responses_server_with_explicit_object_root() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            let count = stream.read(&mut buffer).await.expect("read request");
            if count == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..count]);
            let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
                continue;
            };
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .and_then(|length| length.parse::<usize>().ok())
                })
                .expect("content length");
            if bytes.len() >= header_end + 4 + content_length {
                let request: serde_json::Value =
                    serde_json::from_slice(&bytes[header_end + 4..header_end + 4 + content_length])
                        .expect("request JSON");
                let schema = &request["text"]["format"]["schema"];
                assert_eq!(
                    schema["type"], "object",
                    "backend rejects a missing root type"
                );
                assert_eq!(schema["required"], json!(["content"]));
                assert_eq!(schema["additionalProperties"], false);
                break;
            }
        }
        let body = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"{\\\"content\\\":\\\"done\\\"}\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\ndata: [DONE]\n\n";
        let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
        stream
            .write_all(response.as_bytes())
            .await
            .expect("response");
    });

    let mag_schema = TypeSchema {
        version: SCHEMA_VERSION,
        root: SchemaType::Named {
            name: "nefor.contracts.FinalAnswer".into(),
            body: Box::new(SchemaType::Record {
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
    let mut request = minimal_request();
    request.text = Some(TextControls {
        verbosity: None,
        format: Some(json!({
            "type": "json_schema",
            "name": "mag_output",
            "strict": true,
            "schema": provider_schema.schema
        })),
    });
    let auth = AuthSnapshot {
        tokens: Some(TokenData {
            id_token: "test-id".into(),
            access_token: AccessToken("test-access".into()),
            refresh_token: RefreshToken("test-refresh".into()),
            account_id: None,
        }),
        state: AuthState::Connected,
        source: Some(TokenSource::AuthSet),
    };
    let client = ResponsesClient::new(
        format!("http://{addr}"),
        "test-installation".into(),
        "nefor_test".into(),
    );
    let mut turn = ResponsesTurnContext::new("session", "thread");
    let mut response = client
        .stream(&request, &auth, &mut turn)
        .await
        .expect("stream");
    use futures::StreamExt;
    let mut delta = String::new();
    while let Some(event) = response.next().await {
        if let chatgpt_provider::responses::ResponseEvent::OutputTextDelta { delta: part, .. } =
            event.expect("event")
        {
            delta.push_str(&part);
        }
    }
    server.await.expect("server");
    assert_eq!(delta, r#"{"content":"done"}"#);
    let decoded = mag_schema.validate_provider_json(&delta);
    assert!(decoded.ok, "{:?}", decoded.violations);
    assert_eq!(decoded.value, Some(json!({"content": "done"})));
}

#[test]
fn tools_request_serializes_passthrough_tool_value() {
    let mut req = minimal_request();
    req.tools = vec![json!({
        "type": "function",
        "name": "read_file",
        "description": "Read a file",
        "parameters": {"type": "object", "properties": {"path": {"type": "string"}}},
    })];
    let v = serde_json::to_value(&req).expect("serialize");
    assert_eq!(v["tools"].as_array().expect("array").len(), 1);
    assert_eq!(v["tools"][0]["name"], json!("read_file"));
}

#[test]
fn function_call_item_uses_snake_case_tag() {
    let item = ResponseItem::FunctionCall {
        id: None,
        name: "read_file".into(),
        arguments: r#"{"path":"/tmp/x"}"#.into(),
        call_id: "call_abc".into(),
    };
    let v = serde_json::to_value(&item).expect("serialize");
    assert_eq!(v["type"], json!("function_call"));
    assert_eq!(v["name"], json!("read_file"));
    assert_eq!(v["arguments"], json!(r#"{"path":"/tmp/x"}"#));
    assert_eq!(v["call_id"], json!("call_abc"));
}

#[test]
fn function_call_output_item_uses_snake_case_tag() {
    let item = ResponseItem::FunctionCallOutput {
        call_id: "call_abc".into(),
        output: "result text".into(),
    };
    let v = serde_json::to_value(&item).expect("serialize");
    assert_eq!(v["type"], json!("function_call_output"));
    assert_eq!(v["call_id"], json!("call_abc"));
    assert_eq!(v["output"], json!("result text"));
}

#[test]
fn reasoning_item_round_trips_encrypted_content() {
    let item = ResponseItem::Reasoning {
        id: Some("rs_1".into()),
        encrypted_content: Some("opaque-blob".into()),
        summary: vec![],
    };
    let v = serde_json::to_value(&item).expect("serialize");
    assert_eq!(v["type"], json!("reasoning"));
    assert_eq!(v["encrypted_content"], json!("opaque-blob"));

    let back: ResponseItem = serde_json::from_value(v).expect("deserialize");
    assert_eq!(back, item);
}

#[test]
fn compaction_summary_item_round_trips_unknown_fields() {
    let v = json!({
        "type": "compaction_summary",
        "id": "cs_1",
        "encrypted_content": "opaque-summary",
        "text": "Short user-visible summary.",
        "server_field": {"kept": true}
    });

    let item: ResponseItem = serde_json::from_value(v.clone()).expect("deserialize");
    let ResponseItem::CompactionSummary {
        id,
        encrypted_content,
        text,
        extra,
        ..
    } = &item
    else {
        panic!("expected compaction summary");
    };
    assert_eq!(id.as_deref(), Some("cs_1"));
    assert_eq!(encrypted_content.as_deref(), Some("opaque-summary"));
    assert_eq!(text.as_deref(), Some("Short user-visible summary."));
    assert_eq!(extra["server_field"], json!({"kept": true}));

    let back = serde_json::to_value(&item).expect("serialize");
    assert_eq!(back["type"], json!("compaction_summary"));
    assert_eq!(back["server_field"], json!({"kept": true}));
}

#[test]
fn message_content_input_text_uses_snake_case_tag() {
    let mc = MessageContent::InputText {
        text: "hello".into(),
    };
    let v = serde_json::to_value(&mc).expect("serialize");
    assert_eq!(v["type"], json!("input_text"));
    assert_eq!(v["text"], json!("hello"));
}

#[test]
fn message_content_output_text_uses_snake_case_tag() {
    let mc = MessageContent::OutputText {
        text: "hello".into(),
    };
    let v = serde_json::to_value(&mc).expect("serialize");
    assert_eq!(v["type"], json!("output_text"));
    assert_eq!(v["text"], json!("hello"));
}
