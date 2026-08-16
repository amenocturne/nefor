//! Provider-bound accounting for the exact Responses request representation.
//!
//! The backend's completed-response usage is authoritative when available.
//! Between completions we estimate the next request from its fully lowered JSON
//! representation. The estimate deliberately counts each serialized request
//! component once (instructions, input/native items and attachments, tools and
//! schemas, structured output, and reasoning controls) rather than attempting
//! to reassemble those components from provider-neutral history.

use crate::responses::request::{MessageContent, ResponseItem, ResponsesApiRequest};

const RESIZED_IMAGE_BYTES_ESTIMATE: u64 = 7_373;

/// Conservative local approximation used only while no authoritative count
/// exists for the current request. JSON bytes are a better stable proxy than
/// characters because they include provider-owned structure and escaping.
/// Inline base64 image payloads are replaced with Codex's model-visible image
/// cost instead of being counted as text.
pub fn estimate_serialized_tokens(request: &ResponsesApiRequest) -> u64 {
    let Ok(serialized) = serde_json::to_vec(request) else {
        return 0;
    };
    let serialized_bytes = serialized.len() as u64;
    let (payload_bytes, image_count) = request
        .input
        .iter()
        .filter_map(|item| match item {
            ResponseItem::Message { content, .. } => Some(content),
            _ => None,
        })
        .flatten()
        .filter_map(|content| match content {
            MessageContent::InputImage { image_url } => base64_image_payload(image_url),
            _ => None,
        })
        .filter(|payload| !payload.is_empty())
        .fold((0_u64, 0_u64), |(bytes, count), payload| {
            (
                bytes.saturating_add(payload.len() as u64),
                count.saturating_add(1),
            )
        });

    let adjusted_bytes = serialized_bytes
        .saturating_sub(payload_bytes)
        .saturating_add(image_count.saturating_mul(RESIZED_IMAGE_BYTES_ESTIMATE));
    adjusted_bytes.saturating_add(3) / 4
}

fn base64_image_payload(image_url: &str) -> Option<&str> {
    let (metadata, payload) = image_url.split_once(',')?;
    let mut parts = metadata.split(';');
    let media_type = parts.next()?.get(5..)?;
    if !metadata.get(..5)?.eq_ignore_ascii_case("data:")
        || !media_type
            .get(..6)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("image/"))
        || !parts.any(|part| part.eq_ignore_ascii_case("base64"))
    {
        return None;
    }
    Some(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::responses::request::{Reasoning, ReasoningEffort, ReasoningSummary, TextControls};
    use serde_json::json;

    fn request_with_content(content: Vec<MessageContent>) -> ResponsesApiRequest {
        ResponsesApiRequest {
            model: "gpt-test".into(),
            instructions: "instructions".into(),
            input: vec![ResponseItem::Message {
                role: "user".into(),
                content,
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

    fn text_content() -> MessageContent {
        MessageContent::InputText {
            text: "question".into(),
        }
    }

    fn expected_tokens(request: &ResponsesApiRequest, payload_lengths: &[usize]) -> u64 {
        let serialized_bytes = serde_json::to_vec(request).expect("serialize").len() as u64;
        payload_lengths
            .iter()
            .fold(serialized_bytes, |bytes, payload_length| {
                bytes
                    .saturating_sub(*payload_length as u64)
                    .saturating_add(RESIZED_IMAGE_BYTES_ESTIMATE)
            })
            .div_ceil(4)
    }

    #[test]
    fn text_only_request_keeps_the_full_serialized_estimate() {
        let request = request_with_content(vec![text_content()]);
        let serialized_bytes = serde_json::to_vec(&request).expect("serialize").len() as u64;

        assert_eq!(
            estimate_serialized_tokens(&request),
            serialized_bytes.div_ceil(4)
        );
    }

    #[test]
    fn large_resized_image_uses_fixed_model_visible_cost() {
        // Representative of a multi-megabyte image after the 2048-edge preparation path.
        // Normal image detail is fixed-cost, so source dimensions do not enter this estimate.
        let payload = "A".repeat(4 * 1024 * 1024);
        let request = request_with_content(vec![
            text_content(),
            MessageContent::InputImage {
                image_url: format!("data:image/png;base64,{payload}"),
            },
        ]);

        assert_eq!(
            estimate_serialized_tokens(&request),
            expected_tokens(&request, &[payload.len()])
        );
        assert!(estimate_serialized_tokens(&request) < 3_000);
    }

    #[test]
    fn each_inline_image_gets_one_fixed_cost() {
        let landscape_payload = "A".repeat(1_500_000);
        let portrait_payload = "B".repeat(2_000_000);
        let request = request_with_content(vec![
            text_content(),
            MessageContent::InputImage {
                image_url: format!("data:image/jpeg;base64,{landscape_payload}"),
            },
            MessageContent::InputImage {
                image_url: format!("data:image/webp;base64,{portrait_payload}"),
            },
        ]);

        assert_eq!(
            estimate_serialized_tokens(&request),
            expected_tokens(&request, &[landscape_payload.len(), portrait_payload.len()])
        );
    }

    #[test]
    fn mixed_case_data_url_markers_are_adjusted() {
        let payload = "C".repeat(100_000);
        let request = request_with_content(vec![MessageContent::InputImage {
            image_url: format!("DATA:Image/PNG;charset=utf-8;BASE64,{payload}"),
        }]);

        assert_eq!(
            estimate_serialized_tokens(&request),
            expected_tokens(&request, &[payload.len()])
        );
    }

    #[test]
    fn unsupported_image_url_shapes_fall_back_to_raw_serialized_size() {
        for image_url in [
            "https://example.test/image.png",
            "file:///tmp/image.png",
            "data:image/png,AAAA",
            "data:text/plain;base64,AAAA",
            "data:image/png;base64,",
        ] {
            let request = request_with_content(vec![MessageContent::InputImage {
                image_url: image_url.into(),
            }]);
            let serialized_bytes = serde_json::to_vec(&request).expect("serialize").len() as u64;

            assert_eq!(
                estimate_serialized_tokens(&request),
                serialized_bytes.div_ceil(4),
                "unexpected adjustment for {image_url}"
            );
        }
    }

    #[test]
    fn all_other_lowered_components_still_contribute_once() {
        let full = request_with_content(vec![text_content()]);
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
