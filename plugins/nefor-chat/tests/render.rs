//! Golden-style render tests: given a ChatState, assert the exact event
//! sequence. Complements the unit tests in `src/render.rs` by exercising
//! end-to-end emission ordering and shape.
//!
//! These include the source modules directly so we can construct a
//! `ChatState` and call `render_frame` without a published API. The
//! alternative would be to expose an `ncp_events::*` pub module; we opted
//! to keep the plugin's surface crate-internal and test through
//! `#[path]` inclusion.

#[path = "../src/render.rs"]
mod render;
#[path = "../src/state.rs"]
mod state;
#[path = "../src/wrap.rs"]
mod wrap;

use serde_json::Value;

use crate::render::{render_frame, HL_ASSISTANT, HL_INPUT, HL_SYSTEM, HL_USER};
use crate::state::{ChatState, Dims, Role};

fn new_state(cols: u32, rows: u32) -> ChatState {
    let mut s = ChatState::new();
    s.dims = Dims { cols, rows };
    s.tui_ready = true;
    s
}

fn find_line_events(
    events: &[serde_json::Map<String, Value>],
) -> Vec<&serde_json::Map<String, Value>> {
    events
        .iter()
        .filter(|e| e["kind"] == Value::String("nefor-tui.grid.line".into()))
        .collect()
}

#[test]
fn empty_transcript_empty_input_produces_clear_blanks_and_cursor() {
    let s = new_state(10, 3);
    let events = render_frame(&s);
    // Expected sequence:
    //   clear, line(row=0 blank), line(row=1 blank) [NOPE — row 0 only for
    //   transcript; rows-1 is input], input line, cursor_goto, flush.
    // rows = 3 → transcript_rows = 2, input_row = 2.
    // We expect: clear + 2 transcript blank lines + 1 input line +
    // cursor_goto + flush = 6 events.
    assert_eq!(events.len(), 6);
    assert_eq!(
        events[0]["kind"],
        Value::String("nefor-tui.grid.clear".into())
    );
    assert_eq!(events[1]["row"], Value::Number(0u32.into()));
    assert_eq!(events[2]["row"], Value::Number(1u32.into()));
    // Input row.
    assert_eq!(events[3]["row"], Value::Number(2u32.into()));
    assert_eq!(
        events[4]["kind"],
        Value::String("nefor-tui.grid.cursor_goto".into())
    );
    assert_eq!(
        events[5]["kind"],
        Value::String("nefor-tui.grid.flush".into())
    );
}

#[test]
fn user_then_assistant_pair_layout() {
    let mut s = new_state(40, 5);
    s.push_entry(Role::User, "hello".into());
    s.push_entry(Role::Assistant, "hi there".into());
    let events = render_frame(&s);

    let lines = find_line_events(&events);
    // 4 transcript rows + 1 input row = 5 line events.
    assert_eq!(lines.len(), 5);

    // Row 0: "you> hello"
    let cells0 = lines[0]["cells"].as_array().expect("cells array");
    assert_eq!(cells0[0][0], Value::String("you> hello".into()));
    assert_eq!(cells0[0][1], Value::Number(HL_USER.into()));
    // Row 1: "claude> hi there"
    let cells1 = lines[1]["cells"].as_array().expect("cells");
    assert_eq!(cells1[0][0], Value::String("claude> hi there".into()));
    assert_eq!(cells1[0][1], Value::Number(HL_ASSISTANT.into()));
    // Row 2, 3: blank
    assert_eq!(lines[2]["cells"][0][0], Value::String("".into()));
    assert_eq!(lines[3]["cells"][0][0], Value::String("".into()));
    // Row 4 (input): "> " + empty.
    let input_cells = lines[4]["cells"].as_array().expect("cells");
    assert_eq!(input_cells[0][0], Value::String("> ".into()));
    assert_eq!(input_cells[0][1], Value::Number(HL_INPUT.into()));
}

#[test]
fn scrolled_state_shows_older_rows() {
    let mut s = new_state(20, 5);
    // 8 messages, transcript height = 4.
    for i in 0..8 {
        s.push_entry(Role::User, format!("msg {i}"));
    }
    // scroll_offset = 0 → rows 4..8 (msg 4..msg 7) visible.
    let events = render_frame(&s);
    let lines = find_line_events(&events);
    assert_eq!(lines[0]["cells"][0][0], Value::String("you> msg 4".into()));
    assert_eq!(lines[3]["cells"][0][0], Value::String("you> msg 7".into()));

    // Scroll up by 3 → rows 1..5 visible.
    s.scroll_up(3);
    let events = render_frame(&s);
    let lines = find_line_events(&events);
    assert_eq!(lines[0]["cells"][0][0], Value::String("you> msg 1".into()));
    assert_eq!(lines[3]["cells"][0][0], Value::String("you> msg 4".into()));
}

#[test]
fn input_longer_than_cols_truncates_from_left() {
    // cols 10, buffer of 20 chars, cursor at end → "> " + last 8 visible
    // chars. Cursor at col 9.
    let mut s = new_state(10, 3);
    for c in "abcdefghijklmnopqrst".chars() {
        s.input.insert_char(c);
    }
    let events = render_frame(&s);
    let goto = events
        .iter()
        .find(|e| e["kind"] == Value::String("nefor-tui.grid.cursor_goto".into()))
        .expect("cursor_goto");
    assert_eq!(goto["col"], Value::Number(9u32.into()));
    // Input-line text includes "> " prefix + tail.
    let input_line = events
        .iter()
        .find(|e| {
            e["kind"] == Value::String("nefor-tui.grid.line".into())
                && e["row"] == Value::Number(2u32.into())
        })
        .expect("input line");
    let text = input_line["cells"][0][0].as_str().unwrap_or("");
    assert!(text.starts_with("> "));
    // Visible budget = 10 - 2 = 8. Right-anchor makes cursor at end
    // visible; the last visible char should be 't' (index 19).
    assert_eq!(text.chars().last(), Some('t'));
}

#[test]
fn system_entry_is_bracketed_with_system_hl() {
    let mut s = new_state(20, 3);
    s.push_entry(Role::System, "tool: read".into());
    let events = render_frame(&s);
    let lines = find_line_events(&events);
    assert_eq!(
        lines[0]["cells"][0][0],
        Value::String("[tool: read]".into())
    );
    assert_eq!(lines[0]["cells"][0][1], Value::Number(HL_SYSTEM.into()));
}

#[test]
fn padding_run_fills_to_cols() {
    let mut s = new_state(12, 3);
    s.push_entry(Role::User, "hi".into());
    let events = render_frame(&s);
    let lines = find_line_events(&events);
    let cells = lines[0]["cells"].as_array().expect("cells");
    // "you> hi" is 7 chars; remaining 5 = padding run.
    assert_eq!(cells.len(), 2);
    assert_eq!(cells[0][0], Value::String("you> hi".into()));
    assert_eq!(cells[1][0], Value::String(" ".into()));
    assert_eq!(cells[1][2], Value::Number(5u32.into()));
}

#[test]
fn flush_always_last() {
    let s = new_state(8, 4);
    let events = render_frame(&s);
    assert_eq!(
        events.last().expect("non-empty")["kind"],
        Value::String("nefor-tui.grid.flush".into())
    );
}

#[test]
fn every_line_event_targets_grid_1() {
    let s = new_state(20, 4);
    let events = render_frame(&s);
    for e in &events {
        if let Some(g) = e.get("grid") {
            assert_eq!(g, &Value::Number(1u32.into()));
        }
    }
}
