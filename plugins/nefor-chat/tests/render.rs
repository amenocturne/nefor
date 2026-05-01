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
#[path = "../src/sidebar.rs"]
mod sidebar;
#[path = "../src/state.rs"]
mod state;
#[path = "../src/wrap.rs"]
mod wrap;

use serde_json::Value;

use crate::render::{
    build_status_spans, render_frame, HL_FOOTER, HL_MD_BOLD, HL_MD_CODE_INLINE, HL_MD_HEADING,
    HL_MD_ITALIC, HL_STATUS, HL_STATUS_DANGER, HL_STATUS_DIM, HL_STATUS_INFO, HL_STATUS_WARN,
    HL_SYSTEM,
};
use crate::state::{
    ChatState, DagNodeState, DagNodeStatus, DagRunUiState, Dims, Popup, Role, SessionMetadata,
};
use std::collections::{BTreeMap, HashSet};

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
fn empty_transcript_empty_input_produces_blanks_and_cursor() {
    let mut s = new_state(10, 3);
    let events = render_frame(&mut s);
    // rows = 3 (too tight for vpad) → row 0 transcript blank · row 1 input ·
    // row 2 status. First frame paints all three rows from an empty cache,
    // then cursor + flush. No grid.clear — diff rendering owns the cell
    // state directly.
    assert_eq!(events.len(), 5);
    assert!(
        events
            .iter()
            .all(|e| e["kind"] != Value::String("nefor-tui.grid.clear".into())),
        "diff renderer must not emit grid.clear"
    );
    assert_eq!(events[0]["row"], Value::Number(0u32.into())); // transcript
    assert_eq!(events[1]["row"], Value::Number(1u32.into())); // input
    assert_eq!(events[2]["row"], Value::Number(2u32.into())); // status
    assert_eq!(
        events[3]["kind"],
        Value::String("nefor-tui.grid.cursor_goto".into())
    );
    assert_eq!(
        events[4]["kind"],
        Value::String("nefor-tui.grid.flush".into())
    );
}

#[test]
fn second_render_with_unchanged_state_emits_no_grid_lines() {
    // Diff cache: after a fresh render, re-rendering the same state must
    // not re-emit any rows. Cursor + flush still emit because they're
    // small and not part of the diff discipline.
    let mut s = new_state(20, 4);
    s.push_entry(Role::User, "hello".into());
    let _ = render_frame(&mut s); // priming pass populates the cache
    let second = render_frame(&mut s);
    let line_events = find_line_events(&second);
    assert!(
        line_events.is_empty(),
        "no row should re-emit when nothing changed: {} lines",
        line_events.len()
    );
    // Frame still terminates with cursor_goto then flush.
    assert_eq!(second.len(), 2);
    assert_eq!(
        second[0]["kind"],
        Value::String("nefor-tui.grid.cursor_goto".into())
    );
    assert_eq!(
        second[1]["kind"],
        Value::String("nefor-tui.grid.flush".into())
    );
}

#[test]
fn single_keystroke_re_emits_only_input_row() {
    // Typing diffs to the input row alone — the transcript and status
    // cells haven't moved, so they stay quiet.
    let mut s = new_state(40, 6);
    s.push_entry(Role::User, "context".into());
    let _ = render_frame(&mut s);
    s.input.insert_char('a');
    let events = render_frame(&mut s);
    let line_events = find_line_events(&events);
    assert_eq!(
        line_events.len(),
        1,
        "single keystroke should re-emit exactly one row"
    );
    let text = row_text(line_events[0]);
    // hpad=2 prepends two spaces before the bar prefix.
    assert!(text.starts_with("  │ "), "input row text: {text:?}");
    assert!(text.contains('a'), "typed char must appear: {text:?}");
}

#[test]
fn resize_invalidation_forces_full_re_emit() {
    // After a fresh render the cache is hot; manually invalidating it
    // (which the main-loop resize handler does) must produce a complete
    // frame on the next render.
    let mut s = new_state(20, 4);
    let first = render_frame(&mut s);
    let first_lines = find_line_events(&first).len();
    let _ = render_frame(&mut s); // confirm cache is hot
    s.invalidate_row_cache();
    let after = render_frame(&mut s);
    assert_eq!(
        find_line_events(&after).len(),
        first_lines,
        "post-invalidation frame must re-emit all rows"
    );
}

#[test]
fn user_then_assistant_pair_layout() {
    // 12 rows give enough slack for: vpad_top(1) · user block (3) ·
    // inter-turn blank(1) · assistant body(1) · vpad_input_top(1) ·
    // input top bar(1) · input(1) · input bottom bar(1) · status(1) ·
    // vpad_bottom(1) = 12.
    let mut s = new_state(40, 12);
    s.push_entry(Role::User, "hello".into());
    s.push_entry(Role::Assistant, "hi there".into());
    let events = render_frame(&mut s);

    let lines = find_line_events(&events);
    let texts: Vec<String> = lines.iter().map(|l| row_text(l)).collect();
    let joined = texts.join("\n");
    // User block: top rule, content, bottom rule (in some adjacent rows).
    assert!(
        texts.iter().any(|t| t.trim_start().starts_with("╭─")),
        "top rule missing:\n{joined}"
    );
    assert!(
        texts.iter().any(|t| t == "  │ hello"),
        "user content row missing:\n{joined}"
    );
    assert!(
        texts.iter().any(|t| t.trim_start().starts_with("╰─")),
        "bottom rule missing:\n{joined}"
    );
    // Inter-turn blank: somewhere between the bottom rule and the
    // assistant body, a row whose text is purely whitespace.
    let bot_idx = texts
        .iter()
        .position(|t| t.trim_start().starts_with("╰─"))
        .expect("bottom rule");
    let asst_idx = texts
        .iter()
        .position(|t| t.contains("hi there"))
        .expect("assistant body");
    assert!(asst_idx > bot_idx, "assistant after bottom rule");
    let between: Vec<&String> = texts[bot_idx + 1..asst_idx].iter().collect();
    assert!(
        between.iter().any(|t| t.trim().is_empty()),
        "expected a blank inter-turn row between user block and assistant: {between:?}"
    );
    // Input prompt block: top + bottom horizontal rules around the `│ ` row.
    let input_idx = texts
        .iter()
        .position(|t| t == "  │ ")
        .expect("input prompt row");
    assert!(input_idx >= 2, "input must have a top rule above");
    assert!(
        texts[input_idx - 1].trim_start().starts_with("╭─"),
        "input top rule missing: got {:?}",
        texts[input_idx - 1]
    );
    assert!(
        texts[input_idx + 1].trim_start().starts_with("╰─"),
        "input bottom rule missing: got {:?}",
        texts[input_idx + 1]
    );
    // Status now sits at rows-2, with a blank row beneath it. Find it by
    // row index (rows=12 → status at row 10) rather than by list position.
    let status = lines
        .iter()
        .find(|r| r["row"] == Value::Number(10u32.into()))
        .expect("status row at row=10");
    assert!(
        row_text(status).contains("Start chatting"),
        "got: {:?}",
        row_text(status)
    );
    assert_eq!(row_hl(status), HL_STATUS_DIM as u64);
    // Final emitted row is the blank gutter beneath status (row 11).
    let bottom = lines
        .iter()
        .find(|r| r["row"] == Value::Number(11u32.into()))
        .expect("bottom blank at row=11");
    assert!(
        row_text(bottom).trim().is_empty(),
        "bottom row should be blank, got: {:?}",
        row_text(bottom)
    );
}

