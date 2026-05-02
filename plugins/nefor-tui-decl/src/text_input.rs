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
}
