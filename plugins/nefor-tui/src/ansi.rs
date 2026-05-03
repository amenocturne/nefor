//! ANSI escape sequence helpers used by the line-diff renderer.
//!
//! Phase 1 emits a deliberately small set: cursor positioning, line
//! erasure, full SGR, hide cursor, clear screen, and the synchronized-
//! output begin/end pair (CSI 2026). Minimal-diff SGR is deferred to a
//! later phase.

use std::fmt::Write as _;

use crate::desc::{Color, Style};

/// Begin a synchronized update batch. Terminals that support DEC mode
/// 2026 will hold rendering until the matching end sequence.
pub const SYNC_BEGIN: &str = "\x1b[?2026h";

/// End the synchronized update batch. Pairs with [`SYNC_BEGIN`].
pub const SYNC_END: &str = "\x1b[?2026l";

/// Hide the terminal cursor. Phase 1 has no input cursor primitive, so we
/// hide unconditionally to keep the output flicker-free.
pub const HIDE_CURSOR: &str = "\x1b[?25l";

/// Clear the entire screen and move the cursor to (1,1).
pub const CLEAR_SCREEN: &str = "\x1b[2J\x1b[H";

/// SGR reset — turns off every attribute and reverts colors.
pub const SGR_RESET: &str = "\x1b[0m";

/// Erase the line at the cursor position from the cursor through the
/// end of line.
pub const CLEAR_LINE: &str = "\x1b[2K";

/// Append a "move cursor to (row, col)" sequence using 1-based
/// coordinates (the terminal's own convention).
pub fn write_move_to(out: &mut String, row: u16, col: u16) {
    let _ = write!(
        out,
        "\x1b[{};{}H",
        row.saturating_add(1),
        col.saturating_add(1)
    );
}

/// Emit the SGR sequence for a [`Style`]. Reset is implicit at the end
/// of each row (callers pair this with [`SGR_RESET`]).
pub fn write_style(out: &mut String, s: &Style) {
    out.push_str("\x1b[0");
    if s.bold {
        out.push_str(";1");
    }
    if s.italic {
        out.push_str(";3");
    }
    if s.underline {
        out.push_str(";4");
    }
    if s.reverse {
        out.push_str(";7");
    }
    if s.strikethrough {
        out.push_str(";9");
    }
    if let Some(fg) = s.fg {
        write_fg(out, fg);
    }
    if let Some(bg) = s.bg {
        write_bg(out, bg);
    }
    out.push('m');
}

fn write_fg(out: &mut String, c: Color) {
    match c {
        Color::Reset => out.push_str(";39"),
        Color::Indexed(n) => {
            let _ = write!(out, ";38;5;{n}");
        }
        Color::Rgb(r, g, b) => {
            let _ = write!(out, ";38;2;{r};{g};{b}");
        }
    }
}

fn write_bg(out: &mut String, c: Color) {
    match c {
        Color::Reset => out.push_str(";49"),
        Color::Indexed(n) => {
            let _ = write!(out, ";48;5;{n}");
        }
        Color::Rgb(r, g, b) => {
            let _ = write!(out, ";48;2;{r};{g};{b}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_to_uses_one_based_coords() {
        let mut s = String::new();
        write_move_to(&mut s, 0, 0);
        assert_eq!(s, "\x1b[1;1H");
        let mut s = String::new();
        write_move_to(&mut s, 4, 9);
        assert_eq!(s, "\x1b[5;10H");
    }

    #[test]
    fn neutral_style_emits_bare_zero_reset() {
        let mut s = String::new();
        write_style(&mut s, &Style::default());
        assert_eq!(s, "\x1b[0m");
    }

    #[test]
    fn bold_underline_sgr_payload() {
        let mut s = String::new();
        let style = Style {
            bold: true,
            underline: true,
            ..Style::default()
        };
        write_style(&mut s, &style);
        assert_eq!(s, "\x1b[0;1;4m");
    }

    #[test]
    fn strikethrough_emits_sgr_9() {
        let mut s = String::new();
        let style = Style {
            strikethrough: true,
            ..Style::default()
        };
        write_style(&mut s, &style);
        assert_eq!(s, "\x1b[0;9m");
    }

    #[test]
    fn fg_indexed_emits_38_5() {
        let mut s = String::new();
        let style = Style {
            fg: Some(Color::Indexed(196)),
            ..Style::default()
        };
        write_style(&mut s, &style);
        assert_eq!(s, "\x1b[0;38;5;196m");
    }

    #[test]
    fn bg_rgb_emits_48_2() {
        let mut s = String::new();
        let style = Style {
            bg: Some(Color::Rgb(10, 20, 30)),
            ..Style::default()
        };
        write_style(&mut s, &style);
        assert_eq!(s, "\x1b[0;48;2;10;20;30m");
    }
}
