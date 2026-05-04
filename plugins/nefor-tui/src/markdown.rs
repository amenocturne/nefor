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
            // Tables / footnotes / metadata blocks fall through — text
            // events still fire and we surface them as plain spans.
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
