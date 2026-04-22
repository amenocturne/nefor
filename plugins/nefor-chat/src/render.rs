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
    // Accent colors only for HL_USER and HL_SYSTEM; body text (assistant,
    // input) uses terminal defaults so the chat blends with whatever theme
    // the user has.
    vec![
        hl_attr_define(HL_USER, Some(0x7FB4FF), None, true, false),
        hl_attr_define(HL_ASSISTANT, None, None, false, false),
        hl_attr_define(HL_SYSTEM, Some(0x808080), None, false, true),
        hl_attr_define(HL_INPUT, None, None, false, false),
        hl_attr_define(HL_STATUS, Some(0x808080), None, false, true),
    ]
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

    // Compute input block: wrapped lines + cursor position within them.
    // The input grows vertically as the user types past `cols`, pushing the
    // transcript upward. Capped so that transcript + status bar still keep
    // at least one row between them.
    let (input_lines, cursor_line_in_input, cursor_col) =
        render_input_wrapped(&state.input.as_string(), state.input.cursor(), cols);

    // Reserve a 1-row status bar between the transcript and the input when
    // we have enough vertical space. `rows < 3` is pathologically small; we
    // drop the status bar in that case so the input+transcript still fit.
    let status_height: u32 = if rows >= 3 { 1 } else { 0 };

    let max_input_rows = rows.saturating_sub(1 + status_height).max(1);
    let input_height = (input_lines.len() as u32).clamp(1, max_input_rows);
    let transcript_rows = rows - input_height - status_height;
    let status_row = transcript_rows;
    let input_start_row = transcript_rows + status_height;

    // If input overflows the reserved height, scroll it so the cursor line
    // stays visible (anchored to the bottom of the input block).
    let input_scroll = (input_lines.len() as u32).saturating_sub(input_height);
    let cursor_line_visible = cursor_line_in_input
        .saturating_sub(input_scroll)
        .min(input_height.saturating_sub(1));

    out.push(grid_clear());

    // Wrap every transcript entry. Each entry yields N wrapped lines,
    // each with a single highlight id (role-based).
    let mut wrapped = wrap_transcript(&state.transcript, cols as usize);
    // In-flight turn indicator. We only show it until the first delta
    // arrives (which opens a streaming assistant entry); once Claude is
    // visibly typing, the placeholder becomes redundant.
    if state.pending && !last_is_streaming_assistant(&state.transcript) {
        for line in wrap_to_width("[claude is thinking...]", cols as usize) {
            wrapped.push(WrappedLine {
                text: line,
                hl_id: HL_SYSTEM,
            });
        }
    }
    let total = wrapped.len() as u32;

    // Clamp scroll so the user can't scroll past the oldest content. When
    // total fits in the viewport there's no valid scroll — max = 0.
    let max_offset = total.saturating_sub(transcript_rows);
    let effective_scroll = state.scroll_offset.min(max_offset);

    // Position the viewport. `scroll_offset == 0` anchors the newest line
    // to the bottom; larger offsets walk upward.
    let (first_line_idx, transcript_start_row) =
        compute_viewport(total, transcript_rows, effective_scroll);

    // Render each visible wrapped line.
    for visible_row in 0..transcript_rows {
        let line_idx_u32 = first_line_idx.checked_add(visible_row);
        let row_to_paint = transcript_start_row + visible_row;
        match line_idx_u32.and_then(|i| wrapped.get(i as usize)) {
            Some(line) => out.push(grid_line(row_to_paint, cols, &line.text, line.hl_id)),
            None => out.push(grid_line_blank(row_to_paint, cols)),
        }
    }

    // Status bar: one row, HL_STATUS, showing scroll position + hint.
    if status_height > 0 {
        let text = status_bar_text(total, transcript_rows, effective_scroll, cols);
        out.push(grid_line(status_row, cols, &text, HL_STATUS));
    }

    // Input block: emit one grid.line per visible wrapped line.
    for i in 0..input_height {
        let src_idx = (input_scroll + i) as usize;
        let text = input_lines.get(src_idx).map(String::as_str).unwrap_or("");
        out.push(grid_line(input_start_row + i, cols, text, HL_INPUT));
    }
    // Cursor lands on the wrapped line that contains `cursor_char_offset`.
    // `cursor_col` is clamped in `render_input_wrapped`.
    out.push(grid_cursor_goto(
        input_start_row + cursor_line_visible,
        cursor_col,
    ));

    out.push(grid_flush());
    out
}

