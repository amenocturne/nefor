//! Cell model, highlight table, and grid-mutation application.
//!
//! This module owns the pure render state: what ends up on screen. Engine
//! events are applied via the `apply_*` functions; the render module walks
//! the resulting buffer each `flush`.
//!
//! All grid sizes are u16 at the ratatui boundary; the spec uses u32 for
//! grid event fields, so we narrow with saturation at the apply layer.

use std::collections::HashMap;

use unicode_width::UnicodeWidthStr;

/// A single terminal cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    /// The grapheme cluster rendered at this position. Empty string marks
    /// a continuation cell for a double-width glyph occupying the
    /// preceding column.
    pub text: String,
    /// Index into the highlight table ([`HlTable`]). `0` means "default".
    pub hl_id: u32,
}

impl Cell {
    /// Default blank cell.
    pub fn blank() -> Self {
        Self {
            text: " ".into(),
            hl_id: 0,
        }
    }

    /// Cell marking continuation of a wide glyph in the preceding column.
    pub fn continuation() -> Self {
        Self {
            text: String::new(),
            hl_id: 0,
        }
    }
}

/// RGB triple plus boolean attributes. Absent color fields mean "inherit
/// from `default_colors`".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HlAttr {
    /// Foreground color packed as `0x00RRGGBB`. `None` → default fg.
    pub fg: Option<u32>,
    /// Background color packed as `0x00RRGGBB`. `None` → default bg.
    pub bg: Option<u32>,
    /// Special color (underline / undercurl) packed as `0x00RRGGBB`.
    pub sp: Option<u32>,
    /// Bold.
    pub bold: bool,
    /// Italic.
    pub italic: bool,
    /// Underline.
    pub underline: bool,
    /// Swap fg/bg at render time.
    pub reverse: bool,
}

/// The engine-defined highlight palette (keyed by `hl_id`).
///
/// `id: 0` is the implicit default and does not need an explicit define —
/// its colors come from [`DefaultColors`].
#[derive(Debug, Clone, Default)]
pub struct HlTable {
    attrs: HashMap<u32, HlAttr>,
}

impl HlTable {
    /// Empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or overwrite an attribute entry.
    pub fn define(&mut self, id: u32, attr: HlAttr) {
        self.attrs.insert(id, attr);
    }

    /// Look up an entry. `id == 0` returns the zero-value [`HlAttr`] so
    /// that default colors flow through from [`DefaultColors`].
    pub fn get(&self, id: u32) -> HlAttr {
        self.attrs.get(&id).copied().unwrap_or_default()
    }
}

/// Global default colors (engine-broadcast `default_colors`).
///
/// Each field is optional: `None` means "use the terminal's native default
/// for this channel". Publishers that want to theme the frame send concrete
/// values; publishers that want to blend with the user's terminal can omit
/// any combination of fields (or skip the `default_colors` event entirely).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DefaultColors {
    /// Default foreground (packed `0x00RRGGBB`). `None` → terminal default.
    pub fg: Option<u32>,
    /// Default background. `None` → terminal default (transparent).
    pub bg: Option<u32>,
    /// Default special color. `None` → terminal default.
    pub sp: Option<u32>,
}

/// Grid-1 cell buffer.
#[derive(Debug, Clone)]
pub struct Grid {
    width: u16,
    height: u16,
    /// Row-major, `width * height` cells. Resized on `apply_resize`.
    cells: Vec<Cell>,
    /// Last cursor position (row, col). Out-of-bounds values are clamped
    /// when the renderer places the hardware cursor.
    cursor: (u16, u16),
}

impl Default for Grid {
    fn default() -> Self {
        Self::new(80, 24)
    }
}

impl Grid {
    /// Construct an empty grid of the given size.
    pub fn new(width: u16, height: u16) -> Self {
        let (w, h) = (width.max(1), height.max(1));
        Self {
            width: w,
            height: h,
            cells: vec![Cell::blank(); usize::from(w) * usize::from(h)],
            cursor: (0, 0),
        }
    }

