//! State → NCP grid events.
//!
//! Pure functions: given a [`ChatState`], produce the ordered list of
//! `nefor-tui.*` event bodies that redraw the frame. The main loop wraps
//! each body in a [`PluginOutgoing::event`] and hands it to the stdout
//! writer.
//!
//! Render strategy v1: full redraw per state change. The helper builds the
//! sequence `clear → line*(transcript) → status → line*(input) → cursor_goto
//! → flush` and emits a complete frame each time. Optimise later if needed.
//!
//! Highlights are expressed as per-character spans: each visible row is a
//! `Vec<Span>` where each span is `(text, hl_id)`. The grid emitter expands
//! a span sequence into one cell per grapheme so the renderer can pick up
//! markdown bold/italic/code styling without a separate "rich-text" event.

use std::cell::RefCell;

use serde_json::{Map, Value};

use std::collections::HashMap;

use crate::state::{
    AuthStatus, ChatState, DagNodeStatus, DagRunUiState, Popup, Role, RowSnapshot, SessionMetadata,
    TranscriptEntry,
};
use crate::wrap::{char_width, str_width, wrap_to_width};

// Thread-local cache for the wrapped+markdown-rendered transcript. Reused
// across renders that don't change `transcript_version` (e.g. keystrokes
// in the input buffer). Pulldown-cmark + per-line wrap is the dominant
// per-frame cost; without this every keypress re-parses every assistant
// message, producing visible typing latency on non-trivial transcripts.
thread_local! {
    static TRANSCRIPT_CACHE: RefCell<Option<TranscriptCache>> = const { RefCell::new(None) };
}

struct TranscriptCache {
    version: u64,
    cols: u32,
    pending: bool,
    /// Elapsed seconds shown in the placeholder ("[thinking… Ns]"). Part
    /// of the cache key so the counter advances cleanly on the 1s tick
    /// without re-parsing markdown for the rest of the transcript.
    pending_seconds: Option<u64>,
    lines: Vec<Line>,
}

/// Compute the wrapped + markdown-parsed transcript lines, consulting the
/// thread-local cache. Cache key includes the live `pending_seconds` so
/// the placeholder counter ticks each second; transcript-shape misses
/// recompute markdown, second-only misses recompute just the placeholder.
fn wrapped_with_cache(state: &ChatState, cols: u32) -> Vec<Line> {
    let pending = state.pending && !last_is_streaming_assistant(&state.transcript);
    // Counter only advances while pending — it'd be confusing for a stale
    // "Ns" to ride along on a non-pending render.
    let pending_seconds = if pending {
        state.pending_seconds_at(std::time::Instant::now())
    } else {
        None
    };

    let hit = TRANSCRIPT_CACHE.with(|cell| {
        let c = cell.borrow();
        c.as_ref()
            .filter(|c| {
                c.version == state.transcript_version
                    && c.cols == cols
                    && c.pending == pending
                    && c.pending_seconds == pending_seconds
            })
            .map(|c| c.lines.clone())
    });
    if let Some(lines) = hit {
        return lines;
    }

    let mut wrapped = wrap_transcript(&state.transcript, cols as usize, state.tools_expanded_global);
    if pending {
        if !wrapped.is_empty() {
            wrapped.push(Vec::new());
        }
        let placeholder = match pending_seconds {
            Some(0) | None => "[thinking...]".to_owned(),
            Some(s) => format!("[thinking... {s}s]"),
        };
        for line in wrap_to_width(&placeholder, cols as usize) {
            wrapped.push(vec![Span::new(line, HL_SYSTEM)]);
        }
    }

    TRANSCRIPT_CACHE.with(|cell| {
        *cell.borrow_mut() = Some(TranscriptCache {
            version: state.transcript_version,
            cols,
            pending,
            pending_seconds,
            lines: wrapped.clone(),
        });
    });

    wrapped
}

// ---- palette ---------------------------------------------------------------
//
// Highlight IDs we assign. 0 is the default (engine-managed); we pick small
// positive integers for our palette and emit `hl_attr_define` for each at
// startup via [`palette_defines`].

pub const HL_USER: u32 = 1;
pub const HL_ASSISTANT: u32 = 2;
pub const HL_SYSTEM: u32 = 3;
pub const HL_INPUT: u32 = 4;
pub const HL_STATUS: u32 = 5;
pub const HL_STATUS_DIM: u32 = 6;
pub const HL_STATUS_WARN: u32 = 7;
pub const HL_STATUS_DANGER: u32 = 8;
pub const HL_STATUS_BAR_FILL: u32 = 9;
pub const HL_MD_HEADING: u32 = 10;
pub const HL_MD_BOLD: u32 = 11;
pub const HL_MD_ITALIC: u32 = 12;
pub const HL_MD_CODE_INLINE: u32 = 13;
pub const HL_MD_CODE_BLOCK: u32 = 14;
pub const HL_MD_LIST_MARKER: u32 = 15;
pub const HL_MD_LINK: u32 = 16;
pub const HL_MD_QUOTE_BAR: u32 = 17;
pub const HL_FOOTER: u32 = 19;
pub const HL_STATUS_INFO: u32 = 20;
pub const HL_STATUS_OK: u32 = 21;

/// Width of the vertical separator drawn between the chat pane and the
/// sidebar. One column of `│` in `HL_FOOTER`. Subtracted from the sidebar's
/// reserved width when computing widget content width.
pub const SIDEBAR_SEPARATOR_COLS: usize = 1;

pub fn palette_defines() -> Vec<Map<String, Value>> {
    vec![
        hl_attr_define(HL_USER, Some(0x7FB4FF), None, true, false, false),
        hl_attr_define(HL_ASSISTANT, None, None, false, false, false),
        hl_attr_define(HL_SYSTEM, Some(0x808080), None, false, true, false),
        hl_attr_define(HL_INPUT, None, None, false, false, false),
        hl_attr_define(HL_STATUS, Some(0x808080), None, false, true, false),
        hl_attr_define(HL_STATUS_DIM, Some(0x606060), None, false, false, false),
        hl_attr_define(HL_STATUS_WARN, Some(0xD7AF5F), None, false, false, false),
        hl_attr_define(HL_STATUS_DANGER, Some(0xD75F5F), None, false, false, false),
        hl_attr_define(
            HL_STATUS_BAR_FILL,
            Some(0x7FB4FF),
            None,
            false,
            false,
            false,
        ),
        hl_attr_define(HL_MD_HEADING, Some(0xFFB86C), None, true, false, false),
        hl_attr_define(HL_MD_BOLD, None, None, true, false, false),
        hl_attr_define(HL_MD_ITALIC, None, None, false, true, false),
        hl_attr_define(
            HL_MD_CODE_INLINE,
            Some(0xC0C0C0),
            Some(0x303030),
            false,
            false,
            false,
        ),
        hl_attr_define(
            HL_MD_CODE_BLOCK,
            Some(0xC0C0C0),
            Some(0x202020),
            false,
            false,
            false,
        ),
        hl_attr_define(HL_MD_LIST_MARKER, Some(0x7FB4FF), None, false, false, false),
        hl_attr_define(HL_MD_LINK, Some(0x7FB4FF), None, false, false, true),
        hl_attr_define(HL_MD_QUOTE_BAR, Some(0x808080), None, false, true, false),
        hl_attr_define(HL_FOOTER, Some(0x707070), None, false, false, false),
        hl_attr_define(HL_STATUS_INFO, Some(0x7FB4FF), None, false, false, false),
        hl_attr_define(HL_STATUS_OK, Some(0x87D787), None, false, false, false),
    ]
}

