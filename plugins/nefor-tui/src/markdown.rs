//! Markdown → styled-spans pipeline backed by `pulldown-cmark`.
//!
//! Per spec: thin walker, **no default theme**. Lua supplies the theme
//! table; missing entries fall through to neutral. The walker is run
//! per-render — no caching; the spec calls this fast enough for v1
//! (small messages, low cost) and chooses simplicity over incremental
//! parsing.
//!
//! Output shape: a `Vec<StyledChar>` with embedded `\n` between blocks
//! that the layout pass then wraps into rows. Block separators between
//! paragraphs / headings / lists / code-blocks / blockquotes / hr are
//! single newlines; the consumer decides whether to insert blank rows
//! between blocks (we currently do not — the engine's layout pass
//! treats each `\n` as one line break).

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use unicode_width::UnicodeWidthChar;

use crate::desc::{HeadingStyle, MarkdownTheme, Style};
use crate::layout::StyledChar;

/// Render markdown `source` to a flat styled-char run, applying the
/// caller's `theme`. `theme = None` produces neutral output (no styles
/// applied anywhere). The result includes embedded `\n` characters
/// between blocks; the layout pass then wraps each line.
pub fn render_to_styled_chars(source: &str, theme: Option<&MarkdownTheme>) -> Vec<StyledChar> {
    let mut walker = Walker::new(theme);
    let parser = Parser::new_ext(source, Options::all());
    for ev in parser {
        walker.handle(ev);
    }
    walker.finish()
}

#[derive(Default)]
struct InlineStyleStack {
    bold: u32,
    italic: u32,
    code: u32,
    link: u32,
    strikethrough: u32,
}

/// Tracks block context so newlines / list markers / blockquote marks
/// can be emitted at block boundaries.
struct Walker<'a> {
    theme: Option<&'a MarkdownTheme>,
    out: Vec<StyledChar>,
    inline: InlineStyleStack,
    /// Current heading level if inside a heading start..end pair.
    heading_level: Option<HeadingLevel>,
    /// `Some(bool)` (ordered = true / false) when inside a list. Stack
    /// to track nested lists.
    list_stack: Vec<ListContext>,
    /// `true` while the next text run is the body of a code-block.
    in_code_block: bool,
    /// `true` while inside a blockquote (any nesting level).
    blockquote_depth: u32,
    /// `true` if any visible block has been emitted; used to decide
    /// whether a leading newline is needed before the next block.
    started: bool,
    /// `true` if the most recent character emitted is `\n`. Suppresses
    /// duplicate newline emissions when blocks butt up against each
    /// other (e.g., end-of-paragraph then start-of-list).
    at_line_start: bool,
    /// `true` after `Tag::Item` emits its marker but before the item's
    /// content paragraph starts. Suppresses the paragraph-start newline
    /// so `1. ` and the body sit on the same line.
    suppress_next_paragraph_break: bool,
    /// Set while processing a GFM table. Cell text is accumulated here
    /// rather than emitted into `out` so the whole grid can be measured
    /// and aligned at end-of-table.
    table: Option<TableState>,
}

/// Per-table accumulator. Cells are stored as styled-char runs so the
/// final render can pad and wrap them while preserving any inline
/// styling (bold inside a header cell, code spans inside a body cell).
#[derive(Default)]
struct TableState {
    /// Completed rows. Each row is a Vec of cells; each cell is a Vec
    /// of styled chars.
    rows: Vec<Vec<Vec<StyledChar>>>,
    /// Cells of the row currently being assembled.
    current_row: Vec<Vec<StyledChar>>,
    /// Chars of the cell currently being assembled. `None` between
    /// cells.
    current_cell: Option<Vec<StyledChar>>,
    /// How many leading `rows` are header rows. GFM allows at most 1.
    header_rows: usize,
}

struct ListContext {
    ordered: bool,
    /// 1-based item counter for ordered lists.
    next_index: u64,
}

impl<'a> Walker<'a> {
    fn new(theme: Option<&'a MarkdownTheme>) -> Self {
        Walker {
            theme,
            out: Vec::new(),
            inline: InlineStyleStack::default(),
            heading_level: None,
            list_stack: Vec::new(),
            in_code_block: false,
            blockquote_depth: 0,
            started: false,
            at_line_start: true,
            suppress_next_paragraph_break: false,
            table: None,
        }
    }

