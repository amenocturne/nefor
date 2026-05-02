//! In-process engine integration test for the phase-1 counter scenario.
//!
//! Drives `Engine` directly — no spawned subprocess, no /dev/tty — per
//! orchestrator override Q1 (option b). The scenario lives next to this
//! test in `scenarios/counter.lua`.

use nefor_tui::engine::Engine;
use nefor_tui::input::KeyMessage;

const COUNTER_SCENARIO: &str = include_str!("../scenarios/counter.lua");

fn space() -> KeyMessage {
    KeyMessage {
        name: "space".into(),
        mods: vec![],
    }
}

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
fn counter_initial_render_shows_zero() {
    let mut engine = Engine::new(40, 5).expect("engine");
    engine.load_scenario(COUNTER_SCENARIO).expect("scenario");
    let out = render_str(&mut engine);
    assert!(out.contains("count: 0"), "first frame: count: 0");
    // Synchronized output framing.
    assert!(out.starts_with("\x1b[?2026h"));
    assert!(out.ends_with("\x1b[?2026l"));
    // First frame is full-redraw.
    assert!(out.contains("\x1b[2J"));
    // Cursor hidden in phase 1.
    assert!(out.contains("\x1b[?25l"));
}

#[test]
fn counter_increments_on_each_space() {
    let mut engine = Engine::new(40, 5).expect("engine");
    engine.load_scenario(COUNTER_SCENARIO).expect("scenario");
    let _ = render_str(&mut engine);

    for expected in 1..=4 {
        engine.handle_key(space()).expect("space");
        let out = render_str(&mut engine);
        let needle = format!("count: {expected}");
        assert!(out.contains(&needle), "expected {needle} in:\n{out}");
        // Subsequent frames are diff frames — no full-screen clear.
        assert!(!out.contains("\x1b[2J"), "diff frame should not full-clear");
    }
}

#[test]
fn counter_diff_emits_only_changed_row() {
    let mut engine = Engine::new(40, 5).expect("engine");
    engine.load_scenario(COUNTER_SCENARIO).expect("scenario");
    let _ = render_str(&mut engine);

    engine.handle_key(space()).expect("space");
    let out = render_str(&mut engine);

    // Padding row stays blank — should not be re-emitted.
    // The text row content changed (0 → 1) and is emitted exactly once.
    let occurrences = out.matches("count: 1").count();
    assert_eq!(occurrences, 1, "exactly one occurrence of count: 1");

    // We should not see the prior frame's content re-rendered.
    assert!(
        !out.contains("count: 0"),
        "diff frame must not include the old text"
    );
}

#[test]
fn q_key_requests_exit() {
    let mut engine = Engine::new(40, 5).expect("engine");
    engine.load_scenario(COUNTER_SCENARIO).expect("scenario");
    let _ = render_str(&mut engine);
    assert!(!engine.exit_requested());
    engine.handle_key(key("q")).expect("q");
    assert!(engine.exit_requested(), "q should set exit flag");
}

#[test]
fn unrelated_keys_are_no_ops() {
    let mut engine = Engine::new(40, 5).expect("engine");
    engine.load_scenario(COUNTER_SCENARIO).expect("scenario");
    let _ = render_str(&mut engine);

    // Always-dirty after dispatch (phase-1 invariant per orchestrator
    // override Q2): render_if_dirty returns Some, but the diff frame
    // contains no row content because nothing changed.
    engine.handle_key(key("a")).expect("a");
    let out = render_str(&mut engine);
    assert!(!out.contains("count: 1"), "no increment for unrelated key");
    assert!(
        !out.contains("count: 0"),
        "unchanged row should not be re-emitted"
    );
    assert!(!engine.exit_requested());
}

#[test]
fn resize_redraws_with_full_clear() {
    let mut engine = Engine::new(40, 5).expect("engine");
    engine.load_scenario(COUNTER_SCENARIO).expect("scenario");
    let _ = render_str(&mut engine);

    engine.handle_resize(60, 8).expect("resize");
    let out = render_str(&mut engine);
    assert!(out.contains("\x1b[2J"), "resize forces full redraw");
    assert!(out.contains("count: 0"));
}
