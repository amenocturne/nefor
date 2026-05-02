//! Constraints-down / sizes-up two-pass layout engine.
//!
//! Phase 2 replaces phase-1's recursive single-pass top-down sizing with
//! Flutter's constraints-down / sizes-up algorithm:
//!
//! 1. **Measure pass** (`layout`): the parent passes [`Constraints`] to a
//!    child; the child returns its actual [`Size`]. Children may recurse
//!    into their own children with tightened constraints. Layout side-
//!    effects (e.g. each instance's measured size) are stored on the
//!    instance via [`LayoutResult`] for the paint pass to read.
//! 2. **Paint pass** (`paint`): the parent positions the child within its
//!    own rect and asks the child to paint. Width is clamped at every
//!    leaf (spec invariant #3): a leaf may never paint past its allotted
//!    columns.
//!
//! Width is measured in columns, one column per ASCII char. Wide-char
//! correctness via `unicode-width` lands later (still a documented gap in
//! phase 2; the helpers all route through `char_width`).

use unicode_width::UnicodeWidthChar;

use crate::desc::{
    Alignment, Anchor, Dimension, Span, Style, TextInputStyle, WidgetDescription, WrapMode,
};
use crate::instance::{InstanceKind, InstanceState, WidgetInstance};
use crate::render::{Cell, FrameBuffer};

/// Box constraints flowing down from parent to child during the measure
/// pass. A child must return a [`Size`] within these bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Constraints {
    pub min_width: u16,
    pub max_width: u16,
    pub min_height: u16,
    pub max_height: u16,
}

impl Constraints {
    /// "Loose" constraints: any size from `0` up to the given maximums.
    pub fn loose(max_width: u16, max_height: u16) -> Self {
        Constraints {
            min_width: 0,
            max_width,
            min_height: 0,
            max_height,
        }
    }

    /// "Tight" constraints — child is forced to exactly `(width, height)`.
    pub fn tight(width: u16, height: u16) -> Self {
        Constraints {
            min_width: width,
            max_width: width,
            min_height: height,
            max_height: height,
        }
    }

    /// Clamp `size` so each axis sits within `[min, max]`.
    pub fn constrain(&self, size: Size) -> Size {
        Size {
            width: size.width.clamp(self.min_width, self.max_width),
            height: size.height.clamp(self.min_height, self.max_height),
        }
    }
}

/// Result of the measure pass: the actual size the child chose.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Size {
    pub width: u16,
    pub height: u16,
}

/// A positioned rectangle in the frame buffer, used by the paint pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub row: u16,
    pub col: u16,
    pub width: u16,
    pub height: u16,
}

impl Rect {
    pub fn from_origin_size(origin: (u16, u16), size: Size) -> Self {
        Rect {
            row: origin.0,
            col: origin.1,
            width: size.width,
            height: size.height,
        }
    }
}

/// Measure pass — returns the size `inst` would take inside `c`.
///
/// Side-effect: stores the measured size and any per-primitive layout
/// metadata in `inst.layout`, so the paint pass can position children
/// without re-measuring.
pub fn layout(inst: &mut WidgetInstance, c: Constraints) -> Size {
    let size = match inst.kind() {
        InstanceKind::Text => layout_text(inst, c),
        InstanceKind::Spans => layout_spans(inst, c),
        InstanceKind::Markdown => layout_markdown(inst, c),
        InstanceKind::Column => layout_column(inst, c),
        InstanceKind::Row => layout_row(inst, c),
        InstanceKind::Padding => layout_padding(inst, c),
        InstanceKind::Stack => layout_stack(inst, c),
        InstanceKind::Expanded => layout_expanded(inst, c),
        InstanceKind::Spacer => layout_spacer(inst, c),
        InstanceKind::Constrained => layout_constrained(inst, c),
        InstanceKind::Align => layout_align(inst, c),
        InstanceKind::Anchored => layout_anchored(inst, c),
        InstanceKind::TextInput => layout_text_input(inst, c),
    };
    inst.layout.size = size;
    size
}

/// Paint pass — `inst` paints itself into `out` at the given `rect`.
/// `inst.layout` must have been populated by a prior `layout` call. As a
/// side effect, the painted rect is recorded on the instance so the
/// mouse hit-test (and any other post-paint walk) can map a screen
/// coord to the deepest enclosing instance.
pub fn paint(inst: &mut WidgetInstance, rect: Rect, out: &mut FrameBuffer) {
    if rect.width == 0 || rect.height == 0 {
        // Clipped: clear any prior rect so the hit-test doesn't see
        // stale geometry from a previous frame.
        inst.layout.painted_rect = None;
        return;
    }
    inst.layout.painted_rect = Some(rect);
    match inst.kind() {
        InstanceKind::Text => paint_text(inst, rect, out),
        InstanceKind::Spans => paint_spans(inst, rect, out),
        InstanceKind::Markdown => paint_markdown(inst, rect, out),
        InstanceKind::Column => paint_column(inst, rect, out),
        InstanceKind::Row => paint_row(inst, rect, out),
        InstanceKind::Padding => paint_padding(inst, rect, out),
        InstanceKind::Stack => paint_stack(inst, rect, out),
        InstanceKind::Expanded | InstanceKind::Constrained => paint_passthrough(inst, rect, out),
        InstanceKind::Spacer => { /* empty */ }
        InstanceKind::Align => paint_align(inst, rect, out),
        InstanceKind::Anchored => paint_anchored(inst, rect, out),
        InstanceKind::TextInput => paint_text_input(inst, rect, out),
    }
}

// ── Text ─────────────────────────────────────────────────────────────────

fn layout_text(inst: &mut WidgetInstance, c: Constraints) -> Size {
    let (content, wrap) = match &inst.last_desc {
        WidgetDescription::Text { content, wrap, .. } => (content.clone(), *wrap),
        _ => unreachable!("kind/desc mismatch"),
    };
    let rows = wrap_text(&content, c.max_width, wrap);
    let height = rows.len() as u16;
    let width = rows
        .iter()
        .map(|r| string_width(r) as u16)
        .max()
        .unwrap_or(0);
    let raw = Size { width, height };
    c.constrain(raw)
}

fn paint_text(inst: &mut WidgetInstance, rect: Rect, out: &mut FrameBuffer) {
    let (content, style, wrap) = match &inst.last_desc {
        WidgetDescription::Text {
            content,
            style,
            wrap,
            ..
        } => (content.as_str(), style.unwrap_or_default(), *wrap),
        _ => return,
    };
    let rows = wrap_text(content, rect.width, wrap);
    for (i, line) in rows.into_iter().enumerate() {
        if i as u16 >= rect.height {
            break;
        }
        // Spec invariant #3: width-clamped render contract. wrap_text is
        // expected to already produce ≤ rect.width-wide rows; the assert
        // catches algorithm bugs early in debug builds and silently
        // truncates in release so a buggy render never bleeds past the
        // allotted column count.
        let safe = enforce_width_contract(&line, rect.width);
        let row = rect.row.saturating_add(i as u16);
        write_run(out, row, rect.col, &safe, &style);
    }
}

#[cfg(debug_assertions)]
fn enforce_width_contract(line: &str, max_width: u16) -> std::borrow::Cow<'_, str> {
    let w = string_width(line);
    debug_assert!(
        w <= max_width as usize,
        "leaf paint exceeded width: line {w} cols > rect {max_width} cols ({line:?})"
    );
    std::borrow::Cow::Borrowed(line)
}

#[cfg(not(debug_assertions))]
fn enforce_width_contract(line: &str, max_width: u16) -> std::borrow::Cow<'_, str> {
    if string_width(line) > max_width as usize {
        std::borrow::Cow::Owned(take_columns(line, max_width))
    } else {
        std::borrow::Cow::Borrowed(line)
    }
}

// ── Spans ────────────────────────────────────────────────────────────────

fn layout_spans(inst: &mut WidgetInstance, c: Constraints) -> Size {
    let (spans, wrap) = match &inst.last_desc {
        WidgetDescription::Spans { spans, wrap, .. } => (spans.clone(), *wrap),
        _ => unreachable!("kind/desc mismatch"),
    };
    let rows = wrap_styled(&styled_chars_from_spans(&spans), c.max_width, wrap);
    measure_styled_rows(&rows, c)
}

fn paint_spans(inst: &mut WidgetInstance, rect: Rect, out: &mut FrameBuffer) {
    let (spans, wrap) = match &inst.last_desc {
        WidgetDescription::Spans { spans, wrap, .. } => (spans.clone(), *wrap),
        _ => return,
    };
    let rows = wrap_styled(&styled_chars_from_spans(&spans), rect.width, wrap);
    paint_styled_rows(&rows, rect, out);
}

// ── Markdown ─────────────────────────────────────────────────────────────

fn layout_markdown(inst: &mut WidgetInstance, c: Constraints) -> Size {
    let chars = render_markdown_chars(inst);
    let wrap = match &inst.last_desc {
        WidgetDescription::Markdown { wrap, .. } => *wrap,
        _ => WrapMode::Word,
    };
    let rows = wrap_styled(&chars, c.max_width, wrap);
    measure_styled_rows(&rows, c)
}

fn paint_markdown(inst: &mut WidgetInstance, rect: Rect, out: &mut FrameBuffer) {
    let chars = render_markdown_chars(inst);
    let wrap = match &inst.last_desc {
        WidgetDescription::Markdown { wrap, .. } => *wrap,
        _ => WrapMode::Word,
    };
    let rows = wrap_styled(&chars, rect.width, wrap);
    paint_styled_rows(&rows, rect, out);
}