#[test]
fn scrolled_state_shows_older_rows() {
    let mut s = new_state(40, 12);
    for i in 0..8 {
        s.push_entry(Role::User, format!("msg {i}"));
    }
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);
    // Each user entry now occupies 3 rows (top bar · "│ msg N" · bottom bar).
    // Most-recent visible content rows show the latest messages first.
    let texts: Vec<String> = lines.iter().map(|l| row_text(l)).collect();
    let joined = texts.join(" | ");
    assert!(joined.contains("│ msg 7"), "joined: {joined}");

    s.scroll_up(3);
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);
    let texts: Vec<String> = lines.iter().map(|l| row_text(l)).collect();
    let joined = texts.join(" | ");
    assert!(joined.contains("│ msg 6"), "joined: {joined}");
}

#[test]
fn auto_follow_bumps_offset_when_content_grows_while_scrolled() {
    // The scroll-up path: render once to capture baseline, scroll up,
    // render again, then append content and re-render. The renderer must
    // bump scroll_offset by the wrapped-line growth so the absolute
    // viewport stays anchored on the same older lines.
    let mut s = new_state(40, 12);
    for i in 0..8 {
        s.push_entry(Role::User, format!("msg {i}"));
    }
    let _ = render_frame(&mut s);

    s.scroll_up(3);
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);
    let joined: String = lines
        .iter()
        .map(|l| row_text(l))
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(
        joined.contains("│ msg 6"),
        "expected 'msg 6' in scrolled view, got: {joined}"
    );
    let scroll_before = s.scroll_offset;

    // Stream deltas in — equivalent to assistant chunks arriving.
    s.append_assistant_delta("streaming line one\n");
    s.append_assistant_delta("streaming line two\n");
    s.append_assistant_delta("streaming line three");
    let _ = render_frame(&mut s);

    // scroll_offset should have grown to compensate for the new lines.
    assert!(
        s.scroll_offset > scroll_before,
        "expected scroll_offset to grow from {scroll_before}, got {}",
        s.scroll_offset
    );

    // Force a full re-emit so we can read the transcript rows directly:
    // the per-row diff suppresses unchanged rows, which is exactly the
    // behavior we want at runtime but obscures the viewport contents in
    // a unit test.
    s.invalidate_row_cache();
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);
    let joined: String = lines
        .iter()
        .map(|l| row_text(l))
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(
        joined.contains("│ msg 6"),
        "expected viewport to stay on 'msg 6' after stream growth, got: {joined}"
    );
    assert!(
        !joined.contains("streaming line three"),
        "newest streamed line should NOT be visible while scrolled up, got: {joined}"
    );
}

#[test]
fn auto_follow_pinned_keeps_bottom_on_stream_growth() {
    // Mirror case: when scroll_offset == 0, streaming deltas keep the
    // newest content on-screen — render-time auto-follow is implicit.
    let mut s = new_state(40, 12);
    for i in 0..8 {
        s.push_entry(Role::User, format!("msg {i}"));
    }
    let _ = render_frame(&mut s);
    assert_eq!(s.scroll_offset, 0);

    s.append_assistant_delta("brand new tail");
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);
    let joined: String = lines
        .iter()
        .map(|l| row_text(l))
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(
        joined.contains("brand new tail"),
        "expected newest delta on-screen when pinned, got: {joined}"
    );
    assert_eq!(s.scroll_offset, 0);
}

#[test]
fn input_longer_than_cols_wraps_to_multiple_rows() {
    let mut s = new_state(10, 5);
    for c in "abcdefghijklmnopqrst".chars() {
        s.input.insert_char(c);
    }
    let events = render_frame(&mut s);

    // 5 rows w/ vpad: row 0 vpad_top · row 1 transcript · row 2 input ·
    // row 3 status · row 4 vpad_bottom blank. Input is capped to 1 row in
    // this tight layout; the visible window scrolls to keep the cursor on
    // the last wrapped line ("qrst"). hpad=2, inner cols=6, prefix "│ " = 2
    // → 4-char effective text width per wrapped line.
    let row_input = events
        .iter()
        .find(|e| {
            e["kind"] == Value::String("nefor-tui.grid.line".into())
                && e["row"] == Value::Number(2u32.into())
        })
        .expect("input row");
    assert_eq!(row_text(row_input), "  │ qrst");

    let goto = events
        .iter()
        .find(|e| e["kind"] == Value::String("nefor-tui.grid.cursor_goto".into()))
        .expect("cursor_goto");
    assert_eq!(goto["row"], Value::Number(2u32.into()));
    // hpad(2) + prefix(2) + str_width("qrst")(4) = 8.
    assert_eq!(goto["col"], Value::Number(8u32.into()));
}

#[test]
fn system_entry_is_bracketed_with_system_hl() {
    let mut s = new_state(20, 3);
    s.push_entry(Role::System, "tool: read".into());
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);
    // hpad=2 prepended; first cell carries hl=0 (the pad), the bracketed
    // text follows in its own span with HL_SYSTEM.
    assert_eq!(row_text(lines[0]), "  [tool: read]");
    let cells = lines[0]["cells"].as_array().expect("cells");
    let any_system = cells.iter().any(|c| {
        let arr = c.as_array().expect("cell");
        arr.len() < 3 && arr.get(1).and_then(Value::as_u64) == Some(HL_SYSTEM as u64)
    });
    assert!(any_system, "system hl present somewhere on the row");
}

