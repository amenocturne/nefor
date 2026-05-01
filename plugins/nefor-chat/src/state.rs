//! Internal chat state: transcript, input buffer, scroll, and dimensions.
//!
//! The plugin owns a single [`ChatState`] that every incoming event mutates
//! and every render pass reads. No threading — the main loop is single-task
//! in v1, so a plain `&mut` suffices.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

// (No watchdog deadline. The previous "no response from harness" banner has
// been replaced with a live "thinking… Ns" indicator on the placeholder row;
// the user decides when to interrupt rather than the chat plugin.)

/// Max retained prompt-history entries. Bounded so a long session can't grow
/// the buffer indefinitely; oldest entries fall off the front when the cap is
/// reached.
pub const PROMPT_HISTORY_CAP: usize = 200;

/// Layout constants — kept in lockstep with `render.rs` so the
/// `transcript_rows()` helper produces the same value the renderer uses.
/// `render.rs` owns horizontal padding (`HPAD`), which only affects column
/// budget, not transcript-row count, so we don't mirror it here.
const VPAD: u32 = 1;
const MAX_INPUT_ROWS: u32 = 6;

/// Cap on the DAG-panel height (in rows). Beyond this, the panel truncates
/// with a `… +K more` overflow row so a runaway run never crowds out the
/// transcript or the input box.
pub const DAG_PANEL_MAX_ROWS: u32 = 8;

/// How long a finished DAG run lingers in the panel after `dag.run_complete`
/// before the per-second tick prunes it. Two seconds is short enough to
/// avoid stale panels and long enough that the user sees the final
/// green/red marker as confirmation the run finished.
//
// `#[allow(dead_code)]`: read by the binary build (`main.rs::handle_envelope`
// + the tick handler) but the integration test build only `#[path]`-includes
// state/render/sidebar/wrap, so it doesn't see the use site.
#[allow(dead_code)]
pub const DAG_RUN_LINGER_MS: u64 = 2000;

/// Minimum terminal width before the right sidebar shows up. Below this the
/// chat pane needs every column for typing room — a 30-col-wide sidebar would
/// crush the input box on a narrow split or laptop. The renderer treats this
/// as a hard auto-hide gate, independent of `ChatState::sidebar_visible`.
pub const SIDEBAR_MIN_TERMINAL_COLS: u32 = 100;

/// Lower clamp for sidebar width when it's visible. Below 28 cols the DAG
/// node rows (`<glyph> <node_id> <elapsed>`) start truncating into uselessness.
pub const SIDEBAR_MIN_COLS: u32 = 28;

/// Upper clamp for sidebar width. Above 40 cols we're crowding the chat pane
/// without adding signal — the widgets we render are intrinsically narrow.
pub const SIDEBAR_MAX_COLS: u32 = 40;

/// Who authored a transcript entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// User prompts (what the human typed, committed with Enter).
    User,
    /// Claude's reply text, including streaming deltas.
    Assistant,
    /// Diagnostic lines (tool starts, errors, harness meta).
    System,
    /// Tool invocation. The renderer collapses to a one-liner by default and
    /// expands (full input + output) when the user toggles Ctrl+O.
    Tool,
}

/// One line in the transcript. `text` is the full content — wrapping
/// happens at render time, not here, so the same state renders correctly
/// after a resize.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptEntry {
    /// Who said it.
    pub role: Role,
    /// Raw body text. Plain (no markdown) per the v1 scope. For
    /// [`Role::Tool`] this carries the *output* once the tool returns;
    /// while running it stays empty.
    pub text: String,
    /// If `true`, the assistant entry is still receiving deltas. Only ever
    /// set for [`Role::Assistant`]. `chat.stream.end` flips this to `false`.
    pub streaming: bool,
    /// Model that produced this assistant turn. `None` until the harness
    /// reports it on `chat.stream.end`. Drives the per-turn footer.
    pub model: Option<String>,
    /// Wall-clock duration of this assistant turn, set on `chat.stream.end`.
    /// `None` for streaming-incomplete turns and replayed history.
    pub duration_ms: Option<u64>,
    /// Optional tool-specific payload — present iff `role == Role::Tool`.
    pub tool: Option<ToolPayload>,
    /// Optional reasoning trace attached to an assistant entry. Present
    /// only on `Role::Assistant`; carries the model's thinking output
    /// (Ollama's `delta.reasoning` for Qwen 3 / Gemma 3). Renders as a
    /// dim live preview while `reasoning_streaming` is true; collapses
    /// to a `▶ reasoning (Ns)` row once content begins (or the turn
    /// ends with reasoning-only). Toggled to expanded view by Ctrl+O,
    /// same binding used for tool I/O details. NEVER fed back into the
    /// next request's history.
    pub reasoning: Option<ReasoningPayload>,
}

/// Reasoning trace carried on an assistant entry. Mirrors the shape of
/// `ToolPayload` (collapsed-by-default + expandable on Ctrl+O) so the
/// renderer can reuse the same toggle convention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReasoningPayload {
    /// Accumulated reasoning text. Grows during streaming and finalises
    /// with the authoritative `reasoning_end.text`.
    pub text: String,
    /// `true` while `chat.stream.reasoning_delta` events are still
    /// landing for this entry; flipped to `false` on
    /// `chat.stream.reasoning_end` (or the entry's own `chat.stream.end`
    /// as a defensive close). Drives the live preview vs. collapsed
    /// row distinction in the renderer.
    pub streaming: bool,
    /// Wall-clock duration from the first reasoning chunk to the
    /// `reasoning_end` boundary. Stamped from
    /// `chat.stream.reasoning_end.duration_ms` (provider-side timer).
    /// Used in the collapsed-row label `▶ reasoning (Ns)`.
    pub duration_ms: Option<u64>,
}

/// Tool-call payload carried on a [`Role::Tool`] entry. Holds the harness's
/// `tool_use_id` so a later `chat.tool.end` can find this row, the raw
/// input map for the expanded view, and the output once it arrives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPayload {
    /// Harness-assigned id (Claude's `tool_use_id`). Used to match a
    /// later `chat.tool.end` to the row it should attach to. Empty if
    /// the producer didn't supply one.
    pub id: String,
    /// Tool name, e.g. `"Bash"`, `"Read"`.
    pub name: String,
    /// Stringified JSON of the tool input — kept as a string so the
    /// `Eq` impl on [`TranscriptEntry`] stays cheap and the renderer can
    /// pretty-print without re-serialising.
    pub input_json: String,
    /// Tool output text. `None` while the tool is still running.
    pub output: Option<String>,
    /// `true` if the tool returned an error.
    pub error: bool,
}

impl TranscriptEntry {
    fn new(role: Role, text: String) -> Self {
        Self {
            role,
            text,
            streaming: false,
            model: None,
            duration_ms: None,
            tool: None,
            reasoning: None,
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
    /// Newlines (`\n`) are preserved so pasted code or multi-paragraph
    /// content keeps its structure. Carriage returns (`\r`) — which arrive
    /// only from CRLF or legacy Mac line endings — normalize to `\n` so
    /// the buffer always uses LF as the row separator.
    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            if c == '\r' {
                self.insert_char('\n');
            } else {
                self.insert_char(c);
            }
        }
    }

    /// Split the buffer on `\n` into logical lines. Always returns at
    /// least one element (an empty buffer yields `[""]`). Lines do not
    /// include the trailing newline character.
    pub fn lines(&self) -> Vec<String> {
        let mut out: Vec<String> = vec![String::new()];
        for &c in &self.chars {
            if c == '\n' {
                out.push(String::new());
            } else {
                out.last_mut().expect("at least one line").push(c);
            }
        }
        out
    }

    /// `true` iff the buffer contains at least one `\n`. Drives the
    /// up/down arrow split between in-buffer cursor nav and history nav.
    pub fn is_multiline(&self) -> bool {
        self.chars.contains(&'\n')
    }

    /// Logical row index of the cursor, where each `\n` starts a new row.
    /// Always `< lines().len()`.
    pub fn cursor_row(&self) -> usize {
        self.chars[..self.cursor]
            .iter()
            .filter(|c| **c == '\n')
            .count()
    }

    /// Cursor offset within its current logical row (char-count, not
    /// byte/column). 0 at row start, `row.len()` at row end.
    pub fn cursor_col_within_row(&self) -> usize {
        let mut col = 0usize;
        for &c in &self.chars[..self.cursor] {
            if c == '\n' {
                col = 0;
            } else {
                col += 1;
            }
        }
        col
    }

    /// Move cursor one logical row up, preserving column where possible.
    /// Returns `true` iff the cursor actually moved (i.e. there was a row
    /// above the current one). On the top row this is a no-op.
    pub fn move_cursor_up_in_buffer(&mut self) -> bool {
        let row = self.cursor_row();
        if row == 0 {
            return false;
        }
        let col = self.cursor_col_within_row();
        let lines = self.lines();
        let target_row = row - 1;
        let target_col = col.min(lines[target_row].chars().count());
        // Walk char-offsets to compute the new cursor position.
        let mut new_cursor = 0usize;
        for (i, line) in lines.iter().enumerate() {
            if i < target_row {
                new_cursor += line.chars().count() + 1; // +1 for the '\n'
            } else {
                new_cursor += target_col;
                break;
            }
        }
        self.cursor = new_cursor;
        true
    }

