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

use std::collections::VecDeque;

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
    /// past the rightmost visible column.
    pub scroll_x: u16,
    /// Vertical scroll in lines. Increments past the bottom visible row.
    pub scroll_y: u16,
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
    ///   and clamp cursor + selection + scroll to the new bounds.
    pub fn sync_with_desc(&mut self, value: &str, focused: bool) {
        self.focused = focused;
        if self.last_value != value {
            self.last_value = value.to_string();
            self.cursor = clamp_to_char_boundary(value, self.cursor);
            self.selection_anchor = self
                .selection_anchor
                .map(|a| clamp_to_char_boundary(value, a));
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

/// Whether the input should accept Shift+Enter newline insertion. Single-
/// line inputs (`max_lines = 1`) ignore Shift+Enter and bubble it.
pub fn allows_newline_insert(max_lines: u16) -> bool {
    max_lines > 1
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

    /// Move cursor to the previous line, preserving column when possible.
    /// No-op on the first line.
    pub fn move_up(&mut self, extend_selection: bool) -> EditOutcome {
        self.update_selection_anchor(extend_selection);
        let cur_line_start = line_start(&self.last_value, self.cursor);
        if cur_line_start == 0 {
            return EditOutcome::default();
        }
        let col = self.cursor - cur_line_start;
        // Previous line ends at cur_line_start-1 (the `\n` itself).
        let prev_line_end = cur_line_start - 1;
        let prev_line_start = line_start(&self.last_value, prev_line_end);
        let prev_line_len = prev_line_end - prev_line_start;
        self.cursor = prev_line_start + col.min(prev_line_len);
        if !extend_selection {
            self.selection_anchor = None;
        }
        EditOutcome::default()
    }

    pub fn move_down(&mut self, extend_selection: bool) -> EditOutcome {
        self.update_selection_anchor(extend_selection);
        let cur_line_start = line_start(&self.last_value, self.cursor);
        let cur_line_end = line_end(&self.last_value, self.cursor);
        if cur_line_end == self.last_value.len() {
            // No next line.
            return EditOutcome::default();
        }
        let col = self.cursor - cur_line_start;
        let next_line_start = cur_line_end + 1;
        let next_line_end = line_end(&self.last_value, next_line_start);
        let next_line_len = next_line_end - next_line_start;
        self.cursor = next_line_start + col.min(next_line_len);
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
    fn sync_seeds_baseline_and_keeps_cursor() {
        let mut st = TextInputState::default();
        st.sync_with_desc("hello", true);
        assert_eq!(st.last_value, "hello");
        assert_eq!(st.cursor, 0);
        assert!(st.focused);
    }

    #[test]
    fn sync_clamps_cursor_when_value_shrinks() {
        let mut st = TextInputState::default();
        st.sync_with_desc("hello", true);
        st.cursor = 5;
        st.sync_with_desc("hi", true);
        assert_eq!(st.cursor, 2, "cursor clamped to new len");
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
}
