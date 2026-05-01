//! Right-side sidebar pane: stack of widgets rendered in the columns the
//! chat pane gives up. v1 ships exactly one widget — a live view of the DAG
//! runs the chat plugin is observing — but the assembly path is generic so
//! future widgets (queue depth, provider health, scratchpad…) plug in by
//! producing a [`SidebarWidget`] without touching the layout code.
//!
//! The chat pane and sidebar are adjacent column ranges in the same grid:
//! the renderer composes them side-by-side per row so popups, toasts, and
//! the input box never overlap the sidebar (the popup-rect math runs
//! against `chat_cols`, not `cols`).

use crate::render::{
    build_dag_panel_rows, node_row_spans_compact, run_id_prefix_spans, Span, HL_FOOTER,
    HL_STATUS_DIM,
};
use crate::state::{ChatState, DAG_PANEL_MAX_ROWS};
use crate::wrap::str_width;

/// One stacked widget in the sidebar. The renderer pads each row to
/// `sidebar_cols` and puts a blank separator row between adjacent widgets.
///
/// `title` is rendered as a single header row in `HL_FOOTER` (matching the
/// dim chrome of the chat statusline). `rows` are the widget body — each
/// inner `Vec<Span>` is one row of spans; rows that don't fill the width are
/// right-padded by the assembler.
pub(crate) struct SidebarWidget {
    pub title: String,
    pub rows: Vec<Vec<Span>>,
}

/// Build the rendered sidebar for the current state. Returns one row of spans
/// per `sidebar_rows`; missing trailing rows are padded with blanks so the
/// caller can blindly merge against the chat-pane row indices.
///
/// Truncation: when widgets together exceed `sidebar_rows`, the trailing
/// rows are clipped and a final `… +K more` overflow row is emitted.
pub(crate) fn build_sidebar(
    state: &ChatState,
    sidebar_cols: usize,
    sidebar_rows: usize,
    now_ms: u64,
) -> Vec<Vec<Span>> {
    if sidebar_cols == 0 || sidebar_rows == 0 {
        return Vec::new();
    }

    // Collect widgets. v1: just the DAG widget. Future widgets append here.
    let mut widgets: Vec<SidebarWidget> = Vec::new();
    if !state.dag_runs.is_empty() {
        widgets.push(build_dag_widget(state, sidebar_cols, now_ms));
    }

    // Empty sidebar: the pane is persistent now (see
    // ChatState::sidebar_width), so render a small dim hint at the top so
    // the empty space doesn't read as a layout bug, then pad with blanks.
    if widgets.is_empty() {
        let mut rows: Vec<Vec<Span>> = Vec::with_capacity(sidebar_rows);
        rows.push(empty_hint_row("(no active runs)", sidebar_cols));
        while rows.len() < sidebar_rows {
            rows.push(blank_row(sidebar_cols));
        }
        return rows;
    }

    // Stack widgets with a single blank separator row between them. Each
    // widget contributes `1 (title) + rows.len()` rows.
    let mut stacked: Vec<Vec<Span>> = Vec::new();
    for (i, w) in widgets.iter().enumerate() {
        if i > 0 {
            stacked.push(blank_row(sidebar_cols));
        }
        stacked.push(widget_title_row(&w.title, sidebar_cols));
        for row in &w.rows {
            stacked.push(pad_row(row.clone(), sidebar_cols));
        }
    }

    // Truncate / pad to `sidebar_rows`.
    if stacked.len() > sidebar_rows {
        let visible = sidebar_rows.saturating_sub(1);
        let omitted = stacked.len() - visible;
        let mut out: Vec<Vec<Span>> = stacked.into_iter().take(visible).collect();
        out.push(overflow_row(omitted, sidebar_cols));
        out
    } else {
        while stacked.len() < sidebar_rows {
            stacked.push(blank_row(sidebar_cols));
        }
        stacked
    }
}

/// Build the DAG widget — one row per tracked run header followed by per-node
/// rows. The narrow column budget drops the reasoner column when there isn't
/// room (under 36 cols) so `<glyph> <node_id> <elapsed>` always survives.
fn build_dag_widget(state: &ChatState, sidebar_cols: usize, now_ms: u64) -> SidebarWidget {
    // Title is shown in the widget header row by the assembler; its content
    // captures total in-flight runs at a glance.
    let title = format!(
        "Graph ({} run{})",
        state.dag_runs.len(),
        if state.dag_runs.len() == 1 { "" } else { "s" }
    );

    // Cap how many rows the DAG widget tries to fill so a runaway scheduler
    // doesn't crowd out other future widgets. Reuses `DAG_PANEL_MAX_ROWS`
    // (the historical inline-strip cap) so behaviour stays predictable.
    let max_rows = DAG_PANEL_MAX_ROWS as usize;

    // The DAG body uses two layouts depending on column slack:
    //   - wide (>= 36 cols): the legacy `build_dag_panel_rows` layout, which
    //     packs `<glyph> <id> <reasoner> <status> <elapsed>`. Same renderer
    //     the inline strip used.
    //   - narrow (< 36 cols): per-row compact layout — `<glyph> <id> <elapsed>`
    //     and a header row stripped to `─ <run-prefix> (M/N) ─`.
    let rows: Vec<Vec<Span>> = if sidebar_cols >= 36 {
        build_dag_panel_rows(&state.dag_runs, sidebar_cols, max_rows, now_ms)
    } else {
        build_dag_widget_compact(state, sidebar_cols, max_rows, now_ms)
    };

    SidebarWidget { title, rows }
}

