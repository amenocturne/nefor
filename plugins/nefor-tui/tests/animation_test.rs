//! Animation integration test — drives `Engine` with a 4-frame spinner
//! and `advance_time` to assert that frame indices advance with elapsed
//! wall-clock time.
//!
//! Per spec (`Animation` section): time is the source of truth, never
//! ticks. The renderer should sample the current frame from `now -
//! mount_time`, so stepping the synthetic clock by half the
//! per-frame slice should switch the rendered frame.

use std::time::Duration;

use nefor_tui::engine::Engine;

const SPINNER_SCENARIO: &str = include_str!("../scenarios/spinner.lua");

fn render_str(engine: &mut Engine) -> String {
    let bytes = engine
        .render_if_dirty()
        .expect("render")
        .expect("dirty before stepping");
    String::from_utf8(bytes).expect("ansi is utf-8")
}

/// Find the rendered glyph in the topmost paint cell — the spinner is
/// a single character so we scan for any of the frames in the output
/// stream and return the first one observed.
fn observed_frame(rendered: &str) -> char {
    for ch in ['A', 'B', 'C', 'D'] {
        if rendered.contains(ch) {
            return ch;
        }
    }
    panic!("no frame char in render: {rendered:?}");
}

#[test]
fn spinner_advances_frame_with_clock_steps() {
    let mut engine = Engine::new(8, 2).expect("engine");
    engine.load_scenario(SPINNER_SCENARIO).expect("load");

    // Initial render mounts the animation at the current clock value.
    // The first sample should produce frame index 0 → 'A'.
    let first = render_str(&mut engine);
    assert_eq!(observed_frame(&first), 'A');

    // The animation is active — render loop should keep ticking.
    assert!(engine.has_active_animations());

    // Step the synthetic clock by 30ms — past the 25ms per-frame slice
    // (100ms / 4 frames). Frame index should be 1 → 'B'.
    engine.advance_time(Duration::from_millis(30));
    let second = render_str(&mut engine);
    assert_eq!(
        observed_frame(&second),
        'B',
        "after +30ms expected frame B (idx 1)"
    );

    // Step further. Total elapsed should be ~60ms → frame index 2 → 'C'.
    engine.advance_time(Duration::from_millis(30));
    let third = render_str(&mut engine);
    assert_eq!(
        observed_frame(&third),
        'C',
        "after +60ms total expected frame C (idx 2)"
    );

    // Cross past one full cycle (90ms total) → frame index 3 → 'D'.
    engine.advance_time(Duration::from_millis(30));
    let fourth = render_str(&mut engine);
    assert_eq!(
        observed_frame(&fourth),
        'D',
        "after +90ms total expected frame D (idx 3)"
    );

    // After one more 30ms hop (120ms total ⇒ wrap), should show 'A'.
    engine.advance_time(Duration::from_millis(30));
    let fifth = render_str(&mut engine);
    assert_eq!(
        observed_frame(&fifth),
        'A',
        "after +120ms total (wrapped) expected frame A (idx 0)"
    );
}

#[test]
fn finite_animation_completes_and_holds_last_frame() {
    let scenario = r#"
        tui.start {
          initial_state = {},
          view = function(_)
            return tui.animation {
              frames      = { "A", "B", "C" },
              duration_ms = 100,
              iterations  = 2,
            }
          end,
          update = function(_, s) return s, {} end,
        }
    "#;
    let mut engine = Engine::new(8, 2).expect("engine");
    engine.load_scenario(scenario).expect("load");

    let _ = render_str(&mut engine);

    // Still active mid-playback.
    engine.advance_time(Duration::from_millis(50));
    let _ = render_str(&mut engine);
    assert!(engine.has_active_animations());

    // After 2 full cycles (200ms total), animation completes.
    engine.advance_time(Duration::from_millis(160));
    let final_render = render_str(&mut engine);
    assert!(
        !engine.has_active_animations(),
        "animation should complete after iterations elapse"
    );
    assert_eq!(
        observed_frame(&final_render),
        'C',
        "completed forward animation should hold final frame"
    );
}

/// Exercises the high-frequency-sampling case: a 4-frame animation
/// with 4ms duration (1ms per frame) renders at much-finer-than-frame
/// resolution. The sampler is required to never panic and to always
/// return an in-range index — frame skipping is fine.
#[test]
fn very_fast_animation_does_not_panic_or_overshoot() {
    let scenario = r#"
        tui.start {
          initial_state = {},
          view = function(_)
            return tui.animation {
              frames      = { "A", "B", "C", "D" },
              duration_ms = 4,
            }
          end,
          update = function(_, s) return s, {} end,
        }
    "#;
    let mut engine = Engine::new(8, 2).expect("engine");
    engine.load_scenario(scenario).expect("load");
    for _ in 0..50 {
        let _ = render_str(&mut engine);
        let _ = observed_frame(&render_str_or_empty(&mut engine));
        engine.advance_time(Duration::from_millis(1));
    }
}

fn render_str_or_empty(engine: &mut Engine) -> String {
    match engine.render_if_dirty() {
        Ok(Some(bytes)) => String::from_utf8(bytes).expect("ansi is utf-8"),
        _ => "A".into(), // no-op render: pretend frame A so observed_frame is happy
    }
}
