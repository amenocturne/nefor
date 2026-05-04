//! Mouse event translation + hit-test.
//!
//! Crossterm `MouseEvent` arrives at the binary; we normalize it into
//! [`MouseMessage`] (Lua-friendly shape) and run a hit-test over the
//! reconciled instance tree. The hit-test uses `LayoutResult.painted_rect`
//! captured during `layout::paint`, so it requires at least one render
//! before it can resolve a coord — pre-render mouse events bubble with
//! `target_key = None`.
//!
//! Phase 4 surface (per spec):
//!
//! - `mouse.click` — `Down` button presses. Bubbles to Lua as
//!   `{ kind = "mouse.click", x, y, target_key, button }`.
//! - `mouse.wheel` — scroll events. Bubble; the auto-scroll behaviour
//!   on a `scrollable` lands in phase 5a.
//!
//! Drag-to-select extension: `Down(left)` opens a selection range, `Drag`
//! while the button is held updates the range's end cell, `Up` finalises
//! and dispatches `mouse.selection` to Lua. The renderer paints the
//! selection with terminal "reverse video" so the policy decision (copy?
//! log? ignore?) stays in the user's Lua composition.

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use crate::desc::WidgetDescription;
use crate::instance::{InstanceKind, WidgetInstance};
use crate::layout::Rect;

/// Normalized mouse event the engine routes to Lua. Mirrors the spec's
/// `{ kind = "mouse.click" | "mouse.wheel", x, y, target_key, button? }`
/// shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MouseMessage {
    pub kind: MouseKind,
    pub x: u16,
    pub y: u16,
    pub button: Option<&'static str>,
    pub mods: Vec<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseKind {
    /// Single button-down event. Phase 4 emits `mouse.click` directly on
    /// `Down`; click-vs-drag distinction belongs in Lua user-space.
    Click,
    /// Scroll wheel event (up / down). The scroll-direction lives on the
    /// button slot (`"up"` / `"down"`) for v1 simplicity.
    Wheel,
    /// Drag with a button held — we surface only left-button drags and
    /// use them to extend the engine's selection range. The event does
    /// NOT bubble to Lua during drag; only the final `Up` produces a
    /// `mouse.selection` dispatch.
    Drag,
    /// Button release. Used by the engine to finalise an in-flight
    /// selection (extracts text + dispatches to Lua). Does not bubble
    /// directly to Lua.
    Up,
}

/// Translate a crossterm `MouseEvent` into our normalized form. Returns
/// `None` for events we don't forward (`Moved` w/o a button held, lateral
/// scroll). Drag/Up events for the left button surface so the engine's
/// drag-to-select path can update its selection range; the Lua-visible
/// `mouse.selection` event fires from the engine on `Up`, not from the
/// raw `Drag`/`Up` events themselves.
pub fn from_crossterm(evt: &MouseEvent) -> Option<MouseMessage> {
    let mods = mods_of(evt.modifiers);
    let (kind, button): (MouseKind, Option<&'static str>) = match evt.kind {
        MouseEventKind::Down(b) => (MouseKind::Click, Some(button_name(b))),
        MouseEventKind::ScrollUp => (MouseKind::Wheel, Some("up")),
        MouseEventKind::ScrollDown => (MouseKind::Wheel, Some("down")),
        // Drag / Up: forward only the left-button variant — that's what
        // drives the selection mechanism. Other buttons stay opt-in for
        // later phases.
        MouseEventKind::Drag(b @ MouseButton::Left) => (MouseKind::Drag, Some(button_name(b))),
        MouseEventKind::Up(b @ MouseButton::Left) => (MouseKind::Up, Some(button_name(b))),
        // Up / Drag (non-left) / Moved / lateral scroll: deferred.
        _ => return None,
    };
    Some(MouseMessage {
        kind,
        x: evt.column,
        y: evt.row,
        button,
        mods,
    })
}

fn button_name(b: MouseButton) -> &'static str {
    match b {
        MouseButton::Left => "left",
        MouseButton::Right => "right",
        MouseButton::Middle => "middle",
    }
}