#[test]
fn padding_run_fills_to_cols() {
    let mut s = new_state(12, 5);
    s.push_entry(Role::User, "hi".into());
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);
    // Row 0 is vpad_top blank. The single visible transcript row at row 1
    // shows the bottom of the user block (only 1 row fits — viewport
    // anchors to newest). The bottom rule is `╰` + `─` ×7 spanning the
    // inner 8 cols. hpad=2 prepends two leading spaces, then the
    // remaining cols are filled by the right-side padding cell.
    let row1 = lines
        .iter()
        .find(|e| e["row"] == Value::Number(1u32.into()))
        .expect("transcript row");
    let cells = row1["cells"].as_array().expect("cells");
    assert_eq!(row_text(row1), "  ╰───────");
    let last = cells.last().expect("padding cell");
    assert_eq!(last[0], Value::String(" ".into()));
    // 12 cols total, 2 hpad + 8-char rule = 10 used, 2 left as padding.
    assert_eq!(last[2], Value::Number(2u32.into()));
}

#[test]
fn flush_always_last() {
    let mut s = new_state(8, 4);
    let events = render_frame(&mut s);
    assert_eq!(
        events.last().expect("non-empty")["kind"],
        Value::String("nefor-tui.grid.flush".into())
    );
}

#[test]
fn every_line_event_targets_grid_1() {
    let mut s = new_state(20, 4);
    let events = render_frame(&mut s);
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
        last_turn_context_tokens: Some(47_000),
        last_turn_duration_ms: Some(12_000),
        ..Default::default()
    };

    let providers: Vec<String> = Vec::new();
    let auth: std::collections::HashMap<String, crate::state::AuthStatus> =
        std::collections::HashMap::new();
    let spans = build_status_spans(&md, &providers, &auth, 0, 10, 0, 100, false);
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
    let providers: Vec<String> = Vec::new();
    let auth: std::collections::HashMap<String, crate::state::AuthStatus> =
        std::collections::HashMap::new();
    let spans = build_status_spans(&md, &providers, &auth, 0, 10, 0, 80, false);
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
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);
    // Find the assistant body row (skip vpad_top blank).
    let row = lines
        .iter()
        .find(|r| row_text(r).contains("alpha"))
        .expect("assistant row");
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
    let events = render_frame(&mut s);
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
    s.push_tool_start(
        "toolu_1".into(),
        "Bash".into(),
        serde_json::json!({"command":"ls -la"}).to_string(),
    );
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);
    let row = lines
        .iter()
        .find(|r| row_text(r).contains("Bash"))
        .expect("tool row");
    let text = row_text(row);
    assert!(text.contains("Bash"), "{text}");
    assert!(text.contains("ls -la"), "{text}");
    assert!(
        text.starts_with("▸") || text.contains("▸"),
        "collapsed tool row uses ▸ marker: {text}"
    );
}

#[test]
fn tool_expanded_via_ctrl_o_shows_salient_header_and_output() {
    let mut s = new_state(80, 24);
    s.push_tool_start(
        "toolu_1".into(),
        "Bash".into(),
        serde_json::json!({"command":"ls"}).to_string(),
    );
    assert!(s.attach_tool_end("toolu_1", "file1\nfile2".into(), false));
    s.toggle_tools_expanded();
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);
    let joined: String = lines
        .iter()
        .map(|r| row_text(r))
        .collect::<Vec<_>>()
        .join("\n");
    // Salient command rides on the header AND the full input block
    // renders below. Simple-input tools accept the redundancy so
    // structured-input tools (spawn_graph, write_file) stay viewable.
    assert!(joined.contains("▼ Bash(ls)"), "expanded header: {joined}");
    assert!(joined.contains("input:"), "input block missing: {joined}");
    assert!(
        joined.contains("\"command\""),
        "input json missing: {joined}"
    );
    assert!(joined.contains("output:"), "output label: {joined}");
    assert!(joined.contains("file1"), "output content: {joined}");
}

#[test]
fn reasoning_only_in_flight_renders_live_preview() {
    // Provider has emitted reasoning chunks but no content yet. The
    // expected shape: a `▼ thinking…` header followed by the trace
    // body in italic — the live preview that replaces the static
    // "thinking..." spinner the user used to see.
    let mut s = new_state(80, 24);
    s.append_assistant_reasoning_delta("Reading the input.\n");
    s.append_assistant_reasoning_delta("Picking the right relay.");
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);
    let joined: String = lines
        .iter()
        .map(|r| row_text(r))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("▼ thinking…"),
        "live reasoning header missing: {joined}"
    );
    assert!(
        joined.contains("Reading the input."),
        "live reasoning body missing: {joined}"
    );
    assert!(
        !joined.contains("▸ reasoning"),
        "must not show collapsed marker while still streaming reasoning-only: {joined}"
    );
}

#[test]
fn reasoning_collapses_to_one_row_once_content_arrives() {
    // The full path: reasoning streams first, then reasoning_end fires
    // (provider's boundary signal), then content streams normally. The
    // reasoning trace must collapse to `▸ reasoning (Ns)` and the
    // content render below it.
    let mut s = new_state(80, 24);
    s.append_assistant_reasoning_delta("thinking about it");
    s.finalize_assistant_reasoning(Some("thinking about it".into()), Some(1_500));
    s.append_assistant_delta("Final answer.");
    s.finalize_assistant(None);
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);
    let joined: String = lines
        .iter()
        .map(|r| row_text(r))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("▸ reasoning"),
        "collapsed reasoning marker missing: {joined}"
    );
    assert!(
        joined.contains("1.5s") || joined.contains("1s"),
        "duration label missing: {joined}"
    );
    assert!(
        joined.contains("Final answer"),
        "content body missing: {joined}"
    );
    // The full trace text must NOT bleed into the rendered surface
    // when collapsed — only the marker line.
    assert!(
        !joined.contains("thinking about it"),
        "full trace must be hidden while collapsed: {joined}"
    );
}