    fn finish(self) -> Vec<StyledChar> {
        self.out
    }

    fn handle(&mut self, ev: Event<'_>) {
        match ev {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(t) => {
                let s = self.current_inline_style();
                self.emit_str(&t, s);
            }
            Event::Code(t) => {
                let s = self.theme.and_then(|th| th.code).unwrap_or_default();
                self.emit_str(&t, s);
            }
            Event::SoftBreak | Event::HardBreak => {
                // Inline break inside a paragraph / heading: emit a
                // space — wrap-time decisions handle layout.
                self.emit_str(" ", Style::default());
            }
            Event::Rule => {
                self.ensure_block_separator();
                let s = Style::default();
                self.emit_str("---", s);
                self.emit_str("\n", Style::default());
                self.at_line_start = true;
                self.started = true;
            }
            // Inline HTML, footnote refs, task-list markers, etc. are
            // surfaced as raw text — keep behaviour simple.
            Event::Html(t) | Event::InlineHtml(t) => {
                let s = self.current_inline_style();
                self.emit_str(&t, s);
            }
            Event::TaskListMarker(checked) => {
                let marker = if checked { "[x] " } else { "[ ] " };
                let s = self.theme.and_then(|th| th.list_marker).unwrap_or_default();
                self.emit_str(marker, s);
            }
            Event::FootnoteReference(_) | Event::DisplayMath(_) | Event::InlineMath(_) => {
                // Out-of-scope for v1; keep as plain text would require
                // extracting the raw event payload. Skip silently.
            }
        }
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                if self.suppress_next_paragraph_break {
                    self.suppress_next_paragraph_break = false;
                    self.started = true;
                } else {
                    self.ensure_block_separator();
                }
            }
            Tag::Heading { level, .. } => {
                self.ensure_block_separator();
                self.heading_level = Some(level);
                if let Some(hs) = self.heading_at(level) {
                    if let Some(p) = hs.prefix {
                        let mut s = String::with_capacity(p.len_utf8() + 1);
                        s.push(p);
                        s.push(' ');
                        self.emit_str(&s, hs.style);
                    }
                }
            }
            Tag::BlockQuote(_) => {
                self.ensure_block_separator();
                self.blockquote_depth += 1;
                // Emit a `▎ ` left-rail prefix at the start; the heavier
                // glyph reads more clearly as a quote indicator than a
                // bare `>`. For multiline quotes we don't try to prefix
                // every wrapped line — that's a layout-pass concern and
                // we keep the walker simple.
                let s = self.theme.and_then(|th| th.blockquote).unwrap_or_default();
                self.emit_str("▎ ", s);
                // Same trick as Tag::Item: pulldown wraps the quote body
                // in a Tag::Paragraph that would otherwise insert its
                // own blank-line separator and push the body to the next
                // line, leaving `▎ ` orphaned on its own row.
                self.suppress_next_paragraph_break = true;
            }
            Tag::CodeBlock(_) => {
                self.ensure_block_separator();
                self.in_code_block = true;
            }
            Tag::List(start_n) => {
                self.ensure_block_separator();
                self.list_stack.push(ListContext {
                    ordered: start_n.is_some(),
                    next_index: start_n.unwrap_or(1),
                });
            }
            Tag::Item => {
                self.ensure_at_line_start();
                let s = self.theme.and_then(|th| th.list_marker).unwrap_or_default();
                let indent = self.list_stack.len().saturating_sub(1);
                if indent > 0 {
                    self.emit_str(&"  ".repeat(indent), Style::default());
                }
                if let Some(top) = self.list_stack.last_mut() {
                    if top.ordered {
                        let label = format!("{}. ", top.next_index);
                        top.next_index += 1;
                        self.emit_str(&label, s);
                    } else {
                        self.emit_str("- ", s);
                    }
                }
                self.suppress_next_paragraph_break = true;
            }
            Tag::Strong => self.inline.bold += 1,
            Tag::Emphasis => self.inline.italic += 1,
            Tag::Strikethrough => self.inline.strikethrough += 1,
            Tag::Link { .. } => self.inline.link += 1,
            Tag::Image { .. } => { /* skip image alt; inline-text events still fire */ }
            // GFM tables: accumulate cell content into a per-table grid,
            // then render the whole table at TagEnd::Table with padded
            // columns, a header divider, and per-cell smart wrapping.
            // We can't emit cells inline because column widths aren't
            // known until every cell is in.
            Tag::Table(_) => {
                self.ensure_block_separator();
                self.table = Some(TableState::default());
            }
            Tag::TableHead => {
                if let Some(t) = self.table.as_mut() {
                    t.current_row.clear();
                }
            }
            Tag::TableRow => {
                if let Some(t) = self.table.as_mut() {
                    t.current_row.clear();
                }
            }
            Tag::TableCell => {
                if let Some(t) = self.table.as_mut() {
                    t.current_cell = Some(Vec::new());
                }
            }
            // Footnotes / metadata blocks fall through — text events still
            // fire and we surface them as plain spans.
            _ => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.emit_str("\n", Style::default());
                self.at_line_start = true;
            }
            TagEnd::Heading(_) => {
                self.heading_level = None;
                self.emit_str("\n", Style::default());
                self.at_line_start = true;
            }
            TagEnd::BlockQuote(_) => {
                self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
                self.emit_str("\n", Style::default());
                self.at_line_start = true;
            }
            TagEnd::CodeBlock => {
                self.in_code_block = false;
                if !self.at_line_start {
                    self.emit_str("\n", Style::default());
                    self.at_line_start = true;
                }
            }
            TagEnd::List(_) => {
                self.list_stack.pop();
            }
            TagEnd::Item if !self.at_line_start => {
                self.emit_str("\n", Style::default());
                self.at_line_start = true;
            }
            TagEnd::Item => {}
            TagEnd::Strong => self.inline.bold = self.inline.bold.saturating_sub(1),
            TagEnd::Emphasis => self.inline.italic = self.inline.italic.saturating_sub(1),
            TagEnd::Strikethrough => {
                self.inline.strikethrough = self.inline.strikethrough.saturating_sub(1)
            }
            TagEnd::Link => self.inline.link = self.inline.link.saturating_sub(1),
            TagEnd::TableCell => {
                if let Some(t) = self.table.as_mut() {
                    if let Some(cell) = t.current_cell.take() {
                        t.current_row.push(cell);
                    }
                }
            }
            TagEnd::TableHead => {
                if let Some(t) = self.table.as_mut() {
                    let row = std::mem::take(&mut t.current_row);
                    if !row.is_empty() {
                        t.rows.push(row);
                        t.header_rows = t.rows.len();
                    }
                }
            }
            TagEnd::TableRow => {
                if let Some(t) = self.table.as_mut() {
                    let row = std::mem::take(&mut t.current_row);
                    if !row.is_empty() {
                        t.rows.push(row);
                    }
                }
            }
            TagEnd::Table => {
                if let Some(state) = self.table.take() {
                    self.flush_table(state);
                }
            }
            _ => {}
        }
    }

    /// Style for the current inline context. Layered: heading > code >
    /// link > strong > emphasis. Last-wins semantics keep the output
    /// minimal and predictable; no theme produces neutral styling.
    fn current_inline_style(&self) -> Style {
        if self.in_code_block {
            return self.theme.and_then(|t| t.code_block).unwrap_or_default();
        }
        let mut style = Style::default();
        if let Some(level) = self.heading_level {
            style = self.heading_style(level).unwrap_or_default();
        }
        if self.inline.bold > 0 {
            if let Some(s) = self.theme.and_then(|t| t.bold) {
                style = merge_style(style, s);
            }
        }
        if self.inline.italic > 0 {
            if let Some(s) = self.theme.and_then(|t| t.italic) {
                style = merge_style(style, s);
            }
        }
        if self.inline.link > 0 {
            if let Some(s) = self.theme.and_then(|t| t.link) {
                style = merge_style(style, s);
            }
        }
        if self.inline.code > 0 {
            if let Some(s) = self.theme.and_then(|t| t.code) {
                style = merge_style(style, s);
            }
        }
        if self.inline.strikethrough > 0 {
            if let Some(s) = self.theme.and_then(|t| t.strikethrough) {
                style = merge_style(style, s);
            }
        }
        style
    }

    fn heading_at(&self, level: HeadingLevel) -> Option<HeadingStyle> {
        let theme = self.theme?;
        match level {
            HeadingLevel::H1 => theme.h1,
            HeadingLevel::H2 => theme.h2,
            HeadingLevel::H3 => theme.h3,
            HeadingLevel::H4 => theme.h4,
            HeadingLevel::H5 => theme.h5,
            HeadingLevel::H6 => theme.h6,
        }
    }

    fn heading_style(&self, level: HeadingLevel) -> Option<Style> {
        self.heading_at(level).map(|hs| hs.style)
    }

    fn emit_str(&mut self, s: &str, style: Style) {
        // Inside a table cell: redirect chars into the cell buffer and
        // skip block-state tracking — at_line_start / started apply to
        // the main output stream, not to cell-internal text. Cell content
        // is only inline (text, code, soft-breaks-as-spaces) so the
        // newline-tracking flags don't need to follow.
        if let Some(cell) = self
            .table
            .as_mut()
            .and_then(|t| t.current_cell.as_mut())
        {
            for ch in s.chars() {
                cell.push(StyledChar { ch, style });
            }
            return;
        }
        for ch in s.chars() {
            self.out.push(StyledChar { ch, style });
            self.at_line_start = ch == '\n';
        }
    }

    fn ensure_block_separator(&mut self) {
        if !self.started {
            self.started = true;
            return;
        }
        // Block boundaries (paragraph, heading, list, code-block,
        // blockquote, hr) get a blank line between them so prose stays
        // readable. End the current line if we're not already at line
        // start, then emit one more `\n` for the blank.
        if !self.at_line_start {
            self.emit_str("\n", Style::default());
        }
        self.emit_str("\n", Style::default());
        self.at_line_start = true;
        self.started = true;
    }

    fn ensure_at_line_start(&mut self) {
        if !self.at_line_start {
            self.emit_str("\n", Style::default());
            self.at_line_start = true;
        }
    }

    /// Per-column max width before we wrap. Generous enough that most
    /// real-world cells (short labels, one-sentence descriptions) fit on
    /// one line; tight enough that even a 4-column table stays under a
    /// typical 80-column terminal once you add ` │ ` separators. If this
    /// turns out to be wrong we'll plumb the actual layout width through
    /// the walker.
    const MAX_COL_WIDTH: usize = 40;

    /// Render the accumulated table into `self.out` with padded columns,
    /// a header divider, and per-cell smart wrapping. Called once at
    /// `TagEnd::Table` with the populated state.
    fn flush_table(&mut self, mut state: TableState) {
        // Drop a half-built row if a stray TagEnd::Table fired without
        // the matching TableRow end — defensive, not expected in practice.
        if !state.current_row.is_empty() {
            state.rows.push(std::mem::take(&mut state.current_row));
        }
        let num_rows = state.rows.len();
        if num_rows == 0 {
            return;
        }
        let num_cols = state.rows.iter().map(|r| r.len()).max().unwrap_or(0);
        if num_cols == 0 {
            return;
        }
        // Pad short rows so every row has the same column count. Missing
        // cells become empty strings which render as blank padding.
        for row in state.rows.iter_mut() {
            while row.len() < num_cols {
                row.push(Vec::new());
            }
        }

        // Natural width per column = the widest cell text in that column,
        // capped by MAX_COL_WIDTH so a single huge cell can't blow up the
        // table. Cells longer than the cap are wrapped below.
        let mut col_widths = vec![0usize; num_cols];
        for row in &state.rows {
            for (i, cell) in row.iter().enumerate() {
                let w = cell_natural_width(cell);
                if w > col_widths[i] {
                    col_widths[i] = w;
                }
            }
        }
        for w in col_widths.iter_mut() {
            *w = (*w).clamp(1, Self::MAX_COL_WIDTH);
        }

        // Pre-wrap every cell to its column width. `wrap_cell_lines`
        // word-wraps with a char fallback for words that don't fit.
        let wrapped: Vec<Vec<Vec<Vec<StyledChar>>>> = state
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .enumerate()
                    .map(|(i, cell)| wrap_cell_lines(cell, col_widths[i]))
                    .collect()
            })
            .collect();

        // ensure_block_separator already ran at Tag::Table start; if the
        // walker is mid-line for any reason, finish that line first.
        if !self.at_line_start {
            self.emit_str("\n", Style::default());
            self.at_line_start = true;
        }

        let header_count = state.header_rows.min(num_rows);

        for (row_idx, wrapped_row) in wrapped.iter().enumerate() {
            let visual_lines = wrapped_row.iter().map(|c| c.len().max(1)).max().unwrap_or(1);
            for line_idx in 0..visual_lines {
                self.emit_str("│", Style::default());
                for (col_idx, cell_lines) in wrapped_row.iter().enumerate() {
                    self.emit_str(" ", Style::default());
                    let target = col_widths[col_idx];
                    let used = if let Some(line) = cell_lines.get(line_idx) {
                        for sc in line {
                            self.out.push(sc.clone());
                        }
                        line.iter().map(|c| char_display_width(c.ch)).sum::<usize>()
                    } else {
                        0
                    };
                    if used < target {
                        for _ in 0..(target - used) {
                            self.emit_str(" ", Style::default());
                        }
                    }
                    self.emit_str(" │", Style::default());
                }
                self.emit_str("\n", Style::default());
                self.at_line_start = true;
            }

            // Emit the header divider once we've finished the last header
            // row, only if there are body rows after it. With no body
            // rows the divider would dangle at the bottom — drop it.
            if header_count > 0 && row_idx + 1 == header_count && num_rows > header_count {
                self.emit_str("├", Style::default());
                for (col_idx, &w) in col_widths.iter().enumerate() {
                    let dashes = w + 2; // 2 = the leading + trailing spaces
                    for _ in 0..dashes {
                        self.emit_str("─", Style::default());
                    }
                    if col_idx + 1 < num_cols {
                        self.emit_str("┼", Style::default());
                    } else {
                        self.emit_str("┤", Style::default());
                    }
                }
                self.emit_str("\n", Style::default());
                self.at_line_start = true;
            }
        }
        self.started = true;
    }
}