fn mods_of(m: KeyModifiers) -> Vec<&'static str> {
    let mut out = Vec::new();
    if m.contains(KeyModifiers::SHIFT) {
        out.push("shift");
    }
    if m.contains(KeyModifiers::CONTROL) {
        out.push("ctrl");
    }
    if m.contains(KeyModifiers::ALT) {
        out.push("alt");
    }
    if m.contains(KeyModifiers::SUPER) {
        out.push("super");
    }
    out
}

/// Walk the instance tree depth-first and return the deepest user key
/// under `(x, y)`. Returns `None` when the coord falls outside every
/// painted rect, or when the deepest enclosing instance has no user
/// key (per spec, only keyed primitives surface as `target_key`).
pub fn hit_test(root: &WidgetInstance, x: u16, y: u16) -> Option<String> {
    let mut deepest: Option<String> = None;
    walk_hit(root, x, y, &mut deepest);
    deepest
}

fn walk_hit(inst: &WidgetInstance, x: u16, y: u16, out: &mut Option<String>) {
    let Some(rect) = inst.layout.painted_rect else {
        return;
    };
    if !rect_contains(&rect, x, y) {
        return;
    }
    // Update the deepest match before recursing — children below will
    // overwrite if they also contain the coord. Only keyed instances
    // contribute (per spec).
    if let Some(k) = inst.last_desc.user_key() {
        *out = Some(k.to_string());
    }
    for child in inst.children.iter() {
        walk_hit(child, x, y, out);
    }
}

fn rect_contains(rect: &Rect, x: u16, y: u16) -> bool {
    let x_in = x >= rect.col && x < rect.col.saturating_add(rect.width);
    let y_in = y >= rect.row && y < rect.row.saturating_add(rect.height);
    x_in && y_in
}

/// Walk the instance tree depth-first and return the **path** (sequence
/// of child indices from the root) to the deepest `scrollable` whose
/// painted rect contains `(x, y)`. `None` means no scrollable is under
/// the cursor. The caller follows the path with `instance_at_path` to get
/// a mutable handle (the borrow shape mirrors `input_router::route_key`).
pub fn find_scrollable_path(root: &WidgetInstance, x: u16, y: u16) -> Option<Vec<usize>> {
    let mut path: Vec<usize> = Vec::new();
    let mut deepest: Option<Vec<usize>> = None;
    walk_scrollable(root, x, y, &mut path, &mut deepest);
    deepest
}

fn walk_scrollable(
    inst: &WidgetInstance,
    x: u16,
    y: u16,
    path: &mut Vec<usize>,
    out: &mut Option<Vec<usize>>,
) {
    let Some(rect) = inst.layout.painted_rect else {
        return;
    };
    if !rect_contains(&rect, x, y) {
        return;
    }
    if matches!(inst.kind(), InstanceKind::Scrollable) {
        *out = Some(path.clone());
    }
    for (i, child) in inst.children.iter().enumerate() {
        path.push(i);
        walk_scrollable(child, x, y, path, out);
        path.pop();
    }
}

/// Reach into the tree following `path` and return a mutable reference
/// to the targeted instance. Mirrors `input_router::instance_at_path` so
/// callers don't have to import a different helper for the wheel-scroll
/// path. Returns `None` if any step is out of range.
pub fn instance_at_path<'a>(
    root: &'a mut WidgetInstance,
    path: &[usize],
) -> Option<&'a mut WidgetInstance> {
    let mut cur: &mut WidgetInstance = root;
    for &i in path {
        let child = cur.children.get_mut(i)?;
        cur = child;
    }
    Some(cur)
}

/// Convenience helper: build the kind string Lua sees for this event.
/// `Drag` and `Up` deliberately have no Lua-visible counterpart — the
/// engine consumes them internally to drive the selection mechanism and
/// surfaces only the finalised `mouse.selection` event to Lua.
pub fn kind_string(kind: MouseKind) -> &'static str {
    match kind {
        MouseKind::Click => "mouse.click",
        MouseKind::Wheel => "mouse.wheel",
        // The Drag/Up cases are not bubbled directly; this string is
        // a defensive default in case future code paths route them. See
        // `Engine::handle_mouse` for the actual flow.
        MouseKind::Drag => "mouse.drag",
        MouseKind::Up => "mouse.up",
    }
}