#[test]
fn reasoning_only_finalized_collapses_to_marker() {
    // Provider streamed reasoning, fired reasoning_end, but the turn
    // continued with a tool call instead of content. The reasoning
    // must still collapse to `▸ reasoning (Ns)` — the full trace is
    // reachable via Ctrl+O like every other reasoning entry.
    let mut s = new_state(80, 24);
    s.append_assistant_reasoning_delta("planning the spawn_graph call");
    s.finalize_assistant_reasoning(Some("planning the spawn_graph call".into()), Some(800));
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);
    let joined: String = lines
        .iter()
        .map(|r| row_text(r))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("▸ reasoning"),
        "collapsed reasoning marker missing: {joined}"
    );
    assert!(
        !joined.contains("planning the spawn_graph"),
        "full trace must be hidden while collapsed: {joined}"
    );
    assert!(
        !joined.contains("no content produced"),
        "stale 'no content produced' branch still rendering: {joined}"
    );
}

#[test]
fn assistant_body_survives_empty_authoritative_stream_end() {
    // End-to-end repro for the disappearing-body bug: deltas accumulate
    // a visible reply, then `chat.stream.end` arrives with `text=""`. The
    // body must still render; only the footer should attach.
    // Tall viewport (24 rows) so the rendered table doesn't push the
    // leading text out of the bottom-anchored window.
    let mut s = new_state(80, 24);
    s.append_assistant_delta("here is a tiny table:\n");
    s.append_assistant_delta("\n| a | b |\n| - | - |\n| 1 | 2 |\n");
    s.finalize_assistant(Some(String::new()));
    s.stamp_last_assistant(Some("claude-opus-4-7".into()), Some(3_000));

    let events = render_frame(&mut s);
    let lines = find_line_events(&events);
    let joined: String = lines
        .iter()
        .map(|l| row_text(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("here is a tiny table"),
        "assistant body must render after empty stream.end.text; got rows:\n{joined}"
    );
    assert!(
        joined.contains("opus-4-7"),
        "footer must still render alongside the body; got rows:\n{joined}"
    );
}

#[test]
fn statusline_uses_status_hl_for_real_metadata() {
    let mut s = new_state(60, 6);
    s.metadata.stats_seen = true;
    s.metadata.model = Some("claude-opus-4-7".into());
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);
    // Layout 6 rows: row 0 vpad · row 1 transcript · row 2 vpad_input_top ·
    // row 3 input · row 4 status · row 5 vpad_bottom blank.
    let status_row = lines
        .iter()
        .find(|r| r["row"] == Value::Number(4u32.into()))
        .expect("status row at row=4");
    // First cell carries the model name and `HL_STATUS` (not the dim dash).
    assert_eq!(row_hl(status_row), HL_STATUS as u64);
    assert!(row_text(status_row).starts_with("opus-4-7"));
}

// ---- popup overlay --------------------------------------------------------

#[test]
fn popup_help_renders_centered_box() {
    let mut s = new_state(80, 24);
    s.open_popup_help();
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);
    // Help popup body sits in the centered rect: roughly 60% of 80 = 48 wide,
    // 60% of 24 ≈ 14 tall. Top border should land around row (24-14)/2 = 5.
    // We verify a row in that range carries the title and the box border.
    let top_row = 5u32;
    let top = lines
        .iter()
        .find(|r| r["row"] == Value::Number(top_row.into()))
        .expect("popup top border row");
    let txt = row_text(top);
    assert!(txt.contains("┌"), "popup top border missing: {txt:?}");
    assert!(txt.contains("help"), "popup title missing: {txt:?}");
    // The body should mention at least one slash command.
    let any_help_row = lines.iter().any(|r| row_text(r).contains("/login"));
    assert!(any_help_row, "popup body should list /login");
}

#[test]
fn popup_model_picker_renders_cursor_highlight() {
    let mut s = new_state(80, 24);
    let mut awaiting: HashSet<String> = HashSet::new();
    awaiting.insert("ollama".into());
    s.open_popup_model_picker(awaiting);
    s.popup_models_listed(
        "ollama",
        &[
            "llama3:8b".to_string(),
            "qwen2.5-coder:7b".to_string(),
            "mistral:7b".to_string(),
        ],
    );
    // Sorted: llama3 (0), mistral (1), qwen (2). Set cursor to qwen.
    if let Some(Popup::ModelPicker { cursor, .. }) = &mut s.popup {
        *cursor = 2;
    }
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);
    let cursor_row = lines
        .iter()
        .find(|r| {
            let t = row_text(r);
            t.contains("ollama") && t.contains("qwen2.5-coder")
        })
        .expect("cursor row missing");
    // Inspect cells: first non-padding cell is the left border `│` (HL_USER),
    // second is the body span. Cursor highlight uses HL_STATUS.
    let cells = cursor_row["cells"].as_array().expect("cells");
    // Skip the leading padding cell (hl=0) at index 0; the next cell is `│`,
    // followed by the body cell with the highlighted hl id.
    let mut hls: Vec<u64> = Vec::new();
    for c in cells {
        let arr = c.as_array().expect("cell");
        if arr.len() >= 2 {
            if let Some(hl) = arr[1].as_u64() {
                hls.push(hl);
            }
        }
    }
    assert!(
        hls.contains(&(HL_STATUS as u64)),
        "cursor row should carry HL_STATUS somewhere: hls={hls:?}"
    );
}

#[test]
fn popup_model_picker_shows_count_indicator_in_title_for_long_lists() {
    // Long lists overflow the visible body; instead of a "+N more" footer
    // (which crowded the body) the picker shows a `<cursor>/<total>` chip in
    // the title bar so the user can navigate without consuming a row.
    let mut s = new_state(80, 24);
    s.open_popup_model_picker(HashSet::new());
    let many: Vec<String> = (0..200).map(|i| format!("model-{i:03}")).collect();
    s.popup_models_listed("groq", &many);
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);
    let has_indicator = lines.iter().any(|r| row_text(r).contains("/200"));
    assert!(
        has_indicator,
        "expected `<cursor>/200` count indicator in title"
    );
}

#[test]
fn popup_blank_state_when_no_providers_renders_helpful_message() {
    let mut s = new_state(80, 24);
    s.open_popup_model_picker(HashSet::new());
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);
    let any_msg = lines
        .iter()
        .any(|r| row_text(r).contains("No providers connected"));
    assert!(any_msg, "expected empty-state copy in popup");
}

/// Walks every span in the rendered frame and reports the set of `hl` ids
/// the body uses. Helper for the warning/error styling assertions below.
fn frame_hl_ids(events: &[serde_json::Map<String, Value>]) -> Vec<u64> {
    let mut out: Vec<u64> = Vec::new();
    for ev in events {
        let Some(cells) = ev.get("cells").and_then(Value::as_array) else {
            continue;
        };
        for c in cells {
            let Some(arr) = c.as_array() else { continue };
            if arr.len() < 2 {
                continue;
            }
            if let Some(hl) = arr[1].as_u64() {
                if !out.contains(&hl) {
                    out.push(hl);
                }
            }
        }
    }
    out
}