fn cell_natural_width(cell: &[StyledChar]) -> usize {
    cell.iter().map(|c| char_display_width(c.ch)).sum()
}

fn char_display_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

/// Word-wrap `cell` into lines no wider than `limit`. A "word" is a
/// run of non-whitespace chars. When a single word doesn't fit on its
/// own line, fall back to char-wrapping it. Mirrors the smart-fallback
/// behavior of `wrap_styled_word` in layout.rs but kept self-contained
/// to avoid a circular module dep — markdown.rs is upstream of layout.rs
/// in the call graph (layout.rs renders markdown's output).
fn wrap_cell_lines(cell: &[StyledChar], limit: usize) -> Vec<Vec<StyledChar>> {
    if limit == 0 || cell.is_empty() {
        return vec![cell.to_vec()];
    }
    let mut out: Vec<Vec<StyledChar>> = Vec::new();
    let mut current: Vec<StyledChar> = Vec::new();
    let mut col = 0usize;

    for word in split_styled_into_words(cell) {
        let ww: usize = word.iter().map(|c| char_display_width(c.ch)).sum();
        let is_ws = word.iter().all(|c| c.ch.is_whitespace());

        if col > 0 && col + ww > limit {
            out.push(std::mem::take(&mut current));
            col = 0;
            if is_ws {
                continue;
            }
        }

        if col == 0 && ww > limit {
            for sub in char_wrap_styled(word, limit) {
                out.push(sub);
            }
            current.clear();
            col = 0;
            continue;
        }

        current.extend_from_slice(word);
        col += ww;
    }
    if !current.is_empty() {
        out.push(current);
    }
    if out.is_empty() {
        out.push(Vec::new());
    }
    out
}

