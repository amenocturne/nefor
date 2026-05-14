//! `tui.text_input` per-instance state + editing-key handlers.
//!
//! Browser-style controlled-component model: Lua holds the value source
//! of truth; the engine maintains the cursor, selection, scroll offset,
//! IME composition, and undo stack across re-renders. The reconciler
//! preserves this state by matching `(type_tag, key)`; here we expose the
//! editing operations the input router invokes when a focused text_input
//! receives an editing key.
//!
//! All offsets are byte offsets into the UTF-8 value. Helpers below clamp
//! to the nearest character boundary so external callers never have to
//! reason about it.
//!
//! ## Soft-wrap (multi-line inputs)
//!
//! When `max_lines > 1` the layout pass word-wraps the value to the
//! viewport width and counts each soft-wrapped row toward the visible
//! line count. Cursor navigation (Up/Down arrows, Home/End) operates on
//! visual rows, not logical rows. Each wrapped row carries the byte
//! range `[start, end)` it covers in the original value so the cursor
//! can translate between byte offset and `(visual_row, visual_col)`.
//!
//! Single-line inputs (`max_lines == 1`) keep horizontal-scroll
//! behaviour: the value never wraps, but `scroll_x` is bumped so the
//! cursor stays inside the viewport.

use std::collections::VecDeque;

use unicode_width::UnicodeWidthChar;

/// Maximum entries kept in the undo / redo history. The cap exists so a
/// runaway typing session doesn't grow unbounded; 128 covers normal
/// human-pace editing comfortably.
const HISTORY_CAP: usize = 128;

/// Per-instance editing state for a `text_input`. Lives inside
/// `InstanceState::TextInput(_)` so the reconciler preserves it across
/// rebuilds keyed on `(type_tag, key)`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextInputState {
    /// The most recently observed `value` from the description tree.
    /// Drives `sync_with_desc`: when the value changes externally (Lua
    /// rewrote it), local cursor/selection/scroll get clamped instead of
    /// preserved blind.
    pub last_value: String,
    /// Cursor position in bytes within `last_value`.
    pub cursor: usize,
    /// Anchored end of the selection. `None` = no selection. Selection
    /// spans `[min(anchor, cursor), max(anchor, cursor))`.
    pub selection_anchor: Option<usize>,
    /// Horizontal scroll in cells. Increments when the cursor moves
    /// past the rightmost visible column. Used by single-line inputs;
    /// multi-line inputs soft-wrap instead and never advance this.
    pub scroll_x: u16,
    /// Vertical scroll in lines. Increments past the bottom visible row.
    pub scroll_y: u16,
    /// Most-recent viewport width observed during layout (in cells).
    /// Cached so editing-key handlers (Up/Down, Home/End on a wrapped
    /// row, scroll bookkeeping) can reason about the visual layout
    /// without re-deriving it each call. Zero until the first paint —
    /// fall back to whole-line semantics until the layout pass runs.
    pub viewport_width: u16,
    /// Active IME composition, if any. While composing, the engine
    /// inserts a placeholder run at the cursor; `commit_ime` splices the
    /// committed text in and clears this slot.
    pub composing: Option<ImeComposition>,
    /// Undo stack — most recent edit at the back.
    pub undo: VecDeque<Snapshot>,
    /// Redo stack — most recent undone edit at the back.
    pub redo: VecDeque<Snapshot>,
    /// Whether the focused-prop changed since the last `sync_with_desc`.
    /// Used by the input router to gate engine-internal cursor updates.
    pub focused: bool,
    /// User is mouse-wheeling through the buffer; suspends the
    /// cursor-pin in [`crate::layout::sync_multi_line_scroll_y`] so the
    /// wheel can peek at parts of the buffer outside the cursor's row.
    /// Cleared by any cursor-moving key, any content mutation, and the
    /// natural value-shrink path (`sync_with_desc` rewriting `last_value`
    /// — submit clears the buffer, so scroll_y collapses to 0 and the
    /// flag must clear too). Mirrors `ScrollableState::was_at_end` in
    /// purpose: a one-bit user-intent latch that the auto-pin checks
    /// before stealing the viewport back.
    pub manual_scroll: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImeComposition {
    pub text: String,
    /// Offset (byte) where the composition is anchored in `last_value`.
    pub anchor: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub value: String,
    pub cursor: usize,
    pub selection_anchor: Option<usize>,
}

/// Result of an editing operation. The engine relays `new_value` back
/// to Lua via the configured `on_change` msg kind.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EditOutcome {
    /// New value, if it changed. `None` = pure cursor / selection /
    /// scroll move, no `on_change` to fire.
    pub new_value: Option<String>,
    /// `true` if Enter was pressed (no Shift). The input router fires
    /// `on_submit` and does NOT modify the value (per spec).
    pub submitted: bool,
}

impl TextInputState {
    /// Synchronise the per-instance state with the current description
    /// `value` and `focused` prop. Called once per render before any
    /// painting. Behaviour:
    ///
    /// - First sync ever: stash `value` as baseline; cursor stays at 0.
    /// - Value unchanged: keep cursor / selection / scroll verbatim.
    /// - Value changed externally (Lua rewrote it): adopt the new value
    ///   and move the cursor to the end. This matches browser semantics
    ///   for `<input>.value = ...` — when application code replaces the
    ///   value, the user expects the caret at the end of the new text
    ///   (e.g. autocomplete: typing `/mo` then completing to `/model`
    ///   should leave the cursor at offset 6, not stranded at 3). The
    ///   selection clears since the prior anchor no longer maps onto the
    ///   new content; scroll is similarly clamped on next paint.
    pub fn sync_with_desc(&mut self, value: &str, focused: bool) {
        self.focused = focused;
        if self.last_value != value {
            self.last_value = value.to_string();
            // External mutation → cursor jumps to end; drop selection.
            self.cursor = value.len();
            self.selection_anchor = None;
            // External rewrite invalidates the manual-scroll latch — the
            // user's old viewport offset doesn't map onto the new
            // content, so the auto-pin should re-engage. Submit clears
            // the buffer this way; autocomplete rewrites also flow here.
            self.manual_scroll = false;
            // Scroll y can outlive a value rewrite (e.g. only one line
            // changed in a multi-line); keep it but clamp to line count.
            let lines = value.split('\n').count() as u16;
            if self.scroll_y >= lines {
                self.scroll_y = lines.saturating_sub(1);
            }
            // x is similarly clamped on next paint; no information here
            // to clamp tighter without knowing the visible width.
        }
    }

