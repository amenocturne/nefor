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

/// Per-instance state preserved across `view` rebuilds via the reconciler.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScrollableState {
    /// Vertical scroll offset, in cells, into the child's content. `0` =
    /// top of content visible at top of viewport.
    pub scroll_y: u16,
    /// Horizontal scroll offset. Plumbed through for forward compatibility
    /// — phase 5a only honors vertical scrolling, but the field is here so
    /// the public API (`scroll_to`, `scroll_position`) does not need a
    /// future signature break.
    pub scroll_x: u16,
    /// Most recent measured content height (from the prior layout pass).
    /// Used by clamp + scrollbar geometry.
    pub content_height: u16,
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

/// Apply a wheel notch to `state`, clamping against the cached geometry.
/// Positive `delta` scrolls down (toward end); negative scrolls up.
pub fn scroll_by_signed(state: &mut ScrollableState, delta: i32) {
    let max = state.scroll_y_max();
    let next = (state.scroll_y as i32)
        .saturating_add(delta)
        .clamp(0, max as i32);
    state.scroll_y = next as u16;
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
}