    /// Move cursor one logical row down, preserving column where possible.
    /// Returns `true` iff the cursor actually moved (i.e. there was a row
    /// below the current one). On the bottom row this is a no-op.
    pub fn move_cursor_down_in_buffer(&mut self) -> bool {
        let row = self.cursor_row();
        let col = self.cursor_col_within_row();
        let lines = self.lines();
        if row + 1 >= lines.len() {
            return false;
        }
        let target_row = row + 1;
        let target_col = col.min(lines[target_row].chars().count());
        let mut new_cursor = 0usize;
        for (i, line) in lines.iter().enumerate() {
            if i < target_row {
                new_cursor += line.chars().count() + 1;
            } else {
                new_cursor += target_col;
                break;
            }
        }
        self.cursor = new_cursor;
        true
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

    /// Compute the char-offset at the start of the previous word.
    ///
    /// "Word" = maximal run of non-whitespace chars. Algorithm: skip
    /// whitespace going left, then skip non-whitespace going left, stop at
    /// the start of that run. Returns 0 when already at or before the
    /// first word.
    pub fn cursor_word_back(&self) -> usize {
        let mut i = self.cursor;
        while i > 0 && self.chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !self.chars[i - 1].is_whitespace() {
            i -= 1;
        }
        i
    }

    /// Compute the char-offset at the end of the next word.
    ///
    /// Skip whitespace going right, then skip non-whitespace going right,
    /// stop one past the last non-whitespace char of that run. Matches
    /// macOS Option+Right convention: `|hello world` → `hello| world`.
    pub fn cursor_word_forward(&self) -> usize {
        let n = self.chars.len();
        let mut i = self.cursor;
        while i < n && self.chars[i].is_whitespace() {
            i += 1;
        }
        while i < n && !self.chars[i].is_whitespace() {
            i += 1;
        }
        i
    }

    /// Move cursor to the start of the previous word.
    pub fn move_word_back(&mut self) {
        self.cursor = self.cursor_word_back();
    }

    /// Move cursor to the end of the next word.
    pub fn move_word_forward(&mut self) {
        self.cursor = self.cursor_word_forward();
    }

    /// Delete from the start of the previous word up to the cursor.
    pub fn delete_word_back(&mut self) {
        let start = self.cursor_word_back();
        if start < self.cursor {
            self.chars.drain(start..self.cursor);
            self.cursor = start;
        }
    }

    /// Delete from the cursor up to the end of the next word.
    pub fn delete_word_forward(&mut self) {
        let end = self.cursor_word_forward();
        if self.cursor < end {
            self.chars.drain(self.cursor..end);
        }
    }

    /// Delete from the start of the buffer up to the cursor (Ctrl+U).
    /// Single-line buffer, so "line start" == buffer start.
    pub fn delete_to_start(&mut self) {
        if self.cursor > 0 {
            self.chars.drain(0..self.cursor);
            self.cursor = 0;
        }
    }

    /// Delete from the cursor to the end of the buffer (Ctrl+K).
    /// Single-line buffer, so "line end" == buffer end.
    pub fn delete_to_end(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.truncate(self.cursor);
        }
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

/// Session-wide telemetry surfaced on the statusline.
///
/// All fields are optional — producers populate whichever subset they have.
/// `stats_seen` flips to `true` the first time any `chat.session.stats`
/// event arrives, even if every field is absent: it's the signal that "no
/// stats provider is wired" (vs. "wired but values not yet known").
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionMetadata {
    pub model: Option<String>,
    pub turns: Option<u64>,
    pub cumulative_cost_usd: Option<f64>,
    pub cumulative_input_tokens: Option<u64>,
    pub cumulative_output_tokens: Option<u64>,
    pub cumulative_cache_read: Option<u64>,
    pub cumulative_cache_creation: Option<u64>,
    /// Context window size sent to the model on the most recent turn —
    /// `input_tokens + cache_read + cache_creation` summed across all
    /// usage frames within that turn. This (not the cumulative across
    /// turns) is what "context used" means on the statusline.
    pub last_turn_context_tokens: Option<u64>,
    pub last_turn_duration_ms: Option<u64>,
    /// Output tokens generated on the most recent turn — divided by
    /// `last_turn_duration_ms` to render a `tok/s` segment on the
    /// statusline. Cumulative output is on its own field; the per-turn
    /// number is the only one with a meaningful denominator.
    pub last_turn_output_tokens: Option<u64>,
    pub stats_seen: bool,
}

impl SessionMetadata {
    /// Merge a `chat.session.stats` body into this state.
    ///
    /// Each field present in `body` overwrites the current value; absent
    /// fields are preserved. Always sets `stats_seen = true`.
    //
    // `#[allow(dead_code)]`: integration tests `#[path]`-include state.rs
    // but never reach `main.rs`, where this is invoked.
    #[allow(dead_code)]
    pub fn update_from(&mut self, body: &serde_json::Map<String, serde_json::Value>) {
        use serde_json::Value;
        self.stats_seen = true;
        if let Some(s) = body.get("model").and_then(Value::as_str) {
            self.model = Some(s.to_owned());
        }
        if let Some(n) = body.get("turns").and_then(Value::as_u64) {
            self.turns = Some(n);
        }
        if let Some(n) = body.get("cumulative_cost_usd").and_then(Value::as_f64) {
            self.cumulative_cost_usd = Some(n);
        }
        if let Some(n) = body.get("cumulative_input_tokens").and_then(Value::as_u64) {
            self.cumulative_input_tokens = Some(n);
        }
        if let Some(n) = body.get("cumulative_output_tokens").and_then(Value::as_u64) {
            self.cumulative_output_tokens = Some(n);
        }
        if let Some(n) = body.get("cumulative_cache_read").and_then(Value::as_u64) {
            self.cumulative_cache_read = Some(n);
        }
        if let Some(n) = body
            .get("cumulative_cache_creation")
            .and_then(Value::as_u64)
        {
            self.cumulative_cache_creation = Some(n);
        }
        if let Some(n) = body.get("last_turn_context_tokens").and_then(Value::as_u64) {
            self.last_turn_context_tokens = Some(n);
        }
        if let Some(n) = body.get("last_turn_duration_ms").and_then(Value::as_u64) {
            self.last_turn_duration_ms = Some(n);
        }
        if let Some(n) = body.get("last_turn_output_tokens").and_then(Value::as_u64) {
            self.last_turn_output_tokens = Some(n);
        }
    }
}

/// One row of the rendered grid as a flat (text, hl) sequence. The
/// renderer produces a snapshot per emitted line and compares against the
/// last-emitted snapshot to skip unchanged rows on the next frame.
pub type RowSnapshot = Vec<(String, u32)>;

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

/// Per-provider authentication state, sourced from `chat.auth.status` events
/// emitted by the adapter. `state` is a free-form string so the producer can
/// extend the vocabulary without lockstep changes here ("connected",
/// "login_required", "error", etc.); `message` is an optional human-readable
/// hint to surface alongside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthStatus {
    pub state: String,
    pub message: Option<String>,
}

/// Slash command entry surfaced by the autocomplete popup. `name` is the
/// command word (without the leading `/`), `hint` is a short usage description
/// shown alongside the name. The list comes from a single source so the popup
/// and the dispatcher can never drift apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommand {
    /// Command word — e.g. `"login"`.
    pub name: String,
    /// Alternate command words that resolve to the same action. Populated for
    /// commands like `/new` (alias `/clear`); empty for commands without
    /// aliases. Help and autocomplete render aliases inline alongside the
    /// canonical `name`.
    pub aliases: Vec<String>,
    /// One-line hint — e.g. `"authenticate a provider"`.
    pub hint: String,
    /// `true` when the command takes optional/required positional args. Drives
    /// whether Tab-completion appends a trailing space (so the user can type
    /// the arg directly) or just leaves the cursor at end of name.
    pub takes_args: bool,
}

/// Canonical slash-command registry. Single source of truth for the
/// autocomplete popup, the parser, and the help text — adding a built-in here
/// keeps every surface in lockstep. Order is the order shown in the popup and
/// help body.
pub fn slash_command_registry() -> Vec<SlashCommand> {
    vec![
        SlashCommand {
            name: "new".into(),
            aliases: vec!["clear".into()],
            hint: "start a fresh chat (clears transcript)".into(),
            takes_args: false,
        },
        SlashCommand {
            name: "help".into(),
            aliases: Vec::new(),
            hint: "show the help popup".into(),
            takes_args: false,
        },
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
        SlashCommand {
            name: "model".into(),
            aliases: Vec::new(),
            hint: "list/switch active model".into(),
            takes_args: true,
        },
        SlashCommand {
            name: "resume".into(),
            aliases: Vec::new(),
            hint: "resume previous session".into(),
            takes_args: true,
        },
        SlashCommand {
            name: "yolo".into(),
            aliases: Vec::new(),
            hint: "disable tool permission prompts (DANGEROUS)".into(),
            takes_args: false,
        },
        SlashCommand {
            name: "safe".into(),
            aliases: Vec::new(),
            hint: "re-enable tool permission prompts".into(),
            takes_args: false,
        },
        SlashCommand {
            name: "dag-test".into(),
            aliases: Vec::new(),
            hint: "submit a 2-node parallel DAG to the dag-scheduler as a smoke test".into(),
            takes_args: false,
        },
    ]
}

/// Filter the registry by `query` (text after the leading `/`). Case-insensitive
/// prefix match against each command's `name` AND each alias — a command is
/// returned (once, with its canonical `name`) if any of its names match. Empty
/// query returns the full registry. Sorted by registry order.
//
// `#[allow(dead_code)]`: the integration `tests/render.rs` build
// `#[path]`-includes this module but never reaches the caller in `main.rs`.
#[allow(dead_code)]
pub fn slash_command_matches(query: &str) -> Vec<SlashCommand> {
    let q = query.to_lowercase();
    slash_command_registry()
        .into_iter()
        .filter(|c| {
            c.name.to_lowercase().starts_with(&q)
                || c.aliases.iter().any(|a| a.to_lowercase().starts_with(&q))
        })
        .collect()
}

