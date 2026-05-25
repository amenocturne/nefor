//! `tui.scrollable` per-instance state + scroll arithmetic.
//!
//! Browser-like overflow: the child is laid out under unbounded vertical
//! constraints (so it returns its natural content height), then a viewport
//! window of the parent's height is taken from the child starting at
//! `scroll_y`. Wheel events auto-scroll; keyboard scrolling stays in Lua's
//! domain (per spec — shortcuts are entirely Lua's responsibility).
//!
//! State preservation: the reconciler matches `(type_tag, key)`, so the
//! scroll offset survives `view` rebuilds. The state-mutation surface is
//! intentionally tiny — a single `scroll_y` cell, plus enough cached
//! geometry from the last layout for clamp + stick-to-end.
//!
//! Stick-to-end model: per spec ("end = chat-style auto-pin"), the widget
//! tracks whether the user *was* at the bottom on the previous render. If
//! they were, and content grew, the new `scroll_y` is pinned to the new
//! `scroll_y_max`. Once the user scrolls away from the bottom, the sticky
//! flag clears and content growth no longer drags them along.

/// Per-item geometry cache for virtual-scroll widgets. Holds heights,
/// cumulative y positions (including gaps), and total content height.
/// Updated incrementally by `tui.virtual_scroll_prepare`. Heights
/// come from Lua's `tui.measure()` — exact layout-pass values, not
/// estimates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GeoCache {
    pub heights: Vec<u16>,
    pub cumul: Vec<u32>,
    pub total: u32,
}

/// Per-instance state preserved across `view` rebuilds via the reconciler.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScrollableState {
    /// Vertical scroll offset, in cells, into the child's content. `0` =
    /// top of content visible at top of viewport.
    pub scroll_y: u16,
    /// Horizontal scroll offset. Plumbed through for forward compatibility
    /// — phase 5a only honors vertical scrolling, but the field is here so
    /// the public API (`scroll_to`, `scroll_position`) does not need a
    /// future signature break.
    pub scroll_x: u16,
    /// Content height used for scroll math (max scroll, scrollbar).
    /// When virtual_content_height is set, this holds the virtual value
    /// (stable, estimate-based). Otherwise holds the measured child height.
    pub content_height: u16,
    /// Actual measured child height from the layout pass. Used by the
    /// paint pass for the child rect so content isn't clipped when actual
    /// height exceeds the virtual content_height.
    pub measured_content_height: u16,
    /// Most recent viewport height (the scrollable's own measured size).
    pub viewport_height: u16,
    /// Whether the prior render had `scroll_y == scroll_y_max` (i.e. the
    /// viewport sat at the bottom). Drives `stick_to = "end"` auto-pin
    /// when content grows. `true` for first paint so chat-style transcripts
    /// land pinned.
    pub was_at_end: bool,
    /// Whether the prior render had `scroll_y == 0` (top). Drives
    /// `stick_to = "start"` so an explicit start-anchor stays put when
    /// content grows above what was visible.
    pub was_at_start: bool,
    /// Whether the state has been seeded by at least one paint pass. Used
    /// to detect first-paint so `stick_to = "end"` lands at the bottom on
    /// initial mount even before the user scrolls.
    pub seeded: bool,
    /// Inner content width from the most recent layout pass (post-scrollbar
    /// gutter subtraction). Exposed to Lua via `tui.scrollable_inner_width`
    /// so height measurement uses the exact same width as real layout.
    pub inner_width: u16,
}

impl ScrollableState {
    /// Current maximum scroll offset given cached geometry. Returns `0`
    /// when content fits inside the viewport.
    pub fn scroll_y_max(&self) -> u16 {
        self.content_height.saturating_sub(self.viewport_height)
    }
}

/// Scrollbar visibility policy from the description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollbarMode {
    Auto,
    Always,
    Never,
}

/// Stick-to behavior from the description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StickTo {
    Start,
    End,
}

/// Default rows-per-wheel-notch (matches the legacy chat plugin's choice).
pub const WHEEL_STEP_ROWS: u16 = 3;

/// Rows the auto-scroll-while-dragging path advances per fresh `Drag`
/// event past (or near) the edge. One row keeps the scroll smooth
/// relative to mouse movement: a 30-row drag past the bottom advances
/// `scroll_y` by ~30, which in chat / transcript surfaces matches the
/// natural "selection follows the cursor as content scrolls into view".
pub const DRAG_AUTO_SCROLL_STEP: u16 = 1;

/// Edge zone (in rows from the top / bottom of the painted rect) where
/// a `Drag` event triggers an auto-scroll tick. `0` would only trigger
/// past the rect entirely (a strict "past-the-edge" interpretation);
/// `1` matches what browsers expose — drag onto the last visible row
/// AND past it both kick the auto-scroll, so the user doesn't have to
/// land the cursor exactly past the boundary to start scrolling.
pub const DRAG_AUTO_SCROLL_EDGE_ROWS: u16 = 1;

