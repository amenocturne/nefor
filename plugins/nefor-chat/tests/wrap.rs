//! Integration tests for the wrap module. Complements the unit tests
//! inside `src/wrap.rs` with higher-level edge cases the production
//! renderer cares about.
//!
//! These tests are integration-style only in the `cargo test` sense —
//! they still depend solely on the public module surface.

// The wrap module is private-to-crate. Exercise it via the render module
// which re-uses it, plus a handful of reachable helpers elsewhere. For
// deeper wrap edge cases, see `src/wrap.rs`'s own `#[cfg(test)]` block.

#[path = "../src/wrap.rs"]
mod wrap;

#[test]
fn wraps_single_char_at_width_one() {
    let lines = wrap::wrap_to_width("abc", 1);
    assert_eq!(lines, vec!["a".to_owned(), "b".to_owned(), "c".to_owned()]);
}

#[test]
fn wraps_exactly_at_boundary() {
    // "hello world" → split at boundary 5: "hello", then "world" (width 5).
    let lines = wrap::wrap_to_width("hello world", 5);
    assert_eq!(lines, vec!["hello".to_owned(), "world".to_owned()]);
}

#[test]
fn str_width_zero_width_chars() {
    // Combining acute accent is zero-width; "é" (composed) is 1.
    assert_eq!(wrap::str_width("a"), 1);
    // `e` + combining acute = zero-width stacks on top.
    let composed = "e\u{0301}";
    assert_eq!(wrap::str_width(composed), 1);
}

#[test]
fn hard_break_preserves_total_chars() {
    let input = "abcdefghij";
    let lines = wrap::wrap_to_width(input, 3);
    let joined: String = lines.join("");
    assert_eq!(joined, input);
}

#[test]
fn word_longer_than_width_followed_by_short_word() {
    let lines = wrap::wrap_to_width("longlongword hi", 4);
    // "long", "long", "word", "hi"
    assert_eq!(
        lines,
        vec![
            "long".to_owned(),
            "long".to_owned(),
            "word".to_owned(),
            "hi".to_owned(),
        ]
    );
}
