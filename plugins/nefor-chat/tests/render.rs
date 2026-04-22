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

/// Reconstruct row text from per-character cells. Padding runs (3-element
/// `[text, hl, repeat]` entries) are skipped so the result is the visible,
/// unpadded content of the row.
fn row_text(row: &serde_json::Map<String, Value>) -> String {
    let cells = row["cells"].as_array().expect("cells array");
    let mut out = String::new();
    for cell in cells {
        let arr = cell.as_array().expect("cell array");
        if arr.len() >= 3 {
            continue;
        }
        out.push_str(arr[0].as_str().unwrap_or(""));
    }
    out
}

fn row_hl(row: &serde_json::Map<String, Value>) -> u64 {
    let cells = row["cells"].as_array().expect("cells");
    cells[0][1].as_u64().expect("hl_id on first cell")
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
    // rows=5 → 3 transcript + 1 status + 1 input = 5 line events.
    assert_eq!(lines.len(), 5);

    // Row 0: "you> hello"
    assert_eq!(row_text(lines[0]), "you> hello");
    assert_eq!(row_hl(lines[0]), HL_USER as u64);
    // Row 1: "claude> hi there"
    assert_eq!(row_text(lines[1]), "claude> hi there");
    assert_eq!(row_hl(lines[1]), HL_ASSISTANT as u64);
    // Row 2: blank transcript tail.
    assert_eq!(row_text(lines[2]), "");
    // Row 3: status bar — dim, contains the percentage.
    assert!(row_text(lines[3]).contains("100%"));
    assert_eq!(row_hl(lines[3]), 5u64); // HL_STATUS
                                        // Row 4 (input): "> " + empty.
    assert_eq!(row_text(lines[4]), "> ");
    assert_eq!(row_hl(lines[4]), HL_INPUT as u64);
}

#[test]
fn scrolled_state_shows_older_rows() {
    // rows=5 → transcript height = 3 (1 input + 1 status reserved).
    let mut s = new_state(20, 5);
    for i in 0..8 {
        s.push_entry(Role::User, format!("msg {i}"));
    }
    // scroll_offset = 0 → last 3 rows (msg 5..7) visible.
    let events = render_frame(&s);
    let lines = find_line_events(&events);
    assert_eq!(row_text(lines[0]), "you> msg 5");
    assert_eq!(row_text(lines[2]), "you> msg 7");

    // Scroll up by 3 → rows 2..4 visible (msg 2, 3, 4).
    s.scroll_up(3);
    let events = render_frame(&s);
    let lines = find_line_events(&events);
    assert_eq!(row_text(lines[0]), "you> msg 2");
    assert_eq!(row_text(lines[2]), "you> msg 4");
}

#[test]
fn input_longer_than_cols_wraps_to_multiple_rows() {
    // cols 10, buffer of 20 chars. Full input "> abcdefghijklmnopqrst"
    // is 22 display cols → 3 wrapped rows of 10/10/2. With rows=5,
    // max_input_rows = 4 so the whole thing fits.
    let mut s = new_state(10, 5);
    for c in "abcdefghijklmnopqrst".chars() {
        s.input.insert_char(c);
    }
    let events = render_frame(&s);

    // First input row should carry "> " + first 8 buffer chars.
    let row_first = events
        .iter()
        .find(|e| {
            e["kind"] == Value::String("nefor-tui.grid.line".into())
                && e["row"] == Value::Number(2u32.into())
        })
        .expect("first input row");
    assert_eq!(row_text(row_first), "> abcdefgh");

    // Cursor at end: display col 22 → row 2 from top of input (3 rows
    // into the wrapped sequence), col 2. Input starts at row 2
    // (rows - input_height = 5 - 3 = 2), so cursor_row = 2 + 2 = 4.
    let goto = events
        .iter()
        .find(|e| e["kind"] == Value::String("nefor-tui.grid.cursor_goto".into()))
        .expect("cursor_goto");
    assert_eq!(goto["row"], Value::Number(4u32.into()));
    assert_eq!(goto["col"], Value::Number(2u32.into()));
}

#[test]
fn system_entry_is_bracketed_with_system_hl() {
    let mut s = new_state(20, 3);
    s.push_entry(Role::System, "tool: read".into());
    let events = render_frame(&s);
    let lines = find_line_events(&events);
    assert_eq!(row_text(lines[0]), "[tool: read]");
    assert_eq!(row_hl(lines[0]), HL_SYSTEM as u64);
}

#[test]
fn padding_run_fills_to_cols() {
    let mut s = new_state(12, 3);
    s.push_entry(Role::User, "hi".into());
    let events = render_frame(&s);
    let lines = find_line_events(&events);
    let cells = lines[0]["cells"].as_array().expect("cells");
    // "you> hi" is 7 chars → 7 content cells + 1 padding run = 8 entries.
    // The final entry is a repeat-form padding cell of 5 spaces.
    assert_eq!(cells.len(), 8);
    let last = cells.last().expect("padding cell");
    assert_eq!(last[0], Value::String(" ".into()));
    assert_eq!(last[2], Value::Number(5u32.into()));
    // And the reconstructed content line matches.
    assert_eq!(row_text(lines[0]), "you> hi");
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
