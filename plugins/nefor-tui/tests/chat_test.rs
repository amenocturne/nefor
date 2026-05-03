//! Phase-6 integration test for the chat surface as a Lua composition.
//!
//! Loads `starter/chat.lua` into the in-process engine and verifies the
//! must-have wire path: a `chat.stream.delta` from a peer lands in the
//! transcript, an `input.submit` produces a `chat.input.submit` egress
//! envelope, and `/quit` exits.
//!
//! In-process per the same pattern as `engine_test.rs` — no spawned
//! subprocess, no /dev/tty — so the test stays fast and CI-portable.

use std::path::PathBuf;
use std::time::Duration;

use nefor_tui::engine::Engine;
use nefor_tui::input::KeyMessage;
use serde_json::{json, Map as JsonMap, Value as JsonValue};

fn chat_lua_source() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("repo root")
        .join("starter")
        .join("chat.lua");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {:?}: {e}", path))
}

fn render_str(engine: &mut Engine) -> String {
    match engine.render_if_dirty().expect("render") {
        Some(bytes) => String::from_utf8(bytes).expect("ansi is utf-8"),
        // Render-was-clean is fine for assertions that only care about
        // egress / state shape; the prior frame is on the wire already.
        None => String::new(),
    }
}

fn dispatch_event(engine: &mut Engine, body: JsonValue) {
    let map: JsonMap<String, JsonValue> = body.as_object().expect("event body").clone();
    engine.dispatch_envelope_body(&map).expect("dispatch event");
}

fn key(name: &str) -> KeyMessage {
    KeyMessage {
        name: name.into(),
        mods: vec![],
    }
}

#[test]
fn chat_lua_loads_and_renders_initial_frame() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let out = render_str(&mut engine);
    // Pre-stats statusline shows the dim "Start chatting" placeholder
    // (legacy-spec parity, not the old MVP "model: —" format).
    assert!(
        out.contains("Start chatting to see stats"),
        "pre-stats placeholder missing: {out:?}"
    );
    // Cursor inversion at row start splits the placeholder's first
    // character from the rest, so match a substring that's contiguous
    // after the cursor cell.
    assert!(
        out.contains("ype a message"),
        "input placeholder missing: {out:?}"
    );
    // Drain — the script doesn't emit anything at boot.
    assert!(engine.take_emit_queue().is_empty());
}

#[test]
fn streaming_delta_appends_to_transcript() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.stream.delta", "text": "hello " }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.stream.delta", "text": "world" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.stream.end", "model": "qwen-test", "duration_ms": 42 }),
    );

    let out = render_str(&mut engine);
    assert!(
        out.contains("hello world"),
        "concatenated deltas missing from transcript: {out:?}"
    );
    // Per legacy spec, assistant entries have NO role label — the visual
    // cue is the absence of the user block's left bar. The per-turn
    // footer marker `▣` + model name is the assistant signature.
    assert!(
        out.contains('▣') && out.contains("qwen-test"),
        "per-turn footer (▣ <model>) missing after stream end: {out:?}"
    );
}

#[test]
fn typing_and_enter_emits_chat_input_submit() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Type "hi" — the text_input is focused by default.
    engine.handle_key(key("h")).expect("h");
    engine.handle_key(key("i")).expect("i");
    // Drain emits accumulated from on_change side-effects (none expected).
    let _ = engine.take_emit_queue();

    engine.handle_key(key("enter")).expect("enter");
    let emits = engine.take_emit_queue();
    assert_eq!(emits.len(), 1, "submit should produce exactly one emit");
    let (target_hint, body) = &emits[0];
    assert_eq!(target_hint.as_deref(), Some("engine"));
    assert_eq!(
        body.get("kind").and_then(|v| v.as_str()),
        Some("chat.input.submit")
    );
    assert_eq!(body.get("text").and_then(|v| v.as_str()), Some("hi"));

    // Transcript should now show the user's echo entry.
    let out = render_str(&mut engine);
    assert!(out.contains("hi"), "user echo missing: {out:?}");
}

