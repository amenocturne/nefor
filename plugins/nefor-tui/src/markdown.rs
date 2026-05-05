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
///
/// `available_width` is the column budget the caller has for table
/// rendering. When `Some(w)`, tables proportionally shrink columns so
/// the assembled grid fits within `w` visual columns. When `None`, the
/// table renderer falls back to its natural per-column widths (no cap).
pub fn render_to_styled_chars(
    source: &str,
    theme: Option<&MarkdownTheme>,
    available_width: Option<usize>,
) -> Vec<StyledChar> {
    let mut walker = Walker::new(theme, available_width);
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
    /// Column budget for tables. `None` = render at natural width;
    /// `Some(w)` = proportionally shrink to fit `w` columns.
    available_width: Option<usize>,
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
    fn new(theme: Option<&'a MarkdownTheme>, available_width: Option<usize>) -> Self {
        Walker {
            theme,
            available_width,
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

    /// Per-column hard cap. Set high — the real fit-to-width discipline
    /// is the proportional-shrink pass in `flush_table`, not this cap.
    /// We keep a ceiling only so a single absurdly long cell (e.g., a
    /// pasted URL) can't blow `col_widths` to thousands of columns and
    /// force the shrink to operate on garbage scale.
    const MAX_COL_WIDTH: usize = 200;

    /// Floor for any single column under the proportional-shrink pass.
    /// Below this, cells become unreadable (mid-word breaks every line);
    /// matches what Glamour / Lipgloss use as a lower bound.
    const MIN_COL_WIDTH: usize = 4;

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
        // capped by MAX_COL_WIDTH so one absurd cell can't blow scale.
        // Cells wider than their final column wrap.
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

        // Fit-to-width: when the caller advertised an `available_width`
        // and the natural table is wider, shrink columns proportionally
        // to their natural size (wider columns absorb more shrink). Each
        // column is floored at MIN_COL_WIDTH so a tiny budget can't
        // collapse columns to zero. Visual width =
        //   1 (left │) + Σ(1 + col + 2) = 1 + 3·num_cols + Σ col_widths
        let overhead = 1 + 3 * num_cols;
        if let Some(budget) = self.available_width {
            let natural_total: usize = col_widths.iter().sum::<usize>() + overhead;
            if natural_total > budget {
                shrink_columns_proportionally(&mut col_widths, budget, overhead);
            }
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

/// Shrink `col_widths` in place so `Σ widths + overhead ≤ budget`,
/// distributing the deficit proportionally to each column's current
/// width (so wider columns absorb more shrink than narrow ones). Each
/// column is floored at [`Walker::MIN_COL_WIDTH`].
///
/// Algorithm: when the unfrozen columns sum and budget allow it, scale
/// every unfrozen column by `available / unfrozen_total`. Any column
/// that would land below the floor is "frozen" at the floor and we
/// repeat the pass until either everything fits or every column is
/// frozen at the floor (in which case the table is wider than the
/// budget can support — we accept the overflow rather than emit a
/// degenerate zero-width grid).
fn shrink_columns_proportionally(col_widths: &mut [usize], budget: usize, overhead: usize) {
    let n = col_widths.len();
    if n == 0 {
        return;
    }
    let floor = Walker::MIN_COL_WIDTH;
    let min_total = overhead + n * floor;
    let target_content = budget.saturating_sub(overhead);
    if budget <= min_total {
        // Even all-floor doesn't fit — clamp to floor and accept overflow.
        for w in col_widths.iter_mut() {
            *w = floor;
        }
        return;
    }

    let mut frozen = vec![false; n];
    loop {
        let mut unfrozen_total = 0usize;
        let mut frozen_total = 0usize;
        for (i, &w) in col_widths.iter().enumerate() {
            if frozen[i] {
                frozen_total += w;
            } else {
                unfrozen_total += w;
            }
        }
        // Budget left for the unfrozen columns to share.
        let unfrozen_budget = target_content.saturating_sub(frozen_total);
        if unfrozen_total == 0 || unfrozen_total <= unfrozen_budget {
            // Nothing to shrink.
            return;
        }

        let mut any_newly_frozen = false;
        let mut new_widths = col_widths.to_vec();
        for (i, &w) in col_widths.iter().enumerate() {
            if frozen[i] {
                continue;
            }
            // Proportional share, rounded down. The leftover slack is
            // tolerated — a few columns of unused budget beats overflow.
            let scaled = w.saturating_mul(unfrozen_budget) / unfrozen_total.max(1);
            if scaled < floor {
                new_widths[i] = floor;
                frozen[i] = true;
                any_newly_frozen = true;
            } else {
                new_widths[i] = scaled.max(1);
            }
        }
        col_widths.copy_from_slice(&new_widths);
        if !any_newly_frozen {
            return;
        }
    }
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

    /// Test shim: keeps the historical 2-arg call shape working. Most
    /// tests don't care about the table-fit budget — pass `None` so the
    /// renderer falls back to natural widths.
    fn render_to_styled_chars(
        source: &str,
        theme: Option<&MarkdownTheme>,
    ) -> Vec<StyledChar> {
        super::render_to_styled_chars(source, theme, None)
    }

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
    fn gfm_table_wraps_cells_when_constrained_to_available_width() {
        // Caller advertises an 80-col budget. A cell wider than the
        // proportional share for its column wraps to multiple visual
        // lines; the other column on the same logical row pads blank so
        // the table stays aligned. No row exceeds the 80-col budget.
        let long = "the quick brown fox jumps over the lazy dog and then keeps on running for several more lines worth of words";
        let src = format!("| short | long |\n| --- | --- |\n| a | {long} |\n");
        let r = super::render_to_styled_chars(&src, None, Some(80));
        let s: String = r.iter().map(|c| c.ch).collect();
        // Body row spans more than one visual line: at least one row
        // must have its short-cell column padded blank as a
        // wrap-continuation. Find a `│ a` line and confirm at least one
        // following row has the short-cell column entirely whitespace.
        let lines: Vec<&str> = s.lines().collect();
        let body_row = lines
            .iter()
            .position(|l| l.starts_with("│ a"))
            .expect("body row with short cell `a` should exist");
        let continuation = lines
            .get(body_row + 1)
            .copied()
            .unwrap_or_default();
        // Continuation: starts with `│`, then whitespace-only short
        // column, then `│`, then more content.
        let short_col_blank = continuation
            .strip_prefix('│')
            .and_then(|tail| tail.find('│').map(|idx| (tail, idx)))
            .map(|(tail, idx)| tail[..idx].chars().all(char::is_whitespace))
            .unwrap_or(false);
        assert!(
            short_col_blank,
            "expected wrap-continuation row with blank short cell, got lines: {lines:?}"
        );
        for line in s.lines() {
            if line.starts_with('│') || line.starts_with('├') {
                let w: usize = line.chars().map(char_display_width).sum();
                assert!(w <= 80, "row {line:?} exceeded budget 80, was {w}");
            }
        }
    }

    #[test]
    fn table_shrinks_proportionally_to_fit_available_width() {
        // 5-column reference table modelled on the markdown screenshot
        // bug: per-column natural widths sum well above 80 cols once you
        // add separators. Caller advertises an 80-col budget — the
        // renderer must shrink columns proportionally to fit.
        let src = "\
| Tool | Purpose | Notes | Owner | Status |
| --- | --- | --- | --- | --- |
| read_file | reads file contents from disk | safe and idempotent | platform | stable |
| write_file | writes contents to disk path | mutating side effects | platform | stable |
| run_shell | runs a shell command on host | dangerous, sandboxed only | infra | beta |
";
        let r = super::render_to_styled_chars(src, None, Some(80));
        let s: String = r.iter().map(|c| c.ch).collect();
        for line in s.lines() {
            if line.starts_with('│') || line.starts_with('├') {
                let w: usize = line.chars().map(char_display_width).sum();
                assert!(
                    w <= 80,
                    "row exceeded available_width 80: width={w} line={line:?}"
                );
            }
        }
        // And the table must actually appear (not collapse to nothing).
        assert!(s.contains("│"), "table renderer produced no border glyphs");
        assert!(s.contains("├"), "table renderer produced no header divider");
    }

    #[test]
    fn table_falls_back_to_natural_width_when_unconstrained() {
        // No available_width → no proportional shrink; columns sit at
        // their natural width. The header row should match the historical
        // tight-padding output, confirming the no-cap behaviour.
        let src = "| Tool | Purpose |\n| --- | --- |\n| read_file | reads files |\n";
        let r = super::render_to_styled_chars(src, None, None);
        let s: String = r.iter().map(|c| c.ch).collect();
        assert!(
            s.contains("│ Tool      │ Purpose     │\n"),
            "unconstrained header should keep natural widths, got: {s:?}"
        );
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