fn split_styled_into_words(line: &[StyledChar]) -> Vec<&[StyledChar]> {
    let mut out: Vec<&[StyledChar]> = Vec::new();
    if line.is_empty() {
        return out;
    }
    let mut start = 0usize;
    let mut in_space = line[0].ch.is_whitespace();
    for (i, c) in line.iter().enumerate() {
        let cw = c.ch.is_whitespace();
        if cw != in_space {
            out.push(&line[start..i]);
            start = i;
            in_space = cw;
        }
    }
    if start < line.len() {
        out.push(&line[start..]);
    }
    out
}

fn char_wrap_styled(line: &[StyledChar], limit: usize) -> Vec<Vec<StyledChar>> {
    let mut out: Vec<Vec<StyledChar>> = Vec::new();
    let mut current: Vec<StyledChar> = Vec::new();
    let mut col = 0usize;
    for sc in line {
        let w = char_display_width(sc.ch);
        if col + w > limit && !current.is_empty() {
            out.push(std::mem::take(&mut current));
            col = 0;
        }
        current.push(sc.clone());
        col += w;
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Apply `over` on top of `base`. Each field on `over` wins when set;
/// boolean attributes OR together. Used to layer heading-style with
/// inline styles like bold or italic.
fn merge_style(base: Style, over: Style) -> Style {
    Style {
        fg: over.fg.or(base.fg),
        bg: over.bg.or(base.bg),
        bold: base.bold || over.bold,
        italic: base.italic || over.italic,
        underline: base.underline || over.underline,
        reverse: base.reverse || over.reverse,
        strikethrough: base.strikethrough || over.strikethrough,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desc::Color;

    fn neutral_text(s: &str) -> Vec<StyledChar> {
        s.chars()
            .map(|ch| StyledChar {
                ch,
                style: Style::default(),
            })
            .collect()
    }

    #[test]
    fn neutral_theme_renders_plain_text() {
        let r = render_to_styled_chars("hello world", None);
        assert_eq!(r, neutral_text("hello world\n"));
    }

    #[test]
    fn missing_theme_entries_fall_through_to_neutral() {
        // Bold span with empty theme → no style applied (still neutral).
        let theme = MarkdownTheme::default();
        let r = render_to_styled_chars("**bold**", Some(&theme));
        for ch in &r {
            assert_eq!(
                ch.style,
                Style::default(),
                "empty theme should yield neutral chars"
            );
        }
    }

    #[test]
    fn bold_theme_applies_to_strong_text() {
        let bold = Style {
            bold: true,
            ..Style::default()
        };
        let theme = MarkdownTheme {
            bold: Some(bold),
            ..MarkdownTheme::default()
        };
        let r = render_to_styled_chars("a **b** c", Some(&theme));
        // Find the 'b' and confirm it's bold; surrounding chars are neutral.
        let b_char = r.iter().find(|c| c.ch == 'b').expect("b present");
        assert!(b_char.style.bold);
        let a_char = r.iter().find(|c| c.ch == 'a').expect("a present");
        assert!(!a_char.style.bold);
    }

    #[test]
    fn italic_theme_applies_to_emphasis() {
        let italic = Style {
            italic: true,
            ..Style::default()
        };
        let theme = MarkdownTheme {
            italic: Some(italic),
            ..MarkdownTheme::default()
        };
        let r = render_to_styled_chars("_em_", Some(&theme));
        let e = r.iter().find(|c| c.ch == 'e').expect("e present");
        assert!(e.style.italic);
    }

    #[test]
    fn inline_code_styled_when_themed() {
        let code = Style {
            fg: Some(Color::Indexed(208)),
            ..Style::default()
        };
        let theme = MarkdownTheme {
            code: Some(code),
            ..MarkdownTheme::default()
        };
        let r = render_to_styled_chars("`code`", Some(&theme));
        let c = r.iter().find(|c| c.ch == 'c').expect("c present");
        assert_eq!(c.style.fg, Some(Color::Indexed(208)));
    }

    #[test]
    fn code_block_styled_when_themed() {
        let cb = Style {
            fg: Some(Color::Indexed(244)),
            ..Style::default()
        };
        let theme = MarkdownTheme {
            code_block: Some(cb),
            ..MarkdownTheme::default()
        };
        let r = render_to_styled_chars("```\nx = 1\n```", Some(&theme));
        let x = r.iter().find(|c| c.ch == 'x').expect("x present");
        assert_eq!(x.style.fg, Some(Color::Indexed(244)));
    }

    #[test]
    fn each_heading_level_styled_independently() {
        let h1 = Style {
            bold: true,
            fg: Some(Color::Indexed(196)),
            ..Style::default()
        };
        let h2 = Style {
            italic: true,
            ..Style::default()
        };
        let h3 = Style {
            underline: true,
            ..Style::default()
        };
        let h4 = Style {
            reverse: true,
            ..Style::default()
        };
        let h5 = Style {
            bold: true,
            italic: true,
            ..Style::default()
        };
        let h6 = Style {
            fg: Some(Color::Indexed(8)),
            ..Style::default()
        };
        let theme = MarkdownTheme {
            h1: Some(HeadingStyle {
                style: h1,
                prefix: None,
            }),
            h2: Some(HeadingStyle {
                style: h2,
                prefix: None,
            }),
            h3: Some(HeadingStyle {
                style: h3,
                prefix: None,
            }),
            h4: Some(HeadingStyle {
                style: h4,
                prefix: None,
            }),
            h5: Some(HeadingStyle {
                style: h5,
                prefix: None,
            }),
            h6: Some(HeadingStyle {
                style: h6,
                prefix: None,
            }),
            ..MarkdownTheme::default()
        };
        for (level, expected) in [
            ("# x", h1),
            ("## x", h2),
            ("### x", h3),
            ("#### x", h4),
            ("##### x", h5),
            ("###### x", h6),
        ] {
            let r = render_to_styled_chars(level, Some(&theme));
            let x = r.iter().find(|c| c.ch == 'x').expect("x present");
            assert_eq!(x.style, expected, "for {level}");
        }
    }

    #[test]
    fn unordered_list_emits_dash_marker() {
        let r = render_to_styled_chars("- item one\n- item two", None);
        let s: String = r.iter().map(|c| c.ch).collect();
        assert!(s.contains("- item one"));
        assert!(s.contains("- item two"));
    }

    #[test]
    fn ordered_list_emits_numbered_markers() {
        let r = render_to_styled_chars("1. one\n2. two", None);
        let s: String = r.iter().map(|c| c.ch).collect();
        assert!(s.contains("1. one"));
        assert!(s.contains("2. two"));
    }

    #[test]
    fn list_marker_themed() {
        let marker = Style {
            fg: Some(Color::Indexed(244)),
            ..Style::default()
        };
        let theme = MarkdownTheme {
            list_marker: Some(marker),
            ..MarkdownTheme::default()
        };
        let r = render_to_styled_chars("- item", Some(&theme));
        let dash = r.iter().find(|c| c.ch == '-').expect("- marker");
        assert_eq!(dash.style.fg, Some(Color::Indexed(244)));
        let i = r.iter().find(|c| c.ch == 'i').expect("body");
        // Body is plain — list_marker theme only paints the marker.
        assert_ne!(i.style, dash.style);
    }

    #[test]
    fn blockquote_marker_emitted_and_themed() {
        let q = Style {
            italic: true,
            ..Style::default()
        };
        let theme = MarkdownTheme {
            blockquote: Some(q),
            ..MarkdownTheme::default()
        };
        let r = render_to_styled_chars("> quoted text", Some(&theme));
        let s: String = r.iter().map(|c| c.ch).collect();
        // Walker emits a `▎ ` left-rail glyph styled with the blockquote
        // theme — heavier than `> ` and reads as a quote rail rather
        // than email-style angle-quote.
        assert!(s.contains("▎ "));
        let rail = r.iter().find(|c| c.ch == '▎').expect("rail glyph present");
        assert!(rail.style.italic);
    }

    #[test]
    fn link_theme_applies_to_link_text() {
        let l = Style {
            underline: true,
            ..Style::default()
        };
        let theme = MarkdownTheme {
            link: Some(l),
            ..MarkdownTheme::default()
        };
        let r = render_to_styled_chars("[here](http://x)", Some(&theme));
        let h = r.iter().find(|c| c.ch == 'h').expect("h");
        assert!(h.style.underline);
    }

    #[test]
    fn paragraphs_separated_by_newlines() {
        let r = render_to_styled_chars("first\n\nsecond", None);
        let s: String = r.iter().map(|c| c.ch).collect();
        assert!(s.contains("first\n"));
        assert!(s.contains("second"));
    }

    #[test]
    fn nested_list_indents_inner_items() {
        let r = render_to_styled_chars("- outer\n  - inner", None);
        let s: String = r.iter().map(|c| c.ch).collect();
        // The inner item should be indented (two leading spaces before
        // the marker for nesting depth 1).
        assert!(s.contains("- outer"));
        assert!(s.contains("  - inner"));
    }

    #[test]
    fn empty_input_produces_empty_output() {
        assert!(render_to_styled_chars("", None).is_empty());
    }

    #[test]
    fn gfm_table_renders_with_padded_columns_and_header_divider() {
        // Verify the v2 table renderer pads cells out to the natural
        // column width so columns line up vertically, drops a Unicode
        // header divider between header and body, and uses `│` as the
        // vertical separator.
        let src = "| Tool | Purpose |\n| --- | --- |\n| read_file | reads files |\n";
        let r = render_to_styled_chars(src, None);
        let s: String = r.iter().map(|c| c.ch).collect();
        // Column widths: max("Tool", "read_file") = 9; max("Purpose",
        // "reads files") = 11. Cells are wrapped with ` ... ` padding,
        // the row borders are `│` glyphs.
        assert!(
            s.contains("│ Tool      │ Purpose     │\n"),
            "header should be padded out to column widths, got: {s:?}"
        );
        assert!(
            s.contains("├───────────┼─────────────┤\n"),
            "header divider missing or wrong, got: {s:?}"
        );
        assert!(
            s.contains("│ read_file │ reads files │\n"),
            "body row should be padded, got: {s:?}"
        );
    }

    #[test]
    fn gfm_table_wraps_cells_that_exceed_column_cap() {
        // A cell whose content is wider than MAX_COL_WIDTH (40) should
        // wrap to multiple visual lines. The other column on the same
        // logical row pads out blank so the table stays aligned.
        let long = "the quick brown fox jumps over the lazy dog and then keeps on running for several more lines worth of words";
        let src = format!("| short | long |\n| --- | --- |\n| a | {long} |\n");
        let r = render_to_styled_chars(&src, None);
        let s: String = r.iter().map(|c| c.ch).collect();
        // Body row spans more than one visual line: count lines that
        // start with `│ a ` (the short cell only appears on the first
        // wrap line — subsequent lines have an empty short cell).
        let blank_short_cell_lines = s
            .lines()
            .filter(|l| l.starts_with("│       │ ") && l.ends_with(" │"))
            .count();
        assert!(
            blank_short_cell_lines >= 1,
            "expected at least one wrap-continuation line with the short cell padded blank, got: {s:?}"
        );
        // No row ever exceeds: 1 (left │) + 1 (pad) + 5 (short col) + 1 (pad) + 1 (│)
        // + 1 (pad) + 40 (long col cap) + 1 (pad) + 1 (right │) = 52 cols.
        for line in s.lines() {
            if line.starts_with('│') {
                let w: usize = line.chars().map(char_display_width).sum();
                assert!(w <= 52, "row {line:?} exceeded expected width 52, was {w}");
            }
        }
    }

    #[test]
    fn strikethrough_body_picks_up_themed_style() {
        // pulldown-cmark's Strikethrough extension is enabled via
        // Options::all() — `~~deleted~~` becomes a Tag::Strikethrough
        // inline. The walker maps the run to the theme's
        // `strikethrough` entry; with a themed entry, the inner chars
        // carry the strikethrough attribute.
        let strike = Style {
            strikethrough: true,
            ..Style::default()
        };
        let theme = MarkdownTheme {
            strikethrough: Some(strike),
            ..MarkdownTheme::default()
        };
        let r = render_to_styled_chars("~~gone~~", Some(&theme));
        let g = r.iter().find(|c| c.ch == 'g').expect("g present");
        assert!(
            g.style.strikethrough,
            "themed strikethrough should mark the inner text",
        );
    }

    #[test]
    fn strikethrough_without_theme_entry_stays_neutral() {
        // No theme → neutral output, exactly as bold/italic without
        // theme entries. Confirms the no-default-theme contract.
        let r = render_to_styled_chars("~~gone~~", None);
        for c in &r {
            assert_eq!(c.style, Style::default());
        }
    }
}
