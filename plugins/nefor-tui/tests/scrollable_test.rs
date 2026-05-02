//! End-to-end integration test for `tui.scrollable`.
//!
//! Drives the engine through key + mouse events, asserting the
//! browser-style overflow contract:
//!
//! 1. Initial frame with `stick_to = "end"` lands at the bottom and
//!    `on_scroll` reflects that offset.
//! 2. Mouse wheel inside the scrollable's painted rect scrolls it (and
//!    fires `on_scroll`); does NOT bubble as `mouse.wheel`.
//! 3. Keyboard scrolling stays in Lua's domain — `key.pageup` /
//!    `key.pagedown` bubble and Lua's `update` calls `tui.scroll_by` to
//!    translate them.
//! 4. `tui.scroll_to(key, 0)` from Lua jumps to the top.
//! 5. `tui.scroll_into_view(key)` jumps back to the bottom (v1 minimal
//!    semantics).

use nefor_tui::engine::Engine;
use nefor_tui::input::KeyMessage;
use nefor_tui::mouse::{MouseKind, MouseMessage};

const SCROLLABLE_SCENARIO: &str = include_str!("../scenarios/scrollable.lua");

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
fn first_render_with_stick_to_end_pins_to_bottom() {
    let mut engine = Engine::new(40, 8).expect("engine");
    engine.load_scenario(SCROLLABLE_SCENARIO).expect("scenario");
    let _ = render_str(&mut engine);

    // Trigger an `on_scroll` so Lua observes the post-paint offset.
    // The scrollable is at the bottom (scroll_y_max). Wheel-up moves
    // it to scroll_y_max - 3 and fires log.scrolled.
    engine
        .handle_mouse(MouseMessage {
            kind: MouseKind::Wheel,
            x: 1,
            y: 2,
            button: Some("up"),
            mods: vec![],
        })
        .expect("wheel");
    let out = render_str(&mut engine);
    // 30 rows of content into a 7-row viewport (column has 1-row text +
    // 7 rows for the expanded scrollable). max = 30 - 7 = 23. Wheel-up
    // moves us 3 rows up: 20.
    assert!(
        out.contains("offset: 20"),
        "stick_to=end pins at scroll_y_max, then wheel-up retreats; got:\n{out}"
    );
}

#[test]
fn wheel_inside_scrollable_does_not_bubble() {
    // The starter scenario doesn't track raw mouse.wheel events; this
    // assertion is implicit: if wheel bubbled to Lua, the offset
    // wouldn't move (only key-driven and on_scroll do). After three
    // wheel-ups, scroll_y advances by 9 rows from the bottom, so
    // offset goes from 23 → 14.
    let mut engine = Engine::new(40, 8).expect("engine");
    engine.load_scenario(SCROLLABLE_SCENARIO).expect("scenario");
    let _ = render_str(&mut engine);
    for _ in 0..3 {
        engine
            .handle_mouse(MouseMessage {
                kind: MouseKind::Wheel,
                x: 1,
                y: 2,
                button: Some("up"),
                mods: vec![],
            })
            .expect("wheel");
    }
    let out = render_str(&mut engine);
    // 23 - 9 = 14.
    assert!(
        out.contains("offset: 14"),
        "wheel inside scrollable should advance scroll_y by 9 rows total; got:\n{out}"
    );
}

#[test]
fn pageup_pagedown_bubble_and_lua_translates_to_scroll_by() {
    let mut engine = Engine::new(40, 8).expect("engine");
    engine.load_scenario(SCROLLABLE_SCENARIO).expect("scenario");
    let _ = render_str(&mut engine);
    // Initial offset = 23 (stick to end). PgUp twice: -10 each → 3.
    engine.handle_key(key("pageup")).expect("pgup");
    engine.handle_key(key("pageup")).expect("pgup");
    let out = render_str(&mut engine);
    assert!(
        out.contains("offset: 3"),
        "two PgUp = -20 from 23 = 3; got:\n{out}"
    );
    // PgDn once: +10 from 3 = 13.
    engine.handle_key(key("pagedown")).expect("pgdn");
    let out = render_str(&mut engine);
    assert!(
        out.contains("offset: 13"),
        "PgDn = +10 from 3 = 13; got:\n{out}"
    );
}

#[test]
fn home_jumps_to_top_via_scroll_to() {
    let mut engine = Engine::new(40, 8).expect("engine");
    engine.load_scenario(SCROLLABLE_SCENARIO).expect("scenario");
    let _ = render_str(&mut engine);
    // Move off the bottom first so home → 0 produces a visible offset
    // change (initial Lua state already has scroll_offset = 0, so a
    // direct home from the pinned-bottom would yield the same desc text
    // and the diff frame wouldn't repaint row 0).
    engine.handle_key(key("pageup")).expect("pgup");
    let _ = render_str(&mut engine);
    engine.handle_key(key("home")).expect("home");
    let out = render_str(&mut engine);
    assert!(
        out.contains("offset: 0"),
        "home should jump to top via tui.scroll_to; got:\n{out}"
    );
}

#[test]
fn end_jumps_to_bottom_via_scroll_into_view() {
    let mut engine = Engine::new(40, 8).expect("engine");
    engine.load_scenario(SCROLLABLE_SCENARIO).expect("scenario");
    let _ = render_str(&mut engine);
    // Move off the bottom first.
    engine.handle_key(key("home")).expect("home");
    let _ = render_str(&mut engine);
    // Now jump back via `tui.scroll_into_view`.
    engine.handle_key(key("end")).expect("end");
    let out = render_str(&mut engine);
    assert!(
        out.contains("offset: 23"),
        "end should restore bottom via tui.scroll_into_view; got:\n{out}"
    );
}