fn render_markdown_chars(inst: &WidgetInstance) -> Vec<StyledChar> {
    match &inst.last_desc {
        WidgetDescription::Markdown { source, theme, .. } => {
            crate::markdown::render_to_styled_chars(source, theme.as_ref())
        }
        _ => Vec::new(),
    }
}

// ── Column ───────────────────────────────────────────────────────────────

fn layout_column(inst: &mut WidgetInstance, c: Constraints) -> Size {
    let gap = match &inst.last_desc {
        WidgetDescription::Column { gap, .. } => *gap,
        _ => unreachable!(),
    };
    flex_layout(inst, c, Axis::Vertical, gap)
}

fn paint_column(inst: &mut WidgetInstance, rect: Rect, out: &mut FrameBuffer) {
    let gap = match &inst.last_desc {
        WidgetDescription::Column { gap, .. } => *gap,
        _ => 0,
    };
    paint_flex(inst, rect, out, Axis::Vertical, gap);
}

// ── Row ──────────────────────────────────────────────────────────────────

fn layout_row(inst: &mut WidgetInstance, c: Constraints) -> Size {
    let gap = match &inst.last_desc {
        WidgetDescription::Row { gap, .. } => *gap,
        _ => unreachable!(),
    };
    flex_layout(inst, c, Axis::Horizontal, gap)
}

