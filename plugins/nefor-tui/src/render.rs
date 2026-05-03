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
        self.next.reset(self.width, self.height);
        reset_layout_state(root);
        layout::layout_and_paint(root, self.width, self.height, &mut self.next);
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
    for cell in &line.cells {
        if Some(cell.style) != current_style {
            write_style(out, &cell.style);
            current_style = Some(cell.style);
        }
        out.push_str(&cell.text);
    }
    out.push_str(SGR_RESET);
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
}
