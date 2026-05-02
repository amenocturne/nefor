//! End-to-end integration test for `tui.text_input`.
//!
//! Drives `Engine` directly through key events, asserting:
//!
//! 1. Typing into a focused input fires `on_change` with the new value
//!    and the value visibly updates in the next render.
//! 2. Enter (no Shift) fires `on_submit` and does NOT modify the value.
//! 3. Tab bubbles through to Lua as `key.tab` even with the input focused.
//! 4. Backspace edits propagate through the controlled-component cycle.

use nefor_tui_decl::engine::Engine;
use nefor_tui_decl::input::KeyMessage;

const TEXT_INPUT_SCENARIO: &str = include_str!("../scenarios/text_input.lua");

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
