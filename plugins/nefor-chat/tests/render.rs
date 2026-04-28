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

use crate::render::{
    build_status_spans, render_frame, tool_start_line, HL_ASSISTANT, HL_INPUT, HL_MD_BOLD,
    HL_MD_CODE_INLINE, HL_MD_HEADING, HL_MD_ITALIC, HL_STATUS, HL_STATUS_DIM, HL_SYSTEM, HL_USER,
};
use crate::state::{ChatState, Dims, Role, SessionMetadata};

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
    // rows = 3 → row 0 transcript blank · row 1 input · row 2 status. Order
    // emitted: clear · transcript · input · status · cursor · flush = 6.
    assert_eq!(events.len(), 6);
    assert_eq!(
        events[0]["kind"],
        Value::String("nefor-tui.grid.clear".into())
    );
    assert_eq!(events[1]["row"], Value::Number(0u32.into())); // transcript
    assert_eq!(events[2]["row"], Value::Number(1u32.into())); // input
    assert_eq!(events[3]["row"], Value::Number(2u32.into())); // status
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
    let mut s = new_state(40, 8);
    s.push_entry(Role::User, "hello".into());
    s.push_entry(Role::Assistant, "hi there".into());
    let events = render_frame(&s);

    let lines = find_line_events(&events);
    // 6 transcript rows: user block (top bar · "│ hello" · bottom bar) +
    // assistant body "hi there" + 2 blank tails. Then input row + status.
    // Row 0: top bar of user block.
    assert_eq!(row_text(lines[0]), "│");
    assert_eq!(row_hl(lines[0]), HL_USER as u64);
    // Row 1: "│ hello" — bar carries HL_USER, text default fg.
    assert_eq!(row_text(lines[1]), "│ hello");
    assert_eq!(row_hl(lines[1]), HL_USER as u64);
    // Row 2: bottom bar of user block.
    assert_eq!(row_text(lines[2]), "│");
    // Row 3: assistant body, plain markdown (no prefix).
    assert!(row_text(lines[3]).contains("hi there"));
    // Input row: the second-to-last line event, with just `│ ` (empty input).
    let input_line = lines[lines.len() - 2];
    assert_eq!(row_text(input_line), "│ ");
    // Bottom row: status — no stats yet, hint shows.
    let status = lines.last().expect("status");
    assert!(
        row_text(status).contains("Start chatting"),
        "got: {:?}",
        row_text(status)
    );
    assert_eq!(row_hl(status), HL_STATUS_DIM as u64);
}

#[test]
fn scrolled_state_shows_older_rows() {
    let mut s = new_state(40, 12);
    for i in 0..8 {
        s.push_entry(Role::User, format!("msg {i}"));
    }
    let events = render_frame(&s);
    let lines = find_line_events(&events);
    // Each user entry now occupies 3 rows (top bar · "│ msg N" · bottom bar).
    // Most-recent visible content rows show the latest messages first.
    let texts: Vec<String> = lines.iter().map(|l| row_text(l)).collect();
    let joined = texts.join(" | ");
    assert!(joined.contains("│ msg 7"), "joined: {joined}");

    s.scroll_up(3);
    let events = render_frame(&s);
    let lines = find_line_events(&events);
    let texts: Vec<String> = lines.iter().map(|l| row_text(l)).collect();
    let joined = texts.join(" | ");
    assert!(joined.contains("│ msg 6"), "joined: {joined}");
}