/// Modal popup overlay variants. Reusable across slash-commands that surface
/// reference-shaped information (help, model picker, future session picker).
/// While the popup is `Some`, key input is routed to the popup instead of the
/// chat input buffer; ESC closes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Popup {
    /// Static help screen — no filtering or selection. Enter / ESC / Q closes.
    Help {
        /// Body scroll offset (in rows). Up/Down adjusts by 1; PageUp/PageDown
        /// by visible height; Home/End jumps to top/bottom.
        scroll: u16,
    },
    /// Searchable model picker. Aggregates models from every connected
    /// provider, supports incremental filtering, and on Enter emits a
    /// `chat.model.set` for the cursor row.
    ModelPicker {
        /// Aggregated `(provider, model)` pairs in stable sort order.
        all_models: Vec<(String, String)>,
        /// User-typed filter query. Case-insensitive substring match against
        /// `"<provider> <model>"`.
        query: String,
        /// Cursor index into the *filtered* list.
        cursor: usize,
        /// Provider names whose `chat.models.listed` response is still
        /// outstanding. Drives the "loading from N more provider(s)…" footer.
        awaiting: HashSet<String>,
        /// First visible row in the body — kept in sync with `cursor` so the
        /// cursor row stays in view after key input or filter changes.
        scroll: u16,
    },
    /// Transient info overlay — surfaced by `chat.popup` events with
    /// `level="info"` from any plugin on the bus. Neutral styling
    /// (cyan-ish). ESC / Q closes.
    Info {
        title: String,
        message: String,
        /// Plugin that published the popup, when supplied. Renders as a dim
        /// `from: <source>` footer above the close-hint.
        source: Option<String>,
        scroll: u16,
    },
    /// Transient warning overlay — surfaced for non-fatal validation issues
    /// (unknown provider, no providers configured, etc). ESC / Q closes.
    Warning {
        title: String,
        message: String,
        /// Plugin that published the popup, when supplied. Renders as a dim
        /// `from: <source>` footer above the close-hint. `None` for popups
        /// opened by nefor-chat's own internal paths (login/logout
        /// validation).
        source: Option<String>,
        scroll: u16,
    },
    /// Transient error overlay — surfaced for fatal-shaped events
    /// (auth.status state="error", system messages starting with "Error:"
    /// or "[error]"). ESC / Q closes.
    Error {
        title: String,
        message: String,
        /// Plugin that published the popup, when supplied. Renders as a dim
        /// `from: <source>` footer above the close-hint. `None` for popups
        /// opened by nefor-chat's own internal paths (auth.status="error",
        /// `Error:`-prefixed system messages).
        source: Option<String>,
        scroll: u16,
    },
    /// Slash-command autocomplete. Tracks the filtered list of commands that
    /// match the current input prefix, plus a cursor + scroll for navigation.
    /// Opened automatically when the input starts with `/`; closes when the
    /// user backspaces past the slash or hits ESC.
    //
    // `#[allow(dead_code)]`: the integration `tests/render.rs` build
    // `#[path]`-includes this module but never reaches the variant's
    // constructor in `main.rs`.
    #[allow(dead_code)]
    SlashAutocomplete {
        /// Filtered command list, ordered by the canonical registry order.
        matches: Vec<SlashCommand>,
        /// Index into `matches` of the highlighted row.
        cursor: usize,
        /// First visible row in the popup body.
        scroll: u16,
    },
    /// Transient confirmation toast — appears at the bottom-left above the
    /// input bar, auto-dismisses after `expires_at`. Used today for "copied
    /// N chars" after a mouse selection. No keys close it; the main loop's
    /// 1s tick clears it once `expires_at <= Instant::now()`.
    Toast {
        message: String,
        /// Wall-clock instant when the toast should self-dismiss.
        expires_at: Instant,
    },
    /// Tool permission request — opened by `chat.tool.permission_request`
    /// (emitted by tool-gate). Shows the tool name and pretty-printed args
    /// and waits for `A` (approve) / `D` or ESC (deny). On either key the
    /// chat emits `tool.permission_response { id, decision }`. The `id`
    /// is preserved verbatim from the request so the gate can match.
    ToolPermission {
        /// Correlation id from the gate (= provider's outer tool-call id).
        id: String,
        /// Tool name for the title.
        tool: String,
        /// Human-readable args preview (pretty JSON, possibly truncated).
        args_preview: String,
        /// Plugin that published the request — surfaced as a `from: <source>`
        /// footer when present. The gate currently doesn't include itself
        /// (the chat already infers from the wire), so this is `None` today
        /// but keeps the contract symmetric with Info/Warning/Error.
        source: Option<String>,
    },
}

/// Per-node lifecycle state in a tracked DAG run, driven by lifecycle events
/// from `dag-scheduler` (`dag.run_started`, `dag.node_dispatched`,
/// `dag.node_result`, `dag.run_complete`). Surfaced by the live panel above
/// the statusline so the user can see in-flight nodes per run.
//
// `#[allow(dead_code)]` on every variant: the integration `tests/render.rs`
// build `#[path]`-includes `state.rs` only and never reaches `main.rs`,
// where `Error` / `Skipped` / `Pending` are constructed. Kept on the enum
// so the type stays consistent across both build configurations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum DagNodeStatus {
    /// `dag.run_started` seen but no dispatch yet — node is in the graph but
    /// not yet runnable (or runnable and waiting on the scheduler tick).
    Pending,
    /// `dag.node_dispatched` seen — node is in flight at the reasoner.
    Running,
    /// `dag.node_result` with `output` seen — node finished successfully.
    Done,
    /// `dag.node_result` with `error` seen — node failed.
    Error,
    /// Run completed and the node was marked `{ skipped: true }` in the
    /// results map. Only assignable indirectly via `dag.run_complete`; the
    /// panel itself clears the run on complete so this status is short-lived.
    Skipped,
}

/// One node's live state inside a tracked DAG run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DagNodeState {
    /// Reasoner plugin name the node was dispatched to. Echoed from
    /// `dag.node_dispatched`.
    pub reasoner: String,
    /// Current lifecycle bucket — pending / running / done / error / skipped.
    pub status: DagNodeStatus,
    /// Milliseconds since the chat plugin started (`ChatState::epoch`),
    /// stamped when the node first transitions to `Running`. Used by the
    /// renderer to compute "running 2.3s" elapsed times against the same
    /// epoch.
    pub started_at_ms: u64,
    /// Milliseconds since the chat epoch at which the node reached a terminal
    /// status (`Done` / `Error` / `Skipped`). `None` while still running.
    pub finished_at_ms: Option<u64>,
}

/// Live state for one tracked DAG run. Held in `ChatState::dag_runs`, keyed
/// by `run_id`. Cleared on `dag.run_complete` and `/new`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DagRunUiState {
    /// Run id, echoed from the lifecycle events. Useful as a tie-breaker
    /// inside the renderer when showing a header line.
    pub run_id: String,
    /// Milliseconds since the chat epoch at which `dag.run_started` was
    /// observed. Drives the `?` running-since header indicator if we ever
    /// add one — currently the renderer reads only per-node times.
    //
    // `#[allow(dead_code)]`: stored for future header use; the current panel
    // shows per-node elapsed times and a "M/N nodes" counter.
    #[allow(dead_code)]
    pub started_at_ms: u64,
    /// Total nodes reported by the scheduler at `dag.run_started`. Drives the
    /// `(running M / total N)` header counter.
    pub total_nodes: usize,
    /// Per-node state, keyed by `node_id`. `BTreeMap` so the renderer can
    /// iterate in stable lexicographic order without a separate sort step.
    pub nodes: BTreeMap<String, DagNodeState>,
    /// Set when `dag.run_complete` arrives. The run lingers in the panel for
    /// [`DAG_RUN_LINGER_MS`] so the user gets visual confirmation the run
    /// finished before it disappears, then it's pruned by the per-second
    /// tick. `None` while still in flight.
    pub completed_at_ms: Option<u64>,
}