#[test]
fn input_field_renders_full_width_rounded_border() {
    // Per legacy spec section 7: input box has `╭─╮ │ ╰─╯` chrome in
    // HL_USER. The bordered_box helper composes corners + tui.fill for
    // the rules + side bars around the text_input.
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let out = render_str(&mut engine);
    for glyph in ['╭', '╮', '╰', '╯', '─'] {
        assert!(
            out.contains(glyph),
            "input border missing glyph {glyph:?}: {out:?}"
        );
    }
}

#[test]
fn user_message_renders_full_width_rounded_border() {
    // User entries also use `╭─╮ │ ╰─╯` per spec section 5.
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.message.append", "role": "user", "text": "hello" }),
    );
    let out = render_str(&mut engine);
    // The body text must land between the rules.
    assert!(out.contains("hello"), "user body missing: {out:?}");
    // All four corners present (the input field gives us a full set
    // already; here we additionally assert the user block runs end to
    // end with a multi-cell horizontal rule).
    for corner in ['╭', '╮', '╰', '╯'] {
        assert!(
            out.contains(corner),
            "user block missing corner {corner:?}: {out:?}"
        );
    }
}

#[test]
fn slash_quit_requests_exit() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    for ch in "/quit".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    assert!(engine.exit_requested(), "exit not requested after /quit");
}

#[test]
fn ctrl_c_exits_even_when_input_is_focused() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Type some text so the focused input is non-empty — the realistic
    // case where the user is mid-message and wants out.
    engine.handle_key(key("h")).expect("h");
    engine.handle_key(key("i")).expect("hi");

    // Send the realistic Ctrl+C shape (name="c", mods=["ctrl"]) — what
    // crossterm produces. Pre-fix the router absorbed this as no-op
    // copy; post-fix it bubbles to Lua as `key.ctrl_c`.
    engine
        .handle_key(KeyMessage {
            name: "c".into(),
            mods: vec!["ctrl"],
        })
        .expect("ctrl+c");
    assert!(
        engine.exit_requested(),
        "Ctrl+C must exit even with a focused text_input",
    );
}

#[test]
fn ctrl_d_exits() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Ctrl+D bubbles unconditionally (the editing-key classifier never
    // claimed it). Verify the chat surface wires it to exit.
    engine
        .handle_key(KeyMessage {
            name: "d".into(),
            mods: vec!["ctrl"],
        })
        .expect("ctrl+d");
    assert!(engine.exit_requested(), "Ctrl+D must exit");
}

#[test]
fn slash_new_clears_transcript_and_emits_chat_reset() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Seed a couple of entries first.
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.message.append", "role": "user", "text": "previous" }),
    );
    let _ = render_str(&mut engine);

    // Type "/new" + Enter.
    for ch in "/new".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    let _ = engine.take_emit_queue();
    engine.handle_key(key("enter")).expect("enter");
    let emits = engine.take_emit_queue();
    assert_eq!(emits.len(), 1, "expected one chat.reset egress");
    assert_eq!(
        emits[0].1.get("kind").and_then(|v| v.as_str()),
        Some("chat.reset")
    );

    let out = render_str(&mut engine);
    assert!(
        !out.contains("previous"),
        "transcript should be cleared after /new: {out:?}"
    );
}

// ── DAG panel (phase 7) ───────────────────────────────────────────────
//
// These exercise the sidebar that subscribes to `reasoner-graph` plugin
// lifecycle events (`graph.run_started`, `graph.node_dispatched`,
// `graph.node_result`, `graph.run_complete`). The panel is hidden by
// default; Ctrl+B toggles it on. Linger handling is pure-update, so a
// completed run drops on the next event after `DAG_LINGER_MS` of engine
// time has passed — `Engine::advance_time` plus a synthetic event drives
// the prune deterministically without sleeping.