/// Walks the cell array tracking the running hl id (cells encoded as
/// `[ch]` inherit the last `[ch, hl]`; trailing pads encode as `[ch, hl, n]`).
/// Returns the de-duplicated list of hl ids seen on the row.
fn row_hls(row: &serde_json::Map<String, Value>) -> Vec<u64> {
    let cells = row["cells"].as_array().expect("cells");
    let mut current: Option<u64> = None;
    let mut out: Vec<u64> = Vec::new();
    for c in cells {
        let Some(arr) = c.as_array() else { continue };
        if arr.len() >= 2 {
            if let Some(hl) = arr[1].as_u64() {
                current = Some(hl);
            }
        }
        if let Some(hl) = current {
            if !out.contains(&hl) {
                out.push(hl);
            }
        }
    }
    out
}

#[test]
fn popup_warning_renders_with_warning_styling() {
    let mut s = new_state(80, 24);
    s.open_popup_warning("login", "Unknown provider 'foo'.", None);
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);
    let title_row = lines
        .iter()
        .find(|r| row_text(r).contains("warning") && row_text(r).contains("login"))
        .expect("warning title row");
    let hls = row_hls(title_row);
    assert!(
        hls.contains(&(HL_STATUS_WARN as u64)),
        "title bar must carry HL_STATUS_WARN: hls={hls:?}"
    );
    assert!(
        lines
            .iter()
            .any(|r| row_text(r).contains("Unknown provider")),
        "warning message body missing"
    );
    assert!(
        lines.iter().any(|r| row_text(r).contains("ESC or Q")),
        "footer hint missing"
    );
    let ids = frame_hl_ids(&events);
    assert!(
        ids.contains(&(HL_STATUS_WARN as u64)),
        "expected HL_STATUS_WARN somewhere in the frame"
    );
}

#[test]
fn popup_error_renders_with_danger_styling() {
    let mut s = new_state(80, 24);
    s.open_popup_error("anthropic", "HTTP 401: bad token", None);
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);
    let title_row = lines
        .iter()
        .find(|r| row_text(r).contains("error") && row_text(r).contains("anthropic"))
        .expect("error title row");
    let hls = row_hls(title_row);
    assert!(
        hls.contains(&(HL_STATUS_DANGER as u64)),
        "title bar must carry HL_STATUS_DANGER: hls={hls:?}"
    );
    assert!(
        lines.iter().any(|r| row_text(r).contains("HTTP 401")),
        "error message body missing"
    );
    let ids = frame_hl_ids(&events);
    assert!(
        ids.contains(&(HL_STATUS_DANGER as u64)),
        "expected HL_STATUS_DANGER somewhere in the frame"
    );
}

#[test]
fn popup_info_renders_with_info_styling() {
    let mut s = new_state(80, 24);
    s.open_popup_info("models updated", "phi4-mini selected.", None);
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);
    let title_row = lines
        .iter()
        .find(|r| row_text(r).contains("info") && row_text(r).contains("models updated"))
        .expect("info title row");
    let hls = row_hls(title_row);
    assert!(
        hls.contains(&(HL_STATUS_INFO as u64)),
        "title bar must carry HL_STATUS_INFO: hls={hls:?}"
    );
    assert!(
        lines
            .iter()
            .any(|r| row_text(r).contains("phi4-mini selected")),
        "info message body missing"
    );
    assert!(
        lines.iter().any(|r| row_text(r).contains("ESC or Q")),
        "footer hint missing"
    );
    let ids = frame_hl_ids(&events);
    assert!(
        ids.contains(&(HL_STATUS_INFO as u64)),
        "expected HL_STATUS_INFO somewhere in the frame"
    );
}

#[test]
fn popup_warning_with_source_renders_from_footer() {
    // When `source` is set, a dim `from: <source>` line renders directly
    // above the close-hint footer so the user can attribute the popup to
    // its publisher.
    let mut s = new_state(80, 24);
    s.open_popup_warning("rate limit", "slow down", Some("anthropic".into()));
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);

    let from_row = lines
        .iter()
        .find(|r| row_text(r).contains("from: anthropic"))
        .expect("expected `from: anthropic` footer line");
    let hls = row_hls(from_row);
    assert!(
        hls.contains(&(HL_FOOTER as u64)),
        "from-line body must render with HL_FOOTER (dim): hls={hls:?}"
    );

    // The `from:` row sits above the close-hint row — geometric ordering is
    // load-bearing for the layout described in `render_popup_message`.
    let from_row_idx = from_row["row"].as_u64().expect("row index");
    let close_row = lines
        .iter()
        .find(|r| row_text(r).contains("ESC or Q"))
        .and_then(|r| r["row"].as_u64())
        .expect("close-hint row");
    assert!(
        from_row_idx < close_row,
        "from-line ({from_row_idx}) must precede close-hint ({close_row})"
    );
}

// ---- popup viewport / scrolling ------------------------------------------

#[test]
fn popup_viewport_clamps_to_leave_input_and_status_visible() {
    // Tall help body (lots of entries) on a tight terminal: the popup must
    // not extend into the rows reserved for the input box and status bar.
    // We assert by checking the bottom-most popup border row is well above
    // the terminal's last few rows.
    let mut s = new_state(80, 14);
    s.open_popup_help();
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);
    // Rows reserved at the bottom: status (last row) + input box (3) +
    // padding = roughly the last 6 rows. We require the popup's bottom
    // border to land at or above row `rows - reserved`.
    let bottom_border_row = lines
        .iter()
        .filter_map(|r| {
            if row_text(r).contains("└") {
                r["row"].as_u64()
            } else {
                None
            }
        })
        .max()
        .expect("popup bottom border must render");
    // POPUP_RESERVED_ROWS = 6 in render.rs; the popup must end strictly
    // before the input box starts. With rows=14, reserved=6, the popup
    // body has at most 14-6=8 rows; centered around row 3, the bottom
    // border lands at most at row ~9.
    assert!(
        bottom_border_row <= 14 - 4,
        "popup bottom border at row {bottom_border_row} overlaps reserved rows"
    );
}