/// Compact DAG body for narrow sidebars. One row per run-header, one row
/// per node showing `<glyph> <node_id> <elapsed>` in the available width.
fn build_dag_widget_compact(
    state: &ChatState,
    width: usize,
    max_rows: usize,
    now_ms: u64,
) -> Vec<Vec<Span>> {
    let mut out: Vec<Vec<Span>> = Vec::new();
    for run in state.dag_runs.values() {
        if out.len() >= max_rows {
            break;
        }
        // Header: `─ <prefix> (M/N) ─`, padded with dashes. Reuses the
        // run_id prefix helper to keep the prefix length consistent with the
        // wide layout.
        out.push(run_id_prefix_spans(run, width));
        for (node_id, node) in &run.nodes {
            if out.len() >= max_rows {
                break;
            }
            out.push(node_row_spans_compact(node_id, node, width, now_ms));
        }
    }
    if out.len() > max_rows {
        // Defensive — `max_rows` is an upper bound; the loops above respect it
        // already, but truncation here keeps the contract obvious.
        out.truncate(max_rows);
    }
    out
}

// ---- internal row builders ----

fn blank_row(width: usize) -> Vec<Span> {
    vec![Span::new(" ".repeat(width), 0)]
}

#[allow(dead_code)]
fn blank_sidebar_rows(width: usize, rows: usize) -> Vec<Vec<Span>> {
    (0..rows).map(|_| blank_row(width)).collect()
}

/// Single dim hint row used when the sidebar is visible but no widgets are
/// active. Keeps the pane reading as "intentional empty" rather than broken.
fn empty_hint_row(text: &str, width: usize) -> Vec<Span> {
    let mut row = String::with_capacity(width);
    row.push(' ');
    let used = 1 + str_width(text);
    if used <= width {
        row.push_str(text);
        for _ in used..width {
            row.push(' ');
        }
    } else {
        // Width is too narrow for the hint — fall back to all-blank.
        row.clear();
        for _ in 0..width {
            row.push(' ');
        }
    }
    vec![Span::new(row, HL_STATUS_DIM)]
}

/// Right-pad `spans` so total display width equals `width`. Used because
/// widgets emit row-spans without knowing their final column budget; the
/// assembler enforces a uniform width so the merge with the chat pane is
/// clean.
fn pad_row(mut spans: Vec<Span>, width: usize) -> Vec<Span> {
    let used: usize = spans.iter().map(|s| str_width(&s.text)).sum();
    if used >= width {
        return spans;
    }
    let pad = width - used;
    spans.push(Span::new(" ".repeat(pad), 0));
    spans
}

/// Render the widget's title row — single line in `HL_FOOTER` so it reads as
/// chrome rather than content. Truncates to `width` and right-pads with
/// dashes to give the title a separator-rule feel.
fn widget_title_row(title: &str, width: usize) -> Vec<Span> {
    let title = format!(" {title} ");
    let mut text = String::with_capacity(width);
    let mut used = 0usize;
    for ch in title.chars() {
        let cw = unicode_char_width(ch);
        if used + cw > width {
            break;
        }
        text.push(ch);
        used += cw;
    }
    while used < width {
        text.push('─');
        used += 1;
    }
    vec![Span::new(text, HL_FOOTER)]
}

/// Render the truncation marker that appears when widgets exceed `sidebar_rows`.
fn overflow_row(omitted: usize, width: usize) -> Vec<Span> {
    let text = format!("… +{omitted} more");
    let used: usize = str_width(&text);
    let pad = width.saturating_sub(used);
    let mut out = String::with_capacity(used + pad);
    out.push_str(&text);
    for _ in 0..pad {
        out.push(' ');
    }
    vec![Span::new(out, HL_STATUS_DIM)]
}

/// Local re-export of `crate::wrap::char_width` to avoid pulling the whole
/// module across just for the title row builder.
fn unicode_char_width(ch: char) -> usize {
    crate::wrap::char_width(ch)
}