    /// Produce a snapshot of the current value+cursor for the undo
    /// stack. Caller pushes via [`Self::push_undo`] before mutating.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            value: self.last_value.clone(),
            cursor: self.cursor,
            selection_anchor: self.selection_anchor,
        }
    }

    /// Push the current state onto the undo stack and clear redo (any
    /// new edit invalidates the redo trail). Capped at [`HISTORY_CAP`].
    pub fn push_undo(&mut self) {
        if self.undo.len() == HISTORY_CAP {
            self.undo.pop_front();
        }
        self.undo.push_back(self.snapshot());
        self.redo.clear();
    }

    /// Apply a snapshot in place, replacing value+cursor+selection.
    pub fn restore(&mut self, snap: Snapshot) {
        self.last_value = snap.value;
        self.cursor = snap.cursor;
        self.selection_anchor = snap.selection_anchor;
    }

    /// Selection range as `[start, end)`. `None` if no selection.
    pub fn selection_range(&self) -> Option<(usize, usize)> {
        self.selection_anchor.map(|anchor| {
            let lo = anchor.min(self.cursor);
            let hi = anchor.max(self.cursor);
            (lo, hi)
        })
    }

    /// Clear the selection (cursor stays put).
    pub fn clear_selection(&mut self) {
        self.selection_anchor = None;
    }
}

