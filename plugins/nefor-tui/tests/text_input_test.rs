//! End-to-end integration test for `tui.text_input`.
//!
//! Drives `Engine` directly through key events, asserting:
//!
//! 1. Typing into a focused input fires `on_change` with the new value
//!    and the value visibly updates in the next render.
//! 2. Enter (no Shift) fires `on_submit` and does NOT modify the value.
//! 3. Tab bubbles through to Lua as `key.tab` even with the input focused.
//! 4. Backspace edits propagate through the controlled-component cycle.

use nefor_tui::engine::Engine;
use nefor_tui::input::KeyMessage;
use nefor_tui::mouse::{MouseKind, MouseMessage};

const TEXT_INPUT_SCENARIO: &str = include_str!("../scenarios/text_input.lua");
const TEXT_INPUT_MULTI_SCENARIO: &str = include_str!("../scenarios/text_input_multi.lua");

fn key(name: &str) -> KeyMessage {
    KeyMessage {
        name: name.into(),
        mods: vec![],
    }
}

fn wheel(direction: &'static str, x: u16, y: u16) -> MouseMessage {
    MouseMessage {
        kind: MouseKind::Wheel,
        x,
        y,
        button: Some(direction),
        mods: vec![],
    }
}

fn render_str(engine: &mut Engine) -> String {
    let bytes = engine
        .render_if_dirty()
        .expect("render")
        .expect("dirty after dispatch");
    String::from_utf8(bytes).expect("ansi is utf-8")
}

#[test]
fn typing_hello_round_trips_through_lua() {
    let mut engine = Engine::new(40, 8).expect("engine");
    engine.load_scenario(TEXT_INPUT_SCENARIO).expect("scenario");
    let _ = render_str(&mut engine); // initial frame

    for ch in ['h', 'e', 'l', 'l', 'o'] {
        engine
            .handle_key(key(&ch.to_string()))
            .expect("dispatch printable");
    }

    let out = render_str(&mut engine);
    assert!(
        out.contains("value: hello"),
        "expected value to mirror typed input, got:\n{out}"
    );
}

#[test]
fn enter_fires_on_submit_without_changing_value() {
    let mut engine = Engine::new(40, 8).expect("engine");
    engine.load_scenario(TEXT_INPUT_SCENARIO).expect("scenario");
    let _ = render_str(&mut engine);

    for ch in ['h', 'i'] {
        engine.handle_key(key(&ch.to_string())).expect("printable");
    }
    engine.handle_key(key("enter")).expect("enter");

    let out = render_str(&mut engine);
    assert!(out.contains("value: hi"), "value preserved after submit");
    assert!(
        out.contains("submitted: hi"),
        "submitted callback fired with current value, got:\n{out}"
    );
}

#[test]
fn tab_bubbles_to_lua_even_when_input_focused() {
    let mut engine = Engine::new(40, 8).expect("engine");
    engine.load_scenario(TEXT_INPUT_SCENARIO).expect("scenario");
    let _ = render_str(&mut engine);

    engine.handle_key(key("tab")).expect("tab");
    engine.handle_key(key("tab")).expect("tab");

    let out = render_str(&mut engine);
    assert!(
        out.contains("tabs: 2"),
        "tab should bubble even with focused input, got:\n{out}"
    );
}

#[test]
fn backspace_removes_last_typed_char() {
    let mut engine = Engine::new(40, 8).expect("engine");
    engine.load_scenario(TEXT_INPUT_SCENARIO).expect("scenario");
    let _ = render_str(&mut engine);

    for ch in ['h', 'i'] {
        engine.handle_key(key(&ch.to_string())).expect("printable");
    }
    engine.handle_key(key("backspace")).expect("backspace");

    let out = render_str(&mut engine);
    assert!(
        out.contains("value: h"),
        "expected backspaced value, got:\n{out}"
    );
    assert!(
        !out.contains("value: hi"),
        "old value should not be re-emitted, got:\n{out}"
    );
}

