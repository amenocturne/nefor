//! Line-diff renderer.
//!
//! The renderer holds a rolling pair of frame buffers — `prev` (last
//! flushed) and `next` (in-progress). Each frame:
//! 1. Reset `next` to blank cells.
//! 2. Walk the instance tree via `layout::paint`, which writes into
//!    `next`.
//! 3. Compare `next` against `prev` row-by-row; emit only the dirty rows.
//! 4. Swap.
//!
//! Output is wrapped in DEC mode 2026 (synchronized output) so partial
//! frames never make it to the screen.

use crate::ansi::{
    write_move_to, write_style, CLEAR_LINE, CLEAR_SCREEN, HIDE_CURSOR, SGR_RESET, SYNC_BEGIN,
    SYNC_END,
};
use crate::desc::Style;
use crate::instance::WidgetInstance;
use crate::layout;
use crate::mouse::SelectionRange;
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    /// One grapheme cluster's text. `" "` for blanks.
    pub text: String,
    pub style: Style,
}

impl Cell {
    pub fn blank() -> Self {
        Cell {
            text: " ".into(),
            style: Style::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub cells: Vec<Cell>,
}

impl Line {
    fn blank(width: u16) -> Self {
        Line {
            cells: (0..width as usize).map(|_| Cell::blank()).collect(),
        }
    }

    fn reset(&mut self) {
        for c in &mut self.cells {
            *c = Cell::blank();
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameBuffer {
    pub lines: Vec<Line>,
}

impl FrameBuffer {
    pub fn new(width: u16, height: u16) -> Self {
        FrameBuffer {
            lines: (0..height as usize).map(|_| Line::blank(width)).collect(),
        }
    }

    fn reset(&mut self, width: u16, height: u16) {
        if self.lines.len() != height as usize
            || self.lines.first().map(|l| l.cells.len()).unwrap_or(0) != width as usize
        {
            *self = FrameBuffer::new(width, height);
            return;
        }
        for line in &mut self.lines {
            line.reset();
        }
    }
}

#[derive(Debug)]
pub struct Renderer {
    width: u16,
    height: u16,
    prev: FrameBuffer,
    next: FrameBuffer,
    needs_full: bool,
}

impl Renderer {
    pub fn new(width: u16, height: u16) -> Self {
        Renderer {
            width,
            height,
            prev: FrameBuffer::new(width, height),
            next: FrameBuffer::new(width, height),
            needs_full: true,
        }
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        if width == self.width && height == self.height {
            return;
        }
        self.width = width;
        self.height = height;
        self.prev = FrameBuffer::new(width, height);
        self.next = FrameBuffer::new(width, height);
        self.needs_full = true;
    }

    pub fn width(&self) -> u16 {
        self.width
    }
    pub fn height(&self) -> u16 {
        self.height
    }

    /// The last frame buffer painted via [`Renderer::render`]. After
    /// `render()` swaps `prev` and `next`, the just-painted frame lives
    /// in `prev`. Used by `Engine::snapshot()` for integration testing
    /// against exact visual output.
    ///
    /// Before the first render, returns an all-blank buffer of the
    /// configured size.
    pub fn last_frame(&self) -> &FrameBuffer {
        &self.prev
    }

    /// Render `root` and return the ANSI byte stream that brings the
    /// terminal up to date. Subsequent calls diff against the prior
    /// frame's contents; force a full redraw with [`Renderer::mark_full`].
    pub fn render(&mut self, root: &mut WidgetInstance) -> Vec<u8> {
        self.render_with_selection(root, None)
    }

    /// Like [`Renderer::render`] but applies `selection`'s reverse-video
    /// highlight to the cells covered by the range before emitting ANSI.
    /// Selection is engine state, not layout state — we paint the tree
    /// first, then post-process the framebuffer so existing widget paint
    /// paths stay oblivious to the selection mechanism.
    ///
    /// `selection` carries the geometric range AND the captured
    /// selectable widget's painted rect. The highlight is intersected
    /// with the rect: cells outside it stay un-highlighted even when
    /// the user drags past the widget's edge into a neighbouring panel.
    /// That's the closing half of the per-widget selection-scoping
    /// model — the COPY clamp landed in `141692f`; the visual
    /// highlight clamp lands here.
    pub fn render_with_selection(
        &mut self,
        root: &mut WidgetInstance,
        selection: Option<(SelectionRange, layout::Rect)>,
    ) -> Vec<u8> {
        self.next.reset(self.width, self.height);
        reset_layout_state(root);
        layout::layout_and_paint(root, self.width, self.height, &mut self.next);
        if let Some((range, clip)) = selection {
            apply_selection_highlight(&mut self.next, range, Some(clip));
        }
        let bytes = if self.needs_full {
            self.emit_full()
        } else {
            self.emit_diff()
        };
        std::mem::swap(&mut self.prev, &mut self.next);
        self.needs_full = false;
        bytes
    }

    /// Force the next render to emit the full buffer.
    pub fn mark_full(&mut self) {
        self.needs_full = true;
    }

    fn emit_full(&self) -> Vec<u8> {
        let mut out = String::new();
        out.push_str(SYNC_BEGIN);
        out.push_str(CLEAR_SCREEN);
        for (i, line) in self.next.lines.iter().enumerate() {
            write_move_to(&mut out, i as u16, 0);
            out.push_str(CLEAR_LINE);
            push_line(&mut out, line);
        }
        out.push_str(HIDE_CURSOR);
        out.push_str(SYNC_END);
        out.into_bytes()
    }

    fn emit_diff(&self) -> Vec<u8> {
        let mut out = String::new();
        out.push_str(SYNC_BEGIN);
        for (i, (next_line, prev_line)) in self
            .next
            .lines
            .iter()
            .zip(self.prev.lines.iter())
            .enumerate()
        {
            if next_line == prev_line {
                continue;
            }
            write_move_to(&mut out, i as u16, 0);
            out.push_str(CLEAR_LINE);
            push_line(&mut out, next_line);
        }
        out.push_str(HIDE_CURSOR);
        out.push_str(SYNC_END);
        out.into_bytes()
    }
}

/// Walk `inst` and reset every instance's `layout` cache so a fresh
/// measure pass starts clean. Layout state is not part of `InstanceState`
/// (which the reconciler preserves verbatim across rebuilds), but it
/// still lives on each instance and would otherwise leak per-frame data.
fn reset_layout_state(inst: &mut WidgetInstance) {
    inst.layout.reset();
    for c in inst.children.iter_mut() {
        reset_layout_state(c);
    }
}

fn push_line(out: &mut String, line: &Line) {
    let mut current_style: Option<Style> = None;
    let mut i = 0;
    while i < line.cells.len() {
        let cell = &line.cells[i];
        if Some(cell.style) != current_style {
            write_style(out, &cell.style);
            current_style = Some(cell.style);
        }
        out.push_str(&cell.text);
        // Wide chars (East-Asian Wide / Fullwidth / most emoji) advance
        // the terminal cursor by their display width, not by 1. The
        // painter records a wide char as one model cell with the glyph
        // plus (w - 1) trailing "spillover" blank cells (so prior-frame
        // ink doesn't bleed and neighbouring writes know the cell is
        // taken). The terminal already moves the cursor past those
        // spillover cells when it renders the wide glyph, so emitting
        // them as ASCII spaces here would push every subsequent cell on
        // the row right by (w - 1) per wide char — visible as the panel
        // divider shifting right one cell per emoji on the rows that
        // contain emoji. Skip the spillover cells in the byte stream;
        // their model state is purely a paint-side bookkeeping device.
        let w = UnicodeWidthStr::width(cell.text.as_str()).max(1);
        i += w;
    }
    out.push_str(SGR_RESET);
}

/// Walk the framebuffer cells covered by `range` and toggle their
/// `reverse` SGR bit. We use the terminal's own reverse-video so the
/// highlight stays neutral against any user theme — no engine-baked
/// colors. Cells outside the buffer are silently skipped (the renderer
/// never overdraws past `width × height`).
///
/// When `clip` is `Some(rect)`, cells outside the rect are also skipped
/// — the highlight is the geometric intersection of the selection range
/// with the captured selectable widget's painted rect. This stops a
/// drag past the widget's edge from painting reverse-video onto cells
/// that belong to a neighbouring panel (chat-into-sidebar, modal
/// overlay-into-base). With `clip = None` the legacy unclipped path
/// applies — kept available for tests of the highlight primitive in
/// isolation.
fn apply_selection_highlight(
    buf: &mut FrameBuffer,
    range: SelectionRange,
    clip: Option<layout::Rect>,
) {
    let height = buf.lines.len() as u16;
    if height == 0 {
        return;
    }
    let width = buf.lines.first().map(|l| l.cells.len() as u16).unwrap_or(0);
    for row in range.start_row..=range.end_row {
        if row >= height {
            break;
        }
        let Some((c0, c1)) = range.row_span(row, width) else {
            continue;
        };
        // Intersect the row's selection segment with the clip rect when
        // present. `rect.row + rect.height` is exclusive; rows past it
        // contribute nothing. Same shape on the column axis.
        let (c0, c1) = match clip {
            Some(rect) => {
                let row_in = row >= rect.row && row < rect.row.saturating_add(rect.height);
                if !row_in {
                    continue;
                }
                let rect_c0 = rect.col;
                let rect_c1 = rect.col.saturating_add(rect.width).saturating_sub(1);
                let lo = c0.max(rect_c0);
                let hi = c1.min(rect_c1);
                if lo > hi {
                    continue;
                }
                (lo, hi)
            }
            None => (c0, c1),
        };
        let line = &mut buf.lines[row as usize];
        for col in c0..=c1 {
            if let Some(cell) = line.cells.get_mut(col as usize) {
                cell.style.reverse = !cell.style.reverse;
            }
        }
    }
}

/// Extract plain-text from the framebuffer's cells covered by `range`.
/// Each row's cells are joined into a string, trailing whitespace is
/// trimmed per row, and rows are joined with `\n`. Empty selections
/// (range entirely outside the buffer) yield `""`.
pub fn extract_selection_text(buf: &FrameBuffer, range: SelectionRange) -> String {
    let height = buf.lines.len() as u16;
    if height == 0 {
        return String::new();
    }
    let width = buf.lines.first().map(|l| l.cells.len() as u16).unwrap_or(0);
    let mut rows: Vec<String> = Vec::new();
    for row in range.start_row..=range.end_row {
        if row >= height {
            break;
        }
        let Some((c0, c1)) = range.row_span(row, width) else {
            rows.push(String::new());
            continue;
        };
        let mut piece = String::new();
        let line = &buf.lines[row as usize];
        for col in c0..=c1 {
            if let Some(cell) = line.cells.get(col as usize) {
                piece.push_str(&cell.text);
            }
        }
        // Trim only trailing whitespace; leading spaces may be intentional
        // (e.g. indentation inside a code block).
        while piece.ends_with(' ') {
            piece.pop();
        }
        rows.push(piece);
    }
    rows.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desc::{WidgetDescription, WrapMode};
    use crate::reconciler::Reconciler;

    fn text_root(content: &str) -> WidgetDescription {
        WidgetDescription::Text {
            content: content.into(),
            style: None,
            wrap: WrapMode::Word,
            key: None,
        }
    }

    fn column(children: Vec<WidgetDescription>) -> WidgetDescription {
        WidgetDescription::Column {
            children,
            gap: 0,
            key: None,
        }
    }

    fn padding(
        child: WidgetDescription,
        top: u16,
        right: u16,
        bottom: u16,
        left: u16,
    ) -> WidgetDescription {
        WidgetDescription::Padding {
            top,
            right,
            bottom,
            left,
            child: Box::new(child),
            key: None,
        }
    }

    fn render_once(w: u16, h: u16, desc: WidgetDescription) -> (Renderer, Reconciler, String) {
        let mut rec = Reconciler::new();
        rec.reconcile(desc);
        let mut renderer = Renderer::new(w, h);
        let bytes = renderer.render(rec.root.as_mut().unwrap());
        let s = String::from_utf8(bytes).expect("ansi is utf-8");
        (renderer, rec, s)
    }

    #[test]
    fn full_frame_emits_clear_then_rows() {
        let (_r, _rec, out) = render_once(10, 2, text_root("hi"));
        assert!(out.starts_with(SYNC_BEGIN));
        assert!(out.contains(CLEAR_SCREEN));
        assert!(out.contains("hi"));
        assert!(out.ends_with(SYNC_END));
    }

    #[test]
    fn synchronized_output_wraps_emission() {
        let (_r, _rec, out) = render_once(5, 1, text_root("abc"));
        let begin = out.find(SYNC_BEGIN).unwrap();
        let end = out.rfind(SYNC_END).unwrap();
        assert!(begin < end, "begin must precede end");
    }

    #[test]
    fn diff_emits_only_changed_rows() {
        let mut rec = Reconciler::new();
        rec.reconcile(column(vec![text_root("aaa"), text_root("bbb")]));
        let mut renderer = Renderer::new(10, 3);
        let _ = renderer.render(rec.root.as_mut().unwrap());

        // Change only the second child.
        rec.reconcile(column(vec![text_root("aaa"), text_root("BBB")]));
        let bytes = renderer.render(rec.root.as_mut().unwrap());
        let out = String::from_utf8(bytes).expect("utf-8");

        // No CLEAR_SCREEN on diff frame.
        assert!(!out.contains(CLEAR_SCREEN));
        // The changed row's text appears.
        assert!(out.contains("BBB"));
        // The unchanged row's text does NOT appear.
        assert!(
            !out.contains("aaa"),
            "unchanged row should not be re-emitted"
        );
    }

    #[test]
    fn padding_offsets_text_into_inner_rect() {
        let desc = padding(text_root("hi"), 1, 0, 0, 2);
        let (_r, _rec, _out) = render_once(8, 3, desc);
        // Verified more rigorously by layout::tests::column_padding_text_positions_correctly.
    }

    #[test]
    fn resize_forces_full_redraw() {
        let mut rec = Reconciler::new();
        rec.reconcile(text_root("hello"));
        let mut renderer = Renderer::new(10, 2);
        let _ = renderer.render(rec.root.as_mut().unwrap());
        renderer.resize(20, 4);
        let bytes = renderer.render(rec.root.as_mut().unwrap());
        let out = String::from_utf8(bytes).expect("utf-8");
        assert!(out.contains(CLEAR_SCREEN), "post-resize must full-redraw");
    }

    fn make_buf(rows: &[&str], width: u16) -> FrameBuffer {
        let mut buf = FrameBuffer::new(width, rows.len() as u16);
        for (i, row) in rows.iter().enumerate() {
            for (j, ch) in row.chars().enumerate() {
                if j as u16 >= width {
                    break;
                }
                buf.lines[i].cells[j].text = ch.to_string();
            }
        }
        buf
    }

    #[test]
    fn extract_selection_text_single_row() {
        let buf = make_buf(&["hello world"], 11);
        // Cells 6..=10 → "world"
        let text = extract_selection_text(&buf, SelectionRange::normalised((6, 0), (10, 0)));
        assert_eq!(text, "world");
    }

    #[test]
    fn extract_selection_text_multi_row_line_flow() {
        // Three rows of width 8.
        let buf = make_buf(&["abcdefgh", "ijklmnop", "qrstuvwx"], 8);
        // Drag from (5, 0) to (3, 2): row 0 from col 5 → end ("fgh"),
        // row 1 full ("ijklmnop"), row 2 from start to col 3 ("qrst").
        let text = extract_selection_text(&buf, SelectionRange::normalised((5, 0), (3, 2)));
        assert_eq!(text, "fgh\nijklmnop\nqrst");
    }

    #[test]
    fn extract_selection_text_trims_trailing_spaces_per_row() {
        // Row 0 has trailing blanks past the visible chars.
        let buf = make_buf(&["hi      "], 8);
        let text = extract_selection_text(&buf, SelectionRange::normalised((0, 0), (7, 0)));
        assert_eq!(text, "hi");
    }

    /// Wide chars (East-Asian Wide / Fullwidth / most emoji) advance the
    /// terminal's cursor by their display width — so a row of model
    /// width N that contains a wide char must NOT emit a trailing
    /// "spillover" cell as a literal space, or the terminal runs the
    /// cursor one column past the row's intended right edge per wide
    /// char. Visible failure mode: the panel divider (next column over)
    /// looked one cell to the right on every emoji-bearing row, jagging
    /// the otherwise-flush vertical line. Fix is in `push_line`: walk
    /// cells stepping by display width instead of by 1.
    #[test]
    fn wide_char_does_not_emit_extra_space_into_byte_stream() {
        let (_r, _rec, out) = render_once(10, 1, text_root("ab📄cd"));
        // The cell payload run is `ab📄cd    ` — emoji counts as one
        // visible char that the terminal expands to 2 cells. The earlier
        // shape emitted `ab📄 cd    ` (extra space after the emoji),
        // which terminals render as 11 visible cells in a 10-wide row.
        assert!(
            out.contains("ab📄cd    "),
            "expected emoji + 4 trailing pads, got: {out:?}"
        );
        assert!(
            !out.contains("ab📄 cd"),
            "spillover blank must not be emitted as a literal space"
        );
    }

    #[test]
    fn apply_selection_highlight_toggles_reverse() {
        let mut buf = make_buf(&["abcdef"], 6);
        apply_selection_highlight(&mut buf, SelectionRange::normalised((1, 0), (3, 0)), None);
        assert!(!buf.lines[0].cells[0].style.reverse);
        assert!(buf.lines[0].cells[1].style.reverse);
        assert!(buf.lines[0].cells[2].style.reverse);
        assert!(buf.lines[0].cells[3].style.reverse);
        assert!(!buf.lines[0].cells[4].style.reverse);
    }

    #[test]
    fn apply_selection_highlight_clips_to_rect_columns() {
        // 6-cell row "abcdef" — geometric range covers cols 1..=5 but
        // the clip rect is cols 0..=2 wide. Only cells 1..=2 should
        // flip; cells 3..=5 stay un-reversed.
        let mut buf = make_buf(&["abcdef"], 6);
        let clip = layout::Rect {
            row: 0,
            col: 0,
            width: 3, // cols 0..=2
            height: 1,
        };
        apply_selection_highlight(
            &mut buf,
            SelectionRange::normalised((1, 0), (5, 0)),
            Some(clip),
        );
        assert!(!buf.lines[0].cells[0].style.reverse);
        assert!(buf.lines[0].cells[1].style.reverse);
        assert!(buf.lines[0].cells[2].style.reverse);
        assert!(
            !buf.lines[0].cells[3].style.reverse,
            "col 3 is past clip rect right edge — must stay unreversed"
        );
        assert!(!buf.lines[0].cells[4].style.reverse);
        assert!(!buf.lines[0].cells[5].style.reverse);
    }

    #[test]
    fn apply_selection_highlight_clips_to_rect_rows() {
        // Two rows: row 0 inside clip, row 1 outside.
        let mut buf = make_buf(&["abcdef", "ghijkl"], 6);
        let clip = layout::Rect {
            row: 0,
            col: 0,
            width: 6,
            height: 1, // rows 0..=0
        };
        apply_selection_highlight(
            &mut buf,
            SelectionRange::normalised((0, 0), (5, 1)),
            Some(clip),
        );
        // Row 0 cells should be reversed (range covers full row 0).
        for col in 0..6 {
            assert!(
                buf.lines[0].cells[col].style.reverse,
                "row 0 col {col} should be reversed"
            );
        }
        // Row 1 outside clip — must stay un-reversed.
        for col in 0..6 {
            assert!(
                !buf.lines[1].cells[col].style.reverse,
                "row 1 col {col} is past clip rect — must stay unreversed"
            );
        }
    }
}
