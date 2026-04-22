//! State → NCP grid events.
//!
//! Pure functions: given a [`ChatState`], produce the ordered list of
//! `nefor-tui.*` event bodies that redraw the frame. The main loop wraps
//! each body in a [`PluginOutgoing::event`] and hands it to the stdout
//! writer.
//!
//! Render strategy v1: full redraw per state change. The helper builds the
//! sequence `clear → line*(transcript) → line(input) → cursor_goto → flush`
//! and emits a complete frame each time. Optimise later if needed.

use serde_json::{Map, Value};

use crate::state::{ChatState, Role, TranscriptEntry};
use crate::wrap::{str_width, wrap_to_width};

// ---- palette ---------------------------------------------------------------
//
// Highlight IDs we assign. 0 is the default (engine-managed); we pick small
// positive integers for our palette and emit `hl_attr_define` for each at
// startup via [`palette_defines`].
//
// These IDs are plugin-local — only we reference them. The names are
// written as ASCII-adjacent muted terminal colors (24-bit RGB) that look
// reasonable on both light and dark backgrounds; the user can theme later
// by injecting different `hl_attr_define` values.

/// "user> " prompt prefix (bright accent, bold).
pub const HL_USER: u32 = 1;
/// Assistant response body (default fg, regular).
pub const HL_ASSISTANT: u32 = 2;
/// System/meta lines (tool starts, errors — dim).
pub const HL_SYSTEM: u32 = 3;
/// Input line body (default fg, regular).
pub const HL_INPUT: u32 = 4;
/// Status / reminders (italic dim, unused in v1 but reserved).
pub const HL_STATUS: u32 = 5;

/// Build the `hl_attr_define` events for our palette. Called once after
/// `ready_ok`.
pub fn palette_defines() -> Vec<Map<String, Value>> {
    vec![
        hl_attr_define(HL_USER, Some(0x7FB4FF), None, true, false),
        hl_attr_define(HL_ASSISTANT, Some(0xE0E0E0), None, false, false),
        hl_attr_define(HL_SYSTEM, Some(0x808080), None, false, true),
        hl_attr_define(HL_INPUT, Some(0xFFFFFF), None, false, false),
        hl_attr_define(HL_STATUS, Some(0x808080), None, false, true),
    ]
}

/// Default colors. Engine-ish defaults — black background, light
/// foreground, cyan special. The user's terminal theme usually overrides
/// these at the outer shell, but nefor-tui needs concrete values for the
/// cells that don't reference a palette entry.
//
// `#[allow(dead_code)]` is for integration-test builds that `#[path]`-
// include this module but never reach `main.rs`. Production emits this
// from `emit_palette` in `src/main.rs`.
#[allow(dead_code)]
pub fn default_colors_event() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String("nefor-tui.default_colors".into()),
    );
    m.insert("fg".into(), Value::Number(0x00E0E0E0_u32.into()));
    m.insert("bg".into(), Value::Number(0x00000000_u32.into()));
    m.insert("sp".into(), Value::Number(0x00_66_66_66_u32.into()));
    m
}

fn hl_attr_define(
    id: u32,
    fg: Option<u32>,
    bg: Option<u32>,
    bold: bool,
    italic: bool,
) -> Map<String, Value> {
    let mut rgb = Map::new();
    if let Some(v) = fg {
        rgb.insert("fg".into(), Value::Number(v.into()));
    }
    if let Some(v) = bg {
        rgb.insert("bg".into(), Value::Number(v.into()));
    }
    if bold {
        rgb.insert("bold".into(), Value::Bool(true));
    }
    if italic {
        rgb.insert("italic".into(), Value::Bool(true));
    }
    let mut body = Map::new();
    body.insert(
        "kind".into(),
        Value::String("nefor-tui.hl_attr_define".into()),
    );
    body.insert("id".into(), Value::Number(id.into()));
    body.insert("rgb".into(), Value::Object(rgb));
    body
}

// ---- frame construction ----------------------------------------------------