fn paint_row(inst: &mut WidgetInstance, rect: Rect, out: &mut FrameBuffer) {
    let gap = match &inst.last_desc {
        WidgetDescription::Row { gap, .. } => *gap,
        _ => 0,
    };
    paint_flex(inst, rect, out, Axis::Horizontal, gap);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Axis {
    Horizontal,
    Vertical,
}

/// Two-pass flex distribution (shared between column + row).
///
/// Pass 1 — measure non-flex children with the parent's cross-axis budget
/// and natural main-axis space. The flex children get whatever main-axis
/// space remains, distributed proportionally.
///
/// Edge cases worth keeping in mind:
///   • If `remaining` is 0 (or negative once gaps are subtracted), every
///     flex child gets `0` on the main axis and lays out tight on that
///     axis — they may still measure non-zero on the cross axis, but
///     their main-axis size collapses.
///   • Integer division uses (`remaining * weight) / total_flex`. The
///     residual (so total flex sizes don't sum exactly to `remaining`) is
///     handed to the *last* flex child to keep totals consistent.
fn flex_layout(inst: &mut WidgetInstance, c: Constraints, axis: Axis, gap: u16) -> Size {
    let n = inst.children.len();
    if n == 0 {
        return c.constrain(Size::default());
    }

    let main_max = main_of(axis, c.max_width, c.max_height);
    let cross_max = cross_of(axis, c.max_width, c.max_height);
    let total_gap = (n as u16).saturating_sub(1).saturating_mul(gap);

    // Inspect children to find flex factors (only `expanded`/`spacer`).
    let flex_factors: Vec<u16> = inst.children.iter().map(child_flex_factor).collect();
    let total_flex: u32 = flex_factors.iter().map(|&f| f as u32).sum();

    let mut sizes: Vec<Size> = vec![Size::default(); n];
    let mut non_flex_main: u32 = 0;

    // Pass 1: lay out non-flex children with their natural main-axis size.
    for i in 0..n {
        if flex_factors[i] > 0 {
            continue;
        }
        let child_constraints = match axis {
            Axis::Vertical => Constraints {
                min_width: 0,
                max_width: c.max_width,
                min_height: 0,
                max_height: main_max
                    .saturating_sub(non_flex_main as u16)
                    .saturating_sub(total_gap),
            },
            Axis::Horizontal => Constraints {
                min_width: 0,
                max_width: main_max
                    .saturating_sub(non_flex_main as u16)
                    .saturating_sub(total_gap),
                min_height: 0,
                max_height: c.max_height,
            },
        };
        let s = layout(&mut inst.children[i], child_constraints);
        sizes[i] = s;
        non_flex_main = non_flex_main.saturating_add(main_of(axis, s.width, s.height) as u32);
    }

    // Pass 2: distribute remaining main-axis space across flex children.
    let remaining_main = (main_max as u32)
        .saturating_sub(non_flex_main)
        .saturating_sub(total_gap as u32);
    let last_flex_idx = flex_factors
        .iter()
        .enumerate()
        .rev()
        .find(|(_, &f)| f > 0)
        .map(|(i, _)| i);
    let mut handed_out_main: u32 = 0;
    for (i, &f) in flex_factors.iter().enumerate() {
        if f == 0 {
            continue;
        }
        let allotment = if total_flex == 0 || remaining_main == 0 {
            0
        } else if Some(i) == last_flex_idx {
            // Hand leftover to the last flex child so totals match exactly.
            remaining_main.saturating_sub(handed_out_main) as u16
        } else {
            ((remaining_main * f as u32) / total_flex) as u16
        };
        handed_out_main = handed_out_main.saturating_add(allotment as u32);

        let child_constraints = match axis {
            Axis::Vertical => Constraints {
                min_width: 0,
                max_width: c.max_width,
                min_height: allotment,
                max_height: allotment,
            },
            Axis::Horizontal => Constraints {
                min_width: allotment,
                max_width: allotment,
                min_height: 0,
                max_height: c.max_height,
            },
        };
        let s = layout(&mut inst.children[i], child_constraints);
        sizes[i] = s;
    }

    // Persist per-child main-axis size for the paint pass.
    inst.layout.flex_main_sizes = sizes
        .iter()
        .map(|s| main_of(axis, s.width, s.height))
        .collect();

    let main_used: u32 = sizes
        .iter()
        .map(|s| main_of(axis, s.width, s.height) as u32)
        .sum::<u32>()
        + total_gap as u32;
    let cross_used: u16 = sizes
        .iter()
        .map(|s| cross_of(axis, s.width, s.height))
        .max()
        .unwrap_or(0)
        .min(cross_max);

    let raw = match axis {
        Axis::Vertical => Size {
            width: cross_used,
            height: main_used.min(u16::MAX as u32) as u16,
        },
        Axis::Horizontal => Size {
            width: main_used.min(u16::MAX as u32) as u16,
            height: cross_used,
        },
    };
    c.constrain(raw)
}

fn paint_flex(inst: &mut WidgetInstance, rect: Rect, out: &mut FrameBuffer, axis: Axis, gap: u16) {
    let n = inst.children.len();
    if n == 0 {
        return;
    }
    let main_sizes = inst.layout.flex_main_sizes.clone();
    let mut cursor: u16 = 0;
    let main_total = main_of(axis, rect.width, rect.height);
    for (i, child) in inst.children.iter_mut().enumerate() {
        if cursor >= main_total {
            break;
        }
        let main_size = main_sizes.get(i).copied().unwrap_or(0);
        if main_size == 0 {
            continue;
        }
        let child_main = main_size.min(main_total.saturating_sub(cursor));
        let cross_size = match axis {
            Axis::Vertical => child.layout.size.width.min(rect.width),
            Axis::Horizontal => child.layout.size.height.min(rect.height),
        };
        let child_rect = match axis {
            Axis::Vertical => Rect {
                row: rect.row.saturating_add(cursor),
                col: rect.col,
                width: rect.width,
                height: child_main,
            },
            Axis::Horizontal => Rect {
                row: rect.row,
                col: rect.col.saturating_add(cursor),
                width: child_main,
                height: rect.height,
            },
        };
        let _ = cross_size; // currently only used to inform measure
        paint(child, child_rect, out);
        cursor = cursor.saturating_add(child_main);
        if i + 1 < n {
            cursor = cursor.saturating_add(gap);
        }
    }
}

// ── Padding ──────────────────────────────────────────────────────────────

fn layout_padding(inst: &mut WidgetInstance, c: Constraints) -> Size {
    let (top, right, bottom, left) = match &inst.last_desc {
        WidgetDescription::Padding {
            top,
            right,
            bottom,
            left,
            ..
        } => (*top, *right, *bottom, *left),
        _ => unreachable!(),
    };
    let h_pad = left.saturating_add(right);
    let v_pad = top.saturating_add(bottom);
    let inner = Constraints {
        min_width: c.min_width.saturating_sub(h_pad),
        max_width: c.max_width.saturating_sub(h_pad),
        min_height: c.min_height.saturating_sub(v_pad),
        max_height: c.max_height.saturating_sub(v_pad),
    };
    let child_size = match inst.children.first_mut() {
        Some(child) => layout(child, inner),
        None => Size::default(),
    };
    let raw = Size {
        width: child_size.width.saturating_add(h_pad),
        height: child_size.height.saturating_add(v_pad),
    };
    c.constrain(raw)
}

fn paint_padding(inst: &mut WidgetInstance, rect: Rect, out: &mut FrameBuffer) {
    let (top, _right, _bottom, left) = match &inst.last_desc {
        WidgetDescription::Padding {
            top,
            right,
            bottom,
            left,
            ..
        } => (*top, *right, *bottom, *left),
        _ => return,
    };
    let h_pad = left.saturating_add(_right);
    let v_pad = top.saturating_add(_bottom);
    let inner_w = rect.width.saturating_sub(h_pad);
    let inner_h = rect.height.saturating_sub(v_pad);
    if inner_w == 0 || inner_h == 0 {
        return;
    }
    if let Some(child) = inst.children.first_mut() {
        let inner = Rect {
            row: rect.row.saturating_add(top),
            col: rect.col.saturating_add(left),
            width: inner_w,
            height: inner_h,
        };
        paint(child, inner, out);
    }
}

// ── Stack ────────────────────────────────────────────────────────────────

fn layout_stack(inst: &mut WidgetInstance, c: Constraints) -> Size {
    let inner = Constraints {
        min_width: 0,
        max_width: c.max_width,
        min_height: 0,
        max_height: c.max_height,
    };
    let mut max_w: u16 = 0;
    let mut max_h: u16 = 0;
    for child in inst.children.iter_mut() {
        let s = layout(child, inner);
        max_w = max_w.max(s.width);
        max_h = max_h.max(s.height);
    }
    c.constrain(Size {
        width: max_w,
        height: max_h,
    })
}

fn paint_stack(inst: &mut WidgetInstance, rect: Rect, out: &mut FrameBuffer) {
    for child in inst.children.iter_mut() {
        // All children share the stack's full rect; later children paint
        // on top of earlier ones (no compositing — last writer wins).
        let s = child.layout.size;
        let child_rect = Rect {
            row: rect.row,
            col: rect.col,
            width: s.width.min(rect.width),
            height: s.height.min(rect.height),
        };
        paint(child, child_rect, out);
    }
}

// ── Expanded / Spacer ────────────────────────────────────────────────────

fn layout_expanded(inst: &mut WidgetInstance, c: Constraints) -> Size {
    // Pass parent's constraints straight through. The flex parent
    // (row/column) tightens the main axis via `min == max` before calling
    // here; the cross axis stays loose so the child picks its natural
    // size. Outside a flex parent, behaves like the bare child.
    let child_size = match inst.children.first_mut() {
        Some(child) => layout(child, c),
        None => Size::default(),
    };
    c.constrain(child_size)
}

fn layout_spacer(_inst: &mut WidgetInstance, c: Constraints) -> Size {
    // Same logic as expanded but no child: fill the main axis (tight via
    // parent), collapse on the loose axis.
    c.constrain(Size {
        width: c.min_width,
        height: c.min_height,
    })
}

fn paint_passthrough(inst: &mut WidgetInstance, rect: Rect, out: &mut FrameBuffer) {
    if let Some(child) = inst.children.first_mut() {
        let s = child.layout.size;
        let child_rect = Rect {
            row: rect.row,
            col: rect.col,
            width: s.width.min(rect.width),
            height: s.height.min(rect.height),
        };
        paint(child, child_rect, out);
    }
}

// ── Constrained ──────────────────────────────────────────────────────────

fn layout_constrained(inst: &mut WidgetInstance, c: Constraints) -> Size {
    let (min_w, max_w, min_h, max_h) = match &inst.last_desc {
        WidgetDescription::Constrained {
            min_width,
            max_width,
            min_height,
            max_height,
            ..
        } => (*min_width, *max_width, *min_height, *max_height),
        _ => unreachable!(),
    };
    // Intersect requested bounds with parent's. `nil` requested fields
    // fall through to the parent's bound.
    let inner = Constraints {
        min_width: min_w.unwrap_or(c.min_width).max(c.min_width),
        max_width: max_w.unwrap_or(c.max_width).min(c.max_width),
        min_height: min_h.unwrap_or(c.min_height).max(c.min_height),
        max_height: max_h.unwrap_or(c.max_height).min(c.max_height),
    };
    // Guard against inverted bounds (user requested min > parent max).
    let inner = Constraints {
        min_width: inner.min_width.min(inner.max_width),
        max_width: inner.max_width.max(inner.min_width),
        min_height: inner.min_height.min(inner.max_height),
        max_height: inner.max_height.max(inner.min_height),
    };
    let child_size = match inst.children.first_mut() {
        Some(child) => layout(child, inner),
        None => Size {
            width: inner.min_width,
            height: inner.min_height,
        },
    };
    c.constrain(child_size)
}

// ── Align ────────────────────────────────────────────────────────────────

fn layout_align(inst: &mut WidgetInstance, c: Constraints) -> Size {
    // Child gets loose constraints (0..max). Align fills its own slot at
    // the parent's max.
    let inner = Constraints {
        min_width: 0,
        max_width: c.max_width,
        min_height: 0,
        max_height: c.max_height,
    };
    if let Some(child) = inst.children.first_mut() {
        let _ = layout(child, inner);
    }
    c.constrain(Size {
        width: c.max_width,
        height: c.max_height,
    })
}

fn paint_align(inst: &mut WidgetInstance, rect: Rect, out: &mut FrameBuffer) {
    let alignment = match &inst.last_desc {
        WidgetDescription::Align { alignment, .. } => *alignment,
        _ => return,
    };
    let Some(child) = inst.children.first_mut() else {
        return;
    };
    let cs = child.layout.size;
    let cw = cs.width.min(rect.width);
    let ch = cs.height.min(rect.height);
    let (row_off, col_off) = align_offset(alignment, rect.width, rect.height, cw, ch);
    let child_rect = Rect {
        row: rect.row.saturating_add(row_off),
        col: rect.col.saturating_add(col_off),
        width: cw,
        height: ch,
    };
    paint(child, child_rect, out);
}

// ── Anchored ─────────────────────────────────────────────────────────────

fn layout_anchored(inst: &mut WidgetInstance, c: Constraints) -> Size {
    let (width, height) = match &inst.last_desc {
        WidgetDescription::Anchored { width, height, .. } => (*width, *height),
        _ => unreachable!(),
    };

    // Resolve fixed/percent dimensions up-front against the parent's max.
    // `Intrinsic` is left unresolved here; we ask the child to lay itself
    // out under loose bounds and read its measured size.
    let resolved_w = resolve_dimension(width, c.max_width);
    let resolved_h = resolve_dimension(height, c.max_height);

    let child_constraints = Constraints {
        min_width: resolved_w.unwrap_or(0),
        max_width: resolved_w.unwrap_or(c.max_width),
        min_height: resolved_h.unwrap_or(0),
        max_height: resolved_h.unwrap_or(c.max_height),
    };

    let child_size = match inst.children.first_mut() {
        Some(child) => layout(child, child_constraints),
        None => Size::default(),
    };

    // Final child size: explicit dim wins over measured size; clamp to parent.
    let final_w = resolved_w.unwrap_or(child_size.width).min(c.max_width);
    let final_h = resolved_h.unwrap_or(child_size.height).min(c.max_height);
    inst.layout.anchored_child_size = Some(Size {
        width: final_w,
        height: final_h,
    });

    // Anchored claims all available space — paint the child within.
    c.constrain(Size {
        width: c.max_width,
        height: c.max_height,
    })
}

fn paint_anchored(inst: &mut WidgetInstance, rect: Rect, out: &mut FrameBuffer) {
    let (anchor, offset_x, offset_y) = match &inst.last_desc {
        WidgetDescription::Anchored {
            anchor,
            offset_x,
            offset_y,
            ..
        } => (*anchor, *offset_x, *offset_y),
        _ => return,
    };
    let anchored_child_size = inst.layout.anchored_child_size;
    let Some(child) = inst.children.first_mut() else {
        return;
    };
    let child_size = anchored_child_size.unwrap_or(child.layout.size);
    let cw = child_size.width.min(rect.width);
    let ch = child_size.height.min(rect.height);
    if cw == 0 || ch == 0 {
        return;
    }
    let (row, col) = anchor_position(anchor, rect.width, rect.height, cw, ch, offset_x, offset_y);
    let max_col = rect.width.saturating_sub(cw);
    let max_row = rect.height.saturating_sub(ch);
    let row = row.min(max_row);
    let col = col.min(max_col);
    let child_rect = Rect {
        row: rect.row.saturating_add(row),
        col: rect.col.saturating_add(col),
        width: cw,
        height: ch,
    };
    paint(child, child_rect, out);
}

/// Resolve a [`Dimension`] against `axis_max`. Returns `None` for
/// `Intrinsic`, which signals the caller to fall back to the child's
/// measured size.
fn resolve_dimension(d: Dimension, axis_max: u16) -> Option<u16> {
    match d {
        Dimension::Intrinsic => None,
        Dimension::Cells(n) => Some(n.min(axis_max)),
        Dimension::Percent(p) => Some(((axis_max as u32 * p as u32) / 100) as u16),
    }
}

/// Compute the child's `(row, col)` inside the parent rect for the given
/// anchor, then apply the cell offset. Negative offsets shift toward the
/// origin and saturate at zero.
fn anchor_position(
    a: Anchor,
    parent_w: u16,
    parent_h: u16,
    cw: u16,
    ch: u16,
    offset_x: i16,
    offset_y: i16,
) -> (u16, u16) {
    let h_extra = parent_w.saturating_sub(cw);
    let v_extra = parent_h.saturating_sub(ch);
    let (base_row, base_col) = match a {
        Anchor::TopLeft => (0u16, 0u16),
        Anchor::Top => (0, h_extra / 2),
        Anchor::TopRight => (0, h_extra),
        Anchor::Left => (v_extra / 2, 0),
        Anchor::Center => (v_extra / 2, h_extra / 2),
        Anchor::Right => (v_extra / 2, h_extra),
        Anchor::BottomLeft => (v_extra, 0),
        Anchor::Bottom => (v_extra, h_extra / 2),
        Anchor::BottomRight => (v_extra, h_extra),
    };
    let row = apply_offset(base_row, offset_y);
    let col = apply_offset(base_col, offset_x);
    (row, col)
}

fn apply_offset(base: u16, delta: i16) -> u16 {
    if delta >= 0 {
        base.saturating_add(delta as u16)
    } else {
        base.saturating_sub(delta.unsigned_abs())
    }
}

/// Compute (row_offset, col_offset) of a `(child_w × child_h)` box inside
/// a `(parent_w × parent_h)` rect, per [`Alignment`]. Saturates to zero
/// when the child is bigger than the parent (paint then clips).
fn align_offset(a: Alignment, parent_w: u16, parent_h: u16, cw: u16, ch: u16) -> (u16, u16) {
    let h_extra = parent_w.saturating_sub(cw);
    let v_extra = parent_h.saturating_sub(ch);
    let (row_frac, col_frac) = match a {
        Alignment::TopLeft => (0, 0),
        Alignment::Top => (0, h_extra / 2),
        Alignment::TopRight => (0, h_extra),
        Alignment::Left => (v_extra / 2, 0),
        Alignment::Center => (v_extra / 2, h_extra / 2),
        Alignment::Right => (v_extra / 2, h_extra),
        Alignment::BottomLeft => (v_extra, 0),
        Alignment::Bottom => (v_extra, h_extra / 2),
        Alignment::BottomRight => (v_extra, h_extra),
    };
    (row_frac, col_frac)
}

// ── TextInput ────────────────────────────────────────────────────────────

fn layout_text_input(inst: &mut WidgetInstance, c: Constraints) -> Size {
    let (value, min_lines, max_lines) = match &inst.last_desc {
        WidgetDescription::TextInput {
            value,
            min_lines,
            max_lines,
            ..
        } => (value.clone(), *min_lines, *max_lines),
        _ => unreachable!("kind/desc mismatch"),
    };
    // Width: prefer parent's max so wrapping/scroll can use the full
    // budget. Sync state's `last_value` here so the paint pass and the
    // input router both see the latest.
    if let InstanceState::TextInput(st) = &mut inst.state {
        let focused = matches!(
            &inst.last_desc,
            WidgetDescription::TextInput { focused, .. } if *focused
        );
        st.sync_with_desc(&value, focused);
    }

    let visible_lines = visible_line_count(&value, min_lines, max_lines);
    let raw = Size {
        width: c.max_width,
        height: visible_lines.min(c.max_height),
    };
    c.constrain(raw)
}

/// Number of rows the input wants to occupy. Bounded by `[min_lines,
/// max_lines]`. Currently counts only hard newlines; soft-wrap lands
/// in phase 5a alongside `scrollable` proper.
fn visible_line_count(value: &str, min_lines: u16, max_lines: u16) -> u16 {
    let actual = value.split('\n').count() as u32;
    actual
        .clamp(min_lines as u32, max_lines as u32)
        .min(u16::MAX as u32) as u16
}

fn paint_text_input(inst: &mut WidgetInstance, rect: Rect, out: &mut FrameBuffer) {
    let (value, focused, placeholder, style) = match &inst.last_desc {
        WidgetDescription::TextInput {
            value,
            focused,
            placeholder,
            style,
            ..
        } => (value.as_str(), *focused, placeholder.clone(), *style),
        _ => return,
    };
    let st = match &inst.state {
        InstanceState::TextInput(s) => s,
        _ => return,
    };
    let style = style.unwrap_or_default();

    let lines: Vec<&str> = if value.is_empty() && placeholder.is_some() {
        // We render the placeholder run instead; cursor still draws
        // at column 0 so a focused empty input shows where typing
        // will land.
        vec![placeholder.as_deref().unwrap_or_default()]
    } else if value.is_empty() {
        vec![""]
    } else {
        value.split('\n').collect()
    };

    let scroll_y = st.scroll_y as usize;
    let scroll_x = st.scroll_x as usize;
    let body_style = body_style_for(&style);
    let placeholder_style = placeholder_style_for(&style);
    let is_placeholder_run = value.is_empty() && placeholder.is_some();

    for r in 0..rect.height as usize {
        let row = rect.row.saturating_add(r as u16);
        let line_idx = scroll_y + r;
        let line = lines.get(line_idx).copied().unwrap_or("");
        let visible: String = take_columns_from(line, scroll_x, rect.width as usize);
        let safe = enforce_width_contract(&visible, rect.width);
        let run_style = if is_placeholder_run {
            placeholder_style
        } else {
            body_style
        };
        write_run(out, row, rect.col, &safe, &run_style);
    }

    // Cursor: only paint a visible cursor when focused. The painter draws
    // a reverse-video cell at the cursor position. This is the engine's
    // default; users can theme via `style.cursor`. Phase 4 ignores
    // cursor_blink (no internal clock).
    if focused {
        paint_cursor(rect, out, st, value, &style);
    }
}

fn paint_cursor(
    rect: Rect,
    out: &mut FrameBuffer,
    st: &crate::text_input::TextInputState,
    value: &str,
    style: &TextInputStyle,
) {
    // Find the (line_idx, col_within_line) of the cursor.
    let (line_idx, col) = cursor_visual_position(value, st.cursor);
    let scroll_y = st.scroll_y as usize;
    let scroll_x = st.scroll_x as usize;
    if line_idx < scroll_y {
        return;
    }
    let row_within = line_idx - scroll_y;
    if row_within >= rect.height as usize {
        return;
    }
    if col < scroll_x {
        return;
    }
    let col_within = col - scroll_x;
    if col_within >= rect.width as usize {
        return;
    }
    let row = rect.row.saturating_add(row_within as u16);
    let col_abs = rect.col.saturating_add(col_within as u16);

    // The cursor is rendered as reverse video over whichever cell lies
    // there. If the user supplied `style.cursor`, treat that as the cell
    // bg.
    let row_idx = row as usize;
    if row_idx >= out.lines.len() {
        return;
    }
    let line = &mut out.lines[row_idx];
    let col_idx = col_abs as usize;
    if col_idx >= line.cells.len() {
        return;
    }
    let mut cell = line.cells[col_idx].clone();
    cell.style.reverse = true;
    if let Some(c) = style.cursor {
        cell.style.bg = Some(c);
    }
    line.cells[col_idx] = cell;
}

fn body_style_for(style: &TextInputStyle) -> Style {
    Style {
        fg: style.fg,
        bg: style.bg,
        bold: false,
        italic: false,
        underline: false,
        reverse: false,
    }
}

fn placeholder_style_for(style: &TextInputStyle) -> Style {
    Style {
        fg: style.placeholder,
        bg: style.bg,
        bold: false,
        italic: false,
        underline: false,
        reverse: false,
    }
}

/// Cursor visual position: `(line_index, column_within_line)` where
/// `line_index` counts from 0 and column is the cell-width prefix sum
/// from the line start to the cursor offset.
fn cursor_visual_position(value: &str, cursor: usize) -> (usize, usize) {
    let cursor = cursor.min(value.len());
    let before = &value[..cursor];
    let line_idx = before.bytes().filter(|b| *b == b'\n').count();
    let last_nl = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line_prefix = &before[last_nl..];
    let col = string_width(line_prefix);
    (line_idx, col)
}

/// Take up to `width` columns from `s` starting at column `start`.
fn take_columns_from(s: &str, start: usize, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut col = 0usize;
    let mut out = String::new();
    for ch in s.chars() {
        let w = char_width(ch);
        if col + w <= start {
            col += w;
            continue;
        }
        if col >= start + width {
            break;
        }
        // If the char straddles the start, emit a space to keep
        // alignment.
        if col < start {
            for _ in 0..(start - col) {
                out.push(' ');
            }
            col = start;
        }
        if col + w > start + width {
            break;
        }
        out.push(ch);
        col += w;
    }
    out
}

// ── Helpers ──────────────────────────────────────────────────────────────

fn child_flex_factor(child: &WidgetInstance) -> u16 {
    match &child.last_desc {
        WidgetDescription::Expanded { flex, .. } | WidgetDescription::Spacer { flex, .. } => *flex,
        _ => 0,
    }
}

fn main_of(axis: Axis, width: u16, height: u16) -> u16 {
    match axis {
        Axis::Horizontal => width,
        Axis::Vertical => height,
    }
}

fn cross_of(axis: Axis, width: u16, height: u16) -> u16 {
    match axis {
        Axis::Horizontal => height,
        Axis::Vertical => width,
    }
}

/// Word-wrap `content` to fit in `width` columns. Newlines in source are
/// hard breaks; returned vec elements never include a trailing newline.
/// One char paired with its style. The styled-text path (spans, and in
/// later 5b commits the markdown walker + animation frames) operates on
/// `Vec<StyledChar>` so wrapping decisions are independent of span
/// boundaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyledChar {
    pub ch: char,
    pub style: Style,
}

