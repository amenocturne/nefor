//! SSE parser coverage. Each test feeds a canned wire fragment through
//! the parser and verifies the typed event sequence.
//!
//! No real network calls — fixtures are inline strings so the test
//! binary is hermetic.

use bytes::Bytes;
use chatgpt_provider::responses::{parse_sse_frame, ResponseEvent, ResponseItem, SseBuffer};

fn parse_all(raw: &str) -> Vec<ResponseEvent> {
    let mut buf = SseBuffer::new();
    buf.push(&Bytes::copy_from_slice(raw.as_bytes()));
    let mut out = Vec::new();
    for frame in buf.drain() {
        match parse_sse_frame(&frame) {
            None => {}
            Some(Ok(ev)) => out.push(ev),
            Some(Err(e)) => panic!("parse error: {e}"),
        }
    }
    out
}

#[test]
fn parses_canonical_streaming_sequence() {
    const SAMPLE: &str = "\
data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_x\"}}\n\
\n\
data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello\"}\n\
\n\
data: {\"type\":\"response.output_text.delta\",\"delta\":\" world\"}\n\
\n\
data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_x\",\"usage\":{\"input_tokens\":5}}}\n\
\n\
data: [DONE]\n\
\n";

    let events = parse_all(SAMPLE);
    assert_eq!(events.len(), 4);

    match &events[0] {
        ResponseEvent::Created { response } => {
            assert_eq!(response["id"], "resp_x");
        }
        other => panic!("expected Created, got {other:?}"),
    }

    match &events[1] {
        ResponseEvent::OutputTextDelta { delta, .. } => assert_eq!(delta, "Hello"),
        other => panic!("expected OutputTextDelta, got {other:?}"),
    }

    match &events[2] {
        ResponseEvent::OutputTextDelta { delta, .. } => assert_eq!(delta, " world"),
        other => panic!("expected OutputTextDelta, got {other:?}"),
    }

    match &events[3] {
        ResponseEvent::Completed { response } => {
            assert_eq!(response["id"], "resp_x");
            assert_eq!(response["usage"]["input_tokens"], 5);
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}

#[test]
fn parses_output_item_added_with_message() {
    let raw = r#"
data: {"type":"response.output_item.added","output_index":0,"item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Hi"}]}}

"#;
    let events = parse_all(raw);
    assert_eq!(events.len(), 1);
    match &events[0] {
        ResponseEvent::OutputItemAdded { item, output_index } => {
            assert_eq!(*output_index, Some(0));
            match item {
                ResponseItem::Message { role, content } => {
                    assert_eq!(role, "assistant");
                    assert_eq!(content.len(), 1);
                }
                other => panic!("expected Message item, got {other:?}"),
            }
        }
        other => panic!("expected OutputItemAdded, got {other:?}"),
    }
}

#[test]
fn parses_output_item_done_with_function_call() {
    let raw = r#"
data: {"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","name":"read_file","arguments":"{\"path\":\"/tmp/x\"}","call_id":"call_a"}}

"#;
    let events = parse_all(raw);
    assert_eq!(events.len(), 1);
    match &events[0] {
        ResponseEvent::OutputItemDone { item, .. } => match item {
            ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            } => {
                assert_eq!(name, "read_file");
                assert_eq!(arguments, r#"{"path":"/tmp/x"}"#);
                assert_eq!(call_id, "call_a");
            }
            other => panic!("expected FunctionCall, got {other:?}"),
        },
        other => panic!("expected OutputItemDone, got {other:?}"),
    }
}

#[test]
fn parses_reasoning_summary_delta() {
    let raw = r#"
data: {"type":"response.reasoning_summary_text.delta","delta":"Considering ","summary_index":0,"item_id":"rs_1"}

"#;
    let events = parse_all(raw);
    assert_eq!(events.len(), 1);
    match &events[0] {
        ResponseEvent::ReasoningSummaryDelta {
            delta,
            summary_index,
            item_id,
        } => {
            assert_eq!(delta, "Considering ");
            assert_eq!(*summary_index, Some(0));
            assert_eq!(item_id.as_deref(), Some("rs_1"));
        }
        other => panic!("expected ReasoningSummaryDelta, got {other:?}"),
    }
}