/// Produce the full sequence of grid events for a single redraw.
///
/// Events are returned in emission order; the caller ships each one over
/// NCP with `kind: "event"`. The sequence always ends with a `grid.flush`
/// so the renderer commits atomically.
pub fn render_frame(state: &ChatState) -> Vec<Map<String, Value>> {
    let mut out: Vec<Map<String, Value>> = Vec::new();
    // Minimum sensible dims: we still honor whatever the TUI told us, but
    // refuse to render into a degenerate grid to keep math safe.
    let cols = state.dims.cols.max(1);
    let rows = state.dims.rows.max(2);
    let transcript_rows = rows - 1; // last row reserved for input
    let input_row = rows - 1;

    out.push(grid_clear());

    // Wrap every transcript entry. Each entry yields N wrapped lines,
    // each with a single highlight id (role-based).
    let wrapped = wrap_transcript(&state.transcript, cols as usize);
    let total = wrapped.len() as u32;

    // Position the viewport. `scroll_offset == 0` anchors the newest line
    // to the bottom; larger offsets walk upward.
    let (first_line_idx, transcript_start_row) =
        compute_viewport(total, transcript_rows, state.scroll_offset);

    // Render each visible wrapped line.
    for visible_row in 0..transcript_rows {
        let line_idx_u32 = first_line_idx.checked_add(visible_row);
        let row_to_paint = transcript_start_row + visible_row;
        match line_idx_u32.and_then(|i| wrapped.get(i as usize)) {
            Some(line) => out.push(grid_line(row_to_paint, cols, &line.text, line.hl_id)),
            None => out.push(grid_line_blank(row_to_paint, cols)),
        }
    }

    // Input line.
    let (input_text, cursor_col) =
        render_input_line(&state.input.as_string(), state.input.cursor(), cols);
    out.push(grid_line(input_row, cols, &input_text, HL_INPUT));
    out.push(grid_cursor_goto(input_row, cursor_col));

    out.push(grid_flush());
    out
}

/// Returned wrapped line + highlight assignment.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WrappedLine {
    text: String,
    hl_id: u32,
}

fn wrap_transcript(entries: &[TranscriptEntry], cols: usize) -> Vec<WrappedLine> {
    let mut out: Vec<WrappedLine> = Vec::new();
    for e in entries {
        let (prefix, hl) = match e.role {
            Role::User => ("you> ", HL_USER),
            Role::Assistant => ("claude> ", HL_ASSISTANT),
            Role::System => ("", HL_SYSTEM),
        };
        let full = format!(
            "{prefix}{}",
            if e.role == Role::System {
                // System entries wrap in brackets so tool starts / errors
                // are visually distinct from user prose.
                format!("[{}]", e.text)
            } else {
                e.text.clone()
            }
        );
        for line in wrap_to_width(&full, cols) {
            out.push(WrappedLine {
                text: line,
                hl_id: hl,
            });
        }
    }
    out
}

/// Compute (first_wrapped_line_index, transcript_start_row).
///
/// If the transcript is shorter than the transcript area, it sits at the
/// top (row 0) and the remaining rows are blanked. Otherwise we scroll
/// from the bottom — `scroll_offset == 0` shows the last `transcript_rows`
/// lines.
fn compute_viewport(total: u32, transcript_rows: u32, scroll_offset: u32) -> (u32, u32) {
    if total <= transcript_rows {
        // All content fits — no scrolling. Top of viewport anchored to
        // row 0.
        return (0, 0);
    }
    // There are `total - transcript_rows` rows hiding above the viewport
    // when scroll_offset == 0. Scrolling up reveals them.
    let max_offset = total - transcript_rows;
    let offset = scroll_offset.min(max_offset);
    let first = max_offset - offset;
    (first, 0)
}