/// Concatenate every span's chars into a single styled-char run.
/// Embedded `\n` becomes an explicit line break in `wrap_styled`.
pub fn styled_chars_from_spans(spans: &[Span]) -> Vec<StyledChar> {
    let mut out: Vec<StyledChar> = Vec::new();
    for span in spans {
        for ch in span.text.chars() {
            out.push(StyledChar {
                ch,
                style: span.style,
            });
        }
    }
    out
}

/// Wrap a styled-char run into rows of `width` cells. Honors `\n` as a
/// hard line break; otherwise uses the same word/char/none algorithm as
/// the unstyled path.
pub fn wrap_styled(chars: &[StyledChar], width: u16, wrap: WrapMode) -> Vec<Vec<StyledChar>> {
    if width == 0 {
        return Vec::new();
    }
    let limit = width as usize;
    // Split on `\n` first so wrapping operates on logical lines.
    let mut logical: Vec<Vec<StyledChar>> = Vec::new();
    let mut line: Vec<StyledChar> = Vec::new();
    for c in chars {
        if c.ch == '\n' {
            logical.push(std::mem::take(&mut line));
            continue;
        }
        line.push(c.clone());
    }
    if !line.is_empty() || !chars.is_empty() {
        logical.push(line);
    }

    let mut wrapped: Vec<Vec<StyledChar>> = Vec::new();
    for raw in logical {
        match wrap {
            WrapMode::None => wrapped.push(take_styled_columns(&raw, limit)),
            WrapMode::Char => wrapped.extend(wrap_styled_char(&raw, limit)),
            WrapMode::Word => wrapped.extend(wrap_styled_word(&raw, limit)),
        }
    }
    wrapped
}

