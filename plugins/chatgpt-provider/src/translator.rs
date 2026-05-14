//! Translate between nefor's chat history shape (`Vec<Message>`) and
//! the Responses API's `(instructions, input: Vec<ResponseItem>)` pair.
//!
//! The Responses API differs from chat-completions in two ways that
//! matter at the boundary:
//!
//! 1. **System prompt is out-of-band.** Chat-completions sticks the
//!    system message in the `messages` array with `role: "system"`;
//!    Responses puts it on a top-level `instructions: String` field.
//!    Multiple `system` messages in our history get concatenated.
//! 2. **Tool calls and outputs are first-class items**, not interleaved
//!    inside an assistant `Message`. An assistant turn that emits N
//!    tool calls becomes N `ResponseItem::FunctionCall` items (plus an
//!    optional preceding `Message` item for interleaved prose).
//!
//! The translator is *forward-only* — we never reconstruct a
//! `Vec<Message>` from a `Vec<ResponseItem>` because the model's output
//! comes back through streaming SSE events, not as a serialized
//! ResponseItem sequence. (The streamed events are decoded into
//! `(text, tool_calls)` and the dispatcher reconstructs `Message`s from
//! that.)

use serde_json::{Map, Value};

use crate::catalog::ToolSpec;
use crate::responses::request::{MessageContent, ResponseItem};
use crate::state::Message;

/// Output of `history_to_input`: the Responses-API `instructions`
/// (concatenated system messages, empty if none) plus the `input`
/// array.
#[derive(Debug, Clone)]
pub struct Translated {
    pub instructions: String,
    pub input: Vec<ResponseItem>,
}

/// Convert a `Vec<Message>` into the Responses-API request shape.
///
/// Rules:
/// - `role == "system"` → concatenated into `instructions` (joined with
///   `"\n\n"`). Never emitted as a ResponseItem.
/// - `role == "user"` → `ResponseItem::Message { role: "user", content:
///   [InputText { text }] }`.
/// - `role == "assistant"` with tool calls → optional `Message`
///   carrying any prose `content`, then one `FunctionCall` per
///   `tool_calls[i]`.
/// - `role == "assistant"` without tool calls → `Message { role:
///   "assistant", content: [OutputText { text }] }`. Empty content is
///   preserved as an empty OutputText to keep the item count stable.
/// - `role == "tool"` → `FunctionCallOutput { call_id, output }`. The
///   call_id is `message.tool_call_id`; output is `message.content`
///   (empty string if absent).
///
/// An optional explicit `system_prompt` (from `chat.create`) is
/// prepended to any inline system messages — both contribute to the
/// final `instructions`.
pub fn history_to_input(history: &[Message], system_prompt: Option<&str>) -> Translated {
    let mut instructions_parts: Vec<String> = Vec::new();
    if let Some(s) = system_prompt {
        if !s.is_empty() {
            instructions_parts.push(s.to_string());
        }
    }

    let mut input: Vec<ResponseItem> = Vec::new();
    for msg in history {
        match msg.role.as_str() {
            "system" => {
                if let Some(text) = &msg.content {
                    if !text.is_empty() {
                        instructions_parts.push(text.clone());
                    }
                }
            }
            "user" => {
                let text = msg.content.clone().unwrap_or_default();
                input.push(ResponseItem::Message {
                    role: "user".into(),
                    content: vec![MessageContent::InputText { text }],
                });
            }
            "assistant" => {
                let text = msg.content.clone().unwrap_or_default();
                if !msg.tool_calls.is_empty() {
                    if !text.is_empty() {
                        input.push(ResponseItem::Message {
                            role: "assistant".into(),
                            content: vec![MessageContent::OutputText { text }],
                        });
                    }
                    for call in &msg.tool_calls {
                        input.push(ResponseItem::FunctionCall {
                            id: None,
                            call_id: call.id.clone(),
                            name: call.function.name.clone(),
                            arguments: call.function.arguments.clone(),
                        });
                    }
                } else {
                    input.push(ResponseItem::Message {
                        role: "assistant".into(),
                        content: vec![MessageContent::OutputText { text }],
                    });
                }
            }
            "tool" => {
                let call_id = msg.tool_call_id.clone().unwrap_or_default();
                let output = msg.content.clone().unwrap_or_default();
                input.push(ResponseItem::FunctionCallOutput { call_id, output });
            }
            _ => {
                // Unknown role: drop quietly. The chat plugin author
                // would have caught it at append time; here we just
                // preserve the rest of the conversation.
                tracing::warn!(role = %msg.role, "translator: dropping message with unknown role");
            }
        }
    }

    Translated {
        instructions: instructions_parts.join("\n\n"),
        input,
    }
}

