//! Ratatui frame rendering. Translates the cell model into buffer writes
//! and positions the hardware cursor.
//!
//! Kept deliberately dumb: the frontend doesn't decide what to draw, it
//! only replays the engine's grid state. The only cleverness is
//! highlight-attribute lookup and wide-glyph handling (continuation cells
//! produced by [`crate::grid::Grid::apply_line`] are not redrawn).

use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::Frame;

use crate::grid::{DefaultColors, Grid, HlAttr, HlTable};
use crate::selection::{col_range_for_row, Selection};
use crate::HL_SELECTION;

/// Draw the grid and position the hardware cursor.
///
/// `selection`, when present, overlays HL_SELECTION on the cells inside the
/// selection rect — the canonical grid is left intact (a follow-up render
/// without a selection should restore the original highlights), so the
/// overlay is computed at draw time per cell.
pub fn draw(
    frame: &mut Frame<'_>,
    grid: &Grid,
    hl: &HlTable,
    defaults: &DefaultColors,
    selection: Option<&Selection>,
) {
    let area = frame.area();
    let buf = frame.buffer_mut();

    let rows = grid.height().min(area.height);
    let cols = grid.width().min(area.width);

    // Pre-compute the selection rect once so the per-cell overlay check is a
    // simple range comparison rather than a function call per cell.
    let sel_rect = selection.map(|s| s.normalized());

    for r in 0..rows {
        let row_cells = grid.row(r);
        // For each visible row, work out the column band (if any) that falls
        // inside the selection rect. Returned as `[start, end)`.
        let row_sel_band = sel_rect.and_then(|(top, left, bottom, right)| {
            col_range_for_row(r, top, left, bottom, right, cols)
        });
        let mut c: u16 = 0;
        while c < cols {
            let cell = &row_cells[usize::from(c)];
            if cell.text.is_empty() {
                // Continuation column; previous iteration already drew
                // the wide glyph that claims this cell.
                c += 1;
                continue;
            }
            // Pick the active hl: HL_SELECTION inside the selection band,
            // the cell's own highlight otherwise. This is a draw-time
            // overlay only — `cell.hl_id` is not mutated.
            let in_selection = row_sel_band
                .map(|(start, end)| c >= start && c < end)
                .unwrap_or(false);
            let attr = if in_selection {
                hl.get(HL_SELECTION)
            } else {
                hl.get(cell.hl_id)
            };
            let style = attr_to_style(attr, defaults);
            let x = area.x + c;
            let y = area.y + r;
            if !inside(area, x, y) {
                break;
            }
            // Ratatui 0.30: cell mutation via `buf[(x, y)]`.
            let bcell = &mut buf[(x, y)];
            bcell.set_symbol(&cell.text);
            bcell.set_style(style);

            // Advance by glyph width. The continuation-cell convention
            // from Grid means we can always step forward by 1; the next
            // iteration will skip the empty continuation.
            c += 1;
        }
    }

    // Hardware cursor placement. Grid::cursor clamps to bounds.
    let (cr, cc) = grid.cursor();
    let cx = area.x.saturating_add(cc);
    let cy = area.y.saturating_add(cr);
    if inside(area, cx, cy) {
        frame.set_cursor_position(Position::new(cx, cy));
    }
}

fn inside(area: Rect, x: u16, y: u16) -> bool {
    x >= area.x && x < area.x + area.width && y >= area.y && y < area.y + area.height
}

fn attr_to_style(attr: HlAttr, defaults: &DefaultColors) -> Style {
    // `None` at both the attr and defaults layer → terminal default
    // (Color::Reset). That's how a publisher opts into the user's theme
    // without hardcoding an RGB value.
    let fg = attr.fg.or(defaults.fg);
    let bg = attr.bg.or(defaults.bg);
    let (fg, bg) = if attr.reverse { (bg, fg) } else { (fg, bg) };
    let mut style = Style::default()
        .fg(fg.map_or(Color::Reset, rgb_to_color))
        .bg(bg.map_or(Color::Reset, rgb_to_color));
    let mut modifier = Modifier::empty();
    if attr.bold {
        modifier |= Modifier::BOLD;
    }
    if attr.italic {
        modifier |= Modifier::ITALIC;
    }
    if attr.underline {
        modifier |= Modifier::UNDERLINED;
    }
    if !modifier.is_empty() {
        style = style.add_modifier(modifier);
    }
    style
}

fn rgb_to_color(packed: u32) -> Color {
    let r = ((packed >> 16) & 0xFF) as u8;
    let g = ((packed >> 8) & 0xFF) as u8;
    let b = (packed & 0xFF) as u8;
    Color::Rgb(r, g, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_unpacks_to_components() {
        assert_eq!(rgb_to_color(0x00FF8000), Color::Rgb(0xFF, 0x80, 0x00));
        assert_eq!(rgb_to_color(0x00000000), Color::Rgb(0, 0, 0));
        assert_eq!(rgb_to_color(0x00FFFFFF), Color::Rgb(0xFF, 0xFF, 0xFF));
    }

    #[test]
    fn attr_defaults_to_default_colors() {
        let d = DefaultColors {
            fg: Some(0x00AAAAAA),
            bg: Some(0x00111111),
            sp: None,
        };
        let s = attr_to_style(HlAttr::default(), &d);
        assert_eq!(s.fg, Some(Color::Rgb(0xAA, 0xAA, 0xAA)));
        assert_eq!(s.bg, Some(Color::Rgb(0x11, 0x11, 0x11)));
        assert!(s.add_modifier.is_empty());
    }

    #[test]
    fn empty_defaults_render_as_terminal_reset() {
        // No default_colors event + no attr override → Color::Reset so the
        // terminal's native theme shows through.
        let d = DefaultColors::default();
        let s = attr_to_style(HlAttr::default(), &d);
        assert_eq!(s.fg, Some(Color::Reset));
        assert_eq!(s.bg, Some(Color::Reset));
    }

    #[test]
    fn reverse_swaps_fg_and_bg() {
        let d = DefaultColors {
            fg: Some(0x00AABBCC),
            bg: Some(0x00112233),
            sp: None,
        };
        let s = attr_to_style(
            HlAttr {
                reverse: true,
                ..HlAttr::default()
            },
            &d,
        );
        assert_eq!(s.fg, Some(Color::Rgb(0x11, 0x22, 0x33)));
        assert_eq!(s.bg, Some(Color::Rgb(0xAA, 0xBB, 0xCC)));
    }

    #[test]
    fn bold_italic_underline_modifiers() {
        let d = DefaultColors::default();
        let s = attr_to_style(
            HlAttr {
                bold: true,
                italic: true,
                underline: true,
                ..HlAttr::default()
            },
            &d,
        );
        assert!(s.add_modifier.contains(Modifier::BOLD));
        assert!(s.add_modifier.contains(Modifier::ITALIC));
        assert!(s.add_modifier.contains(Modifier::UNDERLINED));
    }
}