fn hl_attr_define(
    id: u32,
    fg: Option<u32>,
    bg: Option<u32>,
    bold: bool,
    italic: bool,
    underline: bool,
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
    if underline {
        rgb.insert("underline".into(), Value::Bool(true));
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

// ---- spans -----------------------------------------------------------------

/// One run of text sharing a single highlight id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub hl: u32,
}

impl Span {
    pub fn new(text: impl Into<String>, hl: u32) -> Self {
        Self {
            text: text.into(),
            hl,
        }
    }

    fn width(&self) -> usize {
        str_width(&self.text)
    }
}

/// One visual row: ordered spans whose total display-width fits within the
/// renderer's column budget.
type Line = Vec<Span>;

// Visual breathing room around chat content. HPAD applies to transcript and
// input rows (status fills its row, no horizontal pad). VPAD is a single
// blank row at the top of the transcript and a single blank row beneath the
// status bar when there's space.
const HPAD: u32 = 2;
const VPAD: u32 = 1;
const MAX_INPUT_ROWS: u32 = 6;

/// Cap for the inline slash-autocomplete band. The renderer reserves at most
/// this many rows directly above the input top bar; longer match lists scroll
/// inside this window via the `scroll` field on `Popup::SlashAutocomplete`.
pub(crate) const MAX_INLINE_AUTOCOMPLETE_ROWS: u32 = 8;

// ---- frame construction ----------------------------------------------------

pub fn render_frame(state: &mut ChatState) -> Vec<Map<String, Value>> {
    let cols = state.dims.cols.max(1);
    let rows = state.dims.rows.max(2);

    // Right sidebar reservation. Auto-hides when the user toggled it off,
    // when the terminal is too narrow, or when there's no widget content
    // (the chat reclaims those columns rather than leaving an empty pane).
    // See `ChatState::sidebar_width` for the full decision tree.
    let sidebar_w = state.sidebar_width().min(cols.saturating_sub(1));
    let chat_cols = cols.saturating_sub(sidebar_w);

    let hpad = if chat_cols > 2 * HPAD { HPAD } else { 0 };
    let inner_cols = chat_cols - 2 * hpad;

    let status_height: u32 = if rows >= 3 { 1 } else { 0 };

    // Drop the top / bottom VPAD on tight terminals — minimum required is
    // one transcript row + one input row, plus status if it's already on.
    // Each VPAD only turns on once there's slack beyond that floor.
    let vpad_budget = rows.saturating_sub(2 + status_height);
    let vpad_top: u32 = if vpad_budget >= 1 { VPAD } else { 0 };
    let vpad_bottom: u32 = if vpad_budget >= 2 && status_height > 0 {
        VPAD
    } else {
        0
    };

    let (input_lines, cursor_line_in_input, cursor_col) =
        render_input_wrapped(&state.input.as_string(), state.input.cursor(), chat_cols, hpad);

    // Layout, top → bottom: vpad_top · transcript · vpad_input_top ·
    // input_top_bar · input · input_bot_bar · status · vpad_bottom. Status
    // sits directly under the input bottom rule; the optional `vpad_bottom`
    // is a blank row below status (the absolute last row of the terminal).
    // Two optional decorations above status:
    //   - `vpad_input_top` (1 row): blank gutter between transcript and input.
    //   - `input_bars` (2 rows): `╭─` top + `╰─` bottom around the input.
    // Both fire only when there's slack — at least one transcript row must
    // survive after every floor item.
    let gutter_floor = vpad_top + 1 + 1 + vpad_bottom + status_height + 1;
    let vpad_input_top: u32 = if rows >= gutter_floor { 1 } else { 0 };
    let bar_floor = vpad_top + vpad_input_top + 2 + 1 + vpad_bottom + status_height + 1;
    let input_bars: u32 = if rows >= bar_floor { 2 } else { 0 };
    let max_input_rows = rows
        .saturating_sub(vpad_top + vpad_input_top + input_bars + 1 + vpad_bottom + status_height)
        .clamp(1, MAX_INPUT_ROWS);
    let input_height = (input_lines.len() as u32).clamp(1, max_input_rows);

    // Slash autocomplete band — inline strip directly above the input top
    // bar when `Popup::SlashAutocomplete` is open. Steals rows from the
    // transcript budget so the input box stays put. Capped at
    // `MAX_INLINE_AUTOCOMPLETE_ROWS`; never grows past whatever rows are
    // left over after the input + status floor (so a tight terminal can
    // still render the input).
    let autocomplete_height: u32 = match &state.popup {
        Some(Popup::SlashAutocomplete { matches, .. }) => {
            let want = (matches.len() as u32).clamp(1, MAX_INLINE_AUTOCOMPLETE_ROWS);
            let floor = vpad_top + vpad_input_top + input_bars + input_height + vpad_bottom + status_height + 1;
            let avail = rows.saturating_sub(floor);
            want.min(avail)
        }
        _ => 0,
    };

    // The DAG panel that used to live between the input box and the
    // statusline is gone — it moved into the right sidebar (see
    // `crate::sidebar`). Transcript-row math therefore no longer subtracts
    // a panel height; the only chrome between the input box and the status
    // row is the input bottom bar itself.
    let transcript_rows = rows
        - vpad_top
        - vpad_input_top
        - input_bars
        - input_height
        - vpad_bottom
        - status_height
        - autocomplete_height;
    let transcript_top_row = vpad_top;
    let autocomplete_top_row = vpad_top + transcript_rows;
    let input_top_bar_row = autocomplete_top_row + autocomplete_height + vpad_input_top;
    let input_start_row = input_top_bar_row + if input_bars > 0 { 1 } else { 0 };
    let input_bot_bar_row = input_start_row + input_height;
    let status_row = rows - 1 - vpad_bottom;

    let content_rows = transcript_rows;

    // Scroll the input window so the cursor stays visible. Bottom-anchored
    // by default (cursor typically at end); if the cursor sits above the
    // bottom-anchored window — say the user navigated back through a long
    // multi-line buffer — the window slides up to keep it on-screen.
    let total_input_lines = input_lines.len() as u32;
    let end = (cursor_line_in_input + 1)
        .max(input_height)
        .min(total_input_lines);
    let input_scroll = end.saturating_sub(input_height);
    let cursor_line_visible = cursor_line_in_input
        .saturating_sub(input_scroll)
        .min(input_height.saturating_sub(1));

    let wrapped = wrapped_with_cache(state, inner_cols);
    let total = wrapped.len() as u32;

    // Auto-follow rule: when the user is scrolled up (scroll_offset > 0)
    // and content has grown since the last render, bump scroll_offset by
    // the same delta so the absolute viewport range stays put. When the
    // user is pinned to the bottom (scroll_offset == 0), the bottom-anchored
    // viewport math naturally follows new content without changes here.
    if let Some(prev) = state.last_wrapped_total {
        if state.scroll_offset > 0 && total > prev {
            let delta = total - prev;
            state.scroll_offset = state.scroll_offset.saturating_add(delta);
        }
    }
    state.last_wrapped_total = Some(total);

    let max_offset = total.saturating_sub(content_rows);
    state.scroll_offset = state.scroll_offset.min(max_offset);
    let effective_scroll = state.scroll_offset;

    let (first_line_idx, transcript_start_row) =
        compute_viewport(total, content_rows, effective_scroll);

    // Build the full intended frame in memory first. Each entry is the
    // exact span sequence for its row index. Diffing against the
    // last-emitted snapshots lives below — keeping construction and
    // emission separate makes the diff loop self-contained.
    let mut frame: Vec<(u32, Vec<Span>)> = Vec::with_capacity(rows as usize);

    if vpad_top > 0 {
        for r in 0..vpad_top {
            frame.push((r, blank_row_spans()));
        }
    }

    let content_top_row = transcript_top_row;
    for visible_row in 0..content_rows {
        let line_idx_u32 = first_line_idx.checked_add(visible_row);
        let row_to_paint = content_top_row + transcript_start_row + visible_row;
        let spans = match line_idx_u32.and_then(|i| wrapped.get(i as usize)) {
            Some(line) => pad_left(line.clone(), hpad),
            None => blank_row_spans(),
        };
        frame.push((row_to_paint, spans));
    }

    // Inline slash-autocomplete band sits directly above the input top
    // bar, stealing rows from the transcript budget. See the layout math
    // above for the height calculation.
    if autocomplete_height > 0 {
        if let Some(Popup::SlashAutocomplete {
            matches,
            cursor,
            scroll,
        }) = state.popup.clone()
        {
            let rows_out = render_inline_slash_autocomplete(
                &matches,
                cursor,
                scroll,
                inner_cols as usize,
                autocomplete_height as usize,
            );
            for (i, row_spans) in rows_out.into_iter().enumerate() {
                if (i as u32) >= autocomplete_height {
                    break;
                }
                frame.push((
                    autocomplete_top_row + i as u32,
                    pad_left(row_spans, hpad),
                ));
            }
        }
    }

    if vpad_input_top > 0 {
        let gutter_row = autocomplete_top_row + autocomplete_height;
        for r in 0..vpad_input_top {
            frame.push((gutter_row + r, blank_row_spans()));
        }
    }

    if input_bars > 0 {
        let rule_text = top_rule(inner_cols as usize);
        frame.push((
            input_top_bar_row,
            pad_left(vec![Span::new(rule_text, HL_USER)], hpad),
        ));
    }

    for i in 0..input_height {
        let src_idx = (input_scroll + i) as usize;
        let text = input_lines.get(src_idx).map(String::as_str).unwrap_or("");
        // Split off the "│ " prefix so the bar carries HL_USER (matching
        // the user-message block above) while the typed text stays in
        // HL_INPUT (terminal-default).
        let body_spans: Vec<Span> = match text.strip_prefix("│ ") {
            Some(rest) => vec![Span::new("│ ", HL_USER), Span::new(rest, HL_INPUT)],
            None => vec![Span::new(text, HL_INPUT)],
        };
        frame.push((input_start_row + i, pad_left(body_spans, hpad)));
    }

    if input_bars > 0 {
        let rule_text = bottom_rule(inner_cols as usize);
        frame.push((
            input_bot_bar_row,
            pad_left(vec![Span::new(rule_text, HL_USER)], hpad),
        ));
    }

    if status_height > 0 {
        let spans = build_status_spans(
            &state.metadata,
            &state.providers,
            &state.auth_status,
            total,
            transcript_rows,
            effective_scroll,
            chat_cols,
            state.gate_yolo,
        );
        frame.push((status_row, spans));
    }

    if vpad_bottom > 0 {
        for r in 0..vpad_bottom {
            frame.push((status_row + status_height + r, blank_row_spans()));
        }
    }

    // Popup overlay: replaces frame rows in the centered popup rect, drawn
    // ON TOP of the transcript / status. This runs before the diff loop, so
    // overlaid rows naturally take part in the row_cache delta-emission and
    // a closed popup re-emits the rows it previously covered.
    //
    // Popups are scoped to the chat pane so they never overlap the sidebar
    // — `popup_rect` and `overlay_popup` take `chat_cols`, not `cols`.
    if let Some(popup) = state.popup.clone() {
        overlay_popup(&mut frame, &popup, chat_cols, rows);
    }

    // Right sidebar — built after the chat frame so we can stitch the two
    // halves together at merge time. The sidebar's row spans are
    // co-indexed with the chat frame's row indices and combined cell-by-cell
    // in `merge_chat_and_sidebar`. One column from the reserve goes to the
    // vertical separator drawn at merge time, so widgets are built one
    // column narrower than `sidebar_w`.
    let sidebar_content_cols = (sidebar_w as usize).saturating_sub(SIDEBAR_SEPARATOR_COLS);
    let sidebar_rows = if sidebar_w > 0 && sidebar_content_cols > 0 {
        let now_ms = state.now_ms();
        crate::sidebar::build_sidebar(state, sidebar_content_cols, rows as usize, now_ms)
    } else {
        Vec::new()
    };

    // Stitch chat + sidebar into one frame keyed by row index. Each row's
    // final spans are `<chat spans padded to chat_cols> | <sidebar spans>`.
    // Rows where the chat side is missing get a blank chat-pane stripe; rows
    // where the sidebar is missing get a blank sidebar stripe. After this,
    // `cols`-wide grid_line emits the full row.
    let frame = if sidebar_w > 0 {
        merge_chat_and_sidebar(
            frame,
            &sidebar_rows,
            chat_cols,
            sidebar_content_cols as u32,
            rows,
        )
    } else {
        frame
    };

    let mut out: Vec<Map<String, Value>> = Vec::new();

    // Resize the cache to cover every row index in this frame. New slots
    // start as `None`, which guarantees first-emit. Shrinking past the
    // current frame's max row simply drops stale snapshots — those rows
    // don't exist in the smaller grid.
    let cache_len = frame
        .iter()
        .map(|(r, _)| *r as usize + 1)
        .max()
        .unwrap_or(0);
    if state.row_cache.len() != cache_len {
        state.row_cache.resize(cache_len, None);
    }

    for (row, spans) in &frame {
        let snapshot = snapshot_of(spans);
        let idx = *row as usize;
        let differs = state
            .row_cache
            .get(idx)
            .map(|c| c.as_ref() != Some(&snapshot))
            .unwrap_or(true);
        if differs {
            out.push(grid_line(*row, cols, spans));
            if let Some(slot) = state.row_cache.get_mut(idx) {
                *slot = Some(snapshot);
            }
        }
    }

    out.push(grid_cursor_goto(
        input_start_row + cursor_line_visible,
        cursor_col,
    ));

    out.push(grid_flush());
    out
}

/// Merge the chat-pane frame and sidebar rows into a single combined frame
/// keyed by row index. Each row's spans are padded so the chat side fills
/// exactly `chat_cols` columns, then the sidebar's spans are appended (which
/// already total `sidebar_cols`). Rows present in only one side are still
/// emitted with the missing side filled by blanks.
fn merge_chat_and_sidebar(
    chat_frame: Vec<(u32, Vec<Span>)>,
    sidebar_rows: &[Vec<Span>],
    chat_cols: u32,
    sidebar_cols: u32,
    rows: u32,
) -> Vec<(u32, Vec<Span>)> {
    use std::collections::BTreeMap;

    // Index chat-frame rows by row number for O(log n) lookups during the
    // combined-row build. BTreeMap so we iterate the merged frame in row
    // order (the cache layer doesn't strictly require it, but it makes
    // diffs and debug logs read top-to-bottom).
    let mut chat_by_row: BTreeMap<u32, Vec<Span>> = BTreeMap::new();
    for (r, spans) in chat_frame {
        chat_by_row.insert(r, spans);
    }

    let blank_chat = vec![Span::new(" ".repeat(chat_cols as usize), 0)];
    let blank_side = vec![Span::new(" ".repeat(sidebar_cols as usize), 0)];
    // One column of `│` in dim-chrome highlight. Drawn between the chat
    // pane and the sidebar so the DAG widget reads as a deliberate panel
    // rather than floating chat content.
    let separator: Span = Span::new("│".to_owned(), HL_FOOTER);

    let mut out: Vec<(u32, Vec<Span>)> = Vec::with_capacity(rows as usize);
    for r in 0..rows {
        let chat_spans = chat_by_row.remove(&r).unwrap_or_else(|| blank_chat.clone());
        let chat_padded = pad_to_width(chat_spans, chat_cols as usize);
        let side_spans = sidebar_rows
            .get(r as usize)
            .cloned()
            .unwrap_or_else(|| blank_side.clone());
        let mut combined = chat_padded;
        combined.push(separator.clone());
        combined.extend(side_spans);
        out.push((r, combined));
    }
    out
}

/// Right-pad `spans` with hl=0 spaces until the total display width hits
/// `width`. If the spans already exceed `width`, they're returned unchanged
/// (`grid_line` will clip on emit). Used to align the chat pane to its
/// column boundary before the sidebar is appended.
fn pad_to_width(spans: Vec<Span>, width: usize) -> Vec<Span> {
    let used: usize = spans.iter().map(|s| str_width(&s.text)).sum();
    if used >= width {
        return spans;
    }
    let pad = width - used;
    let mut out = spans;
    out.push(Span::new(" ".repeat(pad), 0));
    out
}

/// The span sequence we emit for an otherwise-empty row. Single space at
/// `hl=0`, matching `grid_line_blank`'s output.
fn blank_row_spans() -> Vec<Span> {
    vec![Span::new("", 0)]
}

// ---- popup overlay ---------------------------------------------------------

/// Vertical reservation against the popup height: status row + input box
/// (top bar + 1 line + bottom bar) + a single row of breathing room above and
/// below the popup. Centering math drives the actual top row; this is the
/// floor we never want to draw past so the input box and status bar remain
/// readable while a popup is open.
pub(crate) const POPUP_RESERVED_ROWS: u32 = 1 /* status */ + 3 /* input */ + 2 /* margins */;

/// Compute the popup rect (width, height, top-row, left-pad) from the
/// terminal dims. Returns `None` when there isn't enough space for a usable
/// popup. The height is clamped to `rows - POPUP_RESERVED_ROWS` so the popup
/// body can never overlap the input box or status bar.
pub(crate) fn popup_rect(cols: u32, rows: u32) -> Option<(usize, usize, u32, usize)> {
    let popup_w = (cols as usize * 6 / 10).clamp(40, cols.saturating_sub(2) as usize);
    let max_h = rows.saturating_sub(POPUP_RESERVED_ROWS) as usize;
    let preferred_h = (rows as usize * 6 / 10).clamp(8, rows.saturating_sub(2) as usize);
    let popup_h = preferred_h.min(max_h);
    if popup_w < 8 || popup_h < 4 {
        return None;
    }
    let left_pad = (cols as usize - popup_w) / 2;
    let top = ((rows as usize - popup_h) / 2) as u32;
    Some((popup_w, popup_h, top, left_pad))
}

/// Maximum width of the toast box (including borders). Smaller terminals
/// shrink this proportionally; below the floor we skip rendering.
const TOAST_MAX_WIDTH: u32 = 40;

/// Paint a single-line toast at the bottom-left of the chat area, just above
/// the input bar. Layout: a 1-row body line in `HL_STATUS_INFO`, anchored to
/// column 1 (one cell of left padding) and clipped to `TOAST_MAX_WIDTH`. No
/// border — keeps the visual weight low.
fn overlay_toast(frame: &mut Vec<(u32, Vec<Span>)>, message: &str, cols: u32, rows: u32) {
    if cols < 8 || rows < 4 {
        return;
    }
    // Pick a target row near the bottom of the transcript area (right above
    // the input bar). We approximate by walking the frame: the last row
    // strictly below midline and at least 3 rows above the bottom is a
    // reasonable choice. Here we just hard-pick: bottom row index minus an
    // offset that mirrors the layout.rs constants (1 vpad_bottom + 1 status
    // + 2 input_bars + 1 input + 1 vpad_input_top = 6). Clamp.
    let bottom_offset = 6u32.min(rows.saturating_sub(2));
    let target_row = rows.saturating_sub(1).saturating_sub(bottom_offset);

    let max_w = TOAST_MAX_WIDTH.min(cols.saturating_sub(2));
    let body_w = (max_w as usize).saturating_sub(2);
    let truncated = clip_to_width(message, body_w);
    let used = str_width(&truncated);
    let pad = body_w.saturating_sub(used);
    let mut text = String::with_capacity(used + pad + 2);
    text.push(' ');
    text.push_str(&truncated);
    for _ in 0..pad {
        text.push(' ');
    }
    text.push(' ');

    let row_spans: Vec<Span> = vec![
        Span::new(" ", 0),
        Span::new(text, HL_STATUS_INFO),
    ];
    if let Some(slot) = frame.iter_mut().find(|(r, _)| *r == target_row) {
        slot.1 = row_spans;
    } else {
        frame.push((target_row, row_spans));
    }
}

/// Replace rows in `frame` with the popup body, centered roughly 60% wide ×
/// 60% tall over the chat frame. The popup is built as `popup_height` rows of
/// spans, then each row is left-padded with `left_pad` blank cells and each
/// frame row in `[top..top+popup_height)` is overwritten in place. The height
/// is clamped to leave the input box and status bar visible (see
/// [`POPUP_RESERVED_ROWS`]).
fn overlay_popup(frame: &mut Vec<(u32, Vec<Span>)>, popup: &Popup, cols: u32, rows: u32) {
    // Toast popup is bottom-left, NOT centered. It doesn't follow popup_rect.
    if let Popup::Toast { message, .. } = popup {
        overlay_toast(frame, message, cols, rows);
        return;
    }

    // Slash autocomplete renders inline above the input bar (handled in
    // `render_frame`), not as a centered overlay. Skip the popup-overlay
    // path entirely so the inline band isn't shadowed by a centered box.
    if let Popup::SlashAutocomplete { .. } = popup {
        return;
    }

    let Some((popup_w, popup_h, top, left_pad)) = popup_rect(cols, rows) else {
        return;
    };

    let body = match popup {
        Popup::Help { scroll } => render_popup_help(popup_w, popup_h, *scroll),
        Popup::ModelPicker {
            all_models,
            query,
            cursor,
            awaiting,
            scroll,
        } => render_popup_model_picker(
            all_models, query, *cursor, awaiting, *scroll, popup_w, popup_h,
        ),
        Popup::Info {
            title,
            message,
            source,
            scroll,
        } => render_popup_message(
            title,
            message,
            source.as_deref(),
            *scroll,
            popup_w,
            popup_h,
            PopupKind::Info,
        ),
        Popup::Warning {
            title,
            message,
            source,
            scroll,
        } => render_popup_message(
            title,
            message,
            source.as_deref(),
            *scroll,
            popup_w,
            popup_h,
            PopupKind::Warning,
        ),
        Popup::Error {
            title,
            message,
            source,
            scroll,
        } => render_popup_message(
            title,
            message,
            source.as_deref(),
            *scroll,
            popup_w,
            popup_h,
            PopupKind::Error,
        ),
        Popup::SlashAutocomplete { .. } => {
            unreachable!("slash autocomplete handled above with inline rendering")
        }
        Popup::Toast { .. } => unreachable!("toast handled above with bottom-left layout"),
        Popup::ToolPermission {
            tool,
            args_preview,
            source,
            ..
        } => render_popup_tool_permission(
            tool,
            args_preview,
            source.as_deref(),
            popup_w,
            popup_h,
        ),
    };

    for (i, line) in body.into_iter().enumerate() {
        if i >= popup_h {
            break;
        }
        let row = top + i as u32;
        let mut spans: Vec<Span> = Vec::new();
        if left_pad > 0 {
            spans.push(Span::new(" ".repeat(left_pad), 0));
        }
        spans.extend(line);
        // Replace whatever frame row sits at this index. If the row isn't in
        // `frame` yet (shouldn't happen with the layout above), append.
        if let Some(slot) = frame.iter_mut().find(|(r, _)| *r == row) {
            slot.1 = spans;
        } else {
            frame.push((row, spans));
        }
    }
}

/// Format a single help label from a registry entry. Aliases are appended
/// after the canonical name as `(/alias1, /alias2)`; commands flagged
/// `takes_args` get a `[name]` placeholder. Mirrors the user-facing surface
/// described in the README so the autocomplete popup and help body stay in
/// lockstep with the parser.
fn slash_help_label(cmd: &crate::state::SlashCommand) -> String {
    let mut label = format!("/{}", cmd.name);
    if !cmd.aliases.is_empty() {
        let joined = cmd
            .aliases
            .iter()
            .map(|a| format!("/{a}"))
            .collect::<Vec<_>>()
            .join(", ");
        label.push_str(&format!(" ({joined})"));
    }
    if cmd.takes_args {
        label.push_str(" [name]");
    }
    label
}

/// All help entries that the help popup renders, in the order they appear.
/// Returned as a vec of pre-formatted body lines (no border/padding) so the
/// scroll path can slice it without re-running the formatting.
///
/// The slash-command rows are derived from `slash_command_registry()` so
/// adding a built-in there flows through to the help popup automatically. Key
/// shortcuts below are static — they're not commands.
pub(crate) fn help_body_lines(label_w: usize) -> Vec<String> {
    let mut entries: Vec<(String, String)> = crate::state::slash_command_registry()
        .iter()
        .map(|cmd| (slash_help_label(cmd), cmd.hint.clone()))
        .collect();
    let key_entries: &[(&str, &str)] = &[
        ("", ""),
        ("ESC or Q", "close popup (model picker: ESC only)"),
        ("Ctrl+O", "expand/collapse tool calls"),
        ("PageUp/Down", "scroll transcript / popup body"),
        ("Up/Down", "prompt history (when empty)"),
        ("Home/End", "jump to top/bottom of popup body"),
    ];
    entries.extend(
        key_entries
            .iter()
            .map(|(l, d)| ((*l).to_owned(), (*d).to_owned())),
    );
    entries
        .into_iter()
        .map(|(label, desc)| {
            if label.is_empty() && desc.is_empty() {
                String::new()
            } else {
                format!("{label:<label_w$}  {desc}")
            }
        })
        .collect()
}

/// Render the help popup body. `width` is the inner-popup column count
/// including borders; `height` is the row count. `scroll` is the first
/// content row to display (clamped against the body length).
fn render_popup_help(width: usize, height: usize, scroll: u16) -> Vec<Vec<Span>> {
    let mut rows: Vec<Vec<Span>> = Vec::new();
    let inner_w = width.saturating_sub(2);
    let label_w = 14usize.min(inner_w.saturating_sub(4));
    let body_lines = help_body_lines(label_w);
    // Body height = total - top border - bottom border.
    let body_height = height.saturating_sub(2);
    let max_scroll = body_lines.len().saturating_sub(body_height);
    let scroll = (scroll as usize).min(max_scroll);

    let title = build_help_title(body_lines.len(), body_height, scroll);
    rows.push(popup_top_border(&title, width));

    let visible_end = (scroll + body_height).min(body_lines.len());
    for line in &body_lines[scroll..visible_end] {
        let truncated = clip_to_width(line, inner_w.saturating_sub(2));
        rows.push(popup_text_row(&truncated, inner_w));
    }
    while rows.len() + 1 < height {
        rows.push(popup_blank_row(inner_w));
    }
    rows.push(popup_bottom_border(width));
    rows
}

/// Build the help popup's title bar. Includes a `n/m` indicator when the body
/// overflows so the user can tell they're not looking at a static list.
fn build_help_title(total: usize, visible: usize, scroll: usize) -> String {
    if total <= visible {
        " help ".to_string()
    } else {
        let last = (scroll + visible).min(total);
        format!(" help  {last}/{total} ")
    }
}

/// Distinguishes the three transient text popups so we can pick title text,
/// title-bar highlight, and border highlight without dragging three booleans
/// through the call site.
#[derive(Clone, Copy)]
enum PopupKind {
    Info,
    Warning,
    Error,
}

/// Render a transient text popup (info, warning, or error). Title-bar text +
/// border highlight pick from `kind`; `message` wraps to the inner column
/// budget using `wrap_to_width`. Footer hint reminds the user how to dismiss.
/// When `source` is `Some`, a dim `from: <source>` line renders directly
/// above the close-hint footer so the user can see which plugin published
/// the popup.
//
// Layout when `source` is `None`:
//     ┌── title ──┐
//     │ body line │
//     │ ...       │
//     ├───────────┤
//     │ ESC or Q  │
//     └───────────┘
//
// Layout when `source` is `Some(_)`:
//     ┌── title ──┐
//     │ body line │
//     │ ...       │
//     ├───────────┤
//     │ from: foo │
//     │ ESC or Q  │
//     └───────────┘
fn render_popup_message(
    title: &str,
    message: &str,
    source: Option<&str>,
    scroll: u16,
    width: usize,
    height: usize,
    kind: PopupKind,
) -> Vec<Vec<Span>> {
    let (mut title_text, hl) = match kind {
        PopupKind::Info => (format!(" info ℹ {title} "), HL_STATUS_INFO),
        PopupKind::Warning => (format!(" warning ⚠ {title} "), HL_STATUS_WARN),
        PopupKind::Error => (format!(" error ✕ {title} "), HL_STATUS_DANGER),
    };
    let footer = "ESC or Q to close";
    let source_line = source.map(|s| format!("from: {s}"));

    let inner_w = width.saturating_sub(2);
    let body_w = inner_w.saturating_sub(2);
    // Reserve below the body: separator + (optional) source line + footer
    // hint + bottom border.
    let footer_rows: usize = if source_line.is_some() { 2 } else { 1 };
    let reserved_below: usize = 1 + footer_rows + 1;
    let body_height = height.saturating_sub(1 + reserved_below);

    let wrapped: Vec<String> = if body_w == 0 {
        Vec::new()
    } else {
        wrap_to_width(message, body_w)
    };
    let max_scroll = wrapped.len().saturating_sub(body_height);
    let scroll = (scroll as usize).min(max_scroll);

    if wrapped.len() > body_height {
        let last = (scroll + body_height).min(wrapped.len());
        title_text = match kind {
            PopupKind::Info => format!(" info ℹ {title}  {last}/{} ", wrapped.len()),
            PopupKind::Warning => format!(" warning ⚠ {title}  {last}/{} ", wrapped.len()),
            PopupKind::Error => format!(" error ✕ {title}  {last}/{} ", wrapped.len()),
        };
    }

    let mut rows: Vec<Vec<Span>> = Vec::new();
    rows.push(popup_top_border_with_hl(&title_text, width, hl));

    let visible_end = (scroll + body_height).min(wrapped.len());
    let mut emitted = 0usize;
    for line in &wrapped[scroll..visible_end] {
        let truncated = clip_to_width(line, body_w);
        rows.push(popup_text_row_with_border_hl(&truncated, inner_w, hl));
        emitted += 1;
    }
    while emitted < body_height {
        rows.push(popup_blank_row_with_border_hl(inner_w, hl));
        emitted += 1;
    }

    rows.push(popup_separator_row_with_hl(width, hl));
    if let Some(line) = &source_line {
        let truncated = clip_to_width(line, body_w);
        rows.push(popup_text_row_with_body_hl_and_border(
            &truncated, inner_w, hl, HL_FOOTER,
        ));
    }
    let footer_truncated = clip_to_width(footer, body_w);
    rows.push(popup_text_row_with_border_hl(&footer_truncated, inner_w, hl));
    rows.push(popup_bottom_border_with_hl(width, hl));
    rows
}

/// Render the tool-permission popup body. Borders use the warning hue
/// (this is a guard prompt, not a fatal error). Layout:
///   top border with title "permission requested · <tool>"
///   body: pretty-printed args, wrapped to width
///   separator
///   optional `from: <source>` line
///   footer: "[A]pprove   [D]eny   (ESC = deny)"
///   bottom border
fn render_popup_tool_permission(
    tool: &str,
    args_preview: &str,
    source: Option<&str>,
    width: usize,
    height: usize,
) -> Vec<Vec<Span>> {
    let title_text = format!(" permission requested · {tool} ");
    let footer = "[A]pprove   [D]eny   (ESC = deny)";
    let source_line = source.map(|s| format!("from: {s}"));
    let hl = HL_STATUS_WARN;

    let inner_w = width.saturating_sub(2);
    let body_w = inner_w.saturating_sub(2);
    let footer_rows: usize = if source_line.is_some() { 2 } else { 1 };
    let reserved_below: usize = 1 + footer_rows + 1;
    let body_height = height.saturating_sub(1 + reserved_below);

    // Args preview is already pretty-printed (multi-line JSON). Split on
    // existing newlines first, then wrap each line to body_w so long
    // values don't run off-screen.
    let mut wrapped: Vec<String> = Vec::new();
    if body_w > 0 {
        for raw in args_preview.split('\n') {
            if raw.is_empty() {
                wrapped.push(String::new());
            } else {
                wrapped.extend(wrap_to_width(raw, body_w));
            }
        }
    }

    let mut rows: Vec<Vec<Span>> = Vec::new();
    rows.push(popup_top_border_with_hl(&title_text, width, hl));

    let visible_end = wrapped.len().min(body_height);
    let mut emitted = 0usize;
    for line in &wrapped[..visible_end] {
        let truncated = clip_to_width(line, body_w);
        rows.push(popup_text_row_with_border_hl(&truncated, inner_w, hl));
        emitted += 1;
    }
    while emitted < body_height {
        rows.push(popup_blank_row_with_border_hl(inner_w, hl));
        emitted += 1;
    }

    rows.push(popup_separator_row_with_hl(width, hl));
    if let Some(line) = &source_line {
        let truncated = clip_to_width(line, body_w);
        rows.push(popup_text_row_with_body_hl_and_border(
            &truncated, inner_w, hl, HL_FOOTER,
        ));
    }
    let footer_truncated = clip_to_width(footer, body_w);
    rows.push(popup_text_row_with_border_hl(&footer_truncated, inner_w, hl));
    rows.push(popup_bottom_border_with_hl(width, hl));
    rows
}

/// Render the model picker popup body. Layout (top to bottom):
/// `┌── title ──┐ · search · ── · list * body_height · ── · footer · └── ──┘`.
/// The search bar moved to the top in this revision so it sits adjacent to
/// the title where the user is already focused.
fn render_popup_model_picker(
    all_models: &[(String, String)],
    query: &str,
    cursor: usize,
    awaiting: &std::collections::HashSet<String>,
    scroll: u16,
    width: usize,
    height: usize,
) -> Vec<Vec<Span>> {
    let mut rows: Vec<Vec<Span>> = Vec::new();
    let inner_w = width.saturating_sub(2);
    let body_w = inner_w.saturating_sub(2);

    // Layout reservations (in rows):
    //   1 top border
    //   1 search line
    //   1 separator
    //   body_height list rows
    //   1 footer (loading/blank)
    //   1 bottom border
    // body_height = height - 5
    let reserved: usize = 5;
    let body_height = height.saturating_sub(reserved);

    // Filter
    let q = query.to_lowercase();
    let filtered: Vec<&(String, String)> = if q.is_empty() {
        all_models.iter().collect()
    } else {
        all_models
            .iter()
            .filter(|(p, m)| format!("{p} {m}").to_lowercase().contains(&q))
            .collect()
    };

    // Title with optional position indicator when the list overflows.
    let title = if filtered.len() > body_height && body_height > 0 {
        format!(" pick a model  {}/{} ", cursor + 1, filtered.len())
    } else {
        " pick a model ".to_string()
    };

    rows.push(popup_top_border(&title, width));

    // Search line goes immediately under the title, before the separator.
    let search_label = "search: ";
    let search_text = format!("{search_label}{query}");
    let truncated_search = clip_to_width(&search_text, body_w);
    rows.push(popup_text_row(&truncated_search, inner_w));
    rows.push(popup_separator_row(width));

    // Empty state — no providers connected.
    if all_models.is_empty() && awaiting.is_empty() {
        let msg = [
            "No providers connected.",
            "",
            "Wire one up in init.lua",
            "(see docs/provider-plugins.md)",
        ];
        let mut emitted = 0usize;
        for line in &msg {
            if emitted >= body_height {
                break;
            }
            let truncated = clip_to_width(line, body_w);
            rows.push(popup_text_row(&truncated, inner_w));
            emitted += 1;
        }
        while emitted < body_height {
            rows.push(popup_blank_row(inner_w));
            emitted += 1;
        }
        // Footer + bottom border to keep the geometry stable.
        rows.push(popup_blank_row(inner_w));
        rows.push(popup_bottom_border(width));
        return rows;
    }

    // Anchor viewport on the explicit `scroll` value, but re-clamp it so the
    // cursor is always within `[scroll, scroll + body_height)`. Without this
    // the cursor could fall outside the visible window after a filter change.
    let max_start = filtered.len().saturating_sub(body_height);
    let mut start = (scroll as usize).min(max_start);
    if body_height > 0 {
        if cursor < start {
            start = cursor;
        } else if cursor >= start + body_height {
            start = cursor + 1 - body_height;
        }
    }
    let end = (start + body_height).min(filtered.len());

    let provider_w = filtered
        .iter()
        .map(|(p, _)| str_width(p))
        .max()
        .unwrap_or(6)
        .min(20);

    let mut emitted = 0usize;
    for (i, (provider, model)) in filtered[start..end].iter().enumerate() {
        let actual = start + i;
        let line = format!("{:<width$}  {}", provider, model, width = provider_w);
        let truncated = clip_to_width(&line, body_w);
        let highlighted = actual == cursor;
        rows.push(popup_row(&truncated, inner_w, highlighted));
        emitted += 1;
    }

    // Pad body area with blanks up to body_height.
    while emitted < body_height {
        rows.push(popup_blank_row(inner_w));
        emitted += 1;
    }

    // Loading footer (if any provider still pending).
    if !awaiting.is_empty() {
        let n = awaiting.len();
        let plural = if n == 1 { "provider" } else { "providers" };
        let msg = format!("loading from {n} {plural}…");
        let truncated = clip_to_width(&msg, body_w);
        rows.push(popup_text_row(&truncated, inner_w));
    } else {
        rows.push(popup_blank_row(inner_w));
    }

    rows.push(popup_bottom_border(width));
    rows
}

/// Render the inline slash-command autocomplete band. Sits directly above
/// the input top bar (no border, no title) and emits up to `max_rows` rows
/// of `/{name}  {hint}` entries. The cursor row carries
/// `HL_STATUS_BAR_FILL` as bg so it reads as selected; other rows are
/// plain `HL_INPUT`. When `matches` is empty, a single dim
/// "no matching commands" row renders so the user knows their prefix
/// matched nothing.
fn render_inline_slash_autocomplete(
    matches: &[crate::state::SlashCommand],
    cursor: usize,
    scroll: u16,
    width: usize,
    max_rows: usize,
) -> Vec<Vec<Span>> {
    let mut rows: Vec<Vec<Span>> = Vec::new();
    if max_rows == 0 || width == 0 {
        return rows;
    }

    if matches.is_empty() {
        let msg = "no matching commands";
        let truncated = clip_to_width(msg, width);
        let used = str_width(&truncated);
        let pad = width.saturating_sub(used);
        let mut text = String::with_capacity(used + pad);
        text.push_str(&truncated);
        for _ in 0..pad {
            text.push(' ');
        }
        rows.push(vec![Span::new(text, HL_STATUS_DIM)]);
        return rows;
    }

    // Re-anchor scroll so cursor stays in view. Mirrors `clamp_slash_scroll`
    // in main.rs but applied to the rendering window directly.
    let visible = max_rows.min(matches.len());
    let max_start = matches.len().saturating_sub(visible);
    let mut start = (scroll as usize).min(max_start);
    if visible > 0 {
        if cursor < start {
            start = cursor;
        } else if cursor >= start + visible {
            start = cursor + 1 - visible;
        }
    }
    let end = (start + visible).min(matches.len());

    let name_w = matches
        .iter()
        .map(|c| str_width(&c.name))
        .max()
        .unwrap_or(6)
        .min(16);

    for (i, cmd) in matches[start..end].iter().enumerate() {
        let actual = start + i;
        let line = format!("/{:<width$}  {}", cmd.name, cmd.hint, width = name_w);
        let truncated = clip_to_width(&line, width);
        let used = str_width(&truncated);
        let pad = width.saturating_sub(used);
        let mut text = String::with_capacity(used + pad);
        text.push_str(&truncated);
        for _ in 0..pad {
            text.push(' ');
        }
        let hl = if actual == cursor {
            HL_STATUS_BAR_FILL
        } else {
            HL_INPUT
        };
        rows.push(vec![Span::new(text, hl)]);
    }

    rows
}

/// Truncate `s` to `max` display columns. Single-byte ASCII assumed for the
/// popup text (labels, command names) — multibyte input via the search box
/// uses `split_spans_at_width`'s logic indirectly via `clip_to_width_chars`.
fn clip_to_width(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = char_width(ch);
        if w + cw > max {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out
}

fn popup_top_border(title: &str, width: usize) -> Vec<Span> {
    let inner = width.saturating_sub(2);
    let title_w = str_width(title).min(inner.saturating_sub(2));
    let after = inner.saturating_sub(2 + title_w);
    let mut s = String::new();
    s.push('┌');
    s.push('─');
    s.push('─');
    let title_clipped = clip_to_width(title, title_w);
    s.push_str(&title_clipped);
    let already = 2 + str_width(&title_clipped);
    let dashes = inner.saturating_sub(already);
    for _ in 0..dashes {
        s.push('─');
    }
    s.push('┐');
    let _ = after;
    vec![Span::new(s, HL_USER)]
}

fn popup_bottom_border(width: usize) -> Vec<Span> {
    let inner = width.saturating_sub(2);
    let mut s = String::new();
    s.push('└');
    for _ in 0..inner {
        s.push('─');
    }
    s.push('┘');
    vec![Span::new(s, HL_USER)]
}

fn popup_separator_row(width: usize) -> Vec<Span> {
    let inner = width.saturating_sub(2);
    let mut s = String::new();
    s.push('├');
    for _ in 0..inner {
        s.push('─');
    }
    s.push('┤');
    vec![Span::new(s, HL_USER)]
}

fn popup_blank_row(inner_w: usize) -> Vec<Span> {
    let pad = " ".repeat(inner_w);
    vec![
        Span::new("│", HL_USER),
        Span::new(pad, HL_SYSTEM),
        Span::new("│", HL_USER),
    ]
}

fn popup_text_row(text: &str, inner_w: usize) -> Vec<Span> {
    let body_w = inner_w.saturating_sub(2);
    let used = str_width(text);
    let pad = body_w.saturating_sub(used);
    let mut padded = String::with_capacity(used + pad + 2);
    padded.push(' ');
    padded.push_str(text);
    for _ in 0..pad {
        padded.push(' ');
    }
    padded.push(' ');
    vec![
        Span::new("│", HL_USER),
        Span::new(padded, HL_SYSTEM),
        Span::new("│", HL_USER),
    ]
}

fn popup_row(text: &str, inner_w: usize, highlighted: bool) -> Vec<Span> {
    let body_w = inner_w.saturating_sub(2);
    let used = str_width(text);
    let pad = body_w.saturating_sub(used);
    let mut padded = String::with_capacity(used + pad + 2);
    padded.push(' ');
    padded.push_str(text);
    for _ in 0..pad {
        padded.push(' ');
    }
    padded.push(' ');
    let body_hl = if highlighted { HL_STATUS } else { HL_SYSTEM };
    vec![
        Span::new("│", HL_USER),
        Span::new(padded, body_hl),
        Span::new("│", HL_USER),
    ]
}

// ---- popup border variants with custom highlight --------------------------
//
// Warning / error popups use a status-color border to make their severity
// readable at a glance. The default (`HL_USER`) borders are reused for help
// and the model picker; these variants take an explicit hl id so the same
// box drawing can carry a yellow or red border without a layout change.

fn popup_top_border_with_hl(title: &str, width: usize, hl: u32) -> Vec<Span> {
    let inner = width.saturating_sub(2);
    let title_w = str_width(title).min(inner.saturating_sub(2));
    let mut s = String::new();
    s.push('┌');
    s.push('─');
    s.push('─');
    let title_clipped = clip_to_width(title, title_w);
    s.push_str(&title_clipped);
    let already = 2 + str_width(&title_clipped);
    let dashes = inner.saturating_sub(already);
    for _ in 0..dashes {
        s.push('─');
    }
    s.push('┐');
    vec![Span::new(s, hl)]
}

fn popup_bottom_border_with_hl(width: usize, hl: u32) -> Vec<Span> {
    let inner = width.saturating_sub(2);
    let mut s = String::new();
    s.push('└');
    for _ in 0..inner {
        s.push('─');
    }
    s.push('┘');
    vec![Span::new(s, hl)]
}

fn popup_separator_row_with_hl(width: usize, hl: u32) -> Vec<Span> {
    let inner = width.saturating_sub(2);
    let mut s = String::new();
    s.push('├');
    for _ in 0..inner {
        s.push('─');
    }
    s.push('┤');
    vec![Span::new(s, hl)]
}

fn popup_text_row_with_border_hl(text: &str, inner_w: usize, hl: u32) -> Vec<Span> {
    let body_w = inner_w.saturating_sub(2);
    let used = str_width(text);
    let pad = body_w.saturating_sub(used);
    let mut padded = String::with_capacity(used + pad + 2);
    padded.push(' ');
    padded.push_str(text);
    for _ in 0..pad {
        padded.push(' ');
    }
    padded.push(' ');
    vec![
        Span::new("│", hl),
        Span::new(padded, HL_SYSTEM),
        Span::new("│", hl),
    ]
}

fn popup_blank_row_with_border_hl(inner_w: usize, hl: u32) -> Vec<Span> {
    let pad = " ".repeat(inner_w);
    vec![
        Span::new("│", hl),
        Span::new(pad, HL_SYSTEM),
        Span::new("│", hl),
    ]
}

/// Variant of `popup_text_row_with_border_hl` where the body span uses a
/// caller-supplied highlight (e.g. `HL_FOOTER` for a dim `from: <source>`
/// line) while the borders keep the popup's own colour.
fn popup_text_row_with_body_hl_and_border(
    text: &str,
    inner_w: usize,
    border_hl: u32,
    body_hl: u32,
) -> Vec<Span> {
    let body_w = inner_w.saturating_sub(2);
    let used = str_width(text);
    let pad = body_w.saturating_sub(used);
    let mut padded = String::with_capacity(used + pad + 2);
    padded.push(' ');
    padded.push_str(text);
    for _ in 0..pad {
        padded.push(' ');
    }
    padded.push(' ');
    vec![
        Span::new("│", border_hl),
        Span::new(padded, body_hl),
        Span::new("│", border_hl),
    ]
}

/// Prepend `pad` blank columns (hl=0) to a row's spans. The trailing
/// right-side gap is taken care of automatically by `grid_line`'s padding
/// run, which fills any remaining cols with hl=0 spaces.
fn pad_left(mut spans: Vec<Span>, pad: u32) -> Vec<Span> {
    if pad == 0 {
        return spans;
    }
    let mut out = Vec::with_capacity(spans.len() + 1);
    out.push(Span::new(" ".repeat(pad as usize), 0));
    out.append(&mut spans);
    out
}

fn snapshot_of(spans: &[Span]) -> RowSnapshot {
    spans
        .iter()
        .map(|s| (s.text.clone(), s.hl))
        .collect()
}


fn last_is_streaming_assistant(entries: &[TranscriptEntry]) -> bool {
    entries
        .last()
        .is_some_and(|e| e.role == Role::Assistant && e.streaming)
}

fn wrap_transcript(entries: &[TranscriptEntry], cols: usize, tools_expanded: bool) -> Vec<Line> {
    let mut out: Vec<Line> = Vec::new();
    let last_idx = entries.len().saturating_sub(1);
    for (i, e) in entries.iter().enumerate() {
        match e.role {
            Role::Tool => {
                if let Some(payload) = e.tool.as_ref() {
                    if tools_expanded {
                        for line in tool_expanded_lines(payload, cols) {
                            out.push(line);
                        }
                    } else {
                        out.push(tool_collapsed_line(payload, cols));
                        if payload.error {
                            out.push(vec![Span::new("  error".to_string(), HL_STATUS_DANGER)]);
                        }
                    }
                }
            }
            Role::User => {
                // Visual block: `╭──...` top rule · `│ <text>` content rows ·
                // `╰──...` bottom rule. All glyphs in HL_USER so the block
                // outline tints uniformly without forcing a background colour
                // (preserves terminal transparency).
                out.push(vec![Span::new(top_rule(cols), HL_USER)]);
                let inner = cols.saturating_sub(2);
                if inner == 0 {
                    out.push(vec![Span::new("│ ", HL_USER)]);
                } else {
                    for line in wrap_to_width(&e.text, inner) {
                        out.push(vec![
                            Span::new("│ ", HL_USER),
                            Span::new(line, HL_ASSISTANT),
                        ]);
                    }
                }
                out.push(vec![Span::new(bottom_rule(cols), HL_USER)]);
            }
            Role::System => {
                let bracketed = format!("[{}]", e.text);
                for line in wrap_to_width(&bracketed, cols) {
                    out.push(vec![Span::new(line, HL_SYSTEM)]);
                }
            }
            Role::Assistant => {
                // No prefix — assistant messages are the dominant content
                // and stand on their own. Markdown rendering applies to the
                // whole body. The visual cue that this is the assistant is
                // the *absence* of a left bar (only user blocks carry one).
                let md_lines = markdown::render(&e.text, cols);
                for line in md_lines {
                    out.push(line);
                }
                if let Some(footer) = assistant_footer_spans(e, cols) {
                    out.push(footer);
                }
            }
        }
        // One blank row between turns. The trailing entry gets no
        // separator — the layout reserves a dedicated blank row between
        // the transcript area and the input block, so a trailing blank
        // here would compete with content for the bottom-anchored
        // viewport's last visible line.
        if i < last_idx {
            out.push(Vec::new());
        }
    }
    out
}

/// Top horizontal rule for a user block: `╭` + `─` repeated to fill the
/// inner width. Falls back to a bare `╭─` on extremely narrow terminals.
fn top_rule(cols: usize) -> String {
    rule('╭', cols)
}

/// Bottom horizontal rule for a user block: `╰` + `─` repeated to fill
/// the inner width.
fn bottom_rule(cols: usize) -> String {
    rule('╰', cols)
}

fn rule(corner: char, cols: usize) -> String {
    let width = cols.max(2);
    let mut s = String::with_capacity(width * 3);
    s.push(corner);
    for _ in 1..width {
        s.push('─');
    }
    s
}

/// Build the per-turn footer for a finalized assistant entry, or `None` if
/// the entry is still streaming or has neither model nor duration. Renders
/// whichever fields are present:
///   - both     → `▣ <model> · <human_duration>`
///   - model    → `▣ <model>`            (replayed history: duration unknown)
///   - duration → `▣ <human_duration>`   (model unknown)
fn assistant_footer_spans(entry: &TranscriptEntry, cols: usize) -> Option<Line> {
    if entry.streaming {
        return None;
    }
    if cols == 0 {
        return None;
    }
    let model = entry
        .model
        .as_deref()
        .map(|m| m.strip_prefix("claude-").unwrap_or(m));
    let dur = entry.duration_ms.map(humanize_duration_ms);
    let body = match (model, dur) {
        (Some(m), Some(d)) => format!("{m} · {d}"),
        (Some(m), None) => m.to_owned(),
        (None, Some(d)) => d,
        (None, None) => return None,
    };
    let text = format!("▣ {body}");
    let spans = vec![Span::new(text, HL_FOOTER)];
    if str_width(&spans[0].text) > cols {
        Some(truncate_spans(&spans, cols))
    } else {
        Some(spans)
    }
}

fn compute_viewport(total: u32, transcript_rows: u32, scroll_offset: u32) -> (u32, u32) {
    if total <= transcript_rows {
        return (0, 0);
    }
    let max_offset = total - transcript_rows;
    let offset = scroll_offset.min(max_offset);
    let first = max_offset - offset;
    (first, 0)
}

// ---- markdown rendering ----------------------------------------------------

mod markdown {
    use super::*;
    use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

    /// Render markdown `text` to a sequence of wrapped lines (each a
    /// `Vec<Span>`). Empty input yields an empty vec; a paragraph of plain
    /// text yields one line per wrapped row.
    pub fn render(text: &str, cols: usize) -> Vec<Line> {
        if text.is_empty() {
            return Vec::new();
        }
        let blocks = parse_blocks(text);
        let mut lines: Vec<Line> = Vec::new();
        for (i, block) in blocks.iter().enumerate() {
            if i > 0 && block_needs_top_separator(block, &blocks[i - 1]) {
                lines.push(Vec::new());
            }
            render_block(block, cols, &mut lines);
        }
        lines
    }

    fn block_needs_top_separator(_curr: &Block, _prev: &Block) -> bool {
        // CommonMark blocks are typically paragraph-separated; we fold a
        // single blank line between most consecutive blocks. Skip the gap
        // between adjacent list items so the bullets stay tight.
        true
    }

    /// Top-level block representation. We collapse the pulldown stream into
    /// a small set of variants the renderer cares about.
    #[allow(clippy::enum_variant_names)]
    enum Block {
        Paragraph(Vec<Span>),
        Heading(u32, Vec<Span>),
        CodeBlock(String),
        BlockQuote(Vec<Block>),
        List(bool, Vec<Vec<Block>>),
        Rule,
        // header is row 0; rows is the body. Each cell is a span sequence.
        Table {
            header: Vec<Vec<Span>>,
            rows: Vec<Vec<Vec<Span>>>,
        },
    }

    fn parse_blocks(text: &str) -> Vec<Block> {
        let parser = Parser::new_ext(text, Options::all());
        let events: Vec<Event> = parser.collect();
        let mut idx = 0usize;
        let mut blocks: Vec<Block> = Vec::new();
        while idx < events.len() {
            if let Some((b, next)) = parse_block_at(&events, idx) {
                blocks.push(b);
                idx = next;
            } else {
                idx += 1;
            }
        }
        blocks
    }

    fn parse_block_at(events: &[Event], start: usize) -> Option<(Block, usize)> {
        let ev = events.get(start)?;
        match ev {
            Event::Start(Tag::Paragraph) => {
                let (spans, next) = collect_inline_until(events, start + 1, &TagEnd::Paragraph);
                Some((Block::Paragraph(spans), next))
            }
            Event::Start(Tag::Heading { level, .. }) => {
                let lv = heading_level(*level);
                let (spans, next) =
                    collect_inline_until(events, start + 1, &TagEnd::Heading(*level));
                Some((Block::Heading(lv, spans), next))
            }
            Event::Start(Tag::CodeBlock(_)) => {
                let mut buf = String::new();
                let mut i = start + 1;
                while let Some(e) = events.get(i) {
                    match e {
                        Event::End(TagEnd::CodeBlock) => {
                            i += 1;
                            break;
                        }
                        Event::Text(t) => buf.push_str(t),
                        _ => {}
                    }
                    i += 1;
                }
                Some((Block::CodeBlock(buf), i))
            }
            Event::Start(Tag::BlockQuote(_)) => {
                let mut inner: Vec<Block> = Vec::new();
                let mut i = start + 1;
                while let Some(e) = events.get(i) {
                    if matches!(e, Event::End(TagEnd::BlockQuote(_))) {
                        i += 1;
                        break;
                    }
                    if let Some((b, n)) = parse_block_at(events, i) {
                        inner.push(b);
                        i = n;
                    } else {
                        i += 1;
                    }
                }
                Some((Block::BlockQuote(inner), i))
            }
            Event::Rule => Some((Block::Rule, start + 1)),
            Event::Start(Tag::Table(_)) => {
                let mut header: Vec<Vec<Span>> = Vec::new();
                let mut rows: Vec<Vec<Vec<Span>>> = Vec::new();
                let mut i = start + 1;
                while let Some(e) = events.get(i) {
                    match e {
                        Event::End(TagEnd::Table) => {
                            i += 1;
                            break;
                        }
                        Event::Start(Tag::TableHead) => {
                            let (cells, n) = collect_table_cells(events, i + 1, true);
                            header = cells;
                            i = n;
                        }
                        Event::Start(Tag::TableRow) => {
                            let (cells, n) = collect_table_cells(events, i + 1, false);
                            rows.push(cells);
                            i = n;
                        }
                        _ => i += 1,
                    }
                }
                Some((Block::Table { header, rows }, i))
            }
            Event::Start(Tag::List(start_n)) => {
                let ordered = start_n.is_some();
                let mut items: Vec<Vec<Block>> = Vec::new();
                let mut i = start + 1;
                while let Some(e) = events.get(i) {
                    match e {
                        Event::End(TagEnd::List(_)) => {
                            i += 1;
                            break;
                        }
                        Event::Start(Tag::Item) => {
                            let (item_blocks, next) = collect_item(events, i + 1);
                            items.push(item_blocks);
                            i = next;
                        }
                        _ => i += 1,
                    }
                }
                Some((Block::List(ordered, items), i))
            }
            _ => None,
        }
    }

    fn collect_item(events: &[Event], start: usize) -> (Vec<Block>, usize) {
        let mut out: Vec<Block> = Vec::new();
        let mut inline: Vec<Span> = Vec::new();
        let mut i = start;
        while let Some(e) = events.get(i) {
            if matches!(e, Event::End(TagEnd::Item)) {
                if !inline.is_empty() {
                    out.push(Block::Paragraph(std::mem::take(&mut inline)));
                }
                i += 1;
                break;
            }
            // Try block parsing first; if not a block-start, treat as inline.
            if matches!(
                e,
                Event::Start(Tag::Paragraph)
                    | Event::Start(Tag::CodeBlock(_))
                    | Event::Start(Tag::List(_))
                    | Event::Start(Tag::BlockQuote(_))
                    | Event::Start(Tag::Heading { .. })
            ) {
                if !inline.is_empty() {
                    out.push(Block::Paragraph(std::mem::take(&mut inline)));
                }
                if let Some((b, n)) = parse_block_at(events, i) {
                    out.push(b);
                    i = n;
                    continue;
                }
            }
            // Inline event — append to the current run.
            inline.extend(inline_event_to_spans(e, HL_ASSISTANT));
            i += 1;
        }
        (out, i)
    }

    /// Walk events from `start` consuming `Tag::TableCell` blocks until the
    /// matching `TagEnd::TableHead`/`TagEnd::TableRow`. Returns the per-cell
    /// span sequences and the index after the row/head close.
    fn collect_table_cells(
        events: &[Event],
        start: usize,
        is_head: bool,
    ) -> (Vec<Vec<Span>>, usize) {
        let mut cells: Vec<Vec<Span>> = Vec::new();
        let mut i = start;
        while let Some(e) = events.get(i) {
            match e {
                Event::End(TagEnd::TableHead) if is_head => {
                    i += 1;
                    break;
                }
                Event::End(TagEnd::TableRow) if !is_head => {
                    i += 1;
                    break;
                }
                Event::Start(Tag::TableCell) => {
                    let (spans, next) =
                        collect_inline_until(events, i + 1, &TagEnd::TableCell);
                    cells.push(spans);
                    i = next;
                }
                _ => i += 1,
            }
        }
        (cells, i)
    }

    fn collect_inline_until(events: &[Event], start: usize, end: &TagEnd) -> (Vec<Span>, usize) {
        let mut spans: Vec<Span> = Vec::new();
        let mut style_stack: Vec<u32> = Vec::new();
        let mut i = start;
        while let Some(e) = events.get(i) {
            if matches_end(e, end) {
                i += 1;
                break;
            }
            match e {
                Event::Start(Tag::Strong) => style_stack.push(HL_MD_BOLD),
                Event::End(TagEnd::Strong) => {
                    style_stack.pop();
                }
                Event::Start(Tag::Emphasis) => style_stack.push(HL_MD_ITALIC),
                Event::End(TagEnd::Emphasis) => {
                    style_stack.pop();
                }
                Event::Start(Tag::Link { .. }) => style_stack.push(HL_MD_LINK),
                Event::End(TagEnd::Link) => {
                    style_stack.pop();
                }
                Event::Start(_) | Event::End(_) => {}
                Event::Code(t) => spans.push(Span::new(t.to_string(), HL_MD_CODE_INLINE)),
                Event::Text(t) => {
                    let hl = *style_stack.last().unwrap_or(&HL_ASSISTANT);
                    spans.push(Span::new(t.to_string(), hl));
                }
                Event::SoftBreak | Event::HardBreak => {
                    let hl = *style_stack.last().unwrap_or(&HL_ASSISTANT);
                    spans.push(Span::new(" ", hl));
                }
                _ => {}
            }
            i += 1;
        }
        (spans, i)
    }

    fn matches_end(e: &Event, end: &TagEnd) -> bool {
        if let Event::End(t) = e {
            return std::mem::discriminant(t) == std::mem::discriminant(end);
        }
        false
    }

    fn inline_event_to_spans(e: &Event, default_hl: u32) -> Vec<Span> {
        match e {
            Event::Text(t) => vec![Span::new(t.to_string(), default_hl)],
            Event::Code(t) => vec![Span::new(t.to_string(), HL_MD_CODE_INLINE)],
            Event::SoftBreak | Event::HardBreak => vec![Span::new(" ", default_hl)],
            _ => Vec::new(),
        }
    }

    fn heading_level(l: HeadingLevel) -> u32 {
        match l {
            HeadingLevel::H1 => 1,
            HeadingLevel::H2 => 2,
            HeadingLevel::H3 => 3,
            HeadingLevel::H4 => 4,
            HeadingLevel::H5 => 5,
            HeadingLevel::H6 => 6,
        }
    }

    fn render_block(block: &Block, cols: usize, out: &mut Vec<Line>) {
        match block {
            Block::Paragraph(spans) => {
                for line in wrap_spans(spans, cols) {
                    out.push(line);
                }
            }
            Block::Heading(_lv, spans) => {
                let prefixed: Vec<Span> = spans
                    .iter()
                    .map(|s| Span::new(s.text.clone(), HL_MD_HEADING))
                    .collect();
                for line in wrap_spans(&prefixed, cols) {
                    out.push(line);
                }
            }
            Block::CodeBlock(body) => {
                // Render the block as a uniform dark rectangle:
                //   ` <code> ` per row, every row padded to the same width
                //   so the bg color forms a clean rectangle.
                //
                // 1col left inset + 1col right inset live *inside* the bg,
                // so code text never visually butts against the rectangle's
                // edge. Long lines hard-wrap at `inner = cols - 2`.
                let cols = cols.max(1);
                let left_inset: usize = if cols >= 3 { 1 } else { 0 };
                let right_inset: usize = if cols >= 3 { 1 } else { 0 };
                let inner = cols.saturating_sub(left_inset + right_inset).max(1);

                // Collect wrapped chunks first so we can size the block to
                // the widest row, then pad every row to that width.
                let mut chunks: Vec<String> = Vec::new();
                for raw in body.lines() {
                    let line_chunks = crate::wrap::split_by_columns(raw, inner);
                    if line_chunks.is_empty() {
                        chunks.push(String::new());
                    } else {
                        chunks.extend(line_chunks);
                    }
                }
                if chunks.is_empty() {
                    // Empty fenced block (no lines): still render a single
                    // padded row so the user sees the block was emitted.
                    chunks.push(String::new());
                }

                let max_chunk_w = chunks.iter().map(|c| str_width(c)).max().unwrap_or(0);
                // Minimum visible width — a third of the column budget — so
                // a one-token code block doesn't render as a tiny chip. Cap
                // at `inner` (the post-inset budget) to prevent overflow.
                let min_inner = (cols / 3).min(inner);
                let block_inner_w = max_chunk_w.max(min_inner).min(inner);

                let left_pad: String = " ".repeat(left_inset);
                let right_pad: String = " ".repeat(right_inset);

                for chunk in chunks {
                    let chunk_w = str_width(&chunk);
                    let trailing = block_inner_w.saturating_sub(chunk_w);
                    let mut spans: Vec<Span> = Vec::new();
                    if !left_pad.is_empty() {
                        spans.push(Span::new(left_pad.clone(), HL_MD_CODE_BLOCK));
                    }
                    spans.push(Span::new(chunk, HL_MD_CODE_BLOCK));
                    if trailing > 0 {
                        spans.push(Span::new(" ".repeat(trailing), HL_MD_CODE_BLOCK));
                    }
                    if !right_pad.is_empty() {
                        spans.push(Span::new(right_pad.clone(), HL_MD_CODE_BLOCK));
                    }
                    out.push(spans);
                }
            }
            Block::BlockQuote(inner) => {
                let mut sub: Vec<Line> = Vec::new();
                for b in inner {
                    render_block(b, cols.saturating_sub(2).max(1), &mut sub);
                }
                for mut line in sub {
                    let mut prefixed = vec![Span::new("│ ", HL_MD_QUOTE_BAR)];
                    prefixed.append(&mut line);
                    out.push(prefixed);
                }
            }
            Block::Rule => {
                let width = cols.max(1);
                let bar: String = std::iter::repeat_n('─', width).collect();
                out.push(vec![Span::new(bar, HL_FOOTER)]);
            }
            Block::Table { header, rows } => {
                render_table(header, rows, cols, out);
            }
            Block::List(ordered, items) => {
                for (i, item_blocks) in items.iter().enumerate() {
                    let marker = if *ordered {
                        format!("{}. ", i + 1)
                    } else {
                        "• ".to_string()
                    };
                    let marker_w = str_width(&marker);
                    let inner_cols = cols.saturating_sub(marker_w).max(1);
                    let mut sub: Vec<Line> = Vec::new();
                    for b in item_blocks {
                        render_block(b, inner_cols, &mut sub);
                    }
                    for (j, mut line) in sub.into_iter().enumerate() {
                        if j == 0 {
                            let mut row = vec![Span::new(marker.clone(), HL_MD_LIST_MARKER)];
                            row.append(&mut line);
                            out.push(row);
                        } else {
                            let pad = " ".repeat(marker_w);
                            let mut row = vec![Span::new(pad, HL_ASSISTANT)];
                            row.append(&mut line);
                            out.push(row);
                        }
                    }
                }
            }
        }
    }

    /// Hard cap on a single column's display width inside a table. Cells
    /// longer than this wrap to multiple visual rows within the column.
    /// Without this one pathological cell would blow the table past the
    /// terminal width.
    const TABLE_COL_MAX: usize = 30;

    /// Render a table to `out`. Empty (no header AND no body) tables emit
    /// nothing. Box-drawing borders use HL_FOOTER (dim grey); cell content
    /// keeps its inline highlights.
    fn render_table(
        header: &[Vec<Span>],
        rows: &[Vec<Vec<Span>>],
        cols: usize,
        out: &mut Vec<Line>,
    ) {
        if header.is_empty() && rows.is_empty() {
            return;
        }

        let ncols = header
            .len()
            .max(rows.iter().map(|r| r.len()).max().unwrap_or(0));
        if ncols == 0 {
            return;
        }

        // Flatten cell spans to display strings for width measurement and
        // truncation. We keep the spans (with their highlights) for the
        // final emit, but width work happens on plain text.
        let header_cells = pad_or_truncate_row(header, ncols);
        let body_cells: Vec<Vec<Vec<Span>>> = rows
            .iter()
            .map(|r| pad_or_truncate_row(r, ncols))
            .collect();

        let mut widths: Vec<usize> = vec![0; ncols];
        for c in 0..ncols {
            widths[c] = widths[c].max(cell_display_width(&header_cells[c]));
            for r in &body_cells {
                widths[c] = widths[c].max(cell_display_width(&r[c]));
            }
            widths[c] = widths[c].clamp(1, TABLE_COL_MAX);
        }

        // Total table width = sum(widths) + per-column padding (2: one space
        // each side) + (ncols + 1) vertical border characters.
        let pad_each: usize = 2;
        let borders = ncols + 1;
        let mut total = widths.iter().sum::<usize>() + ncols * pad_each + borders;
        if total > cols && cols > borders + ncols * pad_each {
            // Shrink columns from the right until we fit. Best-effort — when
            // the budget is too tight we leave columns at width=1.
            let budget = cols - borders - ncols * pad_each;
            let mut excess = widths.iter().sum::<usize>().saturating_sub(budget);
            while excess > 0 {
                let mut shrunk = false;
                for w in widths.iter_mut().rev() {
                    if *w > 1 {
                        *w -= 1;
                        excess -= 1;
                        shrunk = true;
                        if excess == 0 {
                            break;
                        }
                    }
                }
                if !shrunk {
                    break;
                }
            }
            total = widths.iter().sum::<usize>() + ncols * pad_each + borders;
            let _ = total; // keep the calc for future tweaks; suppress unused
        }

        let top = build_border(&widths, '┌', '┬', '┐');
        let mid = build_border(&widths, '├', '┼', '┤');
        let bot = build_border(&widths, '└', '┴', '┘');

        out.push(vec![Span::new(top, HL_FOOTER)]);
        if !header.is_empty() {
            for line in build_wrapped_row(&header_cells, &widths) {
                out.push(line);
            }
            out.push(vec![Span::new(mid.clone(), HL_FOOTER)]);
        }
        for (i, r) in body_cells.iter().enumerate() {
            for line in build_wrapped_row(r, &widths) {
                out.push(line);
            }
            if i + 1 < body_cells.len() {
                out.push(vec![Span::new(mid.clone(), HL_FOOTER)]);
            }
        }
        out.push(vec![Span::new(bot, HL_FOOTER)]);
    }

    fn pad_or_truncate_row(row: &[Vec<Span>], ncols: usize) -> Vec<Vec<Span>> {
        let mut out: Vec<Vec<Span>> = row.to_vec();
        while out.len() < ncols {
            out.push(Vec::new());
        }
        out.truncate(ncols);
        // Replace embedded newlines with spaces in cell content; we don't
        // wrap inside cells in v1.
        for cell in out.iter_mut() {
            for span in cell.iter_mut() {
                if span.text.contains(['\n', '\r']) {
                    span.text = span.text.replace(['\n', '\r'], " ");
                }
            }
        }
        out
    }

    fn cell_display_width(cell: &[Span]) -> usize {
        cell.iter().map(|s| str_width(&s.text)).sum()
    }

    fn build_border(widths: &[usize], left: char, mid: char, right: char) -> String {
        let mut s = String::new();
        s.push(left);
        for (i, w) in widths.iter().enumerate() {
            for _ in 0..(w + 2) {
                s.push('─');
            }
            if i + 1 < widths.len() {
                s.push(mid);
            }
        }
        s.push(right);
        s
    }

    /// Render a single logical row (header or body) as one or more visual
    /// lines. Each cell is flattened to plain text and wrapped to its
    /// column width via `wrap_to_width`; row height = max sub-line count
    /// across cells. Shorter cells are padded with blank-content sub-lines
    /// so the borders line up. Inline cell highlights (bold/italic/code)
    /// are not preserved — wrapping happens on plain text.
    fn build_wrapped_row(cells: &[Vec<Span>], widths: &[usize]) -> Vec<Vec<Span>> {
        let wrapped: Vec<Vec<String>> = cells
            .iter()
            .zip(widths.iter())
            .map(|(cell, w)| wrap_to_width(&cell_plain_text(cell), *w))
            .collect();
        let row_height = wrapped.iter().map(|w| w.len()).max().unwrap_or(1).max(1);

        let mut out: Vec<Vec<Span>> = Vec::with_capacity(row_height);
        for k in 0..row_height {
            let mut line: Vec<Span> = Vec::new();
            line.push(Span::new("│", HL_FOOTER));
            for (i, w) in widths.iter().enumerate() {
                let sub = wrapped
                    .get(i)
                    .and_then(|sl| sl.get(k))
                    .map(String::as_str)
                    .unwrap_or("");
                let sub_w = str_width(sub);
                line.push(Span::new(" ", HL_ASSISTANT));
                if !sub.is_empty() {
                    line.push(Span::new(sub.to_owned(), HL_ASSISTANT));
                }
                if sub_w < *w {
                    line.push(Span::new(" ".repeat(*w - sub_w), HL_ASSISTANT));
                }
                line.push(Span::new(" ", HL_ASSISTANT));
                line.push(Span::new("│", HL_FOOTER));
            }
            out.push(line);
        }
        out
    }

    fn cell_plain_text(cell: &[Span]) -> String {
        let mut s = String::new();
        for span in cell {
            s.push_str(&span.text);
        }
        s
    }
}

/// Wrap a span sequence to `cols`. Word boundaries within a single span are
/// honoured; spans never split mid-word unless a single word exceeds `cols`,
/// in which case it's hard-broken at column boundaries (preserving its
/// highlight). Whitespace at line breaks is dropped.
fn wrap_spans(spans: &[Span], cols: usize) -> Vec<Line> {
    if cols == 0 {
        return vec![spans.to_vec()];
    }
    let mut lines: Vec<Line> = Vec::new();
    let mut current: Line = Vec::new();
    let mut current_w = 0usize;

    let push_word = |current: &mut Line, current_w: &mut usize, word: &str, hl: u32| {
        if word.is_empty() {
            return;
        }
        if let Some(last) = current.last_mut() {
            if last.hl == hl {
                last.text.push_str(word);
                *current_w += str_width(word);
                return;
            }
        }
        current.push(Span::new(word, hl));
        *current_w += str_width(word);
    };

    for span in spans {
        let hl = span.hl;
        // Tokenize on spaces; preserve runs of non-space and emit space
        // separators. CR/LF is treated as space — assistant messages reach
        // us as paragraphs already separated by markdown blocks, so we don't
        // need to honour mid-paragraph hard breaks here.
        let mut parts: Vec<(bool, String)> = Vec::new();
        let mut cur = String::new();
        let mut cur_is_space = false;
        for ch in span.text.chars() {
            let is_space = ch == ' ' || ch == '\t' || ch == '\n' || ch == '\r';
            let normalized = if ch == '\n' || ch == '\r' || ch == '\t' {
                ' '
            } else {
                ch
            };
            if is_space != cur_is_space && !cur.is_empty() {
                parts.push((cur_is_space, std::mem::take(&mut cur)));
            }
            cur_is_space = is_space;
            cur.push(normalized);
        }
        if !cur.is_empty() {
            parts.push((cur_is_space, cur));
        }

        for (is_space, token) in parts {
            if is_space {
                // Spaces only stick to the line if the line is non-empty
                // and we're not at the column limit. They never start a
                // wrapped row.
                if current.is_empty() {
                    continue;
                }
                let token_w = str_width(&token);
                if current_w + token_w > cols {
                    // Drop trailing whitespace; flush.
                    lines.push(std::mem::take(&mut current));
                    current_w = 0;
                } else {
                    push_word(&mut current, &mut current_w, &token, hl);
                }
            } else {
                let token_w = str_width(&token);
                if token_w <= cols {
                    if current_w + token_w > cols && !current.is_empty() {
                        lines.push(std::mem::take(&mut current));
                        current_w = 0;
                    }
                    push_word(&mut current, &mut current_w, &token, hl);
                } else {
                    // Hard-break the long token across multiple rows.
                    let chunks = crate::wrap::split_by_columns(&token, cols);
                    for chunk in chunks {
                        let chunk_w = str_width(&chunk);
                        if current_w + chunk_w > cols && !current.is_empty() {
                            lines.push(std::mem::take(&mut current));
                            current_w = 0;
                        }
                        push_word(&mut current, &mut current_w, &chunk, hl);
                        if current_w >= cols {
                            lines.push(std::mem::take(&mut current));
                            current_w = 0;
                        }
                    }
                }
            }
        }
    }

    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(Vec::new());
    }
    lines
}

/// Split a span sequence at the first column-position where the cumulative
/// display width would exceed `width`. Returns `(prefix, overflow)` where
/// `prefix` fits in `width` columns and `overflow` is everything after,
/// preserving span boundaries. Whitespace at the split boundary is dropped
/// from the overflow side (so wrapped-overflow lines don't start with a
/// stray leading space).
fn split_spans_at_width(spans: &[Span], width: usize) -> (Line, Line) {
    if width == 0 {
        return (Vec::new(), spans.to_vec());
    }
    let mut prefix: Line = Vec::new();
    let mut overflow: Line = Vec::new();
    let mut taken = 0usize;
    let mut split_done = false;
    for span in spans {
        if split_done {
            overflow.push(span.clone());
            continue;
        }
        let span_w = span.width();
        if taken + span_w <= width {
            prefix.push(span.clone());
            taken += span_w;
            continue;
        }
        // Mid-span split: walk chars until adding the next would exceed.
        let mut head = String::new();
        let mut head_w = 0usize;
        let mut tail = String::new();
        let mut into_tail = false;
        for ch in span.text.chars() {
            let cw = char_width(ch);
            if into_tail {
                tail.push(ch);
                continue;
            }
            if taken + head_w + cw > width {
                into_tail = true;
                tail.push(ch);
            } else {
                head.push(ch);
                head_w += cw;
            }
        }
        if !head.is_empty() {
            prefix.push(Span::new(head, span.hl));
        }
        // Drop a single leading whitespace from the tail to avoid a stray
        // space at the start of the wrapped continuation.
        let tail_trimmed = tail.trim_start_matches([' ', '\t']);
        if !tail_trimmed.is_empty() {
            overflow.push(Span::new(tail_trimmed.to_string(), span.hl));
        }
        split_done = true;
    }
    (prefix, overflow)
}

// ---- statusline ------------------------------------------------------------

/// Compose the statusline as a span sequence.
///
/// Layout (left → right): `model · ctx 47k/200k bar 24% · $0.42 · 3 turns ·
/// 12s · auth-indicators · scroll-info`. When `cols` is too narrow for
/// everything, segments are dropped right-to-left in this order: scroll-info,
/// auth-indicators, last-duration, turns, cost, ctx-bar. The model name is
/// preserved as the most identifying piece.
// Eight args is one over clippy's default. Bundling them into a struct
// would bury the call-site (one render pass per frame, per dim change)
// behind a no-value indirection — every field is read directly. Silence.
#[allow(clippy::too_many_arguments)]
pub fn build_status_spans(
    md: &SessionMetadata,
    providers: &[String],
    auth_status: &HashMap<String, AuthStatus>,
    total: u32,
    transcript_rows: u32,
    scroll_offset: u32,
    cols: u32,
    gate_yolo: bool,
) -> Vec<Span> {
    let cols = cols as usize;

    // YOLO is the most safety-relevant fact on screen — pin it to the very
    // first column in the danger hue so the user can't miss it. Every other
    // segment dropper below treats this as an immovable head.
    let yolo_seg: Option<Vec<Span>> = if gate_yolo {
        Some(vec![Span::new("YOLO", HL_STATUS_DANGER)])
    } else {
        None
    };

    // No turn has completed yet. If the model is already known (from a
    // pre-turn `chat.model.set_ack`), lead with it — same shape as a
    // post-turn statusline, just without usage segments. Otherwise fall
    // back to the inviting hint. Auth indicators always surface so the
    // user can see who's logged in pre-first-turn.
    if !md.stats_seen {
        let mut out: Vec<Span> = Vec::new();
        if let Some(yolo) = yolo_seg.clone() {
            out.extend(yolo);
            out.push(Span::new(" │ ", HL_STATUS_DIM));
        }
        if md.model.is_some() {
            out.extend(build_model_segment(md));
        } else {
            out.push(Span::new("Start chatting to see stats", HL_STATUS_DIM));
        }
        if let Some(auth) = build_auth_segment(providers, auth_status) {
            out.push(Span::new(" │ ", HL_STATUS_DIM));
            out.extend(auth);
        }
        if let Some(scroll) = build_scroll_segment(total, transcript_rows, scroll_offset) {
            out.push(Span::new(" │ ", HL_STATUS_DIM));
            out.extend(scroll);
        }
        let width: usize = out.iter().map(|s| s.width()).sum();
        if width > cols {
            out = truncate_spans(&out, cols);
        }
        return out;
    }

    // Build candidate segments in priority order (must-keep first). Each
    // segment carries its own spans and a width.
    let model_seg = build_model_segment(md);
    let ctx_seg = build_ctx_segment(md, cols);
    let cost_seg = build_cost_segment(md);
    let turns_seg = build_turns_segment(md);
    let dur_seg = build_duration_segment(md);
    let speed_seg = build_speed_segment(md);
    let auth_seg = build_auth_segment(providers, auth_status);
    let scroll_seg = build_scroll_segment(total, transcript_rows, scroll_offset);

    let separator = || Span::new(" │ ", HL_STATUS_DIM);
    let sep_w = str_width(" │ ");

    // YOLO sits at index 0 when present — the truncate-from-the-right loop
    // below stops once segs.len() == 1, so on a very narrow terminal we'd
    // keep YOLO and drop the model. That's the right priority — losing the
    // model name on a sliver is recoverable (`/model` re-shows it); losing
    // the danger flag is not.
    let mut segs: Vec<Vec<Span>> = Vec::new();
    if let Some(yolo) = yolo_seg {
        segs.push(yolo);
    }
    segs.push(model_seg);
    for seg in [ctx_seg, cost_seg, turns_seg, dur_seg, speed_seg, auth_seg, scroll_seg]
        .into_iter()
        .flatten()
    {
        segs.push(seg);
    }

    // Drop right-side segments until the total fits. Always keep the
    // first segment (YOLO if active, else model).
    while segs.len() > 1 && total_width(&segs, sep_w) > cols {
        segs.pop();
    }
    // If even the head doesn't fit, truncate it.
    if total_width(&segs, sep_w) > cols {
        segs[0] = truncate_spans(&segs[0], cols);
    }

    let mut out: Vec<Span> = Vec::new();
    for (i, seg) in segs.into_iter().enumerate() {
        if i > 0 {
            out.push(separator());
        }
        out.extend(seg);
    }
    out
}

fn total_width(segs: &[Vec<Span>], sep_w: usize) -> usize {
    let mut total = 0usize;
    for (i, seg) in segs.iter().enumerate() {
        if i > 0 {
            total += sep_w;
        }
        for s in seg {
            total += s.width();
        }
    }
    total
}

fn truncate_spans(spans: &[Span], width: usize) -> Vec<Span> {
    let (prefix, _) = split_spans_at_width(spans, width);
    prefix
}

fn build_model_segment(md: &SessionMetadata) -> Vec<Span> {
    match &md.model {
        Some(m) => {
            let display = m.strip_prefix("claude-").unwrap_or(m);
            vec![Span::new(display.to_owned(), HL_STATUS)]
        }
        None => vec![Span::new("—", HL_STATUS_DIM)],
    }
}

fn build_ctx_segment(md: &SessionMetadata, _cols: usize) -> Option<Vec<Span>> {
    let model = md.model.as_deref()?;
    let max = context_max(model)?;
    let used = md.last_turn_context_tokens?;
    if used == 0 {
        return None;
    }
    let pct = ((used as f64 / max as f64) * 100.0).round() as u32;
    let bar_width: u32 = 8;
    let filled = ((bar_width as f64) * (used as f64 / max as f64)).round() as u32;
    let filled = filled.min(bar_width);
    let empty = bar_width - filled;
    let bar_hl = if pct >= 90 {
        HL_STATUS_DANGER
    } else if pct >= 70 {
        HL_STATUS_WARN
    } else {
        HL_STATUS_BAR_FILL
    };

    let mut spans: Vec<Span> = Vec::new();
    spans.push(Span::new(
        format!("ctx {}/{} ", humanize_tokens(used), humanize_tokens(max)),
        HL_STATUS,
    ));
    spans.push(Span::new("█".repeat(filled as usize), bar_hl));
    spans.push(Span::new("░".repeat(empty as usize), HL_STATUS_DIM));
    spans.push(Span::new(format!(" {pct}%"), HL_STATUS));
    Some(spans)
}

fn build_cost_segment(md: &SessionMetadata) -> Option<Vec<Span>> {
    md.cumulative_cost_usd
        .map(|c| vec![Span::new(format!("${c:.2}"), HL_STATUS)])
}

fn build_turns_segment(md: &SessionMetadata) -> Option<Vec<Span>> {
    md.turns
        .map(|n| vec![Span::new(format!("{n} turns"), HL_STATUS)])
}

fn build_duration_segment(md: &SessionMetadata) -> Option<Vec<Span>> {
    md.last_turn_duration_ms
        .map(|ms| vec![Span::new(humanize_duration_ms(ms), HL_STATUS)])
}

/// Per-turn output throughput. Only renders when the provider supplied
/// both a non-zero duration and a non-zero output count — otherwise the
/// number is meaningless (zero-duration → division by zero; zero tokens →
/// `0 tok/s` is noise on a turn that still has content like cached tool
/// results). Rounds to the nearest integer; sub-1 tok/s is rare on chat
/// turns and would render as `0` anyway.
fn build_speed_segment(md: &SessionMetadata) -> Option<Vec<Span>> {
    let tokens = md.last_turn_output_tokens?;
    let ms = md.last_turn_duration_ms?;
    if tokens == 0 || ms == 0 {
        return None;
    }
    let tok_per_s = ((tokens as f64) * 1000.0 / (ms as f64)).round() as u64;
    Some(vec![Span::new(format!("{tok_per_s} tok/s"), HL_STATUS)])
}

/// Compact per-provider auth indicators for the statusline.
///
/// Format: `name:✓` for connected, `name:?` for `login_required`, `name:!`
/// for any other state (error, etc.). Multiple providers join with a single
/// space. When more than `MAX_AUTH_PROVIDERS_SHOWN` are present, the first
/// few render normally and the rest collapse to a `+N` tail to bound width.
fn build_auth_segment(
    providers: &[String],
    auth_status: &HashMap<String, AuthStatus>,
) -> Option<Vec<Span>> {
    if providers.is_empty() {
        return None;
    }
    const MAX_SHOWN: usize = 3;
    let shown = providers.iter().take(MAX_SHOWN);
    let mut spans: Vec<Span> = Vec::new();
    let mut first = true;
    for name in shown {
        if !first {
            spans.push(Span::new(" ", HL_STATUS_DIM));
        }
        first = false;
        let (marker, hl) = match auth_status.get(name).map(|s| s.state.as_str()) {
            Some("connected") => ("✓", HL_STATUS),
            Some("login_required") => ("?", HL_STATUS_WARN),
            Some(_) => ("!", HL_STATUS_DANGER),
            None => ("·", HL_STATUS_DIM),
        };
        spans.push(Span::new(format!("{name}:"), HL_STATUS_DIM));
        spans.push(Span::new(marker, hl));
    }
    if providers.len() > MAX_SHOWN {
        let extra = providers.len() - MAX_SHOWN;
        spans.push(Span::new(format!(" +{extra}"), HL_STATUS_DIM));
    }
    Some(spans)
}

fn build_scroll_segment(total: u32, transcript_rows: u32, scroll_offset: u32) -> Option<Vec<Span>> {
    let max_offset = total.saturating_sub(transcript_rows);
    if max_offset == 0 {
        return None;
    }
    if scroll_offset == 0 {
        return Some(vec![Span::new("100% ↓ bottom", HL_STATUS)]);
    }
    if scroll_offset >= max_offset {
        return Some(vec![Span::new("0% ↑ PgDn to return", HL_STATUS)]);
    }
    let pct = ((max_offset - scroll_offset) as u64 * 100 / max_offset as u64) as u32;
    Some(vec![Span::new(
        format!("{pct}% ↓ PgDn to return"),
        HL_STATUS,
    )])
}

fn context_max(model: &str) -> Option<u64> {
    if model.contains("opus") || model.contains("sonnet") || model.contains("haiku") {
        Some(200_000)
    } else {
        None
    }
}

// ---- DAG panel -------------------------------------------------------------

/// One row in the DAG panel — either a run header or a node row.
enum PanelLine<'a> {
    Header(&'a DagRunUiState),
    Node(&'a str, &'a crate::state::DagNodeState),
}

/// Build the live DAG-panel rows: one header per tracked run followed by one
/// row per known node, ordered by `(run_id, node_id)`. Honours the `max_rows`
/// cap by truncating overflow with a single `… +K more` row. `now_ms` should
/// be `state.now_ms()` so per-node elapsed times are computed against the
/// same monotonic base the state uses.
///
/// Pure: takes the runs map by reference, no state mutation.
pub(crate) fn build_dag_panel_rows(
    runs: &std::collections::BTreeMap<String, DagRunUiState>,
    width: usize,
    max_rows: usize,
    now_ms: u64,
) -> Vec<Vec<Span>> {
    if runs.is_empty() || width == 0 || max_rows == 0 {
        return Vec::new();
    }

    // First pass: collect every line so we know whether the overflow row is
    // needed before we start emitting.
    let mut lines: Vec<PanelLine> = Vec::new();
    for run in runs.values() {
        lines.push(PanelLine::Header(run));
        for (node_id, node) in &run.nodes {
            lines.push(PanelLine::Node(node_id.as_str(), node));
        }
    }

    let total = lines.len();
    let mut out: Vec<Vec<Span>> = Vec::with_capacity(max_rows);
    if total <= max_rows {
        for line in &lines {
            out.push(panel_line_to_spans(line, width, now_ms));
        }
    } else {
        // Reserve the last row for the overflow message.
        let visible = max_rows.saturating_sub(1);
        for line in lines.iter().take(visible) {
            out.push(panel_line_to_spans(line, width, now_ms));
        }
        let omitted = total - visible;
        let msg = format!("… +{omitted} more");
        let truncated = clip_to_width(&msg, width);
        let used = str_width(&truncated);
        let pad = width.saturating_sub(used);
        let mut text = String::with_capacity(used + pad);
        text.push_str(&truncated);
        for _ in 0..pad {
            text.push(' ');
        }
        out.push(vec![Span::new(text, HL_STATUS_DIM)]);
    }
    out
}

fn panel_line_to_spans(line: &PanelLine, width: usize, now_ms: u64) -> Vec<Span> {
    match line {
        PanelLine::Header(run) => header_row_spans(run, width),
        PanelLine::Node(node_id, node) => node_row_spans(node_id, node, width, now_ms),
    }
}

pub(crate) fn header_row_spans(run: &DagRunUiState, width: usize) -> Vec<Span> {
    // Header: `─ DAG run-1ab8 (M/N nodes) ────`
    let short = run_id_prefix(&run.run_id, 8);
    let done = run
        .nodes
        .values()
        .filter(|n| {
            matches!(
                n.status,
                DagNodeStatus::Done | DagNodeStatus::Error | DagNodeStatus::Skipped
            )
        })
        .count();
    let total = run.total_nodes.max(run.nodes.len());
    let title = format!("─ DAG {short} ({done}/{total} nodes) ");
    let mut text = String::with_capacity(width);
    text.push_str(&clip_to_width(&title, width));
    let used = str_width(&text);
    for _ in used..width {
        text.push('─');
    }
    vec![Span::new(text, HL_FOOTER)]
}

fn node_row_spans(
    node_id: &str,
    node: &crate::state::DagNodeState,
    width: usize,
    now_ms: u64,
) -> Vec<Span> {
    let (marker, status_hl) = match node.status {
        DagNodeStatus::Pending => ("○", HL_STATUS_DIM),
        DagNodeStatus::Running => ("●", HL_STATUS_WARN),
        DagNodeStatus::Done => ("✓", HL_STATUS_OK),
        DagNodeStatus::Error => ("✗", HL_STATUS_DANGER),
        DagNodeStatus::Skipped => ("⊘", HL_STATUS_DIM),
    };
    let marker_hl = status_hl;
    // Elapsed time: for terminal statuses, use `(finished - started)`; for
    // running, use `(now - started)`. Skipped/pending nodes have no
    // meaningful duration.
    let elapsed: Option<u64> = match node.status {
        DagNodeStatus::Pending | DagNodeStatus::Skipped => None,
        DagNodeStatus::Running => Some(now_ms.saturating_sub(node.started_at_ms)),
        DagNodeStatus::Done | DagNodeStatus::Error => node
            .finished_at_ms
            .map(|f| f.saturating_sub(node.started_at_ms)),
    };
    let status_word = match node.status {
        DagNodeStatus::Pending => "pending",
        DagNodeStatus::Running => "running",
        DagNodeStatus::Done => "done",
        DagNodeStatus::Error => "error",
        DagNodeStatus::Skipped => "skipped",
    };

    // Build the row text in two columns:
    //   <marker> <node_id-fixed> <reasoner-fixed> <status> <elapsed?>
    let id_col = clip_to_width_chars(node_id, 6);
    let reasoner_col = clip_to_width_chars(&node.reasoner, 12);
    let elapsed_str = match elapsed {
        Some(ms) => format!(" {}", format_elapsed_ms(ms)),
        None => String::new(),
    };
    // Two-space gutters between columns; padded with spaces for stable column widths.
    let prefix = format!(
        "{marker} {id_col:<6}  {reasoner_col:<12}  {status_word}{elapsed_str}",
    );
    let truncated = clip_to_width(&prefix, width);
    let used = str_width(&truncated);
    let pad = width.saturating_sub(used);
    let mut padded = String::with_capacity(used + pad);
    padded.push_str(&truncated);
    for _ in 0..pad {
        padded.push(' ');
    }

    // Three-segment colouring: marker glyph in the status colour, the
    // id+reasoner stretch in neutral chrome, and the trailing status word
    // (and any elapsed time) back in the status colour so the row reads as
    // green/amber/red end-to-end at a glance.
    let mut spans: Vec<Span> = Vec::with_capacity(3);
    let marker_w = str_width(marker);
    // The status word starts after `marker + space + id_col(6) + 2sp + reasoner_col(12) + 2sp` columns.
    // That's `marker_w + 1 + 6 + 2 + 12 + 2 = marker_w + 23` columns into the row.
    let status_start = marker_w + 23;
    let (head, rest) = split_at_width(&padded, marker_w);
    let (mid, tail) = split_at_width(&rest, status_start - marker_w);
    spans.push(Span::new(head, marker_hl));
    spans.push(Span::new(mid, HL_STATUS));
    spans.push(Span::new(tail, status_hl));
    spans
}

/// Compact node row for the narrow sidebar: `<glyph> <node_id> <elapsed>`.
/// Drops the reasoner column and the spelled-out status word — the marker
/// glyph carries that information, and 28-cols-wide doesn't have room for
/// "running 12.3s" alongside a 12-col reasoner.
pub(crate) fn node_row_spans_compact(
    node_id: &str,
    node: &crate::state::DagNodeState,
    width: usize,
    now_ms: u64,
) -> Vec<Span> {
    let (marker, status_hl) = match node.status {
        DagNodeStatus::Pending => ("○", HL_STATUS_DIM),
        DagNodeStatus::Running => ("●", HL_STATUS_WARN),
        DagNodeStatus::Done => ("✓", HL_STATUS_OK),
        DagNodeStatus::Error => ("✗", HL_STATUS_DANGER),
        DagNodeStatus::Skipped => ("⊘", HL_STATUS_DIM),
    };
    let marker_hl = status_hl;
    let elapsed: Option<u64> = match node.status {
        DagNodeStatus::Pending | DagNodeStatus::Skipped => None,
        DagNodeStatus::Running => Some(now_ms.saturating_sub(node.started_at_ms)),
        DagNodeStatus::Done | DagNodeStatus::Error => node
            .finished_at_ms
            .map(|f| f.saturating_sub(node.started_at_ms)),
    };
    let elapsed_str = match elapsed {
        Some(ms) => format_elapsed_ms(ms),
        None => String::new(),
    };

    // Layout: `<marker> <id…> <pad> <elapsed>` where elapsed sits at the
    // right. We compute: marker_w + 1 (gap) + id_w + ... + elapsed_w == width.
    let marker_w = str_width(marker);
    let elapsed_w = str_width(&elapsed_str);
    // 1 col gap after marker, 1 col gap before elapsed when elapsed is shown.
    let elapsed_pad = if elapsed_w > 0 { 1 } else { 0 };
    let avail_for_id = width
        .saturating_sub(marker_w + 1) // marker + gap
        .saturating_sub(elapsed_w + elapsed_pad);
    let id_clipped = clip_to_width(node_id, avail_for_id);
    let id_used = str_width(&id_clipped);

    // Build: marker · " " · id · pad · elapsed
    let mut padded = String::with_capacity(width);
    padded.push_str(&id_clipped);
    let used_so_far = marker_w + 1 + id_used + elapsed_w + elapsed_pad;
    let inner_pad = width.saturating_sub(used_so_far);
    for _ in 0..inner_pad {
        padded.push(' ');
    }
    if elapsed_pad > 0 {
        padded.push(' ');
    }
    padded.push_str(&elapsed_str);

    // Compact layout: the elapsed/status-cue is the entire trailing run, so
    // colour it with the status colour for end-to-end readability.
    vec![
        Span::new(marker.to_string(), marker_hl),
        Span::new(" ", HL_STATUS),
        Span::new(padded, status_hl),
    ]
}

/// Compact run-header for the narrow sidebar: `─ <prefix> (M/N) ` then
/// dashes to fill. Drops the "DAG " label (the widget title already has it)
/// and the " nodes" suffix.
pub(crate) fn run_id_prefix_spans(run: &DagRunUiState, width: usize) -> Vec<Span> {
    let short = run_id_prefix(&run.run_id, 8);
    let done = run
        .nodes
        .values()
        .filter(|n| {
            matches!(
                n.status,
                DagNodeStatus::Done | DagNodeStatus::Error | DagNodeStatus::Skipped
            )
        })
        .count();
    let total = run.total_nodes.max(run.nodes.len());
    let title = format!("─ {short} ({done}/{total}) ");
    let mut text = String::with_capacity(width);
    text.push_str(&clip_to_width(&title, width));
    let used = str_width(&text);
    for _ in used..width {
        text.push('─');
    }
    vec![Span::new(text, HL_FOOTER)]
}

/// Truncate `s` to `max_chars` characters (not display columns). Used for
/// the DAG panel's narrow id/reasoner columns where we want a stable char
/// count regardless of east-asian width.
fn clip_to_width_chars(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i >= max_chars {
            break;
        }
        out.push(ch);
    }
    out
}

