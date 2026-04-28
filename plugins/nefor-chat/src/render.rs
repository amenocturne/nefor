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

use crate::state::{ChatState, Role, SessionMetadata, TranscriptEntry};
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
    lines: Vec<Line>,
}

/// Compute the wrapped + markdown-parsed transcript lines, consulting the
/// thread-local cache. Cache key = `(transcript_version, cols, pending)`.
/// Misses recompute and store; hits clone the cached `Vec<Line>` (cheap
/// compared to a markdown re-parse of the entire transcript).
fn wrapped_with_cache(state: &ChatState, cols: u32) -> Vec<Line> {
    let pending = state.pending && !last_is_streaming_assistant(&state.transcript);

    let hit = TRANSCRIPT_CACHE.with(|cell| {
        let c = cell.borrow();
        c.as_ref()
            .filter(|c| {
                c.version == state.transcript_version && c.cols == cols && c.pending == pending
            })
            .map(|c| c.lines.clone())
    });
    if let Some(lines) = hit {
        return lines;
    }

    let mut wrapped = wrap_transcript(&state.transcript, cols as usize);
    if pending {
        for line in wrap_to_width("[claude is thinking...]", cols as usize) {
            wrapped.push(vec![Span::new(line, HL_SYSTEM)]);
        }
    }

    TRANSCRIPT_CACHE.with(|cell| {
        *cell.borrow_mut() = Some(TranscriptCache {
            version: state.transcript_version,
            cols,
            pending,
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

// ---- frame construction ----------------------------------------------------

pub fn render_frame(state: &ChatState) -> Vec<Map<String, Value>> {
    let mut out: Vec<Map<String, Value>> = Vec::new();
    let cols = state.dims.cols.max(1);
    let rows = state.dims.rows.max(2);

    let (input_lines, cursor_line_in_input, cursor_col) =
        render_input_wrapped(&state.input.as_string(), state.input.cursor(), cols);

    let status_height: u32 = if rows >= 3 { 1 } else { 0 };

    // Layout, top → bottom: transcript · input · status. Status anchors to
    // the very bottom row so it stays visible at startup (before any
    // transcript content) and acts as a stable anchor for the eye when the
    // input grows / shrinks.
    let max_input_rows = rows.saturating_sub(1 + status_height).max(1);
    let input_height = (input_lines.len() as u32).clamp(1, max_input_rows);
    let transcript_rows = rows - input_height - status_height;
    let input_start_row = transcript_rows;
    let status_row = transcript_rows + input_height;

    let input_scroll = (input_lines.len() as u32).saturating_sub(input_height);
    let cursor_line_visible = cursor_line_in_input
        .saturating_sub(input_scroll)
        .min(input_height.saturating_sub(1));

    out.push(grid_clear());

    let wrapped = wrapped_with_cache(state, cols);
    let total = wrapped.len() as u32;

    let max_offset = total.saturating_sub(transcript_rows);
    let effective_scroll = state.scroll_offset.min(max_offset);

    let (first_line_idx, transcript_start_row) =
        compute_viewport(total, transcript_rows, effective_scroll);

    for visible_row in 0..transcript_rows {
        let line_idx_u32 = first_line_idx.checked_add(visible_row);
        let row_to_paint = transcript_start_row + visible_row;
        match line_idx_u32.and_then(|i| wrapped.get(i as usize)) {
            Some(line) => out.push(grid_line(row_to_paint, cols, line)),
            None => out.push(grid_line_blank(row_to_paint, cols)),
        }
    }

    for i in 0..input_height {
        let src_idx = (input_scroll + i) as usize;
        let text = input_lines.get(src_idx).map(String::as_str).unwrap_or("");
        out.push(grid_line(
            input_start_row + i,
            cols,
            &[Span::new(text, HL_INPUT)],
        ));
    }

    if status_height > 0 {
        let spans = build_status_spans(
            &state.metadata,
            total,
            transcript_rows,
            effective_scroll,
            cols,
        );
        out.push(grid_line(status_row, cols, &spans));
    }

    out.push(grid_cursor_goto(
        input_start_row + cursor_line_visible,
        cursor_col,
    ));

    out.push(grid_flush());
    out
}

fn last_is_streaming_assistant(entries: &[TranscriptEntry]) -> bool {
    entries
        .last()
        .is_some_and(|e| e.role == Role::Assistant && e.streaming)
}

fn wrap_transcript(entries: &[TranscriptEntry], cols: usize) -> Vec<Line> {
    let mut out: Vec<Line> = Vec::new();
    for e in entries {
        match e.role {
            Role::User => {
                let prefix = "you> ";
                let full = format!("{prefix}{}", e.text);
                for line in wrap_to_width(&full, cols) {
                    out.push(vec![Span::new(line, HL_USER)]);
                }
            }
            Role::System => {
                let bracketed = format!("[{}]", e.text);
                for line in wrap_to_width(&bracketed, cols) {
                    out.push(vec![Span::new(line, HL_SYSTEM)]);
                }
            }
            Role::Assistant => {
                // Assistant messages flow through the markdown pipeline.
                // The "claude> " prefix lives on its own first line so
                // bold/italic/code formatting only applies to body text,
                // not the role label. This preserves the previous look while
                // adding rich rendering.
                let md_lines = markdown::render(&e.text, cols);
                for (i, line) in md_lines.iter().enumerate() {
                    if i == 0 {
                        let mut row: Line = vec![Span::new("claude> ", HL_ASSISTANT)];
                        let prefix_w = "claude> ".len();
                        // Re-wrap the first line under a tighter budget so
                        // the prefix doesn't push the row past `cols`.
                        let remaining = cols.saturating_sub(prefix_w);
                        let (first, overflow) = split_spans_at_width(line, remaining);
                        row.extend(first);
                        out.push(row);
                        if !overflow.is_empty() {
                            // Continue the wrapped overflow on a fresh line
                            // (no prefix). Also wrap that further if needed.
                            for sub in wrap_spans(&overflow, cols) {
                                out.push(sub);
                            }
                        }
                    } else {
                        out.push(line.clone());
                    }
                }
                if md_lines.is_empty() {
                    // Empty assistant entry — still surface the prefix so
                    // streaming start is visible.
                    out.push(vec![Span::new("claude> ", HL_ASSISTANT)]);
                }
            }
        }
    }
    out
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
                for raw in body.lines() {
                    // Hard-break long code-lines at column boundary; we
                    // never word-wrap inside code.
                    let chunks = crate::wrap::split_by_columns(raw, cols.max(1));
                    if chunks.is_empty() {
                        out.push(vec![Span::new("", HL_MD_CODE_BLOCK)]);
                    } else {
                        for c in chunks {
                            out.push(vec![Span::new(c, HL_MD_CODE_BLOCK)]);
                        }
                    }
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
/// 12s · scroll-info`. When `cols` is too narrow for everything, segments
/// are dropped right-to-left in this order: scroll-info, last-duration,
/// turns, cost, ctx-bar. The model name is preserved as the most identifying
/// piece.
pub fn build_status_spans(
    md: &SessionMetadata,
    total: u32,
    transcript_rows: u32,
    scroll_offset: u32,
    cols: u32,
) -> Vec<Span> {
    let cols = cols as usize;

    // Build candidate segments in priority order (must-keep first). Each
    // segment carries its own spans and a width.
    let model_seg = build_model_segment(md);
    let ctx_seg = build_ctx_segment(md, cols);
    let cost_seg = build_cost_segment(md);
    let turns_seg = build_turns_segment(md);
    let dur_seg = build_duration_segment(md);
    let scroll_seg = build_scroll_segment(total, transcript_rows, scroll_offset);

    let separator = || Span::new(" │ ", HL_STATUS_DIM);
    let sep_w = str_width(" │ ");

    let mut segs: Vec<Vec<Span>> = vec![model_seg];
    for seg in [ctx_seg, cost_seg, turns_seg, dur_seg, scroll_seg]
        .into_iter()
        .flatten()
    {
        segs.push(seg);
    }

    // Drop right-side segments until the total fits. Always keep the model.
    while segs.len() > 1 && total_width(&segs, sep_w) > cols {
        segs.pop();
    }
    // If even the model doesn't fit, truncate it.
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
    let used = md.cumulative_input_tokens.unwrap_or(0)
        + md.cumulative_cache_read.unwrap_or(0)
        + md.cumulative_cache_creation.unwrap_or(0);
    if used == 0 && md.cumulative_input_tokens.is_none() {
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

/// Compose a one-liner system message for a `chat.tool.start` event.
///
/// Recognises the common Claude Code tool names and surfaces the most
/// salient input field (file path, command, pattern). Unknown tools fall
/// back to just the name. The line is bracketed by the system-entry
/// renderer and truncated at 80 cols on the salient field.
pub fn tool_start_line(name: &str, input: Option<&Value>) -> String {
    let arrow = "⏵";
    let salient = match name {
        "Bash" => input.and_then(|v| v.get("command")).and_then(Value::as_str),
        "Read" => input
            .and_then(|v| v.get("file_path"))
            .and_then(Value::as_str),
        "Edit" | "Write" | "MultiEdit" => input
            .and_then(|v| v.get("file_path"))
            .and_then(Value::as_str),
        "Grep" => input.and_then(|v| v.get("pattern")).and_then(Value::as_str),
        "Glob" => input.and_then(|v| v.get("pattern")).and_then(Value::as_str),
        _ => None,
    };

    match (name, salient) {
        ("Read", Some(path)) => {
            // Optional :line suffix when input includes an `offset` field.
            if let Some(line) = input.and_then(|v| v.get("offset")).and_then(Value::as_u64) {
                format!("{arrow} {name}: {} :{line}", truncate(path, 80))
            } else {
                format!("{arrow} {name}: {}", truncate(path, 80))
            }
        }
        ("Grep", Some(pattern)) => {
            let path = input
                .and_then(|v| v.get("path"))
                .and_then(Value::as_str)
                .unwrap_or(".");
            format!("{arrow} {name}: \"{}\" in {path}", truncate(pattern, 60))
        }
        (_, Some(s)) => format!("{arrow} {name}: {}", truncate(s, 80)),
        (_, None) => format!("{arrow} {name}"),
    }
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
) -> (Vec<String>, u32, u32) {
    let prefix = "> ";
    let prefix_w = str_width(prefix);
    let cols_usize = cols as usize;

    if cols_usize == 0 {
        return (vec![String::new()], 0, 0);
    }
    if cols_usize <= prefix_w {
        let text: String = prefix.chars().take(cols_usize).collect();
        return (vec![text], 0, 0);
    }

    let full = format!("{prefix}{buffer}");
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_w = 0usize;
    for c in full.chars() {
        let cw = char_width(c);
        if current_w + cw > cols_usize && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            current_w = 0;
        }
        current.push(c);
        current_w += cw;
    }
    lines.push(current);

    let cursor_display_col = prefix_w
        + buffer
            .chars()
            .take(cursor_char_offset)
            .map(char_width)
            .sum::<usize>();
    let cursor_line = (cursor_display_col / cols_usize) as u32;
    let cursor_col = (cursor_display_col % cols_usize) as u32;
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

    fn row_hl(row: &Map<String, Value>) -> u64 {
        let cells = row["cells"].as_array().expect("cells");
        cells[0][1].as_u64().expect("hl_id on first cell")
    }

    #[test]
    fn palette_defines_one_per_constant() {
        let defs = palette_defines();
        // Must include every constant we hand out. 17 IDs × 1 define each.
        assert_eq!(defs.len(), 17);
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
        // clear + 2 transcript blanks + 1 status + 1 input + cursor + flush = 7
        assert_eq!(events.len(), 7);
        assert_eq!(
            events[0]["kind"],
            Value::String("nefor-tui.grid.clear".into())
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
        let s = state_with(
            vec![(Role::User, "hello"), (Role::Assistant, "hi there")],
            "",
            Dims { cols: 40, rows: 6 },
        );
        let events = render_frame(&s);
        let row0 = &events[1];
        let row1 = &events[2];
        assert_eq!(row0["row"], Value::Number(0u32.into()));
        assert_eq!(row_text(row0), "you> hello");
        assert_eq!(row_hl(row0), HL_USER as u64);
        assert!(row_text(row1).starts_with("claude> "));
        assert!(row_text(row1).contains("hi there"));
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
        let goto = events
            .iter()
            .find(|e| e["kind"] == Value::String("nefor-tui.grid.cursor_goto".into()))
            .expect("cursor_goto emitted");
        // Layout in 3 rows: row 0 transcript, row 1 input, row 2 status.
        // Cursor goes on the input row, after the 5-char prompt + 2-char
        // "> " prefix → col 7.
        assert_eq!(goto["col"], Value::Number(7u32.into()));
        assert_eq!(goto["row"], Value::Number(1u32.into()));
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
    fn status_with_no_metadata_shows_dim_dash_for_model() {
        let md = SessionMetadata::default();
        let spans = build_status_spans(&md, 0, 10, 0, 80);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "—");
        assert_eq!(spans[0].hl, HL_STATUS_DIM);
    }

    #[test]
    fn status_with_full_metadata_renders_full_layout() {
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

        let spans = build_status_spans(&md, 0, 10, 0, 200);
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
        let spans = build_status_spans(&md, 0, 10, 0, 18);
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
            cumulative_input_tokens: Some(150_000), // 75%
            ..Default::default()
        };
        let spans = build_status_spans(&md, 0, 10, 0, 200);
        let bar = spans.iter().find(|s| s.text.contains('█'));
        assert!(bar.is_some(), "filled bar present: {spans:?}");
        assert_eq!(bar.unwrap().hl, HL_STATUS_WARN);
    }

    #[test]
    fn ctx_bar_uses_danger_color_above_90_percent() {
        let md = SessionMetadata {
            stats_seen: true,
            model: Some("claude-opus-4-7".into()),
            cumulative_input_tokens: Some(190_000), // 95%
            ..Default::default()
        };
        let spans = build_status_spans(&md, 0, 10, 0, 200);
        let bar = spans.iter().find(|s| s.text.contains('█'));
        assert_eq!(bar.unwrap().hl, HL_STATUS_DANGER);
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
        let line_a = lines.iter().find(|l| l.iter().any(|s| s.text == "foo"));
        let line_b = lines.iter().find(|l| l.iter().any(|s| s.text == "bar"));
        assert!(line_a.is_some() && line_b.is_some(), "code lines missing");
        for line in [line_a.unwrap(), line_b.unwrap()] {
            for s in line {
                if !s.text.is_empty() {
                    assert_eq!(s.hl, HL_MD_CODE_BLOCK);
                }
            }
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
    fn tool_start_line_for_bash_includes_command() {
        let input = serde_json::json!({"command": "ls -la"});
        let line = tool_start_line("Bash", Some(&input));
        assert!(line.contains("Bash"), "{line}");
        assert!(line.contains("ls -la"), "{line}");
    }

    #[test]
    fn tool_start_line_for_read_includes_path() {
        let input = serde_json::json!({"file_path": "/etc/hosts"});
        let line = tool_start_line("Read", Some(&input));
        assert!(line.contains("Read"));
        assert!(line.contains("/etc/hosts"));
    }

    #[test]
    fn tool_start_line_for_grep_quotes_pattern() {
        let input = serde_json::json!({"pattern": "TODO", "path": "src/"});
        let line = tool_start_line("Grep", Some(&input));
        assert!(line.contains("\"TODO\""), "{line}");
        assert!(line.contains("src/"));
    }

    #[test]
    fn tool_start_line_for_unknown_tool_just_name() {
        let line = tool_start_line("CustomTool", None);
        assert!(line.contains("CustomTool"));
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
}
