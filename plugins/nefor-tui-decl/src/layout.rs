//! Phase-1 layout for the three primitives.
//!
//! Top-down recursive sizing. Each call `paint` produces lines into the
//! shared frame buffer at the supplied origin, clipped to the supplied
//! width × height. Phase 2 will replace this with the constraints-down /
//! sizes-up two-pass algorithm; phase 1 only needs to position
//! `column { padding { text } }` correctly.
//!
//! Width is measured in columns, one column per ASCII char. Wide-char
//! correctness via `unicode-width` lands in phase 2 (documented gap).

use unicode_width::UnicodeWidthChar;

use crate::desc::{Style, WidgetDescription, WrapMode};
use crate::instance::WidgetInstance;
use crate::render::{Cell, FrameBuffer};

/// Paint `inst` into `buf` at `origin = (row, col)`, clipped to
/// `(width, height)`. Cells outside the clip are dropped silently. Cells
/// inside the clip but not produced by the primitive are left as the
/// buffer's existing content (the renderer initializes the buffer to
/// blank cells before each frame).
pub fn paint(
    inst: &WidgetInstance,
    width: u16,
    height: u16,
    origin: (u16, u16),
    buf: &mut FrameBuffer,
) {
    if width == 0 || height == 0 {
        return;
    }
    match &inst.last_desc {
        WidgetDescription::Text {
            content,
            style,
            wrap,
            ..
        } => paint_text(content, style, *wrap, width, height, origin, buf),
        WidgetDescription::Column { gap, .. } => {
            paint_column(&inst.children, *gap, width, height, origin, buf);
        }
        WidgetDescription::Padding {
            top,
            right,
            bottom,
            left,
            ..
        } => paint_padding(
            inst, *top, *right, *bottom, *left, width, height, origin, buf,
        ),
    }
}

fn paint_text(
    content: &str,
    style: &Option<Style>,
    wrap: WrapMode,
    width: u16,
    height: u16,
    origin: (u16, u16),
    buf: &mut FrameBuffer,
) {
    let s = style.unwrap_or_default();
    let rows = wrap_text(content, width, wrap);
    let (origin_row, origin_col) = origin;
    for (i, line) in rows.into_iter().enumerate() {
        if i as u16 >= height {
            break;
        }
        let row = origin_row.saturating_add(i as u16);
        write_run(buf, row, origin_col, &line, &s);
    }
}