fn toggle_sidebar(engine: &mut Engine) {
    engine.handle_key(key("ctrl_b")).expect("ctrl_b");
}

#[test]
fn graph_run_started_creates_a_dag_panel_row() {
    let mut engine = Engine::new(120, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);
    toggle_sidebar(&mut engine);

    dispatch_event(
        &mut engine,
        json!({
            "kind": "graph.run_started",
            "run_id": "run-aaaaaaaa",
            "total_nodes": 3,
        }),
    );

    let out = render_str(&mut engine);
    // Header shows the abbreviated run id and (done/total) counter.
    assert!(
        out.contains("DAG run-aaaa"),
        "dag header missing for run-aaaaaaaa: {out:?}"
    );
    assert!(
        out.contains("(0/3)"),
        "dag counter missing 0/3 for fresh run: {out:?}"
    );
}

#[test]
fn graph_node_dispatched_then_result_updates_status_glyph() {
    let mut engine = Engine::new(120, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);
    toggle_sidebar(&mut engine);

    dispatch_event(
        &mut engine,
        json!({
            "kind": "graph.run_started",
            "run_id": "run-bbbbbbbb",
            "total_nodes": 2,
        }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "graph.node_dispatched",
            "run_id": "run-bbbbbbbb",
            "node_id": "summarise",
            "reasoner": "ollama",
        }),
    );

    // After dispatch the node is "running" — the panel should render
    // the running glyph (●) for it.
    let out = render_str(&mut engine);
    assert!(
        out.contains("summarise"),
        "node id missing from panel: {out:?}"
    );
    assert!(
        out.contains('●'),
        "running glyph (●) missing for dispatched node: {out:?}"
    );

    // Now flip the node to `done` via a node_result with `output`.
    dispatch_event(
        &mut engine,
        json!({
            "kind": "graph.node_result",
            "run_id": "run-bbbbbbbb",
            "node_id": "summarise",
            "output": "summary text",
        }),
    );
    let out = render_str(&mut engine);
    assert!(
        out.contains('✓'),
        "done glyph (✓) missing after node_result: {out:?}"
    );
    // The (done/total) counter should now read 1/2.
    assert!(
        out.contains("(1/2)"),
        "counter should read 1/2 after one node done: {out:?}"
    );
}

#[test]
fn graph_run_complete_removes_run_after_linger_window() {
    let mut engine = Engine::new(120, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);
    toggle_sidebar(&mut engine);

    // Stand up a completed run: started, dispatched, result, complete.
    dispatch_event(
        &mut engine,
        json!({
            "kind": "graph.run_started",
            "run_id": "run-cccccccc",
            "total_nodes": 1,
        }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "graph.node_dispatched",
            "run_id": "run-cccccccc",
            "node_id": "n1",
            "reasoner": "ollama",
        }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "graph.node_result",
            "run_id": "run-cccccccc",
            "node_id": "n1",
            "output": "ok",
        }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "graph.run_complete",
            "run_id": "run-cccccccc",
            "status": "success",
            "results": { "n1": { "output": "ok" } },
        }),
    );

    // The run is still within its linger window — header is visible.
    let out = render_str(&mut engine);
    assert!(
        out.contains("DAG run-cccc"),
        "completed run should linger initially: {out:?}"
    );

    // Advance past the 2s linger and dispatch a no-op event so update
    // runs and prunes the stale entry. (The pure-update prune fires on
    // every dispatch — we use any event with a kind chat.lua handles
    // and that doesn't touch dag_runs; chat.session.stats fits.)
    engine.advance_time(Duration::from_millis(3000));
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.session.stats", "turns": 1 }),
    );
    let out = render_str(&mut engine);
    assert!(
        !out.contains("DAG run-cccc"),
        "completed run should be pruned past linger window: {out:?}"
    );
    // The empty-state hint should now show in the sidebar.
    assert!(
        out.contains("(no active runs)"),
        "empty-state hint missing after prune: {out:?}"
    );
}

