//! Widget registry — the Rust side of the `nefor.ui.register_widget` surface.
//!
//! Per spec §Core API Surface (Lua): plugins claim a [`Region`] and supply a
//! renderer. The registry owns the `(Region, Box<dyn Widget>)` pairs and
//! fails *at registration time* if two widgets claim the exact same region —
//! the spec insists conflicts surface eagerly, not at first render.
//!
//! The Lua bindings (widget handles returned to Lua, `invalidate`,
//! `subscribe_key` / `subscribe_resize`) are deferred; this module only
//! provides the Rust API the Lua layer will bind to.
//!
//! `dead_code` is allowed module-wide because `unregister` / `len` /
//! `is_empty` / `WidgetHandle::as_u64` / the stored `Entry::handle` have no
//! in-binary caller yet — the Lua binding (next commits) is their consumer.
#![allow(dead_code)]

use ratatui::layout::Rect;
use ratatui::Frame;

use crate::ui::error::UiError;
use crate::ui::region::{layout, Region};

/// A renderable region on the frame.
///
/// Implementors draw into `area` using `frame`'s widget APIs. The trait is
/// `Send + Sync` because future evolution lets plugins render from the tokio
/// worker pool; the current single-threaded `render_all` doesn't require it,
/// but the bound is part of the public contract and costs us nothing here.
pub trait Widget: Send + Sync {
    /// Render this widget into `area`.
    fn render(&self, frame: &mut Frame<'_>, area: Rect);

    /// How many rows this widget wants when registered under
    /// [`Region::BottomAuto`] (or a future `TopAuto`). Called each frame
    /// *before* layout with the frame width so the widget can answer after
    /// soft-wrap. Default `1` — fine for widgets that never claim auto rows.
    ///
    /// [`Region::BottomAuto`]: crate::ui::region::Region::BottomAuto
    fn measure(&self, _width: u16) -> u16 {
        1
    }
}

/// Opaque handle returned by [`WidgetRegistry::register`].
///
/// Monotonically assigned; callers keep the handle to later [`unregister`].
///
/// [`unregister`]: WidgetRegistry::unregister
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WidgetHandle(u64);

impl WidgetHandle {
    /// The raw monotonic id. Only useful for debug output.
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

struct Entry {
    handle: WidgetHandle,
    region: Region,
    widget: Box<dyn Widget>,
}

/// Holds the set of widgets to render each frame.
///
/// Per spec: "widget region conflicts fail at `register_widget`, not at first
/// render." That rule means the registry is authoritative about legality, and
/// callers can trust that iterating its entries is safe.
pub struct WidgetRegistry {
    entries: Vec<Entry>,
    next_handle: u64,
}

impl WidgetRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            next_handle: 0,
        }
    }

    /// Register `widget` at `region`.
    ///
    /// Fails with [`UiError::WidgetRegionConflict`] if an existing widget
    /// already claims the exact same `Region` variant+value. Different-sized
    /// claims (`Top(1)` vs `Top(2)`) are treated as different regions — two
    /// top bars of different heights almost certainly indicates a plugin bug
    /// *worth reporting*, but the MVP rule is literal equality. Tighter
    /// conflict detection lands when a real use case demands it.
    pub fn register(
        &mut self,
        region: Region,
        widget: Box<dyn Widget>,
    ) -> Result<WidgetHandle, UiError> {
        if self.entries.iter().any(|e| e.region == region) {
            return Err(UiError::WidgetRegionConflict { region });
        }
        let handle = WidgetHandle(self.next_handle);
        self.next_handle += 1;
        self.entries.push(Entry {
            handle,
            region,
            widget,
        });
        Ok(handle)
    }

    /// Remove the widget previously registered with `handle`. No-op if the
    /// handle has already been unregistered.
    pub fn unregister(&mut self, handle: WidgetHandle) {
        self.entries.retain(|e| e.handle != handle);
    }

    /// Number of registered widgets. Mostly used by tests.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether any widgets are registered.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Render every registered widget onto `frame`.
    ///
    /// Layout is computed each frame from the widget regions — frame size can
    /// change between draws (terminal resize), and the layout computation is
    /// cheap relative to the draw itself.
    ///
    /// [`Region::BottomAuto`] is resolved *here* by asking each auto-sized
    /// widget for its desired height via [`Widget::measure`]. The measure is
    /// clamped to `[1, frame_height]` so one widget can't eat the whole screen
    /// by accident. Layout then sees a concrete `Bottom(h)` and never
    /// encounters `BottomAuto`.
    pub fn render_all(&self, frame: &mut Frame<'_>) {
        let frame_area = frame.area();
        let regions: Vec<Region> = self
            .entries
            .iter()
            .map(|e| match e.region {
                Region::BottomAuto => {
                    let want = e.widget.measure(frame_area.width);
                    let clamped = want.clamp(1, frame_area.height.max(1));
                    Region::Bottom(clamped)
                }
                other => other,
            })
            .collect();
        let rects = layout(frame_area, &regions);
        for (entry, rect) in self.entries.iter().zip(rects) {
            entry.widget.render(frame, rect);
        }
    }
}

impl Default for WidgetRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Noop;
    impl Widget for Noop {
        fn render(&self, _frame: &mut Frame<'_>, _area: Rect) {}
    }

    #[test]
    fn register_and_unregister_changes_len() {
        let mut reg = WidgetRegistry::new();
        assert!(reg.is_empty());
        let h = reg
            .register(Region::Top(1), Box::new(Noop))
            .expect("register ok");
        assert_eq!(reg.len(), 1);
        reg.unregister(h);
        assert!(reg.is_empty());
    }

    #[test]
    fn handles_are_monotonic() {
        let mut reg = WidgetRegistry::new();
        let a = reg.register(Region::Top(1), Box::new(Noop)).unwrap();
        let b = reg.register(Region::Bottom(1), Box::new(Noop)).unwrap();
        assert_eq!(a.as_u64(), 0);
        assert_eq!(b.as_u64(), 1);
    }

    #[test]
    fn duplicate_exact_region_is_a_conflict() {
        let mut reg = WidgetRegistry::new();
        reg.register(Region::Top(1), Box::new(Noop)).unwrap();
        let err = reg
            .register(Region::Top(1), Box::new(Noop))
            .expect_err("should conflict");
        assert!(matches!(
            err,
            UiError::WidgetRegionConflict {
                region: Region::Top(1)
            }
        ));
    }

    #[test]
    fn different_sized_top_regions_are_not_conflicts() {
        let mut reg = WidgetRegistry::new();
        reg.register(Region::Top(1), Box::new(Noop)).unwrap();
        // Different size = different region under the MVP rule.
        reg.register(Region::Top(2), Box::new(Noop))
            .expect("different size is not a conflict");
        assert_eq!(reg.len(), 2);
    }

    #[test]
    fn duplicate_center_is_a_conflict() {
        let mut reg = WidgetRegistry::new();
        reg.register(Region::Center, Box::new(Noop)).unwrap();
        let err = reg
            .register(Region::Center, Box::new(Noop))
            .expect_err("center must be singular");
        assert!(matches!(
            err,
            UiError::WidgetRegionConflict {
                region: Region::Center
            }
        ));
    }

    #[test]
    fn unregister_nonexistent_is_noop() {
        let mut reg = WidgetRegistry::new();
        reg.unregister(WidgetHandle(42));
        assert!(reg.is_empty());
    }
}
