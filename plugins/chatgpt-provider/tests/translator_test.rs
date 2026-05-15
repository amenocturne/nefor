//! Translator round-trip: chat-history → Responses-API
//! `(instructions, input)`.
//!
//! Integration-level coverage on top of the unit tests inside
//! `translator.rs`. Hits the public surface via `lib::translator`.

use chatgpt_provider::responses::request::{MessageContent, ResponseItem};
use chatgpt_provider::state::{Message, ToolCall, ToolCallFunction};
use chatgpt_provider::translator::{
    history_to_input, model_supports_reasoning, tools_to_responses_format,
};

#[test]
fn full_round_trip_realistic_history() {
    // user → assistant tool call → tool result → assistant final
    let history = vec![
        Message::user("What's in /etc/hostname?"),
        Message::assistant_with_tool_calls(
            "Let me read that.",
            vec![ToolCall {
                id: "call_1".into(),
                function: ToolCallFunction {
                    name: "read_file".into(),
                    arguments: r#"{"path":"/etc/hostname"}"#.into(),
                },
            }],
        ),
        Message::tool_result("call_1".into(), "darwin"),
        Message::assistant("Your hostname is darwin."),
    ];

    let t = history_to_input(&history, Some("Be concise."));

    assert_eq!(t.instructions, "Be concise.");
    assert_eq!(t.input.len(), 5);

    // user
    match &t.input[0] {
        ResponseItem::Message { role, content } => {
            assert_eq!(role, "user");
            match &content[0] {
                MessageContent::InputText { text } => {
                    assert_eq!(text, "What's in /etc/hostname?")
                }
                _ => panic!("expected InputText"),
            }
        }
        _ => panic!("expected user Message"),
    }
    // assistant prose
    match &t.input[1] {
        ResponseItem::Message { role, content } => {
            assert_eq!(role, "assistant");
            match &content[0] {
                MessageContent::OutputText { text } => assert_eq!(text, "Let me read that."),
                _ => panic!("expected OutputText"),
            }
        }
        _ => panic!("expected assistant Message"),
    }
    // function call
    match &t.input[2] {
        ResponseItem::FunctionCall {
            call_id,
            name,
            arguments,
            ..
        } => {
            assert_eq!(call_id, "call_1");
            assert_eq!(name, "read_file");
            assert_eq!(arguments, r#"{"path":"/etc/hostname"}"#);
        }
        _ => panic!("expected FunctionCall"),
    }
    // function call output
    match &t.input[3] {
        ResponseItem::FunctionCallOutput { call_id, output } => {
            assert_eq!(call_id, "call_1");
            assert_eq!(output, "darwin");
        }
        _ => panic!("expected FunctionCallOutput"),
    }
    // final assistant
    match &t.input[4] {
        ResponseItem::Message { role, .. } => assert_eq!(role, "assistant"),
        _ => panic!("expected assistant Message"),
    }
}

#[test]
fn empty_assistant_with_only_tool_calls_omits_prose_message() {
    let history = vec![Message::assistant_tool_calls(vec![
        ToolCall {
            id: "call_a".into(),
            function: ToolCallFunction {
                name: "do_thing".into(),
                arguments: "{}".into(),
            },
        },
        ToolCall {
            id: "call_b".into(),
            function: ToolCallFunction {
                name: "do_other".into(),
                arguments: "{}".into(),
            },
        },
    ])];
    let t = history_to_input(&history, None);
    // Just the two function calls — no leading Message item.
    assert_eq!(t.input.len(), 2);
    assert!(matches!(&t.input[0], ResponseItem::FunctionCall { .. }));
    assert!(matches!(&t.input[1], ResponseItem::FunctionCall { .. }));
}

#[test]
fn system_messages_concat_into_instructions_in_order() {
    let history = vec![
        Message::user("hi"),
        Message::system("rule one"),
        Message::system("rule two"),
        Message::user("hi again"),
    ];
    let t = history_to_input(&history, Some("base"));
    assert_eq!(t.instructions, "base\n\nrule one\n\nrule two");
    // Only the two user messages — systems are stripped.
    assert_eq!(t.input.len(), 2);
}

#[test]
fn unknown_role_is_dropped() {
    let history = vec![
        Message::user("ok"),
        Message {
            role: "weird".into(),
            content: Some("xx".into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
        },
        Message::assistant("end"),
    ];
    let t = history_to_input(&history, None);
    assert_eq!(t.input.len(), 2);
}

#[test]
fn tools_to_responses_format_round_trip_through_request() {
    use chatgpt_provider::catalog::ToolSpec;
    use chatgpt_provider::responses::request::ResponsesApiRequest;
    use serde_json::json;

    let specs = vec![ToolSpec {
        name: "read_file".into(),
        description: "Read a file".into(),
        input_schema: json!({"type": "object"}),
    }];
    let tools = tools_to_responses_format(&specs);
    let req = ResponsesApiRequest {
        model: "test-model".into(),
        instructions: String::new(),
        input: Vec::new(),
        tools,
        tool_choice: "auto".into(),
        parallel_tool_calls: false,
        reasoning: None,
        store: false,
        stream: true,
        include: Vec::new(),
        service_tier: None,
        prompt_cache_key: None,
        text: None,
    };
    // Confirm the request serializes; the test is about the tools[]
    // wire shape staying in step with the broader request body.
    let json = serde_json::to_value(&req).expect("ser");
    let tools = json.get("tools").and_then(|v| v.as_array()).expect("array");
    assert_eq!(tools.len(), 1);
    assert_eq!(
        tools[0].get("type").and_then(|v| v.as_str()),
        Some("function")
    );
    assert_eq!(
        tools[0].get("name").and_then(|v| v.as_str()),
        Some("read_file")
    );
}

#[test]
fn model_reasoning_capability_check_filters_correctly() {
    // gpt-5 family + o-series → reasoning; others → no.
    assert!(model_supports_reasoning("gpt-5"));
    assert!(model_supports_reasoning("gpt-5.5"));
    assert!(model_supports_reasoning("o3-mini"));
    assert!(!model_supports_reasoning("gpt-4o"));
}