#[test]
fn chat_session_stats_updates_statusline() {
    let mut engine = Engine::new(120, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.session.stats",
            "model": "qwen-test",
            "prompt_tokens": 11,
            "completion_tokens": 7,
            "cost_usd": 0.0042,
            "turns": 1,
            "duration_ms": 1500,
        }),
    );

    let out = render_str(&mut engine);
    // Spec section 4 segment order: model · ctx · cost · turns · dur · speed.
    // "qwen-test" doesn't carry a `claude-` prefix so the stripped form
    // is identical.
    assert!(
        out.contains("qwen-test"),
        "statusline missing model: {out:?}"
    );
    assert!(out.contains("$0.00"), "cost segment missing: {out:?}");
    assert!(out.contains("1 turns"), "turns segment missing: {out:?}");
    assert!(out.contains("1s"), "duration segment missing: {out:?}");
}

#[test]
fn ctrl_o_toggles_expanded_details() {
    let mut engine = Engine::new(120, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Seed a tool call (running — no output yet) so we have a tool entry
    // to compare collapsed/expanded against.
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.tool.start",
            "id": "t1",
            "name": "Bash",
            "input": "ls -la /tmp",
        }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.tool.end",
            "id": "t1",
            "output": "drwxr-xr-x 4 root  root  128 May  2 12:00 .",
        }),
    );

    // Collapsed: header glyph is `▸`, no `input:` / `output:` blocks.
    let out = render_str(&mut engine);
    assert!(
        out.contains('▸') && out.contains("Bash"),
        "collapsed tool header missing: {out:?}"
    );
    assert!(
        !out.contains("output:"),
        "collapsed view should not show 'output:' label: {out:?}"
    );

    // Toggle to expanded via Ctrl+O.
    engine.handle_key(key("ctrl_o")).expect("ctrl_o");
    let out = render_str(&mut engine);
    assert!(
        out.contains('▼'),
        "expanded glyph (▼) missing after Ctrl+O: {out:?}"
    );
    assert!(
        out.contains("output:"),
        "expanded view missing 'output:' label: {out:?}"
    );

    // Toggle back: collapsed again.
    engine.handle_key(key("ctrl_o")).expect("ctrl_o again");
    let out = render_str(&mut engine);
    assert!(
        out.contains('▸') && !out.contains("output:"),
        "second Ctrl+O should collapse: {out:?}"
    );
}

#[test]
fn thinking_indicator_shows_pending_then_clears_on_stream_end() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Submit a prompt → state.pending becomes true, turn_started_at set.
    for ch in "hi".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    let out = render_str(&mut engine);
    assert!(
        out.contains("[thinking"),
        "thinking placeholder missing while pending: {out:?}"
    );

    // Stream end clears pending, records last_turn_duration_ms.
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.stream.delta", "text": "hello" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.stream.end", "model": "test", "duration_ms": 100 }),
    );
    let out = render_str(&mut engine);
    assert!(
        !out.contains("[thinking"),
        "thinking placeholder should clear after stream end: {out:?}"
    );
    assert!(
        out.contains("[done in"),
        "[done in Xms] indicator missing on statusline: {out:?}"
    );
}

#[test]
fn double_escape_within_window_emits_interrupt_all() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Mid-turn → first ESC interrupts.
    for ch in "hi".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    let _ = engine.take_emit_queue();
    engine.handle_key(key("escape")).expect("first esc");
    let first = engine.take_emit_queue();
    assert_eq!(
        first[0].1.get("kind").and_then(|v| v.as_str()),
        Some("chat.interrupt"),
        "first ESC should emit chat.interrupt"
    );

    // Second ESC within 600ms → escalates to interrupt_all.
    engine.handle_key(key("escape")).expect("second esc");
    let second = engine.take_emit_queue();
    assert_eq!(
        second[0].1.get("kind").and_then(|v| v.as_str()),
        Some("chat.interrupt_all"),
        "second ESC within window should escalate"
    );
}

