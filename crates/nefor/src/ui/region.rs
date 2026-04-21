//! Region layout primitives.
//!
//! Per spec §`nefor` binary "UI primitives": widgets claim one of a small set
//! of regions (top / bottom / left / right / center), and the core lays them
//! out deterministically. Core doesn't know what a "statusline" is — it only
//! knows that *something* asked for `Top(1)`.
//!
//! This MVP models exactly the shapes the spec calls out; extending it (e.g.,
//! stacked top bars, percentage-based sizes) is follow-up scope, not core.
//!
//! `Top` / `Bottom` / `Left` / `Right` variants have no in-binary caller yet
//! — the placeholder widget uses `Center`. They're part of the public API
//! the Lua binding (next commits) will expose.
#![allow(dead_code)]

use ratatui::layout::Rect;

/// Where a widget sits in the frame.
///
/// The `u16` payload on the fixed-size variants is the claimed extent in
/// cells — rows for horizontal bars (`Top`, `Bottom`), columns for vertical
/// bars (`Left`, `Right`). `Center` takes whatever's left after the fixed
/// regions are carved out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Region {
    /// Pinned to the top of the frame; `u16` is the height in rows.
    Top(u16),
    /// Pinned to the bottom of the frame; `u16` is the height in rows.
    Bottom(u16),
    /// Pinned to the bottom of the frame; height is whatever the widget asks
    /// for via [`Widget::measure`] each frame. Typical use: a chat input line
    /// that grows to 2/3/… rows as the draft wraps. Resolved into
    /// [`Region::Bottom`] by [`WidgetRegistry::render_all`] *before* [`layout`]
    /// runs, so [`layout`] itself never sees this variant.
    ///
    /// [`Widget::measure`]: crate::ui::widget::Widget::measure
    /// [`WidgetRegistry::render_all`]: crate::ui::widget::WidgetRegistry::render_all
    BottomAuto,
    /// Pinned to the left edge; `u16` is the width in columns.
    Left(u16),
    /// Pinned to the right edge; `u16` is the width in columns.
    Right(u16),
    /// Fills whatever space remains after the fixed regions are claimed.
    Center,
}

/// Compute the rect each region occupies inside `frame_area`.
///
/// Two-pass, deliberately order-independent: pass 1 lets every fixed region
/// (`Top` / `Bottom` / `Left` / `Right`) carve its slice in declaration order;
/// pass 2 assigns the leftover rect to any `Center`. Registration order of
/// `Center` relative to the fixed regions does *not* affect the layout — a
/// plugin that registers `Center` before `Bottom` still leaves room for the
/// bottom bar. Without this, a natural registration order (title / transcript
/// / input) silently ate the input's region.
///
/// The return vector matches `regions` one-for-one. If a fixed region's claim
/// exceeds the remaining area it gets clamped; later fixed regions may end up
/// with [`Rect::ZERO`]. `Center` gets [`Rect::ZERO`] when nothing is left.
pub fn layout(frame_area: Rect, regions: &[Region]) -> Vec<Rect> {
    let mut out = vec![Rect::ZERO; regions.len()];
    let mut remaining = frame_area;
    let mut center_indices: Vec<usize> = Vec::new();

    for (i, region) in regions.iter().enumerate() {
        match *region {
            Region::Top(h) => out[i] = carve_top(&mut remaining, h),
            Region::Bottom(h) => out[i] = carve_bottom(&mut remaining, h),
            Region::Left(w) => out[i] = carve_left(&mut remaining, w),
            Region::Right(w) => out[i] = carve_right(&mut remaining, w),
            Region::Center => center_indices.push(i),
            // Expected to be resolved to Bottom(h) by WidgetRegistry::render_all
            // before layout is called. If we see one here a dev missed a
            // resolution step — stay out of the way and log so it's findable.
            Region::BottomAuto => {
                tracing::error!("Region::BottomAuto reached layout() unresolved");
                out[i] = Rect::ZERO;
            }
        }
    }

    let center_rect = if remaining.width == 0 || remaining.height == 0 {
        Rect::ZERO
    } else {
        remaining
    };
    // `WidgetRegistry::register` already treats a second `Center` as a conflict,
    // so in practice this loop runs at most once. If it ever runs twice, both
    // widgets get the same rect — defensible fallback, matches the "fail soft"
    // posture the surrounding doc comment states.
    for idx in center_indices {
        out[idx] = center_rect;
    }

    out
}

