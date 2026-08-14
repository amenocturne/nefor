//! Provider-bound accounting for the exact Responses request representation.
//!
//! The backend's completed-response usage is authoritative when available.
//! Between completions we estimate the next request from its fully lowered JSON
//! representation. The estimate deliberately counts each serialized request
//! component once (instructions, input/native items and attachments, tools and
//! schemas, structured output, and reasoning controls) rather than attempting
//! to reassemble those components from provider-neutral history.

use serde::Serialize;

/// Conservative local approximation used only while no authoritative count
/// exists for the current request. JSON bytes are a better stable proxy than
/// characters because they include provider-owned structure and escaping.
pub fn estimate_serialized_tokens<T: Serialize>(request: &T) -> u64 {
    let bytes = serde_json::to_vec(request).map_or(0, |encoded| encoded.len() as u64);
    bytes.saturating_add(3) / 4
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::responses::request::{
        MessageContent, Reasoning, ReasoningEffort, ReasoningSummary, ResponseItem,
        ResponsesApiRequest, TextControls,
    };
    use serde_json::json;

    fn request() -> ResponsesApiRequest {
        ResponsesApiRequest {
            model: "gpt-test".into(),
            instructions: "instructions".into(),
            input: vec![ResponseItem::Message {
                role: "user".into(),
                content: vec![
                    MessageContent::InputText {
                        text: "question".into(),
                    },
                    MessageContent::InputImage {
                        image_url: "data:image/png;base64,AAAA".into(),
                    },
                ],
            }],
            tools: vec![json!({"type":"function","name":"read","parameters":{"type":"object"}})],
            tool_choice: "auto".into(),
            parallel_tool_calls: true,
            reasoning: Some(Reasoning {
                effort: Some(ReasoningEffort::High),
                summary: Some(ReasoningSummary::Concise),
            }),
            store: false,
            stream: true,
            include: vec!["reasoning.encrypted_content".into()],
            service_tier: None,
            prompt_cache_key: Some("thread".into()),
            text: Some(TextControls {
                verbosity: None,
                format: Some(json!({"type":"json_schema","schema":{"type":"object"}})),
            }),
        }
    }

    #[test]
    fn every_lowered_component_contributes_once() {
        let full = request();
        let full_bytes = serde_json::to_vec(&full).expect("serialize").len() as u64;
        assert_eq!(estimate_serialized_tokens(&full), full_bytes.div_ceil(4));

        let mut reduced = full.clone();
        reduced.instructions.clear();
        reduced.input.clear();
        reduced.tools.clear();
        reduced.reasoning = None;
        reduced.include.clear();
        reduced.text = None;
        assert!(estimate_serialized_tokens(&full) > estimate_serialized_tokens(&reduced));
    }
}
