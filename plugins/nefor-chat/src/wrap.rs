//! Word-wrap helpers, unicode-width aware.
//!
//! `nefor-tui` renders cells; we count terminal *columns*, not chars or
//! bytes. `unicode_width::UnicodeWidthChar` is the source of truth for how
//! wide a grapheme renders (CJK ideographs, emoji, etc.).
//!
//! `wrap_to_width` is a greedy word wrapper:
//!
//! - Prefers to break on whitespace.
//! - Falls back to hard breaks when a single word exceeds `width`.
//! - Empty input yields a single empty line (so the caller still reserves
//!   a row for the entry).
//! - Zero `width` collapses everything onto one line (defensive — the
//!   caller almost certainly mis-sized).

use unicode_width::UnicodeWidthChar;

/// Display width of a single character, with a safe fallback.
///
/// `UnicodeWidthChar::width` returns `None` for control characters; we
/// treat those as zero-width so they don't disturb layout. Tab is
/// coerced to a single space at the caller, not here.
pub fn char_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

/// Display width of a string (sum of [`char_width`] over each char).
pub fn str_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// Wrap `text` to `width` columns. Always returns at least one line.
///
/// Line breaks in `text` (`\n`) are honored as hard breaks. Within a line,
/// we greedily fit whole words; a word longer than `width` is broken at
/// column boundaries.
pub fn wrap_to_width(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        // Degenerate case — return the whole text on one line so the
        // caller can render something diagnostic rather than looping.
        return vec![text.to_owned()];
    }
    let mut out: Vec<String> = Vec::new();
    for paragraph in text.split('\n') {
        let wrapped = wrap_paragraph(paragraph, width);
        if wrapped.is_empty() {
            // Preserve empty paragraphs (blank lines in input).
            out.push(String::new());
        } else {
            out.extend(wrapped);
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn wrap_paragraph(paragraph: &str, width: usize) -> Vec<String> {
    // A fully-empty paragraph yields nothing — the caller (wrap_to_width)
    // decides whether to preserve the blank row.
    if paragraph.is_empty() {
        return Vec::new();
    }

    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_width: usize = 0;

    // Split on ASCII whitespace for word boundaries. Non-ASCII whitespace
    // is rare in chat content and treating it as part of the word is
    // acceptable for v1.
    let mut words = paragraph.split(' ').peekable();
    while let Some(word) = words.next() {
        // Empty "word" comes from consecutive spaces; preserve as a
        // single space in the current line if it fits.
        if word.is_empty() {
            if current_width < width {
                current.push(' ');
                current_width += 1;
            } else if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
                // Do not emit a leading space on the next line.
            }
            continue;
        }

        let word_width = str_width(word);

        // If current line is empty, try to place the word directly. If it
        // exceeds width, hard-break it across lines.
        if current.is_empty() {
            if word_width <= width {
                current.push_str(word);
                current_width = word_width;
            } else {
                for chunk in split_by_columns(word, width) {
                    // All-but-last chunk is exactly `width` wide → flush
                    // immediately.
                    let chunk_w = str_width(&chunk);
                    if chunk_w == width {
                        lines.push(chunk);
                    } else {
                        current = chunk;
                        current_width = chunk_w;
                    }
                }
            }
            continue;
        }

        // Non-empty current: see if word fits after a separator space.
        let needed = current_width + 1 + word_width;
        if needed <= width {
            current.push(' ');
            current.push_str(word);
            current_width += 1 + word_width;
        } else {
            // Doesn't fit on the current line. Flush and start fresh.
            lines.push(std::mem::take(&mut current));
            current_width = 0;
            if word_width <= width {
                current.push_str(word);
                current_width = word_width;
            } else {
                for chunk in split_by_columns(word, width) {
                    let chunk_w = str_width(&chunk);
                    if chunk_w == width {
                        lines.push(chunk);
                    } else {
                        current = chunk;
                        current_width = chunk_w;
                    }
                }
            }
        }

        // Drain any lingering lookahead-only whitespace — handled
        // implicitly by the next iteration of `words`.
        let _ = words.peek();
    }

    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

/// Break a string into chunks each no wider than `width` columns.
///
/// Used when a single "word" overflows the line budget. We walk chars and
/// accumulate until adding the next char would exceed `width`, then flush.
/// Wide chars (CJK) that don't fit are simply moved to the next chunk.
pub fn split_by_columns(s: &str, width: usize) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_w: usize = 0;
    for c in s.chars() {
        let w = char_width(c);
        if w == 0 {
            // Zero-width char (combining mark, control). Attach to
            // current chunk if any, else drop.
            if !current.is_empty() {
                current.push(c);
            }
            continue;
        }
        if current_w + w > width && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            current_w = 0;
        }
        current.push(c);
        current_w += w;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_yields_one_empty_line() {
        let lines = wrap_to_width("", 10);
        assert_eq!(lines, vec![String::new()]);
    }

    #[test]
    fn short_text_one_line() {
        let lines = wrap_to_width("hi there", 20);
        assert_eq!(lines, vec!["hi there".to_owned()]);
    }

    #[test]
    fn wraps_on_word_boundary() {
        let lines = wrap_to_width("the quick brown fox", 10);
        assert_eq!(lines, vec!["the quick".to_owned(), "brown fox".to_owned()]);
    }

    #[test]
    fn exact_width_fits_one_line() {
        let lines = wrap_to_width("abcdefghij", 10);
        assert_eq!(lines, vec!["abcdefghij".to_owned()]);
    }

    #[test]
    fn hard_break_long_unbroken_word() {
        let lines = wrap_to_width("abcdefghijklmnop", 5);
        assert_eq!(
            lines,
            vec![
                "abcde".to_owned(),
                "fghij".to_owned(),
                "klmno".to_owned(),
                "p".to_owned(),
            ]
        );
    }

    #[test]
    fn hard_break_then_continue() {
        let lines = wrap_to_width("supercalifragilistic hi", 10);
        assert_eq!(
            lines,
            vec![
                "supercalif".to_owned(),
                "ragilistic".to_owned(),
                "hi".to_owned(),
            ]
        );
    }

    #[test]
    fn honors_newlines() {
        let lines = wrap_to_width("line one\nline two", 20);
        assert_eq!(lines, vec!["line one".to_owned(), "line two".to_owned()]);
    }

    #[test]
    fn preserves_blank_line() {
        let lines = wrap_to_width("a\n\nb", 10);
        assert_eq!(lines, vec!["a".to_owned(), String::new(), "b".to_owned()]);
    }

    #[test]
    fn wide_char_counts_two_columns() {
        // 漢 is a width-2 char. Width 4 fits exactly 2 of them.
        let lines = wrap_to_width("漢字漢字", 4);
        assert_eq!(lines, vec!["漢字".to_owned(), "漢字".to_owned()]);
    }

    #[test]
    fn wide_char_hard_break_respects_width() {
        // width 3 allows one wide char (2) then stop — next char starts
        // a new chunk.
        let chunks = split_by_columns("漢字漢", 3);
        assert_eq!(
            chunks,
            vec!["漢".to_owned(), "字".to_owned(), "漢".to_owned()]
        );
    }

    #[test]
    fn str_width_matches_display() {
        assert_eq!(str_width("hi"), 2);
        assert_eq!(str_width("漢"), 2);
        assert_eq!(str_width(""), 0);
    }

    #[test]
    fn zero_width_returns_one_line() {
        let lines = wrap_to_width("abc", 0);
        assert_eq!(lines, vec!["abc".to_owned()]);
    }

    #[test]
    fn multiple_spaces_between_words() {
        // Two spaces between "a" and "b"; fits comfortably.
        let lines = wrap_to_width("a  b", 10);
        assert_eq!(lines, vec!["a  b".to_owned()]);
    }

    #[test]
    fn only_whitespace_paragraph() {
        // Pure whitespace folds into a blank line (current impl).
        let lines = wrap_to_width("   ", 10);
        assert_eq!(lines.len(), 1);
    }
}