/// Cell range produced by a finished drag. Coordinates are absolute
/// terminal cells; the engine extracts text from the framebuffer using
/// `(start_col, start_row) -> (end_col, end_row)` in line-flow order
/// (browser-style: first row from `start_col` to end-of-row, middle rows
/// full width, last row from start-of-row to `end_col`). Both ends are
/// inclusive; the engine resolves an empty drag (start == end) by
/// extracting the single cell's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionRange {
    pub start_col: u16,
    pub start_row: u16,
    pub end_col: u16,
    pub end_row: u16,
}

impl SelectionRange {
    /// Build a range from raw `(start, end)` cell coordinates. The two
    /// ends are normalised so `(start_row, start_col)` always precedes
    /// `(end_row, end_col)` in reading order — the renderer and the text
    /// extractor both rely on that invariant.
    pub fn normalised(start: (u16, u16), end: (u16, u16)) -> Self {
        let (s, e) = if (start.1, start.0) <= (end.1, end.0) {
            (start, end)
        } else {
            (end, start)
        };
        SelectionRange {
            start_col: s.0,
            start_row: s.1,
            end_col: e.0,
            end_row: e.1,
        }
    }

    /// `(col_start, col_end_inclusive)` for `row` under line-flow shape.
    /// Returns `None` when the row sits outside the selection's vertical
    /// span. The end column is inclusive — caller iterates `start..=end`.
    pub fn row_span(&self, row: u16, line_width: u16) -> Option<(u16, u16)> {
        if line_width == 0 {
            return None;
        }
        let last_col = line_width.saturating_sub(1);
        if row < self.start_row || row > self.end_row {
            return None;
        }
        if self.start_row == self.end_row {
            // Single-row selection — the cells between the two columns,
            // honouring whichever end sits leftmost.
            let (a, b) = if self.start_col <= self.end_col {
                (self.start_col, self.end_col)
            } else {
                (self.end_col, self.start_col)
            };
            Some((a.min(last_col), b.min(last_col)))
        } else if row == self.start_row {
            Some((self.start_col.min(last_col), last_col))
        } else if row == self.end_row {
            Some((0, self.end_col.min(last_col)))
        } else {
            Some((0, last_col))
        }
    }
}

