//! Mouse-driven cell selection over the rendered grid.
//!
//! Lives in nefor-tui (rather than a chat-layer plugin) because the grid
//! buffer — the actual cells drawn on screen — is owned here. A peer that
//! only sees its own subset (e.g. nefor-chat's transcript) can't honor a
//! drag that crosses statusline / popup / autocomplete boundaries; this
//! module operates over the whole `Grid`.
//!
//! Selection FSM:
//!   * Left mouse down at `(r, c)` → `anchor = focus = (r, c)`, `active = true`.
//!   * Drag with `active == true` → `focus = (r, c)`.
//!   * Up: drop `active`. Zero-distance click clears silently; otherwise the
//!     caller harvests text via `extract_text` and copies.
//!
//! Selection is also cleared by any keystroke (the user has moved on) and by
//! resize (cell coordinates would invalidate). Wheel events are not part of
//! the FSM — they're forwarded as-is so transcript scrolling keeps working.

use crate::grid::Grid;

/// Active or just-finished selection. `(row, col)` are absolute terminal
/// cells matching the crossterm mouse-event coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    /// Cell where the drag started.
    pub anchor: (u16, u16),
    /// Current cell under the cursor (or release cell on `up`).
    pub focus: (u16, u16),
    /// `true` while the left button is still held.
    pub active: bool,
}

impl Selection {
    /// Start a fresh selection at `(row, col)`.
    pub fn new(row: u16, col: u16) -> Self {
        Self {
            anchor: (row, col),
            focus: (row, col),
            active: true,
        }
    }

    /// Inclusive normalized rect `(top, left, bottom, right)`. Standard
    /// editor convention: column order on the top row reads left→right
    /// from the *anchor* side; on the bottom row up to the *focus* side.
    /// For single-row selections the column order is left→right.
    pub fn normalized(&self) -> (u16, u16, u16, u16) {
        let (a_row, a_col) = self.anchor;
        let (f_row, f_col) = self.focus;
        let (top, bottom) = if a_row <= f_row {
            (a_row, f_row)
        } else {
            (f_row, a_row)
        };
        let (left, right) = if a_row == f_row {
            if a_col <= f_col {
                (a_col, f_col)
            } else {
                (f_col, a_col)
            }
        } else if a_row < f_row {
            (a_col, f_col)
        } else {
            (f_col, a_col)
        };
        (top, left, bottom, right)
    }

    /// `true` when anchor and focus refer to the same cell — a click without
    /// drag, treated as "deselect" rather than a copy.
    pub fn is_zero_distance(&self) -> bool {
        self.anchor == self.focus
    }
}

/// Per-row column range to paint / harvest given a normalized selection
/// rect. `None` when `row` is outside the rect; `Some((start, end))`
/// otherwise, in inclusive-exclusive cell-column form clamped to
/// `[0, total_cols]`. Top row spans `[left, total_cols)`, bottom row spans
/// `[0, right + 1)`, middle rows full width, single row `[left, right + 1)`.
pub fn col_range_for_row(
    row: u16,
    sel_top: u16,
    sel_left: u16,
    sel_bottom: u16,
    sel_right: u16,
    total_cols: u16,
) -> Option<(u16, u16)> {
    if row < sel_top || row > sel_bottom {
        return None;
    }
    let total = total_cols;
    let (start, end) = if sel_top == sel_bottom {
        (sel_left, sel_right.saturating_add(1))
    } else if row == sel_top {
        (sel_left, total)
    } else if row == sel_bottom {
        (0u16, sel_right.saturating_add(1))
    } else {
        (0u16, total)
    };
    let start = start.min(total);
    let end = end.min(total);
    if end <= start {
        None
    } else {
        Some((start, end))
    }
}

