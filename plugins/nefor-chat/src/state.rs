//! Internal chat state: transcript, input buffer, scroll, and dimensions.
//!
//! The plugin owns a single [`ChatState`] that every incoming event mutates
//! and every render pass reads. No threading — the main loop is single-task
//! in v1, so a plain `&mut` suffices.

/// Who authored a transcript entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// User prompts (what the human typed, committed with Enter).
    User,
    /// Claude's reply text, including streaming deltas.
    Assistant,
    /// Diagnostic lines (tool starts, errors, mock-plugin meta).
    System,
}

/// One line in the transcript. `text` is the full content — wrapping
/// happens at render time, not here, so the same state renders correctly
/// after a resize.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptEntry {
    /// Who said it.
    pub role: Role,
    /// Raw body text. Plain (no markdown) per the v1 scope.
    pub text: String,
    /// If `true`, the assistant entry is still receiving deltas. Only ever
    /// set for [`Role::Assistant`]. `cc.stream.end` flips this to `false`.
    pub streaming: bool,
}

impl TranscriptEntry {
    fn new(role: Role, text: String) -> Self {
        Self {
            role,
            text,
            streaming: false,
        }
    }
}

/// Editable input buffer with a cursor position measured in *char*
/// offsets (not bytes, not display columns). Char offsets keep cursor
/// arithmetic simple while still being safe across multi-byte UTF-8.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputBuffer {
    chars: Vec<char>,
    cursor: usize,
}

impl InputBuffer {
    /// Current char-count length (not byte, not column width).
    pub fn len(&self) -> usize {
        self.chars.len()
    }

    /// Cursor position as a char offset in `[0, len()]`.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Render the buffer as a single string.
    pub fn as_string(&self) -> String {
        self.chars.iter().collect()
    }

    /// Insert a single character at the cursor, advancing the cursor.
    pub fn insert_char(&mut self, c: char) {
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
    }

    /// Insert a whole string at the cursor (used by bracketed paste).
    /// Newlines are replaced with spaces — the input line is single-row,
    /// and newlines arrived via paste shouldn't visually break the
    /// cell model. Callers that want multi-line input can opt in later.
    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            let c = if c == '\n' || c == '\r' { ' ' } else { c };
            self.insert_char(c);
        }
    }

    /// Delete the character immediately before the cursor, if any.
    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    /// Move cursor one char left, clamped to 0.
    pub fn cursor_left(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
        }
    }

    /// Move cursor one char right, clamped to `len()`.
    pub fn cursor_right(&mut self) {
        if self.cursor < self.chars.len() {
            self.cursor += 1;
        }
    }

    /// Jump cursor to start.
    pub fn cursor_home(&mut self) {
        self.cursor = 0;
    }

    /// Jump cursor to end.
    pub fn cursor_end(&mut self) {
        self.cursor = self.chars.len();
    }

    /// Clear the buffer and reset the cursor. Called after Enter flushes
    /// the current line as a user prompt.
    //
    // `#[allow(dead_code)]` keeps integration-test builds — which `#[path]`-
    // include state.rs but never reach `main.rs` — warning-free. Production
    // uses this via `src/main.rs::handle_key` on Enter.
    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
    }
}

/// Terminal dimensions the plugin has been told about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dims {
    /// Total columns in the grid.
    pub cols: u32,
    /// Total rows in the grid (input line is always the last row).
    pub rows: u32,
}

impl Dims {
    /// Fallback dims used before the first `nefor-tui.ready` arrives. Any
    /// reasonable shell terminal is larger than this, so we paint
    /// *something* even if the message arrives slightly before ready.
    pub fn fallback() -> Self {
        Self { cols: 80, rows: 24 }
    }
}

/// Everything the renderer needs to produce a frame.
#[derive(Debug, Clone)]
pub struct ChatState {
    /// Ordered transcript. Oldest first.
    pub transcript: Vec<TranscriptEntry>,
    /// Current line being composed.
    pub input: InputBuffer,
    /// Rows scrolled up from the bottom of the transcript. 0 → stuck to
    /// newest; larger values show older messages.
    pub scroll_offset: u32,
    /// Latest dimensions we've been told about.
    pub dims: Dims,
    /// `false` until we've seen `nefor-tui.ready`. No rendering happens
    /// before this flips.
    pub tui_ready: bool,
    /// True between `cc.prompt` dispatch and `cc.stream.end`/`cc.turn.error`.
    /// Renderer injects a "[claude is thinking...]" row while this is set
    /// *and* no assistant entry has started streaming yet.
    pub pending: bool,
}