#[test]
fn parses_reasoning_summary_part_added() {
    let raw = r#"
data: {"type":"response.reasoning_summary_part.added","summary_index":0,"item_id":"rs_1"}

"#;
    let events = parse_all(raw);
    assert_eq!(events.len(), 1);
    match &events[0] {
        ResponseEvent::ReasoningSummaryPartAdded {
            summary_index,
            item_id,
        } => {
            assert_eq!(*summary_index, Some(0));
            assert_eq!(item_id.as_deref(), Some("rs_1"));
        }
        other => panic!("expected ReasoningSummaryPartAdded, got {other:?}"),
    }
}

#[test]
fn parses_reasoning_content_delta() {
    let raw = r#"
data: {"type":"response.reasoning_text.delta","delta":"Step 1","content_index":0,"item_id":"rs_1"}

"#;
    let events = parse_all(raw);
    assert_eq!(events.len(), 1);
    match &events[0] {
        ResponseEvent::ReasoningContentDelta {
            delta,
            content_index,
            item_id,
        } => {
            assert_eq!(delta, "Step 1");
            assert_eq!(*content_index, Some(0));
            assert_eq!(item_id.as_deref(), Some("rs_1"));
        }
        other => panic!("expected ReasoningContentDelta, got {other:?}"),
    }
}

#[test]
fn parses_function_call_arguments_delta() {
    let raw = r#"
data: {"type":"response.function_call_arguments.delta","delta":"{\"path\":","item_id":"fc_1"}

"#;
    let events = parse_all(raw);
    assert_eq!(events.len(), 1);
    match &events[0] {
        ResponseEvent::FunctionCallArgumentsDelta { delta, item_id } => {
            assert_eq!(delta, r#"{"path":"#);
            assert_eq!(item_id.as_deref(), Some("fc_1"));
        }
        other => panic!("expected FunctionCallArgumentsDelta, got {other:?}"),
    }
}

#[test]
fn parses_failed_event() {
    let raw = r#"
data: {"type":"response.failed","response":{"id":"resp_x","status":"failed","error":{"code":"rate_limit_exceeded","message":"Too many requests"}}}

"#;
    let events = parse_all(raw);
    assert_eq!(events.len(), 1);
    match &events[0] {
        ResponseEvent::Failed { response } => {
            assert_eq!(response["error"]["code"], "rate_limit_exceeded");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[test]
fn parses_incomplete_event() {
    let raw = r#"
data: {"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}

"#;
    let events = parse_all(raw);
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], ResponseEvent::Incomplete { .. }));
}

#[test]
fn unknown_event_type_decodes_as_other() {
    let raw = r#"
data: {"type":"response.future_event_2027","payload":{"x":1}}

"#;
    let events = parse_all(raw);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0], ResponseEvent::Other);
}

#[test]
fn done_sentinel_is_filtered_out() {
    // [DONE] should leave zero events in the parsed sequence.
    let raw = "data: [DONE]\n\n";
    let events = parse_all(raw);
    assert!(events.is_empty());
}

#[test]
fn buffer_handles_split_frames_across_pushes() {
    let mut buf = SseBuffer::new();
    // First push: half a frame.
    buf.push(&Bytes::from_static(
        b"data: {\"type\":\"response.output_text.delta\",\"delta",
    ));
    assert!(buf.drain().is_empty());

    // Second push: rest of frame plus a complete one.
    buf.push(&Bytes::from_static(
        b"\":\"Hello\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\"}}\n\n",
    ));
    let frames = buf.drain();
    assert_eq!(frames.len(), 2);

    let ev1 = parse_sse_frame(&frames[0]).expect("Some").expect("Ok");
    let ev2 = parse_sse_frame(&frames[1]).expect("Some").expect("Ok");
    match ev1 {
        ResponseEvent::OutputTextDelta { delta, .. } => assert_eq!(delta, "Hello"),
        other => panic!("expected OutputTextDelta, got {other:?}"),
    }
    assert!(matches!(ev2, ResponseEvent::Completed { .. }));
}

#[test]
fn buffer_ignores_non_data_lines() {
    let mut buf = SseBuffer::new();
    buf.push(&Bytes::from_static(
        b": keepalive\nevent: response.created\ndata: {\"type\":\"response.created\",\"response\":{}}\n\n",
    ));
    let frames = buf.drain();
    assert_eq!(frames.len(), 1);
    let ev = parse_sse_frame(&frames[0]).expect("Some").expect("Ok");
    assert!(matches!(ev, ResponseEvent::Created { .. }));
}