/// Returned wrapped line + highlight assignment.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WrappedLine {
    text: String,
    hl_id: u32,
}

/// Build the one-row status bar that sits between the transcript and the
/// input. Shows scroll position as a percentage plus a short hint about
/// hidden content. Truncated to fit `cols` so it never forces a wrap.
///
/// Percentage convention (from the user request): `100%` = pinned to the
/// newest content (bottom), `0%` = top of history. When the transcript is
/// short enough to fit entirely in the viewport, we still report `100%`
/// since there's nothing to scroll away from.
fn status_bar_text(total: u32, transcript_rows: u32, scroll_offset: u32, cols: u32) -> String {
    let max_offset = total.saturating_sub(transcript_rows);
    let (percent, hint) = if max_offset == 0 {
        (100u32, format!("{total} lines"))
    } else if scroll_offset == 0 {
        (100u32, format!("↓ bottom · {total} lines"))
    } else if scroll_offset >= max_offset {
        (0u32, format!("↑ top · {total} lines · PgDn to return"))
    } else {
        let remaining_below = scroll_offset;
        let pct = ((max_offset - scroll_offset) as u64 * 100 / max_offset as u64) as u32;
        (pct, format!("{remaining_below} below · PgDn to return"))
    };
    let text = format!(" {percent}%  ·  {hint}");
    // Truncate by display columns, never by bytes.
    let mut out = String::new();
    let mut w = 0;
    for ch in text.chars() {
        let cw = crate::wrap::char_width(ch);
        if w + cw > cols as usize {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out
}

fn last_is_streaming_assistant(entries: &[TranscriptEntry]) -> bool {
    entries
        .last()
        .is_some_and(|e| e.role == Role::Assistant && e.streaming)
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

/// Render the input line as *wrapped* rows plus cursor position.
///
/// The input is laid out as `> buffer`, hard-wrapped every `cols` display
/// columns. Returns `(wrapped_lines, cursor_line_index, cursor_col)` where
/// `cursor_line_index` is 0-based into the returned vec and `cursor_col`
/// is the display column on that line (clamped to `cols - 1` to dodge the
/// last-cell cursor flake). The caller decides how many lines fit in the
/// reserved input block and how to scroll when overflow occurs.
///
/// Why hard-wrap (not word-wrap): the user is typing. If we broke at the
/// last whitespace, the cursor would suddenly jump rows as spaces enter or
/// leave the buffer. Hard-wrap keeps cursor motion continuous.
pub fn render_input_wrapped(
    buffer: &str,
    cursor_char_offset: usize,
    cols: u32,
) -> (Vec<String>, u32, u32) {
    let prefix = "> ";
    let prefix_w = str_width(prefix);
    let cols_usize = cols as usize;

    if cols_usize == 0 {
        return (vec![String::new()], 0, 0);
    }
    if cols_usize <= prefix_w {
        // Degenerate: not enough room for the prefix. Show what fits and
        // park the cursor at column 0.
        let text: String = prefix.chars().take(cols_usize).collect();
        return (vec![text], 0, 0);
    }

    // Build the full visible string (prefix + buffer) and hard-wrap by
    // display columns.
    let full = format!("{prefix}{buffer}");
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_w = 0usize;
    for c in full.chars() {
        let cw = crate::wrap::char_width(c);
        if current_w + cw > cols_usize && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            current_w = 0;
        }
        current.push(c);
        current_w += cw;
    }
    lines.push(current);

    // Cursor's absolute display column from the start of the full string
    // (past the prefix).
    let cursor_display_col = prefix_w
        + buffer
            .chars()
            .take(cursor_char_offset)
            .map(crate::wrap::char_width)
            .sum::<usize>();
    let cursor_line = (cursor_display_col / cols_usize) as u32;
    let cursor_col = (cursor_display_col % cols_usize) as u32;
    // Clamp to cols-1 to avoid the rightmost cell that some terminals
    // render inconsistently under ratatui.
    let cursor_col = cursor_col.min(cols.saturating_sub(1));

    (lines, cursor_line, cursor_col)
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

/// Emit a line event. Each cell in the `cells` array represents ONE grid
/// column (the NCP schema is cell-per-column, not run-per-string), so we
/// iterate the text by character and emit one entry per char. The first
/// cell carries `hl_id`; subsequent cells inherit it per spec. The row is
/// padded with spaces via the `repeat` form so prior content is cleared.
fn grid_line(row: u32, cols: u32, text: &str, hl_id: u32) -> Map<String, Value> {
    let used = str_width(text) as u32;
    let padding = cols.saturating_sub(used);

    let mut cells: Vec<Value> = Vec::new();
    let mut first = true;
    for ch in text.chars() {
        let mut buf = [0u8; 4];
        let s = ch.encode_utf8(&mut buf).to_owned();
        let cell = if first {
            first = false;
            Value::Array(vec![Value::String(s), Value::Number(hl_id.into())])
        } else {
            Value::Array(vec![Value::String(s)])
        };
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

    /// Reconstruct the visible text of a `grid.line` event by concatenating
    /// each content cell's `[0]` field. Each cell entry is one column; the
    /// final padding entry has `[text, hl, repeat]` (3 elements) and is
    /// skipped.
    fn row_text(row: &Map<String, Value>) -> String {
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

    /// First cell's `hl_id` (the first cell of each row carries the run's
    /// highlight; subsequent cells inherit it per spec).
    fn row_hl(row: &Map<String, Value>) -> u64 {
        let cells = row["cells"].as_array().expect("cells");
        cells[0][1].as_u64().expect("hl_id on first cell")
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
        assert_eq!(row_text(row0), "you> hello", "first row text");
        assert_eq!(row_hl(row0), HL_USER as u64, "first row hl");
        assert_eq!(row_text(row1), "claude> hi there");
        assert_eq!(row_hl(row1), HL_ASSISTANT as u64);
        // Blank rows emit just the padding run; reconstructed text is empty.
        assert_eq!(row_text(row2), "");
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
        assert_eq!(row_text(row0), "[tool: read]");
        assert_eq!(row_hl(row0), HL_SYSTEM as u64);
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
        assert_eq!(row_text(row0), "you> the quick");
        // Remaining "brown fox jumps" wraps to row 1.
        assert!(row_text(&events[2]).contains("brown"));
    }

    #[test]
    fn scrolled_view_reveals_older_content() {
        // rows=5 → 1 row input + 1 row status + 3 rows transcript. With
        // 10 entries, scroll_offset=0 shows the last three (msg 7..9).
        let mut s = ChatState::new();
        s.dims = Dims { cols: 20, rows: 5 };
        for i in 0..10 {
            s.push_entry(Role::User, format!("msg {i}"));
        }
        let events = render_frame(&s);
        // events[1..4] are transcript rows 0,1,2.
        let row_top = &events[1];
        let row_mid = &events[2];
        let row_bot = &events[3];
        assert_eq!(row_text(row_top), "you> msg 7");
        assert_eq!(row_text(row_mid), "you> msg 8");
        assert_eq!(row_text(row_bot), "you> msg 9");

        // Scroll up by 2: should now show 5,6,7.
        s.scroll_up(2);
        let events = render_frame(&s);
        assert_eq!(row_text(&events[1]), "you> msg 5");
        assert_eq!(row_text(&events[2]), "you> msg 6");
        assert_eq!(row_text(&events[3]), "you> msg 7");
    }

    #[test]
    fn status_bar_pins_to_bottom_when_not_scrolled() {
        let mut s = ChatState::new();
        s.dims = Dims { cols: 40, rows: 6 };
        for i in 0..20 {
            s.push_entry(Role::User, format!("msg {i}"));
        }
        let events = render_frame(&s);
        // rows=6 → 1 input + 1 status + 4 transcript. Status row = 4.
        let status = events
            .iter()
            .find(|e| {
                e["kind"] == Value::String("nefor-tui.grid.line".into())
                    && e["row"] == Value::Number(4u32.into())
            })
            .expect("status row present");
        let text = row_text(status);
        assert!(
            text.contains("100%"),
            "expected 100% at bottom, got: {text:?}"
        );
        assert!(
            text.contains("bottom"),
            "expected 'bottom' label, got: {text:?}"
        );
    }

    #[test]
    fn status_bar_shows_zero_percent_at_top() {
        let mut s = ChatState::new();
        s.dims = Dims { cols: 40, rows: 6 };
        for i in 0..20 {
            s.push_entry(Role::User, format!("msg {i}"));
        }
        s.scroll_up(1000); // far past max; clamped at render
        let events = render_frame(&s);
        let status = events
            .iter()
            .find(|e| {
                e["kind"] == Value::String("nefor-tui.grid.line".into())
                    && e["row"] == Value::Number(4u32.into())
            })
            .expect("status row present");
        let text = row_text(status);
        assert!(
            text.contains("0%") && !text.contains("100%"),
            "expected 0% at top, got: {text:?}"
        );
        assert!(text.contains("top"), "expected 'top' label, got: {text:?}");
    }

    #[test]
    fn input_wraps_to_multiple_rows_when_buffer_overflows() {
        // cols 10, rows 5 → max_input_rows = 4. Buffer "0..29" + prefix "> "
        // = 32 display columns, hard-wrapped: 4 rows. Last row at `rows-1`.
        let s = state_with(
            vec![],
            "0123456789abcdefghijklmnopqrst",
            Dims { cols: 10, rows: 5 },
        );
        let events = render_frame(&s);
        // Find every grid.line event whose row is in the input block
        // (rows `rows - input_height`..`rows`). input_height should be 4.
        let line_rows: Vec<u64> = events
            .iter()
            .filter(|e| e["kind"] == Value::String("nefor-tui.grid.line".into()))
            .filter_map(|e| e["row"].as_u64())
            .collect();
        // Transcript rows 0, input rows 1..=4. We expect at least 4 input
        // rows emitted (plus transcript rows above).
        let input_rows_emitted = line_rows.iter().filter(|r| **r >= 1).count();
        assert!(
            input_rows_emitted >= 4,
            "expected input to span >= 4 rows, got rows {line_rows:?}"
        );
        // Cursor is at end of 30-char buffer → display col 32 → row offset
        // 3 from top of input, col 2.
        let goto = events
            .iter()
            .find(|e| e["kind"] == Value::String("nefor-tui.grid.cursor_goto".into()))
            .expect("cursor_goto");
        assert_eq!(goto["col"], Value::Number(2u32.into()));
        // Last row of input block = rows-1 = 4.
        assert_eq!(goto["row"], Value::Number(4u32.into()));
    }

    #[test]
    fn input_cursor_at_start_sits_on_first_wrapped_line() {
        let mut s = state_with(vec![], "", Dims { cols: 10, rows: 5 });
        for c in "abcdefghijkl".chars() {
            s.input.insert_char(c);
        }
        s.input.cursor_home();
        let events = render_frame(&s);
        let goto = events
            .iter()
            .find(|e| e["kind"] == Value::String("nefor-tui.grid.cursor_goto".into()))
            .expect("cursor_goto");
        // Cursor at buffer start → display col 2, on first input row.
        // input_height=2, transcript_rows=3 → first input row = 3.
        assert_eq!(goto["col"], Value::Number(2u32.into()));
        assert_eq!(goto["row"], Value::Number(3u32.into()));
    }

    #[test]
    fn render_input_wrapped_short_buffer_single_line() {
        let (lines, cline, col) = render_input_wrapped("abc", 3, 10);
        assert_eq!(lines, vec!["> abc".to_string()]);
        assert_eq!(cline, 0);
        assert_eq!(col, 5);
    }

    #[test]
    fn render_input_wrapped_empty_buffer_cursor_at_prefix_end() {
        let (lines, cline, col) = render_input_wrapped("", 0, 10);
        assert_eq!(lines, vec!["> ".to_string()]);
        assert_eq!(cline, 0);
        assert_eq!(col, 2);
    }

    #[test]
    fn render_input_wrapped_wraps_at_cols_boundary() {
        // cols=10 → each line holds 10 display cols. "> " = 2 cols, so the
        // first line fits "> " + 8 buffer chars. Buffer of 15 chars →
        // 17 display cols → 2 lines (10 + 7).
        let (lines, cline, col) = render_input_wrapped("abcdefghijklmno", 15, 10);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "> abcdefgh");
        assert_eq!(lines[1], "ijklmno");
        // Cursor at offset 15 → display col 17 → row 1, col 7.
        assert_eq!(cline, 1);
        assert_eq!(col, 7);
    }

    #[test]
    fn render_input_wrapped_degenerate_cols_below_prefix() {
        // cols=1 → no room for the full prefix. Show what fits, park cursor.
        let (lines, cline, col) = render_input_wrapped("x", 1, 1);
        assert_eq!(cline, 0);
        assert_eq!(col, 0);
        assert_eq!(lines.len(), 1);
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
                    && row_text(e).contains("claude>")
            })
            .expect("assistant row present");
        assert!(row_text(row_with_text).contains("part two"));
    }
}