/// Split a UTF-8 string after the first `n_cols` display columns.
/// Returns `(head, tail)` where `str_width(head) == n_cols` (or the whole
/// string when `n_cols` exceeds the string width). Used to give the DAG
/// panel its per-marker highlight.
fn split_at_width(s: &str, n_cols: usize) -> (String, String) {
    let mut head = String::new();
    let mut tail = String::new();
    let mut w = 0usize;
    let mut split_done = false;
    for ch in s.chars() {
        let cw = char_width(ch);
        if !split_done && w + cw <= n_cols {
            head.push(ch);
            w += cw;
            if w == n_cols {
                split_done = true;
            }
        } else {
            split_done = true;
            tail.push(ch);
        }
    }
    (head, tail)
}

/// Format milliseconds as a compact "running 2.3s" duration. <1s shows
/// "0.0s" precision; ≥60s collapses to "1m05s" matching `humanize_duration_ms`
/// so two-digit minutes don't blow out the column budget.
fn format_elapsed_ms(ms: u64) -> String {
    if ms >= 60_000 {
        let s = ms / 1_000;
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        // One decimal place for sub-minute durations: 12_345ms → "12.3s".
        let tenths = (ms + 50) / 100; // round to nearest tenth
        format!("{}.{}s", tenths / 10, tenths % 10)
    }
}