#[test]
fn paste_past_max_lines_keeps_cursor_row_visible_at_bottom() {
    // Regression: pasting content that wraps to more rows than `max_lines`
    // must leave the LAST line of the paste (where the cursor lives) on
    // screen — not the first line. Claude-Code-style bottom anchoring:
    // the input grows up to its cap and then internal-scrolls so the
    // cursor stays visible.
    //
    // The buggy behaviour was that scroll_y stayed at 0 so the painted
    // window showed rows [0, max_lines) — the TOP of the paste — and the
    // cursor row at the bottom of the buffer was off-screen.
    let mut engine = Engine::new(80, 24).expect("engine");
    engine
        .load_scenario(TEXT_INPUT_MULTI_SCENARIO)
        .expect("scenario");
    let _ = render_str(&mut engine);

    // 50-line paste with unique sentinels per row so we can pin which
    // window of the buffer is being painted.
    let lines: Vec<String> = (0..50).map(|i| format!("LINE{i:02}")).collect();
    let payload = lines.join("\n");
    engine.handle_paste(&payload).expect("paste");
    let out = render_str(&mut engine);

    // The LAST line must be visible (cursor lives at the end of the
    // buffer post-paste).
    assert!(
        out.contains("LINE49"),
        "last line of paste (cursor row) must be visible, got:\n{out}"
    );
    // The FIRST line must NOT be visible — the input has max_lines = 6
    // so a 50-line buffer overflows by 44 rows; row 0 is scrolled
    // off-screen above the visible window.
    assert!(
        !out.contains("LINE00"),
        "first line of paste must be scrolled off the top, got:\n{out}"
    );
}

#[test]
fn typing_past_max_lines_keeps_cursor_row_visible() {
    // Pin the typing path independently of the paste path. Typing a
    // newline-separated block via Shift+Enter / direct chars must also
    // keep the cursor visible after each insertion.
    let mut engine = Engine::new(80, 24).expect("engine");
    engine
        .load_scenario(TEXT_INPUT_MULTI_SCENARIO)
        .expect("scenario");
    let _ = render_str(&mut engine);

    // Build the buffer through the paste primitive (single insert_str —
    // simpler than 200+ keystrokes), then append one final char via the
    // key path so we verify the post-key window pins to the cursor.
    let lines: Vec<String> = (0..20).map(|i| format!("ROW{i:02}")).collect();
    engine.handle_paste(&lines.join("\n")).expect("paste");
    engine.handle_key(key("!")).expect("printable");
    let out = render_str(&mut engine);

    // The cursor row carries "ROW19" + "!" — check the suffix.
    assert!(
        out.contains("ROW19!"),
        "cursor row must remain visible after a key press at end, got:\n{out}"
    );
    assert!(
        !out.contains("ROW00"),
        "top of buffer must be scrolled off, got:\n{out}"
    );
}

// ── Mouse-wheel scrolling on a focused multi-line text_input ───────────
//
// The cursor-pin auto-scroll keeps the cursor visible after edits, but
// the user also wants to wheel the prompt to peek at the top of a long
// pasted block — even though the cursor is at the bottom. Wheel sets
// `manual_scroll` so the auto-pin temporarily stops yanking the
// viewport back; cursor moves and content mutations re-engage it.

#[test]
fn wheel_up_on_long_buffer_scrolls_past_cursor() {
    // Paste a 50-line buffer (cursor at end, auto-scrolled to bottom),
    // wheel up several ticks, assert the top of the buffer comes into
    // view AND the cursor row is now off-screen above the visible
    // window. Without the wheel handler the wheel either bubbled to a
    // sibling scrollable or no-oped, leaving the cursor row pinned.
    let mut engine = Engine::new(80, 24).expect("engine");
    engine
        .load_scenario(TEXT_INPUT_MULTI_SCENARIO)
        .expect("scenario");
    let _ = render_str(&mut engine);

    let lines: Vec<String> = (0..50).map(|i| format!("LINE{i:02}")).collect();
    engine.handle_paste(&lines.join("\n")).expect("paste");
    // Render once so the post-paste paint pass runs and the input's
    // viewport_width / scroll_y are seeded against the long buffer.
    let post_paste = render_str(&mut engine);
    assert!(
        post_paste.contains("LINE49"),
        "sanity: cursor row visible after paste, got:\n{post_paste}"
    );

    // Wheel up many notches — each notch advances scroll_y by
    // WHEEL_STEP_ROWS (3). After 5 ticks (15 rows up from max_scroll =
    // 50 - 6 = 44) the visible window covers rows 29..35. The cursor
    // row (49) is well past the bottom edge.
    for _ in 0..5 {
        engine.handle_mouse(wheel("up", 5, 2)).expect("wheel up");
    }
    let out = render_str(&mut engine);
    assert!(
        !contains_at_line_start(&out, "LINE49"),
        "cursor row must scroll off the bottom on wheel-up, got:\n{out}"
    );
    // Some mid-buffer line should now be visible — pick LINE30 as a
    // sentinel that lives squarely inside the new viewport (max_scroll
    // = 44; after 5 wheel-ups @ 3 rows each, scroll_y = 29, viewport
    // covers rows 29..35).
    assert!(
        contains_at_line_start(&out, "LINE30"),
        "wheel up should bring earlier lines into view, got:\n{out}"
    );
}