impl Default for ChatState {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatState {
    /// Build an empty state with fallback dims.
    pub fn new() -> Self {
        Self {
            transcript: Vec::new(),
            input: InputBuffer::default(),
            scroll_offset: 0,
            dims: Dims::fallback(),
            tui_ready: false,
            pending: false,
        }
    }

    // `#[allow(dead_code)]` here — as for `clear`/`clamp_scroll` below —
    // is because the integration tests in `tests/render.rs` `#[path]`-
    // include this module but never reach `main.rs`, which is where these
    // helpers are actually called.

    /// Mark a turn as in-flight. Called when the plugin ships `cc.prompt`.
    #[allow(dead_code)]
    pub fn begin_turn(&mut self) {
        self.pending = true;
    }

    /// Mark the in-flight turn as finished. Called on `cc.stream.end` or
    /// `cc.turn.error`.
    #[allow(dead_code)]
    pub fn end_turn(&mut self) {
        self.pending = false;
    }

    /// Append a finished entry (user prompt, system line).
    pub fn push_entry(&mut self, role: Role, text: String) {
        self.transcript.push(TranscriptEntry::new(role, text));
        self.reset_scroll();
    }

    /// Append a chunk of streaming assistant text. Creates a new streaming
    /// entry if none is currently open; otherwise appends to the last
    /// streaming assistant entry.
    pub fn append_assistant_delta(&mut self, chunk: &str) {
        if let Some(last) = self.transcript.last_mut() {
            if last.role == Role::Assistant && last.streaming {
                last.text.push_str(chunk);
                self.reset_scroll();
                return;
            }
        }
        self.transcript.push(TranscriptEntry {
            role: Role::Assistant,
            text: chunk.to_owned(),
            streaming: true,
        });
        self.reset_scroll();
    }

    /// Finalize the open streaming assistant entry.
    ///
    /// If `final_text` is `Some`, replace the streaming entry's text with
    /// the authoritative value (e.g. mock-plugin's `cc.stream.end.text`
    /// after reconciliation). Otherwise keep whatever accumulated from
    /// deltas. If no streaming entry is open, `final_text` (if present)
    /// is appended as a new assistant entry — so `cc.stream.end` never
    /// silently drops content.
    pub fn finalize_assistant(&mut self, final_text: Option<String>) {
        if let Some(last) = self.transcript.last_mut() {
            if last.role == Role::Assistant && last.streaming {
                if let Some(t) = final_text {
                    last.text = t;
                }
                last.streaming = false;
                self.reset_scroll();
                return;
            }
        }
        if let Some(t) = final_text {
            self.transcript.push(TranscriptEntry {
                role: Role::Assistant,
                text: t,
                streaming: false,
            });
            self.reset_scroll();
        }
    }

    /// Scroll up (toward older content) by `delta` rows.
    pub fn scroll_up(&mut self, delta: u32) {
        self.scroll_offset = self.scroll_offset.saturating_add(delta);
    }

    /// Scroll down (toward newer content) by `delta` rows.
    pub fn scroll_down(&mut self, delta: u32) {
        self.scroll_offset = self.scroll_offset.saturating_sub(delta);
    }

    /// Clamp the scroll offset to `max` (computed by the renderer once
    /// wrap/layout is known). Callers should invoke this after any
    /// content change that shrinks the transcript.
    //
    // Reserved for post-v1 use: once render passes compute a max-offset
    // directly (today we auto-snap to 0 on new content), this becomes the
    // hook that keeps a user-scrolled view inside bounds. Kept on the API
    // so callers don't have to add it later.
    #[allow(dead_code)]
    pub fn clamp_scroll(&mut self, max: u32) {
        if self.scroll_offset > max {
            self.scroll_offset = max;
        }
    }

