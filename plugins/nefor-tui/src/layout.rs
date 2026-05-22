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

use crate::animation::{sample as animation_sample, AnimationState};
use crate::desc::{
    Alignment, Anchor, AnimationFrame, Dimension, ScrollableStyle, Span, Style, TextInputStyle,
    WidgetDescription, WrapMode,
};
use crate::instance::{InstanceKind, InstanceState, WidgetInstance};
use crate::render::{Cell, FrameBuffer};
use crate::scrollable::{ScrollbarMode, StickTo};

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
    if inst.layout.cached_constraints == Some(c) {
        let cacheable = matches!(
            inst.kind(),
            InstanceKind::Text
                | InstanceKind::Spans
                | InstanceKind::Markdown
                | InstanceKind::Animation
                | InstanceKind::Spacer
                | InstanceKind::Fill
        );
        if cacheable {
            return inst.layout.size;
        }
    }
    let size = match inst.kind() {
        InstanceKind::Text => layout_text(inst, c),
        InstanceKind::Spans => layout_spans(inst, c),
        InstanceKind::Markdown => layout_markdown(inst, c),
        InstanceKind::Animation => layout_animation(inst, c),
        InstanceKind::Column => layout_column(inst, c),
        InstanceKind::Row => layout_row(inst, c),
        InstanceKind::Padding => layout_padding(inst, c),
        InstanceKind::Stack => layout_stack(inst, c),
        InstanceKind::Expanded => layout_expanded(inst, c),
        InstanceKind::Spacer => layout_spacer(inst, c),
        InstanceKind::Fill => layout_fill(inst, c),
        InstanceKind::Constrained => layout_constrained(inst, c),
        InstanceKind::Align => layout_align(inst, c),
        InstanceKind::Anchored => layout_anchored(inst, c),
        InstanceKind::TextInput => layout_text_input(inst, c),
        InstanceKind::Scrollable => layout_scrollable(inst, c),
    };
    inst.layout.cached_constraints = Some(c);
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
        InstanceKind::Animation => paint_animation(inst, rect, out),
        InstanceKind::Column => paint_column(inst, rect, out),
        InstanceKind::Row => paint_row(inst, rect, out),
        InstanceKind::Padding => paint_padding(inst, rect, out),
        InstanceKind::Stack => paint_stack(inst, rect, out),
        InstanceKind::Expanded | InstanceKind::Constrained => paint_passthrough(inst, rect, out),
        InstanceKind::Spacer => { /* empty */ }
        InstanceKind::Fill => paint_fill(inst, rect, out),
        InstanceKind::Align => paint_align(inst, rect, out),
        InstanceKind::Anchored => paint_anchored(inst, rect, out),
        InstanceKind::TextInput => paint_text_input(inst, rect, out),
        InstanceKind::Scrollable => paint_scrollable(inst, rect, out),
    }
}

// ── Text ─────────────────────────────────────────────────────────────────

fn layout_text(inst: &mut WidgetInstance, c: Constraints) -> Size {
    let (content, wrap) = match &inst.last_desc {
        WidgetDescription::Text { content, wrap, .. } => (content.clone(), *wrap),
        _ => {
            tracing::warn!("layout_text: kind/desc mismatch");
            return Size::default();
        }
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
    let layout_w = inst.layout.size.width.max(1);
    let rows = wrap_text(content, layout_w, wrap);
    let written_rows = rows.len().min(rect.height as usize);
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
        let row_w = string_width(&safe) as u16;
        write_run(out, row, rect.col, &safe, &style);
        // Occlusion contract: a leaf text widget owns the entire rect
        // its parent allocated, not just the cells the glyphs fall on.
        // When the painted line is narrower than `rect.width`, blank
        // out the trailing cells with the same style — this is what
        // makes `tui.text { style = { bg = ... }}` a reliable solid
        // overlay in stack composition. Without it, anything painted
        // earlier (lower z-order) bleeds through the gap.
        if row_w < rect.width {
            let blank_col = rect.col.saturating_add(row_w);
            let blank_w = rect.width - row_w;
            blank_run(out, row, blank_col, blank_w, &style);
        }
    }
    // Same idea on the cross axis: rows the wrap didn't produce still
    // belong to this widget, so styled blanks fill the remainder.
    for i in written_rows..rect.height as usize {
        let row = rect.row.saturating_add(i as u16);
        blank_run(out, row, rect.col, rect.width, &style);
    }
}