#[test]
fn popup_help_scroll_offset_shifts_visible_lines() {
    // Set scroll=2 on a Help popup; the rendered body should start two
    // entries deeper into the help-line list.
    let mut s = new_state(80, 12);
    s.open_popup_help();
    if let Some(Popup::Help { scroll }) = &mut s.popup {
        *scroll = 2;
    }
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);
    // Default first entry is "/help"; with scroll=2 it should NOT appear in
    // the rendered body any more (within the smaller height).
    let any_help_row = lines
        .iter()
        .any(|r| row_text(r).contains("show this popup"));
    assert!(
        !any_help_row,
        "scroll=2 should drop the /help row off the top of the body"
    );
}

#[test]
fn popup_model_picker_search_bar_appears_above_list() {
    // Search bar moved from below to immediately under the title. The row
    // index of the `search:` text must be smaller than the row index of
    // any model row.
    let mut s = new_state(80, 24);
    s.open_popup_model_picker(HashSet::new());
    s.popup_models_listed("ollama", &["llama3:8b".into(), "mistral:7b".into()]);
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);
    let search_row = lines
        .iter()
        .find(|r| row_text(r).contains("search:"))
        .and_then(|r| r["row"].as_u64())
        .expect("search row must render");
    let first_model_row = lines
        .iter()
        .filter(|r| row_text(r).contains("llama3:8b"))
        .filter_map(|r| r["row"].as_u64())
        .min()
        .expect("first model row must render");
    assert!(
        search_row < first_model_row,
        "search row ({search_row}) must precede the model list ({first_model_row})"
    );
}

#[test]
fn popup_slash_autocomplete_renders_matches() {
    use crate::state::SlashCommand;
    let mut s = new_state(80, 20);
    let matches = vec![
        SlashCommand {
            name: "login".into(),
            aliases: Vec::new(),
            hint: "authenticate a provider".into(),
            takes_args: true,
        },
        SlashCommand {
            name: "logout".into(),
            aliases: Vec::new(),
            hint: "revoke a provider's auth".into(),
            takes_args: true,
        },
    ];
    s.open_or_update_popup_slash_autocomplete(matches);
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);
    let has_login = lines.iter().any(|r| row_text(r).contains("/login"));
    let has_logout = lines.iter().any(|r| row_text(r).contains("/logout"));
    assert!(has_login, "expected `/login` row in autocomplete band");
    assert!(has_logout, "expected `/logout` row in autocomplete band");
}

/// Find the row index of the input top-bar by looking for the `╭─` glyph
/// (which only the top rule emits). Used by the inline-autocomplete tests
/// to locate the band sitting directly above it.
fn find_input_top_bar_row(events: &[serde_json::Map<String, Value>]) -> Option<u64> {
    for ev in events {
        if ev["kind"] != Value::String("nefor-tui.grid.line".into()) {
            continue;
        }
        let text = row_text(ev);
        if text.contains("╭") {
            return ev["row"].as_u64();
        }
    }
    None
}

#[test]
fn slash_autocomplete_open_renders_inline_rows_above_input() {
    use crate::state::SlashCommand;
    let mut s = new_state(80, 20);
    let matches = vec![
        SlashCommand {
            name: "help".into(),
            aliases: Vec::new(),
            hint: "show help".into(),
            takes_args: false,
        },
        SlashCommand {
            name: "login".into(),
            aliases: Vec::new(),
            hint: "authenticate a provider".into(),
            takes_args: true,
        },
    ];
    s.open_or_update_popup_slash_autocomplete(matches);
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);
    let top_bar = find_input_top_bar_row(&events).expect("input top bar must be present");

    // Rows immediately above `input_top_bar_row` carry the matches. With
    // 2 matches, that's exactly the 2 rows directly above the bar.
    let row_help = lines
        .iter()
        .find(|r| row_text(r).contains("/help"))
        .expect("/help row missing");
    let row_login = lines
        .iter()
        .find(|r| row_text(r).contains("/login"))
        .expect("/login row missing");

    let help_row_idx = row_help["row"].as_u64().unwrap();
    let login_row_idx = row_login["row"].as_u64().unwrap();
    assert!(
        help_row_idx < top_bar,
        "/help row {help_row_idx} must precede input top bar {top_bar}"
    );
    assert!(
        login_row_idx < top_bar,
        "/login row {login_row_idx} must precede input top bar {top_bar}"
    );
    // The autocomplete band sits directly above the input top bar,
    // separated only by the existing 1-row `vpad_input_top` gutter (when
    // the layout has slack for one). So the last autocomplete row should
    // be at most 2 rows above the top bar.
    assert!(
        top_bar - login_row_idx <= 2,
        "last autocomplete row {login_row_idx} should be near input top bar {top_bar}"
    );
}

#[test]
fn slash_autocomplete_height_caps_at_eight() {
    use crate::state::SlashCommand;
    let mut s = new_state(80, 30);
    let matches: Vec<SlashCommand> = (0..20)
        .map(|i| SlashCommand {
            name: format!("cmd{i:02}"),
            aliases: Vec::new(),
            hint: format!("hint {i}"),
            takes_args: false,
        })
        .collect();
    s.open_or_update_popup_slash_autocomplete(matches);
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);

    // Count rows whose text starts with `/cmd` — those are autocomplete
    // entries. The cap is 8 visible at a time.
    let n: usize = lines
        .iter()
        .filter(|r| row_text(r).contains("/cmd"))
        .count();
    assert!(n <= 8, "autocomplete band must cap at 8 rows, got {n}");
    // And we expect the cap to actually bind here (registry has 20).
    assert_eq!(n, 8, "expected cap of 8 rows when 20 matches available");
}

#[test]
fn slash_autocomplete_empty_matches_shows_no_match_row() {
    let mut s = new_state(80, 20);
    s.open_or_update_popup_slash_autocomplete(Vec::new());
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);
    let has_no_match = lines
        .iter()
        .any(|r| row_text(r).contains("no matching commands"));
    assert!(
        has_no_match,
        "empty autocomplete must show 'no matching commands' row"
    );
}