/// Everything the renderer needs to produce a frame.
#[derive(Debug, Clone)]
pub struct ChatState {
    /// Ordered transcript. Oldest first.
    pub transcript: Vec<TranscriptEntry>,
    /// Current line being composed.
    pub input: InputBuffer,
    /// Rows scrolled up from the bottom of the transcript. 0 → stuck to
    /// newest; larger values show older messages. While the user is
    /// auto-following (offset == 0) new content keeps them at the bottom;
    /// once they scroll up the renderer compensates by bumping `scroll_offset`
    /// when the wrapped-line total grows, so the absolute viewport stays put.
    pub scroll_offset: u32,
    /// Wrapped-line total observed at the end of the previous render. The
    /// renderer compares the freshly-wrapped total against this to decide
    /// how far to bump `scroll_offset` so a user who has scrolled up keeps
    /// their viewport pinned to the same absolute lines while content
    /// streams in. `None` before the first render so we don't treat the
    /// initial transcript as "growth" against an absent baseline.
    pub last_wrapped_total: Option<u32>,
    /// Latest dimensions we've been told about.
    pub dims: Dims,
    /// `false` until we've seen `nefor-tui.ready`. No rendering happens
    /// before this flips.
    pub tui_ready: bool,
    /// True between `chat.input.submit` dispatch and `chat.stream.end`.
    /// Renderer injects a "[thinking...]" row while this is set
    /// *and* no assistant entry has started streaming yet.
    pub pending: bool,
    /// Optional session telemetry — model, cost, token usage, last turn
    /// duration. Populated from `chat.session.stats` events; `stats_seen`
    /// drives whether the statusline shows real values or `—` placeholders
    /// for absent fields.
    pub metadata: SessionMetadata,
    /// Monotonic counter bumped whenever the transcript or `pending` flag
    /// mutates. The renderer reads this to skip an expensive re-wrap +
    /// markdown re-parse when only the input buffer has changed (typing).
    pub transcript_version: u64,
    /// Per-row last-emitted snapshot. The renderer diffs the intended
    /// frame against this and emits `grid.line` only for rows whose
    /// content changed since the last flush. Cleared on resize, on
    /// `nefor-tui.ready`, and any time the grid is otherwise reset.
    pub row_cache: Vec<Option<RowSnapshot>>,
    /// Set on `chat.input.submit`; cleared on the first inbound chat-side
    /// acknowledgment. While `Some` and unacknowledged, the placeholder
    /// renders a live "thinking… Ns" counter so the user can decide
    /// whether to wait or interrupt.
    pub awaiting_response_since: Option<Instant>,
    /// Flips to `true` on the first response-shaped chat event after a
    /// submit. Stays `true` until the next submit.
    pub awaiting_response_acknowledged: bool,
    /// Global toggle for tool-call expansion (Ctrl+O). `false` collapses
    /// every tool entry to a one-line summary; `true` expands all of them
    /// to show full input + output.
    pub tools_expanded_global: bool,
    /// Most recent ESC keypress timestamp. Two ESCs within
    /// `DOUBLE_ESC_WINDOW` escalate from `Action::Interrupt` (cancel the
    /// in-flight chat run) to `Action::InterruptAll` (cancel chat + all
    /// sub-graph runs, drop deferred queue). `None` if no ESC has been
    /// pressed yet, or if the previous ESC was outside the window.
    #[allow(dead_code)]
    // read by main.rs; tests/render.rs #[path]-includes state.rs without main
    pub last_escape_at: Option<Instant>,
    /// Submitted-prompt history, oldest first. Up/Down on an empty input
    /// buffer recall older/newer entries — same convention as a shell.
    pub prompt_history: Vec<String>,
    /// Cursor into `prompt_history` while navigating with Up/Down. `None`
    /// means "past the end" (next Up recalls the latest entry); `Some(i)`
    /// means the buffer is showing `prompt_history[i]`.
    pub history_cursor: Option<usize>,
    /// Provider names seen via `chat.auth.status` events, in arrival order.
    /// Drives slash-command disambiguation and the statusline auth indicators.
    pub providers: Vec<String>,
    /// The provider used for normal prompt submissions and `/model` queries
    /// when no explicit target is given. Set to the first provider that
    /// reports `state == "connected"`; user can override via `/login` etc.
    pub active_provider: Option<String>,
    /// Latest auth state per provider, keyed by name.
    pub auth_status: HashMap<String, AuthStatus>,
    /// Active model per provider, keyed by name. Updated on
    /// `chat.model.set_ack` so `/model` with no arg can show what's
    /// currently selected alongside the catalog.
    //
    // `#[allow(dead_code)]`: the read happens in `main.rs`'s `handle_event`,
    // which the `tests/render.rs` integration test build doesn't reach
    // (it `#[path]`-includes only `state.rs`).
    #[allow(dead_code)]
    pub active_model_per_provider: HashMap<String, String>,
    /// Open modal overlay (help, model picker, …) or `None` when the chat
    /// input has focus. Drives the popup-rendering pass and the input-routing
    /// gate in `handle_key`.
    pub popup: Option<Popup>,
    /// Tool-gate runtime mode mirror. `true` means yolo (no permission
    /// prompts) — surfaced as a high-visibility statusline indicator. Updated
    /// from `tool-gate.mode_changed` events; defaults to `false` (safe).
    pub gate_yolo: bool,
    /// In-flight `/dag-test` `run_id`s. Inserted on dispatch, removed when the
    /// matching `dag.run_complete` arrives. Lets the chat plugin distinguish
    /// "this complete event is mine" from "some other plugin's run".
    //
    // `#[allow(dead_code)]`: production reads/writes this from `main.rs`'s
    // `handle_command`/`handle_dag_run_complete` paths; the `tests/render.rs`
    // integration build `#[path]`-includes only `state.rs` and never reaches
    // those, so the field looks unused from that build's perspective.
    #[allow(dead_code)]
    pub pending_dag_runs: HashSet<String>,
    /// Live DAG runs the chat is observing — one entry per `dag.run_started`
    /// not yet matched by a `dag.run_complete`. Drives the in-flight panel
    /// rendered above the statusline. Keyed by `run_id`; `BTreeMap` so the
    /// renderer iterates in stable order.
    pub dag_runs: BTreeMap<String, DagRunUiState>,
    /// Monotonic instant captured at construction time. The DAG panel stamps
    /// `started_at_ms` / `finished_at_ms` as offsets from this so the
    /// renderer can compute elapsed times without keeping an `Instant` per
    /// node (which would complicate `Eq` and serialization).
    pub epoch: Instant,
    /// User-controlled toggle for the right-side sidebar pane. Defaults to
    /// `true`; flipped by Ctrl-B. Auto-hidden by the renderer when there's
    /// no widget content to show or when the terminal is narrower than
    /// [`SIDEBAR_MIN_TERMINAL_COLS`] regardless of this flag's value, so
    /// `true` here means "show when there's room and content," not "always
    /// reserve columns."
    pub sidebar_visible: bool,
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
            last_wrapped_total: None,
            dims: Dims::fallback(),
            tui_ready: false,
            pending: false,
            metadata: SessionMetadata::default(),
            transcript_version: 0,
            row_cache: Vec::new(),
            awaiting_response_since: None,
            awaiting_response_acknowledged: false,
            tools_expanded_global: false,
            last_escape_at: None,
            prompt_history: Vec::new(),
            history_cursor: None,
            providers: Vec::new(),
            active_provider: None,
            auth_status: HashMap::new(),
            active_model_per_provider: HashMap::new(),
            popup: None,
            gate_yolo: false,
            pending_dag_runs: HashSet::new(),
            dag_runs: BTreeMap::new(),
            epoch: Instant::now(),
            sidebar_visible: true,
        }
    }

    /// Milliseconds since the chat epoch — used by the DAG panel to stamp
    /// `started_at_ms` / `finished_at_ms` and compute elapsed times.
    pub fn now_ms(&self) -> u64 {
        Instant::now()
            .saturating_duration_since(self.epoch)
            .as_millis() as u64
    }

    /// Register a provider name if not already present. No-op when known.
    pub fn register_provider(&mut self, name: &str) {
        if !self.providers.iter().any(|p| p == name) {
            self.providers.push(name.to_owned());
        }
    }

    /// Set the active provider. Registers it first if it hasn't been seen.
    //
    // `#[allow(dead_code)]`: production sets `active_provider` directly via
    // the auth-status promotion rule; this helper exists for tests and for
    // future explicit-set flows (e.g. `/use <provider>`).
    #[allow(dead_code)]
    pub fn set_active_provider(&mut self, name: &str) {
        self.register_provider(name);
        self.active_provider = Some(name.to_owned());
    }

    /// Arm the pending-response state. Called when the user submits an
    /// input — drives the live "thinking… Ns" counter on the placeholder
    /// row until the first response-shaped event arrives.
    pub fn arm_watchdog(&mut self) {
        self.awaiting_response_since = Some(Instant::now());
        self.awaiting_response_acknowledged = false;
    }

    /// Acknowledge the harness — called when any response-shaped chat event
    /// arrives (`chat.stream.delta`, `chat.stream.end`, assistant
    /// `chat.message.append`, `chat.tool.start`). Stops the counter.
    pub fn acknowledge_response(&mut self) {
        if self.awaiting_response_since.is_some() {
            self.awaiting_response_acknowledged = true;
        }
    }

    /// Elapsed seconds since submit, while pending and unacknowledged.
    /// `None` outside that window. Used by the renderer to drive the live
    /// "thinking… Ns" placeholder and by the main-loop tick to decide
    /// whether to re-render this second.
    pub fn pending_seconds_at(&self, now: Instant) -> Option<u64> {
        let since = self.awaiting_response_since?;
        if self.awaiting_response_acknowledged {
            return None;
        }
        Some(now.saturating_duration_since(since).as_secs())
    }

    /// Forget every row snapshot so the next render emits a complete frame.
    /// Called when the grid is reset on the renderer side — resize, ready,
    /// or an explicit clear — so rendered cells in nefor-tui's state cannot
    /// be assumed to match anything we previously emitted.
    pub fn invalidate_row_cache(&mut self) {
        self.row_cache.clear();
    }

    /// Bump the transcript-version counter. Call after any direct mutation
    /// to `transcript` that bypasses the helper methods (`push_entry`,
    /// `append_assistant_delta`, `finalize_assistant`). Required for the
    /// renderer's wrap+markdown cache to correctly invalidate.
    pub fn bump_transcript_version(&mut self) {
        self.transcript_version = self.transcript_version.wrapping_add(1);
    }

    /// Open the help popup. Closes any currently-open popup first so chained
    /// `/help` calls reset cleanly.
    pub fn open_popup_help(&mut self) {
        self.popup = Some(Popup::Help { scroll: 0 });
        self.invalidate_row_cache();
        self.bump_transcript_version();
    }

    /// Open a transient info popup. Closes any currently-open popup first.
    /// `source`, when `Some`, names the plugin that published the popup and
    /// renders as a dim `from: <source>` footer line. Surfaced by
    /// `chat.popup { level = "info" }` events on the bus.
    pub fn open_popup_info(
        &mut self,
        title: impl Into<String>,
        message: impl Into<String>,
        source: Option<String>,
    ) {
        self.popup = Some(Popup::Info {
            title: title.into(),
            message: message.into(),
            source,
            scroll: 0,
        });
        self.invalidate_row_cache();
        self.bump_transcript_version();
    }

    /// Open a transient warning popup. Closes any currently-open popup first.
    /// `source`, when `Some`, names the plugin that published the popup and
    /// renders as a dim `from: <source>` footer line. Internal callers
    /// (login/logout validation) pass `None`; bus-routed `chat.popup` events
    /// pass through whatever the publisher set.
    pub fn open_popup_warning(
        &mut self,
        title: impl Into<String>,
        message: impl Into<String>,
        source: Option<String>,
    ) {
        self.popup = Some(Popup::Warning {
            title: title.into(),
            message: message.into(),
            source,
            scroll: 0,
        });
        self.invalidate_row_cache();
        self.bump_transcript_version();
    }

    /// Open a transient error popup. Closes any currently-open popup first.
    /// `source`, when `Some`, names the plugin that published the popup and
    /// renders as a dim `from: <source>` footer line. Internal callers
    /// (auth.status="error", `Error:`-prefixed system messages) pass `None`;
    /// bus-routed `chat.popup` events pass through whatever the publisher set.
    pub fn open_popup_error(
        &mut self,
        title: impl Into<String>,
        message: impl Into<String>,
        source: Option<String>,
    ) {
        self.popup = Some(Popup::Error {
            title: title.into(),
            message: message.into(),
            source,
            scroll: 0,
        });
        self.invalidate_row_cache();
        self.bump_transcript_version();
    }

    /// Open the model picker popup. `awaiting` should be the set of provider
    /// names whose `chat.models.listed` response is still pending; the picker
    /// renders a footer until that set drains.
    pub fn open_popup_model_picker(&mut self, awaiting: HashSet<String>) {
        self.popup = Some(Popup::ModelPicker {
            all_models: Vec::new(),
            query: String::new(),
            cursor: 0,
            awaiting,
            scroll: 0,
        });
        self.invalidate_row_cache();
        self.bump_transcript_version();
    }

    // `#[allow(dead_code)]`: production calls these from `main.rs`'s slash
    // refresh path; the `tests/render.rs` integration build `#[path]`-includes
    // `state.rs` but never reaches the caller.
    #[allow(dead_code)]
    /// Open or update the slash-autocomplete popup with the given matches.
    /// If a SlashAutocomplete is already open the cursor/scroll are clamped
    /// against the new match list; otherwise a fresh popup is created. Does
    /// NOT replace any other popup variant — the caller is expected to gate
    /// on the popup state before invoking.
    pub fn open_or_update_popup_slash_autocomplete(&mut self, matches: Vec<SlashCommand>) {
        match self.popup.as_mut() {
            Some(Popup::SlashAutocomplete {
                matches: m,
                cursor,
                scroll,
            }) => {
                *m = matches;
                if m.is_empty() {
                    *cursor = 0;
                    *scroll = 0;
                } else if *cursor >= m.len() {
                    *cursor = m.len() - 1;
                }
                if (*scroll as usize) >= m.len().max(1) {
                    *scroll = 0;
                }
            }
            _ => {
                self.popup = Some(Popup::SlashAutocomplete {
                    matches,
                    cursor: 0,
                    scroll: 0,
                });
            }
        }
        self.invalidate_row_cache();
        self.bump_transcript_version();
    }

    #[allow(dead_code)]
    /// Close the slash-autocomplete popup if it's currently open. No-op for
    /// any other variant — keeps callers from accidentally stomping unrelated
    /// popups when typing in the input buffer.
    pub fn close_popup_slash_autocomplete(&mut self) {
        if matches!(self.popup, Some(Popup::SlashAutocomplete { .. })) {
            self.popup = None;
            self.invalidate_row_cache();
            self.bump_transcript_version();
        }
    }

    /// Close any open popup. No-op when none is open.
    //
    // `#[allow(dead_code)]`: production calls this from `main.rs`'s popup
    // key handler; the `tests/render.rs` integration build `#[path]`-includes
    // `state.rs` but never reaches the caller.
    #[allow(dead_code)]
    pub fn close_popup(&mut self) {
        if self.popup.is_some() {
            self.popup = None;
            self.invalidate_row_cache();
            self.bump_transcript_version();
        }
    }

    /// Open a transient toast popup. The toast self-dismisses after
    /// `duration` elapses; the main loop's per-second tick clears it.
    /// Closes any currently-open popup first so a slow-stream toast
    /// can't cover an already-open warning indefinitely.
    //
    // `#[allow(dead_code)]`: production calls this from `main.rs`'s mouse
    // handler; the `tests/render.rs` integration build never reaches that
    // path.
    #[allow(dead_code)]
    pub fn open_popup_toast(&mut self, message: impl Into<String>, duration: Duration) {
        self.popup = Some(Popup::Toast {
            message: message.into(),
            expires_at: Instant::now() + duration,
        });
        self.invalidate_row_cache();
        self.bump_transcript_version();
    }

    /// Open a tool-permission popup. Closes any currently-open popup first.
    /// `args_preview` is the pre-formatted body the user sees — the caller
    /// is expected to pretty-print and truncate before passing in so the
    /// state layer stays format-free.
    //
    // `#[allow(dead_code)]`: production calls this from `main.rs`'s
    // `chat.tool.permission_request` handler; the `tests/render.rs`
    // integration build `#[path]`-includes this module without reaching
    // that path.
    #[allow(dead_code)]
    pub fn open_popup_tool_permission(
        &mut self,
        id: impl Into<String>,
        tool: impl Into<String>,
        args_preview: impl Into<String>,
        source: Option<String>,
    ) {
        self.popup = Some(Popup::ToolPermission {
            id: id.into(),
            tool: tool.into(),
            args_preview: args_preview.into(),
            source,
        });
        self.invalidate_row_cache();
        self.bump_transcript_version();
    }

    /// `true` if the open popup is a `Toast` whose `expires_at` is in the
    /// past relative to `now`. The main loop polls this on every pending
    /// tick to drive auto-dismiss without spinning a dedicated timer.
    //
    // `#[allow(dead_code)]`: production calls this from `main.rs`'s tick
    // handler; integration tests `#[path]`-include this module without
    // reaching that path.
    #[allow(dead_code)]
    pub fn toast_expired_at(&self, now: Instant) -> bool {
        matches!(&self.popup, Some(Popup::Toast { expires_at, .. }) if *expires_at <= now)
    }

    /// Append `models` from `provider` into the active ModelPicker popup, if
    /// any, and remove `provider` from the awaiting set. Sorts the aggregated
    /// list by `(provider, model)` for stable display. Bumps the version so
    /// the renderer re-emits popup rows. No-op when no model picker is open.
    pub fn popup_models_listed(&mut self, provider: &str, models: &[String]) {
        let Some(Popup::ModelPicker {
            all_models,
            cursor,
            awaiting,
            query,
            scroll,
        }) = self.popup.as_mut()
        else {
            return;
        };
        for m in models {
            all_models.push((provider.to_owned(), m.clone()));
        }
        all_models.sort();
        all_models.dedup();
        awaiting.remove(provider);
        // Re-clamp cursor against the *filtered* list (filter may exclude new
        // arrivals). Computing the filter inline avoids exposing the helper
        // outside this module.
        let q = query.to_lowercase();
        let visible_count = all_models
            .iter()
            .filter(|(p, m)| {
                if q.is_empty() {
                    true
                } else {
                    format!("{p} {m}").to_lowercase().contains(&q)
                }
            })
            .count();
        if visible_count == 0 {
            *cursor = 0;
            *scroll = 0;
        } else if *cursor >= visible_count {
            *cursor = visible_count - 1;
        }
        if (*scroll as usize) >= visible_count.max(1) {
            *scroll = 0;
        }
        self.invalidate_row_cache();
        self.bump_transcript_version();
    }

    // `#[allow(dead_code)]` here — as for `clear`/`clamp_scroll` below —
    // is because the integration tests in `tests/render.rs` `#[path]`-
    // include this module but never reach `main.rs`, which is where these
    // helpers are actually called.

    /// Mark a turn as in-flight. Called when the plugin ships
    /// `chat.input.submit`.
    #[allow(dead_code)]
    pub fn begin_turn(&mut self) {
        self.pending = true;
        self.bump_transcript_version();
    }

    /// Mark the in-flight turn as finished. Called on `chat.stream.end`.
    /// Disarms the pending-response state so the placeholder counter
    /// stops ticking against the previous prompt.
    #[allow(dead_code)]
    pub fn end_turn(&mut self) {
        self.pending = false;
        self.awaiting_response_since = None;
        self.awaiting_response_acknowledged = false;
        self.bump_transcript_version();
    }

    /// Append a finished entry (user prompt, system line).
    pub fn push_entry(&mut self, role: Role, text: String) {
        self.transcript.push(TranscriptEntry::new(role, text));
        self.bump_transcript_version();
    }

    /// Append a Tool entry on `chat.tool.start`. `id` is the harness's
    /// `tool_use_id` (used to match the later `chat.tool.end`); pass an
    /// empty string if the producer didn't supply one. `input_json` is the
    /// pretty-printed JSON of the input map — kept as text so the renderer
    /// doesn't have to re-serialise on every frame.
    pub fn push_tool_start(&mut self, id: String, name: String, input_json: String) {
        self.transcript.push(TranscriptEntry {
            role: Role::Tool,
            text: String::new(),
            streaming: false,
            model: None,
            duration_ms: None,
            tool: Some(ToolPayload {
                id,
                name,
                input_json,
                output: None,
                error: false,
            }),
            reasoning: None,
        });
        self.bump_transcript_version();
    }

    /// Find the most recent Tool entry matching `id` (or the most recent one
    /// at all when `id` is empty) and stamp its output. Returns `true` iff a
    /// matching entry was found and updated.
    pub fn attach_tool_end(&mut self, id: &str, output: String, error: bool) -> bool {
        for entry in self.transcript.iter_mut().rev() {
            if entry.role != Role::Tool {
                continue;
            }
            let Some(payload) = entry.tool.as_mut() else {
                continue;
            };
            let matches = if id.is_empty() {
                payload.output.is_none()
            } else {
                payload.id == id
            };
            if matches {
                payload.output = Some(output);
                payload.error = error;
                self.bump_transcript_version();
                return true;
            }
        }
        false
    }

    /// Flip the global tool-expansion toggle (Ctrl+O). Bumps the transcript
    /// version so the wrap cache invalidates.
    pub fn toggle_tools_expanded(&mut self) {
        self.tools_expanded_global = !self.tools_expanded_global;
        self.bump_transcript_version();
    }

    /// Append a chunk of streaming reasoning text to the in-flight
    /// assistant entry. Creates a fresh streaming assistant entry (with
    /// `text` empty) if none is open — reasoning often arrives BEFORE
    /// the first content delta, so this is the typical first observation
    /// of a turn for thinking-trace models. Subsequent
    /// `append_assistant_delta` calls then attach content to the same
    /// entry.
    pub fn append_assistant_reasoning_delta(&mut self, chunk: &str) {
        if let Some(last) = self.transcript.last_mut() {
            if last.role == Role::Assistant && last.streaming {
                let r = last.reasoning.get_or_insert_with(|| ReasoningPayload {
                    text: String::new(),
                    streaming: true,
                    duration_ms: None,
                });
                r.text.push_str(chunk);
                r.streaming = true;
                self.bump_transcript_version();
                return;
            }
        }
        self.transcript.push(TranscriptEntry {
            role: Role::Assistant,
            text: String::new(),
            streaming: true,
            model: None,
            duration_ms: None,
            tool: None,
            reasoning: Some(ReasoningPayload {
                text: chunk.to_owned(),
                streaming: true,
                duration_ms: None,
            }),
        });
        self.bump_transcript_version();
    }

    /// Close the in-flight assistant entry's reasoning channel. Called
    /// from `chat.stream.reasoning_end`. If `final_text` is non-empty
    /// it overrides the accumulated streaming text (the provider's
    /// authoritative full trace); empty replaces nothing. `duration_ms`
    /// stamps the collapsed-row label. Triggers the visual transition
    /// from live-preview to collapsed `▶ reasoning (Ns)` row.
    ///
    /// Idempotent: if there's no in-flight assistant entry or no
    /// reasoning has been observed, this is a no-op (a stray
    /// `reasoning_end` from a non-thinking model just gets dropped).
    pub fn finalize_assistant_reasoning(
        &mut self,
        final_text: Option<String>,
        duration_ms: Option<u64>,
    ) {
        let Some(last) = self.transcript.last_mut() else {
            return;
        };
        if last.role != Role::Assistant {
            return;
        }
        let Some(r) = last.reasoning.as_mut() else {
            return;
        };
        if let Some(t) = final_text {
            if !t.is_empty() {
                r.text = t;
            }
        }
        r.streaming = false;
        if duration_ms.is_some() {
            r.duration_ms = duration_ms;
        }
        self.bump_transcript_version();
    }

    /// Append a chunk of streaming assistant text. Creates a new streaming
    /// entry if none is currently open; otherwise appends to the last
    /// streaming assistant entry.
    pub fn append_assistant_delta(&mut self, chunk: &str) {
        if let Some(last) = self.transcript.last_mut() {
            if last.role == Role::Assistant && last.streaming {
                last.text.push_str(chunk);
                self.bump_transcript_version();
                return;
            }
        }
        self.transcript.push(TranscriptEntry {
            role: Role::Assistant,
            text: chunk.to_owned(),
            streaming: true,
            model: None,
            duration_ms: None,
            tool: None,
            reasoning: None,
        });
        self.bump_transcript_version();
    }

    /// Finalize the open streaming assistant entry.
    ///
    /// If `final_text` is a non-empty `Some`, replace the streaming entry's
    /// text with the authoritative value (e.g. the harness's
    /// `chat.stream.end.text` after reconciliation). An empty `Some("")` is
    /// treated as `None` against an in-progress streaming entry: some
    /// producers (notably mock-plugin via claude's terminal `result.result`)
    /// emit `text=""` even when deltas carried the actual reply, and an
    /// authoritative-empty override would silently wipe the body. If no
    /// streaming entry is open, a non-empty `final_text` is appended as a
    /// new assistant entry; an empty `final_text` is dropped — `chat.stream.end`
    /// shouldn't manifest a blank assistant turn out of nothing.
    pub fn finalize_assistant(&mut self, final_text: Option<String>) {
        if let Some(last) = self.transcript.last_mut() {
            if last.role == Role::Assistant && last.streaming {
                if let Some(t) = final_text {
                    if !t.is_empty() {
                        last.text = t;
                    }
                }
                last.streaming = false;
                // Defensive close: if reasoning was still streaming
                // (provider emitted stream.end without a paired
                // reasoning_end), flip it to collapsed so the entry
                // doesn't render as a frozen live-preview.
                if let Some(r) = last.reasoning.as_mut() {
                    r.streaming = false;
                }
                self.bump_transcript_version();
                return;
            }
        }
        if let Some(t) = final_text {
            if t.is_empty() {
                return;
            }
            self.transcript.push(TranscriptEntry {
                role: Role::Assistant,
                text: t,
                streaming: false,
                model: None,
                duration_ms: None,
                tool: None,
                reasoning: None,
            });
            self.bump_transcript_version();
        }
    }

    /// Stamp `model` and `duration_ms` on the most recent assistant entry,
    /// if one exists. Called from `chat.stream.end` after `finalize_assistant`
    /// so the per-turn footer can show "▣ <model> · <human_duration>".
    /// Bumps `transcript_version` to invalidate the wrap cache.
    pub fn stamp_last_assistant(&mut self, model: Option<String>, duration_ms: Option<u64>) {
        if model.is_none() && duration_ms.is_none() {
            return;
        }
        for entry in self.transcript.iter_mut().rev() {
            if entry.role == Role::Assistant {
                if model.is_some() {
                    entry.model = model;
                }
                if duration_ms.is_some() {
                    entry.duration_ms = duration_ms;
                }
                self.bump_transcript_version();
                return;
            }
        }
    }

    /// Desired height of the live DAG widget, in rows. Each tracked run
    /// renders as 1 header row + N node rows; total height is capped at
    /// [`DAG_PANEL_MAX_ROWS`] with a `… +K more` overflow row when needed.
    /// Returns 0 when there are no tracked runs.
    //
    // No longer drives the chat-pane row budget (the DAG widget moved into
    // the right sidebar, which steals columns rather than rows). Kept as a
    // public helper because it's a reasonable size estimate to surface
    // through the introspection surface — e.g. tests and future widgets.
    #[allow(dead_code)]
    pub fn dag_panel_rows(&self) -> u32 {
        if self.dag_runs.is_empty() {
            return 0;
        }
        // Sum of header + node rows across every tracked run.
        let raw_rows: usize = self.dag_runs.values().map(|r| 1 + r.nodes.len()).sum();
        let max = DAG_PANEL_MAX_ROWS as usize;
        let height = if raw_rows > max {
            // Reserve one row for the "… +K more" overflow line.
            max
        } else {
            raw_rows
        };
        height as u32
    }

    /// Number of visible transcript content rows, computed with the same
    /// math `render.rs` uses. Input height is derived from the logical-line
    /// count of the input buffer (`\n` count + 1) rather than from the
    /// word-wrapped width — close enough for sizing PageUp / PageDown steps
    /// to a viewport without re-running the full input wrapper.
    ///
    /// The DAG panel used to steal rows from this budget; it now lives in
    /// the right sidebar and consumes columns instead, so the row math
    /// simplifies to `rows − layout chrome`.
    pub fn transcript_rows(&self) -> u32 {
        let rows = self.dims.rows.max(2);
        let status_height: u32 = if rows >= 3 { 1 } else { 0 };
        let vpad_budget = rows.saturating_sub(2 + status_height);
        let vpad_top: u32 = if vpad_budget >= 1 { VPAD } else { 0 };
        let vpad_bottom: u32 = if vpad_budget >= 2 && status_height > 0 {
            VPAD
        } else {
            0
        };
        let gutter_floor = vpad_top + 1 + 1 + vpad_bottom + status_height + 1;
        let vpad_input_top: u32 = if rows >= gutter_floor { 1 } else { 0 };
        let bar_floor = vpad_top + vpad_input_top + 2 + 1 + vpad_bottom + status_height + 1;
        let input_bars: u32 = if rows >= bar_floor { 2 } else { 0 };
        let max_input_rows = rows
            .saturating_sub(
                vpad_top + vpad_input_top + input_bars + 1 + vpad_bottom + status_height,
            )
            .clamp(1, MAX_INPUT_ROWS);
        let input_logical_lines = self.input.lines().len() as u32;
        let input_height = input_logical_lines.clamp(1, max_input_rows);
        rows.saturating_sub(vpad_top)
            .saturating_sub(vpad_input_top)
            .saturating_sub(input_bars)
            .saturating_sub(input_height)
            .saturating_sub(vpad_bottom)
            .saturating_sub(status_height)
    }

    /// Whether the sidebar has any content to show right now. Currently
    /// unused at runtime (the sidebar pane is persistent — see
    /// [`ChatState::sidebar_width`]) but kept as a tested helper for future
    /// widgets that might want to gate themselves on activity.
    #[allow(dead_code)]
    pub fn has_sidebar_widgets(&self) -> bool {
        !self.dag_runs.is_empty()
    }

    /// Whether the chat has any DAG activity worth re-rendering for: either
    /// in-flight nodes whose elapsed counter advances each second, or
    /// finished runs lingering in the panel for the visual-confirmation
    /// window (see [`DAG_RUN_LINGER_MS`]).
    //
    // `#[allow(dead_code)]`: integration-test build doesn't see the call
    // site in `main.rs`'s tick handler.
    #[allow(dead_code)]
    pub fn has_dag_activity(&self) -> bool {
        !self.dag_runs.is_empty()
    }

    /// Drop runs whose linger window expired. Returns `true` if anything was
    /// pruned (caller bumps the transcript version + invalidates the row
    /// cache so the next render redraws the now-shorter panel).
    //
    // `#[allow(dead_code)]`: integration-test build doesn't see the call
    // site in `main.rs`'s tick handler.
    #[allow(dead_code)]
    pub fn prune_finished_dag_runs(&mut self, now_ms: u64) -> bool {
        let before = self.dag_runs.len();
        self.dag_runs.retain(|_, run| match run.completed_at_ms {
            Some(c) => now_ms.saturating_sub(c) < DAG_RUN_LINGER_MS,
            None => true,
        });
        self.dag_runs.len() != before
    }

    /// Compute the right sidebar's column reservation for the current state.
    /// Returns 0 (sidebar collapses) when:
    ///   - the user toggled it off (`sidebar_visible == false`), OR
    ///   - the terminal is narrower than [`SIDEBAR_MIN_TERMINAL_COLS`] (auto
    ///     hide on narrow splits / laptops so the chat pane keeps its room).
    ///
    /// Otherwise the width is `clamp(cols * 25 / 100, MIN, MAX)` — roughly
    /// a quarter of the terminal, bounded so it neither shrinks below
    /// readability nor crowds the chat. The sidebar stays reserved even when
    /// no widgets are active so the layout doesn't jump as runs come and go;
    /// an empty pane shows a small placeholder row.
    pub fn sidebar_width(&self) -> u32 {
        if !self.sidebar_visible {
            return 0;
        }
        let cols = self.dims.cols;
        if cols < SIDEBAR_MIN_TERMINAL_COLS {
            return 0;
        }
        let w = ((cols as usize) * 25 / 100) as u32;
        w.clamp(SIDEBAR_MIN_COLS, SIDEBAR_MAX_COLS)
    }

    /// Toggle the sidebar visibility flag. Bound to Ctrl-B in the keyboard
    /// handler. Bumps the row cache so the renderer redraws the (now wider /
    /// narrower) chat pane on the next frame.
    //
    // `#[allow(dead_code)]`: caller is in `main.rs::handle_key`, which the
    // integration `tests/render.rs` build doesn't reach (it `#[path]`-includes
    // only state/render/sidebar/wrap), so the integration build flags it as
    // dead code. The unit-test build sees the call site.
    #[allow(dead_code)]
    pub fn toggle_sidebar(&mut self) {
        self.sidebar_visible = !self.sidebar_visible;
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

    /// Record `text` as a submitted prompt. Drops adjacent duplicates so a
    /// rapid double-submit doesn't waste a slot. Caps the buffer at
    /// [`PROMPT_HISTORY_CAP`] (FIFO eviction). Resets the navigation cursor
    /// so the next Up recalls the just-submitted entry.
    pub fn push_history(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        if self.prompt_history.last().map(String::as_str) == Some(text.as_str()) {
            self.history_cursor = None;
            return;
        }
        self.prompt_history.push(text);
        if self.prompt_history.len() > PROMPT_HISTORY_CAP {
            let overflow = self.prompt_history.len() - PROMPT_HISTORY_CAP;
            self.prompt_history.drain(0..overflow);
        }
        self.history_cursor = None;
    }

    /// Move toward older history. Returns `Some(text)` to install in the
    /// input buffer when navigation actually moved; `None` when there's
    /// nothing older to walk to (or the history is empty).
    pub fn history_up(&mut self) -> Option<String> {
        if self.prompt_history.is_empty() {
            return None;
        }
        let next = match self.history_cursor {
            None => self.prompt_history.len() - 1,
            Some(0) => return None,
            Some(i) => i - 1,
        };
        self.history_cursor = Some(next);
        Some(self.prompt_history[next].clone())
    }

    /// Move toward newer history. Returns the text to install (or empty
    /// string when stepping past the newest entry — that's the "back to a
    /// fresh input" state). Returns `None` when no navigation has happened
    /// yet (cursor is already past the end).
    pub fn history_down(&mut self) -> Option<String> {
        let i = self.history_cursor?;
        if i + 1 >= self.prompt_history.len() {
            self.history_cursor = None;
            Some(String::new())
        } else {
            self.history_cursor = Some(i + 1);
            Some(self.prompt_history[i + 1].clone())
        }
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
    fn paste_with_newlines_preserves_them() {
        let mut b = InputBuffer::default();
        b.insert_str("a\nb\rc");
        // \n preserved, \r normalised to \n.
        assert_eq!(b.as_string(), "a\nb\nc");
    }

    #[test]
    fn lines_splits_on_newline() {
        let mut b = InputBuffer::default();
        b.insert_str("a\nbc\nd");
        assert_eq!(b.lines(), vec!["a", "bc", "d"]);
    }

    #[test]
    fn lines_empty_buffer_yields_one_empty_line() {
        let b = InputBuffer::default();
        assert_eq!(b.lines(), vec![String::new()]);
    }

    #[test]
    fn is_multiline_detects_newline() {
        let mut b = InputBuffer::default();
        b.insert_str("hello");
        assert!(!b.is_multiline());
        b.insert_str("\nworld");
        assert!(b.is_multiline());
    }

    #[test]
    fn cursor_row_and_col_within_row() {
        let mut b = InputBuffer::default();
        b.insert_str("ab\ncde\nf");
        // cursor at end → row 2, col 1
        assert_eq!(b.cursor_row(), 2);
        assert_eq!(b.cursor_col_within_row(), 1);
        b.cursor_home();
        assert_eq!(b.cursor_row(), 0);
        assert_eq!(b.cursor_col_within_row(), 0);
    }

    #[test]
    fn move_cursor_up_in_buffer_preserves_column() {
        let mut b = InputBuffer::default();
        b.insert_str("hello\nworld");
        // cursor at end of "world" (row 1, col 5)
        assert_eq!(b.cursor_row(), 1);
        assert_eq!(b.cursor_col_within_row(), 5);
        let moved = b.move_cursor_up_in_buffer();
        assert!(moved);
        assert_eq!(b.cursor_row(), 0);
        assert_eq!(b.cursor_col_within_row(), 5);
    }

    #[test]
    fn move_cursor_up_at_top_returns_false() {
        let mut b = InputBuffer::default();
        b.insert_str("hello\nworld");
        b.cursor_home();
        let moved = b.move_cursor_up_in_buffer();
        assert!(!moved);
        assert_eq!(b.cursor, 0);
    }

    #[test]
    fn move_cursor_down_in_buffer_clamps_column() {
        let mut b = InputBuffer::default();
        b.insert_str("verylong\nshort");
        b.cursor_home();
        // walk cursor to col 6 of row 0
        for _ in 0..6 {
            b.cursor_right();
        }
        let moved = b.move_cursor_down_in_buffer();
        assert!(moved);
        assert_eq!(b.cursor_row(), 1);
        // "short" is 5 chars; col clamps to 5.
        assert_eq!(b.cursor_col_within_row(), 5);
    }

    #[test]
    fn move_cursor_down_at_bottom_returns_false() {
        let mut b = InputBuffer::default();
        b.insert_str("a\nb");
        // cursor at end of "b" → bottom row.
        let moved = b.move_cursor_down_in_buffer();
        assert!(!moved);
    }

    fn buf_with_cursor(text: &str, cursor: usize) -> InputBuffer {
        let mut b = InputBuffer::default();
        b.insert_str(text);
        b.cursor = cursor;
        b
    }

    #[test]
    fn delete_word_back_at_end_drops_last_word() {
        let mut b = buf_with_cursor("hello world", 11);
        b.delete_word_back();
        assert_eq!(b.as_string(), "hello ");
        assert_eq!(b.cursor(), 6);
    }

    #[test]
    fn delete_word_back_twice_clears_buffer() {
        let mut b = buf_with_cursor("hello world", 11);
        b.delete_word_back();
        b.delete_word_back();
        assert_eq!(b.as_string(), "");
        assert_eq!(b.cursor(), 0);
    }

    #[test]
    fn delete_word_back_on_empty_buffer_is_noop() {
        let mut b = InputBuffer::default();
        b.delete_word_back();
        assert_eq!(b.as_string(), "");
        assert_eq!(b.cursor(), 0);
    }

    #[test]
    fn delete_word_back_at_start_is_noop() {
        let mut b = buf_with_cursor("hello", 0);
        b.delete_word_back();
        assert_eq!(b.as_string(), "hello");
        assert_eq!(b.cursor(), 0);
    }

    #[test]
    fn delete_word_back_preserves_multibyte_prefix() {
        // "café world|" → delete back should leave "café " intact and not
        // corrupt the é.
        let mut b = buf_with_cursor("café world", 10);
        b.delete_word_back();
        assert_eq!(b.as_string(), "café ");
        assert_eq!(b.cursor(), 5);
    }

    #[test]
    fn delete_word_forward_from_start_drops_first_word() {
        let mut b = buf_with_cursor("hello world", 0);
        b.delete_word_forward();
        assert_eq!(b.as_string(), " world");
        assert_eq!(b.cursor(), 0);
    }

    #[test]
    fn delete_word_forward_at_end_is_noop() {
        let mut b = buf_with_cursor("hello", 5);
        b.delete_word_forward();
        assert_eq!(b.as_string(), "hello");
        assert_eq!(b.cursor(), 5);
    }

    #[test]
    fn cursor_word_forward_lands_after_word() {
        // "|hello world" → "hello| world" — macOS Option+Right convention.
        let b = buf_with_cursor("hello world", 0);
        assert_eq!(b.cursor_word_forward(), 5);
    }

    #[test]
    fn cursor_word_forward_skips_leading_whitespace() {
        let b = buf_with_cursor("   hi", 0);
        assert_eq!(b.cursor_word_forward(), 5);
    }

    #[test]
    fn cursor_word_back_lands_at_start_of_prev_word() {
        let b = buf_with_cursor("hello world", 11);
        assert_eq!(b.cursor_word_back(), 6);
    }

    #[test]
    fn cursor_word_back_skips_trailing_whitespace() {
        let b = buf_with_cursor("hi   ", 5);
        assert_eq!(b.cursor_word_back(), 0);
    }

    #[test]
    fn delete_to_start_clears_prefix() {
        let mut b = buf_with_cursor("hello world", 6);
        b.delete_to_start();
        assert_eq!(b.as_string(), "world");
        assert_eq!(b.cursor(), 0);
    }

    #[test]
    fn delete_to_start_at_zero_is_noop() {
        let mut b = buf_with_cursor("hello", 0);
        b.delete_to_start();
        assert_eq!(b.as_string(), "hello");
    }

    #[test]
    fn delete_to_end_clears_suffix() {
        let mut b = buf_with_cursor("hello world", 5);
        b.delete_to_end();
        assert_eq!(b.as_string(), "hello");
        assert_eq!(b.cursor(), 5);
    }

    #[test]
    fn delete_to_end_at_end_is_noop() {
        let mut b = buf_with_cursor("hello", 5);
        b.delete_to_end();
        assert_eq!(b.as_string(), "hello");
    }

    #[test]
    fn move_word_forward_advances_cursor() {
        let mut b = buf_with_cursor("hello world", 0);
        b.move_word_forward();
        assert_eq!(b.cursor(), 5);
        b.move_word_forward();
        assert_eq!(b.cursor(), 11);
    }

    #[test]
    fn move_word_back_retreats_cursor() {
        let mut b = buf_with_cursor("hello world", 11);
        b.move_word_back();
        assert_eq!(b.cursor(), 6);
        b.move_word_back();
        assert_eq!(b.cursor(), 0);
    }

    #[test]
    fn delete_word_forward_on_empty_is_noop() {
        let mut b = InputBuffer::default();
        b.delete_word_forward();
        assert_eq!(b.as_string(), "");
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
    fn finalize_assistant_empty_authoritative_text_keeps_deltas() {
        // Repro for the disappearing-body bug: mock-plugin's `result.result`
        // can be `""` for some claude responses (e.g. table-only outputs);
        // the adapter forwards that as `chat.stream.end.text=""`. An empty
        // authoritative override would wipe the accumulated deltas, leaving
        // a finalized entry with no body — which still gets the per-turn
        // footer stamped. Treat `Some("")` like `None` when deltas exist.
        let mut s = ChatState::new();
        s.append_assistant_delta("hello ");
        s.append_assistant_delta("world");
        s.finalize_assistant(Some(String::new()));
        assert_eq!(s.transcript[0].text, "hello world");
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

    #[test]
    fn auto_follow_default_pins_to_bottom_on_new_entry() {
        // At scroll_offset == 0 the user is auto-following; appending a new
        // entry must leave the offset at 0 so the bottom-anchored viewport
        // math keeps showing the newest content.
        let mut s = ChatState::new();
        assert_eq!(s.scroll_offset, 0);
        s.push_entry(Role::User, "first".into());
        assert_eq!(s.scroll_offset, 0);
        s.append_assistant_delta("hello ");
        s.append_assistant_delta("world");
        assert_eq!(s.scroll_offset, 0);
        s.finalize_assistant(None);
        assert_eq!(s.scroll_offset, 0);
    }

    #[test]
    fn auto_follow_disabled_keeps_scroll_top_when_new_entry_appended() {
        // Once the user has scrolled up, transcript-mutation paths must not
        // snap them back to the bottom. The render layer compensates for
        // content growth so the absolute viewport stays put; state-level
        // mutators just leave scroll_offset alone.
        let mut s = ChatState::new();
        for i in 0..5 {
            s.push_entry(Role::User, format!("msg {i}"));
        }
        s.scroll_up(3);
        assert_eq!(s.scroll_offset, 3);
        s.push_entry(Role::User, "interrupt".into());
        assert_eq!(s.scroll_offset, 3, "push_entry must not reset scroll");
        s.append_assistant_delta("streaming chunk");
        assert_eq!(s.scroll_offset, 3, "delta must not reset scroll");
        s.append_assistant_delta(" more");
        assert_eq!(s.scroll_offset, 3, "subsequent delta must not reset scroll");
        s.finalize_assistant(None);
        assert_eq!(s.scroll_offset, 3, "finalize must not reset scroll");
    }

    #[test]
    fn re_enable_auto_follow_via_scroll_down_to_bottom() {
        // PageDown / wheel-down is the user's signal to re-engage
        // auto-follow: once scroll_offset reaches 0 again, subsequent
        // appends keep them pinned to the bottom.
        let mut s = ChatState::new();
        s.scroll_up(7);
        assert_eq!(s.scroll_offset, 7);
        s.scroll_down(7);
        assert_eq!(s.scroll_offset, 0);
        s.push_entry(Role::User, "fresh".into());
        assert_eq!(s.scroll_offset, 0);
    }

    #[test]
    fn pending_seconds_at_returns_elapsed_while_unacknowledged() {
        let mut s = ChatState::new();
        assert_eq!(
            s.pending_seconds_at(Instant::now()),
            None,
            "not armed: None"
        );

        let t0 = Instant::now();
        s.awaiting_response_since = Some(t0 - Duration::from_secs(7));
        s.awaiting_response_acknowledged = false;
        assert_eq!(s.pending_seconds_at(t0), Some(7));

        s.acknowledge_response();
        assert_eq!(s.pending_seconds_at(t0), None, "ack stops the counter");
    }

    #[test]
    fn arm_watchdog_arms_pending_state() {
        let mut s = ChatState::new();
        s.arm_watchdog();
        assert!(s.awaiting_response_since.is_some());
        assert!(!s.awaiting_response_acknowledged);
    }

    #[test]
    fn stamp_last_assistant_sets_metadata_and_bumps_version() {
        let mut s = ChatState::new();
        s.append_assistant_delta("hi");
        s.finalize_assistant(None);
        let v0 = s.transcript_version;
        s.stamp_last_assistant(Some("claude-sonnet-4-6".into()), Some(1500));
        let last = s.transcript.last().expect("entry");
        assert_eq!(last.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(last.duration_ms, Some(1500));
        assert_ne!(s.transcript_version, v0, "version must bump");
    }

    #[test]
    fn stamp_last_assistant_skips_user_entries() {
        let mut s = ChatState::new();
        s.push_entry(Role::User, "hello".into());
        let v0 = s.transcript_version;
        s.stamp_last_assistant(Some("model".into()), Some(100));
        // No assistant entry to stamp — version unchanged, user untouched.
        assert_eq!(s.transcript_version, v0);
        assert!(s.transcript[0].model.is_none());
    }

    #[test]
    fn stamp_last_assistant_with_no_args_is_noop() {
        let mut s = ChatState::new();
        s.append_assistant_delta("hi");
        s.finalize_assistant(None);
        let v0 = s.transcript_version;
        s.stamp_last_assistant(None, None);
        assert_eq!(s.transcript_version, v0);
    }

    #[test]
    fn arm_watchdog_resets_prior_ack() {
        let mut s = ChatState::new();
        s.awaiting_response_acknowledged = true;
        s.arm_watchdog();
        assert!(!s.awaiting_response_acknowledged);
        assert!(s.awaiting_response_since.is_some());
    }

    #[test]
    fn prompt_history_records_submitted_prompts() {
        let mut s = ChatState::new();
        s.push_history("first".into());
        s.push_history("second".into());
        assert_eq!(s.prompt_history, vec!["first".to_owned(), "second".into()]);
    }

    #[test]
    fn prompt_history_dedups_adjacent_duplicates() {
        let mut s = ChatState::new();
        s.push_history("hi".into());
        s.push_history("hi".into());
        s.push_history("hi".into());
        assert_eq!(s.prompt_history, vec!["hi".to_owned()]);
        // Non-adjacent dup is still recorded.
        s.push_history("bye".into());
        s.push_history("hi".into());
        assert_eq!(
            s.prompt_history,
            vec!["hi".to_owned(), "bye".into(), "hi".into()]
        );
    }

    #[test]
    fn up_arrow_with_empty_input_recalls_latest() {
        let mut s = ChatState::new();
        s.push_history("first".into());
        s.push_history("second".into());
        let recalled = s.history_up().expect("recall latest");
        assert_eq!(recalled, "second");
    }

    #[test]
    fn up_then_up_walks_older() {
        let mut s = ChatState::new();
        s.push_history("a".into());
        s.push_history("b".into());
        s.push_history("c".into());
        assert_eq!(s.history_up().as_deref(), Some("c"));
        assert_eq!(s.history_up().as_deref(), Some("b"));
        assert_eq!(s.history_up().as_deref(), Some("a"));
        // Past the start: returns None, cursor stays put.
        assert_eq!(s.history_up(), None);
        assert_eq!(s.history_cursor, Some(0));
    }

    #[test]
    fn down_returns_to_empty_after_walking_back() {
        let mut s = ChatState::new();
        s.push_history("a".into());
        s.push_history("b".into());
        s.history_up(); // "b"
        s.history_up(); // "a"
        assert_eq!(s.history_down().as_deref(), Some("b"));
        // One more Down past the newest yields empty (back to fresh input).
        assert_eq!(s.history_down().as_deref(), Some(""));
        assert!(s.history_cursor.is_none());
        // Further Down with no active cursor: None.
        assert_eq!(s.history_down(), None);
    }

    #[test]
    fn up_with_nonempty_input_is_noop() {
        // The state-level history_up() is unconditional — the noop policy
        // lives in main.rs (only call when the input buffer is empty).
        // Verify the contract that state cleanly returns the entry; main.rs
        // is what gates the call. This test is the seam.
        let mut s = ChatState::new();
        s.push_history("a".into());
        let result = s.history_up();
        assert_eq!(result.as_deref(), Some("a"));
    }

    #[test]
    fn history_capped_at_200() {
        let mut s = ChatState::new();
        for i in 0..PROMPT_HISTORY_CAP + 50 {
            s.push_history(format!("p{i}"));
        }
        assert_eq!(s.prompt_history.len(), PROMPT_HISTORY_CAP);
        // Oldest evicted: first surviving entry is `p50` (50 dropped).
        assert_eq!(s.prompt_history[0], "p50");
        assert_eq!(
            s.prompt_history.last().map(String::as_str),
            Some(format!("p{}", PROMPT_HISTORY_CAP + 50 - 1).as_str())
        );
    }

    #[test]
    fn submit_resets_history_cursor_so_next_up_returns_latest() {
        // After a submit, the cursor is past the end again; subsequent Up
        // recalls the just-submitted entry. Matches shell behavior.
        let mut s = ChatState::new();
        s.push_history("a".into());
        s.push_history("b".into());
        s.history_up(); // walk to "b"
        s.history_up(); // walk to "a"
                        // New submit lands.
        s.push_history("c".into());
        assert_eq!(s.history_cursor, None);
        assert_eq!(s.history_up().as_deref(), Some("c"));
    }

    #[test]
    fn transcript_rows_reflects_layout_subtractions() {
        // 24 rows total: vpad_top=1, vpad_input_top=1, input_bars=2,
        // input_height=1, vpad_bottom=1, status=1 → transcript = 24-7 = 17.
        let mut s = ChatState::new();
        s.dims = Dims { cols: 80, rows: 24 };
        assert_eq!(s.transcript_rows(), 17);
    }

    #[test]
    fn transcript_rows_shrinks_when_input_grows() {
        let mut s = ChatState::new();
        s.dims = Dims { cols: 80, rows: 24 };
        let base = s.transcript_rows();
        s.input.insert_str("a\nb\nc"); // 3 logical lines
        assert_eq!(s.transcript_rows(), base - 2);
    }
}
