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
fn drag_past_bottom_edge_auto_scrolls_transcript_down() {
    // Standard editor / browser behaviour: when the user drags a text
    // selection past the bottom of the visible region, the transcript
    // auto-scrolls so the selection can extend beyond the viewport. Each
    // fresh `Drag` event whose y lies in (or past) the bottom edge zone
    // advances `scroll_y` by `DRAG_AUTO_SCROLL_STEP` (1 row).
    //
    // Scrollable scenario: 30 rows of `row N` content in a 7-row viewport
    // (40×8 frame, top row is the "offset:" label). Pinned-to-bottom on
    // first paint → visible rows 24..30. After a wheel-up step (3 rows),
    // scroll_y = 20 → visible rows 21..27. The mouse-down at (2, 4) lands
    // inside the scrollable's painted rect (rows 1..7); the captured
    // scrollable key is `log`. Each subsequent `Drag` past y=7 (the rect's
    // last visible row, in the edge zone) advances scroll_y by 1.
    let mut engine = Engine::new(40, 8).expect("engine");
    engine.load_scenario(SCROLLABLE_SCENARIO).expect("scenario");
    let _ = render_str(&mut engine);

    // Wheel-up so we're not pinned at the bottom — gives us headroom in
    // both directions for the assertion. Initial scroll_y_max = 30 - 7 =
    // 23, wheel-up by 3 lands at scroll_y = 20.
    engine
        .handle_mouse(MouseMessage {
            kind: MouseKind::Wheel,
            x: 1,
            y: 4,
            button: Some("up"),
            mods: vec![],
        })
        .expect("wheel-up");
    let pre = render_str(&mut engine);
    assert!(
        pre.contains("offset: 20"),
        "expected wheel-up to put us at offset 20 before the drag; got:\n{pre}"
    );
    // Pre-drag visible window: rows 21..27. row 27 is the last visible
    // line, row 28 is the next row that should come into view as we
    // auto-scroll.
    assert!(
        pre.contains("row 27"),
        "pre-drag frame should show row 27 as bottom of viewport:\n{pre}"
    );
    assert!(
        !pre.contains("row 28"),
        "pre-drag frame should NOT yet show row 28:\n{pre}"
    );

    // Click inside the scrollable rect (rows 1..7) — captures `log` as
    // the drag-origin scrollable. Drag past the bottom of the rect (y
    // beyond row 7) — past-the-edge triggers auto-scroll-down.
    engine
        .handle_mouse(MouseMessage {
            kind: MouseKind::Click,
            x: 2,
            y: 4,
            button: Some("left"),
            mods: vec![],
        })
        .expect("click");
    // Five drag events past the bottom — each advances scroll_y by 1
    // (DRAG_AUTO_SCROLL_STEP). After five: scroll_y = 25, visible
    // rows 26..32, which is clamped against scroll_y_max = 23 → wait,
    // actually 20 + 5 = 25 but max is 23, so two of those clamp. Net
    // visible after clamp: scroll_y = 23, rows 24..30. row 30 is the
    // very last; the assertion below uses row 28 which is well clear
    // of the edge case.
    for _ in 0..5 {
        engine
            .handle_mouse(MouseMessage {
                kind: MouseKind::Drag,
                x: 2,
                // y past the rect (rect bottom row is 7); 10 is comfortably
                // past so we don't depend on the edge-zone constant.
                y: 10,
                button: Some("left"),
                mods: vec![],
            })
            .expect("drag-down");
    }
    // No `Up` — we want to observe the in-flight scroll.
    let mid = render_str(&mut engine);
    // row 28 should now be visible — the transcript scrolled down to
    // follow the drag.
    assert!(
        mid.contains("row 28"),
        "drag past the bottom edge should auto-scroll the transcript down to bring row 28 into view; got:\n{mid}"
    );
    // Sanity: row 21 (which WAS visible pre-drag) should now be gone.
    assert!(
        !mid.contains("row 21"),
        "drag-down should have scrolled past row 21:\n{mid}"
    );

    // Now drag back UP past the top edge (rect top is row 1) — each
    // event retreats scroll_y by 1.
    for _ in 0..10 {
        engine
            .handle_mouse(MouseMessage {
                kind: MouseKind::Drag,
                x: 2,
                // y above the rect entirely (rect top is row 1); y=0
                // is past the top.
                y: 0,
                button: Some("left"),
                mods: vec![],
            })
            .expect("drag-up");
    }
    let post = render_str(&mut engine);
    // After drag-down clamp at scroll_y=23 then 10× drag-up: scroll_y =
    // max(23 - 10, 0) = 13. Visible rows 14..20.
    assert!(
        post.contains("row 14"),
        "drag past the top edge should auto-scroll the transcript up; row 14 expected in view, got:\n{post}"
    );
    // row 28 (visible mid-drag) should be gone again.
    assert!(
        !post.contains("row 28"),
        "drag-up should have left row 28 behind:\n{post}"
    );

    // Release — clears the in-flight selection and the captured
    // drag-origin scrollable key. Subsequent Drag events with no
    // active selection are silent no-ops on the auto-scroll path.
    engine
        .handle_mouse(MouseMessage {
            kind: MouseKind::Up,
            x: 2,
            y: 0,
            button: Some("left"),
            mods: vec![],
        })
        .expect("up");
}

#[test]
fn drag_inside_viewport_does_not_auto_scroll() {
    // Sanity: while the drag-y stays inside the scrollable's painted
    // rect AND outside the edge zone, scroll_y does not change. The
    // selection still extends, but the transcript stays put.
    let mut engine = Engine::new(40, 8).expect("engine");
    engine.load_scenario(SCROLLABLE_SCENARIO).expect("scenario");
    let _ = render_str(&mut engine);
    // Wheel-up to leave the bottom-pin so a stray scroll would be
    // observable through the offset text.
    engine
        .handle_mouse(MouseMessage {
            kind: MouseKind::Wheel,
            x: 1,
            y: 4,
            button: Some("up"),
            mods: vec![],
        })
        .expect("wheel-up");
    let pre = render_str(&mut engine);
    assert!(
        pre.contains("offset: 20"),
        "expected offset 20 pre-drag; got:\n{pre}"
    );
    let pre_snap = engine.snapshot();

    engine
        .handle_mouse(MouseMessage {
            kind: MouseKind::Click,
            x: 2,
            y: 4,
            button: Some("left"),
            mods: vec![],
        })
        .expect("click");
    // y=3,4,5 — all comfortably inside the rect (rows 1..7) and outside
    // both edge zones (top edge ≤ 1, bottom edge ≥ 6).
    for y in [3, 4, 5] {
        engine
            .handle_mouse(MouseMessage {
                kind: MouseKind::Drag,
                x: 2,
                y,
                button: Some("left"),
                mods: vec![],
            })
            .expect("drag-mid");
    }
    let post = render_str(&mut engine);
    let post_snap = engine.snapshot();
    // No offset change — the offset text only updates via on_scroll
    // (wheel path), but if the auto-scroll path bumped scroll_y the
    // visible row range would shift. Compare snapshots: the row content
    // must be identical pre vs post.
    let extract_rows = |snap: &str| -> Vec<String> {
        snap.lines()
            .filter(|l| l.contains("row "))
            .map(|s| s.to_string())
            .collect()
    };
    assert_eq!(
        extract_rows(&pre_snap),
        extract_rows(&post_snap),
        "drag inside viewport must not move scroll_y; pre:\n{pre}\npost:\n{post}"
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