#[test]
fn slash_autocomplete_does_not_render_centered_popup_overlay() {
    use crate::state::SlashCommand;
    // Centered popups draw a top-border with `┌` glyph in the middle of
    // the screen. With slash autocomplete open we must NOT see that —
    // it's an inline band, not a centered overlay.
    let mut s = new_state(80, 24);
    let matches = vec![SlashCommand {
        name: "help".into(),
        aliases: Vec::new(),
        hint: "show help".into(),
        takes_args: false,
    }];
    s.open_or_update_popup_slash_autocomplete(matches);
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);
    for r in &lines {
        let text = row_text(r);
        let row_idx = r["row"].as_u64().unwrap();
        // Centered popup top borders include `┌──` (corner + dashes); the
        // input top bar uses `╭─` (different rounded corner). Either char
        // appearing in the middle third of the screen would mean a
        // centered popup leaked through.
        let middle = (8u64..16u64).contains(&row_idx);
        assert!(
            !(middle && text.contains('┌')),
            "centered popup top border leaked at row {row_idx}: {text:?}"
        );
    }
}

#[test]
fn dag_panel_renders_run_header_and_node_lines() {
    // 18 rows of slack — comfortably above DAG_PANEL_MAX_ROWS so the panel
    // can render a full header + 2-node block without truncation.
    // 200 cols leaves the sidebar with `cols * 25 / 100 = 50` clamped to
    // SIDEBAR_MAX_COLS (40), well above the 36-col threshold that selects
    // the wide DAG layout (with reasoner + status word). Below 100 cols
    // the sidebar auto-hides entirely; that's covered separately by
    // `sidebar_auto_hides_on_narrow_terminal`.
    let mut s = new_state(200, 18);
    let mut nodes = BTreeMap::new();
    nodes.insert(
        "n1".to_string(),
        DagNodeState {
            reasoner: "ollama".into(),
            status: DagNodeStatus::Running,
            started_at_ms: 0,
            finished_at_ms: None,
        },
    );
    nodes.insert(
        "n2".to_string(),
        DagNodeState {
            reasoner: "ollama".into(),
            status: DagNodeStatus::Done,
            started_at_ms: 0,
            finished_at_ms: Some(2_300),
        },
    );
    s.dag_runs.insert(
        "ab8c0000-0000-0000-0000-000000000000".to_string(),
        DagRunUiState {
            run_id: "ab8c0000-0000-0000-0000-000000000000".into(),
            started_at_ms: 0,
            total_nodes: 2,
            nodes,
            completed_at_ms: None,
        },
    );
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);
    let texts: Vec<String> = lines.iter().map(|l| row_text(l)).collect();
    let joined = texts.join("\n");
    // Header carries the 8-char run-id prefix and the (M/N nodes) counter.
    assert!(
        texts
            .iter()
            .any(|t| t.contains("ab8c0000") && t.contains("nodes")),
        "panel header missing run-id prefix or counter:\n{joined}"
    );
    // Node rows mention the node ids and the reasoner.
    assert!(
        texts
            .iter()
            .any(|t| t.contains("n1") && t.contains("ollama") && t.contains("running")),
        "n1 running row missing:\n{joined}"
    );
    assert!(
        texts
            .iter()
            .any(|t| t.contains("n2") && t.contains("ollama") && t.contains("done")),
        "n2 done row missing:\n{joined}"
    );
}

#[test]
fn dag_panel_absent_when_no_runs() {
    // Sanity: no DAG runs means no panel rows — every diff should still go
    // through, but no row should mention "DAG ".
    let mut s = new_state(40, 10);
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);
    let joined: String = lines
        .iter()
        .map(|l| row_text(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !joined.contains("─ DAG "),
        "panel must not render: {joined}"
    );
}

// ---- right sidebar pane --------------------------------------------------

/// Build a `ChatState` pre-loaded with one DAG run + N nodes. Used by
/// several sidebar tests to exercise the widget under different terminal
/// dimensions.
fn state_with_dag_runs(cols: u32, rows: u32, run_count: usize, nodes_per_run: usize) -> ChatState {
    let mut s = new_state(cols, rows);
    for r in 0..run_count {
        let mut nodes = BTreeMap::new();
        for n in 0..nodes_per_run {
            nodes.insert(
                format!("n{n}"),
                DagNodeState {
                    reasoner: "ollama".into(),
                    status: if n % 2 == 0 {
                        DagNodeStatus::Running
                    } else {
                        DagNodeStatus::Done
                    },
                    started_at_ms: 0,
                    finished_at_ms: if n % 2 == 0 { None } else { Some(2_300) },
                },
            );
        }
        // Pad the run id to 36 chars (uuid-shaped) so prefix-truncation gets
        // exercised on the header line.
        s.dag_runs.insert(
            format!("ab8c{r:04}-0000-0000-0000-000000000000"),
            DagRunUiState {
                run_id: format!("ab8c{r:04}-0000-0000-0000-000000000000"),
                started_at_ms: 0,
                total_nodes: nodes_per_run,
                nodes,
                completed_at_ms: None,
            },
        );
    }
    s
}

#[test]
fn sidebar_hidden_means_full_width_transcript() {
    // With sidebar_visible = false, the chat pane gets the full terminal
    // width — no columns are reserved on the right. We assert that by
    // checking the transcript top rule of a long user-block bottom rule
    // spans nearly all `cols` (allowing for the `hpad` left-inset).
    let mut s = new_state(120, 12);
    s.sidebar_visible = false;
    s.dag_runs.insert(
        "ab8c0000-0000-0000-0000-000000000000".to_string(),
        DagRunUiState {
            run_id: "ab8c0000-0000-0000-0000-000000000000".into(),
            started_at_ms: 0,
            total_nodes: 1,
            nodes: BTreeMap::new(),
            completed_at_ms: None,
        },
    );
    s.push_entry(Role::User, "hello".into());
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);
    // The user-block bottom rule (`╰─` ... `─`) is a stable proxy for the
    // chat pane's effective width: it always spans `chat_cols`. With
    // sidebar hidden, that should be ~`cols - 2*hpad` wide, not
    // ~`chat_cols - 2*hpad` (which would be smaller).
    let bot_rule = lines
        .iter()
        .find(|r| row_text(r).trim_start().starts_with("╰─"))
        .expect("bottom rule must render");
    let text = row_text(bot_rule).trim().to_string();
    // hpad=2 on each side → the rule fills `cols - 2*2 = 116` cols.
    assert_eq!(
        text.chars().count(),
        116,
        "bottom rule should span full chat width when sidebar hidden, got {} chars: {text:?}",
        text.chars().count()
    );
}