fn take_styled_columns(line: &[StyledChar], limit: usize) -> Vec<StyledChar> {
    let mut taken = 0usize;
    let mut out = Vec::new();
    for c in line {
        let w = char_width(c.ch);
        if taken + w > limit {
            break;
        }
        out.push(c.clone());
        taken += w;
    }
    out
}

fn wrap_styled_char(line: &[StyledChar], limit: usize) -> Vec<Vec<StyledChar>> {
    let mut out: Vec<Vec<StyledChar>> = Vec::new();
    let mut current: Vec<StyledChar> = Vec::new();
    let mut col = 0usize;
    for c in line {
        let w = char_width(c.ch);
        if col + w > limit && !current.is_empty() {
            out.push(std::mem::take(&mut current));
            col = 0;
        }
        if w > limit {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            current.push(c.clone());
            out.push(std::mem::take(&mut current));
            col = 0;
            continue;
        }
        current.push(c.clone());
        col += w;
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn wrap_styled_word(line: &[StyledChar], limit: usize) -> Vec<Vec<StyledChar>> {
    let mut out: Vec<Vec<StyledChar>> = Vec::new();
    let mut current: Vec<StyledChar> = Vec::new();
    let mut col = 0usize;
    for word in split_styled_words(line) {
        let ww: usize = word.iter().map(|c| char_width(c.ch)).sum();
        if col == 0 && ww > limit {
            for sub in wrap_styled_char(word, limit) {
                out.push(sub);
            }
            current.clear();
            col = 0;
            continue;
        }
        if col + ww > limit {
            out.push(std::mem::take(&mut current));
            col = 0;
            if word.iter().all(|c| c.ch.is_whitespace()) {
                continue;
            }
        }
        current.extend_from_slice(word);
        col += ww;
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn split_styled_words(line: &[StyledChar]) -> Vec<&[StyledChar]> {
    let mut out: Vec<&[StyledChar]> = Vec::new();
    if line.is_empty() {
        return out;
    }
    let mut start = 0usize;
    let mut in_space = line[0].ch.is_whitespace();
    for (i, c) in line.iter().enumerate() {
        let cw = c.ch.is_whitespace();
        if cw != in_space {
            out.push(&line[start..i]);
            start = i;
            in_space = cw;
        }
    }
    if start < line.len() {
        out.push(&line[start..]);
    }
    out
}

pub(crate) fn measure_styled_rows(rows: &[Vec<StyledChar>], c: Constraints) -> Size {
    let height = rows.len() as u16;
    let width = rows
        .iter()
        .map(|r| r.iter().map(|sc| char_width(sc.ch) as u16).sum::<u16>())
        .max()
        .unwrap_or(0);
    c.constrain(Size { width, height })
}

pub(crate) fn paint_styled_rows(rows: &[Vec<StyledChar>], rect: Rect, out: &mut FrameBuffer) {
    for (i, row) in rows.iter().enumerate() {
        if i as u16 >= rect.height {
            break;
        }
        let r = rect.row.saturating_add(i as u16);
        write_styled_row(out, r, rect.col, rect.width, row);
    }
}

fn write_styled_row(
    buf: &mut FrameBuffer,
    row: u16,
    col_start: u16,
    max_width: u16,
    line: &[StyledChar],
) {
    let row_idx = row as usize;
    if row_idx >= buf.lines.len() {
        return;
    }
    let line_buf = &mut buf.lines[row_idx];
    let limit = max_width as usize;
    let mut col = col_start as usize;
    let bound = (col_start as usize).saturating_add(limit);
    for sc in line {
        let w = char_width(sc.ch);
        if w == 0 {
            continue;
        }
        if col >= line_buf.cells.len() || col >= bound {
            break;
        }
        if col + w > bound {
            break;
        }
        let mut s = String::new();
        s.push(sc.ch);
        line_buf.cells[col] = Cell {
            text: s,
            style: sc.style,
        };
        col += w;
        for _ in 1..w {
            if col >= line_buf.cells.len() || col >= bound {
                break;
            }
            line_buf.cells[col] = Cell::blank();
            col += 1;
        }
    }
}

pub fn wrap_text(content: &str, width: u16, wrap: WrapMode) -> Vec<String> {
    if width == 0 {
        return vec![];
    }
    let mut out: Vec<String> = Vec::new();
    for raw_line in content.split('\n') {
        match wrap {
            WrapMode::None => {
                let truncated: String = take_columns(raw_line, width);
                out.push(truncated);
            }
            WrapMode::Char => out.extend(wrap_char(raw_line, width)),
            WrapMode::Word => out.extend(wrap_word(raw_line, width)),
        }
    }
    out
}

fn take_columns(s: &str, width: u16) -> String {
    let limit = width as usize;
    let mut taken = 0usize;
    let mut out = String::new();
    for ch in s.chars() {
        let w = char_width(ch);
        if taken + w > limit {
            break;
        }
        out.push(ch);
        taken += w;
    }
    out
}

fn wrap_char(line: &str, width: u16) -> Vec<String> {
    let limit = width as usize;
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut col = 0usize;
    for ch in line.chars() {
        let w = char_width(ch);
        if col + w > limit && !current.is_empty() {
            out.push(std::mem::take(&mut current));
            col = 0;
        }
        if w > limit {
            // Single grapheme wider than the line; emit on its own row.
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            current.push(ch);
            out.push(std::mem::take(&mut current));
            col = 0;
            continue;
        }
        current.push(ch);
        col += w;
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn wrap_word(line: &str, width: u16) -> Vec<String> {
    let limit = width as usize;
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut col = 0usize;
    for word in split_keeping_spaces(line) {
        let ww = string_width(word);
        if col == 0 && ww > limit {
            // Word longer than the line — char-wrap it on its own row(s).
            for sub in wrap_char(word, width) {
                out.push(sub);
            }
            current.clear();
            col = 0;
            continue;
        }
        if col + ww > limit {
            out.push(std::mem::take(&mut current));
            col = 0;
            // Skip pure-whitespace words at line starts to avoid leaving
            // a trailing-space artefact at the start of the new line.
            if word.chars().all(char::is_whitespace) {
                continue;
            }
        }
        current.push_str(word);
        col += ww;
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Split `s` into runs that alternate whitespace and non-whitespace.
fn split_keeping_spaces(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut in_space = s.starts_with(char::is_whitespace);
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = s[i..].chars().next().unwrap_or(' ');
        let cw = c.is_whitespace();
        if cw != in_space {
            out.push(&s[start..i]);
            start = i;
            in_space = cw;
        }
        i += c.len_utf8();
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

fn string_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

fn char_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

fn write_run(buf: &mut FrameBuffer, row: u16, col_start: u16, text: &str, style: &Style) {
    let row_idx = row as usize;
    if row_idx >= buf.lines.len() {
        return;
    }
    let line = &mut buf.lines[row_idx];
    let mut col = col_start as usize;
    for ch in text.chars() {
        let w = char_width(ch);
        if w == 0 {
            continue;
        }
        if col >= line.cells.len() {
            break;
        }
        let mut s = String::new();
        s.push(ch);
        line.cells[col] = Cell {
            text: s,
            style: *style,
        };
        col += w;
        // Wide chars: blank out the trailing cell(s) to avoid duplicating
        // ink. Phase 1/2 are ASCII-only so this branch is dead; landed
        // for forward compatibility.
        for _ in 1..w {
            if col >= line.cells.len() {
                break;
            }
            line.cells[col] = Cell::blank();
            col += 1;
        }
    }
}

/// Convenience: drive the full two-pass over `root` and paint into `out`.
/// The renderer calls this; tests use it too.
pub fn layout_and_paint(root: &mut WidgetInstance, width: u16, height: u16, out: &mut FrameBuffer) {
    let _ = layout(root, Constraints::loose(width, height));
    paint(
        root,
        Rect {
            row: 0,
            col: 0,
            width,
            height,
        },
        out,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desc::{Alignment, Anchor, Dimension, MarkdownTheme, WidgetDescription, WrapMode};
    use crate::reconciler::Reconciler;
    use crate::render::FrameBuffer;

    fn text(content: &str) -> WidgetDescription {
        WidgetDescription::Text {
            content: content.into(),
            style: None,
            wrap: WrapMode::Word,
            key: None,
        }
    }

    fn cell_at(buf: &FrameBuffer, row: usize, col: usize) -> &str {
        buf.lines[row].cells[col].text.as_str()
    }

    fn paint_root(desc: WidgetDescription, w: u16, h: u16) -> FrameBuffer {
        let mut rec = Reconciler::new();
        rec.reconcile(desc);
        let mut buf = FrameBuffer::new(w, h);
        let root = rec.root.as_mut().unwrap();
        layout_and_paint(root, w, h, &mut buf);
        buf
    }

    #[test]
    fn text_layout_wraps_to_max_width() {
        let mut rec = Reconciler::new();
        rec.reconcile(text("hello world"));
        let root = rec.root.as_mut().unwrap();
        let s = layout(root, Constraints::loose(6, 5));
        assert_eq!(s.height, 2);
        assert!(s.width <= 6);
    }

    #[test]
    fn column_padding_text_positions_correctly() {
        let desc = WidgetDescription::Column {
            gap: 0,
            key: None,
            children: vec![WidgetDescription::Padding {
                top: 1,
                right: 0,
                bottom: 0,
                left: 2,
                child: Box::new(text("hi")),
                key: None,
            }],
        };
        let buf = paint_root(desc, 8, 3);
        assert_eq!(cell_at(&buf, 0, 0), " ", "top-left should be padding");
        assert_eq!(cell_at(&buf, 1, 0), " ", "left padding column is blank");
        assert_eq!(cell_at(&buf, 1, 1), " ", "left padding column is blank");
        assert_eq!(cell_at(&buf, 1, 2), "h", "h at offset (1, 2)");
        assert_eq!(cell_at(&buf, 1, 3), "i", "i follows");
        assert_eq!(cell_at(&buf, 1, 4), " ", "remaining row blank");
    }

    #[test]
    fn column_with_gap_inserts_blank_row() {
        let desc = WidgetDescription::Column {
            gap: 1,
            key: None,
            children: vec![text("a"), text("b")],
        };
        let buf = paint_root(desc, 4, 4);
        assert_eq!(cell_at(&buf, 0, 0), "a");
        assert_eq!(cell_at(&buf, 1, 0), " ", "gap row");
        assert_eq!(cell_at(&buf, 2, 0), "b");
    }

    #[test]
    fn padding_layout_returns_inner_plus_padding() {
        let desc = WidgetDescription::Padding {
            top: 1,
            right: 2,
            bottom: 3,
            left: 4,
            child: Box::new(text("x")),
            key: None,
        };
        let mut rec = Reconciler::new();
        rec.reconcile(desc);
        let root = rec.root.as_mut().unwrap();
        let s = layout(root, Constraints::loose(20, 20));
        // text is 1×1 inside; padding adds (4+2, 1+3) = (6, 4) → 7 × 5.
        assert_eq!(
            s,
            Size {
                width: 7,
                height: 5
            }
        );
    }

    #[test]
    fn align_offset_for_each_corner() {
        // 10×4 frame, 2×1 child.
        let cases = [
            (Alignment::TopLeft, (0, 0)),
            (Alignment::Top, (0, 4)),
            (Alignment::TopRight, (0, 8)),
            (Alignment::Left, (1, 0)),
            (Alignment::Center, (1, 4)),
            (Alignment::Right, (1, 8)),
            (Alignment::BottomLeft, (3, 0)),
            (Alignment::Bottom, (3, 4)),
            (Alignment::BottomRight, (3, 8)),
        ];
        for (a, (row, col)) in cases {
            assert_eq!(
                align_offset(a, 10, 4, 2, 1),
                (row, col),
                "alignment {a:?} mispositions"
            );
        }
    }

    #[test]
    fn row_lays_children_horizontally_with_gap() {
        let desc = WidgetDescription::Row {
            gap: 1,
            key: None,
            children: vec![text("ab"), text("cd")],
        };
        let buf = paint_root(desc, 8, 1);
        assert_eq!(cell_at(&buf, 0, 0), "a");
        assert_eq!(cell_at(&buf, 0, 1), "b");
        assert_eq!(cell_at(&buf, 0, 2), " ", "gap column");
        assert_eq!(cell_at(&buf, 0, 3), "c");
        assert_eq!(cell_at(&buf, 0, 4), "d");
    }

    #[test]
    fn row_layout_sums_children_widths_plus_gap() {
        let desc = WidgetDescription::Row {
            gap: 2,
            key: None,
            children: vec![text("xx"), text("yy"), text("zz")],
        };
        let mut rec = Reconciler::new();
        rec.reconcile(desc);
        let root = rec.root.as_mut().unwrap();
        let s = layout(root, Constraints::loose(40, 5));
        // 2 + 2 + 2 + 2 (gap) + 2 (gap) = 10 wide, max child height = 1.
        assert_eq!(s.width, 10);
        assert_eq!(s.height, 1);
    }

    #[test]
    fn stack_overlays_children_with_later_on_top() {
        let desc = WidgetDescription::Stack {
            key: None,
            children: vec![text("aaa"), text("b")],
        };
        let buf = paint_root(desc, 5, 1);
        // First child paints "aaa" at col 0..3; second paints "b" at col 0,
        // overwriting the first cell. The remainder of "aaa" stays.
        assert_eq!(cell_at(&buf, 0, 0), "b", "later child wins position 0");
        assert_eq!(cell_at(&buf, 0, 1), "a");
        assert_eq!(cell_at(&buf, 0, 2), "a");
    }

    #[test]
    fn stack_size_is_max_of_children() {
        let desc = WidgetDescription::Stack {
            key: None,
            children: vec![text("hi"), text("hello")],
        };
        let mut rec = Reconciler::new();
        rec.reconcile(desc);
        let root = rec.root.as_mut().unwrap();
        let s = layout(root, Constraints::loose(40, 5));
        // max(2, 5) wide, max(1, 1) tall
        assert_eq!(
            s,
            Size {
                width: 5,
                height: 1
            }
        );
    }

    fn expanded(child: WidgetDescription, flex: u16) -> WidgetDescription {
        WidgetDescription::Expanded {
            flex,
            child: Box::new(child),
            key: None,
        }
    }

    fn spacer(flex: u16) -> WidgetDescription {
        WidgetDescription::Spacer { flex, key: None }
    }

    #[test]
    fn row_with_one_expanded_takes_remaining_width() {
        // Row of 20 cols; "abc" (3) + expanded(text "x") → expanded gets 17.
        let desc = WidgetDescription::Row {
            gap: 0,
            key: None,
            children: vec![text("abc"), expanded(text(""), 1)],
        };
        let mut rec = Reconciler::new();
        rec.reconcile(desc);
        let root = rec.root.as_mut().unwrap();
        let _ = layout(root, Constraints::loose(20, 1));
        // First child main = 3, expanded main = 17.
        assert_eq!(root.layout.flex_main_sizes, vec![3, 17]);
    }

    #[test]
    fn row_with_two_expanded_split_proportionally() {
        // Both flex=1 → equal split of 10 cols.
        let desc = WidgetDescription::Row {
            gap: 0,
            key: None,
            children: vec![expanded(text(""), 1), expanded(text(""), 1)],
        };
        let mut rec = Reconciler::new();
        rec.reconcile(desc);
        let root = rec.root.as_mut().unwrap();
        let _ = layout(root, Constraints::loose(10, 1));
        // First gets 5; last (residual) gets the rest = 5.
        assert_eq!(root.layout.flex_main_sizes, vec![5, 5]);
    }

    #[test]
    fn row_with_weighted_flex_distributes_unevenly() {
        // flex 1 + flex 3 → 25%/75% of 20 cols = 5 + 15.
        let desc = WidgetDescription::Row {
            gap: 0,
            key: None,
            children: vec![expanded(text(""), 1), expanded(text(""), 3)],
        };
        let mut rec = Reconciler::new();
        rec.reconcile(desc);
        let root = rec.root.as_mut().unwrap();
        let _ = layout(root, Constraints::loose(20, 1));
        assert_eq!(root.layout.flex_main_sizes, vec![5, 15]);
    }

    #[test]
    fn flex_residual_handed_to_last_child() {
        // 10 cols / 3 flex = 3 each, with 1 residual to last.
        let desc = WidgetDescription::Row {
            gap: 0,
            key: None,
            children: vec![
                expanded(text(""), 1),
                expanded(text(""), 1),
                expanded(text(""), 1),
            ],
        };
        let mut rec = Reconciler::new();
        rec.reconcile(desc);
        let root = rec.root.as_mut().unwrap();
        let _ = layout(root, Constraints::loose(10, 1));
        assert_eq!(root.layout.flex_main_sizes, vec![3, 3, 4]);
    }

    #[test]
    fn spacer_pushes_following_text_to_end() {
        // Row: "L" + spacer(1) + "R" inside 5 cols → "L   R".
        let desc = WidgetDescription::Row {
            gap: 0,
            key: None,
            children: vec![text("L"), spacer(1), text("R")],
        };
        let buf = paint_root(desc, 5, 1);
        assert_eq!(cell_at(&buf, 0, 0), "L");
        assert_eq!(cell_at(&buf, 0, 1), " ");
        assert_eq!(cell_at(&buf, 0, 2), " ");
        assert_eq!(cell_at(&buf, 0, 3), " ");
        assert_eq!(cell_at(&buf, 0, 4), "R");
    }

    #[test]
    fn column_with_expanded_grows_vertically() {
        // Column of 10 rows: text(1 row) + expanded(text "") → expanded = 9.
        let desc = WidgetDescription::Column {
            gap: 0,
            key: None,
            children: vec![text("hi"), expanded(text(""), 1)],
        };
        let mut rec = Reconciler::new();
        rec.reconcile(desc);
        let root = rec.root.as_mut().unwrap();
        let _ = layout(root, Constraints::loose(20, 10));
        assert_eq!(root.layout.flex_main_sizes, vec![1, 9]);
    }

    #[test]
    fn flex_with_zero_remaining_collapses() {
        // Non-flex children eat all available main; expanded gets 0.
        let desc = WidgetDescription::Row {
            gap: 0,
            key: None,
            children: vec![text("xxxxx"), expanded(text(""), 1)],
        };
        let mut rec = Reconciler::new();
        rec.reconcile(desc);
        let root = rec.root.as_mut().unwrap();
        let _ = layout(root, Constraints::loose(5, 1));
        assert_eq!(root.layout.flex_main_sizes, vec![5, 0]);
    }

    fn constrained(
        child: WidgetDescription,
        min_w: Option<u16>,
        max_w: Option<u16>,
        min_h: Option<u16>,
        max_h: Option<u16>,
    ) -> WidgetDescription {
        WidgetDescription::Constrained {
            min_width: min_w,
            max_width: max_w,
            min_height: min_h,
            max_height: max_h,
            child: Box::new(child),
            key: None,
        }
    }

    fn align(child: WidgetDescription, alignment: Alignment) -> WidgetDescription {
        WidgetDescription::Align {
            alignment,
            child: Box::new(child),
            key: None,
        }
    }

    #[test]
    fn constrained_tightens_max_width() {
        // text "hello world" inside constrained max_width=4 → wraps at 4.
        let desc = constrained(text("hello world"), None, Some(4), None, None);
        let mut rec = Reconciler::new();
        rec.reconcile(desc);
        let root = rec.root.as_mut().unwrap();
        let s = layout(root, Constraints::loose(40, 5));
        assert!(s.width <= 4, "width {s:?} must be ≤ 4");
        assert!(s.height >= 3, "wrapped to multiple lines");
    }

    #[test]
    fn constrained_min_pads_to_minimum() {
        // text "x" with min_width=10 → child renders at 10 wide.
        let desc = constrained(text("x"), Some(10), None, None, None);
        let mut rec = Reconciler::new();
        rec.reconcile(desc);
        let root = rec.root.as_mut().unwrap();
        let s = layout(root, Constraints::loose(40, 5));
        assert_eq!(s.width, 10);
    }

    #[test]
    fn constrained_intersects_with_parent_max() {
        // requested max_width=20 but parent only allows 5 → 5 wins.
        let desc = constrained(text("hello world"), None, Some(20), None, None);
        let mut rec = Reconciler::new();
        rec.reconcile(desc);
        let root = rec.root.as_mut().unwrap();
        let s = layout(root, Constraints::loose(5, 5));
        assert!(s.width <= 5);
    }

    #[test]
    fn align_center_positions_text_in_middle() {
        // 11×3 buffer, align center, text "hi" (2 cols × 1 row).
        // h_extra = 9, /2 = 4 → "hi" lands at col 4.
        // v_extra = 2, /2 = 1 → at row 1.
        let desc = align(text("hi"), Alignment::Center);
        let buf = paint_root(desc, 11, 3);
        assert_eq!(cell_at(&buf, 1, 4), "h");
        assert_eq!(cell_at(&buf, 1, 5), "i");
        assert_eq!(cell_at(&buf, 0, 0), " ", "top-left blank");
        assert_eq!(cell_at(&buf, 2, 10), " ", "bottom-right blank");
    }

    #[test]
    fn align_top_left_paints_at_origin() {
        let desc = align(text("AB"), Alignment::TopLeft);
        let buf = paint_root(desc, 10, 3);
        assert_eq!(cell_at(&buf, 0, 0), "A");
        assert_eq!(cell_at(&buf, 0, 1), "B");
    }

    #[test]
    fn align_bottom_right_paints_at_far_corner() {
        let desc = align(text("XY"), Alignment::BottomRight);
        let buf = paint_root(desc, 10, 3);
        // h_extra = 8, v_extra = 2 → starts at (2, 8)
        assert_eq!(cell_at(&buf, 2, 8), "X");
        assert_eq!(cell_at(&buf, 2, 9), "Y");
    }

    #[test]
    fn wrap_word_splits_on_word_boundary() {
        let rows = wrap_text("hello world", 6, WrapMode::Word);
        assert_eq!(rows, vec!["hello ".to_string(), "world".to_string()]);
    }

    #[test]
    fn wrap_word_keeps_intact_when_fits() {
        let rows = wrap_text("hello", 10, WrapMode::Word);
        assert_eq!(rows, vec!["hello".to_string()]);
    }

    #[test]
    fn wrap_char_breaks_anywhere() {
        let rows = wrap_text("abcdefg", 3, WrapMode::Char);
        assert_eq!(rows, vec!["abc", "def", "g"]);
    }

    #[test]
    fn wrap_none_truncates() {
        let rows = wrap_text("abcdefghij", 4, WrapMode::None);
        assert_eq!(rows, vec!["abcd"]);
    }

    #[test]
    fn explicit_newline_breaks_line() {
        let rows = wrap_text("a\nb", 10, WrapMode::Word);
        assert_eq!(rows, vec!["a", "b"]);
    }

    fn anchored(
        child: WidgetDescription,
        anchor: Anchor,
        width: Dimension,
        height: Dimension,
        offset_x: i16,
        offset_y: i16,
    ) -> WidgetDescription {
        WidgetDescription::Anchored {
            anchor,
            offset_x,
            offset_y,
            width,
            height,
            child: Box::new(child),
            key: None,
        }
    }

    #[test]
    fn anchor_position_for_each_anchor_with_small_child() {
        // Parent 12×6, child 2×2.
        let cases = [
            (Anchor::TopLeft, (0, 0)),
            (Anchor::Top, (0, 5)),
            (Anchor::TopRight, (0, 10)),
            (Anchor::Left, (2, 0)),
            (Anchor::Center, (2, 5)),
            (Anchor::Right, (2, 10)),
            (Anchor::BottomLeft, (4, 0)),
            (Anchor::Bottom, (4, 5)),
            (Anchor::BottomRight, (4, 10)),
        ];
        for (a, (row, col)) in cases {
            assert_eq!(
                anchor_position(a, 12, 6, 2, 2, 0, 0),
                (row, col),
                "anchor {a:?} mispositions"
            );
        }
    }

    #[test]
    fn anchored_center_paints_at_middle() {
        // 12×4 frame, child "AB" (2×1) → center at row 1, col (12-2)/2 = 5.
        let desc = anchored(
            text("AB"),
            Anchor::Center,
            Dimension::Intrinsic,
            Dimension::Intrinsic,
            0,
            0,
        );
        let buf = paint_root(desc, 12, 4);
        assert_eq!(cell_at(&buf, 1, 5), "A");
        assert_eq!(cell_at(&buf, 1, 6), "B");
        // Surrounding cells are blank — anchored doesn't paint outside the
        // child rect, leaving the parent framebuffer untouched.
        assert_eq!(cell_at(&buf, 0, 0), " ");
        assert_eq!(cell_at(&buf, 3, 11), " ");
    }

    #[test]
    fn anchored_top_left_paints_at_origin() {
        let desc = anchored(
            text("XY"),
            Anchor::TopLeft,
            Dimension::Intrinsic,
            Dimension::Intrinsic,
            0,
            0,
        );
        let buf = paint_root(desc, 8, 3);
        assert_eq!(cell_at(&buf, 0, 0), "X");
        assert_eq!(cell_at(&buf, 0, 1), "Y");
    }

    #[test]
    fn anchored_bottom_right_paints_at_far_corner() {
        let desc = anchored(
            text("MN"),
            Anchor::BottomRight,
            Dimension::Intrinsic,
            Dimension::Intrinsic,
            0,
            0,
        );
        let buf = paint_root(desc, 10, 4);
        // h_extra = 10-2 = 8, v_extra = 4-1 = 3 → starts at (3, 8)
        assert_eq!(cell_at(&buf, 3, 8), "M");
        assert_eq!(cell_at(&buf, 3, 9), "N");
    }

    #[test]
    fn anchored_percent_width_resolves_against_parent() {
        // 20-col parent, width = "50%" → child measured at 10 cols.
        let desc = anchored(
            text("hello world"),
            Anchor::TopLeft,
            Dimension::Percent(50),
            Dimension::Intrinsic,
            0,
            0,
        );
        let mut rec = Reconciler::new();
        rec.reconcile(desc);
        let root = rec.root.as_mut().unwrap();
        let _ = layout(root, Constraints::loose(20, 5));
        let cs = root.layout.anchored_child_size.expect("anchored size");
        assert_eq!(cs.width, 10);
    }

    #[test]
    fn anchored_offset_shifts_position() {
        // Center of a 10×4 with child 2×1 = (1, 4); +offset (1, 1) = (2, 5).
        let desc = anchored(
            text("AB"),
            Anchor::Center,
            Dimension::Intrinsic,
            Dimension::Intrinsic,
            1,
            1,
        );
        let buf = paint_root(desc, 10, 4);
        assert_eq!(cell_at(&buf, 2, 5), "A");
        assert_eq!(cell_at(&buf, 2, 6), "B");
    }

    #[test]
    fn anchored_negative_offset_shifts_toward_origin() {
        // Center → (1, 4); -1, -1 → (0, 3).
        let desc = anchored(
            text("AB"),
            Anchor::Center,
            Dimension::Intrinsic,
            Dimension::Intrinsic,
            -1,
            -1,
        );
        let buf = paint_root(desc, 10, 4);
        assert_eq!(cell_at(&buf, 0, 3), "A");
        assert_eq!(cell_at(&buf, 0, 4), "B");
    }

    #[test]
    fn anchored_negative_offset_saturates_at_zero() {
        // Top-left + negative offset clamps at origin (no underflow).
        let desc = anchored(
            text("AB"),
            Anchor::TopLeft,
            Dimension::Intrinsic,
            Dimension::Intrinsic,
            -10,
            -10,
        );
        let buf = paint_root(desc, 8, 3);
        assert_eq!(cell_at(&buf, 0, 0), "A");
        assert_eq!(cell_at(&buf, 0, 1), "B");
    }

    #[test]
    fn anchored_fixed_width_clamps_to_parent() {
        // Requested width 100 in a 10-col parent → clamped to 10.
        let desc = anchored(
            text("hi"),
            Anchor::TopLeft,
            Dimension::Cells(100),
            Dimension::Intrinsic,
            0,
            0,
        );
        let mut rec = Reconciler::new();
        rec.reconcile(desc);
        let root = rec.root.as_mut().unwrap();
        let _ = layout(root, Constraints::loose(10, 3));
        let cs = root.layout.anchored_child_size.expect("anchored size");
        assert_eq!(cs.width, 10);
    }

    #[test]
    fn anchored_size_fills_parent() {
        // Anchored claims the full parent rect regardless of child size.
        let desc = anchored(
            text("a"),
            Anchor::Center,
            Dimension::Intrinsic,
            Dimension::Intrinsic,
            0,
            0,
        );
        let mut rec = Reconciler::new();
        rec.reconcile(desc);
        let root = rec.root.as_mut().unwrap();
        let s = layout(root, Constraints::loose(20, 5));
        assert_eq!(
            s,
            Size {
                width: 20,
                height: 5
            }
        );
    }

    #[test]
    fn anchored_child_larger_than_parent_clips_at_anchor_side() {
        // Child 6×3 inside a 4×2 parent, anchored bottom-right.
        // Resolved child size clamps to parent (4×2); anchor offsets clamp to 0.
        let desc = anchored(
            text("ABCDEF"),
            Anchor::BottomRight,
            Dimension::Intrinsic,
            Dimension::Intrinsic,
            0,
            0,
        );
        let mut rec = Reconciler::new();
        rec.reconcile(desc);
        let root = rec.root.as_mut().unwrap();
        let _ = layout(root, Constraints::loose(4, 2));
        let cs = root.layout.anchored_child_size.expect("anchored size");
        // child measured 6×1 inside loose bounds (text wraps to width 4 →
        // multiple rows). Either way, clamped to parent width.
        assert!(cs.width <= 4);
        assert!(cs.height <= 2);
    }

    #[test]
    fn anchored_inside_stack_overlays_centered() {
        // stack { text("background"), anchored center { text("X") } }
        // 11×3 → "X" lands at (1, 5).
        let desc = WidgetDescription::Stack {
            key: None,
            children: vec![
                text("aaaaaaaaaaa"),
                anchored(
                    text("X"),
                    Anchor::Center,
                    Dimension::Intrinsic,
                    Dimension::Intrinsic,
                    0,
                    0,
                ),
            ],
        };
        let buf = paint_root(desc, 11, 3);
        assert_eq!(cell_at(&buf, 1, 5), "X", "popup centered over background");
        assert_eq!(cell_at(&buf, 0, 0), "a", "background preserved at top-left");
        assert_eq!(
            cell_at(&buf, 0, 10),
            "a",
            "background preserved at top-right"
        );
    }

    // ── Spans ────────────────────────────────────────────────────────

    fn span(text: &str, style: Style) -> Span {
        Span {
            text: text.to_string(),
            style,
        }
    }

    fn spans_desc(spans: Vec<Span>, wrap: WrapMode) -> WidgetDescription {
        WidgetDescription::Spans {
            spans,
            wrap,
            key: None,
        }
    }

    #[test]
    fn spans_layout_concatenates_widths() {
        let s = vec![
            span("hello ", Style::default()),
            span(
                "world",
                Style {
                    bold: true,
                    ..Style::default()
                },
            ),
        ];
        let desc = spans_desc(s, WrapMode::Word);
        let buf = paint_root(desc, 20, 1);
        assert_eq!(cell_at(&buf, 0, 0), "h");
        assert_eq!(cell_at(&buf, 0, 6), "w");
        assert_eq!(cell_at(&buf, 0, 10), "d");
    }

    #[test]
    fn spans_per_segment_styles_land_on_cells() {
        use crate::desc::Color;
        let red = Style {
            fg: Some(Color::Rgb(255, 0, 0)),
            ..Style::default()
        };
        let bold = Style {
            bold: true,
            ..Style::default()
        };
        let s = vec![span("ab", red), span("cd", bold)];
        let desc = spans_desc(s, WrapMode::None);
        let buf = paint_root(desc, 10, 1);
        assert_eq!(buf.lines[0].cells[0].style, red);
        assert_eq!(buf.lines[0].cells[1].style, red);
        assert_eq!(buf.lines[0].cells[2].style, bold);
        assert_eq!(buf.lines[0].cells[3].style, bold);
    }

    #[test]
    fn spans_wrap_word_splits_across_span_boundary() {
        // Span boundary in the middle of a word should NOT force a break
        // — wrapping operates on the concatenated logical text.
        let s = vec![
            span("hello", Style::default()),
            span(
                " world",
                Style {
                    bold: true,
                    ..Style::default()
                },
            ),
        ];
        let desc = spans_desc(s, WrapMode::Word);
        let buf = paint_root(desc, 6, 2);
        // First row: "hello " (6 cells), second row: "world".
        assert_eq!(cell_at(&buf, 0, 0), "h");
        assert_eq!(cell_at(&buf, 0, 4), "o");
        assert_eq!(cell_at(&buf, 1, 0), "w");
        assert_eq!(cell_at(&buf, 1, 4), "d");
    }

    #[test]
    fn spans_wrap_char_treats_styled_chars_uniformly() {
        let s = vec![
            span("abcd", Style::default()),
            span("efgh", Style::default()),
        ];
        let desc = spans_desc(s, WrapMode::Char);
        let buf = paint_root(desc, 3, 3);
        assert_eq!(cell_at(&buf, 0, 0), "a");
        assert_eq!(cell_at(&buf, 0, 2), "c");
        assert_eq!(cell_at(&buf, 1, 0), "d");
        assert_eq!(cell_at(&buf, 1, 2), "f");
        assert_eq!(cell_at(&buf, 2, 0), "g");
        assert_eq!(cell_at(&buf, 2, 1), "h");
    }

    #[test]
    fn spans_wrap_none_truncates_to_width() {
        let s = vec![span("abcdefghij", Style::default())];
        let desc = spans_desc(s, WrapMode::None);
        let buf = paint_root(desc, 4, 1);
        assert_eq!(cell_at(&buf, 0, 0), "a");
        assert_eq!(cell_at(&buf, 0, 3), "d");
    }

    #[test]
    fn spans_explicit_newline_breaks_line() {
        let s = vec![span("a\nb", Style::default())];
        let desc = spans_desc(s, WrapMode::Word);
        let buf = paint_root(desc, 10, 2);
        assert_eq!(cell_at(&buf, 0, 0), "a");
        assert_eq!(cell_at(&buf, 1, 0), "b");
    }

    // ── Markdown ─────────────────────────────────────────────────────

    fn markdown_desc(source: &str, theme: Option<MarkdownTheme>) -> WidgetDescription {
        WidgetDescription::Markdown {
            source: source.into(),
            theme,
            wrap: WrapMode::Word,
            key: None,
        }
    }

    #[test]
    fn markdown_renders_paragraph_through_engine() {
        let desc = markdown_desc("hello", None);
        let buf = paint_root(desc, 20, 2);
        assert_eq!(cell_at(&buf, 0, 0), "h");
        assert_eq!(cell_at(&buf, 0, 4), "o");
    }

    #[test]
    fn markdown_with_theme_paints_styled_chars() {
        use crate::desc::Color;
        let h1 = Style {
            bold: true,
            fg: Some(Color::Rgb(0xff, 0, 0)),
            ..Style::default()
        };
        let theme = MarkdownTheme {
            h1: Some(h1),
            ..MarkdownTheme::default()
        };
        let desc = markdown_desc("# Title", Some(theme));
        let buf = paint_root(desc, 20, 2);
        let cell_t = &buf.lines[0].cells[0];
        assert_eq!(cell_t.text, "T");
        assert_eq!(cell_t.style, h1);
    }

    #[test]
    fn markdown_neutral_theme_keeps_default_style() {
        let desc = markdown_desc("**bold**", None);
        let buf = paint_root(desc, 20, 2);
        let b = &buf.lines[0].cells[0];
        assert_eq!(b.text, "b");
        assert_eq!(b.style, Style::default());
    }
}