#[test]
fn wheel_down_to_bottom_re_pins_cursor() {
    // After scrolling up off the cursor, wheel-down enough to clamp
    // back at max_scroll. The cursor row reappears. This exercises
    // the clamp path on `state.scroll_y` and pins the "wheel can't
    // run past bottom" invariant.
    let mut engine = Engine::new(80, 24).expect("engine");
    engine
        .load_scenario(TEXT_INPUT_MULTI_SCENARIO)
        .expect("scenario");
    let _ = render_str(&mut engine);

    let lines: Vec<String> = (0..50).map(|i| format!("LINE{i:02}")).collect();
    engine.handle_paste(&lines.join("\n")).expect("paste");
    let _ = render_str(&mut engine);

    // Wheel up 5 times, render to confirm cursor row scrolled away.
    for _ in 0..5 {
        engine.handle_mouse(wheel("up", 5, 2)).expect("wheel up");
    }
    let scrolled = render_str(&mut engine);
    assert!(
        !contains_at_line_start(&scrolled, "LINE49"),
        "sanity: cursor row should be off-screen, got:\n{scrolled}"
    );
    // Wheel down enough to overshoot — clamp keeps scroll_y at
    // max_scroll, no panic, cursor row visible again.
    for _ in 0..20 {
        engine
            .handle_mouse(wheel("down", 5, 2))
            .expect("wheel down");
    }
    let out = render_str(&mut engine);
    assert!(
        contains_at_line_start(&out, "LINE49"),
        "wheel-down past max should clamp; cursor row back in view, got:\n{out}"
    );
}

/// Helper: search for a row sentinel in the rendered output, tolerating
/// the cursor's reverse-video ANSI break inside the matched substring.
/// The cursor lives on the LAST line; that line emits as
/// `LINE49\x1b[0;7m\x1b[0m` (the trailing 9 carries the reverse-video
/// run for the cursor cell). A naive `out.contains("LINE49")` matches
/// `LINE4` followed by literal `9` in the next ANSI burst — but the
/// ANSI escape sits between `LINE4` and `9`. Walk the bytes and tolerate
/// CSI escapes inside the needle.
fn contains_at_line_start(out: &str, needle: &str) -> bool {
    // Quick path: literal substring.
    if out.contains(needle) {
        return true;
    }
    // Slow path: scan the output, allowing `\x1b[...m` escape sequences
    // between adjacent characters of the needle. Suffices for the
    // cursor-cell case (one escape inside the matched word).
    let bytes = out.as_bytes();
    let nbytes = needle.as_bytes();
    let mut i = 0;
    while i + nbytes.len() <= bytes.len() {
        let mut bi = i;
        let mut ni = 0;
        while ni < nbytes.len() && bi < bytes.len() {
            if bytes[bi] == 0x1b && bytes[bi + 1] == b'[' {
                // Skip CSI: `\x1b[` then params/intermediates, then a
                // final byte in the 0x40..=0x7e range.
                bi += 2;
                while bi < bytes.len() && !(0x40..=0x7e).contains(&bytes[bi]) {
                    bi += 1;
                }
                bi += 1;
                continue;
            }
            if bytes[bi] != nbytes[ni] {
                break;
            }
            bi += 1;
            ni += 1;
        }
        if ni == nbytes.len() {
            return true;
        }
        i += 1;
    }
    false
}

#[test]
fn typing_after_manual_scroll_re_pins_to_cursor() {
    // Wheel up to peek at the top, then type a char. The keypress is
    // a content mutation; the auto-pin must re-engage so the cursor
    // row (now at the very end of buffer + new char) is visible again.
    let mut engine = Engine::new(80, 24).expect("engine");
    engine
        .load_scenario(TEXT_INPUT_MULTI_SCENARIO)
        .expect("scenario");
    let _ = render_str(&mut engine);

    let lines: Vec<String> = (0..50).map(|i| format!("LINE{i:02}")).collect();
    engine.handle_paste(&lines.join("\n")).expect("paste");
    let _ = render_str(&mut engine);
    for _ in 0..5 {
        engine.handle_mouse(wheel("up", 5, 2)).expect("wheel up");
    }
    let scrolled = render_str(&mut engine);
    assert!(
        !contains_at_line_start(&scrolled, "LINE49"),
        "sanity: cursor row scrolled away, got:\n{scrolled}"
    );

    // Type one char — content changes, auto-pin re-engages.
    engine.handle_key(key("X")).expect("printable");
    let out = render_str(&mut engine);
    assert!(
        contains_at_line_start(&out, "LINE49X"),
        "typing should re-pin cursor row, got:\n{out}"
    );
}