/// Take the first `n` chars of a uuid-shaped run id for compact display.
fn run_id_prefix(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn humanize_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
}

fn humanize_duration_ms(ms: u64) -> String {
    if ms >= 60_000 {
        let s = ms / 1_000;
        format!("{}m{}s", s / 60, s % 60)
    } else if ms >= 1_000 {
        format!("{}s", ms / 1_000)
    } else {
        format!("{ms}ms")
    }
}

// ---- tool I/O rendering ----------------------------------------------------

/// Maximum body lines shown per `input:` / `output:` block in the expanded
/// tool view. Anything longer is truncated with a hint to collapse.
const TOOL_EXPANDED_BODY_CAP: usize = 20;

/// Render a one-line collapsed tool summary: `▸ <Name>(<truncated input>)`,
/// or `▸ <Name>(<…>) … running` while still in flight.
fn tool_collapsed_line(payload: &crate::state::ToolPayload, cols: usize) -> Line {
    let input_value: Option<Value> = if payload.input_json.is_empty() {
        None
    } else {
        serde_json::from_str(&payload.input_json).ok()
    };
    let salient = tool_salient_summary(&payload.name, input_value.as_ref());

    // Build the head: `▸ Name`. Tail is the parenthesised summary +
    // optional running marker. We size the tail against the available cols
    // so the whole row fits in one line.
    let head = format!("▸ {}", payload.name);
    let head_w = str_width(&head);

    let running = payload.output.is_none();
    let suffix = if running { " …" } else { "" };
    let suffix_w = str_width(suffix);

    let avail_for_tail = cols.saturating_sub(head_w + suffix_w);
    // Reserve "(" and ")" — 2 cols.
    let tail_inner_budget = avail_for_tail.saturating_sub(2);

    let tail_text = match salient {
        Some(s) if tail_inner_budget > 1 => format!("({})", truncate(&s, tail_inner_budget)),
        _ => String::new(),
    };

    let mut spans = Vec::with_capacity(4);
    // Errored tools are signalled by the dedicated `  error` sub-line below;
    // the tool name itself stays in the neutral system color to avoid two
    // redundant cues for one signal.
    spans.push(Span::new(head, HL_SYSTEM));
    if !tail_text.is_empty() {
        spans.push(Span::new(tail_text, HL_FOOTER));
    }
    if !suffix.is_empty() {
        spans.push(Span::new(suffix.to_string(), HL_FOOTER));
    }
    spans
}