/// Convert a set of ToolSpec entries into the Responses-API `tools`
/// array shape:
///
/// ```json
/// { "type": "function", "name": "...", "description": "...",
///   "parameters": { ... }, "strict": false }
/// ```
///
/// `strict: false` is the default because we don't (yet) want
/// JSON-schema enforcement to bounce calls that nefor's tool plugins
/// would happily accept. Per-chat allowlist filtering is the caller's
/// job — apply before calling this function.
pub fn tools_to_responses_format(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            let mut obj = Map::new();
            obj.insert("type".into(), Value::String("function".into()));
            obj.insert("name".into(), Value::String(t.name.clone()));
            obj.insert("description".into(), Value::String(t.description.clone()));
            obj.insert("parameters".into(), t.input_schema.clone());
            obj.insert("strict".into(), Value::Bool(false));
            Value::Object(obj)
        })
        .collect()
}

/// True when the model name is one we know supports the
/// `reasoning.encrypted_content` `include` flag and a top-level
/// `reasoning` object. Codex restricts these to GPT-5 family; we mirror
/// that. Heuristic match — `gpt-5*` plus the `o*` reasoning models.
pub fn model_supports_reasoning(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m.starts_with("gpt-5") || m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Message, ToolCall, ToolCallFunction};
    use serde_json::json;

    #[test]
    fn empty_history_yields_empty_translation() {
        let t = history_to_input(&[], None);
        assert!(t.instructions.is_empty());
        assert!(t.input.is_empty());
    }

    #[test]
    fn explicit_system_prompt_lands_in_instructions() {
        let t = history_to_input(&[], Some("be concise"));
        assert_eq!(t.instructions, "be concise");
        assert!(t.input.is_empty());
    }

    #[test]
    fn system_messages_concatenate_with_double_newline() {
        let history = vec![
            Message::system("first"),
            Message::user("hi"),
            Message::system("second"),
        ];
        let t = history_to_input(&history, Some("base"));
        assert_eq!(t.instructions, "base\n\nfirst\n\nsecond");
        // Only the user message lands in input.
        assert_eq!(t.input.len(), 1);
        match &t.input[0] {
            ResponseItem::Message { role, content } => {
                assert_eq!(role, "user");
                match &content[0] {
                    MessageContent::InputText { text } => assert_eq!(text, "hi"),
                    _ => panic!("expected InputText"),
                }
            }
            _ => panic!("expected Message"),
        }
    }

    #[test]
    fn user_message_emits_input_text() {
        let history = vec![Message::user("hello")];
        let t = history_to_input(&history, None);
        assert_eq!(t.input.len(), 1);
        match &t.input[0] {
            ResponseItem::Message { role, content } => {
                assert_eq!(role, "user");
                assert_eq!(content.len(), 1);
                assert!(matches!(
                    &content[0],
                    MessageContent::InputText { text } if text == "hello"
                ));
            }
            _ => panic!("expected Message"),
        }
    }

    #[test]
    fn assistant_text_only_emits_output_text() {
        let history = vec![Message::assistant("response")];
        let t = history_to_input(&history, None);
        assert_eq!(t.input.len(), 1);
        match &t.input[0] {
            ResponseItem::Message { role, content } => {
                assert_eq!(role, "assistant");
                assert!(matches!(
                    &content[0],
                    MessageContent::OutputText { text } if text == "response"
                ));
            }
            _ => panic!("expected Message"),
        }
    }

    #[test]
    fn assistant_tool_calls_without_text_emits_only_function_calls() {
        let history = vec![Message::assistant_tool_calls(vec![ToolCall {
            id: "call_1".into(),
            function: ToolCallFunction {
                name: "read_file".into(),
                arguments: r#"{"path":"/x"}"#.into(),
            },
        }])];
        let t = history_to_input(&history, None);
        assert_eq!(t.input.len(), 1);
        match &t.input[0] {
            ResponseItem::FunctionCall {
                call_id,
                name,
                arguments,
                ..
            } => {
                assert_eq!(call_id, "call_1");
                assert_eq!(name, "read_file");
                assert_eq!(arguments, r#"{"path":"/x"}"#);
            }
            _ => panic!("expected FunctionCall"),
        }
    }

    #[test]
    fn assistant_text_plus_tool_calls_emits_message_then_function_calls() {
        let history = vec![Message::assistant_with_tool_calls(
            "thinking...",
            vec![
                ToolCall {
                    id: "call_a".into(),
                    function: ToolCallFunction {
                        name: "read_file".into(),
                        arguments: r#"{"path":"/a"}"#.into(),
                    },
                },
                ToolCall {
                    id: "call_b".into(),
                    function: ToolCallFunction {
                        name: "read_file".into(),
                        arguments: r#"{"path":"/b"}"#.into(),
                    },
                },
            ],
        )];
        let t = history_to_input(&history, None);
        // 1 Message + 2 FunctionCall
        assert_eq!(t.input.len(), 3);
        assert!(matches!(&t.input[0], ResponseItem::Message { .. }));
        assert!(matches!(&t.input[1], ResponseItem::FunctionCall { .. }));
        assert!(matches!(&t.input[2], ResponseItem::FunctionCall { .. }));
    }

    #[test]
    fn tool_message_emits_function_call_output() {
        let history = vec![Message::tool_result("call_1".into(), "file contents")];
        let t = history_to_input(&history, None);
        assert_eq!(t.input.len(), 1);
        match &t.input[0] {
            ResponseItem::FunctionCallOutput { call_id, output } => {
                assert_eq!(call_id, "call_1");
                assert_eq!(output, "file contents");
            }
            _ => panic!("expected FunctionCallOutput"),
        }
    }

    #[test]
    fn full_round_trip_user_assistant_tool_assistant() {
        let history = vec![
            Message::system("be helpful"),
            Message::user("read /etc/hostname"),
            Message::assistant_with_tool_calls(
                "I'll read that.",
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
        let t = history_to_input(&history, None);
        assert_eq!(t.instructions, "be helpful");
        // user msg + assistant msg + function call + function call
        // output + final assistant msg = 5 items
        assert_eq!(t.input.len(), 5);
        match &t.input[0] {
            ResponseItem::Message { role, .. } => assert_eq!(role, "user"),
            _ => panic!("expected user Message"),
        }
        match &t.input[1] {
            ResponseItem::Message { role, .. } => assert_eq!(role, "assistant"),
            _ => panic!("expected assistant Message"),
        }
        assert!(matches!(&t.input[2], ResponseItem::FunctionCall { .. }));
        assert!(matches!(
            &t.input[3],
            ResponseItem::FunctionCallOutput { .. }
        ));
        match &t.input[4] {
            ResponseItem::Message { role, content } => {
                assert_eq!(role, "assistant");
                assert!(matches!(
                    &content[0],
                    MessageContent::OutputText { text } if text == "Your hostname is darwin."
                ));
            }
            _ => panic!("expected assistant Message"),
        }
    }

    #[test]
    fn tools_to_responses_format_emits_expected_shape() {
        let specs = vec![ToolSpec {
            name: "read_file".into(),
            description: "Read a file".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
            }),
        }];
        let out = tools_to_responses_format(&specs);
        assert_eq!(out.len(), 1);
        let t = &out[0];
        assert_eq!(t.get("type").and_then(Value::as_str), Some("function"));
        assert_eq!(t.get("name").and_then(Value::as_str), Some("read_file"));
        assert_eq!(
            t.get("description").and_then(Value::as_str),
            Some("Read a file")
        );
        assert_eq!(
            t.get("parameters"),
            Some(&json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
            }))
        );
        assert_eq!(t.get("strict"), Some(&Value::Bool(false)));
    }

    #[test]
    fn model_supports_reasoning_matches_gpt5_and_o_series() {
        assert!(model_supports_reasoning("gpt-5"));
        assert!(model_supports_reasoning("gpt-5-codex"));
        assert!(model_supports_reasoning("GPT-5"));
        assert!(model_supports_reasoning("o1-preview"));
        assert!(model_supports_reasoning("o3-mini"));
        assert!(!model_supports_reasoning("gpt-4o"));
        assert!(!model_supports_reasoning("gpt-3.5-turbo"));
    }
}