#[test]
fn slash_help_opens_help_popup() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);
    for ch in "/help".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    let _ = render_str(&mut engine);
    // Help popup is wrapped in `bordered_box`. Snapshot the framebuffer
    // to assert the corners + side bars actually paint.
    let snap = engine.snapshot();
    assert!(snap.contains("help"), "help popup body missing: {snap}");
    assert!(
        snap.contains('╭') && snap.contains('╮'),
        "help popup top corners missing: {snap}"
    );
    assert!(
        snap.contains('╰') && snap.contains('╯'),
        "help popup bottom corners missing: {snap}"
    );
}

#[test]
fn slash_help_popup_side_bars_paint_every_body_row() {
    // Two regressions guarded by this test:
    //
    // 1. Cross-axis-stretch: before that fix, `tui.text { content = "│" }`
    //    side bars only painted row 0 of the popup body. After the fix
    //    they're `tui.fill { char = "│" }` and CSS-flexbox-style cross
    //    stretch in the body row guarantees the fill spans the body's
    //    natural cross.
    //
    // 2. Body-overflow / missing bottom rule: the cross-stretch fix
    //    exposed that `popup_help`'s content (~17 lines of HELP_BODY)
    //    overflowed the 60%-of-24 anchored height, starving the bottom
    //    `╰────╯` of its 1-row budget. The popup composition now wraps
    //    the body in `tui.scrollable` inside a flex (`tui.expanded`) cell
    //    so the bottom rule always paints at the popup's bottom edge.
    //
    // Verify by walking every row of the popup's body span and asserting
    // each carries the left + right `│` chrome at the expected columns,
    // PLUS that the popup is fully enclosed (top + bottom rules at the
    // same column span).
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);
    for ch in "/help".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    let _ = render_str(&mut engine);
    let snap = engine.snapshot();

    // Locate the popup's top rule. The bottom rule is the LAST '╯' in
    // the snapshot — the input field at the bottom of the screen also
    // owns one and we want the popup's, which sits above it. Per the
    // anchored 60% sizing the popup span is 14 rows tall starting at
    // row 5 (centered in 24).
    let rows: Vec<&str> = snap.lines().collect();
    let popup_top_idx = rows
        .iter()
        .position(|r| r.contains('╭'))
        .expect("popup top rule");
    // Char-indexed columns — `str::find` returns byte offsets, but
    // multi-byte UTF-8 box-drawing glyphs (each 3 bytes) make those
    // offsets 3× the visible column. Walk chars to get the visible
    // column index instead.
    let popup_top_chars: Vec<char> = rows[popup_top_idx].chars().collect();
    let popup_left_col = popup_top_chars
        .iter()
        .position(|&c| c == '╭')
        .expect("popup top rule column");
    let popup_right_col = popup_top_chars
        .iter()
        .rposition(|&c| c == '╮')
        .expect("popup top right corner column");

    // Every body row of the popup must carry side bars at the popup's
    // left + right edges. Iterate until we hit the popup's bottom rule
    // (`╰────╯`); every row in between must have `│` at both edges, and
    // the bottom rule itself must be present (full enclosure — the
    // overflow regression that motivated the scrollable wrap).
    let mut body_rows_seen = 0;
    let mut popup_bottom_idx: Option<usize> = None;
    for (i, row) in rows.iter().enumerate().skip(popup_top_idx + 1) {
        let chars: Vec<char> = row.chars().collect();
        if popup_left_col < chars.len() && chars[popup_left_col] == '╰' {
            // Hit the popup's bottom rule — stop and verify bottom-right.
            assert!(
                popup_right_col < chars.len() && chars[popup_right_col] == '╯',
                "popup bottom-right corner missing at col {popup_right_col} on row {i}: \
                 {row:?}\nfull snapshot:\n{snap}"
            );
            popup_bottom_idx = Some(i);
            break;
        }
        if popup_left_col >= chars.len() || chars[popup_left_col] != '│' {
            // Past the popup's vertical extent without seeing a bottom rule.
            break;
        }
        body_rows_seen += 1;
        assert!(
            popup_right_col < chars.len() && chars[popup_right_col] == '│',
            "popup body row {i} missing right side bar at col {popup_right_col}: \
             {row:?}\nfull snapshot:\n{snap}"
        );
    }
    assert!(
        popup_bottom_idx.is_some(),
        "popup bottom rule `╰────╯` not found below top rule at row {popup_top_idx} — \
         the help popup must be fully enclosed (top + bottom rules):\n{snap}"
    );
    assert!(
        body_rows_seen >= 5,
        "expected ≥ 5 popup body rows with side bars (saw {body_rows_seen}); \
         the help popup is multi-line by construction:\n{snap}"
    );
}