/// Pick the salient input field for the collapsed view. Mirrors the per-
/// tool dispatch in [`tool_start_line`] but returns the raw value (no
/// `Name: ` prefix) so the caller can compose the parenthesised summary.
///
/// Both claude-flavoured (`Bash`, `Read`, …) and basic-tools snake_case
/// names (`bash`, `read_file`, `write_file`) are recognised explicitly so
/// the collapsed line shows path/command rather than whatever the input-map
/// iteration order happens to surface first — important for `write_file`,
/// where the fallback could grab a multi-KB `content` blob.
fn tool_salient_summary(name: &str, input: Option<&Value>) -> Option<String> {
    let salient = match name {
        "Bash" | "bash" => input.and_then(|v| v.get("command")).and_then(Value::as_str),
        "Read" | "Edit" | "Write" | "MultiEdit" => input
            .and_then(|v| v.get("file_path"))
            .and_then(Value::as_str),
        "read_file" | "write_file" => {
            input.and_then(|v| v.get("path")).and_then(Value::as_str)
        }
        "Grep" | "Glob" => input.and_then(|v| v.get("pattern")).and_then(Value::as_str),
        _ => None,
    };
    salient.map(str::to_owned).or_else(|| {
        // Fall back to the first short string field of the input map, so
        // unknown tools still surface *something*.
        let obj = input.and_then(Value::as_object)?;
        for (_k, v) in obj.iter() {
            if let Some(s) = v.as_str() {
                if !s.is_empty() {
                    return Some(s.to_owned());
                }
            }
        }
        None
    })
}