/// Render the input line + compute cursor column.
///
/// Input line looks like `> buffer_contents`. When the buffer is longer
/// than the available space, we show a right-anchored window so the
/// cursor stays visible. Returns `(rendered_text, cursor_col)` where
/// `cursor_col` is clamped to `[2, cols-1]` (accounting for `> ` prefix
/// and avoiding the last cell, which ratatui tends to eat).
pub fn render_input_line(buffer: &str, cursor_char_offset: usize, cols: u32) -> (String, u32) {
    let prefix = "> ";
    let prefix_w = str_width(prefix) as u32;
    if cols <= prefix_w {
        // Degenerate: not enough space for even the prefix. Show whatever
        // fits and park cursor at column 0.
        let text: String = prefix.chars().take(cols as usize).collect();
        return (text, 0);
    }
    let budget = (cols - prefix_w) as usize;

    // Compute cursor's position measured in display columns from the
    // buffer's left edge.
    let mut cursor_col_in_buffer: usize = 0;
    for (i, c) in buffer.chars().enumerate() {
        if i >= cursor_char_offset {
            break;
        }
        cursor_col_in_buffer += crate::wrap::char_width(c);
    }

    let total_w = str_width(buffer);

    // Decide window. Simple heuristic: if the buffer fits, show the whole
    // thing. Otherwise, right-anchor so the cursor is near the right edge
    // (within `budget - 1`). If the cursor is near the start, left-
    // anchor instead.
    let (visible, cursor_in_visible): (String, usize) = if total_w <= budget {
        (buffer.to_owned(), cursor_col_in_buffer)
    } else if cursor_col_in_buffer + 1 >= budget {
        // Right-anchor: keep cursor at `budget - 1`. Trim from the left
        // until the cursor_col_in_buffer equals `budget - 1`.
        let drop_cols = cursor_col_in_buffer + 1 - budget;
        let (trimmed, trimmed_start_col) = drop_left_cols(buffer, drop_cols);
        (trimmed, cursor_col_in_buffer - trimmed_start_col)
    } else {
        // Left-anchor: take the first `budget` columns.
        let (taken, _) = take_cols(buffer, budget);
        (taken, cursor_col_in_buffer)
    };

    let line = format!("{prefix}{visible}");
    let cursor_col = prefix_w + cursor_in_visible as u32;
    // Never target the last column — ratatui's rightmost cell has flaky
    // cursor behavior across terminals. Clamp to cols-1 as a safety.
    let cursor_col = cursor_col.min(cols.saturating_sub(1));
    (line, cursor_col)
}

/// Drop `drop_cols` display columns from the left of `s`. Returns
/// `(remainder, dropped_col_count)` — the second element can be less
/// than `drop_cols` if wide chars force an uneven split.
fn drop_left_cols(s: &str, drop_cols: usize) -> (String, usize) {
    let mut dropped: usize = 0;
    let mut iter = s.chars();
    for c in iter.by_ref() {
        let w = crate::wrap::char_width(c);
        if dropped + w > drop_cols {
            // Put this char back by reconstructing from here.
            let mut rest = String::new();
            rest.push(c);
            rest.extend(iter);
            return (rest, dropped);
        }
        dropped += w;
    }
    (String::new(), dropped)
}

/// Take at most `budget` display columns from the start of `s`.
fn take_cols(s: &str, budget: usize) -> (String, usize) {
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = crate::wrap::char_width(c);
        if w + cw > budget {
            break;
        }
        out.push(c);
        w += cw;
    }
    (out, w)
}

// ---- grid event helpers ----------------------------------------------------

fn grid_clear() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("nefor-tui.grid.clear".into()));
    m.insert("grid".into(), Value::Number(1u32.into()));
    m
}

fn grid_flush() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("nefor-tui.grid.flush".into()));
    m
}

fn grid_cursor_goto(row: u32, col: u32) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String("nefor-tui.grid.cursor_goto".into()),
    );
    m.insert("grid".into(), Value::Number(1u32.into()));
    m.insert("row".into(), Value::Number(row.into()));
    m.insert("col".into(), Value::Number(col.into()));
    m
}

/// Emit a line event whose text is a single hl run, padded with spaces to
/// `cols` so the previous content at that row is overwritten.
fn grid_line(row: u32, cols: u32, text: &str, hl_id: u32) -> Map<String, Value> {
    let used = str_width(text) as u32;
    let padding = cols.saturating_sub(used);

    let mut cells: Vec<Value> = Vec::new();
    // First cell: the actual text run with our hl_id.
    {
        let cell = Value::Array(vec![
            Value::String(text.to_owned()),
            Value::Number(hl_id.into()),
        ]);
        cells.push(cell);
    }
    // Padding run at hl 0 (default). Use the `repeat` form so we send a
    // single array regardless of how much we need to fill.
    if padding > 0 {
        let cell = Value::Array(vec![
            Value::String(" ".into()),
            Value::Number(0u32.into()),
            Value::Number(padding.into()),
        ]);
        cells.push(cell);
    }

    let mut m = Map::new();
    m.insert("kind".into(), Value::String("nefor-tui.grid.line".into()));
    m.insert("grid".into(), Value::Number(1u32.into()));
    m.insert("row".into(), Value::Number(row.into()));
    m.insert("col_start".into(), Value::Number(0u32.into()));
    m.insert("cells".into(), Value::Array(cells));
    m
}

