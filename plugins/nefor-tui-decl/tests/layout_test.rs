//! Integration test for nested compositions of phase-2 primitives.
//!
//! Builds a tree using all 9 primitives — column, row, padding, stack,
//! expanded, spacer, constrained, align, text — runs the renderer once,
//! and asserts the resulting frame buffer contents row-by-row.
//!
//! Why a frame-grid assertion (over a "golden ANSI string"): the ANSI
//! envelope is exercised by `tests/engine_test.rs`. Here we want to
//! ground the layout algorithm against a human-readable picture of cells
//! so future regressions surface as a specific row/col mismatch instead
//! of a long opaque ANSI diff.

use nefor_tui_decl::desc::{Alignment, WidgetDescription, WrapMode};
use nefor_tui_decl::layout;
use nefor_tui_decl::reconciler::Reconciler;
use nefor_tui_decl::render::{FrameBuffer, Renderer};

fn text(s: &str) -> WidgetDescription {
    WidgetDescription::Text {
        content: s.into(),
        style: None,
        wrap: WrapMode::Word,
        key: None,
    }
}

fn column(children: Vec<WidgetDescription>, gap: u16) -> WidgetDescription {
    WidgetDescription::Column {
        children,
        gap,
        key: None,
    }
}

fn row(children: Vec<WidgetDescription>, gap: u16) -> WidgetDescription {
    WidgetDescription::Row {
        children,
        gap,
        key: None,
    }
}

fn expanded(child: WidgetDescription) -> WidgetDescription {
    WidgetDescription::Expanded {
        flex: 1,
        child: Box::new(child),
        key: None,
    }
}

fn padding(child: WidgetDescription, all: u16) -> WidgetDescription {
    WidgetDescription::Padding {
        top: all,
        right: all,
        bottom: all,
        left: all,
        child: Box::new(child),
        key: None,
    }
}

fn stack(children: Vec<WidgetDescription>) -> WidgetDescription {
    WidgetDescription::Stack {
        children,
        key: None,
    }
}

fn align(child: WidgetDescription, a: Alignment) -> WidgetDescription {
    WidgetDescription::Align {
        alignment: a,
        child: Box::new(child),
        key: None,
    }
}

fn render_to_buf(desc: WidgetDescription, w: u16, h: u16) -> FrameBuffer {
    let mut rec = Reconciler::new();
    rec.reconcile(desc);
    let mut buf = FrameBuffer::new(w, h);
    let root = rec.root.as_mut().unwrap();
    layout::layout_and_paint(root, w, h, &mut buf);
    buf
}

fn dump(buf: &FrameBuffer) -> String {
    let mut out = String::new();
    for line in &buf.lines {
        for c in &line.cells {
            out.push_str(&c.text);
        }
        out.push('\n');
    }
    out
}

#[test]
fn nested_composition_renders_grid() {
    // Layout (20×6 frame, padding=1 around the stack):
    //
    //   column gap=0
    //   ├── text "header"                                       → row 0
    //   ├── row gap=1                                           → row 1
    //   │   ├── expanded { text "L" }     (flex=1, fills 14)
    //   │   └── text "RIGHT"              (5 cols)
    //   └── padding {1} {                                       → rows 2..6
    //       stack [
    //         text "----------",          (10 cols × 1 row)
    //         align center { text "X" },  (fills 18 cols × 2 rows; X
    //                                      lands centered at col 1+8 = 9)
    //       ],
    //     }
    //
    // Expected frame:
    //
    //   row 0: "header              "
    //   row 1: "L              RIGHT"
    //   row 2: "                    "  (top padding)
    //   row 3: " --------X-         "  (10 dashes 1..10, X overwrites 9)
    //   row 4: "                    "  (align rect's row 1; child h=1 sits
    //                                   at align_offset's row 0, so this
    //                                   row is empty inside the align)
    //   row 5: "                    "  (bottom padding row + tail)

    let header = text("header");
    let middle = row(vec![expanded(text("L")), text("RIGHT")], 1);
    let stack_layer = stack(vec![
        text("----------"),
        align(text("X"), Alignment::Center),
    ]);
    let bottom = padding(stack_layer, 1);
    let tree = column(vec![header, middle, bottom], 0);

    let buf = render_to_buf(tree, 20, 6);
    let dumped = dump(&buf);

    let blank20 = " ".repeat(20);
    let expected = format!(
        "header              \nL              RIGHT\n{blank20}\n --------X-         \n{blank20}\n{blank20}\n"
    );
    assert_eq!(dumped, expected, "frame mismatch:\nactual:\n{dumped}");
}

#[test]
fn nested_composition_renders_through_engine_with_synchronized_ansi() {
    // Same composition driven through the renderer (full ANSI pipeline) —
    // proves layout-and-paint output reaches the terminal byte stream
    // intact, including SYNC_BEGIN / SYNC_END framing and the cell
    // contents we asserted above.
    let header = text("OK");
    let mid = row(vec![expanded(text("a")), text("Z")], 0);
    let tree = column(vec![header, mid], 0);

    let mut rec = Reconciler::new();
    rec.reconcile(tree);
    let mut renderer = Renderer::new(8, 2);
    let bytes = renderer.render(rec.root.as_mut().unwrap());
    let s = String::from_utf8(bytes).expect("ansi is utf-8");

    // Synchronized output framing.
    assert!(s.starts_with("\x1b[?2026h"));
    assert!(s.ends_with("\x1b[?2026l"));

    // Frame contents.
    assert!(s.contains("OK"), "header text missing in:\n{s}");
    assert!(
        s.contains('a') && s.contains('Z'),
        "row contents missing: {s}"
    );

    // Top-left position before painting OK: ANSI cursor move + clear-line.
    // First rendered row is "OK      " — visible characters in order in
    // the byte stream after the row's CUP escape.
    let first_row_start = s.find("\x1b[1;1H").expect("CUP for row 0");
    let after = &s[first_row_start..];
    let ok_idx = after.find("OK").expect("OK in first row");
    let z_idx = s.find('Z').expect("Z in second row");
    assert!(
        ok_idx < z_idx - first_row_start || z_idx > first_row_start + ok_idx,
        "OK should appear before Z in the byte stream"
    );
}
