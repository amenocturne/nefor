//! Snapshot integration tests.
//!
//! Demonstrates the pattern for asserting exact visual output of a
//! Lua-driven scenario at a fixed terminal size:
//!
//! 1. Build an [`Engine`] with the desired `(width, height)`.
//! 2. Load a scenario via `engine.load_scenario(lua_source)`.
//! 3. Drive the engine with `handle_key` / `dispatch_msg` /
//!    `handle_resize` etc.
//! 4. Call `render_if_dirty()` to paint the frame.
//! 5. Read [`Engine::snapshot`] for plain text or
//!    [`Engine::snapshot_styled`] for human-readable style markers.
//!
//! The snapshot returns the framebuffer's exact contents — every row is
//! `width` cells wide (trailing spaces preserved), rows joined by `\n`.

use nefor_tui::engine::Engine;
use nefor_tui::input::KeyMessage;

fn key(name: &str) -> KeyMessage {
    KeyMessage {
        name: name.into(),
        mods: vec![],
    }
}

fn paint(engine: &mut Engine) {
    let _ = engine.render_if_dirty().expect("render");
}

/// Bare scenario producing a single line of text — the simplest snapshot
/// shape. Asserts the snapshot is exactly `width × height` characters with
/// the text positioned in the top-left.
#[test]
fn snapshot_bare_text_scenario() {
    const SCENARIO: &str = r#"
        tui.start {
          initial_state = {},
          view = function(_) return tui.text { content = "hello" } end,
          update = function(_, s) return s, {} end,
        }
    "#;
    let mut engine = Engine::new(10, 2).expect("engine");
    engine.load_scenario(SCENARIO).expect("scenario");
    paint(&mut engine);
    // 10 cols × 2 rows. Row 0 = "hello" + 5 trailing spaces. Row 1 = blank.
    assert_eq!(engine.snapshot(), "hello     \n          ");
}

/// Bordered-box composition (the same `bordered_box` helper chat.lua
/// uses for the input field). Asserts the corners and rules land where
/// expected.
#[test]
fn snapshot_bordered_box_corners() {
    const SCENARIO: &str = r#"
        local function rule_row(left, right)
          return tui.constrained {
            max_height = 1,
            child = tui.row {
              gap = 0,
              children = {
                tui.text { content = left,  wrap = "none" },
                tui.expanded { child = tui.fill { char = "─" } },
                tui.text { content = right, wrap = "none" },
              },
            },
          }
        end
        local function box(child)
          local body = tui.row {
            gap = 0,
            children = {
              tui.text { content = "│ ", wrap = "none" },
              tui.expanded { child = child },
              tui.text { content = " │", wrap = "none" },
            },
          }
          return tui.column {
            gap = 0,
            children = { rule_row("╭", "╮"), body, rule_row("╰", "╯") },
          }
        end
        tui.start {
          initial_state = {},
          view = function(_) return box(tui.text { content = "hi" }) end,
          update = function(_, s) return s, {} end,
        }
    "#;
    let mut engine = Engine::new(10, 4).expect("engine");
    engine.load_scenario(SCENARIO).expect("scenario");
    paint(&mut engine);
    let snap = engine.snapshot();
    let rows: Vec<&str> = snap.lines().collect();
    // Row 0: top rule.    Row 1: body.    Row 2: bottom rule.    Row 3: blank.
    assert!(rows[0].starts_with('╭'), "top-left corner: {:?}", rows[0]);
    assert!(rows[0].ends_with('╮'), "top-right corner: {:?}", rows[0]);
    assert!(rows[1].starts_with("│ "), "body left chrome: {:?}", rows[1]);
    assert!(rows[1].ends_with(" │"), "body right chrome: {:?}", rows[1]);
    assert!(rows[1].contains("hi"), "body content: {:?}", rows[1]);
    assert!(rows[2].starts_with('╰'), "bot-left corner: {:?}", rows[2]);
    assert!(rows[2].ends_with('╯'), "bot-right corner: {:?}", rows[2]);
}

/// State-update snapshot — drive a key event, re-render, snapshot again.
/// Confirms the pattern works for behavior-tied assertions, not just
/// initial frames.
#[test]
fn snapshot_after_key_dispatch_reflects_state_change() {
    const SCENARIO: &str = r#"
        tui.start {
          initial_state = { count = 0 },
          view = function(s)
            return tui.text { content = "count: " .. tostring(s.count) }
          end,
          update = function(msg, s)
            if msg.kind == "key.space" then return { count = s.count + 1 }, {} end
            return s, {}
          end,
        }
    "#;
    let mut engine = Engine::new(12, 1).expect("engine");
    engine.load_scenario(SCENARIO).expect("scenario");
    paint(&mut engine);
    assert_eq!(engine.snapshot(), "count: 0    ");

    engine.handle_key(key("space")).expect("space");
    paint(&mut engine);
    assert_eq!(engine.snapshot(), "count: 1    ");

    engine.handle_key(key("space")).expect("space");
    paint(&mut engine);
    assert_eq!(engine.snapshot(), "count: 2    ");
}

/// `snapshot_styled` annotates style regions inline with `[bold]...[/bold]`
/// markers. Useful for golden-file tests where diffs stay readable.
#[test]
fn snapshot_styled_marks_bold_regions() {
    const SCENARIO: &str = r#"
        tui.start {
          initial_state = {},
          view = function(_)
            return tui.text {
              content = "BOLD",
              style = { bold = true },
              wrap = "none",
            }
          end,
          update = function(_, s) return s, {} end,
        }
    "#;
    let mut engine = Engine::new(6, 1).expect("engine");
    engine.load_scenario(SCENARIO).expect("scenario");
    paint(&mut engine);
    // The 4-char bold word, then 2 trailing spaces (unstyled).
    assert_eq!(engine.snapshot_styled(), "[bold]BOLD[/bold]  ");
}