/// Whether a description carries a user-supplied `key`. Used in tests.
#[allow(dead_code)]
pub(crate) fn user_key(desc: &WidgetDescription) -> Option<&str> {
    desc.user_key()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desc::{Anchor, Dimension, WidgetDescription, WrapMode};
    use crate::layout::layout_and_paint;
    use crate::reconciler::Reconciler;
    use crate::render::FrameBuffer;
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

    fn text(content: &str, key: Option<&str>) -> WidgetDescription {
        WidgetDescription::Text {
            content: content.into(),
            style: None,
            wrap: WrapMode::Word,
            key: key.map(|s| s.into()),
        }
    }

    fn column(children: Vec<WidgetDescription>, key: Option<&str>) -> WidgetDescription {
        WidgetDescription::Column {
            children,
            gap: 0,
            key: key.map(|s| s.into()),
        }
    }

    fn padding(child: WidgetDescription, value: u16, key: Option<&str>) -> WidgetDescription {
        WidgetDescription::Padding {
            top: value,
            right: value,
            bottom: value,
            left: value,
            child: Box::new(child),
            key: key.map(|s| s.into()),
        }
    }

    fn render_tree(desc: WidgetDescription, w: u16, h: u16) -> Reconciler {
        let mut r = Reconciler::new();
        r.reconcile(desc);
        let mut buf = FrameBuffer::new(w, h);
        let root = r.root.as_mut().unwrap();
        layout_and_paint(root, w, h, &mut buf);
        r
    }

    #[test]
    fn from_crossterm_classifies_click_and_wheel() {
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 4,
            row: 2,
            modifiers: KeyModifiers::NONE,
        };
        let m = from_crossterm(&click).expect("click");
        assert_eq!(m.kind, MouseKind::Click);
        assert_eq!(m.button, Some("left"));
        assert_eq!(m.x, 4);
        assert_eq!(m.y, 2);

        let wheel = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::SHIFT,
        };
        let m = from_crossterm(&wheel).expect("wheel");
        assert_eq!(m.kind, MouseKind::Wheel);
        assert_eq!(m.button, Some("down"));
        assert_eq!(m.mods, vec!["shift"]);
    }

    #[test]
    fn from_crossterm_drops_uninteresting() {
        let evt = MouseEvent {
            kind: MouseEventKind::Moved,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        assert!(from_crossterm(&evt).is_none());
    }

    #[test]
    fn from_crossterm_surfaces_left_drag_and_up() {
        let drag = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 7,
            row: 3,
            modifiers: KeyModifiers::NONE,
        };
        let m = from_crossterm(&drag).expect("drag");
        assert_eq!(m.kind, MouseKind::Drag);
        assert_eq!(m.x, 7);
        assert_eq!(m.y, 3);

        let up = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 9,
            row: 4,
            modifiers: KeyModifiers::NONE,
        };
        let m = from_crossterm(&up).expect("up");
        assert_eq!(m.kind, MouseKind::Up);
        assert_eq!(m.x, 9);
        assert_eq!(m.y, 4);
    }

    #[test]
    fn from_crossterm_drops_non_left_drag_and_up() {
        // Right-button drag/up don't drive selection; leave them out.
        let drag = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Right),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        assert!(from_crossterm(&drag).is_none());
        let up = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Middle),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        assert!(from_crossterm(&up).is_none());
    }

    #[test]
    fn selection_range_row_span_single_row() {
        let r = SelectionRange::normalised((2, 1), (5, 1));
        assert_eq!(r.row_span(0, 10), None);
        assert_eq!(r.row_span(1, 10), Some((2, 5)));
        assert_eq!(r.row_span(2, 10), None);
    }

    #[test]
    fn selection_range_row_span_multi_row_line_flow() {
        // start (4, 1), end (3, 3) — 3 rows total.
        let r = SelectionRange::normalised((4, 1), (3, 3));
        // First row: from start_col to end-of-line.
        assert_eq!(r.row_span(1, 10), Some((4, 9)));
        // Middle row: full width.
        assert_eq!(r.row_span(2, 10), Some((0, 9)));
        // Last row: from start-of-line to end_col.
        assert_eq!(r.row_span(3, 10), Some((0, 3)));
        // Outside: None.
        assert_eq!(r.row_span(0, 10), None);
        assert_eq!(r.row_span(4, 10), None);
    }

    #[test]
    fn selection_range_normalises_reverse_drag() {
        // Drag from (8, 4) up to (2, 1) — should normalise so start
        // comes first in reading order.
        let r = SelectionRange::normalised((8, 4), (2, 1));
        assert_eq!(r.start_row, 1);
        assert_eq!(r.end_row, 4);
        assert_eq!(r.start_col, 2);
        assert_eq!(r.end_col, 8);
    }

    #[test]
    fn hit_test_returns_deepest_keyed_match() {
        // column { padding(value=1) { text "abc" key="t" } key="col" }
        // root rect 10×3, padding shrinks to 8×1 inner, text occupies (1,1..4).
        let r = render_tree(
            column(
                vec![padding(text("abc", Some("t")), 1, Some("pad"))],
                Some("col"),
            ),
            10,
            3,
        );
        let root = r.root.as_ref().unwrap();
        // At (1, 1) inside the padded text — deepest user key is "t".
        assert_eq!(hit_test(root, 1, 1), Some("t".into()));
    }

    #[test]
    fn hit_test_falls_through_unkeyed_to_parent() {
        // padding has key, text does not → click on text returns "pad".
        let r = render_tree(padding(text("abc", None), 1, Some("pad")), 10, 3);
        let root = r.root.as_ref().unwrap();
        assert_eq!(hit_test(root, 1, 1), Some("pad".into()));
    }

    #[test]
    fn hit_test_outside_rect_returns_none() {
        let r = render_tree(text("ab", Some("t")), 10, 3);
        let root = r.root.as_ref().unwrap();
        // y = 5 is outside the 3-row buffer.
        assert_eq!(hit_test(root, 0, 5), None);
    }

    #[test]
    fn hit_test_returns_none_when_no_keyed_ancestor() {
        let r = render_tree(text("ab", None), 10, 3);
        let root = r.root.as_ref().unwrap();
        assert_eq!(hit_test(root, 0, 0), None);
    }

    #[test]
    fn hit_test_resolves_through_anchored_overlay() {
        // anchored center { text "X" key="popup" } over a 11×3 frame.
        let desc = WidgetDescription::Anchored {
            anchor: Anchor::Center,
            offset_x: 0,
            offset_y: 0,
            width: Dimension::Intrinsic,
            height: Dimension::Intrinsic,
            child: Box::new(text("X", Some("popup"))),
            key: None,
        };
        let r = render_tree(desc, 11, 3);
        let root = r.root.as_ref().unwrap();
        // Anchored center: child at (1, 5).
        assert_eq!(hit_test(root, 5, 1), Some("popup".into()));
    }

    #[test]
    fn kind_string_matches_spec() {
        assert_eq!(kind_string(MouseKind::Click), "mouse.click");
        assert_eq!(kind_string(MouseKind::Wheel), "mouse.wheel");
    }

    fn scrollable_desc(child: WidgetDescription, key: &str) -> WidgetDescription {
        WidgetDescription::Scrollable {
            key: Some(key.into()),
            child: Box::new(child),
            stick_to: None,
            on_scroll: None,
            scrollbar: crate::scrollable::ScrollbarMode::Auto,
            style: None,
        }
    }

    #[test]
    fn find_scrollable_path_returns_path_when_cursor_inside() {
        // column { scrollable { text("longish") }, text("trailer") }
        let kids: Vec<_> = (0..15).map(|i| text(&format!("l{i}"), None)).collect();
        let s = scrollable_desc(column(kids, None), "transcript");
        let layout_tree = column(vec![s, text("trailer", None)], None);
        let r = render_tree(layout_tree, 20, 6);
        let root = r.root.as_ref().unwrap();
        // (x=2, y=2) lands inside the scrollable's first 5 rows.
        let path = find_scrollable_path(root, 2, 2).expect("scrollable under cursor");
        // `instance_at_path` retrieves the scrollable instance.
        let mut r2 = r;
        let inst = instance_at_path(r2.root.as_mut().unwrap(), &path).expect("walk path");
        assert_eq!(inst.last_desc.user_key(), Some("transcript"));
    }

    #[test]
    fn find_scrollable_path_returns_none_when_cursor_outside() {
        // Single text at the root: no scrollable in tree.
        let r = render_tree(text("hello", Some("t")), 10, 3);
        let root = r.root.as_ref().unwrap();
        assert!(find_scrollable_path(root, 0, 0).is_none());
    }

    #[test]
    fn find_scrollable_path_picks_deepest_when_nested() {
        // Outer scrollable wraps an inner scrollable. The deepest hit
        // wins so wheel events scroll the closest container under the
        // cursor.
        let inner_kids: Vec<_> = (0..30).map(|i| text(&format!("i{i}"), None)).collect();
        let inner = scrollable_desc(column(inner_kids, None), "inner");
        let outer = scrollable_desc(inner, "outer");
        let r = render_tree(outer, 20, 6);
        let root = r.root.as_ref().unwrap();
        let path = find_scrollable_path(root, 1, 1).expect("path");
        let mut r2 = r;
        let inst = instance_at_path(r2.root.as_mut().unwrap(), &path).expect("walk");
        assert_eq!(inst.last_desc.user_key(), Some("inner"));
    }
}