/// Minimum gap between continuous-tick auto-scroll advances while the
/// user holds the cursor motionless past the edge of a captured
/// scrollable. Drives the latch's tick gate: every animation frame
/// that's at least this many ms past the previous tick advances
/// `scroll_y` by `DRAG_AUTO_SCROLL_STEP`. 60ms ≈ 16 rows/sec, matching
/// typical text-editor auto-scroll speed — fast enough that long
/// selections feel responsive, slow enough to stay controllable. The
/// 60Hz animation tick (~16ms) is too fast on its own; this gate
/// throttles it down to something the human eye can track.
pub const DRAG_AUTO_SCROLL_LATCH_INTERVAL_MS: u64 = 60;

/// Apply a wheel notch to `state`, clamping against the cached geometry.
/// Positive `delta` scrolls down (toward end); negative scrolls up.
///
/// Also refreshes the `was_at_end` / `was_at_start` bookkeeping so the
/// next paint pass observes the user's new position immediately. Without
/// this, a wheel scroll from the bottom would clamp `scroll_y` correctly
/// but leave `was_at_end = true` from the prior frame, and the next
/// `paint_scrollable` would re-pin to the bottom under `stick_to = end`
/// — making the transcript look "not scrollable" until content grew.
/// The Lua API path (`apply_scroll_command`) used to mirror this update
/// itself; sinking it here keeps wheel and `tui.scroll_by` symmetric.
pub fn scroll_by_signed(state: &mut ScrollableState, delta: i32) {
    let max = state.scroll_y_max();
    let next = (state.scroll_y as i32)
        .saturating_add(delta)
        .clamp(0, max as i32);
    state.scroll_y = next as u16;
    state.was_at_end = state.scroll_y == max;
    state.was_at_start = state.scroll_y == 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scroll_y_max_zero_when_content_fits() {
        let s = ScrollableState {
            content_height: 10,
            viewport_height: 20,
            ..Default::default()
        };
        assert_eq!(s.scroll_y_max(), 0);
    }

    #[test]
    fn scroll_y_max_positive_when_content_overflows() {
        let s = ScrollableState {
            content_height: 100,
            viewport_height: 20,
            ..Default::default()
        };
        assert_eq!(s.scroll_y_max(), 80);
    }

    #[test]
    fn scroll_by_signed_clamps_to_max() {
        let mut s = ScrollableState {
            scroll_y: 0,
            content_height: 50,
            viewport_height: 10,
            ..Default::default()
        };
        scroll_by_signed(&mut s, 100);
        assert_eq!(s.scroll_y, 40);
    }

    #[test]
    fn scroll_by_signed_clamps_to_zero() {
        let mut s = ScrollableState {
            scroll_y: 5,
            content_height: 50,
            viewport_height: 10,
            ..Default::default()
        };
        scroll_by_signed(&mut s, -100);
        assert_eq!(s.scroll_y, 0);
    }

    #[test]
    fn scroll_by_signed_zero_max_is_pinned_at_zero() {
        let mut s = ScrollableState {
            scroll_y: 0,
            content_height: 5,
            viewport_height: 10,
            ..Default::default()
        };
        scroll_by_signed(&mut s, 100);
        assert_eq!(s.scroll_y, 0);
    }

    #[test]
    fn scroll_by_signed_clears_was_at_end_when_user_scrolls_up() {
        // Bug-fix coverage: wheel-up from the bottom must drop the
        // `was_at_end` flag so `paint_scrollable` doesn't re-pin to
        // bottom under `stick_to = end`. Pre-fix this flag stayed sticky
        // and the transcript appeared "not scrollable".
        let mut s = ScrollableState {
            scroll_y: 40,
            content_height: 50,
            viewport_height: 10,
            was_at_end: true,
            was_at_start: false,
            seeded: true,
            ..Default::default()
        };
        scroll_by_signed(&mut s, -3);
        assert_eq!(s.scroll_y, 37);
        assert!(
            !s.was_at_end,
            "scrolling up off the bottom must clear was_at_end"
        );
        assert!(!s.was_at_start);
    }

    #[test]
    fn scroll_by_signed_sets_was_at_start_when_user_hits_top() {
        let mut s = ScrollableState {
            scroll_y: 2,
            content_height: 50,
            viewport_height: 10,
            was_at_end: false,
            was_at_start: false,
            seeded: true,
            ..Default::default()
        };
        scroll_by_signed(&mut s, -100);
        assert_eq!(s.scroll_y, 0);
        assert!(s.was_at_start, "landing at top must set was_at_start");
    }

    #[test]
    fn scroll_by_signed_sets_was_at_end_when_user_scrolls_to_bottom() {
        let mut s = ScrollableState {
            scroll_y: 5,
            content_height: 50,
            viewport_height: 10,
            was_at_end: false,
            was_at_start: false,
            seeded: true,
            ..Default::default()
        };
        scroll_by_signed(&mut s, 100);
        assert_eq!(s.scroll_y, 40);
        assert!(s.was_at_end, "landing at bottom must set was_at_end");
    }
}
