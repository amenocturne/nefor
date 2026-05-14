//! Round-trip serde coverage for `ResponsesApiRequest` and friends.
//!
//! The Responses server is opinionated about field presence — every
//! assertion here corresponds to a way the wire shape could regress
//! and silently break the streaming endpoint.

use chatgpt_provider::responses::{
    MessageContent, Reasoning, ReasoningEffort, ReasoningSummary, ResponseItem,
    ResponsesApiRequest, TextControls, Verbosity,
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