/// Fill `[col, col + width)` on `row` with blank cells carrying `style`.
/// Used by leaf widgets (text, spans, markdown) to enforce the
/// "widget owns its rect" occlusion contract — trailing/leading cells
/// the glyphs don't touch still get the widget's style so a bg paints
/// as a solid rectangle.
fn blank_run(buf: &mut FrameBuffer, row: u16, col_start: u16, width: u16, style: &Style) {
    let Some(row_idx) = buf.resolve_row(row as usize) else {
        return;
    };
    let line = &mut buf.lines[row_idx];
    let start = col_start as usize;
    let end = start.saturating_add(width as usize).min(line.cells.len());
    for col in start..end {
        line.cells[col] = Cell {
            text: " ".to_string(),
            style: *style,
        };
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
        _ => {
            tracing::warn!("layout_spans: kind/desc mismatch");
            return Size::default();
        }
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
    let chars = render_markdown_chars(inst, c.max_width);
    let wrap = match &inst.last_desc {
        WidgetDescription::Markdown { wrap, .. } => *wrap,
        _ => WrapMode::Word,
    };
    let rows = wrap_styled(&chars, c.max_width, wrap);
    measure_styled_rows(&rows, c)
}

fn paint_markdown(inst: &mut WidgetInstance, rect: Rect, out: &mut FrameBuffer) {
    // Use the width from the layout pass so table column widths and
    // word wrapping match exactly what was measured. rect.width can
    // differ from the layout constraint (e.g. scrollable two-pass
    // layout), and tables are sensitive to even 1-column differences
    // because column shrinking is proportional.
    let layout_w = inst.layout.size.width.max(1);
    let chars = render_markdown_chars(inst, layout_w);
    let wrap = match &inst.last_desc {
        WidgetDescription::Markdown { wrap, .. } => *wrap,
        _ => WrapMode::Word,
    };
    let rows = wrap_styled(&chars, layout_w, wrap);
    paint_styled_rows(&rows, rect, out);
}

fn render_markdown_chars(inst: &WidgetInstance, available_width: u16) -> Vec<StyledChar> {
    match &inst.last_desc {
        WidgetDescription::Markdown { source, theme, .. } => {
            // 0 = unconstrained (loose constraints with no parent budget);
            // pass `None` so the table renderer falls back to natural widths
            // rather than collapsing every column to the floor.
            let aw = if available_width == 0 {
                None
            } else {
                Some(available_width as usize)
            };
            crate::markdown::render_to_styled_chars(source, theme.as_ref(), aw)
        }
        _ => Vec::new(),
    }
}

// ── Animation ────────────────────────────────────────────────────────────

fn layout_animation(inst: &mut WidgetInstance, c: Constraints) -> Size {
    let chars = sampled_animation_chars(inst);
    let rows = wrap_styled(&chars, c.max_width, WrapMode::None);
    measure_styled_rows(&rows, c)
}

fn paint_animation(inst: &mut WidgetInstance, rect: Rect, out: &mut FrameBuffer) {
    let chars = sampled_animation_chars(inst);
    let rows = wrap_styled(&chars, rect.width, WrapMode::None);
    paint_styled_rows(&rows, rect, out);
}

/// Sample the current animation frame and convert it to styled chars.
/// Mounts (records `mount_time_ms`) on first observation. The clock
/// value comes from [`crate::engine::current_render_time_ms`], which
/// the engine sets before each measure/paint pass and resets after.
fn sampled_animation_chars(inst: &mut WidgetInstance) -> Vec<StyledChar> {
    let (frames, duration_ms, iterations, direction) = match &inst.last_desc {
        WidgetDescription::Animation {
            frames,
            duration_ms,
            iterations,
            direction,
            ..
        } => (frames.clone(), *duration_ms, *iterations, *direction),
        _ => return Vec::new(),
    };
    if frames.is_empty() || duration_ms == 0 {
        return Vec::new();
    }
    let now = crate::engine::current_render_time_ms();
    let state = match &mut inst.state {
        InstanceState::Animation(s) => s,
        _ => return Vec::new(),
    };
    let mount = match state.mount_time_ms {
        Some(t) => t,
        None => {
            state.mount_time_ms = Some(now);
            now
        }
    };
    let _ = AnimationState::default; // keep AnimationState import warning-free
    let s = animation_sample(frames.len(), duration_ms, iterations, direction, mount, now);
    match &frames[s.frame_index] {
        AnimationFrame::Text(t) => styled_chars_from_str(t, Style::default()),
        AnimationFrame::Spans(spans) => styled_chars_from_spans(spans),
    }
}

/// Convert a plain string + style into styled chars.
pub(crate) fn styled_chars_from_str(s: &str, style: Style) -> Vec<StyledChar> {
    s.chars().map(|ch| StyledChar { ch, style }).collect()
}

// ── Column ───────────────────────────────────────────────────────────────

fn layout_column(inst: &mut WidgetInstance, c: Constraints) -> Size {
    let gap = match &inst.last_desc {
        WidgetDescription::Column { gap, .. } => *gap,
        _ => {
            tracing::warn!("layout_column: kind/desc mismatch, defaulting gap to 0");
            0
        }
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
        _ => {
            tracing::warn!("layout_row: kind/desc mismatch, defaulting gap to 0");
            0
        }
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

/// Three-phase flex distribution (shared between column + row).
///
/// Main-axis allocation (CSS `flex-grow` analogue):
///   • Pass 1 — measure non-flex children with their natural main-axis
///     size, capping at the remaining budget.
///   • Pass 2 — distribute leftover main-axis space across flex children
///     (`expanded` / `spacer`) proportional to `flex`. Residual goes to
///     the last flex child so totals match exactly.
///
/// Cross-axis stretch (CSS `align-items: stretch` default):
///   • Pass 3 — children that opt into cross-greedy (`tui.fill` and
///     wrappers around it) are re-measured with the row's resolved
///     cross size as a TIGHT constraint. The row's cross is
///     `max(non-greedy children's cross sizes)`, clamped into the
///     parent's `[min_cross, max_cross]` bounds. If every child is
///     cross-greedy (no anchor), the row claims the parent's full
///     cross — matching CSS flexbox.
///
/// The classification of a child as cross-greedy is intrinsic to the
/// primitive (see [`child_cross_greedy_in_axis`]), not configurable per
/// row/column. Users wanting natural-cross alignment for an otherwise
/// greedy child can wrap it in `tui.align`, which absorbs the parent's
/// cross and positions the child at its natural size.
///
/// Edge cases worth keeping in mind:
///   • If `remaining` is 0 (or negative once gaps are subtracted), every
///     flex child gets `0` on the main axis and lays out tight on that
///     axis. Cross-greedy reach is unaffected — they still stretch to
///     match the resolved row cross.
///   • Integer division uses (`remaining * weight) / total_flex`. The
///     residual (so total flex sizes don't sum exactly to `remaining`) is
///     handed to the *last* flex child to keep totals consistent.
fn flex_layout(inst: &mut WidgetInstance, c: Constraints, axis: Axis, gap: u16) -> Size {
    let n = inst.children.len();
    if n == 0 {
        return c.constrain(Size::default());
    }

    let main_max = main_of(axis, c.max_width, c.max_height);
    let cross_min = cross_of(axis, c.min_width, c.min_height);
    let cross_max = cross_of(axis, c.max_width, c.max_height);
    let total_gap = (n as u16).saturating_sub(1).saturating_mul(gap);

    // Inspect children for main-axis flex factors and cross-axis greed.
    // Both classifications are static functions of the child's
    // descriptor — the layout engine never asks the child to opt in
    // dynamically. Computed once up front so passes 1-3 share the
    // results without re-walking the child tree.
    let flex_factors: Vec<u16> = inst.children.iter().map(child_flex_factor).collect();
    let cross_greedy: Vec<bool> = inst
        .children
        .iter()
        .map(|c| child_cross_greedy_in_axis(c, axis))
        .collect();
    let total_flex: u32 = flex_factors.iter().map(|&f| f as u32).sum();

    let mut sizes: Vec<Size> = vec![Size::default(); n];
    // Per-child main allotment captured during passes 1+2 so phase 3
    // can re-measure cross-greedy children with the same main constraint
    // they got the first time. Without this capture, the re-measure
    // would lose the flex distribution result.
    let mut main_allotments: Vec<u16> = vec![0; n];
    let mut non_flex_main: u32 = 0;

    // Pass 1: lay out non-flex children with their natural main-axis size.
    for i in 0..n {
        if flex_factors[i] > 0 {
            continue;
        }
        let child_main_max = main_max
            .saturating_sub(non_flex_main as u16)
            .saturating_sub(total_gap);
        main_allotments[i] = child_main_max;
        let child_constraints = match axis {
            Axis::Vertical => Constraints {
                min_width: 0,
                max_width: c.max_width,
                min_height: 0,
                max_height: child_main_max,
            },
            Axis::Horizontal => Constraints {
                min_width: 0,
                max_width: child_main_max,
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
        main_allotments[i] = allotment;

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

    // Compute row's resolved cross size (CSS `align-items: stretch`):
    // ignore cross-greedy children's first-pass sizes (they had no
    // natural cross to contribute); take the max of the rest. If every
    // child is cross-greedy, fall back to `cross_max` so the row spans
    // its full available cross — matches flexbox default.
    let any_non_greedy = cross_greedy.iter().any(|&g| !g);
    let natural_cross: u16 = if any_non_greedy {
        sizes
            .iter()
            .zip(cross_greedy.iter())
            .filter_map(|(s, &greedy)| (!greedy).then_some(cross_of(axis, s.width, s.height)))
            .max()
            .unwrap_or(0)
    } else {
        cross_max
    };
    let row_cross = natural_cross.clamp(cross_min, cross_max);

    // Pass 3: re-measure cross-greedy children with the row's cross
    // size as a tight constraint. Their main-axis allotment is the same
    // as in passes 1/2 — re-applying it via `main_allotments[i]` keeps
    // flex distribution stable.
    for i in 0..n {
        if !cross_greedy[i] {
            continue;
        }
        let allotment = main_allotments[i];
        let is_flex = flex_factors[i] > 0;
        let child_constraints = match axis {
            Axis::Vertical => Constraints {
                min_width: row_cross,
                max_width: row_cross,
                min_height: if is_flex { allotment } else { 0 },
                max_height: allotment,
            },
            Axis::Horizontal => Constraints {
                min_width: if is_flex { allotment } else { 0 },
                max_width: allotment,
                min_height: row_cross,
                max_height: row_cross,
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

    let raw = match axis {
        Axis::Vertical => Size {
            width: row_cross,
            height: main_used.min(u16::MAX as u32) as u16,
        },
        Axis::Horizontal => Size {
            width: main_used.min(u16::MAX as u32) as u16,
            height: row_cross,
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
        _ => {
            tracing::warn!("layout_padding: kind/desc mismatch, defaulting padding to 0");
            (0, 0, 0, 0)
        }
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

fn layout_fill(_inst: &mut WidgetInstance, c: Constraints) -> Size {
    // Greedy: claim the parent's full max on both axes. The same shape
    // as a Flutter `Container` with no child but a decoration, or HTML
    // `width: 100%; height: 100%` inside a bounded parent.
    //
    // Inside a `row` / `column`, the parent's flex layout classifies
    // `Fill` as cross-greedy and re-measures with a tight cross
    // constraint after determining the row's cross from non-greedy
    // siblings (CSS `align-items: stretch`). So the bare-`max_height`
    // here is the unconstrained-parent path; inside a flex parent the
    // tight constraint funnels through `c.constrain` and produces
    // `row_cross × main_allotment`.
    c.constrain(Size {
        width: c.max_width,
        height: c.max_height,
    })
}

fn paint_fill(inst: &mut WidgetInstance, rect: Rect, out: &mut FrameBuffer) {
    let (ch, style) = match &inst.last_desc {
        WidgetDescription::Fill { char, style, .. } => (char.as_str(), style.unwrap_or_default()),
        _ => return,
    };
    if ch.is_empty() || rect.width == 0 || rect.height == 0 {
        return;
    }
    // Pre-build one row's worth of repeats so `write_run` is called
    // once per row instead of once per cell. The desc parser rejects
    // empty strings; multi-grapheme inputs paint the literal sequence
    // every column. Zero-width inputs (e.g. lone ZWJ) get clamped to a
    // 1-cell stride so the loop terminates.
    let unit_width = string_width(ch).max(1);
    let total_width = rect.width as usize;
    let repeats = total_width / unit_width;
    let mut row_text = String::with_capacity(repeats * ch.len() + ch.len());
    for _ in 0..repeats {
        row_text.push_str(ch);
    }
    // Tail: emit one more unit if there are leftover columns; the
    // per-cell width-clamp inside `write_run` drops any overhang so a
    // 2-col char in a 1-col tail isn't half-painted.
    if !total_width.is_multiple_of(unit_width) {
        row_text.push_str(ch);
    }
    for row_off in 0..rect.height {
        let row = rect.row.saturating_add(row_off);
        write_run(out, row, rect.col, &row_text, &style);
    }
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
        _ => {
            tracing::warn!("layout_constrained: kind/desc mismatch, using unconstrained defaults");
            (None, None, None, None)
        }
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
        _ => {
            tracing::warn!("layout_anchored: kind/desc mismatch, defaulting to intrinsic dimensions");
            (Dimension::Intrinsic, Dimension::Intrinsic)
        }
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
        _ => {
            tracing::warn!("layout_text_input: kind/desc mismatch");
            return Size::default();
        }
    };
    // Width: prefer parent's max so wrapping/scroll can use the full
    // budget. Sync state's `last_value` here so the paint pass and the
    // input router both see the latest.
    let viewport_w = c.max_width;
    if let InstanceState::TextInput(st) = &mut inst.state {
        let focused = matches!(
            &inst.last_desc,
            WidgetDescription::TextInput { focused, .. } if *focused
        );
        st.sync_with_desc(&value, focused);
        st.viewport_width = viewport_w;
        // For single-line inputs, keep `scroll_x` glued to the cursor.
        // For multi-line inputs `scroll_x` is unused (soft-wrap covers
        // overflow) — clear it so a value rewrite doesn't strand a
        // stale offset.
        if max_lines == 1 {
            sync_single_line_scroll_x(st, viewport_w);
        } else {
            st.scroll_x = 0;
        }
    }

    let visible_lines = visible_line_count(&value, min_lines, max_lines, viewport_w);
    // Multi-line: pin scroll_y so the cursor's wrapped row stays inside
    // the visible window. Default behaviour matches Claude Code's input
    // — the box grows up to `max_lines`, then internal-scrolls with the
    // cursor anchored to the visible region. Without this, scroll_y
    // stays at 0 forever and a paste / typing run past the cap shows
    // the TOP of the buffer with the cursor scrolled off the bottom.
    if max_lines > 1 && viewport_w > 0 {
        if let InstanceState::TextInput(st) = &mut inst.state {
            sync_multi_line_scroll_y(st, &value, viewport_w, visible_lines);
        }
    }
    let raw = Size {
        width: c.max_width,
        height: visible_lines.min(c.max_height),
    };
    c.constrain(raw)
}

/// Number of rows the input wants to occupy. Bounded by `[min_lines,
/// max_lines]`. Multi-line inputs (`max_lines > 1`) soft-wrap to the
/// viewport so a long buffer grows vertically up to the cap;
/// single-line inputs (`max_lines == 1`) always claim one row and rely
/// on horizontal scrolling for overflow.
pub(crate) fn visible_line_count(
    value: &str,
    min_lines: u16,
    max_lines: u16,
    viewport_w: u16,
) -> u16 {
    if max_lines <= 1 {
        return min_lines.max(1);
    }
    if viewport_w == 0 {
        return min_lines;
    }
    let actual = crate::text_input::soft_wrapped_line_count(value, viewport_w) as u32;
    actual
        .clamp(min_lines as u32, max_lines as u32)
        .min(u16::MAX as u32) as u16
}

/// Single-line cursor-tracking scroll: bump `scroll_x` so the cursor
/// stays inside `[scroll_x, scroll_x + viewport_w)`. Called once per
/// layout — viewport width is known here, so this is the natural seam
/// for the "input disappears off-screen" fix.
fn sync_single_line_scroll_x(st: &mut crate::text_input::TextInputState, viewport_w: u16) {
    if viewport_w == 0 {
        return;
    }
    // Width of the value up to the cursor — that's the cursor's logical
    // column on a single-line input.
    let prefix = &st.last_value[..st.cursor.min(st.last_value.len())];
    let cursor_col: usize = prefix.chars().map(unicode_col_width).sum();
    let scroll_x = st.scroll_x as usize;
    let viewport = viewport_w as usize;
    let new_scroll = if cursor_col < scroll_x {
        cursor_col as u16
    } else if cursor_col >= scroll_x + viewport {
        // Keep the cursor on the rightmost cell.
        (cursor_col + 1).saturating_sub(viewport) as u16
    } else {
        st.scroll_x
    };
    st.scroll_x = new_scroll;
}

fn unicode_col_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

/// Multi-line cursor-tracking vertical scroll. Bumps `scroll_y` so the
/// cursor's wrapped row stays inside `[scroll_y, scroll_y + visible)`.
/// Mirrors [`sync_single_line_scroll_x`] on the y-axis.
///
/// Default-anchor-to-cursor: any time the cursor moves past either edge
/// of the visible window (typing / pasting at end → past the bottom;
/// arrow-up at the top of the window → past the top), we slide the
/// window to put the cursor back in view. Above-cap content is hidden
/// off the TOP, not the bottom — Claude-style.
///
/// Suspended while `state.manual_scroll == true` — the user is wheeling
/// through the buffer and the cursor-pin would otherwise yank the
/// viewport back as soon as the next layout pass ran. We still clamp
/// against `max_scroll` so a value-shrink (submit clears buffer) drops
/// scroll_y to 0; that path also resets the latch through
/// `sync_with_desc`.
fn sync_multi_line_scroll_y(
    st: &mut crate::text_input::TextInputState,
    value: &str,
    viewport_w: u16,
    visible_lines: u16,
) {
    if visible_lines == 0 {
        return;
    }
    let rows = crate::text_input::wrap_value(value, viewport_w);
    let total = rows.len() as u32;
    let visible = visible_lines as u32;
    let scroll_y = st.scroll_y as u32;
    // Clamp first to handle a value rewrite that shrank the buffer
    // below the prior offset (e.g. submit clears value → buffer = 1
    // row, scroll_y was 5 → must reset to 0).
    let max_scroll = total.saturating_sub(visible);
    let new_scroll = scroll_y.min(max_scroll);
    if st.manual_scroll {
        // Manual-scroll latch active: leave the user's offset alone
        // (still post-clamp). Pin re-engages when an editing key or a
        // value rewrite clears the latch.
        st.scroll_y = new_scroll.min(u16::MAX as u32) as u16;
        return;
    }
    let (cursor_row, _) = crate::text_input::cursor_in_wrap_for(value, &rows, st.cursor);
    let cursor_row_u = cursor_row as u32;
    let mut new_scroll = new_scroll;
    if cursor_row_u < new_scroll {
        new_scroll = cursor_row_u;
    } else if cursor_row_u >= new_scroll + visible {
        new_scroll = cursor_row_u + 1 - visible;
    }
    st.scroll_y = new_scroll.min(u16::MAX as u32) as u16;
}

fn paint_text_input(inst: &mut WidgetInstance, rect: Rect, out: &mut FrameBuffer) {
    let (value, focused, placeholder, style, max_lines) = match &inst.last_desc {
        WidgetDescription::TextInput {
            value,
            focused,
            placeholder,
            style,
            max_lines,
            ..
        } => (
            value.as_str(),
            *focused,
            placeholder.clone(),
            *style,
            *max_lines,
        ),
        _ => return,
    };
    let st = match &inst.state {
        InstanceState::TextInput(s) => s,
        _ => return,
    };
    let style = style.unwrap_or_default();

    let body_style = body_style_for(&style);
    let placeholder_style = placeholder_style_for(&style);
    let is_placeholder_run = value.is_empty() && placeholder.is_some();

    if max_lines > 1 {
        // Soft-wrap: paint each wrapped row.
        let display_value: &str = if is_placeholder_run {
            placeholder.as_deref().unwrap_or_default()
        } else {
            value
        };
        let rows = crate::text_input::wrap_value(display_value, rect.width);
        let scroll_y = st.scroll_y as usize;
        for r in 0..rect.height as usize {
            let row_y = rect.row.saturating_add(r as u16);
            let row_idx = scroll_y + r;
            let row = rows.get(row_idx);
            let slice = match row {
                Some(rw) => {
                    let lo = rw.start_byte.min(display_value.len());
                    let hi = rw.end_byte.min(display_value.len());
                    &display_value[lo..hi]
                }
                None => "",
            };
            let safe = enforce_width_contract(slice, rect.width);
            let run_style = if is_placeholder_run {
                placeholder_style
            } else {
                body_style
            };
            write_run(out, row_y, rect.col, &safe, &run_style);
        }
        if focused && !is_placeholder_run {
            paint_cursor_wrapped(rect, out, st, value, &rows, &style);
        }
        return;
    }

    // Single-line: horizontal scroll model unchanged.
    let lines: Vec<&str> = if is_placeholder_run {
        vec![placeholder.as_deref().unwrap_or_default()]
    } else if value.is_empty() {
        vec![""]
    } else {
        value.split('\n').collect()
    };

    let scroll_y = st.scroll_y as usize;
    let scroll_x = st.scroll_x as usize;

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

/// Cursor painter for soft-wrapped multi-line text_input. Maps the
/// byte cursor through the wrapped layout to a `(visual_row, col)`
/// pair, then draws a reverse-video cell at that position (clipped to
/// the viewport).
fn paint_cursor_wrapped(
    rect: Rect,
    out: &mut FrameBuffer,
    st: &crate::text_input::TextInputState,
    value: &str,
    rows: &[crate::text_input::WrappedRow],
    style: &TextInputStyle,
) {
    let (visual_row, col) = crate::text_input::cursor_in_wrap_for(value, rows, st.cursor);
    let scroll_y = st.scroll_y as usize;
    if visual_row < scroll_y {
        return;
    }
    let row_within = visual_row - scroll_y;
    if row_within >= rect.height as usize {
        return;
    }
    if col >= rect.width as usize {
        return;
    }
    let row_idx = (rect.row as usize).saturating_add(row_within);
    let col_idx = (rect.col as usize).saturating_add(col);
    if row_idx >= out.lines.len() {
        return;
    }
    let line = &mut out.lines[row_idx];
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
        strikethrough: false,
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
        strikethrough: false,
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

// ── Scrollable ───────────────────────────────────────────────────────────

/// Whether this scrollable should reserve a column for the scrollbar
/// gutter, given its visibility policy + cached geometry. The reserved
/// column is consistent between layout and paint so the child sees the
/// same constraints in both passes.
fn scrollbar_visible(mode: ScrollbarMode, content_height: u16, viewport_height: u16) -> bool {
    let overflows = content_height > viewport_height;
    match mode {
        ScrollbarMode::Always => true,
        ScrollbarMode::Auto => overflows,
        ScrollbarMode::Never => false,
    }
}

fn layout_scrollable(inst: &mut WidgetInstance, c: Constraints) -> Size {
    let (mode, _stick_to, vch) = match &inst.last_desc {
        WidgetDescription::Scrollable {
            scrollbar,
            stick_to,
            virtual_content_height,
            ..
        } => (*scrollbar, *stick_to, *virtual_content_height),
        _ => {
            tracing::warn!("layout_scrollable: kind/desc mismatch");
            return c.constrain(Size::default());
        }
    };

    // The scrollable claims its parent's max bounds. Then we lay the
    // child out under unbounded vertical extent so its returned size is
    // the natural content height.
    let viewport_w = c.max_width;
    let viewport_h = c.max_height;
    if viewport_w == 0 || viewport_h == 0 {
        return c.constrain(Size::default());
    }

    // Pass 1 — measure child without reserving the gutter so we know if
    // overflow is real (in `Auto` mode the gutter reservation depends on
    // whether content actually overflows).
    let prelim_w = viewport_w;
    let prelim_constraints = Constraints {
        min_width: 0,
        max_width: prelim_w,
        min_height: 0,
        max_height: u16::MAX,
    };
    let prelim_size = match inst.children.first_mut() {
        Some(child) => layout(child, prelim_constraints),
        None => Size::default(),
    };
    let content_height_for_bar = vch.unwrap_or(prelim_size.height);

    let show_bar = scrollbar_visible(mode, content_height_for_bar, viewport_h);

    // Pass 2 — when the gutter is visible, re-measure the child with the
    // reduced inner width so wrapping reflects the post-gutter geometry.
    // (Skipping pass 2 when the gutter is hidden keeps measurement cheap.)
    let inner_w = if show_bar {
        viewport_w.saturating_sub(1)
    } else {
        viewport_w
    };
    let final_size = if show_bar && inner_w != prelim_w {
        let final_constraints = Constraints {
            min_width: 0,
            max_width: inner_w,
            min_height: 0,
            max_height: u16::MAX,
        };
        match inst.children.first_mut() {
            Some(child) => layout(child, final_constraints),
            None => Size::default(),
        }
    } else {
        prelim_size
    };

    // Stash the geometry for the paint pass + Lua-visible scroll APIs.
    // When virtual_content_height is set, use it instead of the measured
    // child height. This breaks the feedback loop in virtual-scroll
    // scenarios where estimated heights ≠ actual rendered heights cause
    // content_height oscillation.
    let effective_content_height = vch.unwrap_or(final_size.height);
    if let InstanceState::Scrollable(s) = &mut inst.state {
        s.content_height = effective_content_height;
        s.measured_content_height = final_size.height;
        s.viewport_height = viewport_h;
    }

    c.constrain(Size {
        width: viewport_w,
        height: viewport_h,
    })
}

fn paint_scrollable(inst: &mut WidgetInstance, rect: Rect, out: &mut FrameBuffer) {
    let (mode, stick_to, style) = match &inst.last_desc {
        WidgetDescription::Scrollable {
            scrollbar,
            stick_to,
            style,
            ..
        } => (*scrollbar, *stick_to, *style),
        _ => return,
    };
    if rect.width == 0 || rect.height == 0 {
        return;
    }

    // Read the geometry the layout pass cached, then settle the final
    // scroll_y (apply stick_to, clamp to max). We mutate state through a
    // small scope so the borrow doesn't fight the child paint below.
    let (scroll_y, content_height, paint_height, show_bar, inner_w) = {
        let st = match &mut inst.state {
            InstanceState::Scrollable(s) => s,
            _ => return,
        };
        let viewport_h = rect.height;
        st.viewport_height = viewport_h;
        let content_height = st.content_height;
        let paint_height = st.measured_content_height.max(content_height);
        let show_bar = scrollbar_visible(mode, content_height, viewport_h);
        let inner_w = if show_bar {
            rect.width.saturating_sub(1)
        } else {
            rect.width
        };
        let max = content_height.saturating_sub(viewport_h);

        // Stick-to handling: pin to the relevant edge before we render
        // when the stickiness flag is still set. First-paint counts as
        // sticky in both directions so transcripts mounted at-bottom
        // stay there before any wheel/key events have moved them.
        let scroll_y = match stick_to {
            Some(StickTo::End) if !st.seeded || st.was_at_end => max,
            Some(StickTo::Start) if !st.seeded || st.was_at_start => 0,
            _ => st.scroll_y.min(max),
        };
        st.scroll_y = scroll_y;
        // Update edge bookkeeping so the next paint pass observes a
        // fresh `was_at_*` snapshot. Once content settles, content_height
        // stays small enough to fit, max == 0 so both edges are true.
        st.was_at_end = max.saturating_sub(scroll_y) <= 1;
        st.was_at_start = scroll_y <= 1;
        st.seeded = true;
        (scroll_y, content_height, paint_height, show_bar, inner_w)
    };

    let content_w = inner_w;
    let viewport_h = rect.height;
    let scratch_w = content_w.max(1);
    let scratch_h = viewport_h.min(paint_height).max(1);
    let mut scratch =
        FrameBuffer::with_offset(scratch_w, scratch_h, scroll_y as usize);
    if let Some(child) = inst.children.first_mut() {
        let child_rect = Rect {
            row: 0,
            col: 0,
            width: content_w,
            height: paint_height,
        };
        paint(child, child_rect, &mut scratch);
    }

    // Copy the scratch buffer (which contains only the visible window)
    // directly into the output framebuffer.
    for r in 0..viewport_h as usize {
        let dst_row = rect.row as usize + r;
        if dst_row >= out.lines.len() {
            break;
        }
        let dst_line = &mut out.lines[dst_row];
        let src_line = scratch.lines.get(r);
        for col in 0..content_w as usize {
            let dst_col = rect.col as usize + col;
            if dst_col >= dst_line.cells.len() {
                break;
            }
            dst_line.cells[dst_col] = src_line
                .and_then(|l| l.cells.get(col))
                .cloned()
                .unwrap_or_else(Cell::blank);
        }
    }

    if show_bar {
        paint_scrollbar(rect, out, content_height, viewport_h, scroll_y, &style);
    }
}

/// Paint the scrollbar gutter in the rect's last column — track + thumb.
/// Thumb position is proportional to `scroll_y / scroll_y_max`; size is
/// proportional to `viewport / content`, with a one-row floor so it stays
/// visible on tiny viewports.
fn paint_scrollbar(
    rect: Rect,
    out: &mut FrameBuffer,
    content_height: u16,
    viewport_height: u16,
    scroll_y: u16,
    style: &Option<ScrollableStyle>,
) {
    let col = rect.col + rect.width.saturating_sub(1);
    let (track_style, thumb_style) = scrollbar_styles(style);

    // Track first — fill every row in the gutter so the thumb overdraws.
    for r in 0..rect.height as usize {
        let row = rect.row.saturating_add(r as u16);
        write_run(out, row, col, "│", &track_style);
    }

    if content_height == 0 || viewport_height == 0 {
        return;
    }
    let viewport_h = viewport_height as u32;
    let content_h = content_height as u32;
    let scroll_y_u = scroll_y as u32;

    let thumb_h = ((viewport_h * viewport_h) / content_h).max(1) as u16;
    let thumb_h = thumb_h.min(viewport_height);
    let track_room = viewport_height.saturating_sub(thumb_h) as u32;
    let scroll_room = content_h.saturating_sub(viewport_h);
    let thumb_top = (scroll_y_u * track_room)
        .checked_div(scroll_room)
        .unwrap_or(0) as u16;

    for r in 0..thumb_h as usize {
        let row = rect.row.saturating_add(thumb_top + r as u16);
        write_run(out, row, col, "█", &thumb_style);
    }
}

/// Resolve user style overrides into `(track_style, thumb_style)`. Both
/// fall back to neutral `Style::default()` when nothing is set, per the
/// no-default-styling rule.
fn scrollbar_styles(style: &Option<ScrollableStyle>) -> (Style, Style) {
    let s = style.unwrap_or_default();
    let track = Style {
        fg: s.scrollbar_fg,
        bg: s.scrollbar_bg,
        ..Style::default()
    };
    let thumb = Style {
        fg: s.thumb.or(s.scrollbar_fg),
        bg: s.scrollbar_bg,
        ..Style::default()
    };
    (track, thumb)
}

// ── Helpers ──────────────────────────────────────────────────────────────

fn child_flex_factor(child: &WidgetInstance) -> u16 {
    match &child.last_desc {
        WidgetDescription::Expanded { flex, .. } | WidgetDescription::Spacer { flex, .. } => *flex,
        _ => 0,
    }
}

/// Whether `child` opts into cross-axis stretch inside a flex parent
/// (`row` or `column`) on `axis`. The flex layout treats cross-greedy
/// children differently:
///
/// 1. their measured cross size does NOT participate in the row's
///    natural-cross calculation (they have no natural cross), and
/// 2. after the row's cross is determined from non-greedy siblings, they
///    are re-measured with a tight cross constraint matching that row.
///
/// CSS-flexbox parallel: `align-items: stretch` is the default; greedy
/// children are the ones that consume the resolved cross size. We make
/// the call per-primitive instead of per-axis-flag because primitives
/// like `tui.fill` are greedy on both axes by definition (paint requires
/// a non-empty rect on both axes), and that property is intrinsic to the
/// primitive's contract — not a layout option exposed at the row level.
///
/// Wrappers (`expanded`, `padding`, `constrained`) delegate to their
/// child so the common composition `expanded { fill }` is correctly
/// classified as cross-greedy without forcing every wrapper to know
/// about the property.
///
/// `axis` is unused at the leaf today (every cross-greedy primitive is
/// greedy on both axes), but it threads through wrappers so a future
/// per-axis primitive (e.g. a hypothetical `tui.hfill` that's greedy
/// only on horizontal) slots in without changing the call sites.
#[allow(clippy::only_used_in_recursion)]
fn child_cross_greedy_in_axis(child: &WidgetInstance, axis: Axis) -> bool {
    match &child.last_desc {
        // Greedy on both axes — claims whatever rect the parent assigns.
        WidgetDescription::Fill { .. } => true,
        // Spacer is empty filler on the main axis only; nothing to draw
        // on the cross axis, so don't pull the row taller than its
        // content.
        WidgetDescription::Spacer { .. } => false,
        // Single-child wrappers delegate. A constrained that bounds the
        // cross axis will still be honored: phase 3 re-measures the
        // child with `min == max == row_cross`, and `Constrained` clamps
        // that into the user's bounds (`Constraints::constrain`). So the
        // delegation is safe for the bounded case.
        WidgetDescription::Expanded { .. }
        | WidgetDescription::Padding { .. }
        | WidgetDescription::Constrained { .. } => child
            .children
            .first()
            .map(|c| child_cross_greedy_in_axis(c, axis))
            .unwrap_or(false),
        _ => false,
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
        // Empty logical lines (consecutive `\n`s) carry the markdown
        // walker's block separators. Preserve them as empty visual rows
        // so blank lines actually render between blocks — without this
        // passthrough, `wrap_styled_word`/`_char` produce zero rows for
        // empty input and the spacing collapses.
        if raw.is_empty() {
            wrapped.push(Vec::new());
            continue;
        }
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
        let is_ws = word.iter().all(|c| c.ch.is_whitespace());

        // Word doesn't fit on the current line — flush before we decide
        // what to do with it.
        if col > 0 && col + ww > limit {
            out.push(std::mem::take(&mut current));
            col = 0;
            if is_ws {
                // Skip pure-whitespace words at line starts so we don't
                // leave a trailing-space artefact at the start of the
                // next line.
                continue;
            }
        }

        // Word is wider than the line itself — fall back to char wrap so
        // it doesn't overflow. Order matters: this check runs AFTER the
        // flush above so a long word arriving mid-line first gets pushed
        // to its own line, then char-wrapped from there. The previous
        // version only caught oversized words that started at col == 0
        // by coincidence, leaving mid-line oversized words to overflow.
        if col == 0 && ww > limit {
            for sub in wrap_styled_char(word, limit) {
                out.push(sub);
            }
            current.clear();
            col = 0;
            continue;
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
    let Some(row_idx) = buf.resolve_row(row as usize) else {
        return;
    };
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
        col += 1;
        // Wide-char spillover (East-Asian Wide / Fullwidth / most
        // emoji): blank the (w - 1) trailing cells so prior-frame ink
        // can't bleed through, AND so total advance equals the glyph's
        // display width. Earlier shape did `col += w` then advanced
        // again per blank, double-counting wide chars and visually
        // gapping CJK runs (Bug 4 wide chars). Mirrors the same fix in
        // `write_run` for plain-text painting.
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
        let is_ws = word.chars().all(char::is_whitespace);

        // Word doesn't fit on the current line — flush before deciding
        // what to do with it.
        if col > 0 && col + ww > limit {
            out.push(std::mem::take(&mut current));
            col = 0;
            if is_ws {
                // Skip pure-whitespace words at line starts so we don't
                // leave a trailing-space artefact.
                continue;
            }
        }

        // Word is wider than the line itself — char-wrap it. Runs AFTER
        // the flush so mid-line oversized words first get pushed to their
        // own line, then char-wrapped. The previous version only caught
        // oversized words that arrived with col == 0 by coincidence,
        // leaving mid-line oversized words to overflow.
        if col == 0 && ww > limit {
            for sub in wrap_char(word, width) {
                out.push(sub);
            }
            current.clear();
            col = 0;
            continue;
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
    let Some(row_idx) = buf.resolve_row(row as usize) else {
        return;
    };
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
        col += 1;
        // Wide chars (East-Asian Wide / Fullwidth / most emoji): the
        // glyph itself painted into one cell above; blank out the
        // remaining (w - 1) trailing cells so a previous frame's ink
        // doesn't bleed through, and so total advance equals the
        // glyph's display width. The earlier shape advanced `col += w`
        // before this loop AND advanced again per iteration, double-
        // counting wide chars: e.g. `你 好` rendered as `你  好` with
        // an extra blank between glyphs and every subsequent char
        // shifted right one cell per wide-char encountered (Bug 4 wide
        // chars).
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
    use crate::scrollable::ScrollableState;

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
            selectable: false,
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
            selectable: false,
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

    fn fill(ch: &str, style: Option<Style>) -> WidgetDescription {
        WidgetDescription::Fill {
            char: ch.into(),
            style,
            key: None,
        }
    }

    #[test]
    fn fill_layout_claims_parent_max_constraints() {
        // Bare fill: layout returns the parent's max bounds on both
        // axes (greedy). Inside a row the user is expected to wrap the
        // row in `tui.constrained { max_height = 1 }` to keep a single
        // rule from bloating the row vertically.
        let desc = fill("─", None);
        let mut rec = Reconciler::new();
        rec.reconcile(desc);
        let root = rec.root.as_mut().unwrap();
        let s = layout(root, Constraints::loose(12, 3));
        assert_eq!(
            s,
            Size {
                width: 12,
                height: 3
            }
        );
    }

    #[test]
    fn fill_paints_char_across_assigned_rect() {
        // Bare fill — paint_root passes `loose(6, 1)` to the root,
        // fill claims max → 6×1 rect filled with `─`.
        let buf = paint_root(fill("─", None), 6, 1);
        for col in 0..6 {
            assert_eq!(cell_at(&buf, 0, col), "─", "col {col} should be filled");
        }
    }

    #[test]
    fn fill_repeats_across_multiple_rows() {
        // Bare fill at the root claims the entire frame.
        let buf = paint_root(fill("─", None), 4, 2);
        for row in 0..2 {
            for col in 0..4 {
                assert_eq!(
                    cell_at(&buf, row, col),
                    "─",
                    "({row},{col}) should be filled"
                );
            }
        }
    }

    #[test]
    fn fill_propagates_style_to_painted_cells() {
        use crate::desc::Color;
        let red = Style {
            fg: Some(Color::Rgb(255, 0, 0)),
            ..Style::default()
        };
        let buf = paint_root(fill("─", Some(red)), 3, 1);
        for col in 0..3 {
            assert_eq!(buf.lines[0].cells[col].style, red);
            assert_eq!(buf.lines[0].cells[col].text.as_str(), "─");
        }
    }

    #[test]
    fn fill_inside_bordered_box_composition() {
        // The intended composition for a bordered input field:
        //   ╭───╮
        //   │ x │
        //   ╰───╯
        // Top/bot rows wrap in `constrained { max_height = 1 }` so
        // fill (which greedily claims the parent's max_height) doesn't
        // bloat the rule rows past 1 row tall.
        let row_with = |corners: (&str, &str)| {
            constrained(
                WidgetDescription::Row {
                    gap: 0,
                    key: None,
                    children: vec![
                        text(corners.0),
                        expanded(fill("─", None), 1),
                        text(corners.1),
                    ],
                },
                None,
                None,
                None,
                Some(1),
            )
        };
        let top = row_with(("╭", "╮"));
        let bot = row_with(("╰", "╯"));
        let mid = WidgetDescription::Row {
            gap: 0,
            key: None,
            children: vec![text("│"), expanded(text("x"), 1), text("│")],
        };
        let desc = WidgetDescription::Column {
            gap: 0,
            key: None,
            selectable: false,
            children: vec![top, mid, bot],
        };
        let buf = paint_root(desc, 5, 3);
        assert_eq!(cell_at(&buf, 0, 0), "╭");
        assert_eq!(cell_at(&buf, 0, 1), "─");
        assert_eq!(cell_at(&buf, 0, 2), "─");
        assert_eq!(cell_at(&buf, 0, 3), "─");
        assert_eq!(cell_at(&buf, 0, 4), "╮");
        assert_eq!(cell_at(&buf, 1, 0), "│");
        assert_eq!(cell_at(&buf, 1, 4), "│");
        assert_eq!(cell_at(&buf, 2, 0), "╰");
        assert_eq!(cell_at(&buf, 2, 1), "─");
        assert_eq!(cell_at(&buf, 2, 4), "╯");
    }

    #[test]
    fn row_with_fill_side_and_multiline_content_stretches_fill() {
        // Cross-axis stretch: side bar = `tui.fill { char = "│" }`,
        // body = a 4-row text. Row's natural cross is 4 rows (from the
        // body). The fill, classified as cross-greedy, is re-measured
        // with `min_cross == max_cross == 4` and paints `│` on every
        // row. Without cross-axis stretch the side bar would only paint
        // at row 0, leaving rows 1..4 unbordered.
        let body = text("aaa\nbbb\nccc\nddd");
        let desc = WidgetDescription::Row {
            gap: 0,
            key: None,
            children: vec![fill("│", None), expanded(body, 1), fill("│", None)],
        };
        let buf = paint_root(desc, 5, 4);
        for row in 0..4 {
            assert_eq!(
                cell_at(&buf, row, 0),
                "│",
                "left side bar must paint row {row}",
            );
            assert_eq!(
                cell_at(&buf, row, 4),
                "│",
                "right side bar must paint row {row}",
            );
        }
    }

    #[test]
    fn column_with_fill_top_and_multiline_content_stretches_fill_horizontally() {
        // Symmetric to the row case: a column with two `tui.fill` bars
        // (one above, one below) and a 5-col body in the middle. Cross
        // axis here is horizontal; bars must paint across the full
        // column cross (5 cols), not collapse to 0.
        let desc = WidgetDescription::Column {
            gap: 0,
            key: None,
            selectable: false,
            children: vec![fill("─", None), expanded(text("hello"), 1), fill("─", None)],
        };
        let buf = paint_root(desc, 5, 3);
        for col in 0..5 {
            assert_eq!(cell_at(&buf, 0, col), "─", "top fill must paint col {col}",);
            assert_eq!(
                cell_at(&buf, 2, col),
                "─",
                "bottom fill must paint col {col}",
            );
        }
    }

    #[test]
    fn row_with_only_greedy_children_uses_parent_max_cross() {
        // Edge case: every child is cross-greedy, so there is no
        // natural-cross signal. CSS-flexbox default: take the parent's
        // full cross. Row of 6 cols × 4 rows, two fills, both must
        // paint all 4 rows.
        let desc = WidgetDescription::Row {
            gap: 0,
            key: None,
            children: vec![expanded(fill("a", None), 1), expanded(fill("b", None), 1)],
        };
        let buf = paint_root(desc, 6, 4);
        for row in 0..4 {
            assert_eq!(cell_at(&buf, row, 0), "a", "row {row} col 0");
            assert_eq!(cell_at(&buf, row, 5), "b", "row {row} col 5");
        }
    }

    #[test]
    fn row_with_no_greedy_children_unchanged_from_phase_2_behavior() {
        // Regression guard: a row of plain text widgets must still
        // collapse to the height of the tallest child, not the parent's
        // max cross. This is the pre-cross-stretch behavior — the new
        // pass 3 is a no-op when no child is cross-greedy.
        let desc = WidgetDescription::Row {
            gap: 0,
            key: None,
            children: vec![text("a"), text("bb"), text("ccc")],
        };
        let mut rec = Reconciler::new();
        rec.reconcile(desc);
        let root = rec.root.as_mut().unwrap();
        let s = layout(root, Constraints::loose(20, 10));
        // Sum of widths is 6; cross is 1 (all text is 1-tall). Loose
        // bounds → row reports its content size, not the parent's max.
        assert_eq!(
            s,
            Size {
                width: 6,
                height: 1
            }
        );
    }

    #[test]
    fn column_with_expanded_grows_vertically() {
        // Column of 10 rows: text(1 row) + expanded(text "") → expanded = 9.
        let desc = WidgetDescription::Column {
            gap: 0,
            key: None,
            selectable: false,
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
    fn wrap_word_falls_back_to_char_wrap_for_oversized_word_at_line_start() {
        // A single word longer than the line should char-wrap instead of
        // overflowing — this case the old implementation already handled.
        let rows = wrap_text("abcdefghij", 4, WrapMode::Word);
        assert_eq!(
            rows,
            vec!["abcd".to_string(), "efgh".to_string(), "ij".to_string(),]
        );
    }

    #[test]
    fn wrap_word_char_wraps_oversized_word_after_mid_line_flush() {
        // Regression: a normal-sized word fills part of the line, then a
        // word longer than the limit follows. The previous implementation
        // flushed the line, then extended `current` with the long word
        // unchanged — producing a row that overflowed `limit`. The fix
        // reorders the checks so the flushed-to-empty state goes through
        // the char-wrap fallback.
        let rows = wrap_text("hi superlongword", 6, WrapMode::Word);
        assert_eq!(
            rows,
            vec![
                "hi ".to_string(),
                "superl".to_string(),
                "ongwor".to_string(),
                "d".to_string(),
            ]
        );
        // No row exceeds the 6-char limit.
        for row in &rows {
            assert!(row.chars().count() <= 6, "row {row:?} exceeded limit");
        }
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

    /// Stacking a styled `tui.text` over a wider painted background must
    /// occlude the cells the overlay's rect covers — not just the cells
    /// the overlay's glyphs touch. This is the toast-popup pattern: a
    /// short label inside a fixed-width anchored rect should hide
    /// whatever was painted underneath, both within the glyph run and
    /// the trailing blank columns of its rect.
    #[test]
    fn styled_text_occludes_full_rect_in_stack_overlay() {
        use crate::desc::Color;

        let bg = Color::Rgb(0x2a, 0x33, 0x40);
        let fg = Color::Rgb(0x88, 0xcc, 0xff);
        let toast_style = Style {
            fg: Some(fg),
            bg: Some(bg),
            ..Style::default()
        };

        // Background: 12 'X' chars across one row.
        // Overlay: anchored top-left, 8 cells wide, with text "hi"
        // (2 chars). Cells 2..8 within the overlay rect are trailing
        // blanks the leaf must paint with the toast bg.
        let overlay = WidgetDescription::Anchored {
            anchor: Anchor::TopLeft,
            offset_x: 0,
            offset_y: 0,
            width: Dimension::Cells(8),
            height: Dimension::Cells(1),
            child: Box::new(WidgetDescription::Text {
                content: "hi".into(),
                style: Some(toast_style),
                wrap: WrapMode::None,
                key: None,
            }),
            key: None,
        };
        let desc = WidgetDescription::Stack {
            key: None,
            children: vec![text("XXXXXXXXXXXX"), overlay],
        };
        let buf = paint_root(desc, 12, 1);

        // The text glyphs land in cols 0-1.
        assert_eq!(cell_at(&buf, 0, 0), "h");
        assert_eq!(cell_at(&buf, 0, 1), "i");
        // Cols 2..8 are the trailing blanks of the overlay rect — they
        // must carry the toast style (bg occludes), and the underlying
        // 'X' chars must be gone.
        for col in 2..8 {
            let cell = &buf.lines[0].cells[col];
            assert_eq!(
                cell.text, " ",
                "col {col} should be a blank, not bg-text leak"
            );
            assert_eq!(
                cell.style.bg,
                Some(bg),
                "col {col} must inherit toast bg so the overlay occludes underneath"
            );
        }
        // Cols 8..12 are outside the overlay rect — the background
        // 'XXXX' is preserved (the overlay only owns its own rect).
        for col in 8..12 {
            assert_eq!(
                cell_at(&buf, 0, col),
                "X",
                "col {col} (outside overlay rect) keeps background"
            );
        }
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
            h1: Some(crate::desc::HeadingStyle {
                style: h1,
                prefix: None,
            }),
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

    // ── Scrollable tests ────────────────────────────────────────────────

    fn scrollable(child: WidgetDescription, key: &str) -> WidgetDescription {
        WidgetDescription::Scrollable {
            key: Some(key.into()),
            child: Box::new(child),
            stick_to: None,
            on_scroll: None,
            scrollbar: ScrollbarMode::Auto,
            style: None,
            selectable: false,
            virtual_content_height: None,
        }
    }

    fn scrollable_with(
        child: WidgetDescription,
        key: &str,
        stick_to: Option<StickTo>,
        scrollbar: ScrollbarMode,
    ) -> WidgetDescription {
        WidgetDescription::Scrollable {
            key: Some(key.into()),
            child: Box::new(child),
            stick_to,
            on_scroll: None,
            scrollbar,
            style: None,
            selectable: false,
            virtual_content_height: None,
        }
    }

    fn column(children: Vec<WidgetDescription>, gap: u16) -> WidgetDescription {
        WidgetDescription::Column {
            children,
            gap,
            key: None,
            selectable: false,
        }
    }

    fn long_column(n: u16) -> WidgetDescription {
        let kids: Vec<_> = (0..n).map(|i| text(&format!("line-{i}"))).collect();
        column(kids, 0)
    }

    fn scrollable_state(rec: &Reconciler) -> ScrollableState {
        let root = rec.root.as_ref().expect("root");
        match &root.state {
            InstanceState::Scrollable(s) => s.clone(),
            _ => panic!("expected scrollable root"),
        }
    }

    #[test]
    fn scrollable_layout_stores_content_and_viewport_heights() {
        // 12 rows of content into a 5-row viewport.
        let mut rec = Reconciler::new();
        rec.reconcile(scrollable(long_column(12), "transcript"));
        let mut buf = FrameBuffer::new(20, 5);
        layout_and_paint(rec.root.as_mut().unwrap(), 20, 5, &mut buf);
        let st = scrollable_state(&rec);
        assert_eq!(st.content_height, 12);
        assert_eq!(st.viewport_height, 5);
        assert_eq!(st.scroll_y_max(), 7);
    }

    #[test]
    fn scrollable_no_overflow_does_not_show_scrollbar() {
        // 3 rows of content into a 5-row viewport — no bar in `auto`.
        let desc = scrollable_with(long_column(3), "k", None, ScrollbarMode::Auto);
        let mut rec = Reconciler::new();
        rec.reconcile(desc);
        let mut buf = FrameBuffer::new(10, 5);
        layout_and_paint(rec.root.as_mut().unwrap(), 10, 5, &mut buf);
        // Last column should be blank (no track painted).
        for r in 0..5 {
            assert_eq!(cell_at(&buf, r, 9), " ", "row {r} last col is blank");
        }
    }

    #[test]
    fn scrollable_overflow_paints_track_and_thumb() {
        let desc = scrollable_with(long_column(20), "k", None, ScrollbarMode::Auto);
        let mut rec = Reconciler::new();
        rec.reconcile(desc);
        let mut buf = FrameBuffer::new(10, 5);
        layout_and_paint(rec.root.as_mut().unwrap(), 10, 5, &mut buf);
        // Last column at row 0 is the thumb (since scroll_y = 0). The
        // remaining rows show the track glyph.
        let bar_col = 9;
        let r0 = cell_at(&buf, 0, bar_col);
        assert_eq!(r0, "█", "thumb at top of track");
        // Some row below has a track glyph.
        let mut found_track = false;
        for r in 1..5 {
            if cell_at(&buf, r, bar_col) == "│" {
                found_track = true;
                break;
            }
        }
        assert!(found_track, "track glyph should appear below thumb");
    }

    #[test]
    fn scrollable_always_mode_paints_bar_even_without_overflow() {
        let desc = scrollable_with(long_column(2), "k", None, ScrollbarMode::Always);
        let mut rec = Reconciler::new();
        rec.reconcile(desc);
        let mut buf = FrameBuffer::new(10, 5);
        layout_and_paint(rec.root.as_mut().unwrap(), 10, 5, &mut buf);
        // Track must be present; with content_h <= viewport_h the
        // thumb fills the gutter (max == 0, scroll_room == 0, thumb_h
        // == viewport_h).
        for r in 0..5 {
            let g = cell_at(&buf, r, 9);
            assert!(g == "█" || g == "│", "row {r} gutter glyph = {g:?}");
        }
    }

    #[test]
    fn scrollable_never_mode_suppresses_bar_even_with_overflow() {
        let desc = scrollable_with(long_column(20), "k", None, ScrollbarMode::Never);
        let mut rec = Reconciler::new();
        rec.reconcile(desc);
        let mut buf = FrameBuffer::new(10, 5);
        layout_and_paint(rec.root.as_mut().unwrap(), 10, 5, &mut buf);
        // No bar painted on the last column.
        for r in 0..5 {
            let g = cell_at(&buf, r, 9);
            assert_ne!(g, "█", "row {r} should not show thumb");
            assert_ne!(g, "│", "row {r} should not show track");
        }
    }

    #[test]
    fn scrollable_clamps_scroll_y_after_layout_when_content_shrinks() {
        // Start with overflow: scroll_y = 5.
        let mut rec = Reconciler::new();
        rec.reconcile(scrollable(long_column(20), "k"));
        let mut buf = FrameBuffer::new(10, 5);
        layout_and_paint(rec.root.as_mut().unwrap(), 10, 5, &mut buf);
        // Move the offset partway down.
        if let Some(root) = rec.root.as_mut() {
            if let InstanceState::Scrollable(s) = &mut root.state {
                s.scroll_y = 12;
                s.was_at_end = false;
                s.was_at_start = false;
            }
        }
        // Now shrink content to 4 rows — scroll_y should clamp to 0.
        rec.reconcile(scrollable(long_column(4), "k"));
        let mut buf = FrameBuffer::new(10, 5);
        layout_and_paint(rec.root.as_mut().unwrap(), 10, 5, &mut buf);
        let st = scrollable_state(&rec);
        assert_eq!(st.scroll_y, 0, "scroll_y must clamp after content shrinks");
    }

    #[test]
    fn stick_to_end_pins_to_bottom_on_first_paint() {
        // 20 rows of content, viewport = 5. With stick_to = end, scroll_y
        // should land at scroll_y_max (15) without any user input.
        let desc = scrollable_with(
            long_column(20),
            "k",
            Some(StickTo::End),
            ScrollbarMode::Auto,
        );
        let mut rec = Reconciler::new();
        rec.reconcile(desc);
        let mut buf = FrameBuffer::new(10, 5);
        layout_and_paint(rec.root.as_mut().unwrap(), 10, 5, &mut buf);
        let st = scrollable_state(&rec);
        assert_eq!(st.scroll_y, 15);
        assert!(st.was_at_end);
    }

    #[test]
    fn stick_to_end_follows_growing_content_when_at_bottom() {
        let desc = scrollable_with(
            long_column(10),
            "k",
            Some(StickTo::End),
            ScrollbarMode::Auto,
        );
        let mut rec = Reconciler::new();
        rec.reconcile(desc);
        let mut buf = FrameBuffer::new(10, 5);
        layout_and_paint(rec.root.as_mut().unwrap(), 10, 5, &mut buf);
        let st0 = scrollable_state(&rec);
        assert_eq!(st0.scroll_y, 5, "first paint pins at bottom");

        // Content grows to 30 rows; user has not moved.
        let desc2 = scrollable_with(
            long_column(30),
            "k",
            Some(StickTo::End),
            ScrollbarMode::Auto,
        );
        rec.reconcile(desc2);
        let mut buf = FrameBuffer::new(10, 5);
        layout_and_paint(rec.root.as_mut().unwrap(), 10, 5, &mut buf);
        let st1 = scrollable_state(&rec);
        assert_eq!(st1.scroll_y, 25, "follows the new bottom");
    }

    #[test]
    fn stick_to_end_does_not_drag_user_who_scrolled_away_from_bottom() {
        let desc = scrollable_with(
            long_column(20),
            "k",
            Some(StickTo::End),
            ScrollbarMode::Auto,
        );
        let mut rec = Reconciler::new();
        rec.reconcile(desc);
        let mut buf = FrameBuffer::new(10, 5);
        layout_and_paint(rec.root.as_mut().unwrap(), 10, 5, &mut buf);
        // User scrolled up away from the bottom.
        if let Some(root) = rec.root.as_mut() {
            if let InstanceState::Scrollable(s) = &mut root.state {
                s.scroll_y = 5;
                s.was_at_end = false;
            }
        }
        // Content grows; scroll_y should stay where it was, not jump back.
        let desc2 = scrollable_with(
            long_column(30),
            "k",
            Some(StickTo::End),
            ScrollbarMode::Auto,
        );
        rec.reconcile(desc2);
        let mut buf = FrameBuffer::new(10, 5);
        layout_and_paint(rec.root.as_mut().unwrap(), 10, 5, &mut buf);
        let st = scrollable_state(&rec);
        assert_eq!(st.scroll_y, 5, "user position preserved");
        assert!(!st.was_at_end);
    }

    /// Bug 4 (wide chars) regression: a CJK / fullwidth / emoji glyph is
    /// width-2 per `unicode-width`. Each such char should occupy
    /// **exactly two cells** — one for the glyph, one blank for the
    /// spillover — so total horizontal advance equals the glyph's
    /// display width and `string_width` is honoured. The earlier shape
    /// did `col += w` for the glyph cell AND advanced once more per
    /// blank, double-counting the spillover and shifting every char
    /// after a wide char one extra cell to the right per occurrence
    /// (visible as e.g. `你 好 世 界` rendering with double-spaced
    /// glyphs and trailing chars wrapping early).
    #[test]
    fn wide_char_writes_one_glyph_plus_one_blank_no_double_advance() {
        // `你好` is two East-Asian Wide chars (width 2 each = 4 cells
        // total). After the run, an ASCII `x` should land at col 4.
        let buf = paint_root(text("你好x"), 8, 1);
        assert_eq!(cell_at(&buf, 0, 0), "你", "glyph at col 0");
        assert_eq!(cell_at(&buf, 0, 1), " ", "spillover blank for `你`");
        assert_eq!(cell_at(&buf, 0, 2), "好", "next glyph at col 2");
        assert_eq!(cell_at(&buf, 0, 3), " ", "spillover blank for `好`");
        assert_eq!(
            cell_at(&buf, 0, 4),
            "x",
            "ASCII follows immediately, no extra gap"
        );
    }

    /// Same contract for the styled-rows path (markdown, spans). The
    /// Lua composition uses both `tui.text` (plain) and `tui.markdown`
    /// (styled) for transcript prose, so both writers need the
    /// invariant pinned.
    #[test]
    fn wide_char_writes_through_styled_painter_without_double_advance() {
        // Drive paint_styled_rows directly via the spans primitive so
        // we exercise write_styled_row, not write_run. `Spans` painter
        // delegates to paint_styled_rows.
        let desc = WidgetDescription::Spans {
            spans: vec![Span {
                text: "你好x".into(),
                style: Style::default(),
            }],
            wrap: WrapMode::None,
            key: None,
        };
        let buf = paint_root(desc, 8, 1);
        assert_eq!(cell_at(&buf, 0, 0), "你");
        assert_eq!(cell_at(&buf, 0, 1), " ", "spillover blank for `你`");
        assert_eq!(cell_at(&buf, 0, 2), "好");
        assert_eq!(cell_at(&buf, 0, 3), " ", "spillover blank for `好`");
        assert_eq!(
            cell_at(&buf, 0, 4),
            "x",
            "ASCII follows at col 4 with no extra cell gap"
        );
    }

    #[test]
    fn scrollable_paints_window_at_scroll_y() {
        // 20 rows: "line-0" .. "line-19". Viewport = 3. Scroll to row 5.
        let desc = scrollable_with(long_column(20), "k", None, ScrollbarMode::Never);
        let mut rec = Reconciler::new();
        rec.reconcile(desc);
        let mut buf = FrameBuffer::new(10, 3);
        layout_and_paint(rec.root.as_mut().unwrap(), 10, 3, &mut buf);
        // First paint at scroll_y = 0.
        assert_eq!(cell_at(&buf, 0, 0), "l", "top of content shows line-0");

        if let Some(root) = rec.root.as_mut() {
            if let InstanceState::Scrollable(s) = &mut root.state {
                s.scroll_y = 5;
            }
        }
        let mut buf = FrameBuffer::new(10, 3);
        layout_and_paint(rec.root.as_mut().unwrap(), 10, 3, &mut buf);
        // Row 0 should now show line-5 ("line-5").
        let row0: String = buf.lines[0].cells.iter().map(|c| c.text.clone()).collect();
        assert!(
            row0.starts_with("line-5"),
            "expected line-5 at row 0, got {row0:?}"
        );
    }
}