#[test]
fn slash_yolo_emits_tool_gate_set_mode() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);
    for ch in "/yolo".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    let emits = engine.take_emit_queue();
    assert_eq!(emits.len(), 1, "expected one egress");
    assert_eq!(
        emits[0].1.get("kind").and_then(|v| v.as_str()),
        Some("tool-gate.set_mode")
    );
    assert_eq!(
        emits[0].1.get("mode").and_then(|v| v.as_str()),
        Some("yolo")
    );
}

#[test]
fn tool_permission_request_opens_popup_with_approve_deny() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({
            "kind": "tool-gate.permission_request",
            "id": "perm-1",
            "tool": "Bash",
            "input_pretty": "ls -la /tmp"
        }),
    );
    let out = render_str(&mut engine);
    assert!(
        out.contains("permission requested"),
        "permission popup title missing: {out:?}"
    );
    assert!(
        out.contains("[A]pprove") && out.contains("[D]eny"),
        "popup footer missing approve/deny chrome: {out:?}"
    );
    // Permission popup wraps content in bordered_box — corners must paint.
    let snap = engine.snapshot();
    assert!(
        snap.contains('╭') && snap.contains('╮') && snap.contains('╰') && snap.contains('╯'),
        "permission popup borders missing: {snap}"
    );

    // Press 'a' → emits approve response.
    let _ = engine.take_emit_queue();
    engine.handle_key(key("a")).expect("a");
    let emits = engine.take_emit_queue();
    assert_eq!(
        emits[0].1.get("kind").and_then(|v| v.as_str()),
        Some("tool.permission_response")
    );
    assert_eq!(
        emits[0].1.get("decision").and_then(|v| v.as_str()),
        Some("approve")
    );
}

#[test]
fn chat_popup_info_warning_error_all_render_with_borders() {
    // All three message-popup variants share `bordered_box` chrome — only
    // the border color and title glyph differ. Verifies each fires the
    // box-drawing corners; color verification stays out of scope (the
    // snapshot drops style by design).
    for level in &["info", "warning", "error"] {
        let mut engine = Engine::new(80, 24).expect("engine");
        engine.load_scenario(&chat_lua_source()).expect("load");
        let _ = render_str(&mut engine);
        dispatch_event(
            &mut engine,
            json!({
                "kind": "chat.popup",
                "level": level,
                "title": "test",
                "message": "body text",
            }),
        );
        let _ = render_str(&mut engine);
        let snap = engine.snapshot();
        assert!(
            snap.contains('╭') && snap.contains('╮') && snap.contains('╰') && snap.contains('╯'),
            "{level} popup borders missing: {snap}"
        );
        assert!(
            snap.contains("body text"),
            "{level} popup body missing: {snap}"
        );
    }
}

