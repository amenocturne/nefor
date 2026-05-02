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

use crate::desc::{Alignment, Style, WidgetDescription, WrapMode};
use crate::instance::{InstanceKind, WidgetInstance};
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
        InstanceKind::Column => layout_column(inst, c),
        InstanceKind::Row => layout_row(inst, c),
        InstanceKind::Padding => layout_padding(inst, c),
        InstanceKind::Stack => layout_stack(inst, c),
        InstanceKind::Expanded => layout_expanded(inst, c),
        InstanceKind::Spacer => layout_spacer(inst, c),
        InstanceKind::Constrained => layout_constrained(inst, c),
        InstanceKind::Align => layout_align(inst, c),
    };
    inst.layout.size = size;
    size
}

/// Paint pass — `inst` paints itself into `out` at the given `rect`.
/// `inst.layout` must have been populated by a prior `layout` call.
pub fn paint(inst: &WidgetInstance, rect: Rect, out: &mut FrameBuffer) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    match inst.kind() {
        InstanceKind::Text => paint_text(inst, rect, out),
        InstanceKind::Column => paint_column(inst, rect, out),
        InstanceKind::Row => paint_row(inst, rect, out),
        InstanceKind::Padding => paint_padding(inst, rect, out),
        InstanceKind::Stack => paint_stack(inst, rect, out),
        InstanceKind::Expanded | InstanceKind::Constrained => paint_passthrough(inst, rect, out),
        InstanceKind::Spacer => { /* empty */ }
        InstanceKind::Align => paint_align(inst, rect, out),
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

fn paint_text(inst: &WidgetInstance, rect: Rect, out: &mut FrameBuffer) {
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

// ── Column ───────────────────────────────────────────────────────────────

fn layout_column(inst: &mut WidgetInstance, c: Constraints) -> Size {
    let gap = match &inst.last_desc {
        WidgetDescription::Column { gap, .. } => *gap,
        _ => unreachable!(),
    };
    flex_layout(inst, c, Axis::Vertical, gap)
}

fn paint_column(inst: &WidgetInstance, rect: Rect, out: &mut FrameBuffer) {
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

fn paint_row(inst: &WidgetInstance, rect: Rect, out: &mut FrameBuffer) {
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

fn paint_flex(inst: &WidgetInstance, rect: Rect, out: &mut FrameBuffer, axis: Axis, gap: u16) {
    let n = inst.children.len();
    if n == 0 {
        return;
    }
    let main_sizes = &inst.layout.flex_main_sizes;
    let mut cursor: u16 = 0;
    let main_total = main_of(axis, rect.width, rect.height);
    for (i, child) in inst.children.iter().enumerate() {
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

fn paint_padding(inst: &WidgetInstance, rect: Rect, out: &mut FrameBuffer) {
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
    if let Some(child) = inst.children.first() {
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

fn paint_stack(inst: &WidgetInstance, rect: Rect, out: &mut FrameBuffer) {
    for child in inst.children.iter() {
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
    // Fill: tight to max on both axes (parent will have already shrunk
    // max via flex distribution if this lives in a row/column).
    let target = Size {
        width: c.max_width,
        height: c.max_height,
    };
    if let Some(child) = inst.children.first_mut() {
        let _ = layout(
            child,
            Constraints {
                min_width: 0,
                max_width: c.max_width,
                min_height: 0,
                max_height: c.max_height,
            },
        );
    }
    c.constrain(target)
}

fn layout_spacer(_inst: &mut WidgetInstance, c: Constraints) -> Size {
    c.constrain(Size {
        width: c.max_width,
        height: c.max_height,
    })
}

fn paint_passthrough(inst: &WidgetInstance, rect: Rect, out: &mut FrameBuffer) {
    if let Some(child) = inst.children.first() {
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

fn paint_align(inst: &WidgetInstance, rect: Rect, out: &mut FrameBuffer) {
    let alignment = match &inst.last_desc {
        WidgetDescription::Align { alignment, .. } => *alignment,
        _ => return,
    };
    let Some(child) = inst.children.first() else {
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
    use crate::desc::{Alignment, WidgetDescription, WrapMode};
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
}