/// Harvest the plain text covered by `sel` from `grid`. Multi-row selections
/// join with `\n`; trailing whitespace is trimmed per row so blank cells at
/// the end of short lines don't pollute the clipboard. Wide-glyph
/// continuation cells (`text` empty) are skipped — the preceding cell already
/// holds the full grapheme.
pub fn extract_text(grid: &Grid, sel: &Selection) -> String {
    let (sel_top, sel_left, sel_bottom, sel_right) = sel.normalized();
    let height = grid.height();
    let width = grid.width();
    if height == 0 || width == 0 {
        return String::new();
    }

    let mut parts: Vec<String> = Vec::new();
    for row in sel_top..=sel_bottom {
        if row >= height {
            break;
        }
        let Some((c_start, c_end)) =
            col_range_for_row(row, sel_top, sel_left, sel_bottom, sel_right, width)
        else {
            continue;
        };
        let row_cells = grid.row(row);
        let mut line = String::new();
        for c in c_start..c_end {
            let cell = &row_cells[usize::from(c)];
            if cell.text.is_empty() {
                // Continuation cell of a wide glyph; the preceding cell
                // already carried the full text.
                continue;
            }
            line.push_str(&cell.text);
        }
        // Trim trailing spaces — selections that overshoot a short row pull
        // padding cells which we don't want on the clipboard.
        let trimmed: &str = line.trim_end_matches(' ');
        parts.push(trimmed.to_owned());
    }

    // Drop trailing empty parts so a selection that overshoots the final
    // row doesn't end with a stray blank line.
    while parts.last().is_some_and(String::is_empty) {
        parts.pop();
    }
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::{Grid, LineCell};

    fn grid_with(rows: &[&str]) -> Grid {
        let width = rows.iter().map(|r| r.chars().count()).max().unwrap_or(1) as u16;
        let height = rows.len() as u16;
        let mut g = Grid::new(width, height);
        for (r, row_text) in rows.iter().enumerate() {
            // Apply each char as its own LineCell so the grid records
            // individual cells matching what the chat-side renderer would
            // emit. Wider glyphs can be tested separately.
            let cells: Vec<LineCell> = row_text
                .chars()
                .map(|ch| LineCell {
                    text: ch.to_string(),
                    hl_id: Some(0),
                    repeat: Some(1),
                })
                .collect();
            g.apply_line(r as u32, 0, &cells);
        }
        g
    }

    #[test]
    fn new_starts_with_anchor_equals_focus_active() {
        let s = Selection::new(3, 5);
        assert_eq!(s.anchor, (3, 5));
        assert_eq!(s.focus, (3, 5));
        assert!(s.active);
        assert!(s.is_zero_distance());
    }

    #[test]
    fn normalized_single_row_left_to_right() {
        let s = Selection {
            anchor: (2, 8),
            focus: (2, 3),
            active: false,
        };
        assert_eq!(s.normalized(), (2, 3, 2, 8));
    }

    #[test]
    fn normalized_reverse_drag_swaps_top_bottom() {
        // Drag from (5, 7) up-left to (2, 3) → top-left should be (2, 3),
        // bottom-right (5, 7) per editor convention.
        let s = Selection {
            anchor: (5, 7),
            focus: (2, 3),
            active: false,
        };
        let (top, left, bottom, right) = s.normalized();
        assert_eq!((top, bottom), (2, 5));
        assert_eq!((left, right), (3, 7));
    }

    #[test]
    fn col_range_single_row_inclusive_right() {
        let r = col_range_for_row(5, 5, 3, 5, 7, 80);
        assert_eq!(r, Some((3, 8)));
    }

    #[test]
    fn col_range_top_row_extends_to_total_cols() {
        let r = col_range_for_row(2, 2, 4, 5, 9, 80);
        assert_eq!(r, Some((4, 80)));
    }

    #[test]
    fn col_range_middle_row_full_width() {
        let r = col_range_for_row(3, 2, 4, 5, 9, 80);
        assert_eq!(r, Some((0, 80)));
    }

    #[test]
    fn col_range_bottom_row_starts_at_zero() {
        let r = col_range_for_row(5, 2, 4, 5, 9, 80);
        assert_eq!(r, Some((0, 10)));
    }

    #[test]
    fn col_range_outside_rect_returns_none() {
        let r = col_range_for_row(10, 2, 4, 5, 9, 80);
        assert_eq!(r, None);
    }

    #[test]
    fn extract_single_row_returns_substring() {
        let g = grid_with(&["hello world"]);
        let sel = Selection {
            anchor: (0, 0),
            focus: (0, 4),
            active: false,
        };
        assert_eq!(extract_text(&g, &sel), "hello");
    }

    #[test]
    fn extract_multi_row_joins_with_newline() {
        // 6-col grid; rows: "first " and "second". Selection from (0,0) to
        // (1,5) covers all of row 0 and "second" on row 1 — multi-row top
        // extends to total width, bottom inclusive of focus column.
        let g = grid_with(&["first ", "second"]);
        let sel = Selection {
            anchor: (0, 0),
            focus: (1, 5),
            active: false,
        };
        let text = extract_text(&g, &sel);
        // Row 0 is "first " (6 chars wide). Top-row reaches total_cols=6,
        // trim_end_matches(' ') leaves "first". Row 1 is "second" (cols
        // 0..=5).
        assert_eq!(text, "first\nsecond");
    }

    #[test]
    fn extract_trims_trailing_spaces() {
        // Pad the row with trailing spaces; selection overshoots the line.
        let g = grid_with(&["hi    "]);
        let sel = Selection {
            anchor: (0, 0),
            focus: (0, 5),
            active: false,
        };
        // Top single-row, [0..=5+1) capped at 6 → entire row, trimmed.
        assert_eq!(extract_text(&g, &sel), "hi");
    }

    #[test]
    fn extract_skips_continuation_cell_after_wide_glyph() {
        // Build a 4-col grid manually: "漢" occupies cols 0..=1 (cell 0
        // holds the glyph, cell 1 is empty continuation), then "ab".
        let mut g = Grid::new(4, 1);
        g.apply_line(
            0,
            0,
            &[LineCell {
                text: "漢".into(),
                hl_id: Some(0),
                repeat: Some(1),
            }],
        );
        g.apply_line(
            0,
            2,
            &[
                LineCell {
                    text: "a".into(),
                    hl_id: Some(0),
                    repeat: Some(1),
                },
                LineCell {
                    text: "b".into(),
                    hl_id: Some(0),
                    repeat: Some(1),
                },
            ],
        );
        let sel = Selection {
            anchor: (0, 0),
            focus: (0, 3),
            active: false,
        };
        let text = extract_text(&g, &sel);
        // Should include the glyph once (continuation cell skipped) plus
        // "ab".
        assert_eq!(text, "漢ab");
    }

    #[test]
    fn extract_reverse_direction_drag_normalizes() {
        // Drag from right back to left on the same row.
        let g = grid_with(&["abcdef"]);
        let sel = Selection {
            anchor: (0, 5),
            focus: (0, 1),
            active: false,
        };
        assert_eq!(extract_text(&g, &sel), "bcdef");
    }

    #[test]
    fn extract_clamps_when_focus_past_row_end() {
        // 3-col grid, only "ab" written, last cell remains a default blank
        // (" "). Selection beyond the row width gets clamped via
        // col_range_for_row → trailing space trimmed.
        let g = grid_with(&["ab "]);
        let sel = Selection {
            anchor: (0, 0),
            focus: (0, 10),
            active: false,
        };
        assert_eq!(extract_text(&g, &sel), "ab");
    }
}
