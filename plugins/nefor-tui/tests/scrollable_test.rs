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

use std::time::Duration;

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
    let _ = render_str(&mut engine);
    let post = engine.snapshot();
    // After drag-down clamp at scroll_y=23 then 10× drag-up: scroll_y =
    // max(23 - 10, 0) = 13. Visible rows 14..20. Read from
    // `engine.snapshot()` (plain-text framebuffer) rather than the
    // ANSI-byte render: the in-flight selection highlight may inject
    // mid-token SGR escapes ("row 14" → "ro\x1b[7mw 14") that would
    // defeat a literal `contains` check on the byte stream.
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

/// Scrollable scenario that mirrors the standard one but also captures
/// the most recent `mouse.selection.text` into state. The captured text
/// is mirrored into the global `LAST_SELECTION` cell via a custom
/// `selection.notify` envelope dispatched from the Lua side — tests
/// inspect that cell directly rather than re-parsing the rendered view
/// (which fights `tui.text`'s wrapping). The scenario still keeps 30
/// distinct `row N` lines so the assertions can pick identifying tokens
/// out of the captured text.
const SCROLLABLE_SELECTION_SCENARIO: &str = r#"
    tui.start {
      initial_state = {
        rows   = 30,
        offset = 0,
      },
      view = function(s)
        local kids = {}
        for i = 1, s.rows do
          kids[#kids + 1] = tui.text { content = "row " .. i }
        end
        return tui.column { gap = 0, children = {
          tui.text { content = "offset: " .. tostring(s.offset) },
          tui.expanded {
            child = tui.scrollable {
              key        = "log",
              child      = tui.column { gap = 0, children = kids },
              stick_to   = "end",
              on_scroll  = "log.scrolled",
              scrollbar  = "auto",
              selectable = true,
            },
          },
        }}
      end,
      update = function(msg, s)
        if msg.kind == "log.scrolled" then
          return { rows = s.rows, offset = msg.offset }, {}
        elseif msg.kind == "mouse.selection" then
          -- Echo the captured text out as an emit so the integration
          -- test can read it directly off `take_emit_queue`. Avoids the
          -- noise of word-wrapping a multi-line copy back into the
          -- visible frame.
          tui.emit { kind = "selection.captured", text = msg.text or "" }
          return s, {}
        elseif msg.kind == "key.pageup" then
          tui.scroll_by("log", -10); return s, {}
        elseif msg.kind == "key.pagedown" then
          tui.scroll_by("log", 10); return s, {}
        end
        return s, {}
      end,
    }
"#;

/// Drain `take_emit_queue` and return the most recent `selection.captured`
/// `text` field — the Lua-visible mirror of the engine's `mouse.selection`
/// dispatch. Returns `None` when no selection has been captured since the
/// last drain.
fn last_captured_selection(engine: &mut Engine) -> Option<String> {
    let queue = engine.take_emit_queue();
    queue
        .into_iter()
        .filter_map(|(_, body)| {
            let kind = body.get("kind")?.as_str()?;
            if kind == "selection.captured" {
                body.get("text")?.as_str().map(|s| s.to_string())
            } else {
                None
            }
        })
        .next_back()
}

/// Helper: drive a click → drags → release sequence on the engine,
/// rendering after each drag event so the framebuffer reflects
/// auto-scroll mid-flight (matching production behaviour where the
/// renderer ticks at frame rate while the user drags). Without this the
/// framebuffer stays stuck on the pre-drag scroll position and the
/// screen-coord copy path ironically "works" by reading stale cells.
fn drag_select(engine: &mut Engine, click: (u16, u16), drags: &[(u16, u16)], release: (u16, u16)) {
    engine
        .handle_mouse(MouseMessage {
            kind: MouseKind::Click,
            x: click.0,
            y: click.1,
            button: Some("left"),
            mods: vec![],
        })
        .expect("click");
    let _ = engine.render_if_dirty().expect("render after click");
    for &(x, y) in drags {
        engine
            .handle_mouse(MouseMessage {
                kind: MouseKind::Drag,
                x,
                y,
                button: Some("left"),
                mods: vec![],
            })
            .expect("drag");
        let _ = engine.render_if_dirty().expect("render after drag");
    }
    engine
        .handle_mouse(MouseMessage {
            kind: MouseKind::Up,
            x: release.0,
            y: release.1,
            button: Some("left"),
            mods: vec![],
        })
        .expect("up");
}

/// The bug the user reported, distilled: drag-selection past the bottom
/// of the scrollable's viewport auto-scrolls the content (good), but on
/// release the copied selection only contains the cells that happen to
/// be visible at release-time (bad — the original anchor row, having
/// scrolled out of view, gets dropped).
///
/// Pre-fix the engine reads selection text from the framebuffer (screen
/// cells) at mouse-up — which means the anchor's transcript content has
/// long since scrolled past the top of the viewport and is gone from the
/// frame. Post-fix the engine should resolve the anchor + drag endpoint
/// in *content* coordinates (transcript line, column-in-content) so the
/// auto-scroll between Click and Up doesn't drop content the user
/// dragged across.
///
/// Repro: 30-row transcript, viewport 7 rows, scroll headroom 23. Wheel-up
/// 5 notches (15 rows) so we sit at offset 8 with rows 9..15 visible.
/// Click on row 9 (transcript anchor at top of viewport), then drag past
/// the bottom edge enough times to scroll the anchor off-screen. On Up,
/// the copied selection MUST include the row 9 marker — that's the
/// content the user originally clicked on.
#[test]
fn drag_past_bottom_with_auto_scroll_copies_full_anchored_range() {
    let mut engine = Engine::new(40, 8).expect("engine");
    engine
        .load_scenario(SCROLLABLE_SELECTION_SCENARIO)
        .expect("scenario");
    let _ = render_str(&mut engine);

    // Wheel-up by 15 rows total (5 notches × 3) — leaves us at offset 8,
    // visible rows 9..15. Pre-drag we have headroom both ways for the
    // auto-scroll path to walk into.
    for _ in 0..5 {
        engine
            .handle_mouse(MouseMessage {
                kind: MouseKind::Wheel,
                x: 1,
                y: 4,
                button: Some("up"),
                mods: vec![],
            })
            .expect("wheel-up");
    }
    let pre = render_str(&mut engine);
    assert!(
        pre.contains("offset: 8"),
        "expected wheel-up to land at offset 8 pre-drag; got:\n{pre}"
    );
    assert!(
        pre.contains("row 9"),
        "pre-drag frame should show row 9 as top of viewport:\n{pre}"
    );

    // Click on the start of row 9 (y=1, x=0 — first row of the
    // scrollable's painted rect), drag past the bottom edge 20 times so
    // the anchor scrolls out of view, then release.
    let drags: Vec<(u16, u16)> = (0..20).map(|_| (5, 10)).collect();
    drag_select(&mut engine, (0, 1), &drags, (5, 10));
    let captured = last_captured_selection(&mut engine).expect("mouse.selection should have fired");

    // The originally-clicked anchor row (row 9) must be present in the
    // captured text even though it scrolled out of view between Click
    // and Up — the heart of the bug.
    assert!(
        captured.contains("row 9"),
        "copied selection must contain the anchor row (row 9) even after \
         auto-scroll moved it out of view; got:\n{captured:?}"
    );
}

/// Static drag entirely inside the visible viewport — no auto-scroll
/// kicks in, so the copy path's behavior must match the legacy
/// screen-coord extraction. Regression guard: don't break the easy case
/// while fixing the auto-scroll case.
#[test]
fn drag_inside_viewport_copies_visible_range_unchanged() {
    let mut engine = Engine::new(40, 8).expect("engine");
    engine
        .load_scenario(SCROLLABLE_SELECTION_SCENARIO)
        .expect("scenario");
    let _ = render_str(&mut engine);
    // Wheel-up by 6 rows so we sit at offset 17 — gives a stable visible
    // window (rows 18..24) for the assertion.
    for _ in 0..2 {
        engine
            .handle_mouse(MouseMessage {
                kind: MouseKind::Wheel,
                x: 1,
                y: 4,
                button: Some("up"),
                mods: vec![],
            })
            .expect("wheel-up");
    }
    let pre = render_str(&mut engine);
    assert!(
        pre.contains("offset: 17"),
        "expected offset 17 pre-drag; got:\n{pre}"
    );

    // Click at (0, 2) — start of "row 19" — drag downward but stay
    // inside the viewport AND outside the edge zone. The scrollable's
    // painted rect is rows 1..=7; with `DRAG_AUTO_SCROLL_EDGE_ROWS = 1`
    // the bottom edge zone is y >= 7, so y=6 (row 23) is the deepest
    // non-triggering drag. No auto-scroll should fire across this drag.
    drag_select(&mut engine, (0, 2), &[(3, 3), (3, 4), (3, 5)], (6, 6));
    let captured = last_captured_selection(&mut engine).expect("mouse.selection should fire");
    // Anchor row (19) and drag-end row (23) must both appear in the
    // captured text — copy of an in-viewport drag must not regress.
    assert!(
        captured.contains("row 19"),
        "in-viewport selection must include the anchor row (row 19); got:\n{captured:?}"
    );
    assert!(
        captured.contains("row 23"),
        "in-viewport selection must include the drag-end row (row 23); got:\n{captured:?}"
    );
}

/// Drag past the TOP edge — symmetric to `drag_past_bottom...`. Click
/// near the bottom of the viewport, drag past the top so the auto-scroll
/// retreats and the original anchor is now below the visible region.
/// The copied selection must still carry the anchor row.
#[test]
fn drag_past_top_with_auto_scroll_copies_full_anchored_range() {
    let mut engine = Engine::new(40, 8).expect("engine");
    engine
        .load_scenario(SCROLLABLE_SELECTION_SCENARIO)
        .expect("scenario");
    let _ = render_str(&mut engine);
    // Initial frame is pinned to bottom (offset 23, visible rows 24..30).
    // Wheel-up two notches to land at offset 17 — gives headroom upward.
    for _ in 0..2 {
        engine
            .handle_mouse(MouseMessage {
                kind: MouseKind::Wheel,
                x: 1,
                y: 4,
                button: Some("up"),
                mods: vec![],
            })
            .expect("wheel-up");
    }
    let pre = render_str(&mut engine);
    assert!(
        pre.contains("offset: 17"),
        "expected offset 17 pre-drag; got:\n{pre}"
    );
    assert!(
        pre.contains("row 24"),
        "pre-drag frame should show row 24 as bottom of viewport:\n{pre}"
    );

    // Click at the bottom of the viewport (y=7 — last row of the
    // scrollable's painted rect; row 24 of the transcript). Anchor at
    // x=6 so the captured row carries "row 24" past the trailing trim.
    // Drag past the top edge (y=0) 20 times to drive auto-scroll
    // upward, then release.
    let drags: Vec<(u16, u16)> = (0..20).map(|_| (0, 0)).collect();
    drag_select(&mut engine, (6, 7), &drags, (0, 0));
    let captured = last_captured_selection(&mut engine).expect("mouse.selection should fire");
    assert!(
        captured.contains("row 24"),
        "copied selection must contain the anchor row (row 24) even after \
         auto-scroll moved it out of view; got:\n{captured:?}"
    );
}

// ── selectable opt-in: per-widget scoping ─────────────────────────────
//
// A scrollable's `selectable = true` flag opts it into drag-to-select.
// Clicks inside fire a selection; clicks elsewhere don't open one. The
// scenario below paints a column where:
// - Row 0 is a non-selectable status text (`status: ...`).
// - Rows 1..7 hold a `tui.row { left | right }`. The left half is a
//   `selectable = true` scrollable named "chat" (30 numbered rows). The
//   right half is a `selectable = false` scrollable named "side" (20
//   numbered rows). Both have keys so they participate in hit-testing.
//
// At a 40×8 frame: status is row 0; row body occupies rows 1..7 (7
// rows). Each half-column is 20 cells wide (40 / 2). The chat scrollable
// owns cells (col 0..=19, row 1..=7); the side scrollable owns cells
// (col 20..=39, row 1..=7). The non-selectable status row is y=0.

const SELECTABLE_LAYOUT_SCENARIO: &str = r#"
    tui.start {
      initial_state = {
        chat_rows = 30,
        side_rows = 20,
      },
      view = function(s)
        local chat_kids = {}
        for i = 1, s.chat_rows do
          chat_kids[#chat_kids + 1] = tui.text { content = "chat " .. i }
        end
        local side_kids = {}
        for i = 1, s.side_rows do
          side_kids[#side_kids + 1] = tui.text { content = "side " .. i }
        end
        return tui.column { gap = 0, children = {
          tui.text { content = "status: ready" },
          tui.expanded {
            child = tui.row { gap = 0, children = {
              tui.expanded {
                child = tui.scrollable {
                  key        = "chat",
                  child      = tui.column { gap = 0, children = chat_kids },
                  stick_to   = "end",
                  scrollbar  = "never",
                  selectable = true,
                },
              },
              tui.expanded {
                child = tui.scrollable {
                  key        = "side",
                  child      = tui.column { gap = 0, children = side_kids },
                  stick_to   = "end",
                  scrollbar  = "never",
                  -- selectable defaults to false: clicks/drags do not open selections.
                },
              },
            }},
          },
        }}
      end,
      update = function(msg, s)
        if msg.kind == "mouse.selection" then
          tui.emit { kind = "selection.captured", text = msg.text or "" }
        end
        return s, {}
      end,
    }
"#;

/// Helper: click without releasing — drives the auto-scroll path's
/// in-flight scenarios that don't end with mouse-up. Tests that need
/// a finalised selection use `drag_select` instead.
fn click_only(engine: &mut Engine, x: u16, y: u16) {
    engine
        .handle_mouse(MouseMessage {
            kind: MouseKind::Click,
            x,
            y,
            button: Some("left"),
            mods: vec![],
        })
        .expect("click");
}

fn release_only(engine: &mut Engine, x: u16, y: u16) {
    engine
        .handle_mouse(MouseMessage {
            kind: MouseKind::Up,
            x,
            y,
            button: Some("left"),
            mods: vec![],
        })
        .expect("up");
}

/// Wheel-up the named scrollable to take it off the bottom-pin, so the
/// resulting visible window is deterministic for cell-coord assertions.
fn wheel_up_n(engine: &mut Engine, x: u16, y: u16, n: usize) {
    for _ in 0..n {
        engine
            .handle_mouse(MouseMessage {
                kind: MouseKind::Wheel,
                x,
                y,
                button: Some("up"),
                mods: vec![],
            })
            .expect("wheel-up");
        let _ = engine.render_if_dirty();
    }
}

/// Drag from the selectable chat scrollable out into the non-selectable
/// sidebar. The drag should clamp to the chat's painted rect (col 0..=19);
/// the copied text must NOT contain any "side N" rows from the sidebar.
#[test]
fn drag_from_selectable_into_nonselectable_clamps_to_origin() {
    let mut engine = Engine::new(40, 8).expect("engine");
    engine
        .load_scenario(SELECTABLE_LAYOUT_SCENARIO)
        .expect("scenario");
    let _ = render_str(&mut engine);

    // Move chat off bottom-pin so we have a stable visible window.
    // chat scroll_y_max = 30 - 7 = 23. wheel-up 5×3 = 15 rows → offset 8,
    // visible chat rows 9..=15 at row positions 1..=7.
    wheel_up_n(&mut engine, 5, 4, 5);
    // Same for sidebar: side scroll_y_max = 20 - 7 = 13. wheel-up 4×3
    // → offset 1, visible side rows 2..=8.
    wheel_up_n(&mut engine, 25, 4, 4);

    // Click inside chat at (col=0, row=1) — anchor at the start of "chat 9"
    // so the assertion below sees the full token even after row-trim.
    // Drag into the sidebar (col=30, row=4) — geometrically inside side's
    // rect, but the captured selection origin is chat. The drag should
    // clamp to chat's rect (col cap at 19; rows stay valid).
    drag_select(&mut engine, (0, 1), &[(15, 2), (25, 3), (30, 4)], (30, 4));
    let captured =
        last_captured_selection(&mut engine).expect("mouse.selection should fire on chat origin");
    assert!(
        captured.contains("chat 9"),
        "drag from chat anchor must include the chat anchor row in copy; \
         got:\n{captured:?}"
    );
    // The sidebar's content must NOT leak into the copy — the drag was
    // clamped to the chat's painted rect (col <= 19), so cells past the
    // boundary shouldn't appear regardless of where the cursor wandered.
    assert!(
        !captured.contains("side "),
        "drag clamped to selectable chat rect must not include sidebar \
         text; got:\n{captured:?}"
    );
}

/// Click on a non-selectable area (the status row at y=0 / the sidebar)
/// must NOT open a selection. Drag + release should produce no
/// `mouse.selection` envelope.
#[test]
fn click_on_nonselectable_widget_does_not_capture_selection() {
    let mut engine = Engine::new(40, 8).expect("engine");
    engine
        .load_scenario(SELECTABLE_LAYOUT_SCENARIO)
        .expect("scenario");
    let _ = render_str(&mut engine);
    // Drain any startup emits — only the post-drag captures matter below.
    let _ = engine.take_emit_queue();

    // Drag entirely inside the sidebar (right half, col >= 20).
    // Sidebar is `selectable = false` so this opens no selection.
    drag_select(&mut engine, (25, 2), &[(28, 3), (30, 4)], (30, 5));
    assert!(
        last_captured_selection(&mut engine).is_none(),
        "drag inside non-selectable sidebar should not fire mouse.selection"
    );

    // Drag on the status row (y=0) — a non-scrollable, no `selectable`
    // widget. Same expectation: no selection.
    drag_select(&mut engine, (1, 0), &[(5, 0), (10, 0)], (12, 0));
    assert!(
        last_captured_selection(&mut engine).is_none(),
        "drag on the non-selectable status row should not fire mouse.selection"
    );
}

/// Highlight paints only inside the captured selectable's rect. Drag
/// from inside chat past the right edge of its painted rect — the
/// drag's geometric range extends past col 19, but the post-render
/// framebuffer must only carry the reverse-video SGR within the chat
/// rect. The sidebar cells under the geometric range stay un-highlighted.
#[test]
fn selection_highlight_clips_to_captured_widget_rect() {
    let mut engine = Engine::new(40, 8).expect("engine");
    engine
        .load_scenario(SELECTABLE_LAYOUT_SCENARIO)
        .expect("scenario");
    let _ = render_str(&mut engine);
    // Take chat off bottom-pin so cells are stable for snapshot assertions.
    wheel_up_n(&mut engine, 5, 4, 5);

    // Click inside chat (col 2, row 1), drag past the right edge of
    // the chat rect (col 19) into the sidebar (col 30, same row).
    click_only(&mut engine, 2, 1);
    let _ = engine.render_if_dirty();
    engine
        .handle_mouse(MouseMessage {
            kind: MouseKind::Drag,
            x: 30,
            y: 1,
            button: Some("left"),
            mods: vec![],
        })
        .expect("drag");
    let _ = engine.render_if_dirty();

    let snap = engine.snapshot_ansi();
    // Split the snapshot into rows so we can inspect row 1 (the drag y)
    // independently of the surrounding rows.
    let row1 = snap.lines().nth(1).expect("row 1");
    // The chat cells (cols 0..=19) should carry the reverse-video SGR.
    // The sidebar half (cols 20..=39) sits geometrically under the drag's
    // range but lives outside chat's rect — its cells must stay un-reversed.
    //
    // Strategy: row 1 must contain at least one "side " token (the sidebar's
    // visible row on this y) AND that token must NOT be wrapped in a
    // reverse-SGR. The simplest invariant: locate the "side " substring's
    // start, then ensure no `;7m` or `\x1b[7m` opens between the last SGR
    // boundary before "side " and the substring itself.
    assert!(
        row1.contains("side "),
        "row 1 should still show sidebar content (snap):\n{snap:?}"
    );

    // The strict structural check: extract the styled text of the row,
    // walk SGR-by-SGR, and verify no cell with a "side"-prefixed glyph
    // is currently inside a reverse-video segment. Reverse video uses
    // either bare `[7m` or `;7m` inside an SGR sequence.
    let row1_string = row1.to_string();
    let mut reverse_active = false;
    let mut byte_idx = 0;
    let mut found_unreversed_side = false;
    while byte_idx < row1_string.len() {
        let rest = &row1_string[byte_idx..];
        if let Some(esc_at) = rest.find('\x1b') {
            let visible_chunk = &rest[..esc_at];
            // Did the sidebar text appear in this visible chunk while
            // reverse_active was off? If yes, the highlight clipped.
            if !reverse_active && visible_chunk.contains("side ") {
                found_unreversed_side = true;
            }
            // Advance past the SGR sequence and update reverse_active.
            let sgr_start = byte_idx + esc_at;
            let sgr_tail = &row1_string[sgr_start..];
            if let Some(end_idx) = sgr_tail.find('m') {
                let sgr_body = &sgr_tail[..=end_idx];
                if sgr_body.contains("[0m") || sgr_body == "\x1b[m" {
                    reverse_active = false;
                } else if sgr_body.contains(";7m") || sgr_body.contains("[7m") {
                    reverse_active = true;
                } else if sgr_body.contains(";27m") || sgr_body.contains("[27m") {
                    reverse_active = false;
                }
                byte_idx = sgr_start + end_idx + 1;
            } else {
                break;
            }
        } else {
            // Trailing visible chunk past the last SGR — same check.
            if !reverse_active && rest.contains("side ") {
                found_unreversed_side = true;
            }
            break;
        }
    }
    assert!(
        found_unreversed_side,
        "sidebar cells outside chat's painted rect must NOT carry the \
         reverse-video highlight; raw row:\n{row1_string:?}"
    );

    // Sanity check that the highlight DID paint inside chat's rect: at
    // least one reverse-SGR must appear before the cells past col 19.
    assert!(
        row1_string.contains("\x1b[7m") || row1_string.contains(";7m"),
        "highlight should still paint inside the chat rect; raw row:\n{row1_string:?}"
    );

    // Release to keep the engine in a clean state for any follow-up.
    release_only(&mut engine, 30, 1);
}

/// Two adjacent selectable widgets, drag from A into B. The selection
/// stays scoped to A (the captured origin) — B's content must not enter
/// the copy. This pins the cross-widget no-selection rule under the
/// scoping model.
#[test]
fn drag_across_two_adjacent_selectables_clamps_to_origin() {
    const TWO_SELECTABLES: &str = r#"
        tui.start {
          initial_state = {},
          view = function(_)
            local left_kids = {}
            for i = 1, 30 do
              left_kids[#left_kids + 1] = tui.text { content = "left " .. i }
            end
            local right_kids = {}
            for i = 1, 30 do
              right_kids[#right_kids + 1] = tui.text { content = "right " .. i }
            end
            return tui.row { gap = 0, children = {
              tui.expanded {
                child = tui.scrollable {
                  key        = "left",
                  child      = tui.column { gap = 0, children = left_kids },
                  stick_to   = "end",
                  scrollbar  = "never",
                  selectable = true,
                },
              },
              tui.expanded {
                child = tui.scrollable {
                  key        = "right",
                  child      = tui.column { gap = 0, children = right_kids },
                  stick_to   = "end",
                  scrollbar  = "never",
                  selectable = true,
                },
              },
            }}
          end,
          update = function(msg, s)
            if msg.kind == "mouse.selection" then
              tui.emit { kind = "selection.captured", text = msg.text or "" }
            end
            return s, {}
          end,
        }
    "#;
    let mut engine = Engine::new(40, 8).expect("engine");
    engine.load_scenario(TWO_SELECTABLES).expect("scenario");
    let _ = render_str(&mut engine);

    // Wheel both off the bottom-pin so the visible windows are stable.
    // 30 rows in 8-row viewport → max = 22. wheel-up 5×3 = 15 → offset 7,
    // rows 8..=14 visible (rows 0..=6 of the rect's 8 rows hold those
    // viewport rows; row 7 of the layout is row 14 of the transcript).
    wheel_up_n(&mut engine, 5, 4, 5);
    wheel_up_n(&mut engine, 25, 4, 5);

    // Click in the LEFT scrollable, drag into the RIGHT scrollable, release.
    drag_select(&mut engine, (2, 0), &[(15, 1), (25, 2), (30, 3)], (30, 3));
    let captured =
        last_captured_selection(&mut engine).expect("mouse.selection should fire on left origin");
    assert!(
        captured.contains("left "),
        "drag from left origin must include left content in copy; got:\n{captured:?}"
    );
    assert!(
        !captured.contains("right "),
        "drag from left origin into right widget must NOT include right \
         content (selection clamps to origin); got:\n{captured:?}"
    );
}

// ── highlight in content coords ───────────────────────────────────────
//
// Pre-fix the highlight painted screen-coord cells: anchor's screen y
// got clamped to row 0 when it scrolled out of frame, but anchor's col
// stayed, producing the "half-selected top line" the user reported. Fix
// renders the highlight in content coords: each visible cell of the
// captured widget's rect resolves to its content-coord row, and the
// in-range predicate runs in content order. Once the anchor's content
// row scrolls out of view, the new top visible row's content row sits
// strictly between anchor and drag in line-flow order, so it's painted
// fully reverse-video — the user's expectation of "rows above the
// cursor are fully selected".

/// Walk a row of the styled snapshot and return the count of cells the
/// renderer painted with the reverse-video SGR active. Used by the
/// content-coord highlight tests below as an aggregate witness — a
/// fully-highlighted row should report ~`width` reversed cells, a
/// half-highlighted row reports a smaller count. The snapshot's per-row
/// SGR shape is "RESET ... [styled-segment] ... RESET", so we walk the
/// byte stream tracking whether the current style carries reverse.
fn count_reverse_cells_on_row(engine: &mut Engine, screen_row: u16) -> u16 {
    use unicode_width::UnicodeWidthStr;
    let bytes = engine
        .render_if_dirty()
        .expect("render-if-dirty")
        .unwrap_or_default();
    if bytes.is_empty() {
        // Nothing dirty — the snapshot is still the prev frame.
    }
    let snap = engine.snapshot_ansi();
    let row = match snap.lines().nth(screen_row as usize) {
        Some(r) => r.to_string(),
        None => return 0,
    };
    let mut reverse_active = false;
    let mut count: u16 = 0;
    let mut i = 0;
    while i < row.len() {
        let rest = &row[i..];
        if let Some(esc_at) = rest.find('\x1b') {
            // Visible chunk before the next SGR.
            let visible = &rest[..esc_at];
            if reverse_active {
                count = count.saturating_add(visible.width() as u16);
            }
            let sgr_start = i + esc_at;
            let sgr_tail = &row[sgr_start..];
            if let Some(end_idx) = sgr_tail.find('m') {
                let body = &sgr_tail[..=end_idx];
                if body.contains("[0m") || body == "\x1b[m" {
                    reverse_active = false;
                } else if body.contains(";7m") || body.contains("[7m") {
                    reverse_active = true;
                } else if body.contains(";27m") || body.contains("[27m") {
                    reverse_active = false;
                }
                i = sgr_start + end_idx + 1;
            } else {
                break;
            }
        } else {
            if reverse_active {
                count = count.saturating_add(rest.width() as u16);
            }
            break;
        }
    }
    count
}

/// Anchor mid-line, drag past the bottom past the anchor row's screen
/// position. Pre-fix the screen-coord highlight clamped the anchor's
/// row to 0 (its row scrolled past the top edge) but kept its col, so
/// the top visible row painted reverse "from anchor.col onwards" —
/// the half-selected look. Post-fix the new top-visible row's
/// content-coord sits strictly between anchor's content row and drag's
/// content row, so it's a middle row → fully reversed.
#[test]
fn anchor_scrolls_out_of_view_top_row_paints_fully_reversed() {
    let mut engine = Engine::new(40, 8).expect("engine");
    engine
        .load_scenario(SCROLLABLE_SELECTION_SCENARIO)
        .expect("scenario");
    let _ = render_str(&mut engine);
    // Wheel-up to land at offset 8 (visible rows 9..15).
    for _ in 0..5 {
        engine
            .handle_mouse(MouseMessage {
                kind: MouseKind::Wheel,
                x: 1,
                y: 4,
                button: Some("up"),
                mods: vec![],
            })
            .expect("wheel-up");
    }
    let _ = render_str(&mut engine);
    // Click mid-line at (col=5, screen_row=2 → content_row 10), then drag
    // past the bottom edge enough times that scroll_y advances past
    // content_row 10 — anchor scrolls past the top of the viewport.
    // Don't release — we want to inspect mid-drag highlight.
    engine
        .handle_mouse(MouseMessage {
            kind: MouseKind::Click,
            x: 5,
            y: 2,
            button: Some("left"),
            mods: vec![],
        })
        .expect("click");
    let _ = engine.render_if_dirty();
    // Many drags past bottom so the anchor's content row scrolls off-screen.
    for _ in 0..20 {
        engine
            .handle_mouse(MouseMessage {
                kind: MouseKind::Drag,
                x: 5,
                y: 10,
                button: Some("left"),
                mods: vec![],
            })
            .expect("drag");
        let _ = engine.render_if_dirty();
    }
    // Top visible row (screen row 1) should be fully reverse-video — its
    // content-coord row is strictly between anchor (which scrolled past
    // the top) and drag (past the visible region), so the line-flow shape
    // marks it a middle row → full width. Width is 40 cells; the
    // scrollbar gutter on the rightmost cell stays un-reversed (paint
    // happens before the highlight runs and the gutter renders the bar
    // glyph; but the predicate will still flip it), so the count is the
    // rect's content width (~39 cells).
    let reversed = count_reverse_cells_on_row(&mut engine, 1);
    assert!(
        reversed >= 30,
        "top visible row should paint ~full-width reverse-video once \
         the anchor scrolls out of view (got {reversed} reversed cells)"
    );
}

// ── continuous-tick auto-scroll latch ─────────────────────────────────
//
// crossterm only emits `Drag` events on cursor MOTION. The per-Drag
// auto-scroll path from `28071ab` advances `scroll_y` exactly once per
// drag event past the edge — so a motionless cursor sitting past the
// edge produced no further scrolls. The latch path arms when a drag
// lands in the edge zone, then advances `scroll_y` on every animation
// tick (gated by an interval so the speed feels controllable). Tests
// drive the engine clock with `advance_time` to step the gate
// deterministically.

/// Drag past the bottom edge then advance the engine clock several
/// latch-intervals. Each interval should advance `scroll_y` by one row,
/// pulling new transcript rows into view even though no further `Drag`
/// events fire. The selection's content-end advances naturally as the
/// content under the (motionless) cursor scrolls.
#[test]
fn motionless_cursor_past_bottom_keeps_scrolling_via_latch() {
    let mut engine = Engine::new(40, 8).expect("engine");
    engine
        .load_scenario(SCROLLABLE_SELECTION_SCENARIO)
        .expect("scenario");
    let _ = render_str(&mut engine);
    // Wheel-up to land at offset 8 (visible rows 9..15) — gives plenty
    // of headroom for the latch to walk into without hitting scroll_y_max.
    for _ in 0..5 {
        engine
            .handle_mouse(MouseMessage {
                kind: MouseKind::Wheel,
                x: 1,
                y: 4,
                button: Some("up"),
                mods: vec![],
            })
            .expect("wheel-up");
    }
    let pre = render_str(&mut engine);
    assert!(
        pre.contains("offset: 8"),
        "expected offset 8 pre-drag; got:\n{pre}"
    );

    // Click inside the rect, then ONE `Drag` past the bottom edge — that
    // arms the latch and advances scroll_y by 1 (the existing per-drag
    // path). After the drag, the cursor doesn't move further: no
    // additional `Drag` events fire, only animation ticks.
    engine
        .handle_mouse(MouseMessage {
            kind: MouseKind::Click,
            x: 0,
            y: 1,
            button: Some("left"),
            mods: vec![],
        })
        .expect("click");
    engine
        .handle_mouse(MouseMessage {
            kind: MouseKind::Drag,
            x: 5,
            y: 10,
            button: Some("left"),
            mods: vec![],
        })
        .expect("drag-down");
    let _ = render_str(&mut engine);
    let post_drag = engine.snapshot();
    // The `offset:` label only updates via the wheel path's `on_scroll`
    // event; auto-scroll mutates `scroll_y` directly, so visible rows
    // are the load-bearing signal. Drag in the edge zone advances
    // scroll_y by 1: visible window 9..15 → 10..16.
    assert!(
        post_drag.contains("row 16"),
        "first Drag past bottom should bring row 16 into view (scroll_y 8 → 9); got:\n{post_drag}"
    );
    assert!(
        engine.has_drag_auto_scroll_latch(),
        "Drag in the bottom edge zone must arm the auto-scroll latch"
    );

    // Now advance the clock by N latch-intervals and call the latched
    // tick on each — production wires this through the binary's 60Hz
    // animation tick. We expect scroll_y to advance by N rows.
    let interval_ms = 60u64;
    for _ in 0..5 {
        engine.advance_time(Duration::from_millis(interval_ms));
        engine.drive_drag_auto_scroll_tick();
        let _ = engine.render_if_dirty();
    }
    let post = engine.snapshot();
    // Five latched ticks → scroll_y 9 + 5 = 14, visible rows 15..21.
    assert!(
        post.contains("row 21"),
        "five latched ticks should pull row 21 into view (scroll_y 9 + 5 = 14); got:\n{post}"
    );
    assert!(
        !post.contains("row 9 ") && !post.contains("row 9\n") && !post.ends_with("row 9"),
        "five latched ticks should have scrolled row 9 out of view; got:\n{post}"
    );
}

/// A `Drag` back into the viewport (off the edge) clears the latch, so
/// subsequent ticks do NOT advance `scroll_y`.
#[test]
fn drag_back_into_viewport_clears_latch() {
    let mut engine = Engine::new(40, 8).expect("engine");
    engine
        .load_scenario(SCROLLABLE_SELECTION_SCENARIO)
        .expect("scenario");
    let _ = render_str(&mut engine);
    // Take off the bottom-pin.
    for _ in 0..5 {
        engine
            .handle_mouse(MouseMessage {
                kind: MouseKind::Wheel,
                x: 1,
                y: 4,
                button: Some("up"),
                mods: vec![],
            })
            .expect("wheel-up");
    }
    let _ = render_str(&mut engine);
    // Click + drag past bottom edge → latch armed.
    engine
        .handle_mouse(MouseMessage {
            kind: MouseKind::Click,
            x: 0,
            y: 1,
            button: Some("left"),
            mods: vec![],
        })
        .expect("click");
    engine
        .handle_mouse(MouseMessage {
            kind: MouseKind::Drag,
            x: 5,
            y: 10,
            button: Some("left"),
            mods: vec![],
        })
        .expect("drag-edge");
    assert!(engine.has_drag_auto_scroll_latch());
    // Drag back inside the viewport, comfortably away from edge zones.
    engine
        .handle_mouse(MouseMessage {
            kind: MouseKind::Drag,
            x: 5,
            y: 4,
            button: Some("left"),
            mods: vec![],
        })
        .expect("drag-mid");
    assert!(
        !engine.has_drag_auto_scroll_latch(),
        "Drag back into the viewport (away from the edge zone) must clear the latch"
    );
    // Snapshot offset, advance the clock, confirm no further scroll.
    let pre = render_str(&mut engine);
    for _ in 0..5 {
        engine.advance_time(Duration::from_millis(60));
        engine.drive_drag_auto_scroll_tick();
    }
    let post = render_str(&mut engine);
    let extract_offset =
        |s: &str| -> Option<String> { s.lines().find(|l| l.contains("offset:")).map(String::from) };
    assert_eq!(
        extract_offset(&pre),
        extract_offset(&post),
        "no latch → ticks must not move scroll_y; pre:\n{pre}\npost:\n{post}"
    );
}

/// Mouse-up while the latch is armed must clear it — no scroll on the
/// next tick.
#[test]
fn mouse_up_clears_latch() {
    let mut engine = Engine::new(40, 8).expect("engine");
    engine
        .load_scenario(SCROLLABLE_SELECTION_SCENARIO)
        .expect("scenario");
    let _ = render_str(&mut engine);
    for _ in 0..5 {
        engine
            .handle_mouse(MouseMessage {
                kind: MouseKind::Wheel,
                x: 1,
                y: 4,
                button: Some("up"),
                mods: vec![],
            })
            .expect("wheel-up");
    }
    let _ = render_str(&mut engine);
    engine
        .handle_mouse(MouseMessage {
            kind: MouseKind::Click,
            x: 0,
            y: 1,
            button: Some("left"),
            mods: vec![],
        })
        .expect("click");
    engine
        .handle_mouse(MouseMessage {
            kind: MouseKind::Drag,
            x: 5,
            y: 10,
            button: Some("left"),
            mods: vec![],
        })
        .expect("drag-edge");
    assert!(engine.has_drag_auto_scroll_latch());
    engine
        .handle_mouse(MouseMessage {
            kind: MouseKind::Up,
            x: 5,
            y: 10,
            button: Some("left"),
            mods: vec![],
        })
        .expect("up");
    assert!(
        !engine.has_drag_auto_scroll_latch(),
        "mouse-up must clear the auto-scroll latch"
    );
}

/// The latch's content-coord drag end advances as content scrolls under
/// a motionless cursor — when the latch ticks pull row N into view past
/// the cursor's screen position, the selection's drag end resolves to
/// row N (not the original click row + zero).
///
/// Verifies that when the user releases AFTER the latch has scrolled the
/// content but BEFORE the cursor moves further, the captured selection
/// covers the full anchored range (anchor + scrolled-into-view rows).
#[test]
fn latch_advances_drag_in_content_under_motionless_cursor() {
    let mut engine = Engine::new(40, 8).expect("engine");
    engine
        .load_scenario(SCROLLABLE_SELECTION_SCENARIO)
        .expect("scenario");
    let _ = render_str(&mut engine);
    for _ in 0..5 {
        engine
            .handle_mouse(MouseMessage {
                kind: MouseKind::Wheel,
                x: 1,
                y: 4,
                button: Some("up"),
                mods: vec![],
            })
            .expect("wheel-up");
    }
    let _ = render_str(&mut engine);
    // Click on row 9 (top of viewport) → anchor at content row 8.
    engine
        .handle_mouse(MouseMessage {
            kind: MouseKind::Click,
            x: 0,
            y: 1,
            button: Some("left"),
            mods: vec![],
        })
        .expect("click");
    // One Drag past the bottom edge, then sit motionless — latch advances
    // scroll_y on subsequent ticks.
    engine
        .handle_mouse(MouseMessage {
            kind: MouseKind::Drag,
            x: 5,
            y: 10,
            button: Some("left"),
            mods: vec![],
        })
        .expect("drag-edge");
    let _ = engine.render_if_dirty();
    // Tick the latch enough times to pull rows 16..20 into view past
    // the cursor — drag's content row should track them.
    for _ in 0..15 {
        engine.advance_time(Duration::from_millis(60));
        engine.drive_drag_auto_scroll_tick();
        let _ = engine.render_if_dirty();
    }
    // Release without further mouse motion — copy must include rows the
    // latch scrolled past.
    engine
        .handle_mouse(MouseMessage {
            kind: MouseKind::Up,
            x: 5,
            y: 10,
            button: Some("left"),
            mods: vec![],
        })
        .expect("up");
    let captured =
        last_captured_selection(&mut engine).expect("mouse.selection should fire on release");
    assert!(
        captured.contains("row 9"),
        "captured copy must contain anchor row 9; got:\n{captured:?}"
    );
    // After 1 (drag) + 15 (latch) = 16 row scrolls past offset 8 = scroll_y 23
    // (clamped at scroll_y_max = 30 - 7 = 23). Visible rows 24..30. The
    // cursor at y=10 (past rect bottom row 7) clamps to last visible row =
    // row 30. So the selection covers rows 9..30 — assert a row well past
    // the original viewport is included.
    assert!(
        captured.contains("row 20"),
        "latch should pull content past the original viewport into the \
         captured copy; got:\n{captured:?}"
    );
}

// ── non-scrollable selectables (Column / TextInput) ──────────────────
//
// `selectable = true` extends to non-scrollable widgets so the user can
// drag-copy from a sidebar column or the prompt text_input. Non-scrolling
// surfaces have no virtual content extent — content-coord = screen-coord
// relative to the rect's top-left, so the copy path paints the widget
// itself (not a child) into a rect-sized scratch buffer and runs the
// existing extract_selection_text.

const NON_SCROLLABLE_SELECTABLE_SCENARIO: &str = r#"
    tui.start {
      initial_state = {},
      view = function(_)
        return tui.row { gap = 0, children = {
          tui.expanded {
            child = tui.column {
              key        = "main",
              selectable = true,
              gap        = 0,
              children   = {
                tui.text { content = "main alpha" },
                tui.text { content = "main bravo" },
                tui.text { content = "main charlie" },
              },
            },
          },
          tui.expanded {
            child = tui.column {
              key        = "sidebar",
              selectable = true,
              gap        = 0,
              children   = {
                tui.text { content = "side foo" },
                tui.text { content = "side bar" },
                tui.text { content = "side baz" },
              },
            },
          },
        }}
      end,
      update = function(msg, s)
        if msg.kind == "mouse.selection" then
          tui.emit { kind = "selection.captured", text = msg.text or "" }
        end
        return s, {}
      end,
    }
"#;

/// Drag inside a non-scrollable selectable column. The copy returns
/// the cells under the drag's content-coord range — content rows are
/// the column's visible rows (no virtual extent).
#[test]
fn drag_inside_selectable_column_copies_visible_text() {
    let mut engine = Engine::new(40, 4).expect("engine");
    engine
        .load_scenario(NON_SCROLLABLE_SELECTABLE_SCENARIO)
        .expect("scenario");
    let _ = render_str(&mut engine);
    // Layout: 40-wide, 4-tall. Two columns side by side, each 20-wide.
    // Main column owns cols 0..=19, rows 0..=2. Drag from row 0 col 5
    // ("alpha" starts ~col 5 of "main alpha") to row 2 col 12 (end of
    // "main charlie") — captures content under the drag range.
    drag_select(&mut engine, (0, 0), &[(10, 1), (15, 2)], (15, 2));
    let captured = last_captured_selection(&mut engine).expect("mouse.selection should fire");
    assert!(
        captured.contains("main alpha"),
        "captured copy must include the anchor row 'main alpha'; got:\n{captured:?}"
    );
    assert!(
        captured.contains("main charlie"),
        "captured copy must include the drag-end row 'main charlie'; got:\n{captured:?}"
    );
}

/// Drag from a non-scrollable selectable into a different non-scrollable
/// selectable: the copy clamps to the captured origin's rect. The
/// sidebar text must NOT leak into the main-column copy.
#[test]
fn drag_from_one_selectable_column_into_another_clamps_to_origin() {
    let mut engine = Engine::new(40, 4).expect("engine");
    engine
        .load_scenario(NON_SCROLLABLE_SELECTABLE_SCENARIO)
        .expect("scenario");
    let _ = render_str(&mut engine);
    // Click in main column (col 5, row 0), drag into sidebar (col 30, row 2).
    drag_select(&mut engine, (5, 0), &[(15, 1), (25, 2), (30, 2)], (30, 2));
    let captured = last_captured_selection(&mut engine).expect("mouse.selection should fire");
    assert!(
        captured.contains("main"),
        "drag from main origin must include main content; got:\n{captured:?}"
    );
    assert!(
        !captured.contains("side "),
        "drag clamped to main rect must NOT include sidebar text; got:\n{captured:?}"
    );
}

const SELECTABLE_TEXT_INPUT_SCENARIO: &str = r#"
    tui.start {
      initial_state = { value = "hello world from prompt" },
      view = function(s)
        return tui.column { gap = 0, children = {
          tui.text { content = "header" },
          tui.text_input {
            key        = "prompt",
            value      = s.value,
            focused    = true,
            on_change  = "input.changed",
            min_lines  = 1,
            max_lines  = 1,
            selectable = true,
          },
        }}
      end,
      update = function(msg, s)
        if msg.kind == "mouse.selection" then
          tui.emit { kind = "selection.captured", text = msg.text or "" }
        elseif msg.kind == "input.changed" then
          return { value = msg.value }, {}
        end
        return s, {}
      end,
    }
"#;

/// Drag inside a `selectable = true` text_input: the copy returns the
/// visible text under the drag (the prompt's displayed value).
#[test]
fn drag_inside_selectable_text_input_copies_visible_text() {
    let mut engine = Engine::new(40, 3).expect("engine");
    engine
        .load_scenario(SELECTABLE_TEXT_INPUT_SCENARIO)
        .expect("scenario");
    let _ = render_str(&mut engine);
    // Header on row 0; text_input on row 1. Drag across the input's row.
    drag_select(&mut engine, (0, 1), &[(10, 1), (20, 1)], (20, 1));
    let captured = last_captured_selection(&mut engine).expect("mouse.selection should fire");
    // The captured text should include "hello world" (or a substring of
    // "hello world from prompt") — exact match depends on the input's
    // visible window, but the displayed prefix must be present.
    assert!(
        captured.contains("hello world") || captured.contains("hello"),
        "drag inside selectable text_input must capture displayed text; got:\n{captured:?}"
    );
}

/// Typing into a `selectable = true` focused text_input still inserts
/// at the cursor — selection-on-drag must not regress the editing flow.
#[test]
fn selectable_text_input_still_accepts_typing_keys() {
    let mut engine = Engine::new(40, 3).expect("engine");
    engine
        .load_scenario(SELECTABLE_TEXT_INPUT_SCENARIO)
        .expect("scenario");
    let _ = render_str(&mut engine);
    // Press 'X' — the focused input should absorb it via the editing
    // path and on_change should fire with the new value.
    engine.handle_key(key("X")).expect("key X");
    let _ = render_str(&mut engine);
    // The state's `value` should now end with "X" (cursor was at end
    // after the initial sync).
    let snap = engine.snapshot();
    assert!(
        snap.contains("hello world from promptX") || snap.contains("X"),
        "typing into a selectable text_input must still insert at cursor; \
         frame:\n{snap}"
    );
}

/// Click on a selectable text_input + immediate release (no drag): no
/// `mouse.selection` envelope fires (selection only opens on actual
/// drag, matching the legacy contract for scrollable selectables).
#[test]
fn click_release_on_selectable_text_input_emits_no_selection() {
    let mut engine = Engine::new(40, 3).expect("engine");
    engine
        .load_scenario(SELECTABLE_TEXT_INPUT_SCENARIO)
        .expect("scenario");
    let _ = render_str(&mut engine);
    // Drain any startup emits.
    let _ = engine.take_emit_queue();
    // Click then release at the same cell — no Drag in between.
    engine
        .handle_mouse(MouseMessage {
            kind: MouseKind::Click,
            x: 5,
            y: 1,
            button: Some("left"),
            mods: vec![],
        })
        .expect("click");
    engine
        .handle_mouse(MouseMessage {
            kind: MouseKind::Up,
            x: 5,
            y: 1,
            button: Some("left"),
            mods: vec![],
        })
        .expect("up");
    assert!(
        last_captured_selection(&mut engine).is_none(),
        "click+release without drag must not fire mouse.selection"
    );
}