    /// Current width (cols).
    pub fn width(&self) -> u16 {
        self.width
    }

    /// Current height (rows).
    pub fn height(&self) -> u16 {
        self.height
    }

    /// Borrowed access to a specific row's cells, clipped to the row range.
    pub fn row(&self, row: u16) -> &[Cell] {
        let r = usize::from(row);
        let w = usize::from(self.width);
        let start = r * w;
        let end = start + w;
        &self.cells[start..end]
    }

    /// Current cursor position. Clamped to `(height-1, width-1)` on read.
    pub fn cursor(&self) -> (u16, u16) {
        let (r, c) = self.cursor;
        (
            r.min(self.height.saturating_sub(1)),
            c.min(self.width.saturating_sub(1)),
        )
    }

    /// Overwrite grid size, re-allocating cells and clearing content. This
    /// mirrors Neovim's `grid_resize` semantics.
    pub fn apply_resize(&mut self, width: u32, height: u32) {
        let w = narrow_u16(width).max(1);
        let h = narrow_u16(height).max(1);
        self.width = w;
        self.height = h;
        self.cells = vec![Cell::blank(); usize::from(w) * usize::from(h)];
        self.cursor = (0, 0);
    }

    /// Blank every cell.
    pub fn apply_clear(&mut self) {
        for c in self.cells.iter_mut() {
            *c = Cell::blank();
        }
    }

    /// Place the cursor.
    pub fn apply_cursor_goto(&mut self, row: u32, col: u32) {
        self.cursor = (narrow_u16(row), narrow_u16(col));
    }

    /// Apply a `grid_line` event. `cells` is the run-length-encoded list
    /// from the event: each triple is `(text, hl_id?, repeat?)`. `repeat`
    /// defaults to 1; `hl_id` repeats the previous one when absent.
    pub fn apply_line(&mut self, row: u32, col_start: u32, cells: &[LineCell]) {
        let r = narrow_u16(row);
        if r >= self.height {
            return;
        }
        let mut col = narrow_u16(col_start);
        let mut last_hl: u32 = 0;
        for piece in cells {
            let hl = piece.hl_id.unwrap_or(last_hl);
            last_hl = hl;
            let repeat = piece.repeat.unwrap_or(1).max(1);
            let text = piece.text.as_str();

            // Width-aware placement: we treat each grapheme-less unit as
            // one logical cell with its display width. For MVP we rely on
            // unicode-width's string-level measure — sufficient for ASCII,
            // CJK, and common emoji. A follow-up can introduce proper
            // grapheme segmentation if the chat plugin emits zwj clusters.
            let width = UnicodeWidthStr::width(text).max(1) as u16;

            for _ in 0..repeat {
                if col >= self.width {
                    break;
                }
                let idx = usize::from(r) * usize::from(self.width) + usize::from(col);
                if let Some(slot) = self.cells.get_mut(idx) {
                    *slot = Cell {
                        text: text.to_owned(),
                        hl_id: hl,
                    };
                }
                // For double-width (or wider) glyphs, mark following
                // columns as continuation so the renderer doesn't
                // double-draw.
                for extra in 1..width {
                    let next_col = col + extra;
                    if next_col >= self.width {
                        break;
                    }
                    let j = usize::from(r) * usize::from(self.width) + usize::from(next_col);
                    if let Some(slot) = self.cells.get_mut(j) {
                        *slot = Cell::continuation();
                    }
                }
                col = col.saturating_add(width);
            }
        }
    }