#[test]
fn input_longer_than_cols_wraps_to_multiple_rows() {
    let mut s = new_state(10, 5);
    for c in "abcdefghijklmnopqrst".chars() {
        s.input.insert_char(c);
    }
    let events = render_frame(&s);

    // Layout 5 rows, input wraps to 3 lines: row 0 transcript · rows 1-3
    // input · row 4 status. First input row is row 1, last is row 3 (cursor).
    let row_first = events
        .iter()
        .find(|e| {
            e["kind"] == Value::String("nefor-tui.grid.line".into())
                && e["row"] == Value::Number(1u32.into())
        })
        .expect("first input row");
    // Bar prefix appears on every wrapped line; inner width is cols-2 = 8.
    assert_eq!(row_text(row_first), "│ abcdefgh");

    let goto = events
        .iter()
        .find(|e| e["kind"] == Value::String("nefor-tui.grid.cursor_goto".into()))
        .expect("cursor_goto");
    assert_eq!(goto["row"], Value::Number(3u32.into()));
    assert_eq!(goto["col"], Value::Number(6u32.into()));
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
    let mut s = new_state(12, 5);
    s.push_entry(Role::User, "hi".into());
    let events = render_frame(&s);
    let lines = find_line_events(&events);
    // Row 0 is the top bar of the user block — "│" + padding to cols.
    let cells = lines[0]["cells"].as_array().expect("cells");
    assert_eq!(row_text(lines[0]), "│");
    let last = cells.last().expect("padding cell");
    assert_eq!(last[0], Value::String(" ".into()));
    assert_eq!(last[2], Value::Number(11u32.into()));
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

#[test]
fn statusline_with_full_metadata_layout() {
    let md = SessionMetadata {
        stats_seen: true,
        model: Some("claude-opus-4-7".into()),
        turns: Some(3),
        cumulative_cost_usd: Some(0.42),
        cumulative_input_tokens: Some(40_000),
        cumulative_cache_read: Some(7_000),
        last_turn_duration_ms: Some(12_000),
        ..Default::default()
    };

    let spans = build_status_spans(&md, 0, 10, 0, 100);
    let joined: String = spans.iter().map(|s| s.text.as_str()).collect();
    assert!(joined.starts_with("opus-4-7"), "{joined}");
    assert!(joined.contains("ctx 47k/200k"), "{joined}");
    assert!(joined.contains("$0.42"), "{joined}");
    assert!(joined.contains("3 turns"), "{joined}");
    assert!(joined.contains("12s"), "{joined}");
}

#[test]
fn statusline_with_no_metadata_shows_invite_hint() {
    let md = SessionMetadata::default();
    let spans = build_status_spans(&md, 0, 10, 0, 80);
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].text, "Start chatting to see stats");
    assert_eq!(spans[0].hl, HL_STATUS_DIM);
}

#[test]
fn markdown_bold_italic_inline_code_render_to_distinct_hls() {
    let mut s = new_state(80, 6);
    s.push_entry(
        Role::Assistant,
        "alpha **bold** and *italic* and `code()` end".into(),
    );
    let events = render_frame(&s);
    let lines = find_line_events(&events);
    let row = &lines[0];
    let cells = row["cells"].as_array().expect("cells");

    // Walk the cells, noting which hls appear at all.
    let mut seen_hls: std::collections::HashSet<u64> = std::collections::HashSet::new();
    for cell in cells {
        let arr = cell.as_array().expect("cell array");
        if arr.len() >= 3 {
            continue; // padding run
        }
        if let Some(hl) = arr.get(1).and_then(Value::as_u64) {
            seen_hls.insert(hl);
        }
    }
    assert!(seen_hls.contains(&(HL_MD_BOLD as u64)), "bold hl missing");
    assert!(
        seen_hls.contains(&(HL_MD_ITALIC as u64)),
        "italic hl missing"
    );
    assert!(
        seen_hls.contains(&(HL_MD_CODE_INLINE as u64)),
        "inline code hl missing"
    );
}

#[test]
fn markdown_heading_in_assistant_text_renders_with_heading_hl() {
    let mut s = new_state(80, 6);
    s.push_entry(Role::Assistant, "# big title".into());
    let events = render_frame(&s);
    let lines = find_line_events(&events);
    // Find the row with "big title" content; walk for a heading-hl span.
    let row = lines
        .iter()
        .find(|r| row_text(r).contains("big title"))
        .expect("heading row");
    let cells = row["cells"].as_array().expect("cells");
    let any_heading = cells.iter().any(|c| {
        let arr = c.as_array().expect("cell");
        arr.len() < 3 && arr.get(1).and_then(Value::as_u64) == Some(HL_MD_HEADING as u64)
    });
    assert!(any_heading, "heading hl absent on heading row");
}

#[test]
fn tool_start_for_bash_renders_one_liner_in_transcript() {
    let mut s = new_state(80, 4);
    let body = tool_start_line("Bash", Some(&serde_json::json!({"command":"ls -la"})));
    s.push_entry(Role::System, body);
    let events = render_frame(&s);
    let lines = find_line_events(&events);
    let text = row_text(lines[0]);
    assert!(text.contains("Bash"), "{text}");
    assert!(text.contains("ls -la"), "{text}");
    assert!(
        text.starts_with('['),
        "system entries are bracketed: {text}"
    );
}

#[test]
fn statusline_uses_status_hl_for_real_metadata() {
    let mut s = new_state(60, 6);
    s.metadata.stats_seen = true;
    s.metadata.model = Some("claude-opus-4-7".into());
    let events = render_frame(&s);
    let lines = find_line_events(&events);
    // Layout 6 rows: rows 0-3 transcript · row 4 input · row 5 status (bottom).
    let status_row = lines
        .iter()
        .find(|r| r["row"] == Value::Number(5u32.into()))
        .expect("status row at row=5");
    // First cell carries the model name and `HL_STATUS` (not the dim dash).
    assert_eq!(row_hl(status_row), HL_STATUS as u64);
    assert!(row_text(status_row).starts_with("opus-4-7"));
}