fn paint_column(
    children: &[WidgetInstance],
    gap: u16,
    width: u16,
    height: u16,
    origin: (u16, u16),
    buf: &mut FrameBuffer,
) {
    let mut cursor_row = origin.0;
    let max_row = origin.0.saturating_add(height);
    for (idx, child) in children.iter().enumerate() {
        if cursor_row >= max_row {
            break;
        }
        let remaining_height = max_row.saturating_sub(cursor_row);
        let child_height = measure_height(child, width).min(remaining_height);
        if child_height == 0 {
            continue;
        }
        paint(child, width, child_height, (cursor_row, origin.1), buf);
        cursor_row = cursor_row.saturating_add(child_height);
        if idx + 1 < children.len() {
            cursor_row = cursor_row.saturating_add(gap);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_padding(
    inst: &WidgetInstance,
    top: u16,
    right: u16,
    bottom: u16,
    left: u16,
    width: u16,
    height: u16,
    origin: (u16, u16),
    buf: &mut FrameBuffer,
) {
    let inner_w = width.saturating_sub(left.saturating_add(right));
    let inner_h = height.saturating_sub(top.saturating_add(bottom));
    if inner_w == 0 || inner_h == 0 {
        return;
    }
    if let Some(child) = inst.children.first() {
        let inner_origin = (origin.0.saturating_add(top), origin.1.saturating_add(left));
        paint(child, inner_w, inner_h, inner_origin, buf);
    }
}

/// Estimate the row count `inst` will consume at `width`. Phase 1 keeps
/// this simple: text computes wrap rows; padding adds vertical padding
/// to its child's measured height; column sums children plus gaps. The
/// renderer is the source of truth — these are best-effort hints used by
/// `paint_column` to slice the available height fairly.
fn measure_height(inst: &WidgetInstance, width: u16) -> u16 {
    if width == 0 {
        return 0;
    }
    match &inst.last_desc {
        WidgetDescription::Text { content, wrap, .. } => {
            wrap_text(content, width, *wrap).len() as u16
        }
        WidgetDescription::Column { gap, .. } => {
            let n = inst.children.len();
            if n == 0 {
                return 0;
            }
            let mut total: u16 = 0;
            for child in &inst.children {
                total = total.saturating_add(measure_height(child, width));
            }
            let gaps = (n as u16).saturating_sub(1).saturating_mul(*gap);
            total.saturating_add(gaps)
        }
        WidgetDescription::Padding { top, bottom, .. } => {
            let inner_w = width.saturating_sub(
                padding_horizontal(inst.last_desc.padding_left())
                    .saturating_add(padding_horizontal(inst.last_desc.padding_right())),
            );
            let child_h = inst
                .children
                .first()
                .map(|c| measure_height(c, inner_w))
                .unwrap_or(0);
            top.saturating_add(*bottom).saturating_add(child_h)
        }
    }
}

fn padding_horizontal(v: Option<u16>) -> u16 {
    v.unwrap_or(0)
}

/// Word-wrap `content` to fit in `width` columns. Newlines in source are
/// hard breaks; returned vec elements never include a trailing newline.
pub fn wrap_text(content: &str, width: u16, wrap: WrapMode) -> Vec<String> {
    if width == 0 {
        return vec![];
    }
    let mut out: Vec<String> = Vec::new();
    for raw_line in content.split('\n') {
        match wrap {
            WrapMode::None => {
                let truncated: String = take_columns(raw_line, width);
                out.push(truncated);
            }
            WrapMode::Char => out.extend(wrap_char(raw_line, width)),
            WrapMode::Word => out.extend(wrap_word(raw_line, width)),
        }
    }
    out
}

fn take_columns(s: &str, width: u16) -> String {
    let limit = width as usize;
    let mut taken = 0usize;
    let mut out = String::new();
    for ch in s.chars() {
        let w = char_width(ch);
        if taken + w > limit {
            break;
        }
        out.push(ch);
        taken += w;
    }
    out
}

fn wrap_char(line: &str, width: u16) -> Vec<String> {
    let limit = width as usize;
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut col = 0usize;
    for ch in line.chars() {
        let w = char_width(ch);
        if col + w > limit && !current.is_empty() {
            out.push(std::mem::take(&mut current));
            col = 0;
        }
        if w > limit {
            // Single grapheme wider than the line; emit on its own row.
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            current.push(ch);
            out.push(std::mem::take(&mut current));
            col = 0;
            continue;
        }
        current.push(ch);
        col += w;
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn wrap_word(line: &str, width: u16) -> Vec<String> {
    let limit = width as usize;
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut col = 0usize;
    for word in split_keeping_spaces(line) {
        let ww = string_width(word);
        if col == 0 && ww > limit {
            // Word longer than the line — char-wrap it on its own row(s).
            for sub in wrap_char(word, width) {
                out.push(sub);
            }
            current.clear();
            col = 0;
            continue;
        }
        if col + ww > limit {
            out.push(std::mem::take(&mut current));
            col = 0;
            // Skip pure-whitespace words at line starts to avoid leaving
            // a trailing-space artefact at the start of the new line.
            if word.chars().all(char::is_whitespace) {
                continue;
            }
        }
        current.push_str(word);
        col += ww;
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Split `s` into runs that alternate whitespace and non-whitespace.
fn split_keeping_spaces(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut in_space = s.starts_with(char::is_whitespace);
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
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

fn string_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

fn char_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

fn write_run(buf: &mut FrameBuffer, row: u16, col_start: u16, text: &str, style: &Style) {
    let row_idx = row as usize;
    if row_idx >= buf.lines.len() {
        return;
    }
    let line = &mut buf.lines[row_idx];
    let mut col = col_start as usize;
    for ch in text.chars() {
        let w = char_width(ch);
        if w == 0 {
            continue;
        }
        if col >= line.cells.len() {
            break;
        }
        let mut s = String::new();
        s.push(ch);
        line.cells[col] = Cell {
            text: s,
            style: *style,
        };
        col += w;
        // Wide chars: blank out the trailing cell(s) to avoid duplicating
        // ink. Phase 1 is ASCII-only so this branch is dead; landed for
        // forward compatibility.
        for _ in 1..w {
            if col >= line.cells.len() {
                break;
            }
            line.cells[col] = Cell::blank();
            col += 1;
        }
    }
}

// Convenience accessors used by `measure_height`.
trait DescPaddingExt {
    fn padding_left(&self) -> Option<u16>;
    fn padding_right(&self) -> Option<u16>;
}

impl DescPaddingExt for WidgetDescription {
    fn padding_left(&self) -> Option<u16> {
        if let WidgetDescription::Padding { left, .. } = self {
            Some(*left)
        } else {
            None
        }
    }
    fn padding_right(&self) -> Option<u16> {
        if let WidgetDescription::Padding { right, .. } = self {
            Some(*right)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desc::WidgetDescription;
    use crate::reconciler::Reconciler;
    use crate::render::FrameBuffer;

    fn text(content: &str) -> WidgetDescription {
        WidgetDescription::Text {
            content: content.into(),
            style: None,
            wrap: WrapMode::Word,
            key: None,
        }
    }

    fn cell_at(buf: &FrameBuffer, row: usize, col: usize) -> &str {
        buf.lines[row].cells[col].text.as_str()
    }

    #[test]
    fn column_padding_text_positions_correctly() {
        // Goal: top=1, left=2 padding around a "hi" text inside a single-
        // child column. Renderer width 8, height 3. The "h" should land at
        // (row=1, col=2).
        let desc = WidgetDescription::Column {
            gap: 0,
            key: None,
            children: vec![WidgetDescription::Padding {
                top: 1,
                right: 0,
                bottom: 0,
                left: 2,
                child: Box::new(text("hi")),
                key: None,
            }],
        };
        let mut rec = Reconciler::new();
        rec.reconcile(desc);
        let root = rec.root.as_ref().unwrap();

        let mut buf = FrameBuffer::new(8, 3);
        paint(root, 8, 3, (0, 0), &mut buf);

        assert_eq!(cell_at(&buf, 0, 0), " ", "top-left should be padding");
        assert_eq!(cell_at(&buf, 1, 0), " ", "left padding column is blank");
        assert_eq!(cell_at(&buf, 1, 1), " ", "left padding column is blank");
        assert_eq!(cell_at(&buf, 1, 2), "h", "h at offset (1, 2)");
        assert_eq!(cell_at(&buf, 1, 3), "i", "i follows");
        assert_eq!(cell_at(&buf, 1, 4), " ", "remaining row blank");
    }

    #[test]
    fn column_with_gap_inserts_blank_row() {
        let desc = WidgetDescription::Column {
            gap: 1,
            key: None,
            children: vec![text("a"), text("b")],
        };
        let mut rec = Reconciler::new();
        rec.reconcile(desc);
        let root = rec.root.as_ref().unwrap();

        let mut buf = FrameBuffer::new(4, 4);
        paint(root, 4, 4, (0, 0), &mut buf);
        assert_eq!(cell_at(&buf, 0, 0), "a");
        assert_eq!(cell_at(&buf, 1, 0), " ", "gap row");
        assert_eq!(cell_at(&buf, 2, 0), "b");
    }

    #[test]
    fn wrap_word_splits_on_word_boundary() {
        let rows = wrap_text("hello world", 6, WrapMode::Word);
        assert_eq!(rows, vec!["hello ".to_string(), "world".to_string()]);
    }

    #[test]
    fn wrap_word_keeps_intact_when_fits() {
        let rows = wrap_text("hello", 10, WrapMode::Word);
        assert_eq!(rows, vec!["hello".to_string()]);
    }

    #[test]
    fn wrap_char_breaks_anywhere() {
        let rows = wrap_text("abcdefg", 3, WrapMode::Char);
        assert_eq!(rows, vec!["abc", "def", "g"]);
    }

    #[test]
    fn wrap_none_truncates() {
        let rows = wrap_text("abcdefghij", 4, WrapMode::None);
        assert_eq!(rows, vec!["abcd"]);
    }

    #[test]
    fn explicit_newline_breaks_line() {
        let rows = wrap_text("a\nb", 10, WrapMode::Word);
        assert_eq!(rows, vec!["a", "b"]);
    }
}