fn grid_line_blank(row: u32, cols: u32) -> Map<String, Value> {
    grid_line(row, cols, "", 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ChatState, Dims, Role};

    fn state_with(transcript: Vec<(Role, &str)>, input: &str, dims: Dims) -> ChatState {
        let mut s = ChatState::new();
        s.dims = dims;
        s.tui_ready = true;
        for (r, t) in transcript {
            s.push_entry(r, t.into());
        }
        s.input.insert_str(input);
        s
    }

    #[test]
    fn palette_defines_five_entries() {
        let defs = palette_defines();
        assert_eq!(defs.len(), 5);
        for d in &defs {
            assert_eq!(d["kind"], Value::String("nefor-tui.hl_attr_define".into()));
            assert!(d.get("id").is_some());
            assert!(d["rgb"].is_object());
        }
    }

    #[test]
    fn empty_transcript_emits_blank_rows_and_input() {
        let s = state_with(vec![], "", Dims { cols: 20, rows: 4 });
        let events = render_frame(&s);
        // clear + 3 transcript blanks + 1 input + cursor_goto + flush = 7
        assert_eq!(events.len(), 7);
        assert_eq!(
            events[0]["kind"],
            Value::String("nefor-tui.grid.clear".into())
        );
        // Last three are input, cursor_goto, flush.
        assert_eq!(
            events[events.len() - 1]["kind"],
            Value::String("nefor-tui.grid.flush".into())
        );
        assert_eq!(
            events[events.len() - 2]["kind"],
            Value::String("nefor-tui.grid.cursor_goto".into())
        );
    }

    #[test]
    fn user_and_assistant_pair_renders_with_prefixes() {
        let s = state_with(
            vec![(Role::User, "hello"), (Role::Assistant, "hi there")],
            "",
            Dims { cols: 40, rows: 6 },
        );
        let events = render_frame(&s);
        // Find the line events for transcript rows; row 0 is "you> hello"
        // once, row 1 is "claude> hi there". Rows 2..rows-1 are blank.
        let row0 = &events[1];
        let row1 = &events[2];
        let row2 = &events[3];
        assert_eq!(row0["row"], Value::Number(0u32.into()));
        let cells0 = row0["cells"].as_array().expect("cells array");
        assert_eq!(
            cells0[0][0],
            Value::String("you> hello".into()),
            "first row text"
        );
        assert_eq!(cells0[0][1], Value::Number(HL_USER.into()), "first row hl");
        let cells1 = row1["cells"].as_array().expect("cells");
        assert_eq!(cells1[0][0], Value::String("claude> hi there".into()));
        assert_eq!(cells1[0][1], Value::Number(HL_ASSISTANT.into()));
        let cells2 = row2["cells"].as_array().expect("cells");
        assert_eq!(cells2[0][0], Value::String("".into()));
    }

    #[test]
    fn system_entries_get_bracketed() {
        let s = state_with(
            vec![(Role::System, "tool: read")],
            "",
            Dims { cols: 30, rows: 3 },
        );
        let events = render_frame(&s);
        let row0 = &events[1];
        let cells = row0["cells"].as_array().expect("cells");
        assert_eq!(cells[0][0], Value::String("[tool: read]".into()));
        assert_eq!(cells[0][1], Value::Number(HL_SYSTEM.into()));
    }

    #[test]
    fn input_line_carries_cursor_goto() {
        let s = state_with(vec![], "hello", Dims { cols: 20, rows: 3 });
        let events = render_frame(&s);
        // Find the cursor_goto event.
        let goto = events
            .iter()
            .find(|e| e["kind"] == Value::String("nefor-tui.grid.cursor_goto".into()))
            .expect("cursor_goto emitted");
        // "> hello" → prefix is 2 chars, cursor at end of 5-char buffer → col 7
        assert_eq!(goto["col"], Value::Number(7u32.into()));
        assert_eq!(goto["row"], Value::Number(2u32.into())); // rows-1
    }

    #[test]
    fn long_user_entry_wraps() {
        let s = state_with(
            vec![(Role::User, "the quick brown fox jumps")],
            "",
            Dims { cols: 15, rows: 6 },
        );
        let events = render_frame(&s);
        // Transcript rows start at events[1]. First wrapped line should
        // contain "you> the quick" (14 chars, fits in 15).
        let row0 = &events[1];
        let cells = row0["cells"].as_array().expect("cells");
        assert_eq!(cells[0][0], Value::String("you> the quick".into()));
        let row1 = &events[2];
        let cells1 = row1["cells"].as_array().expect("cells");
        // Remaining "brown fox jumps" wraps.
        assert!(cells1[0][0].as_str().unwrap_or("").contains("brown"));
    }

    #[test]
    fn scrolled_view_reveals_older_content() {
        // 10 transcript rows worth of content into 3 transcript rows +
        // 1 input row. scroll_offset = 0 shows rows 7,8,9.
        // scroll_offset = 2 shows rows 5,6,7.
        let mut s = ChatState::new();
        s.dims = Dims { cols: 20, rows: 4 };
        for i in 0..10 {
            s.push_entry(Role::User, format!("msg {i}"));
        }
        let events = render_frame(&s);
        // events[1..4] are transcript rows 0,1,2. With no scroll, newest
        // (msg 9) should be in row 2 (just above input).
        let row_top = &events[1];
        let row_mid = &events[2];
        let row_bot = &events[3];
        assert_eq!(row_top["cells"][0][0], Value::String("you> msg 7".into()));
        assert_eq!(row_mid["cells"][0][0], Value::String("you> msg 8".into()));
        assert_eq!(row_bot["cells"][0][0], Value::String("you> msg 9".into()));

        // Scroll up by 2: should now show 5,6,7.
        s.scroll_up(2);
        let events = render_frame(&s);
        assert_eq!(events[1]["cells"][0][0], Value::String("you> msg 5".into()));
        assert_eq!(events[2]["cells"][0][0], Value::String("you> msg 6".into()));
        assert_eq!(events[3]["cells"][0][0], Value::String("you> msg 7".into()));
    }

    #[test]
    fn input_longer_than_cols_right_anchors_cursor() {
        // Buffer 30 chars, cols 10 → prefix "> " (2) + 8 visible. Cursor
        // at end (30) should land at col 9 (cols-1), never col 10.
        let s = state_with(
            vec![],
            "0123456789abcdefghijklmnopqrst",
            Dims { cols: 10, rows: 3 },
        );
        let events = render_frame(&s);
        let goto = events
            .iter()
            .find(|e| e["kind"] == Value::String("nefor-tui.grid.cursor_goto".into()))
            .expect("cursor_goto");
        assert_eq!(goto["col"], Value::Number(9u32.into()));
    }

    #[test]
    fn input_cursor_at_start_left_anchors() {
        let mut s = state_with(vec![], "", Dims { cols: 10, rows: 3 });
        for c in "abcdefghijkl".chars() {
            s.input.insert_char(c);
        }
        s.input.cursor_home();
        let events = render_frame(&s);
        let goto = events
            .iter()
            .find(|e| e["kind"] == Value::String("nefor-tui.grid.cursor_goto".into()))
            .expect("cursor_goto");
        assert_eq!(goto["col"], Value::Number(2u32.into()));
    }

    #[test]
    fn render_input_line_short_buffer() {
        let (line, col) = render_input_line("abc", 3, 10);
        assert_eq!(line, "> abc");
        assert_eq!(col, 5);
    }

    #[test]
    fn render_input_line_empty_buffer_cursor_at_two() {
        let (line, col) = render_input_line("", 0, 10);
        assert_eq!(line, "> ");
        assert_eq!(col, 2);
    }

    #[test]
    fn render_input_line_degenerate_cols() {
        // cols smaller than prefix → truncate prefix, park cursor at 0.
        let (_, col) = render_input_line("x", 1, 1);
        assert_eq!(col, 0);
    }

    #[test]
    fn full_frame_ends_with_flush() {
        let s = state_with(vec![], "", Dims { cols: 10, rows: 3 });
        let events = render_frame(&s);
        assert_eq!(
            events.last().expect("non-empty")["kind"],
            Value::String("nefor-tui.grid.flush".into())
        );
    }

    #[test]
    fn streaming_assistant_text_renders_like_regular() {
        let mut s = ChatState::new();
        s.dims = Dims { cols: 30, rows: 4 };
        s.tui_ready = true;
        s.append_assistant_delta("part ");
        s.append_assistant_delta("two");
        let events = render_frame(&s);
        // Should see "claude> part two" on first visible transcript row.
        let row_with_text = events
            .iter()
            .find(|e| {
                e["kind"] == Value::String("nefor-tui.grid.line".into())
                    && e["cells"][0][0].as_str().unwrap_or("").contains("claude>")
            })
            .expect("assistant row present");
        assert!(row_with_text["cells"][0][0]
            .as_str()
            .unwrap_or("")
            .contains("part two"));
    }
}