    fn reset_scroll(&mut self) {
        // Fresh content always snaps the view to the bottom. Users who
        // have scrolled up explicitly would lose their place here; we
        // accept that in v1 to keep the logic trivial.
        self.scroll_offset = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_buffer_empty_by_default() {
        let b = InputBuffer::default();
        assert_eq!(b.len(), 0);
        assert_eq!(b.cursor(), 0);
        assert_eq!(b.as_string(), "");
    }

    #[test]
    fn insert_and_backspace() {
        let mut b = InputBuffer::default();
        b.insert_char('h');
        b.insert_char('i');
        assert_eq!(b.as_string(), "hi");
        b.backspace();
        assert_eq!(b.as_string(), "h");
        b.backspace();
        b.backspace(); // no-op on empty
        assert_eq!(b.as_string(), "");
    }

    #[test]
    fn cursor_navigation() {
        let mut b = InputBuffer::default();
        b.insert_str("abc");
        assert_eq!(b.cursor(), 3);
        b.cursor_home();
        assert_eq!(b.cursor(), 0);
        b.cursor_right();
        assert_eq!(b.cursor(), 1);
        b.cursor_end();
        assert_eq!(b.cursor(), 3);
        b.cursor_left();
        assert_eq!(b.cursor(), 2);
    }

    #[test]
    fn insert_at_cursor_middle() {
        let mut b = InputBuffer::default();
        b.insert_str("ac");
        b.cursor_left();
        b.insert_char('b');
        assert_eq!(b.as_string(), "abc");
    }

    #[test]
    fn paste_replaces_newlines_with_spaces() {
        let mut b = InputBuffer::default();
        b.insert_str("a\nb\rc");
        assert_eq!(b.as_string(), "a b c");
    }

    #[test]
    fn assistant_delta_creates_streaming_entry() {
        let mut s = ChatState::new();
        s.append_assistant_delta("hel");
        s.append_assistant_delta("lo");
        assert_eq!(s.transcript.len(), 1);
        assert_eq!(s.transcript[0].role, Role::Assistant);
        assert_eq!(s.transcript[0].text, "hello");
        assert!(s.transcript[0].streaming);
    }

    #[test]
    fn finalize_assistant_with_authoritative_text() {
        let mut s = ChatState::new();
        s.append_assistant_delta("partial");
        s.finalize_assistant(Some("final!".into()));
        assert_eq!(s.transcript[0].text, "final!");
        assert!(!s.transcript[0].streaming);
    }

    #[test]
    fn finalize_assistant_without_text_keeps_deltas() {
        let mut s = ChatState::new();
        s.append_assistant_delta("keep me");
        s.finalize_assistant(None);
        assert_eq!(s.transcript[0].text, "keep me");
        assert!(!s.transcript[0].streaming);
    }

    #[test]
    fn finalize_without_streaming_appends_if_text_present() {
        let mut s = ChatState::new();
        s.push_entry(Role::User, "hi".into());
        s.finalize_assistant(Some("hi back".into()));
        assert_eq!(s.transcript.len(), 2);
        assert_eq!(s.transcript[1].role, Role::Assistant);
        assert!(!s.transcript[1].streaming);
    }

    #[test]
    fn user_entry_interrupting_stream_leaves_prior_streaming_alone() {
        // This is the awkward case: a user hits Enter mid-stream. We
        // still close out the stream-entry as streaming=true and add the
        // user entry; the next delta would open a fresh assistant entry.
        let mut s = ChatState::new();
        s.append_assistant_delta("half-reply");
        s.push_entry(Role::User, "interrupt".into());
        assert_eq!(s.transcript.len(), 2);
        assert_eq!(s.transcript[0].role, Role::Assistant);
        assert_eq!(s.transcript[1].role, Role::User);
        // Next delta starts a new entry because last entry is now User.
        s.append_assistant_delta("new reply");
        assert_eq!(s.transcript.len(), 3);
        assert_eq!(s.transcript[2].role, Role::Assistant);
    }

    #[test]
    fn scroll_clamped_at_zero() {
        let mut s = ChatState::new();
        s.scroll_down(5);
        assert_eq!(s.scroll_offset, 0);
        s.scroll_up(3);
        assert_eq!(s.scroll_offset, 3);
        s.scroll_down(10);
        assert_eq!(s.scroll_offset, 0);
    }

    #[test]
    fn clamp_scroll_to_max() {
        let mut s = ChatState::new();
        s.scroll_up(100);
        s.clamp_scroll(5);
        assert_eq!(s.scroll_offset, 5);
    }
}