/// Clamp a byte offset to the nearest valid char boundary, capped at
/// `s.len()`.
pub fn clamp_to_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    let mut i = idx;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Step left by one char from `idx`. Returns `idx` itself when already
/// at zero.
pub fn prev_char_boundary(s: &str, idx: usize) -> usize {
    if idx == 0 {
        return 0;
    }
    let mut i = idx.saturating_sub(1);
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Step right by one char from `idx`. Returns `s.len()` when already at
/// the end.
pub fn next_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    let mut i = idx + 1;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Find the start of the line containing `idx` (offset of the byte right
/// after the previous `\n`, or `0`).
pub fn line_start(s: &str, idx: usize) -> usize {
    let bytes = s.as_bytes();
    let mut i = idx.min(s.len());
    while i > 0 && bytes[i - 1] != b'\n' {
        i -= 1;
    }
    i
}

/// Find the end of the line containing `idx` (offset of the next `\n`,
/// or `s.len()`).
pub fn line_end(s: &str, idx: usize) -> usize {
    let bytes = s.as_bytes();
    let mut i = idx.min(s.len());
    while i < s.len() && bytes[i] != b'\n' {
        i += 1;
    }
    i
}

/// Whether `c` belongs to a "word" run for cursor / delete word
/// operations. Mirrors readline's default class: alphanumerics +
/// underscore. Anything else (whitespace, punctuation, symbols) is a
/// boundary.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Find the offset of the next word boundary to the LEFT of `idx`.
/// Skips trailing whitespace/punctuation, then walks across one word
/// run. Returns `0` when the cursor is at the start. Used by Ctrl+W /
/// Alt+Backspace / Alt+Left.
pub fn word_boundary_left(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    // Step 1: skip non-word chars on the left side of the cursor.
    while i > 0 {
        let prev = prev_char_boundary(s, i);
        let c = s[prev..i].chars().next().unwrap_or(' ');
        if is_word_char(c) {
            break;
        }
        i = prev;
    }
    // Step 2: walk back across the contiguous word run.
    while i > 0 {
        let prev = prev_char_boundary(s, i);
        let c = s[prev..i].chars().next().unwrap_or(' ');
        if !is_word_char(c) {
            break;
        }
        i = prev;
    }
    i
}

/// Find the offset of the next word boundary to the RIGHT of `idx`.
/// Skips leading non-word chars, then walks across one word run.
pub fn word_boundary_right(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    // Step 1: skip non-word chars at the cursor.
    while i < s.len() {
        let c = s[i..].chars().next().unwrap_or(' ');
        if is_word_char(c) {
            break;
        }
        i = next_char_boundary(s, i);
    }
    // Step 2: walk forward across the word run.
    while i < s.len() {
        let c = s[i..].chars().next().unwrap_or(' ');
        if !is_word_char(c) {
            break;
        }
        i = next_char_boundary(s, i);
    }
    i
}

/// Whether the input should accept Shift+Enter newline insertion. Single-
/// line inputs (`max_lines = 1`) ignore Shift+Enter and bubble it.
pub fn allows_newline_insert(max_lines: u16) -> bool {
    max_lines > 1
}

/// Display width of a single char, in cells (zero for combiners).
fn char_cell_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

/// One soft-wrapped row inside [`wrap_value`]'s output. Carries enough
/// context for cursor mapping in either direction:
///
/// - `start_byte` / `end_byte` — half-open byte range into the original
///   value covered by this row.
/// - `width` — visible cell width of the row (post-wrap).
/// - `terminated_by_newline` — `true` when this row ends at a hard `\n`
///   in the source value. Used so the cursor can sit on the empty row
///   after a trailing newline without "jumping" onto the previous row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrappedRow {
    pub start_byte: usize,
    pub end_byte: usize,
    pub width: usize,
    pub terminated_by_newline: bool,
}

/// Word-wrap `value` to the given viewport width, returning one
/// [`WrappedRow`] per visible row. Hard newlines always start a new row;
/// long logical lines are word-wrapped (with `wrap_char` fallback for
/// individual words longer than the viewport, mirroring [`crate::layout::wrap_text`]).
///
/// Width is measured in unicode display columns. A `width = 0` viewport
/// returns one empty row covering the whole value (the caller's clamp
/// handles the visual no-op).
pub fn wrap_value(value: &str, width: u16) -> Vec<WrappedRow> {
    if width == 0 {
        return vec![WrappedRow {
            start_byte: 0,
            end_byte: value.len(),
            width: 0,
            terminated_by_newline: false,
        }];
    }
    let limit = width as usize;
    let mut out: Vec<WrappedRow> = Vec::new();
    let bytes = value.as_bytes();
    let mut logical_start = 0usize;
    let mut i = 0usize;
    // Walk the value one logical line at a time (split by `\n`); after
    // each split, soft-wrap the contained slice.
    while i <= bytes.len() {
        let at_eof = i == bytes.len();
        let at_newline = !at_eof && bytes[i] == b'\n';
        if at_eof || at_newline {
            let line = &value[logical_start..i];
            wrap_logical_line(line, logical_start, limit, at_newline, &mut out);
            if at_newline {
                logical_start = i + 1;
                i += 1;
                if logical_start == bytes.len() {
                    // Trailing newline: emit an empty row anchored at the
                    // very end so the cursor can sit there.
                    out.push(WrappedRow {
                        start_byte: bytes.len(),
                        end_byte: bytes.len(),
                        width: 0,
                        terminated_by_newline: false,
                    });
                    break;
                }
            } else {
                break;
            }
        } else {
            // Skip multi-byte char lead bytes by stepping char-by-char
            // when we would land mid-char. The slice operations above
            // handle correctness; we just need to advance.
            let ch_len = char_byte_len(value, i);
            i += ch_len.max(1);
        }
    }
    if out.is_empty() {
        out.push(WrappedRow {
            start_byte: 0,
            end_byte: 0,
            width: 0,
            terminated_by_newline: false,
        });
    }
    out
}

fn char_byte_len(s: &str, idx: usize) -> usize {
    s[idx..].chars().next().map(|c| c.len_utf8()).unwrap_or(1)
}

/// Word-wrap a single `\n`-free slice to `limit` cells. Each emitted
/// row's byte offsets are anchored at `slice_origin` (the slice's start
/// in the parent value).
fn wrap_logical_line(
    line: &str,
    slice_origin: usize,
    limit: usize,
    terminator_is_newline: bool,
    out: &mut Vec<WrappedRow>,
) {
    if line.is_empty() {
        out.push(WrappedRow {
            start_byte: slice_origin,
            end_byte: slice_origin,
            width: 0,
            terminated_by_newline: terminator_is_newline,
        });
        return;
    }
    let runs = split_keeping_spaces(line);
    let mut row_start_in_line = 0usize;
    let mut cursor = 0usize; // byte offset within `line`
    let mut col = 0usize;
    for run in runs {
        let run_w: usize = run.chars().map(char_cell_width).sum();
        let run_bytes = run.len();
        if col == 0 && run_w > limit {
            // Word longer than the viewport — emit any in-flight row,
            // then char-wrap this run.
            char_wrap_run(run, slice_origin + cursor, limit, out);
            cursor += run_bytes;
            row_start_in_line = cursor;
            col = 0;
            continue;
        }
        if col + run_w > limit {
            // Close the current row at `cursor` and start a new one.
            out.push(WrappedRow {
                start_byte: slice_origin + row_start_in_line,
                end_byte: slice_origin + cursor,
                width: col,
                terminated_by_newline: false,
            });
            // If this run is pure whitespace, swallow it at the line
            // start (mirrors [`crate::layout::wrap_word`]).
            if run.chars().all(char::is_whitespace) {
                cursor += run_bytes;
                row_start_in_line = cursor;
                col = 0;
                continue;
            }
            row_start_in_line = cursor;
            col = 0;
        }
        cursor += run_bytes;
        col += run_w;
    }
    // Trailing partial row.
    out.push(WrappedRow {
        start_byte: slice_origin + row_start_in_line,
        end_byte: slice_origin + cursor,
        width: col,
        terminated_by_newline: terminator_is_newline,
    });
}

/// Char-wrap a single run that's wider than `limit`. Emits as many rows
/// as needed; each row's `terminated_by_newline` is `false` (the parent
/// caller decides what comes after).
fn char_wrap_run(run: &str, slice_origin: usize, limit: usize, out: &mut Vec<WrappedRow>) {
    let mut start = 0usize;
    let mut byte = 0usize;
    let mut col = 0usize;
    for ch in run.chars() {
        let w = char_cell_width(ch);
        if w > limit {
            // Single grapheme wider than the line — emit prior, then this on its own.
            if byte > start {
                out.push(WrappedRow {
                    start_byte: slice_origin + start,
                    end_byte: slice_origin + byte,
                    width: col,
                    terminated_by_newline: false,
                });
            }
            let next = byte + ch.len_utf8();
            out.push(WrappedRow {
                start_byte: slice_origin + byte,
                end_byte: slice_origin + next,
                width: w,
                terminated_by_newline: false,
            });
            byte = next;
            start = next;
            col = 0;
            continue;
        }
        if col + w > limit {
            out.push(WrappedRow {
                start_byte: slice_origin + start,
                end_byte: slice_origin + byte,
                width: col,
                terminated_by_newline: false,
            });
            start = byte;
            col = 0;
        }
        byte += ch.len_utf8();
        col += w;
    }
    if byte > start {
        out.push(WrappedRow {
            start_byte: slice_origin + start,
            end_byte: slice_origin + byte,
            width: col,
            terminated_by_newline: false,
        });
    }
}

/// Split `s` into runs that alternate whitespace and non-whitespace.
/// Mirrors [`crate::layout::split_keeping_spaces`] but lives here so this
/// module is self-contained.
fn split_keeping_spaces(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    if s.is_empty() {
        return out;
    }
    let mut start = 0usize;
    let mut in_space = s.starts_with(char::is_whitespace);
    let mut i = 0usize;
    while i < s.len() {
        let c = s[i..].chars().next().unwrap_or(' ');
        let cw = c.is_whitespace();
        if cw != in_space {
            out.push(&s[start..i]);
            start = i;
            in_space = cw;
        }
        i += c.len_utf8();
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// Soft-wrapped visible line count for a multi-line input — number of
/// rows produced by [`wrap_value`] at the given viewport width.
pub fn soft_wrapped_line_count(value: &str, width: u16) -> usize {
    wrap_value(value, width).len()
}

/// Map a byte offset to `(visual_row, visual_col)` within the soft-
/// wrapped layout. The cursor maps to the *first* row whose byte range
/// covers it — when the cursor sits exactly on a wrap boundary (i.e.
/// `cursor == row.end_byte` for some non-final row that didn't end on a
/// hard `\n`), it visually belongs at the next row's start so typing
/// flows on.
pub fn cursor_in_wrap_for(value: &str, rows: &[WrappedRow], cursor: usize) -> (usize, usize) {
    if rows.is_empty() {
        return (0, 0);
    }
    for (i, row) in rows.iter().enumerate() {
        let in_row = cursor >= row.start_byte && cursor <= row.end_byte;
        if !in_row {
            continue;
        }
        let next_starts_here = rows
            .get(i + 1)
            .is_some_and(|n| n.start_byte == cursor && cursor == row.end_byte);
        if next_starts_here && !row.terminated_by_newline {
            continue;
        }
        let local_end = cursor.min(value.len());
        let local_start = row.start_byte.min(local_end);
        let slice = &value[local_start..local_end];
        let col: usize = slice.chars().map(char_cell_width).sum();
        return (i, col);
    }
    let last = rows.last().expect("non-empty");
    (rows.len() - 1, last.width)
}

/// Inverse of [`cursor_in_wrap_for`] — translate `(visual_row,
/// target_col)` into a byte offset, clamped to the row's content. Used
/// by Up/Down arrow when the cursor needs to land on a different visual
/// row at approximately the same column.
pub fn byte_offset_for_visual(
    value: &str,
    rows: &[WrappedRow],
    visual_row: usize,
    target_col: usize,
) -> usize {
    let row = match rows.get(visual_row) {
        Some(r) => r,
        None => return rows.last().map(|r| r.end_byte).unwrap_or(value.len()),
    };
    if target_col == 0 {
        return row.start_byte;
    }
    let slice = &value[row.start_byte..row.end_byte];
    let mut col = 0usize;
    let mut last_byte = row.start_byte;
    for ch in slice.chars() {
        let w = char_cell_width(ch);
        if col + w > target_col {
            return last_byte;
        }
        col += w;
        last_byte += ch.len_utf8();
    }
    row.end_byte
}

// ── Editing operations ────────────────────────────────────────────────────
//
// All ops act on the latest controlled-component value held in
// `state.last_value`. Each op pushes an undo snapshot before mutating
// (when the mutation actually changes the value), and returns an
// [`EditOutcome`] so the input router can decide whether to fire
// `on_change` / `on_submit`.

impl TextInputState {
    /// Insert a single char at the cursor. Replaces the active selection
    /// if any.
    pub fn insert_char(&mut self, ch: char) -> EditOutcome {
        let mut buf = [0u8; 4];
        let s = ch.encode_utf8(&mut buf);
        self.insert_str(s)
    }

    /// Insert a UTF-8 string at the cursor (or in place of the active
    /// selection).
    pub fn insert_str(&mut self, s: &str) -> EditOutcome {
        if s.is_empty() {
            return EditOutcome::default();
        }
        self.push_undo();
        let mut value = self.last_value.clone();
        let (lo, hi) = match self.selection_range() {
            Some(r) => r,
            None => (self.cursor, self.cursor),
        };
        value.replace_range(lo..hi, s);
        self.cursor = lo + s.len();
        self.selection_anchor = None;
        self.last_value = value.clone();
        EditOutcome {
            new_value: Some(value),
            submitted: false,
        }
    }

    /// Backspace: delete selection if any, else the char before cursor.
    pub fn backspace(&mut self) -> EditOutcome {
        if self.selection_range().is_some() {
            return self.delete_selection();
        }
        if self.cursor == 0 {
            return EditOutcome::default();
        }
        self.push_undo();
        let prev = prev_char_boundary(&self.last_value, self.cursor);
        let mut value = self.last_value.clone();
        value.replace_range(prev..self.cursor, "");
        self.cursor = prev;
        self.last_value = value.clone();
        EditOutcome {
            new_value: Some(value),
            submitted: false,
        }
    }

    /// Delete: drop selection if any, else the char after cursor.
    pub fn delete_forward(&mut self) -> EditOutcome {
        if self.selection_range().is_some() {
            return self.delete_selection();
        }
        if self.cursor >= self.last_value.len() {
            return EditOutcome::default();
        }
        self.push_undo();
        let next = next_char_boundary(&self.last_value, self.cursor);
        let mut value = self.last_value.clone();
        value.replace_range(self.cursor..next, "");
        self.last_value = value.clone();
        EditOutcome {
            new_value: Some(value),
            submitted: false,
        }
    }

    /// Replace the selection with the empty string. No-op when there is
    /// no selection.
    pub fn delete_selection(&mut self) -> EditOutcome {
        let Some((lo, hi)) = self.selection_range() else {
            return EditOutcome::default();
        };
        if lo == hi {
            self.selection_anchor = None;
            return EditOutcome::default();
        }
        self.push_undo();
        let mut value = self.last_value.clone();
        value.replace_range(lo..hi, "");
        self.cursor = lo;
        self.selection_anchor = None;
        self.last_value = value.clone();
        EditOutcome {
            new_value: Some(value),
            submitted: false,
        }
    }

    /// Move cursor one char left. With `extend_selection`, anchors the
    /// selection on first call and grows it.
    pub fn move_left(&mut self, extend_selection: bool) -> EditOutcome {
        self.update_selection_anchor(extend_selection);
        self.cursor = prev_char_boundary(&self.last_value, self.cursor);
        if !extend_selection {
            self.selection_anchor = None;
        }
        EditOutcome::default()
    }

    pub fn move_right(&mut self, extend_selection: bool) -> EditOutcome {
        self.update_selection_anchor(extend_selection);
        self.cursor = next_char_boundary(&self.last_value, self.cursor);
        if !extend_selection {
            self.selection_anchor = None;
        }
        EditOutcome::default()
    }

    pub fn move_to_line_start(&mut self, extend_selection: bool) -> EditOutcome {
        self.update_selection_anchor(extend_selection);
        self.cursor = line_start(&self.last_value, self.cursor);
        if !extend_selection {
            self.selection_anchor = None;
        }
        EditOutcome::default()
    }

    pub fn move_to_line_end(&mut self, extend_selection: bool) -> EditOutcome {
        self.update_selection_anchor(extend_selection);
        self.cursor = line_end(&self.last_value, self.cursor);
        if !extend_selection {
            self.selection_anchor = None;
        }
        EditOutcome::default()
    }

    /// Move cursor to the previous visual row, preserving column when
    /// possible. No-op on the first row. Uses soft-wrap layout when the
    /// viewport width is known (multi-line input post-layout); else
    /// falls back to hard-newline rows.
    pub fn move_up(&mut self, extend_selection: bool) -> EditOutcome {
        self.update_selection_anchor(extend_selection);
        if self.viewport_width > 0 {
            let value = self.last_value.clone();
            let rows = wrap_value(&value, self.viewport_width);
            let (visual_row, col) = cursor_in_wrap_for(&value, &rows, self.cursor);
            if visual_row == 0 {
                return EditOutcome::default();
            }
            self.cursor = byte_offset_for_visual(&value, &rows, visual_row - 1, col);
        } else {
            // Hard-newline fallback (also handles single-line case
            // gracefully — cursor sits at start of value, no-op).
            let cur_line_start = line_start(&self.last_value, self.cursor);
            if cur_line_start == 0 {
                return EditOutcome::default();
            }
            let col = self.cursor - cur_line_start;
            let prev_line_end = cur_line_start - 1;
            let prev_line_start = line_start(&self.last_value, prev_line_end);
            let prev_line_len = prev_line_end - prev_line_start;
            self.cursor = prev_line_start + col.min(prev_line_len);
        }
        if !extend_selection {
            self.selection_anchor = None;
        }
        EditOutcome::default()
    }

    pub fn move_down(&mut self, extend_selection: bool) -> EditOutcome {
        self.update_selection_anchor(extend_selection);
        if self.viewport_width > 0 {
            let value = self.last_value.clone();
            let rows = wrap_value(&value, self.viewport_width);
            let (visual_row, col) = cursor_in_wrap_for(&value, &rows, self.cursor);
            if visual_row + 1 >= rows.len() {
                return EditOutcome::default();
            }
            self.cursor = byte_offset_for_visual(&value, &rows, visual_row + 1, col);
        } else {
            let cur_line_start = line_start(&self.last_value, self.cursor);
            let cur_line_end = line_end(&self.last_value, self.cursor);
            if cur_line_end == self.last_value.len() {
                return EditOutcome::default();
            }
            let col = self.cursor - cur_line_start;
            let next_line_start = cur_line_end + 1;
            let next_line_end = line_end(&self.last_value, next_line_start);
            let next_line_len = next_line_end - next_line_start;
            self.cursor = next_line_start + col.min(next_line_len);
        }
        if !extend_selection {
            self.selection_anchor = None;
        }
        EditOutcome::default()
    }

    pub fn select_all(&mut self) -> EditOutcome {
        if self.last_value.is_empty() {
            return EditOutcome::default();
        }
        self.selection_anchor = Some(0);
        self.cursor = self.last_value.len();
        EditOutcome::default()
    }

    /// Ctrl+U: delete from cursor to start of current logical line.
    /// No-op when cursor already at line start.
    pub fn delete_to_line_start(&mut self) -> EditOutcome {
        let start = line_start(&self.last_value, self.cursor);
        if start == self.cursor {
            return EditOutcome::default();
        }
        self.push_undo();
        let mut value = self.last_value.clone();
        value.replace_range(start..self.cursor, "");
        self.cursor = start;
        self.selection_anchor = None;
        self.last_value = value.clone();
        EditOutcome {
            new_value: Some(value),
            submitted: false,
        }
    }

    /// Ctrl+K: delete from cursor to end of current logical line. The
    /// trailing `\n` (if any) survives so the line break stays intact.
    /// No-op when cursor already at line end.
    pub fn delete_to_line_end(&mut self) -> EditOutcome {
        let end = line_end(&self.last_value, self.cursor);
        if end == self.cursor {
            return EditOutcome::default();
        }
        self.push_undo();
        let mut value = self.last_value.clone();
        value.replace_range(self.cursor..end, "");
        self.selection_anchor = None;
        self.last_value = value.clone();
        EditOutcome {
            new_value: Some(value),
            submitted: false,
        }
    }

    /// Ctrl+W / Alt+Backspace: delete word backward — skip whitespace
    /// then alphanumeric+underscore run before the cursor. Mirrors
    /// readline semantics.
    pub fn delete_word_backward(&mut self) -> EditOutcome {
        let target = word_boundary_left(&self.last_value, self.cursor);
        if target == self.cursor {
            return EditOutcome::default();
        }
        self.push_undo();
        let mut value = self.last_value.clone();
        value.replace_range(target..self.cursor, "");
        self.cursor = target;
        self.selection_anchor = None;
        self.last_value = value.clone();
        EditOutcome {
            new_value: Some(value),
            submitted: false,
        }
    }

    /// Alt+Delete: delete word forward — skip whitespace then word run
    /// after the cursor.
    pub fn delete_word_forward(&mut self) -> EditOutcome {
        let target = word_boundary_right(&self.last_value, self.cursor);
        if target == self.cursor {
            return EditOutcome::default();
        }
        self.push_undo();
        let mut value = self.last_value.clone();
        value.replace_range(self.cursor..target, "");
        self.selection_anchor = None;
        self.last_value = value.clone();
        EditOutcome {
            new_value: Some(value),
            submitted: false,
        }
    }

    /// Alt+Left: move cursor one word backward. Selection-extend-aware.
    pub fn move_word_left(&mut self, extend_selection: bool) -> EditOutcome {
        self.update_selection_anchor(extend_selection);
        self.cursor = word_boundary_left(&self.last_value, self.cursor);
        if !extend_selection {
            self.selection_anchor = None;
        }
        EditOutcome::default()
    }

    /// Alt+Right: move cursor one word forward.
    pub fn move_word_right(&mut self, extend_selection: bool) -> EditOutcome {
        self.update_selection_anchor(extend_selection);
        self.cursor = word_boundary_right(&self.last_value, self.cursor);
        if !extend_selection {
            self.selection_anchor = None;
        }
        EditOutcome::default()
    }

    /// Pop the most recent undo snapshot and apply it, pushing the
    /// current state onto the redo stack.
    pub fn undo(&mut self) -> EditOutcome {
        let Some(snap) = self.undo.pop_back() else {
            return EditOutcome::default();
        };
        let cur = self.snapshot();
        if self.redo.len() == HISTORY_CAP {
            self.redo.pop_front();
        }
        self.redo.push_back(cur);
        self.restore(snap);
        EditOutcome {
            new_value: Some(self.last_value.clone()),
            submitted: false,
        }
    }

    pub fn redo(&mut self) -> EditOutcome {
        let Some(snap) = self.redo.pop_back() else {
            return EditOutcome::default();
        };
        let cur = self.snapshot();
        if self.undo.len() == HISTORY_CAP {
            self.undo.pop_front();
        }
        self.undo.push_back(cur);
        self.restore(snap);
        EditOutcome {
            new_value: Some(self.last_value.clone()),
            submitted: false,
        }
    }

    /// Begin or commit an IME composition. The composition string is
    /// not yet part of `last_value`; the engine paints it as a hint at
    /// the cursor. `commit_ime` splices the committed text in.
    pub fn begin_ime(&mut self, text: &str) {
        self.composing = Some(ImeComposition {
            text: text.into(),
            anchor: self.cursor,
        });
    }

    pub fn update_ime(&mut self, text: &str) {
        if let Some(c) = self.composing.as_mut() {
            c.text = text.into();
        } else {
            self.begin_ime(text);
        }
    }

    pub fn commit_ime(&mut self) -> EditOutcome {
        let Some(c) = self.composing.take() else {
            return EditOutcome::default();
        };
        if c.text.is_empty() {
            return EditOutcome::default();
        }
        // Restore cursor to anchor before splicing so the inserted text
        // lands at the original composition start.
        self.cursor = c.anchor.min(self.last_value.len());
        self.insert_str(&c.text)
    }

    pub fn cancel_ime(&mut self) {
        self.composing = None;
    }

    /// Anchor the selection on the current cursor when `extend` is true
    /// and there is no anchor yet.
    fn update_selection_anchor(&mut self, extend: bool) {
        if extend && self.selection_anchor.is_none() {
            self.selection_anchor = Some(self.cursor);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_seeds_baseline_and_lands_cursor_at_end() {
        // First sync ever — going from "" to "hello" counts as an
        // external value install, so the cursor lands at the end. This
        // matches browser semantics for `<input value="...">`: the
        // caret starts past the prefilled text, ready to append.
        let mut st = TextInputState::default();
        st.sync_with_desc("hello", true);
        assert_eq!(st.last_value, "hello");
        assert_eq!(st.cursor, 5);
        assert!(st.focused);
    }

    #[test]
    fn sync_moves_cursor_to_end_when_value_shrinks() {
        // External value rewrite → cursor jumps to end of the new value.
        // Browser-input semantics: replacing `.value` doesn't preserve
        // the caret position from the old string.
        let mut st = TextInputState::default();
        st.sync_with_desc("hello", true);
        st.cursor = 5;
        st.sync_with_desc("hi", true);
        assert_eq!(st.cursor, 2, "cursor at end of new value");
    }

    #[test]
    fn external_value_change_moves_cursor_to_end() {
        // Autocomplete scenario: user typed `/mo` (cursor=3), Lua rewrote
        // value to `/model ` (length 7) on Tab. Cursor must follow to
        // the end of the new value so the next keystroke appends.
        let mut st = TextInputState::default();
        st.sync_with_desc("/mo", true);
        st.cursor = 3;
        st.sync_with_desc("/model ", true);
        assert_eq!(
            st.cursor,
            "/model ".len(),
            "external rewrite (autocomplete) jumps cursor to end of new value"
        );
        assert!(
            st.selection_anchor.is_none(),
            "selection drops on external rewrite"
        );
    }

    #[test]
    fn sync_drops_selection_on_external_value_change() {
        let mut st = TextInputState::default();
        st.sync_with_desc("hello", true);
        st.cursor = 4;
        st.selection_anchor = Some(1);
        st.sync_with_desc("world!", true);
        assert!(
            st.selection_anchor.is_none(),
            "selection drops since old anchor doesn't map onto the new content"
        );
        assert_eq!(st.cursor, 6);
    }

    #[test]
    fn sync_keeps_cursor_when_value_unchanged() {
        let mut st = TextInputState::default();
        st.sync_with_desc("hello", true);
        st.cursor = 3;
        st.sync_with_desc("hello", true);
        assert_eq!(st.cursor, 3, "cursor preserved when value stable");
    }

    #[test]
    fn sync_clears_focused_when_lua_unfocuses() {
        let mut st = TextInputState::default();
        st.sync_with_desc("x", true);
        assert!(st.focused);
        st.sync_with_desc("x", false);
        assert!(!st.focused);
    }

    #[test]
    fn next_prev_char_boundary_skip_multibyte() {
        let s = "aé"; // 'a' (1 byte) + 'é' (2 bytes) = 3 bytes total.
        assert_eq!(next_char_boundary(s, 0), 1);
        assert_eq!(next_char_boundary(s, 1), 3);
        assert_eq!(prev_char_boundary(s, 3), 1);
        assert_eq!(prev_char_boundary(s, 1), 0);
    }

    #[test]
    fn line_helpers_split_on_newline() {
        let s = "abc\ndef\nghi";
        assert_eq!(line_start(s, 5), 4); // inside "def"
        assert_eq!(line_end(s, 5), 7); // up to next \n
    }

    #[test]
    fn snapshot_undo_redo_round_trip() {
        let mut st = TextInputState {
            last_value: "abc".into(),
            cursor: 3,
            ..TextInputState::default()
        };
        st.push_undo();
        st.last_value = "abcd".into();
        st.cursor = 4;
        // Restore to the snapshot.
        let snap = st.undo.pop_back().expect("snap");
        st.restore(snap);
        assert_eq!(st.last_value, "abc");
        assert_eq!(st.cursor, 3);
    }

    #[test]
    fn selection_range_orders_endpoints() {
        let mut st = TextInputState {
            last_value: "hello".into(),
            cursor: 1,
            selection_anchor: Some(4),
            ..TextInputState::default()
        };
        assert_eq!(st.selection_range(), Some((1, 4)));
        st.cursor = 4;
        st.selection_anchor = Some(1);
        assert_eq!(st.selection_range(), Some((1, 4)));
    }

    fn st(value: &str, cursor: usize) -> TextInputState {
        TextInputState {
            last_value: value.into(),
            cursor,
            ..TextInputState::default()
        }
    }

    #[test]
    fn insert_char_appends_at_cursor() {
        let mut s = st("hello", 5);
        let out = s.insert_char('!');
        assert_eq!(out.new_value.as_deref(), Some("hello!"));
        assert_eq!(s.last_value, "hello!");
        assert_eq!(s.cursor, 6);
        assert!(!out.submitted);
    }

    #[test]
    fn insert_str_replaces_selection() {
        let mut s = st("hello", 1);
        s.selection_anchor = Some(4);
        let out = s.insert_str("XX");
        assert_eq!(out.new_value.as_deref(), Some("hXXo"));
        assert_eq!(s.cursor, 3);
        assert!(s.selection_anchor.is_none());
    }

    #[test]
    fn backspace_removes_previous_char() {
        let mut s = st("hello", 5);
        let out = s.backspace();
        assert_eq!(out.new_value.as_deref(), Some("hell"));
        assert_eq!(s.cursor, 4);
    }

    #[test]
    fn backspace_at_start_is_noop() {
        let mut s = st("hello", 0);
        let out = s.backspace();
        assert!(out.new_value.is_none());
        assert_eq!(s.last_value, "hello");
    }

    #[test]
    fn backspace_with_selection_deletes_selection() {
        let mut s = st("hello", 4);
        s.selection_anchor = Some(1);
        let out = s.backspace();
        assert_eq!(out.new_value.as_deref(), Some("ho"));
        assert_eq!(s.cursor, 1);
    }

    #[test]
    fn delete_forward_removes_next_char() {
        let mut s = st("hello", 0);
        let out = s.delete_forward();
        assert_eq!(out.new_value.as_deref(), Some("ello"));
        assert_eq!(s.cursor, 0);
    }

    #[test]
    fn delete_forward_at_end_is_noop() {
        let mut s = st("hello", 5);
        let out = s.delete_forward();
        assert!(out.new_value.is_none());
    }

    #[test]
    fn move_left_decrements_cursor() {
        let mut s = st("hello", 3);
        s.move_left(false);
        assert_eq!(s.cursor, 2);
        assert!(s.selection_anchor.is_none());
    }

    #[test]
    fn move_right_with_shift_extends_selection() {
        let mut s = st("hello", 1);
        s.move_right(true);
        assert_eq!(s.cursor, 2);
        assert_eq!(s.selection_anchor, Some(1));
        s.move_right(true);
        assert_eq!(s.cursor, 3);
        assert_eq!(s.selection_anchor, Some(1));
    }

    #[test]
    fn move_left_without_shift_clears_selection() {
        let mut s = st("hello", 4);
        s.selection_anchor = Some(1);
        s.move_left(false);
        assert_eq!(s.cursor, 3);
        assert!(s.selection_anchor.is_none());
    }

    #[test]
    fn home_end_jump_within_line() {
        let mut s = st("abc\ndef", 5);
        s.move_to_line_start(false);
        assert_eq!(s.cursor, 4);
        s.move_to_line_end(false);
        assert_eq!(s.cursor, 7);
    }

    #[test]
    fn select_all_anchors_at_zero() {
        let mut s = st("abc", 1);
        s.select_all();
        assert_eq!(s.selection_range(), Some((0, 3)));
    }

    // ── Readline editing chords (Ctrl+U/K/W, Alt+Backspace/Delete/arrows) ─

    #[test]
    fn delete_to_line_start_kills_prefix() {
        let mut s = st("hello world", 6); // cursor at 'w'
        let outcome = s.delete_to_line_start();
        assert_eq!(s.last_value, "world");
        assert_eq!(s.cursor, 0);
        assert_eq!(outcome.new_value.as_deref(), Some("world"));
    }

    #[test]
    fn delete_to_line_start_at_start_is_noop() {
        let mut s = st("hello", 0);
        let outcome = s.delete_to_line_start();
        assert!(outcome.new_value.is_none());
        assert_eq!(s.last_value, "hello");
    }

    #[test]
    fn delete_to_line_end_kills_suffix() {
        let mut s = st("hello world", 5); // cursor at space
        let outcome = s.delete_to_line_end();
        assert_eq!(s.last_value, "hello");
        assert_eq!(s.cursor, 5);
        assert_eq!(outcome.new_value.as_deref(), Some("hello"));
    }

    #[test]
    fn delete_to_line_end_preserves_trailing_newline() {
        // Multi-line: cursor mid-line should kill to newline but keep it.
        let mut s = st("hello\nworld", 3); // cursor in "hello"
        let _ = s.delete_to_line_end();
        assert_eq!(s.last_value, "hel\nworld");
    }

    #[test]
    fn delete_word_backward_skips_whitespace_then_word() {
        let mut s = st("foo bar baz", 11); // cursor at end
        let _ = s.delete_word_backward();
        assert_eq!(s.last_value, "foo bar ");
        assert_eq!(s.cursor, 8);
    }

    #[test]
    fn delete_word_backward_with_trailing_spaces_eats_them() {
        let mut s = st("foo   ", 6);
        let _ = s.delete_word_backward();
        assert_eq!(s.last_value, "");
    }

    #[test]
    fn delete_word_backward_at_zero_is_noop() {
        let mut s = st("abc", 0);
        let outcome = s.delete_word_backward();
        assert!(outcome.new_value.is_none());
        assert_eq!(s.last_value, "abc");
    }

    #[test]
    fn delete_word_forward_drops_next_word() {
        let mut s = st("foo bar baz", 0);
        let _ = s.delete_word_forward();
        assert_eq!(s.last_value, " bar baz");
        assert_eq!(s.cursor, 0);
    }

    #[test]
    fn move_word_left_jumps_to_previous_word_start() {
        let mut s = st("foo bar baz", 11);
        let _ = s.move_word_left(false);
        assert_eq!(s.cursor, 8); // start of "baz"
        let _ = s.move_word_left(false);
        assert_eq!(s.cursor, 4); // start of "bar"
    }

    #[test]
    fn move_word_right_jumps_past_next_word_end() {
        let mut s = st("foo bar baz", 0);
        let _ = s.move_word_right(false);
        assert_eq!(s.cursor, 3); // end of "foo"
        let _ = s.move_word_right(false);
        assert_eq!(s.cursor, 7); // end of "bar"
    }

    #[test]
    fn word_boundary_handles_punctuation_as_separator() {
        // Punctuation is not a word char, so it acts as a boundary.
        let mut s = st("a-b-c", 5);
        let _ = s.delete_word_backward();
        assert_eq!(s.last_value, "a-b-");
    }

    #[test]
    fn word_boundary_handles_underscore_as_word_char() {
        // Underscore counts as a word char (readline default class).
        let mut s = st("foo_bar", 7);
        let _ = s.delete_word_backward();
        assert_eq!(
            s.last_value, "",
            "underscore-joined identifier deletes as one word"
        );
    }

    #[test]
    fn move_up_down_preserves_column() {
        let mut s = st("hello\nworld", 3);
        s.move_down(false);
        assert_eq!(s.cursor, 9, "column 3 on second line");
        s.move_up(false);
        assert_eq!(s.cursor, 3);
    }

    #[test]
    fn move_up_clamps_to_short_line() {
        let mut s = st("ab\nhello", 7); // cursor at 'l' (col 4)
        s.move_up(false);
        assert_eq!(s.cursor, 2, "clamped to end of short line");
    }

    #[test]
    fn undo_after_insert_restores_value() {
        let mut s = st("hello", 5);
        s.insert_char('!');
        let out = s.undo();
        assert_eq!(out.new_value.as_deref(), Some("hello"));
        assert_eq!(s.last_value, "hello");
        assert_eq!(s.cursor, 5);
    }

    #[test]
    fn redo_replays_undone_edit() {
        let mut s = st("hello", 5);
        s.insert_char('!');
        s.undo();
        let out = s.redo();
        assert_eq!(out.new_value.as_deref(), Some("hello!"));
        assert_eq!(s.last_value, "hello!");
        assert_eq!(s.cursor, 6);
    }

    #[test]
    fn undo_stack_caps_at_history_cap() {
        let mut s = st("", 0);
        for _ in 0..(HISTORY_CAP + 10) {
            s.insert_char('x');
        }
        assert!(s.undo.len() <= HISTORY_CAP);
    }

    #[test]
    fn ime_compose_then_commit_inserts_text() {
        let mut s = st("ab", 1);
        s.begin_ime("XYZ");
        let out = s.commit_ime();
        assert_eq!(out.new_value.as_deref(), Some("aXYZb"));
        assert!(s.composing.is_none());
        assert_eq!(s.cursor, 4);
    }

    #[test]
    fn ime_cancel_drops_composition() {
        let mut s = st("ab", 1);
        s.begin_ime("X");
        s.cancel_ime();
        assert!(s.composing.is_none());
        assert_eq!(s.last_value, "ab");
    }

    #[test]
    fn allows_newline_insert_only_above_one() {
        assert!(!allows_newline_insert(1));
        assert!(allows_newline_insert(2));
        assert!(allows_newline_insert(8));
    }

    #[test]
    fn insert_str_handles_multibyte() {
        let mut s = st("aé", 3);
        let out = s.insert_char('ö');
        assert_eq!(out.new_value.as_deref(), Some("aéö"));
        assert_eq!(s.cursor, 5, "byte cursor advances by 2 for ö");
    }

    // ── Soft-wrap (multi-line text_input) ────────────────────────────

    #[test]
    fn wrap_value_short_line_emits_one_row() {
        let rows = wrap_value("hi", 10);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].start_byte, 0);
        assert_eq!(rows[0].end_byte, 2);
        assert_eq!(rows[0].width, 2);
    }

    #[test]
    fn wrap_value_word_wraps_at_viewport_width() {
        // "hello world foo bar" wrapped to 11 cells.
        // Greedy word-wrap: "hello world" (11) | " foo bar" → "foo bar"
        // (the leading whitespace at a line break is consumed).
        let rows = wrap_value("hello world foo bar", 11);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(
            &"hello world foo bar"[rows[0].start_byte..rows[0].end_byte],
            "hello world"
        );
        assert!(rows[1].start_byte > 0);
        assert!(!rows[1].terminated_by_newline);
    }

    #[test]
    fn wrap_value_visible_count_grows_then_clamps() {
        // Soft-wrap: line of "abcd efgh ijkl mnop qrst" at width=9 ought
        // to produce more than one row.
        let value = "abcd efgh ijkl mnop qrst";
        let single = soft_wrapped_line_count(value, 100);
        assert_eq!(single, 1, "wide viewport keeps one row");
        let many = soft_wrapped_line_count(value, 9);
        assert!(many >= 3, "narrow viewport produces multiple rows: {many}");
    }

    #[test]
    fn wrap_value_respects_hard_newlines() {
        let rows = wrap_value("a\nbc\n", 100);
        // Three rows: "a", "bc", "" — the trailing newline emits an
        // empty cursor-target row.
        assert_eq!(rows.len(), 3, "{rows:?}");
        assert!(rows[0].terminated_by_newline);
        assert!(rows[1].terminated_by_newline);
        assert_eq!(rows[2].start_byte, 5);
        assert_eq!(rows[2].end_byte, 5);
    }

    #[test]
    fn wrap_value_long_word_char_wraps() {
        // Single token wider than the viewport — must char-wrap.
        let rows = wrap_value("abcdefghij", 4);
        assert!(rows.len() >= 3, "{rows:?}");
        // Total bytes covered equals input length.
        let covered: usize = rows.iter().map(|r| r.end_byte - r.start_byte).sum();
        // First row has no implicit gap; sum may equal len precisely
        // because no whitespace is dropped.
        assert!(covered >= 10);
    }

    #[test]
    fn cursor_in_wrap_maps_byte_to_visual_position() {
        let value = "hello world foo bar";
        let rows = wrap_value(value, 11);
        // Cursor right after "hello" → row 0, col 5.
        let pos = cursor_in_wrap_for(value, &rows, 5);
        assert_eq!(pos, (0, 5));
        // Cursor at end → final row, end col.
        let end = cursor_in_wrap_for(value, &rows, value.len());
        assert_eq!(end.0, rows.len() - 1);
    }

    #[test]
    fn cursor_in_wrap_with_hard_newline_lands_on_post_row() {
        let value = "abc\ndef";
        let rows = wrap_value(value, 100);
        // Cursor at offset 4 (start of "def") → row 1, col 0.
        let pos = cursor_in_wrap_for(value, &rows, 4);
        assert_eq!(pos, (1, 0));
    }

    #[test]
    fn byte_offset_for_visual_clamps_to_short_row() {
        let value = "hello world foo bar";
        let rows = wrap_value(value, 11);
        // Try column 20 on row 1 — must clamp to row's end.
        let off = byte_offset_for_visual(value, &rows, 1, 20);
        assert_eq!(off, rows[1].end_byte);
    }

    #[test]
    fn byte_offset_for_visual_round_trips_with_cursor_in_wrap() {
        let value = "abcdef ghi jklmno pqrstu";
        let rows = wrap_value(value, 8);
        for cursor in [0, 3, 7, 11, value.len()] {
            let (row, col) = cursor_in_wrap_for(value, &rows, cursor);
            let back = byte_offset_for_visual(value, &rows, row, col);
            assert_eq!(back, cursor, "round-trip failed @ cursor={cursor}");
        }
    }
}