/// Render the expanded tool view as multi-row spans:
/// ```text
/// ▼ <Name>(<salient>)
///   <output text indented; capped at TOOL_EXPANDED_BODY_CAP lines>
/// ```
///
/// The salient input field rides on the header (same summary the collapsed
/// row uses) so we don't repeat it as a redundant `input:` block — that
/// block was just `{"command": "ls"}` for the common case, three lines of
/// noise wrapping the one piece of info already in the title. For tools
/// whose input has multiple meaningful fields the salient picker still
/// surfaces the most informative one; the rest is implicit (the LLM
/// already knows what it sent).
fn tool_expanded_lines(payload: &crate::state::ToolPayload, cols: usize) -> Vec<Line> {
    let mut out: Vec<Line> = Vec::new();
    let header_hl = if payload.error {
        HL_STATUS_DANGER
    } else {
        HL_MD_HEADING
    };

    let input_value: Option<Value> = if payload.input_json.is_empty() {
        None
    } else {
        serde_json::from_str(&payload.input_json).ok()
    };
    let salient = tool_salient_summary(&payload.name, input_value.as_ref());
    let header_text = match salient {
        Some(s) => {
            // Cap the salient tail so a giant single-field input (e.g. a
            // pasted multi-line bash heredoc) doesn't blow out the header.
            let tail_budget = cols.saturating_sub(payload.name.chars().count() + 4);
            format!("▼ {}({})", payload.name, truncate(&s, tail_budget.max(1)))
        }
        None => format!("▼ {}", payload.name),
    };
    out.push(vec![Span::new(header_text, header_hl)]);

    let inner_w = cols.saturating_sub(2).max(1);
    let (label, body_text) = match payload.output.as_deref() {
        None => ("  running...", None),
        Some(o) => ("  output:", Some(o)),
    };
    out.push(vec![Span::new(label.to_string(), HL_FOOTER)]);
    if let Some(text) = body_text {
        for line in body_lines(text, inner_w, TOOL_EXPANDED_BODY_CAP) {
            out.push(vec![
                Span::new("  ".to_string(), HL_FOOTER),
                Span::new(line, HL_MD_CODE_BLOCK),
            ]);
        }
    }
    out
}

/// Wrap `body` to `inner_w` columns, capping at `cap` rows. When the body
/// has more rows than `cap` we replace the trailing rows with a hint that
/// tells the user how many were dropped.
fn body_lines(body: &str, inner_w: usize, cap: usize) -> Vec<String> {
    let wrapped = wrap_to_width(body, inner_w);
    if wrapped.len() <= cap {
        return wrapped;
    }
    let dropped = wrapped.len() - cap + 1;
    let mut out: Vec<String> = wrapped.into_iter().take(cap - 1).collect();
    out.push(format!("… ({dropped} more lines, ctrl+o to collapse)"));
    out
}