fn carve_top(remaining: &mut Rect, h: u16) -> Rect {
    if h == 0 || remaining.height == 0 {
        return Rect::ZERO;
    }
    let take = h.min(remaining.height);
    let taken = Rect {
        x: remaining.x,
        y: remaining.y,
        width: remaining.width,
        height: take,
    };
    remaining.y = remaining.y.saturating_add(take);
    remaining.height -= take;
    taken
}

fn carve_bottom(remaining: &mut Rect, h: u16) -> Rect {
    if h == 0 || remaining.height == 0 {
        return Rect::ZERO;
    }
    let take = h.min(remaining.height);
    let taken = Rect {
        x: remaining.x,
        y: remaining.y + (remaining.height - take),
        width: remaining.width,
        height: take,
    };
    remaining.height -= take;
    taken
}

fn carve_left(remaining: &mut Rect, w: u16) -> Rect {
    if w == 0 || remaining.width == 0 {
        return Rect::ZERO;
    }
    let take = w.min(remaining.width);
    let taken = Rect {
        x: remaining.x,
        y: remaining.y,
        width: take,
        height: remaining.height,
    };
    remaining.x = remaining.x.saturating_add(take);
    remaining.width -= take;
    taken
}

fn carve_right(remaining: &mut Rect, w: u16) -> Rect {
    if w == 0 || remaining.width == 0 {
        return Rect::ZERO;
    }
    let take = w.min(remaining.width);
    let taken = Rect {
        x: remaining.x + (remaining.width - take),
        y: remaining.y,
        width: take,
        height: remaining.height,
    };
    remaining.width -= take;
    taken
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: u16, h: u16) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        }
    }

    #[test]
    fn top_bottom_center_claim_expected_rects() {
        let rects = layout(
            frame(80, 24),
            &[Region::Top(1), Region::Bottom(1), Region::Center],
        );
        assert_eq!(rects.len(), 3);
        assert_eq!(
            rects[0],
            Rect {
                x: 0,
                y: 0,
                width: 80,
                height: 1,
            }
        );
        assert_eq!(
            rects[1],
            Rect {
                x: 0,
                y: 23,
                width: 80,
                height: 1,
            }
        );
        assert_eq!(
            rects[2],
            Rect {
                x: 0,
                y: 1,
                width: 80,
                height: 22,
            }
        );
    }

    #[test]
    fn center_registered_before_fixed_regions_still_carves_correctly() {
        // Declaration order: center, top, bottom. Under order-independent
        // layout the fixed regions still claim their rows; center gets the
        // 22-row remainder regardless of the registration order.
        let rects = layout(
            frame(80, 24),
            &[Region::Center, Region::Top(1), Region::Bottom(2)],
        );
        assert_eq!(rects.len(), 3);
        assert_eq!(
            rects[0],
            Rect {
                x: 0,
                y: 1,
                width: 80,
                height: 21,
            }
        );
        assert_eq!(
            rects[1],
            Rect {
                x: 0,
                y: 0,
                width: 80,
                height: 1,
            }
        );
        assert_eq!(
            rects[2],
            Rect {
                x: 0,
                y: 22,
                width: 80,
                height: 2,
            }
        );
    }

    #[test]
    fn left_right_center_carve_columns() {
        let rects = layout(
            frame(80, 24),
            &[Region::Left(10), Region::Right(5), Region::Center],
        );
        assert_eq!(
            rects[0],
            Rect {
                x: 0,
                y: 0,
                width: 10,
                height: 24,
            }
        );
        assert_eq!(
            rects[1],
            Rect {
                x: 75,
                y: 0,
                width: 5,
                height: 24,
            }
        );
        assert_eq!(
            rects[2],
            Rect {
                x: 10,
                y: 0,
                width: 65,
                height: 24,
            }
        );
    }

    #[test]
    fn oversized_claim_is_clamped_later_regions_get_zero() {
        // Top asks for 30 rows in a 24-row frame. It gets 24; Center gets ZERO.
        let rects = layout(frame(80, 24), &[Region::Top(30), Region::Center]);
        assert_eq!(rects[0].height, 24);
        assert_eq!(rects[1], Rect::ZERO);
    }

    #[test]
    fn zero_sized_region_yields_zero_rect() {
        let rects = layout(frame(80, 24), &[Region::Top(0), Region::Center]);
        assert_eq!(rects[0], Rect::ZERO);
        assert_eq!(rects[1].height, 24);
    }
}