#[test]
fn slash_autocomplete_opens_when_typing_slash() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    engine.handle_key(key("/")).expect("/");
    let out = render_str(&mut engine);
    // Multiple commands begin with `/` so the popup should list them.
    assert!(
        out.contains("/new") || out.contains("/help"),
        "slash autocomplete not visible: {out:?}"
    );
}

#[test]
fn slash_quit_emits_exit_side_effect() {
    // Bug-1 regression coverage. Distinct from `slash_quit_requests_exit`
    // above (which exercises the same code path under a different name)
    // because the spec's bug-list explicitly names this scenario.
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    for ch in "/quit".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");

    assert!(
        engine.exit_requested(),
        "/quit must emit `{{ kind = \"exit\" }}` side effect that the engine acts on"
    );
}

#[test]
fn typing_slash_keeps_cursor_after_slash() {
    // Regression: when the user typed `/` from an empty input, the
    // appearance of the slash autocomplete dropdown shifted main_column's
    // child positions by one slot, re-mounting the input field and
    // dropping the text_input's per-instance cursor (clamping it back to
    // 0). The fix gives bordered_box's outer column a stable user-key so
    // the reconciler reuses the input subtree across the layout shift.
    //
    // We can't read text_input's cursor directly from the test surface,
    // but the next character the user types lands at the cursor's
    // current byte offset. So: type `/` then `quit\n`. If the cursor
    // stayed at 1, the value submitted is "/quit" → exits. If the
    // cursor regressed to 0, every subsequent char prepends → value
    // becomes "tiuq/" (each char inserted at offset 0 in turn). That
    // doesn't match `/quit` so no exit fires; we surface the bug via
    // `exit_requested`.
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    engine.handle_key(key("/")).expect("/");
    let _ = render_str(&mut engine);
    for ch in "quit".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    let _ = engine.take_emit_queue();
    engine.handle_key(key("enter")).expect("enter");

    assert!(
        engine.exit_requested(),
        "cursor regressed: typing `/` then `quit` then Enter must produce `/quit` and exit. \
         If exit_requested is false, each char prepended at cursor 0 instead of appending."
    );
}

#[test]
fn popup_open_routes_pgdn_to_popup_not_transcript() {
    // With a popup open, scroll keys (PgUp/PgDn/Home/End) target the
    // popup's scrollable. The transcript's scroll offset must not move.
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Pump enough transcript content that PgDn would have something to
    // scroll if it were routed to the transcript.
    for _ in 0..40 {
        dispatch_event(
            &mut engine,
            json!({ "kind": "chat.message.append", "role": "user", "text": "x" }),
        );
    }
    let _ = render_str(&mut engine);

    // Open the help popup.
    for ch in "/help".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    let _ = render_str(&mut engine);

    // Read live offsets via the Lua-exposed `tui.scroll_position`. The
    // engine refreshes this map after every render, so it reflects the
    // current frame's geometry.
    fn read_offset(engine: &mut Engine, key: &str) -> u16 {
        let lua = engine.lua();
        let chunk = format!(
            r#"
            local p = tui.scroll_position("{key}")
            return p and p.offset or -1
            "#
        );
        let v: i64 = lua
            .load(chunk.as_str())
            .eval()
            .expect("scroll_position eval");
        if v < 0 {
            panic!("no scroll_position for `{key}`");
        }
        v as u16
    }

    let transcript_before = read_offset(&mut engine, "transcript");

    // PgDn should scroll the popup's body, not the transcript.
    engine.handle_key(key("pagedown")).expect("pagedown");
    let _ = render_str(&mut engine);

    let transcript_after = read_offset(&mut engine, "transcript");
    assert_eq!(
        transcript_before, transcript_after,
        "transcript scroll moved while popup was open — popup should own scroll keys"
    );

    let popup_offset = read_offset(&mut engine, "popup_help");
    assert!(
        popup_offset > 0,
        "popup_help scroll offset stayed at 0 after PgDn — scroll key didn't reach the popup"
    );
}