fn truncate(s: &str, max: usize) -> String {
    if str_width(s) <= max {
        return s.to_owned();
    }
    let mut out = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = char_width(ch);
        if w + cw + 1 > max {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

// ---- input-line rendering --------------------------------------------------

pub fn render_input_wrapped(
    buffer: &str,
    cursor_char_offset: usize,
    cols: u32,
    hpad: u32,
) -> (Vec<String>, u32, u32) {
    // The buffer may contain literal `\n` (Shift+Enter, paste). Each segment
    // between newlines word-wraps independently; segments concatenate into
    // a single visual list whose cursor offset is tracked across boundaries.
    let prefix = "│ ";
    let prefix_w = str_width(prefix);
    let cols_usize = cols as usize;
    let hpad_usize = hpad as usize;

    if cols_usize == 0 {
        return (vec![String::new()], 0, 0);
    }
    let avail = cols_usize.saturating_sub(2 * hpad_usize);
    if avail <= prefix_w {
        let text: String = prefix.chars().take(avail).collect();
        return (vec![text], 0, hpad);
    }
    let inner_w = avail - prefix_w;

    // Split logical lines on '\n'. `chars().count()` per segment lets us
    // place the cursor when the offset lands on a newline (treated as the
    // *start* of the next segment).
    let segments: Vec<&str> = buffer.split('\n').collect();
    let mut inner_lines: Vec<String> = Vec::new();
    let mut cursor_line: u32 = 0;
    let mut cursor_byte_in_line: usize = 0;
    let mut cursor_set = false;

    let mut chars_consumed: usize = 0;
    for (seg_idx, segment) in segments.iter().enumerate() {
        // Wrap this segment exactly like the legacy single-line implementation.
        let seg_first_visual_line = inner_lines.len() as u32;
        let seg_chars = segment.chars().count();

        // If the cursor lives within this segment, mark it as we walk.
        let cursor_in_segment = !cursor_set
            && cursor_char_offset >= chars_consumed
            && cursor_char_offset <= chars_consumed + seg_chars;
        let cursor_offset_in_seg = if cursor_in_segment {
            Some(cursor_char_offset - chars_consumed)
        } else {
            None
        };

        // Cursor anchored at the very start of an empty / about-to-be-walked
        // segment. Set it now so a subsequent break can shift it.
        if cursor_offset_in_seg == Some(0) {
            cursor_set = true;
            cursor_line = seg_first_visual_line;
            cursor_byte_in_line = 0;
        }

        let mut current = String::new();
        let mut current_w = 0usize;
        let mut last_space_byte: Option<usize> = None;
        let mut consumed_in_seg = 0usize;

        for c in segment.chars() {
            let cw = char_width(c);

            if cw > 0 && current_w + cw > inner_w {
                if let Some(sp_byte) = last_space_byte.take() {
                    let split = sp_byte + 1;
                    debug_assert!(current.is_char_boundary(sp_byte));
                    debug_assert!(current.is_char_boundary(split));
                    let carry: String = current[split..].to_owned();
                    let carry_w = str_width(&carry);
                    current.truncate(sp_byte);
                    if cursor_set
                        && cursor_line as usize == inner_lines.len()
                        && cursor_byte_in_line > sp_byte
                    {
                        cursor_line += 1;
                        cursor_byte_in_line = cursor_byte_in_line.saturating_sub(split);
                    }
                    inner_lines.push(std::mem::take(&mut current));
                    current = carry;
                    current_w = carry_w;
                } else {
                    inner_lines.push(std::mem::take(&mut current));
                    current_w = 0;
                }
            }

            if c == ' ' {
                last_space_byte = Some(current.len());
            }

            current.push(c);
            current_w += cw;
            consumed_in_seg += 1;

            if cursor_offset_in_seg == Some(consumed_in_seg) {
                cursor_set = true;
                cursor_line = inner_lines.len() as u32;
                cursor_byte_in_line = current.len();
            }
        }

        inner_lines.push(current);

        // Account for the consumed segment plus the implicit '\n' separator
        // that segments[seg_idx+1] follows. The last segment has no trailing
        // newline so we skip the +1.
        chars_consumed += seg_chars;
        if seg_idx + 1 < segments.len() {
            chars_consumed += 1;
        }
    }

    if !cursor_set {
        cursor_line = inner_lines.len().saturating_sub(1) as u32;
        cursor_byte_in_line = inner_lines.last().map(|s| s.len()).unwrap_or(0);
    }

    let cursor_line_str = inner_lines
        .get(cursor_line as usize)
        .map(String::as_str)
        .unwrap_or("");
    let target = cursor_byte_in_line.min(cursor_line_str.len());
    let mut cursor_col_inner: usize = 0;
    for (i, ch) in cursor_line_str.char_indices() {
        if i >= target {
            break;
        }
        cursor_col_inner += char_width(ch);
    }
    let cursor_col = (hpad + prefix_w as u32 + cursor_col_inner as u32).min(cols.saturating_sub(1));

    let lines: Vec<String> = inner_lines
        .into_iter()
        .map(|s| format!("{prefix}{s}"))
        .collect();

    (lines, cursor_line, cursor_col)
}

// ---- grid event helpers ----------------------------------------------------

#[allow(dead_code)]
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

/// Emit a grid line built from a span sequence. Each character becomes one
/// cell; the first cell of each contiguous-hl run carries the `hl_id`,
/// subsequent cells inherit per spec. The row is padded to `cols` with a
/// trailing repeat-form space cell.
fn grid_line(row: u32, cols: u32, spans: &[Span]) -> Map<String, Value> {
    let mut cells: Vec<Value> = Vec::new();
    let mut used: u32 = 0;
    let mut current_hl: Option<u32> = None;
    for span in spans {
        if span.text.is_empty() {
            continue;
        }
        for ch in span.text.chars() {
            let cw = char_width(ch);
            if cw == 0 {
                continue;
            }
            let cw32 = cw as u32;
            if used.saturating_add(cw32) > cols {
                break;
            }
            let mut buf = [0u8; 4];
            let s = ch.encode_utf8(&mut buf).to_owned();
            let cell = if current_hl != Some(span.hl) {
                current_hl = Some(span.hl);
                Value::Array(vec![Value::String(s), Value::Number(span.hl.into())])
            } else {
                Value::Array(vec![Value::String(s)])
            };
            cells.push(cell);
            used += cw32;
        }
    }

    if cells.is_empty() {
        // Even an empty line needs a leading hl=0 cell so consumers can
        // anchor the row's default highlight.
        cells.push(Value::Array(vec![
            Value::String(" ".into()),
            Value::Number(0u32.into()),
        ]));
        used = 1;
    }
    let padding = cols.saturating_sub(used);
    if padding > 0 {
        cells.push(Value::Array(vec![
            Value::String(" ".into()),
            Value::Number(0u32.into()),
            Value::Number(padding.into()),
        ]));
    }

    let mut m = Map::new();
    m.insert("kind".into(), Value::String("nefor-tui.grid.line".into()));
    m.insert("grid".into(), Value::Number(1u32.into()));
    m.insert("row".into(), Value::Number(row.into()));
    m.insert("col_start".into(), Value::Number(0u32.into()));
    m.insert("cells".into(), Value::Array(cells));
    m
}

#[allow(dead_code)]
fn grid_line_blank(row: u32, cols: u32) -> Map<String, Value> {
    grid_line(row, cols, &[Span::new("", 0)])
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

    #[allow(dead_code)]
    fn row_hl(row: &Map<String, Value>) -> u64 {
        let cells = row["cells"].as_array().expect("cells");
        cells[0][1].as_u64().expect("hl_id on first cell")
    }

    #[test]
    fn help_body_line_for_new_lists_clear_alias_inline() {
        // `/new` advertises `(/clear)` on the same line — the alias renders
        // alongside the canonical name with the hint preserved.
        let lines = help_body_lines(20);
        let new_line = lines
            .iter()
            .find(|l| l.starts_with("/new"))
            .expect("expected a /new help row");
        assert!(
            new_line.contains("/new (/clear)"),
            "alias must render inline next to the canonical name: {new_line:?}"
        );
        assert!(
            new_line.contains("start a fresh chat (clears transcript)"),
            "hint must follow the label: {new_line:?}"
        );
        // No second `/clear` row — the alias is grouped, not duplicated.
        let clear_lines: Vec<&String> = lines
            .iter()
            .filter(|l| l.trim_start().starts_with("/clear"))
            .collect();
        assert!(
            clear_lines.is_empty(),
            "alias should not get its own row: {clear_lines:?}"
        );
    }

    #[test]
    fn palette_defines_one_per_constant() {
        let defs = palette_defines();
        // Must include every constant we hand out. HL_SELECTION moved to
        // nefor-tui (which now owns full-screen selection); HL_BANNER_DANGER
        // dropped when the misconfigured-harness banner was replaced by the
        // live "[thinking… Ns]" placeholder. HL_STATUS_OK added for the DAG
        // widget's "done" colour.
        assert_eq!(defs.len(), 20);
        for d in &defs {
            assert_eq!(d["kind"], Value::String("nefor-tui.hl_attr_define".into()));
            assert!(d.get("id").is_some());
            assert!(d["rgb"].is_object());
        }
    }

    #[test]
    fn empty_transcript_emits_blank_rows_and_input() {
        let mut s = state_with(vec![], "", Dims { cols: 20, rows: 4 });
        let events = render_frame(&mut s);
        // 4-row layout w/ vpad_top=1: vpad + transcript + input + status =
        // 4 line events + cursor_goto + flush = 6.
        // No `grid.clear` — diff rendering owns the cell state directly.
        assert_eq!(events.len(), 6);
        assert!(
            events
                .iter()
                .all(|e| e["kind"] != Value::String("nefor-tui.grid.clear".into())),
            "diff renderer must not emit grid.clear"
        );
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
        // 12 rows leaves room for vpad_top + user 3-row block + inter-turn
        // blank + assistant body + vpad_input_top + input_top_bar + input +
        // input_bot_bar + status + vpad_bottom. The user block top/bottom
        // rules are horizontal rules (`╭───…`, `╰───…`) instead of bare `│`.
        let mut s = state_with(
            vec![(Role::User, "hello"), (Role::Assistant, "hi there")],
            "",
            Dims { cols: 40, rows: 12 },
        );
        let events = render_frame(&mut s);
        let lines = events
            .iter()
            .filter(|e| e["kind"] == Value::String("nefor-tui.grid.line".into()))
            .collect::<Vec<_>>();
        let texts: Vec<String> = lines.iter().map(|l| row_text(l)).collect();
        let joined = texts.join("\n");
        // Top rule starts with `╭` and is followed by `─`s.
        assert!(
            texts.iter().any(|t| t.trim_start().starts_with("╭─")),
            "top rule missing: {joined}"
        );
        // Content row keeps the left bar prefix.
        assert!(
            texts.iter().any(|t| t == "  │ hello"),
            "user content row missing: {joined}"
        );
        // Bottom rule starts with `╰`.
        assert!(
            texts.iter().any(|t| t.trim_start().starts_with("╰─")),
            "bottom rule missing: {joined}"
        );
        // Assistant body still renders, with no left bar.
        assert!(
            texts.iter().any(|t| t.contains("hi there")),
            "assistant body missing: {joined}"
        );
    }

    #[test]
    fn system_entries_get_bracketed() {
        let mut s = state_with(
            vec![(Role::System, "tool: read")],
            "",
            Dims { cols: 30, rows: 3 },
        );
        let events = render_frame(&mut s);
        // 3 rows: vpad disabled. Row 0 transcript, row 1 input, row 2
        // status. hpad=2 prepends "  " before "[tool: read]".
        let row0 = &events[0];
        assert_eq!(row_text(row0), "  [tool: read]");
        // Bracketed system text follows the leading hpad span (hl=0).
        let cells = row0["cells"].as_array().expect("cells");
        let any_system = cells.iter().any(|c| {
            let arr = c.as_array().expect("cell");
            arr.len() < 3 && arr.get(1).and_then(Value::as_u64) == Some(HL_SYSTEM as u64)
        });
        assert!(any_system);
    }

    #[test]
    fn input_line_carries_cursor_goto() {
        let mut s = state_with(vec![], "hello", Dims { cols: 20, rows: 3 });
        let events = render_frame(&mut s);
        let goto = events
            .iter()
            .find(|e| e["kind"] == Value::String("nefor-tui.grid.cursor_goto".into()))
            .expect("cursor_goto emitted");
        // 3 rows (vpad disabled): row 0 transcript, row 1 input, row 2
        // status. Cursor goes on the input row at col hpad(2) + prefix(2)
        // + "hello"(5) = 9.
        assert_eq!(goto["col"], Value::Number(9u32.into()));
        assert_eq!(goto["row"], Value::Number(1u32.into()));
    }

    #[test]
    fn full_frame_ends_with_flush() {
        let mut s = state_with(vec![], "", Dims { cols: 10, rows: 3 });
        let events = render_frame(&mut s);
        assert_eq!(
            events.last().expect("non-empty")["kind"],
            Value::String("nefor-tui.grid.flush".into())
        );
    }

    #[test]
    fn render_input_wrapped_short_buffer_single_line() {
        let (lines, cline, col) = render_input_wrapped("abc", 3, 10, 0);
        assert_eq!(lines, vec!["│ abc".to_string()]);
        assert_eq!(cline, 0);
        assert_eq!(col, 5);
    }

    #[test]
    fn render_input_wrapped_empty_buffer_cursor_at_prefix_end() {
        let (lines, cline, col) = render_input_wrapped("", 0, 10, 0);
        assert_eq!(lines, vec!["│ ".to_string()]);
        assert_eq!(cline, 0);
        assert_eq!(col, 2);
    }

    #[test]
    fn render_input_wrapped_multibyte_cursor_does_not_panic() {
        // "café" — é is a 2-byte codepoint. Cursor at char-offset 3 (between
        // 'f' and 'é'); 4 (after 'é'); past-end. None should panic, and the
        // visual column must reflect char widths, not byte counts.
        let prefix_w = 2; // "│ "
        let buf = "café";
        for (cursor, expected_inner) in [(3, 3), (4, 4)] {
            let (lines, cline, col) = render_input_wrapped(buf, cursor, 20, 0);
            assert_eq!(lines.len(), 1, "cursor={cursor} lines={lines:?}");
            assert_eq!(cline, 0, "cursor={cursor}");
            assert_eq!(
                col,
                (prefix_w + expected_inner) as u32,
                "cursor={cursor} got col={col}"
            );
        }

        // Emoji + CJK to exercise wider multi-byte chars too.
        let mixed = "a😀漢";
        let (_l, _r, _c) = render_input_wrapped(mixed, 3, 20, 0);
    }

    fn no_auth() -> (Vec<String>, HashMap<String, AuthStatus>) {
        (Vec::new(), HashMap::new())
    }

    #[test]
    fn status_with_no_metadata_shows_invite_hint() {
        let md = SessionMetadata::default();
        let (p, a) = no_auth();
        let spans = build_status_spans(&md, &p, &a, 0, 10, 0, 80, false);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "Start chatting to see stats");
        assert_eq!(spans[0].hl, HL_STATUS_DIM);
    }

    #[test]
    fn status_with_full_metadata_renders_full_layout() {
        let md = SessionMetadata {
            stats_seen: true,
            model: Some("claude-opus-4-7".into()),
            turns: Some(3),
            cumulative_cost_usd: Some(0.42),
            last_turn_context_tokens: Some(47_000),
            last_turn_duration_ms: Some(12_000),
            ..Default::default()
        };

        let (p, a) = no_auth();
        let spans = build_status_spans(&md, &p, &a, 0, 10, 0, 200, false);
        let joined: String = spans.iter().map(|s| s.text.as_str()).collect();
        // claude- prefix stripped.
        assert!(joined.starts_with("opus-4-7"), "got {joined:?}");
        assert!(
            joined.contains("ctx 47k/200k"),
            "ctx bar missing: {joined:?}"
        );
        assert!(joined.contains("$0.42"), "cost missing: {joined:?}");
        assert!(joined.contains("3 turns"), "turns missing: {joined:?}");
        assert!(joined.contains("12s"), "duration missing: {joined:?}");
    }

    #[test]
    fn status_drops_right_segments_under_tight_width() {
        let md = SessionMetadata {
            stats_seen: true,
            model: Some("claude-opus-4-7".into()),
            turns: Some(3),
            cumulative_cost_usd: Some(0.42),
            last_turn_duration_ms: Some(12_000),
            ..Default::default()
        };
        // Only enough room for the model and maybe the cost.
        let (p, a) = no_auth();
        let spans = build_status_spans(&md, &p, &a, 0, 10, 0, 18, false);
        let joined: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert!(
            joined.starts_with("opus-4-7"),
            "model preserved: {joined:?}"
        );
        // Duration and turns must drop before cost.
        assert!(
            !joined.contains("12s"),
            "duration should drop under tight width: {joined:?}"
        );
    }

    #[test]
    fn ctx_bar_uses_warn_color_above_70_percent() {
        let md = SessionMetadata {
            stats_seen: true,
            model: Some("claude-opus-4-7".into()),
            last_turn_context_tokens: Some(150_000), // 75%
            ..Default::default()
        };
        let (p, a) = no_auth();
        let spans = build_status_spans(&md, &p, &a, 0, 10, 0, 200, false);
        let bar = spans.iter().find(|s| s.text.contains('█'));
        assert!(bar.is_some(), "filled bar present: {spans:?}");
        assert_eq!(bar.unwrap().hl, HL_STATUS_WARN);
    }

    #[test]
    fn ctx_bar_uses_danger_color_above_90_percent() {
        let md = SessionMetadata {
            stats_seen: true,
            model: Some("claude-opus-4-7".into()),
            last_turn_context_tokens: Some(190_000), // 95%
            ..Default::default()
        };
        let (p, a) = no_auth();
        let spans = build_status_spans(&md, &p, &a, 0, 10, 0, 200, false);
        let bar = spans.iter().find(|s| s.text.contains('█'));
        assert_eq!(bar.unwrap().hl, HL_STATUS_DANGER);
    }

    #[test]
    fn statusline_shows_provider_auth_indicators() {
        let md = SessionMetadata {
            stats_seen: true,
            model: Some("claude-opus-4-7".into()),
            ..Default::default()
        };
        let providers = vec!["ollama".to_owned(), "anthropic".to_owned()];
        let mut auth: HashMap<String, AuthStatus> = HashMap::new();
        auth.insert(
            "ollama".into(),
            AuthStatus {
                state: "connected".into(),
                message: None,
            },
        );
        auth.insert(
            "anthropic".into(),
            AuthStatus {
                state: "login_required".into(),
                message: None,
            },
        );

        let spans = build_status_spans(&md, &providers, &auth, 0, 10, 0, 200, false);
        let joined: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert!(joined.contains("ollama:"), "indicator missing: {joined:?}");
        assert!(joined.contains("anthropic:"), "indicator missing: {joined:?}");
        // Connected → ✓, login_required → ?
        assert!(joined.contains('✓'), "connected marker missing: {joined:?}");
        assert!(joined.contains('?'), "login-required marker missing: {joined:?}");

        // The login_required marker should be HL_STATUS_WARN; connected → HL_STATUS.
        let warn_marker = spans
            .iter()
            .find(|s| s.text == "?")
            .expect("warn marker present");
        assert_eq!(warn_marker.hl, HL_STATUS_WARN);
        let ok_marker = spans
            .iter()
            .find(|s| s.text == "✓")
            .expect("ok marker present");
        assert_eq!(ok_marker.hl, HL_STATUS);
    }

    #[test]
    fn statusline_truncates_many_providers() {
        let md = SessionMetadata {
            stats_seen: true,
            model: Some("claude-opus-4-7".into()),
            ..Default::default()
        };
        let providers: Vec<String> = (0..7).map(|i| format!("p{i}")).collect();
        let mut auth: HashMap<String, AuthStatus> = HashMap::new();
        for p in &providers {
            auth.insert(
                p.clone(),
                AuthStatus {
                    state: "connected".into(),
                    message: None,
                },
            );
        }
        let spans = build_status_spans(&md, &providers, &auth, 0, 10, 0, 200, false);
        let joined: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert!(joined.contains("p0:"), "first provider shown: {joined:?}");
        assert!(joined.contains("+4"), "overflow marker present: {joined:?}");
        assert!(
            !joined.contains("p6:"),
            "tail providers must collapse: {joined:?}"
        );
    }

    #[test]
    fn statusline_renders_tokens_per_second() {
        let md = SessionMetadata {
            stats_seen: true,
            model: Some("gemma4:latest".into()),
            last_turn_duration_ms: Some(10_000),
            last_turn_output_tokens: Some(450),
            ..Default::default()
        };
        let (p, a) = no_auth();
        let spans = build_status_spans(&md, &p, &a, 0, 10, 0, 200, false);
        let joined: String = spans.iter().map(|s| s.text.as_str()).collect();
        // 450 tokens / 10s = 45 tok/s.
        assert!(joined.contains("45 tok/s"), "tok/s missing: {joined:?}");
    }

    #[test]
    fn statusline_omits_tokens_per_second_when_data_missing() {
        let md = SessionMetadata {
            stats_seen: true,
            model: Some("x".into()),
            last_turn_duration_ms: Some(10_000),
            // last_turn_output_tokens absent.
            ..Default::default()
        };
        let (p, a) = no_auth();
        let spans = build_status_spans(&md, &p, &a, 0, 10, 0, 200, false);
        let joined: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert!(!joined.contains("tok/s"), "tok/s leaked: {joined:?}");
    }

    #[test]
    fn statusline_omits_tokens_per_second_when_zero_duration() {
        let md = SessionMetadata {
            stats_seen: true,
            model: Some("x".into()),
            last_turn_duration_ms: Some(0),
            last_turn_output_tokens: Some(120),
            ..Default::default()
        };
        let (p, a) = no_auth();
        let spans = build_status_spans(&md, &p, &a, 0, 10, 0, 200, false);
        let joined: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert!(!joined.contains("tok/s"), "tok/s should not divide by zero");
    }

    #[test]
    fn statusline_yolo_segment_renders_first_in_danger_color() {
        let md = SessionMetadata {
            stats_seen: true,
            model: Some("claude-opus-4-7".into()),
            ..Default::default()
        };
        let (p, a) = no_auth();
        let spans = build_status_spans(&md, &p, &a, 0, 10, 0, 200, true);
        // First segment is YOLO in danger hue.
        assert_eq!(spans[0].text, "YOLO");
        assert_eq!(spans[0].hl, HL_STATUS_DANGER);
        // Model still rendered after the separator.
        let joined: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert!(joined.contains("opus-4-7"), "model preserved: {joined:?}");
    }

    #[test]
    fn statusline_yolo_segment_present_pre_first_turn() {
        // Pre-first-turn path is a different branch — verify it too.
        let md = SessionMetadata::default();
        let (p, a) = no_auth();
        let spans = build_status_spans(&md, &p, &a, 0, 10, 0, 200, true);
        assert_eq!(spans[0].text, "YOLO");
        assert_eq!(spans[0].hl, HL_STATUS_DANGER);
    }

    #[test]
    fn statusline_yolo_kept_when_other_segments_dropped_under_tight_width() {
        let md = SessionMetadata {
            stats_seen: true,
            model: Some("claude-opus-4-7".into()),
            cumulative_cost_usd: Some(0.42),
            last_turn_duration_ms: Some(12_000),
            ..Default::default()
        };
        let (p, a) = no_auth();
        // 8 cols: only YOLO (4) fits — even the model gets truncated.
        let spans = build_status_spans(&md, &p, &a, 0, 10, 0, 8, true);
        assert_eq!(spans[0].text, "YOLO");
    }

    #[test]
    fn markdown_bold_emits_bold_span() {
        let lines = markdown::render("hello **world**", 80);
        assert_eq!(lines.len(), 1);
        let bold = lines[0].iter().find(|s| s.hl == HL_MD_BOLD);
        assert!(bold.is_some(), "bold span missing: {:?}", lines[0]);
        assert_eq!(bold.unwrap().text, "world");
    }

    #[test]
    fn markdown_italic_emits_italic_span() {
        let lines = markdown::render("hello *world*", 80);
        let italic = lines[0].iter().find(|s| s.hl == HL_MD_ITALIC);
        assert!(italic.is_some(), "italic span missing: {:?}", lines[0]);
        assert_eq!(italic.unwrap().text, "world");
    }

    #[test]
    fn markdown_inline_code_emits_code_inline_span() {
        let lines = markdown::render("call `foo()` now", 80);
        let code = lines[0].iter().find(|s| s.hl == HL_MD_CODE_INLINE);
        assert!(code.is_some(), "inline code span missing: {:?}", lines[0]);
        assert_eq!(code.unwrap().text, "foo()");
    }

    #[test]
    fn markdown_code_block_renders_every_line_with_code_block_hl() {
        let src = "```\nfoo\nbar\n```";
        let lines = markdown::render(src, 80);
        assert!(lines.len() >= 2);
        let line_a = lines.iter().find(|l| l.iter().any(|s| s.text.contains("foo")));
        let line_b = lines.iter().find(|l| l.iter().any(|s| s.text.contains("bar")));
        assert!(line_a.is_some() && line_b.is_some(), "code lines missing");
        // Every span on a code-block row carries HL_MD_CODE_BLOCK so the
        // bg color extends as a uniform rectangle (left/right insets +
        // trailing pad inherit the same hl).
        for line in [line_a.unwrap(), line_b.unwrap()] {
            for s in line {
                assert_eq!(s.hl, HL_MD_CODE_BLOCK, "span {s:?} not HL_MD_CODE_BLOCK");
            }
        }
    }

    #[test]
    fn markdown_code_block_pads_lines_to_uniform_width() {
        // Three lines of varying widths should produce three rows of equal
        // total display width — that's how the bg rectangle stays clean.
        let src = "```\na\nbcdefgh\nij\n```";
        let lines = markdown::render(src, 80);
        let code_rows: Vec<&Vec<Span>> = lines
            .iter()
            .filter(|l| l.iter().any(|s| s.hl == HL_MD_CODE_BLOCK))
            .collect();
        assert_eq!(code_rows.len(), 3, "expected 3 code rows, got {}", code_rows.len());
        let widths: Vec<usize> = code_rows
            .iter()
            .map(|row| row.iter().map(|s| str_width(&s.text)).sum())
            .collect();
        let first = widths[0];
        for w in &widths {
            assert_eq!(*w, first, "code rows must be equal width: {widths:?}");
        }
    }

    #[test]
    fn markdown_code_block_has_left_inset_space() {
        // First cell of every code-block row should be a single-space
        // left inset so the code never visually touches the bg edge.
        let src = "```\nhello\n```";
        let lines = markdown::render(src, 80);
        let row = lines
            .iter()
            .find(|l| l.iter().any(|s| s.text.contains("hello")))
            .expect("code row missing");
        // First non-empty span is the left inset: " ", HL_MD_CODE_BLOCK.
        let first = row.iter().find(|s| !s.text.is_empty()).expect("non-empty span");
        assert_eq!(first.text, " ", "first span should be the 1-col left inset");
        assert_eq!(first.hl, HL_MD_CODE_BLOCK);
    }

    #[test]
    fn markdown_code_block_long_line_wraps_at_inner_width() {
        // A line longer than the column budget hard-wraps at `cols - 2`
        // (one col left inset + one col right inset). With cols=10 the
        // inner budget is 8; a 20-char single-word line yields 3 chunks
        // of widths 8, 8, 4 (wrapped through `split_by_columns`).
        let src = "```\nabcdefghijklmnopqrst\n```";
        let lines = markdown::render(src, 10);
        let code_rows: Vec<&Vec<Span>> = lines
            .iter()
            .filter(|l| l.iter().any(|s| s.hl == HL_MD_CODE_BLOCK))
            .collect();
        assert!(code_rows.len() >= 3, "expected >=3 wrapped rows, got {}", code_rows.len());
        // No code chunk on any row should exceed cols - 2 columns.
        for row in &code_rows {
            // The "code chunk" is the middle span — not the first (inset)
            // or last (inset) — find any span whose text isn't all-spaces.
            let widest_code: usize = row
                .iter()
                .filter(|s| !s.text.chars().all(|c| c == ' '))
                .map(|s| str_width(&s.text))
                .max()
                .unwrap_or(0);
            assert!(
                widest_code <= 8,
                "code chunk width {widest_code} > inner budget 8: {row:?}"
            );
        }
        // And every row total should be cols-wide (uniform rectangle).
        for row in &code_rows {
            let total: usize = row.iter().map(|s| str_width(&s.text)).sum();
            assert_eq!(total, 10, "row total width should be cols=10: {row:?}");
        }
    }

    #[test]
    fn markdown_heading_emits_heading_hl() {
        let lines = markdown::render("# Title\n\nbody", 80);
        let heading_line = lines
            .iter()
            .find(|l| l.iter().any(|s| s.text == "Title"))
            .expect("heading line");
        let title = heading_line.iter().find(|s| s.text == "Title").unwrap();
        assert_eq!(title.hl, HL_MD_HEADING);
    }

    #[test]
    fn markdown_unordered_list_emits_marker() {
        let lines = markdown::render("- one\n- two", 80);
        let marker_line = lines
            .iter()
            .find(|l| l.iter().any(|s| s.hl == HL_MD_LIST_MARKER))
            .expect("marker found");
        assert!(marker_line[0].text.contains('•'));
    }

    #[test]
    fn tool_collapsed_for_write_file_shows_path_not_content() {
        // write_file's input has both `path` and `content`. Without the
        // explicit name match, the fallback could pick `content` (which
        // is a multi-line file body) and dominate the one-line summary.
        let big_content = "line\n".repeat(200);
        let payload = crate::state::ToolPayload {
            id: "g-1".into(),
            name: "write_file".into(),
            input_json: serde_json::json!({
                "path": "/tmp/notes.md",
                "content": big_content,
            })
            .to_string(),
            output: Some("wrote 1000 bytes to /tmp/notes.md".into()),
            error: false,
        };
        let line = tool_collapsed_line(&payload, 80);
        let joined: String = line.iter().map(|s| s.text.as_str()).collect();
        assert!(joined.contains("/tmp/notes.md"), "path missing: {joined:?}");
        assert!(!joined.contains("line\nline"), "content leaked: {joined:?}");
    }

    #[test]
    fn tool_collapsed_for_snake_bash_shows_command() {
        let payload = crate::state::ToolPayload {
            id: "g-2".into(),
            name: "bash".into(),
            input_json: serde_json::json!({"command": "cargo test -p tool-gate"}).to_string(),
            output: Some("ok\n[exit 0]".into()),
            error: false,
        };
        let line = tool_collapsed_line(&payload, 80);
        let joined: String = line.iter().map(|s| s.text.as_str()).collect();
        assert!(joined.contains("cargo test"), "command missing: {joined:?}");
    }

    #[test]
    fn tool_collapsed_for_read_file_shows_path() {
        let payload = crate::state::ToolPayload {
            id: "g-3".into(),
            name: "read_file".into(),
            input_json: serde_json::json!({"path": "/etc/hosts"}).to_string(),
            output: Some("...".into()),
            error: false,
        };
        let line = tool_collapsed_line(&payload, 80);
        let joined: String = line.iter().map(|s| s.text.as_str()).collect();
        assert!(joined.contains("/etc/hosts"), "path missing: {joined:?}");
    }

    #[test]
    fn tool_collapsed_renders_single_summary_line() {
        // Bash + command goes through the one-liner with truncation.
        let payload = crate::state::ToolPayload {
            id: "toolu_1".into(),
            name: "Bash".into(),
            input_json: serde_json::json!({"command": "cd ~ && cargo test"}).to_string(),
            output: Some("ok".into()),
            error: false,
        };
        let line = tool_collapsed_line(&payload, 80);
        let joined: String = line.iter().map(|s| s.text.as_str()).collect();
        assert!(joined.starts_with("▸ Bash("), "got {joined:?}");
        assert!(joined.contains("cd ~ && cargo test"), "got {joined:?}");
    }

    #[test]
    fn tool_collapsed_for_running_shows_dots() {
        let payload = crate::state::ToolPayload {
            id: "toolu_1".into(),
            name: "Read".into(),
            input_json: serde_json::json!({"file_path": "/x"}).to_string(),
            output: None,
            error: false,
        };
        let line = tool_collapsed_line(&payload, 80);
        let joined: String = line.iter().map(|s| s.text.as_str()).collect();
        assert!(joined.contains("Read"), "got {joined:?}");
        assert!(joined.contains("…"), "running marker: {joined:?}");
    }

    #[test]
    fn tool_expanded_renders_salient_in_header_and_output_body() {
        let payload = crate::state::ToolPayload {
            id: "toolu_1".into(),
            name: "Bash".into(),
            input_json: serde_json::json!({"command": "ls"}).to_string(),
            output: Some("file1\nfile2".into()),
            error: false,
        };
        let lines = tool_expanded_lines(&payload, 80);
        let joined: String = lines
            .iter()
            .flat_map(|l| l.iter().map(|s| s.text.as_str()))
            .collect();
        // Salient command rides on the header — no separate `input:` block.
        assert!(joined.contains("▼ Bash(ls)"), "header missing: {joined:?}");
        assert!(!joined.contains("input:"), "input block must be dropped");
        assert!(!joined.contains("\"command\""), "raw json must not appear");
        assert!(joined.contains("output:"), "output label missing");
        assert!(joined.contains("file1"), "output body missing");
        assert!(joined.contains("file2"), "output body missing");
    }

    #[test]
    fn tool_expanded_header_uses_snake_case_salient() {
        let payload = crate::state::ToolPayload {
            id: "g-x".into(),
            name: "bash".into(),
            input_json: serde_json::json!({"command": "ls"}).to_string(),
            output: Some("Cargo.toml\n[exit 0]".into()),
            error: false,
        };
        let lines = tool_expanded_lines(&payload, 80);
        let joined: String = lines
            .iter()
            .flat_map(|l| l.iter().map(|s| s.text.as_str()))
            .collect();
        assert!(joined.contains("▼ bash(ls)"), "header missing: {joined:?}");
    }

    #[test]
    fn tool_expanded_running_shows_running_marker() {
        let payload = crate::state::ToolPayload {
            id: "toolu_1".into(),
            name: "Bash".into(),
            input_json: serde_json::json!({"command": "ls"}).to_string(),
            output: None,
            error: false,
        };
        let lines = tool_expanded_lines(&payload, 80);
        let joined: String = lines
            .iter()
            .flat_map(|l| l.iter().map(|s| s.text.as_str()))
            .collect();
        assert!(joined.contains("running"), "running marker: {joined:?}");
    }

    #[test]
    fn tool_expanded_caps_long_output() {
        let big: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let payload = crate::state::ToolPayload {
            id: "toolu_1".into(),
            name: "Bash".into(),
            input_json: serde_json::json!({"command": "noisy"}).to_string(),
            output: Some(big),
            error: false,
        };
        let lines = tool_expanded_lines(&payload, 80);
        let joined: String = lines
            .iter()
            .flat_map(|l| l.iter().map(|s| s.text.as_str()))
            .collect();
        assert!(joined.contains("more lines"), "expected truncation hint: {joined:?}");
    }

    #[test]
    fn humanize_tokens_basics() {
        assert_eq!(humanize_tokens(500), "500");
        assert_eq!(humanize_tokens(1_000), "1k");
        assert_eq!(humanize_tokens(47_000), "47k");
        assert_eq!(humanize_tokens(1_500_000), "1.5M");
    }

    #[test]
    fn humanize_duration_basics() {
        assert_eq!(humanize_duration_ms(500), "500ms");
        assert_eq!(humanize_duration_ms(12_000), "12s");
        assert_eq!(humanize_duration_ms(75_000), "1m15s");
    }

    #[test]
    fn finalized_assistant_with_metadata_renders_footer() {
        let mut s = ChatState::new();
        s.dims = Dims { cols: 60, rows: 8 };
        s.tui_ready = true;
        s.append_assistant_delta("hello world");
        s.finalize_assistant(None);
        s.stamp_last_assistant(Some("claude-sonnet-4-6".into()), Some(12_000));
        let lines = wrap_transcript(&s.transcript, 60, false);
        // `humanize_duration_ms(12_000)` is "12s" — same rounding the
        // statusline uses; floor at the second.
        let last: String = lines
            .last()
            .expect("at least one line")
            .iter()
            .map(|s| s.text.as_str())
            .collect();
        assert!(
            last.contains("▣ sonnet-4-6 · 12s"),
            "footer missing: {last:?}"
        );
        assert_eq!(lines.last().unwrap()[0].hl, HL_FOOTER);
    }

    #[test]
    fn streaming_assistant_renders_no_footer_even_with_metadata() {
        let mut s = ChatState::new();
        s.dims = Dims { cols: 60, rows: 8 };
        s.append_assistant_delta("partial");
        // Manually stamp metadata while still streaming — the footer
        // must not render because the body isn't final.
        if let Some(last) = s.transcript.last_mut() {
            last.model = Some("claude-sonnet-4-6".into());
            last.duration_ms = Some(1500);
        }
        let lines = wrap_transcript(&s.transcript, 60, false);
        let joined: String = lines
            .iter()
            .flat_map(|l| l.iter().map(|s| s.text.as_str()))
            .collect();
        assert!(!joined.contains('▣'), "no footer while streaming: {joined:?}");
    }

    #[test]
    fn assistant_without_metadata_renders_no_footer() {
        let mut s = ChatState::new();
        s.dims = Dims { cols: 60, rows: 8 };
        s.push_entry(Role::Assistant, "plain reply".into());
        let lines = wrap_transcript(&s.transcript, 60, false);
        let joined: String = lines
            .iter()
            .flat_map(|l| l.iter().map(|s| s.text.as_str()))
            .collect();
        assert!(!joined.contains('▣'), "no footer absent metadata: {joined:?}");
    }

    #[test]
    fn assistant_footer_renders_model_only_when_duration_missing() {
        // Replayed-history case: parser stamps model from the assistant
        // message envelope, but Claude's session.jsonl doesn't record per-
        // turn wall-clock duration. Footer must still render.
        let mut s = ChatState::new();
        s.dims = Dims { cols: 60, rows: 8 };
        s.push_entry(Role::Assistant, "replayed reply".into());
        s.stamp_last_assistant(Some("claude-sonnet-4-6".into()), None);
        let lines = wrap_transcript(&s.transcript, 60, false);
        let last: String = lines
            .last()
            .expect("at least one line")
            .iter()
            .map(|s| s.text.as_str())
            .collect();
        assert!(
            last.contains("▣ sonnet-4-6"),
            "model-only footer missing: {last:?}"
        );
        // No interpunct / duration when duration is None.
        assert!(!last.contains('·'), "duration should not appear: {last:?}");
    }

    #[test]
    fn multiline_input_renders_multiple_visual_rows() {
        // Buffer with literal '\n's must split into separate visual rows in
        // the input block — newlines previously got collapsed to spaces.
        let (lines, cursor_line, _col) = render_input_wrapped("a\nbc\nd", 6, 40, 0);
        // 3 segments → 3 rows.
        assert_eq!(lines.len(), 3);
        // Cursor at end (offset 6) sits on the third row (index 2).
        assert_eq!(cursor_line, 2);
    }

    #[test]
    fn markdown_with_horizontal_rule_emits_rule_line() {
        let lines = markdown::render("text\n\n---\n\nmore", 20);
        let has_rule = lines.iter().any(|l| {
            let joined: String = l.iter().map(|s| s.text.as_str()).collect();
            !joined.is_empty() && joined.chars().all(|c| c == '─')
        });
        assert!(has_rule, "expected ─-only line in: {lines:?}");
    }

    #[test]
    fn markdown_with_table_emits_box_drawing() {
        let src = "\
| col1 | col2 |
|------|------|
| a    | b    |
| c    | d    |";
        let lines = markdown::render(src, 80);
        let joined: String = lines
            .iter()
            .flat_map(|l| l.iter().map(|s| s.text.as_str()))
            .collect();
        assert!(joined.contains('┌'), "missing top-left corner: {joined:?}");
        assert!(joined.contains('│'), "missing vertical: {joined:?}");
        assert!(joined.contains('┘'), "missing bottom-right: {joined:?}");
        for needle in ["col1", "col2", "a", "b", "c", "d"] {
            assert!(joined.contains(needle), "missing {needle:?} in {joined:?}");
        }
    }

    #[test]
    fn table_with_cell_too_wide_wraps_within_column() {
        let wide: String = "x".repeat(100);
        let src = format!("| h |\n|---|\n| {wide} |");
        let lines = markdown::render(&src, 40);
        for line in &lines {
            let w: usize = line.iter().map(|s| str_width(&s.text)).sum();
            assert!(w <= 40, "row exceeds width: w={w} line={line:?}");
        }
        let joined: String = lines
            .iter()
            .flat_map(|l| l.iter().map(|s| s.text.as_str()))
            .collect();
        // Wrapping replaces truncation: no ellipsis should appear.
        assert!(!joined.contains('…'), "unexpected ellipsis in: {joined:?}");
        // Column width caps at TABLE_COL_MAX (30); the body cell wraps
        // across multiple visual lines. Count rows that contain only x's
        // (with the surrounding box-drawing border) — there should be
        // multiple such lines for a 100-char cell against a 30-wide col.
        let body_lines = lines
            .iter()
            .filter(|l| {
                let t: String = l.iter().map(|s| s.text.as_str()).collect();
                t.contains('x')
            })
            .count();
        assert!(
            body_lines >= 4,
            "expected ≥4 wrapped body lines for 100-char cell at width 30, got {body_lines}: {joined:?}"
        );
        // The widest border line should be exactly 1 (left) + 30 + 2 (pad)
        //   + 1 (right) = 34 columns wide — proving width capped at 30.
        let has_30_wide_inner = lines.iter().any(|l| {
            let t: String = l.iter().map(|s| s.text.as_str()).collect();
            t.starts_with('┌') && t.chars().filter(|c| *c == '─').count() == 32
        });
        assert!(
            has_30_wide_inner,
            "expected a top border with 32 ─ chars (col=30 + 2 pad), got: {joined:?}"
        );
    }

    #[test]
    fn table_row_height_matches_tallest_cell() {
        // Tall cell + short cell in same row: 30-char column wraps the long
        // word into 3 sub-lines (10 × 3 = 30). Short cell renders on line 1
        // and the border |   | should appear for lines 2 and 3.
        // Use TABLE_COL_MAX=30 by giving the tall cell a 30-char string.
        let tall = "a".repeat(30) + " " + &"b".repeat(30) + " " + &"c".repeat(30);
        let src = format!("| left | right |\n|---|---|\n| {tall} | x |");
        let lines = markdown::render(&src, 80);

        // Find the top-border index, header, header separator, body rows.
        let line_strs: Vec<String> = lines
            .iter()
            .map(|l| l.iter().map(|s| s.text.as_str()).collect())
            .collect();
        // Locate the bottom border to bound the body slice.
        let bot_idx = line_strs
            .iter()
            .position(|t| t.starts_with('└'))
            .expect("bottom border missing");
        // Locate the mid-rule between header and first body row.
        let first_mid_idx = line_strs
            .iter()
            .position(|t| t.starts_with('├'))
            .expect("mid border missing");
        // Body rows live between first_mid_idx+1 and bot_idx (exclusive).
        let body: Vec<&String> = line_strs[first_mid_idx + 1..bot_idx].iter().collect();

        // Three visual lines: tall cell wraps to 3, short cell occupies row
        // 1 with two padded blank-content rows after.
        assert_eq!(
            body.len(),
            3,
            "expected 3 body sub-lines for tallest-cell wrap; got {}: {body:?}",
            body.len()
        );
        // Line 1 contains the short cell text "x".
        assert!(body[0].contains(" x "), "first body sub-line missing 'x': {:?}", body[0]);
        // Lines 2 and 3 keep the right column blank (only spaces between │).
        for line in &body[1..] {
            // Strip the leading │, leading space, and look for content.
            // Right cell substring should be all spaces.
            let parts: Vec<&str> = line.split('│').collect();
            assert!(
                parts.len() >= 3,
                "expected ≥2 vertical bars splitting into ≥3 segments: {line:?}"
            );
            let right = parts[2];
            assert!(
                right.chars().all(|c| c == ' '),
                "right cell on padded sub-line should be blank: right={right:?} line={line:?}"
            );
        }
    }

    #[test]
    fn markdown_with_strikethrough_renders() {
        // ENABLE_STRIKETHROUGH is part of Options::all() — confirm the
        // text content survives the parse (we render it as plain text).
        let lines = markdown::render("hello ~~world~~", 80);
        let joined: String = lines
            .iter()
            .flat_map(|l| l.iter().map(|s| s.text.as_str()))
            .collect();
        assert!(joined.contains("world"), "strikethrough text missing: {joined:?}");
    }

}