    /// Apply a `grid_scroll`. Positive `rows` moves content up (rows at
    /// `top` vanish, new blank rows appear at `bot-1`), matching nvim
    /// semantics. `top` is inclusive, `bot` is exclusive.
    pub fn apply_scroll(&mut self, top: u32, bot: u32, rows: i32) {
        let top = narrow_u16(top);
        let bot = narrow_u16(bot).min(self.height);
        if top >= bot {
            return;
        }
        let height = bot - top;
        if rows == 0 || rows.unsigned_abs() as u16 >= height {
            // Whole-region clear.
            self.clear_region(top, bot);
            return;
        }
        let n = rows.unsigned_abs() as u16;
        if rows > 0 {
            // Move content up.
            for r in top..(bot - n) {
                self.move_row(r + n, r);
            }
            self.clear_region(bot - n, bot);
        } else {
            // Move content down. Iterate from bottom so we don't clobber
            // source rows before copying.
            let mut r = bot;
            while r > top + n {
                r -= 1;
                self.move_row(r - n, r);
            }
            self.clear_region(top, top + n);
        }
    }

    fn move_row(&mut self, src_row: u16, dst_row: u16) {
        let w = usize::from(self.width);
        let src = usize::from(src_row) * w;
        let dst = usize::from(dst_row) * w;
        // Clone each cell individually. `copy_within` would be cheaper
        // but requires Copy; our Cell holds a String.
        for i in 0..w {
            self.cells[dst + i] = self.cells[src + i].clone();
        }
    }

    fn clear_region(&mut self, top: u16, bot: u16) {
        let w = usize::from(self.width);
        for r in top..bot {
            let start = usize::from(r) * w;
            let end = start + w;
            for c in &mut self.cells[start..end] {
                *c = Cell::blank();
            }
        }
    }
}

fn narrow_u16(v: u32) -> u16 {
    v.min(u32::from(u16::MAX)) as u16
}

/// One run-length-encoded cell from a `grid_line` event.
#[derive(Debug, Clone)]
pub struct LineCell {
    /// Text (typically a single grapheme).
    pub text: String,
    /// Highlight id for the cell. `None` reuses the prior cell's id.
    pub hl_id: Option<u32>,
    /// Repeat count; defaults to 1.
    pub repeat: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(text: &str, hl: u32) -> Cell {
        Cell {
            text: text.into(),
            hl_id: hl,
        }
    }

    #[test]
    fn resize_clears_and_resizes() {
        let mut g = Grid::new(4, 2);
        g.apply_line(
            0,
            0,
            &[LineCell {
                text: "x".into(),
                hl_id: Some(1),
                repeat: Some(4),
            }],
        );
        g.apply_resize(2, 3);
        assert_eq!(g.width(), 2);
        assert_eq!(g.height(), 3);
        for r in 0..g.height() {
            for c in g.row(r) {
                assert_eq!(c.text, " ");
                assert_eq!(c.hl_id, 0);
            }
        }
    }

    #[test]
    fn line_applies_repeat_and_hl_inheritance() {
        let mut g = Grid::new(5, 1);
        g.apply_line(
            0,
            0,
            &[
                LineCell {
                    text: "a".into(),
                    hl_id: Some(2),
                    repeat: Some(3),
                },
                // Missing hl_id reuses 2.
                LineCell {
                    text: "b".into(),
                    hl_id: None,
                    repeat: Some(2),
                },
            ],
        );
        let row: Vec<_> = g.row(0).to_vec();
        assert_eq!(
            row,
            vec![
                cell("a", 2),
                cell("a", 2),
                cell("a", 2),
                cell("b", 2),
                cell("b", 2),
            ]
        );
    }

    #[test]
    fn line_respects_row_col_bounds() {
        let mut g = Grid::new(3, 2);
        g.apply_line(
            5, // out of bounds — should be dropped
            0,
            &[LineCell {
                text: "z".into(),
                hl_id: Some(1),
                repeat: Some(1),
            }],
        );
        for r in 0..g.height() {
            for c in g.row(r) {
                assert_eq!(c.text, " ");
            }
        }
    }

