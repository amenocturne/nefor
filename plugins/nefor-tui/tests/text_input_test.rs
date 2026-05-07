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

const TEXT_INPUT_SCENARIO: &str = include_str!("../scenarios/text_input.lua");
const TEXT_INPUT_MULTI_SCENARIO: &str = include_str!("../scenarios/text_input_multi.lua");

fn key(name: &str) -> KeyMessage {
    KeyMessage {
        name: name.into(),
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