#[test]
fn sidebar_persists_when_no_widgets_active() {
    // Sidebar is persistent (decision changed from "collapse when empty" to
    // "always reserve width while visible"). With sidebar_visible=true and
    // zero DAG runs, the chat pane keeps the reduced width and an empty-
    // hint row is shown in the sidebar so the layout doesn't jump as runs
    // come and go.
    let cols = 120u32;
    let mut s = new_state(cols, 12);
    assert!(s.sidebar_visible);
    assert!(s.dag_runs.is_empty());
    s.push_entry(Role::User, "hello".into());
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);
    let bot_rule = lines
        .iter()
        .find(|r| row_text(r).trim_start().starts_with("╰─"))
        .expect("bottom rule must render");
    let text = row_text(bot_rule).trim().to_string();
    let sidebar_w = s.sidebar_width() as usize;
    assert!(
        sidebar_w > 0,
        "sidebar must reserve width even with no widgets",
    );
    let chat_cols = (cols as usize) - sidebar_w;
    // Bottom rule spans the chat-pane interior (cols - 4 indent/pad approx).
    // Just assert it's narrower than `cols - 1` (full width sentinel) and at
    // most chat_cols-ish wide.
    assert!(
        text.chars().count() < cols as usize,
        "bottom rule must not span full width when sidebar is reserved: {text:?}",
    );
    assert!(
        text.chars().count() <= chat_cols,
        "bottom rule width must fit inside chat pane (chat_cols={chat_cols}), got {} chars",
        text.chars().count(),
    );
    // Empty-sidebar hint row should appear at least once.
    let saw_hint = lines
        .iter()
        .any(|r| row_text(r).contains("(no active runs)"));
    assert!(saw_hint, "empty sidebar should render its hint row");
}

#[test]
fn sidebar_renders_dag_widget_when_runs_active() {
    // Cols 200 sits well above SIDEBAR_MIN_TERMINAL_COLS (100) so the
    // sidebar is visible, and well above the 144-col threshold where
    // sidebar_w hits its 40-col max. Confirm DAG content lands on the
    // right side of the screen (column index >= chat_cols).
    let s = state_with_dag_runs(200, 18, 1, 2);
    let mut s = s;
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);
    // chat_cols = 200 - 40 = 160. Run-id prefix and node ids appear in the
    // sidebar region (column range [chat_cols, cols)).
    let run_row = lines
        .iter()
        .find(|r| row_text(r).contains("ab8c0000"))
        .expect("run-id prefix must appear in sidebar");
    let txt = row_text(run_row);
    // Find the column of "ab8c0000" in the row text — every char before it
    // is left padding from the chat pane (blank) and the sidebar's leading
    // dashes.
    let prefix_col = txt
        .char_indices()
        .find(|(_, c)| *c == 'a')
        .map(|(i, _)| i)
        .expect("ab8c0000 must appear");
    // chat_cols when sidebar_w=40 is 160; the sidebar occupies 160..200.
    assert!(
        prefix_col >= 160,
        "run-id prefix must land in sidebar region (col >= 160), found at col {prefix_col}: {txt:?}"
    );
    // Node ids are also in the sidebar region.
    assert!(
        lines
            .iter()
            .any(|r| row_text(r).contains("n0") && row_text(r).contains("ollama")),
        "n0 row must render in sidebar"
    );
}

#[test]
fn sidebar_auto_hides_on_narrow_terminal() {
    // 80 cols < SIDEBAR_MIN_TERMINAL_COLS (100) → sidebar must be 0 wide,
    // even with widget content available. The chat pane gets the full
    // terminal width and the DAG widget doesn't render anywhere.
    let s = state_with_dag_runs(80, 12, 1, 2);
    let mut s = s;
    assert_eq!(s.sidebar_width(), 0);
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);
    let joined: String = lines
        .iter()
        .map(|l| row_text(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !joined.contains("ab8c0000"),
        "sidebar must auto-hide on narrow terminal — DAG content leaked: {joined}"
    );
}

#[test]
fn sidebar_widget_rows_truncate_when_exceeding_rows() {
    // Many runs/nodes — the sidebar caps DAG rows at DAG_PANEL_MAX_ROWS (8)
    // before the sidebar's own per-frame row budget is exhausted, so a
    // running scheduler can't crowd out other (future) widgets. Render
    // and confirm the truncation marker is present on the sidebar side.
    let s = state_with_dag_runs(200, 30, 4, 5); // 4 * (1 header + 5 nodes) = 24 rows of content
    let mut s = s;
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);
    let joined: String = lines
        .iter()
        .map(|l| row_text(l))
        .collect::<Vec<_>>()
        .join("\n");
    // The widget asks for max 8 rows; with 24 raw rows of content the
    // overflow marker `… +K more` lands on the last visible widget row.
    assert!(
        joined.contains("… +"),
        "expected `… +K more` overflow marker when content exceeds widget cap: {joined}"
    );
}

#[test]
fn popup_centered_over_chat_pane_not_full_width() {
    // With sidebar visible and active, popup centering uses chat_cols not
    // cols — so the popup left edge sits at (chat_cols - popup_w) / 2,
    // not (cols - popup_w) / 2. The popup's right border lands inside the
    // chat region, never spilling into the sidebar.
    let s = state_with_dag_runs(200, 24, 1, 2);
    let mut s = s;
    s.open_popup_help();
    let events = render_frame(&mut s);
    let lines = find_line_events(&events);
    // Popup top-border text starts with the leading-pad spaces, then
    // `┌──`. Locate the row carrying the help title.
    let popup_row = lines
        .iter()
        .find(|r| row_text(r).contains("┌") && row_text(r).contains("help"))
        .expect("popup top border with title must render");
    let text = row_text(popup_row);
    // Right edge of the popup is the rightmost `┐` glyph. Translate from
    // char-position (NOT byte-position — `─` is 3 bytes) to display column.
    // For popup-border glyphs each is 1 col wide, so char-position is the
    // column index.
    let right_edge_char = text
        .chars()
        .enumerate()
        .filter(|(_, c)| *c == '┐')
        .map(|(i, _)| i)
        .last()
        .expect("popup top border must contain a `┐` glyph");
    // chat_cols = 200 - 40 (sidebar_w) = 160. Popup must end strictly
    // inside the chat region.
    assert!(
        right_edge_char < 160,
        "popup right edge ({right_edge_char}) must stay inside chat pane (< 160): {text:?}"
    );
}

#[test]
fn sidebar_default_visible_in_new_state() {
    // Sidebar starts on; the user toggles it off via Ctrl-B if they want
    // more typing room.
    let s = ChatState::new();
    assert!(s.sidebar_visible);
}