    #[test]
    fn cursor_clamps_to_bounds() {
        let mut g = Grid::new(3, 2);
        g.apply_cursor_goto(10, 10);
        assert_eq!(g.cursor(), (1, 2));
    }

    #[test]
    fn clear_blanks_every_cell() {
        let mut g = Grid::new(2, 2);
        g.apply_line(
            0,
            0,
            &[LineCell {
                text: "x".into(),
                hl_id: Some(1),
                repeat: Some(4),
            }],
        );
        g.apply_clear();
        for r in 0..g.height() {
            for c in g.row(r) {
                assert_eq!(c.text, " ");
                assert_eq!(c.hl_id, 0);
            }
        }
    }

    #[test]
    fn scroll_up_moves_content_and_clears_tail() {
        let mut g = Grid::new(2, 3);
        // row 0: "a"; row 1: "b"; row 2: "c"
        g.apply_line(
            0,
            0,
            &[LineCell {
                text: "a".into(),
                hl_id: Some(1),
                repeat: Some(2),
            }],
        );
        g.apply_line(
            1,
            0,
            &[LineCell {
                text: "b".into(),
                hl_id: Some(2),
                repeat: Some(2),
            }],
        );
        g.apply_line(
            2,
            0,
            &[LineCell {
                text: "c".into(),
                hl_id: Some(3),
                repeat: Some(2),
            }],
        );
        g.apply_scroll(0, 3, 1);
        // row 0 = old row 1, row 1 = old row 2, row 2 = blank
        assert_eq!(g.row(0)[0].text, "b");
        assert_eq!(g.row(1)[0].text, "c");
        assert_eq!(g.row(2)[0].text, " ");
    }

    #[test]
    fn scroll_down_moves_content() {
        let mut g = Grid::new(2, 3);
        g.apply_line(
            0,
            0,
            &[LineCell {
                text: "a".into(),
                hl_id: Some(1),
                repeat: Some(2),
            }],
        );
        g.apply_line(
            1,
            0,
            &[LineCell {
                text: "b".into(),
                hl_id: Some(2),
                repeat: Some(2),
            }],
        );
        g.apply_line(
            2,
            0,
            &[LineCell {
                text: "c".into(),
                hl_id: Some(3),
                repeat: Some(2),
            }],
        );
        g.apply_scroll(0, 3, -1);
        assert_eq!(g.row(0)[0].text, " ");
        assert_eq!(g.row(1)[0].text, "a");
        assert_eq!(g.row(2)[0].text, "b");
    }

    #[test]
    fn scroll_larger_than_region_clears() {
        let mut g = Grid::new(2, 3);
        g.apply_line(
            0,
            0,
            &[LineCell {
                text: "a".into(),
                hl_id: Some(1),
                repeat: Some(2),
            }],
        );
        g.apply_scroll(0, 3, 10);
        for r in 0..g.height() {
            for c in g.row(r) {
                assert_eq!(c.text, " ");
            }
        }
    }

    #[test]
    fn hl_table_default_id_is_zero_attr() {
        let t = HlTable::new();
        assert_eq!(t.get(0), HlAttr::default());
        assert_eq!(t.get(42), HlAttr::default());
    }

    #[test]
    fn hl_table_define_overrides() {
        let mut t = HlTable::new();
        t.define(
            1,
            HlAttr {
                fg: Some(0x00FF00FF),
                bold: true,
                ..HlAttr::default()
            },
        );
        let a = t.get(1);
        assert_eq!(a.fg, Some(0x00FF00FF));
        assert!(a.bold);
    }

    #[test]
    fn wide_glyph_writes_continuation() {
        let mut g = Grid::new(4, 1);
        g.apply_line(
            0,
            0,
            &[LineCell {
                text: "漢".into(),
                hl_id: Some(1),
                repeat: Some(1),
            }],
        );
        assert_eq!(g.row(0)[0].text, "漢");
        assert_eq!(g.row(0)[1].text, ""); // continuation
    }
}