#[test]
fn arrow_key_after_manual_scroll_re_pins_to_cursor() {
    // Cursor-move keys also clear the manual_scroll latch — pressing
    // Left moves the cursor and the auto-pin should yank the viewport
    // back to the cursor row.
    let mut engine = Engine::new(80, 24).expect("engine");
    engine
        .load_scenario(TEXT_INPUT_MULTI_SCENARIO)
        .expect("scenario");
    let _ = render_str(&mut engine);

    let lines: Vec<String> = (0..50).map(|i| format!("LINE{i:02}")).collect();
    engine.handle_paste(&lines.join("\n")).expect("paste");
    let _ = render_str(&mut engine);
    for _ in 0..5 {
        engine.handle_mouse(wheel("up", 5, 2)).expect("wheel up");
    }
    let scrolled = render_str(&mut engine);
    assert!(
        !contains_at_line_start(&scrolled, "LINE49"),
        "sanity: cursor row scrolled away, got:\n{scrolled}"
    );

    // Cursor-only movement: Left at end-of-buffer moves cursor back
    // by one char but stays on the last logical line. Auto-pin yanks
    // the window back to the cursor row.
    engine.handle_key(key("left")).expect("left");
    let out = render_str(&mut engine);
    assert!(
        contains_at_line_start(&out, "LINE49"),
        "cursor-move key should re-pin viewport, got:\n{out}"
    );
}

#[test]
fn submit_after_manual_scroll_resets_state() {
    // Wheel up off the cursor, press Enter. Enter is routed as an
    // editing key (apply_editing_key clears manual_scroll up-front
    // before deciding submit-vs-newline), so the post-submit render
    // should snap the viewport back to the cursor row even though
    // the multi-line scenario's `update` doesn't itself clear the
    // value. The natural value-clear path (chat surface clears
    // input_value on submit) is covered indirectly: external value
    // rewrites also clear the latch via `sync_with_desc`.
    let mut engine = Engine::new(80, 24).expect("engine");
    engine
        .load_scenario(TEXT_INPUT_MULTI_SCENARIO)
        .expect("scenario");
    let _ = render_str(&mut engine);

    let lines: Vec<String> = (0..50).map(|i| format!("LINE{i:02}")).collect();
    engine.handle_paste(&lines.join("\n")).expect("paste");
    let _ = render_str(&mut engine);
    for _ in 0..5 {
        engine.handle_mouse(wheel("up", 5, 2)).expect("wheel up");
    }
    let scrolled = render_str(&mut engine);
    assert!(
        !contains_at_line_start(&scrolled, "LINE49"),
        "sanity: cursor row scrolled away pre-submit, got:\n{scrolled}"
    );
    engine.handle_key(key("enter")).expect("enter");
    let out = render_str(&mut engine);
    assert!(
        contains_at_line_start(&out, "LINE49"),
        "submit should clear manual_scroll; cursor row re-pinned, got:\n{out}"
    );
}

#[test]
fn wheel_on_short_buffer_consumes_event_without_panic() {
    // Wheel on an input whose content fits in the viewport: nothing
    // to scroll. The handler must not panic on the zero-max_scroll
    // clamp path. After the wheel storm a typed char still appends
    // at the cursor — the input is in a healthy state.
    let mut engine = Engine::new(80, 24).expect("engine");
    engine
        .load_scenario(TEXT_INPUT_MULTI_SCENARIO)
        .expect("scenario");
    let _ = render_str(&mut engine);
    engine.handle_paste("short").expect("paste");
    let post_paste = render_str(&mut engine);
    assert!(
        post_paste.contains("short"),
        "sanity: paste visible, got:\n{post_paste}"
    );
    // No panic regardless of direction.
    engine.handle_mouse(wheel("up", 5, 2)).expect("wheel up");
    engine
        .handle_mouse(wheel("down", 5, 2))
        .expect("wheel down");
    // Type a char to force a guaranteed visible diff post-wheel.
    engine.handle_key(key("!")).expect("printable");
    let out = render_str(&mut engine);
    assert!(
        out.contains("short!"),
        "value must remain intact and editable after wheel on short buffer, got:\n{out}"
    );
}
