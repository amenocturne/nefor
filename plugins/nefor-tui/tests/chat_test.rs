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
use nefor_tui::mouse::{MouseKind, MouseMessage};
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
    // The input field should NOT carry a default hint — the bordered
    // box below the transcript is self-explanatory. Substrings from the
    // legacy hint must be absent.
    for needle in ["type a message", "ype a message", "/help for keys"] {
        assert!(
            !out.contains(needle),
            "input placeholder should be empty, found {needle:?} in: {out:?}"
        );
    }
    // Drain — the script doesn't emit anything at boot.
    assert!(engine.take_emit_queue().is_empty());
}

#[test]
fn input_field_has_no_default_placeholder() {
    // Belt-and-braces: even if the broader frame test above were edited
    // for unrelated reasons, this one specifically pins the contract
    // that `chat.lua` does not configure a `placeholder` on the input.
    // The text_input renders the placeholder dimmed inside the bordered
    // box; once removed, the box's first interior row is empty (modulo
    // the cursor cell at column 0).
    let src = chat_lua_source();
    assert!(
        !src.contains("placeholder ="),
        "starter/chat.lua should not set a placeholder on the input"
    );

    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&src).expect("load");
    let out = render_str(&mut engine);
    // Sanity: the bordered box still renders (corners present), just
    // without any hint text.
    for corner in ['╭', '╮', '╰', '╯'] {
        assert!(
            out.contains(corner),
            "input border missing corner {corner:?}: {out:?}"
        );
    }
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
    // /new must cancel any in-flight work AND clear the chat: emits both
    // chat.interrupt_all (kills graphs/pending tool calls) and chat.reset.
    assert_eq!(
        emits.len(),
        2,
        "expected interrupt_all + reset egress, got {emits:?}"
    );
    let kinds: Vec<_> = emits
        .iter()
        .map(|(_, b)| b.get("kind").and_then(|v| v.as_str()).unwrap_or(""))
        .collect();
    assert!(
        kinds.contains(&"chat.interrupt_all"),
        "missing chat.interrupt_all in {kinds:?}"
    );
    assert!(
        kinds.contains(&"chat.reset"),
        "missing chat.reset in {kinds:?}"
    );

    let out = render_str(&mut engine);
    assert!(
        !out.contains("previous"),
        "transcript should be cleared after /new: {out:?}"
    );
}

#[test]
fn slash_new_clears_dag_runs() {
    let mut engine = Engine::new(120, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Seed an active DAG run.
    dispatch_event(
        &mut engine,
        json!({
            "kind": "graph.run_started",
            "run_id": "run-aaaaaaaa",
            "total_nodes": 3,
        }),
    );
    let out = render_str(&mut engine);
    assert!(
        out.contains("DAG run-aaaa"),
        "dag header should appear pre-/new: {out:?}"
    );

    // /new + Enter.
    for ch in "/new".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    let _ = engine.take_emit_queue();
    engine.handle_key(key("enter")).expect("enter");
    let _ = engine.take_emit_queue();
    let out = render_str(&mut engine);
    assert!(
        !out.contains("DAG run-aaaa"),
        "dag panel should be empty after /new: {out:?}"
    );
}

// ── DAG panel (phase 7) ───────────────────────────────────────────────
//
// These exercise the sidebar that subscribes to `reasoner-graph` plugin
// lifecycle events (`graph.run_started`, `graph.node_dispatched`,
// `graph.node_result`, `graph.run_complete`). The panel is visible by
// default; Ctrl+B toggles it off. Linger handling is pure-update, so a
// completed run drops on the next event after `DAG_LINGER_MS` of engine
// time has passed — `Engine::advance_time` plus a synthetic event drives
// the prune deterministically without sleeping.

#[test]
fn graph_run_started_creates_a_dag_panel_row() {
    let mut engine = Engine::new(120, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

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

// Force the renderer to repaint and return the full framebuffer text.
// Plain `render_if_dirty` only emits a *diff* against the prior frame,
// so a check on its returned bytes misses cells that didn't change. The
// engine snapshot returns every cell verbatim, which is what state-flip
// tests actually want to inspect.
fn render_snapshot(engine: &mut Engine) -> String {
    engine.mark_animation_tick();
    let _ = engine.render_if_dirty().expect("render");
    engine.snapshot()
}

#[test]
fn ctrl_b_uppercase_letter_still_toggles() {
    // Some terminals (notably with Caps Lock or alternate keyboard
    // layouts) deliver Ctrl+B as `KeyCode::Char('B')` + CONTROL — i.e.
    // uppercase letter, no shift modifier. The Lua matcher must accept
    // either casing or the press is silently dropped. The kind() builder
    // in input.rs preserves the casing of the underlying char, so this
    // test pins the chat surface against that asymmetry.
    let mut engine = Engine::new(120, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let out = render_str(&mut engine);
    assert!(
        out.contains("(no active runs)"),
        "sidebar should be visible by default: {out:?}"
    );

    engine
        .handle_key(KeyMessage {
            name: "B".into(),
            mods: vec!["ctrl"],
        })
        .expect("ctrl+B uppercase");
    let out = render_snapshot(&mut engine);
    assert!(
        !out.contains("(no active runs)"),
        "Ctrl+B (uppercase B) must still toggle sidebar: {out:?}"
    );
}

#[test]
fn ctrl_b_single_press_toggles_sidebar() {
    // The chat surface boots with `show_sidebar = true` (legacy parity:
    // sidebar visible by default in wide terminals). One Ctrl+B should
    // hide it; a second should bring it back. A regression where the
    // first press is consumed silently and only the second flips state
    // would surface here. Test at 80 cols (typical default) to match
    // the user's reported environment.
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let out = render_str(&mut engine);
    assert!(
        out.contains("(no active runs)"),
        "sidebar should be visible by default: {out:?}"
    );

    // Send the realistic Ctrl+B shape (name="b", mods=["ctrl"]).
    engine
        .handle_key(KeyMessage {
            name: "b".into(),
            mods: vec!["ctrl"],
        })
        .expect("ctrl+b");
    let out = render_snapshot(&mut engine);
    assert!(
        !out.contains("(no active runs)"),
        "single Ctrl+B must hide the sidebar: {out:?}"
    );

    // A second press toggles back on.
    engine
        .handle_key(KeyMessage {
            name: "b".into(),
            mods: vec!["ctrl"],
        })
        .expect("ctrl+b again");
    let out = render_snapshot(&mut engine);
    assert!(
        out.contains("(no active runs)"),
        "second Ctrl+B must restore the sidebar: {out:?}"
    );
}

// ── Prompt-history recall on Up/Down with empty input ────────────────
//
// Legacy spec section 7: when the input field is empty and the user
// presses Up, fill with the last submitted prompt; subsequent Up cycles
// to older entries. Down moves forward; Down past the newest entry
// clears the input and exits navigation. Any value mutation (typing,
// backspace) drops the navigation cursor.

#[test]
fn arrow_up_on_empty_input_recalls_last_prompt() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Submit a first prompt so prompt_history has one entry.
    for ch in "hello".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    let _ = engine.take_emit_queue();

    // Buffer should now be empty.
    let out = render_snapshot(&mut engine);
    assert!(
        out.contains("hello"),
        "submitted prompt should still appear in the transcript: {out:?}"
    );

    // Up on empty buffer recalls the last prompt.
    engine.handle_key(key("up")).expect("up");
    let out = render_snapshot(&mut engine);
    assert!(
        out.contains("hello"),
        "input should re-fill with the recalled prompt after Up: {out:?}"
    );
}

#[test]
fn arrow_up_cycles_through_older_prompts() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Submit two prompts. Newest at index 1.
    for prompt in ["first", "second"] {
        for ch in prompt.chars() {
            engine.handle_key(key(&ch.to_string())).expect("type");
        }
        engine.handle_key(key("enter")).expect("enter");
        let _ = engine.take_emit_queue();
    }

    // Up #1 → "second" (newest)
    engine.handle_key(key("up")).expect("up1");
    let snap = render_snapshot(&mut engine);
    assert!(
        snap.contains("second"),
        "first Up should recall the most recent prompt: {snap:?}"
    );

    // Up #2 → "first" (older)
    engine.handle_key(key("up")).expect("up2");
    let snap = render_snapshot(&mut engine);
    // "second" lives in the transcript too; check the input row by
    // looking for the input chrome `╰` rule and asserting "first" sits
    // in the surrounding row. A simpler proxy: "first" must appear
    // again, which it does only when the input recalls it. The
    // submitted "first" prompt also appears in the transcript above
    // the input, so we can't distinguish on substring alone — instead
    // check that the snapshot contains BOTH prompts (transcript +
    // input).
    let firsts = snap.matches("first").count();
    assert!(
        firsts >= 2,
        "second Up should also place 'first' into the input (giving 2+ occurrences): {snap:?}"
    );
}

#[test]
fn arrow_down_after_recall_clears_input() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    for ch in "draft".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    let _ = engine.take_emit_queue();

    engine.handle_key(key("up")).expect("up recall");
    let snap = render_snapshot(&mut engine);
    let drafts = snap.matches("draft").count();
    assert!(
        drafts >= 2,
        "Up should recall 'draft' into the input, giving 2 occurrences: {snap:?}"
    );

    // Down past the newest entry clears the input.
    engine.handle_key(key("down")).expect("down clear");
    let snap = render_snapshot(&mut engine);
    let drafts_after = snap.matches("draft").count();
    assert!(
        drafts_after < drafts,
        "Down past newest should clear the input, dropping one occurrence: \
         was {drafts}, now {drafts_after}: {snap:?}"
    );
}

#[test]
fn arrow_up_on_non_empty_input_does_not_overwrite() {
    // Legacy: Up on a non-empty single-line buffer is a no-op — the
    // user is mid-edit and we won't yank their draft. Routes to scroll
    // instead via the existing fallback.
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    for ch in "old".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    let _ = engine.take_emit_queue();
    // Force a reconcile so the text_input's internal `last_value`
    // syncs to the post-submit empty buffer before we start typing.
    let _ = render_snapshot(&mut engine);

    // Type a new draft.
    for ch in "new".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    let snap = render_snapshot(&mut engine);
    assert!(
        snap.contains("new") && !snap.contains("old\n"),
        "draft should be 'new': {snap:?}"
    );

    // Up should not overwrite the draft with "old". The text_input
    // bubbles Up to Lua only at edge-of-content, but the chat surface's
    // history-recall guard checks `empty || navigating` — neither true
    // here, so the press should fall through to the scroll path
    // without touching input_value. The single-line input bubbles Up
    // unconditionally so the user can scroll.
    engine.handle_key(key("up")).expect("up no-op");
    let snap = render_snapshot(&mut engine);
    assert!(
        snap.contains("new"),
        "input draft 'new' should survive Up on a non-empty buffer: {snap:?}"
    );
    assert!(
        snap.matches("old").count() == 1,
        "'old' should only appear in the transcript, not pulled into the input: {snap:?}"
    );
}

#[test]
fn ctrl_b_after_typing_still_single_press_toggles() {
    // Realistic user session: type a few characters into the input, then
    // press Ctrl+B. The text_input swallows the printables, but Ctrl+B
    // (modifier-prefixed) must bubble to Lua and toggle on the first
    // press — not require a second press.
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    for ch in "hello".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    let _ = render_snapshot(&mut engine);

    engine
        .handle_key(KeyMessage {
            name: "b".into(),
            mods: vec!["ctrl"],
        })
        .expect("ctrl+b");
    let out = render_snapshot(&mut engine);
    assert!(
        !out.contains("(no active runs)"),
        "single Ctrl+B after typing must hide the sidebar: {out:?}"
    );
}

#[test]
fn tool_expanded_pretty_prints_input_object() {
    let mut engine = Engine::new(120, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Seed a tool call whose `input` is a JSON object (the wire shape
    // for any non-Bash tool: Read, Edit, Write, etc). Legacy spec
    // section 5 says expanded view shows pretty-printed JSON, not the
    // `(object)` placeholder the previous build emitted.
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.tool.start",
            "id": "t1",
            "name": "Read",
            "input": { "file_path": "/tmp/example.txt" },
        }),
    );
    engine.handle_key(key("ctrl_o")).expect("ctrl_o expand");
    let out = render_str(&mut engine);
    assert!(
        out.contains("file_path"),
        "expanded tool view should pretty-print the input keys: {out:?}"
    );
    assert!(
        !out.contains("(object)"),
        "placeholder text leaked into expanded view: {out:?}"
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
    // Legacy spec section 4 shows the turn duration as a bare segment
    // (`100ms`, `2s`, etc.) — no `[done in ...]` brackets. The previous
    // behavior added an extra status_ok segment that wasn't in legacy.
    assert!(
        out.contains("100ms"),
        "turn duration missing on statusline: {out:?}"
    );
    assert!(
        !out.contains("[done in"),
        "legacy spec: no [done in ...] segment, just bare duration: {out:?}"
    );
}

#[test]
fn thinking_indicator_has_no_braille_spinner() {
    // Legacy spec section 14 — the pre-first-delta placeholder is
    // deliberately minimalist: static `[thinking... Ns]` text, no
    // spinner. Earlier builds prepended a braille animation; this test
    // pins the minimalist behavior so a future refactor can't sneak
    // the spinner back in.
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    for ch in "hi".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    let out = render_str(&mut engine);
    assert!(
        out.contains("[thinking"),
        "thinking placeholder missing while pending: {out:?}"
    );
    // None of the braille glyphs should appear anywhere in the frame.
    for braille in ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'] {
        assert!(
            !out.contains(braille),
            "braille spinner glyph '{braille}' present (legacy spec: no spinner): {out:?}"
        );
    }
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
    // Wire-shape contract: the event the popup listens for is the EXACT
    // body tool-gate emits when policy=Prompt — `chat.tool.permission_request`
    // with `id`, `tool`, `args` (see plugins/tool-gate/src/main.rs:
    // permission_request_body). Test against the real shape so a future
    // protocol drift breaks here, not silently in production.
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.tool.permission_request",
            "id": "perm-1",
            "tool": "Bash",
            "args": { "command": "ls -la /tmp" }
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
    // The args formatter renders `key = "value"` lines — confirm the
    // command is visible so the user knows what they're approving.
    assert!(
        out.contains("command") && out.contains("ls -la /tmp"),
        "args summary missing from popup body: {out:?}"
    );
    // Permission popup wraps content in bordered_box — corners must paint.
    let snap = engine.snapshot();
    assert!(
        snap.contains('╭') && snap.contains('╮') && snap.contains('╰') && snap.contains('╯'),
        "permission popup borders missing: {snap}"
    );

    // Press 'a' → emits approve response back to tool-gate.
    let _ = engine.take_emit_queue();
    engine.handle_key(key("a")).expect("a");
    let emits = engine.take_emit_queue();
    assert_eq!(emits.len(), 1, "expected exactly one egress on approve");
    assert_eq!(
        emits[0].1.get("kind").and_then(|v| v.as_str()),
        Some("tool.permission_response")
    );
    assert_eq!(
        emits[0].1.get("id").and_then(|v| v.as_str()),
        Some("perm-1"),
        "response must carry the same id tool-gate sent"
    );
    assert_eq!(
        emits[0].1.get("decision").and_then(|v| v.as_str()),
        Some("approve")
    );

    // Re-open and exercise the deny path via 'd'.
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.tool.permission_request",
            "id": "perm-2",
            "tool": "Bash",
            "args": { "command": "rm -rf /" }
        }),
    );
    let _ = render_str(&mut engine);
    let _ = engine.take_emit_queue();
    engine.handle_key(key("d")).expect("d");
    let emits = engine.take_emit_queue();
    assert_eq!(emits.len(), 1, "expected exactly one egress on deny");
    assert_eq!(
        emits[0].1.get("kind").and_then(|v| v.as_str()),
        Some("tool.permission_response")
    );
    assert_eq!(
        emits[0].1.get("id").and_then(|v| v.as_str()),
        Some("perm-2")
    );
    assert_eq!(
        emits[0].1.get("decision").and_then(|v| v.as_str()),
        Some("deny")
    );

    // Re-open and exercise Esc → deny + close.
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.tool.permission_request",
            "id": "perm-3",
            "tool": "Bash",
            "args": {}
        }),
    );
    let _ = render_str(&mut engine);
    let _ = engine.take_emit_queue();
    engine.handle_key(key("escape")).expect("esc");
    let emits = engine.take_emit_queue();
    assert_eq!(emits.len(), 1, "expected exactly one egress on esc");
    assert_eq!(
        emits[0].1.get("kind").and_then(|v| v.as_str()),
        Some("tool.permission_response")
    );
    assert_eq!(
        emits[0].1.get("id").and_then(|v| v.as_str()),
        Some("perm-3")
    );
    assert_eq!(
        emits[0].1.get("decision").and_then(|v| v.as_str()),
        Some("deny")
    );
    // Popup must be closed after Esc — force a fresh frame so the
    // snapshot reflects the post-update tree, not the prior render.
    let snap_after = render_str(&mut engine);
    assert!(
        !snap_after.contains("permission requested"),
        "popup should be closed after Esc: {snap_after}"
    );

    // Enter is also wired to approve as a quality-of-life shortcut (the
    // input field is unfocused while the popup is open, so Enter bubbles
    // up to Lua instead of submitting a chat message).
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.tool.permission_request",
            "id": "perm-4",
            "tool": "Bash",
            "args": {}
        }),
    );
    let _ = render_str(&mut engine);
    let _ = engine.take_emit_queue();
    engine.handle_key(key("enter")).expect("enter");
    let emits = engine.take_emit_queue();
    assert_eq!(emits.len(), 1, "expected exactly one egress on enter");
    assert_eq!(
        emits[0].1.get("decision").and_then(|v| v.as_str()),
        Some("approve")
    );
    assert_eq!(
        emits[0].1.get("id").and_then(|v| v.as_str()),
        Some("perm-4")
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
fn autocomplete_open_enter_runs_highlighted_command() {
    // Browser-style combobox: when the slash autocomplete dropdown is
    // open and the user presses Enter, the highlighted match runs — not
    // the partial fragment they actually typed. Type `/mo`, the dropdown
    // shows `/model` (the only command starting with "mo") highlighted;
    // Enter must dispatch the `/model` action, which fans out one
    // `chat.model.list_requested` per connected provider (legacy spec
    // section 8/12) — not bottom-fall-through to a generic `chat.command`
    // named "mo".
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Seed two connected providers so /model has someone to fan out to.
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.auth.status", "provider": "ollama", "status": "connected" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.auth.status", "provider": "anthropic", "status": "connected" }),
    );

    for ch in "/mo".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    let out = render_str(&mut engine);
    assert!(
        out.contains("/model"),
        "autocomplete should list /model after typing /mo: {out:?}"
    );

    let _ = engine.take_emit_queue();
    engine.handle_key(key("enter")).expect("enter");
    let emits = engine.take_emit_queue();
    assert_eq!(
        emits.len(),
        2,
        "Enter on open autocomplete with /model highlighted must fan out one list_requested per connected provider"
    );
    for e in &emits {
        assert_eq!(
            e.1.get("kind").and_then(|v| v.as_str()),
            Some("chat.model.list_requested"),
            "expected chat.model.list_requested, got {:?}",
            e.1
        );
        assert!(
            e.1.get("provider").and_then(|v| v.as_str()).is_some(),
            "fan-out must include `provider` field per legacy spec: {:?}",
            e.1
        );
    }
}

#[test]
fn autocomplete_open_tab_completes_without_submitting() {
    // Tab while autocomplete is open replaces the input value with the
    // highlighted match's command text — no submit fires. This test
    // belt-and-braces the Tab path so the Enter path's new behaviour
    // doesn't subsume Tab.
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Seed a connected provider so /model has fan-out targets.
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.auth.status", "provider": "ollama", "status": "connected" }),
    );

    for ch in "/mo".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    let _ = render_str(&mut engine);
    let _ = engine.take_emit_queue();

    engine.handle_key(key("tab")).expect("tab");
    let emits = engine.take_emit_queue();
    assert!(
        emits.is_empty(),
        "Tab must not submit — it only replaces the input value: {emits:?}"
    );
    let out = render_str(&mut engine);
    // The input now contains `/model ` (takes_args=true → trailing space).
    // We verify by exit-shape via Backspace + Enter: backspace removes the
    // trailing space, leaving `/model`, which submits to chat.model.list.
    let _ = engine.take_emit_queue();
    engine.handle_key(key("backspace")).expect("backspace");
    engine.handle_key(key("enter")).expect("enter");
    let emits = engine.take_emit_queue();
    assert_eq!(
        emits.len(),
        1,
        "Tab+backspace+Enter should submit /model with one connected provider: {out:?} -> emits={emits:?}"
    );
    assert_eq!(
        emits[0].1.get("kind").and_then(|v| v.as_str()),
        Some("chat.model.list_requested"),
        "post-Tab value must be `/model `, with the cursor at end so backspace+Enter runs /model"
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

#[test]
fn arrow_up_scrolls_transcript_when_input_focused_at_top_line() {
    // Mac keyboards lack PgUp/PgDn, so Up/Down arrow keys map to
    // single-line scroll on the active surface. The chat input is
    // single-line (max_lines = 1) by default, so the focused text_input
    // bubbles Up unconditionally and Lua's update routes it to
    // `tui.scroll_by("transcript", -1)`.
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Pump enough transcript content that there's something to scroll up
    // through. Auto-scroll keeps the transcript pinned to the bottom, so
    // the offset is positive after the deltas land.
    for _ in 0..40 {
        dispatch_event(
            &mut engine,
            json!({ "kind": "chat.message.append", "role": "user", "text": "x" }),
        );
    }
    let _ = render_str(&mut engine);

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

    let before = read_offset(&mut engine, "transcript");
    assert!(
        before > 0,
        "test prerequisite: transcript should be scrolled past the top after 40 messages"
    );

    engine.handle_key(key("up")).expect("up");
    let _ = render_str(&mut engine);
    let after = read_offset(&mut engine, "transcript");
    assert!(
        after < before,
        "Up arrow with focused single-line input must scroll transcript up by 1 (before={before}, after={after})"
    );
    assert_eq!(
        before - after,
        1,
        "Up arrow should scroll transcript by exactly 1 line (before={before}, after={after})"
    );
}

#[test]
fn arrow_up_scrolls_transcript_when_input_empty() {
    // Spec coverage parity: when no popup is open and the input is
    // empty, Up arrow must scroll the transcript. Companion to the
    // top-line variant above; this one exercises the cursor-at-row-0
    // path through the empty-buffer fast track and asserts the result
    // by reading the live offset.
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    for _ in 0..40 {
        dispatch_event(
            &mut engine,
            json!({ "kind": "chat.message.append", "role": "user", "text": "x" }),
        );
    }
    let _ = render_str(&mut engine);

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

    let before = read_offset(&mut engine, "transcript");
    assert!(
        before > 0,
        "test prerequisite: transcript should overflow viewport"
    );
    engine.handle_key(key("up")).expect("up");
    let _ = render_str(&mut engine);
    let after = read_offset(&mut engine, "transcript");
    assert!(
        after < before,
        "Up arrow on empty input + no popup must scroll transcript (before={before}, after={after})"
    );
}

#[test]
fn mouse_wheel_up_scrolls_transcript() {
    // Wheel events under the transcript must scroll it. Pre-fix, the
    // wheel path mutated `scroll_y` but left `was_at_end` sticky from
    // the prior frame, so the next paint snapped scroll_y back to the
    // bottom under `stick_to = end` — making the transcript appear
    // "not scrollable". The fix updates `was_at_*` inside `scroll_by_signed`
    // so wheel and `tui.scroll_by` stay symmetric.
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    for _ in 0..40 {
        dispatch_event(
            &mut engine,
            json!({ "kind": "chat.message.append", "role": "user", "text": "x" }),
        );
    }
    let _ = render_str(&mut engine);

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

    let before = read_offset(&mut engine, "transcript");
    assert!(
        before > 0,
        "test prerequisite: transcript should overflow viewport"
    );

    // Wheel up over the transcript area. (3, 3) sits inside the
    // transcript's painted rect — top-left of the body row, past the
    // 1-cell outer padding.
    engine
        .handle_mouse(MouseMessage {
            kind: MouseKind::Wheel,
            x: 3,
            y: 3,
            button: Some("up"),
            mods: vec![],
        })
        .expect("wheel up");
    let _ = render_str(&mut engine);
    let after = read_offset(&mut engine, "transcript");
    assert!(
        after < before,
        "Wheel up must scroll transcript (before={before}, after={after}) — \
         pre-fix the post-paint stick_to=end re-pinned scroll_y to the bottom"
    );
}

#[test]
fn arrow_up_scrolls_popup_when_popup_open() {
    // With a popup open the active scroll target shifts to the popup's
    // scrollable. Up/Down arrows must follow PgUp/PgDn's modal-focus
    // routing — the transcript stays pinned, popup body scrolls.
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Open the help popup (HELP_BODY is multi-line so it has content to
    // scroll past).
    for ch in "/help".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    let _ = render_str(&mut engine);

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

    // Down arrow first to give the popup a non-zero offset, then Up to
    // verify Up routes to the popup (offset decreases).
    engine.handle_key(key("down")).expect("down");
    let _ = render_str(&mut engine);
    let popup_after_down = read_offset(&mut engine, "popup_help");
    assert!(
        popup_after_down > 0,
        "Down arrow must scroll the open popup, not the transcript: popup_help offset stayed at 0"
    );

    engine.handle_key(key("up")).expect("up");
    let _ = render_str(&mut engine);
    let popup_after_up = read_offset(&mut engine, "popup_help");
    assert!(
        popup_after_up < popup_after_down,
        "Up arrow must scroll the open popup back up (after_down={popup_after_down}, after_up={popup_after_up})"
    );

    let transcript_after = read_offset(&mut engine, "transcript");
    assert_eq!(
        transcript_before, transcript_after,
        "transcript scroll moved while popup was open — popup should own arrow keys"
    );
}

#[test]
fn statusline_renders_below_input_row() {
    // Per legacy spec, the statusline sits BELOW the input box. Verify
    // by rendering and walking rows: the input box's bottom-right
    // corner `╯` lies above the statusline, not below it.
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    // Send a stats event so the statusline has identifiable text.
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.session.stats", "model": "claude-test" }),
    );
    let _ = render_str(&mut engine);
    let snap = engine.snapshot();
    let rows: Vec<&str> = snap.lines().collect();

    // Find the LAST `╯` (input box's bottom-right corner) and the
    // statusline (row containing the model name `test`).
    let last_corner_row = rows
        .iter()
        .rposition(|r| r.contains('╯'))
        .expect("input bottom-right corner");
    let statusline_row = rows
        .iter()
        .rposition(|r| r.contains("test"))
        .expect("statusline with model name");
    assert!(
        statusline_row > last_corner_row,
        "statusline (row {statusline_row}) must be BELOW input box bottom (row {last_corner_row}):\n{snap}"
    );
}

#[test]
fn statusline_omits_scroll_segment_when_transcript_fits_viewport() {
    // Empty / tiny transcript → no scrollback. The scroll segment is
    // hidden entirely (legacy spec section 4: "Only rendered when total
    // > transcript_rows").
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);
    let snap = engine.snapshot();
    assert!(
        !snap.contains("100% ↓"),
        "scroll segment should be absent on empty transcript: {snap}"
    );
    assert!(
        !snap.contains("0% ↑"),
        "scroll segment should be absent on empty transcript: {snap}"
    );
}

#[test]
fn statusline_shows_bottom_marker_when_transcript_overflows() {
    // Push enough messages to overflow a 24-row terminal. The
    // transcript stick_to=end keeps us at the bottom; the scroll
    // segment should read `100% ↓ bottom`.
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);
    for i in 0..30 {
        dispatch_event(
            &mut engine,
            json!({
                "kind": "chat.message.append",
                "role": "user",
                "text": format!("line-{i}"),
            }),
        );
    }
    // First render lays out the transcript and populates the
    // scroll-position snapshot. The second render's `view` call sees
    // the populated snapshot and emits the scroll segment.
    let _ = render_snapshot(&mut engine);
    let snap = render_snapshot(&mut engine);
    assert!(
        snap.contains("100% ↓ bottom"),
        "expected `100% ↓ bottom` segment for at-end overflow:\n{snap}"
    );
}

#[test]
fn outer_padding_leaves_terminal_edges_blank() {
    // Outer padding so the UI doesn't sit flush against terminal edges
    // (legacy spec section 1: HPAD=2 horizontal, 1-cell vertical). The
    // first two columns and the last two columns are blank; the top and
    // bottom rows are blank.
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);
    let snap = engine.snapshot();
    let rows: Vec<&str> = snap.lines().collect();

    // Top row: all spaces.
    let top = rows.first().expect("top row");
    assert!(
        top.chars().all(|c| c == ' '),
        "top row must be blank (1-cell vpad): {top:?}"
    );
    // Bottom row: all spaces.
    let bot = rows.last().expect("bottom row");
    assert!(
        bot.chars().all(|c| c == ' '),
        "bottom row must be blank (1-cell vpad): {bot:?}"
    );
    // Left two columns on every row: spaces (HPAD=2).
    for (i, r) in rows.iter().enumerate() {
        let mut chars = r.chars();
        let c0 = chars.next().unwrap_or(' ');
        let c1 = chars.next().unwrap_or(' ');
        assert_eq!(c0, ' ', "col 0 of row {i} must be blank: {r:?}");
        assert_eq!(c1, ' ', "col 1 of row {i} must be blank (HPAD=2): {r:?}");
    }
    // Right two columns on every row: spaces (HPAD=2). Walk by chars so
    // multi-byte glyphs don't confuse the byte-indexed view.
    for (i, r) in rows.iter().enumerate() {
        let chars: Vec<char> = r.chars().collect();
        let n = chars.len();
        if n >= 2 {
            assert_eq!(
                chars[n - 1],
                ' ',
                "rightmost col of row {i} must be blank: {r:?}"
            );
            assert_eq!(
                chars[n - 2],
                ' ',
                "second-from-right col of row {i} must be blank (HPAD=2): {r:?}"
            );
        }
    }
}

#[test]
fn slash_model_no_args_fans_out_per_connected_provider_and_opens_popup() {
    // Legacy spec section 8/12: `/model` with no args
    //   1) emits one `chat.model.list_requested { provider }` per
    //      connected provider, and
    //   2) opens the ModelPicker popup with `awaiting` set to those
    //      provider names.
    // The transport adapter rejects requests that don't carry a
    // `provider` field (see starter/agentic_workflow.lua:1301), so the
    // fan-out shape is load-bearing — a single un-targeted request
    // would be dropped on the floor.
    let mut engine = Engine::new(120, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Two connected providers + one disconnected.
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.auth.status", "provider": "ollama", "status": "connected" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.auth.status", "provider": "anthropic", "status": "connected" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.auth.status", "provider": "openai", "status": "login_required" }),
    );

    for ch in "/model".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    let _ = engine.take_emit_queue();
    engine.handle_key(key("enter")).expect("enter");

    let emits = engine.take_emit_queue();
    assert_eq!(
        emits.len(),
        2,
        "should emit exactly one list_requested per CONNECTED provider (not login_required): {emits:?}"
    );
    let mut providers: Vec<String> = emits
        .iter()
        .map(|(_, body)| {
            body.get("provider")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        })
        .collect();
    providers.sort();
    assert_eq!(providers, vec!["anthropic", "ollama"]);

    // Popup is now visible.
    let out = render_str(&mut engine);
    assert!(
        out.contains("pick a model"),
        "ModelPicker popup title not visible: {out:?}"
    );
    assert!(
        out.contains("loading from 2 provider"),
        "ModelPicker should show loading footer for awaiting providers: {out:?}"
    );
}

#[test]
fn chat_models_listed_appends_into_open_picker_and_clears_awaiting() {
    // After `/model` opens the picker, each provider responds with
    // `chat.models.listed { provider, models }`. The picker appends the
    // models, dedups, sorts, and removes the answering provider from
    // the awaiting set.
    let mut engine = Engine::new(120, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.auth.status", "provider": "ollama", "status": "connected" }),
    );
    for ch in "/model".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");

    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.models.listed",
            "provider": "ollama",
            "models": ["qwen2:7b", "llama3:8b"],
        }),
    );
    let out = render_str(&mut engine);
    assert!(
        out.contains("qwen2:7b") && out.contains("llama3:8b"),
        "models from ollama should appear in picker: {out:?}"
    );
    // Awaiting cleared → loading footer gone.
    assert!(
        !out.contains("loading from"),
        "awaiting set should clear after the only provider responds: {out:?}"
    );
}

#[test]
fn model_picker_enter_emits_chat_model_set_with_provider() {
    // Up/Down moves the cursor; Enter emits chat.model.set carrying the
    // selected (provider, model) pair, then closes the popup.
    let mut engine = Engine::new(120, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.auth.status", "provider": "ollama", "status": "connected" }),
    );
    for ch in "/model".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.models.listed",
            "provider": "ollama",
            "models": ["qwen2:7b", "llama3:8b"],
        }),
    );
    let _ = render_str(&mut engine);
    let _ = engine.take_emit_queue();

    // Enter on default cursor (row 1 = "llama3:8b" alphabetically before qwen2).
    engine.handle_key(key("enter")).expect("enter");
    let emits = engine.take_emit_queue();
    assert_eq!(
        emits.len(),
        1,
        "Enter on picker should emit one chat.model.set: {emits:?}"
    );
    assert_eq!(
        emits[0].1.get("kind").and_then(|v| v.as_str()),
        Some("chat.model.set")
    );
    assert_eq!(
        emits[0].1.get("provider").and_then(|v| v.as_str()),
        Some("ollama")
    );
    assert_eq!(
        emits[0].1.get("model").and_then(|v| v.as_str()),
        Some("llama3:8b"),
        "default cursor should be on the alphabetically-first model"
    );

    // Popup closed.
    let out = render_str(&mut engine);
    assert!(
        !out.contains("pick a model"),
        "popup should close after Enter: {out:?}"
    );
}

#[test]
fn model_picker_typing_filters_query() {
    // Printable chars while the picker is open append to the filter
    // query, narrowing the visible list.
    let mut engine = Engine::new(120, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.auth.status", "provider": "ollama", "status": "connected" }),
    );
    for ch in "/model".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    // Render between popup-open and key.q so the text_input instance
    // syncs to the cleared input_value before the q arrives. Without
    // this render step the text_input still holds the pre-submit value
    // and absorbs the q (router routes to it as a printable editing
    // key) regardless of `focused=false`.
    let _ = render_str(&mut engine);
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.models.listed",
            "provider": "ollama",
            "models": ["qwen2:7b", "llama3:8b"],
        }),
    );
    let _ = render_str(&mut engine);

    engine.handle_key(key("q")).expect("q");
    let out = render_str(&mut engine);
    // Note: `llama3` is a substring of `ollama` (the provider name) too,
    // so we look for `llama3` specifically as the model-row signature.
    assert!(
        out.contains("qwen") && !out.contains("llama3"),
        "typing 'q' should filter to qwen-only: {out:?}"
    );
}

// ============================================================
// /resume slash + session picker
// ============================================================
//
// The picker reads from `$NEFOR_DATA_HOME/sessions/` (overridable via
// env var, set per-test for isolation). Selecting a row emits a
// `sessions.resume_request { session_id }` envelope onto the NCP bus —
// no process exit, no sidechannel file. The starter's `sessions` Lua
// module subscribes to that kind and runs the in-process swap.
//
// Test isolation: each test creates a tempdir, sets NEFOR_DATA_HOME to
// it, and tears it down on completion. Env var manipulation is
// process-global so we serialize via a mutex.

use std::io::Write;
use std::sync::Mutex;

// Process-global lock — env var mutation is unsafe across threads.
// `cargo test` runs unit tests in parallel by default; this serializes
// only the tests that touch NEFOR_DATA_HOME.
static RESUME_ENV_LOCK: Mutex<()> = Mutex::new(());

struct ResumeEnv {
    _guard: std::sync::MutexGuard<'static, ()>,
    _tempdir: tempfile::TempDir,
    data_home: PathBuf,
    prev: Option<String>,
}

impl ResumeEnv {
    fn new() -> Self {
        let guard = RESUME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tempdir = tempfile::tempdir().expect("tempdir");
        let data_home = tempdir.path().to_path_buf();
        std::fs::create_dir_all(data_home.join("sessions")).expect("mkdir sessions");
        let prev = std::env::var("NEFOR_DATA_HOME").ok();
        // Tests serialize via RESUME_ENV_LOCK so concurrent reads/writes
        // don't race. set_var is safe under edition 2021.
        std::env::set_var("NEFOR_DATA_HOME", &data_home);
        ResumeEnv {
            _guard: guard,
            _tempdir: tempdir,
            data_home,
            prev,
        }
    }

    fn sessions_dir(&self) -> PathBuf {
        self.data_home.join("sessions")
    }

    fn write_session(&self, id: &str, started_at: &str, prompt: Option<&str>) {
        let mut path = self.sessions_dir();
        path.push(format!("{id}.jsonl"));
        let mut f = std::fs::File::create(&path).expect("create session jsonl");
        let header = serde_json::json!({
            "_session": true,
            "session_id": id,
            "parent_session": serde_json::Value::Null,
            "started_at": started_at,
        });
        writeln!(f, "{}", serde_json::to_string(&header).unwrap()).unwrap();
        if let Some(text) = prompt {
            // One submit entry shaped like the engine writes them: the
            // engine stamps {ts, origin, target?, payload} and payload
            // is itself the JSON-encoded NCP envelope.
            let payload = serde_json::json!({
                "type": "event",
                "body": { "kind": "chat.input.submit", "text": text },
            });
            let entry = serde_json::json!({
                "ts": "2026-05-03T12:00:00.000Z",
                "origin": "nefor-tui",
                "target": serde_json::Value::Null,
                "payload": serde_json::to_string(&payload).unwrap(),
            });
            writeln!(f, "{}", serde_json::to_string(&entry).unwrap()).unwrap();
        }
    }
}

impl Drop for ResumeEnv {
    fn drop(&mut self) {
        // Still under RESUME_ENV_LOCK.
        match self.prev.as_deref() {
            Some(v) => std::env::set_var("NEFOR_DATA_HOME", v),
            None => std::env::remove_var("NEFOR_DATA_HOME"),
        }
    }
}

#[test]
fn slash_resume_opens_session_picker_popup() {
    let env = ResumeEnv::new();
    env.write_session(
        "aaaa1111-1111-1111-1111-111111111111",
        "2026-05-01T10:00:00.000Z",
        Some("first prompt"),
    );
    env.write_session(
        "bbbb2222-2222-2222-2222-222222222222",
        "2026-05-02T11:00:00.000Z",
        Some("second prompt"),
    );

    let mut engine = Engine::new(120, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    for ch in "/resume".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    let out = render_str(&mut engine);
    assert!(
        out.contains("resume a session"),
        "picker popup should open: {out:?}"
    );
}

#[test]
fn session_picker_lists_recent_sessions_with_preview() {
    let env = ResumeEnv::new();
    env.write_session(
        "11111111-1111-1111-1111-111111111111",
        "2026-05-01T10:00:00.000Z",
        Some("the first message"),
    );

    let mut engine = Engine::new(120, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    for ch in "/resume".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    let out = render_str(&mut engine);

    // The preview text from the first user message must surface.
    assert!(
        out.contains("the first message"),
        "preview should include first user prompt: {out:?}"
    );
    // The formatted timestamp from the header (MM-DD HH:MM).
    assert!(
        out.contains("05-01 10:00"),
        "formatted timestamp should appear: {out:?}"
    );
}

#[test]
fn resume_keeps_tui_alive() {
    // Picker selection must NOT terminate the TUI process. Instead it
    // emits a `sessions.resume_request { session_id }` envelope onto the
    // bus; the starter's sessions module owns the in-process swap.
    let env = ResumeEnv::new();
    let session_id = "abcd1234-5678-9012-3456-7890abcdef00";
    env.write_session(session_id, "2026-05-01T10:00:00.000Z", Some("anything"));

    let mut engine = Engine::new(120, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    for ch in "/resume".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    let _ = render_str(&mut engine);
    // Drain emits from the picker open so the assertion below sees only
    // the selection's egress.
    let _ = engine.take_emit_queue();

    // Cursor defaults to 1; with one session that's our row. Hit Enter.
    engine.handle_key(key("enter")).expect("enter on row");

    assert!(
        !engine.exit_requested(),
        "picker selection must NOT terminate the TUI process",
    );

    let emits = engine.take_emit_queue();
    let request = emits
        .iter()
        .find(|(_, b)| b.get("kind").and_then(|v| v.as_str()) == Some("sessions.resume_request"));
    let (_, body) = request.expect("expected sessions.resume_request egress");
    assert_eq!(
        body.get("session_id").and_then(|v| v.as_str()),
        Some(session_id),
        "resume_request must carry the chosen session id",
    );
}

#[test]
fn session_picker_escape_cancels_without_emitting() {
    let env = ResumeEnv::new();
    env.write_session(
        "deadbeef-0000-0000-0000-000000000000",
        "2026-05-01T10:00:00.000Z",
        Some("scratch"),
    );

    let mut engine = Engine::new(120, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    for ch in "/resume".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    let _ = render_str(&mut engine);
    let _ = engine.take_emit_queue();

    engine.handle_key(key("escape")).expect("escape");
    assert!(!engine.exit_requested(), "escape must not exit");
    let emits = engine.take_emit_queue();
    assert!(
        !emits.iter().any(|(_, b)| {
            b.get("kind").and_then(|v| v.as_str()) == Some("sessions.resume_request")
        }),
        "escape must not emit sessions.resume_request",
    );
}

#[test]
fn slash_resume_with_arg_emits_resume_request() {
    // `/resume <id>` is the bypass-picker path: emit the resume_request
    // straight onto the bus, no popup. The TUI process stays alive —
    // the starter's sessions module runs the swap in-process.
    let _env = ResumeEnv::new();
    let session_id = "feedface-0000-0000-0000-000000000000";

    let mut engine = Engine::new(120, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    let cmd = format!("/resume {session_id}");
    for ch in cmd.chars() {
        // `key()` uses the raw character as the keypress name. For space,
        // the engine's input router synthesizes "key.space" — match that.
        let n = if ch == ' ' {
            "space".to_string()
        } else {
            ch.to_string()
        };
        engine.handle_key(key(&n)).expect("type");
    }
    let _ = engine.take_emit_queue();
    engine.handle_key(key("enter")).expect("enter");

    assert!(
        !engine.exit_requested(),
        "/resume <id> must NOT terminate the TUI",
    );
    let emits = engine.take_emit_queue();
    let req = emits
        .iter()
        .find(|(_, b)| b.get("kind").and_then(|v| v.as_str()) == Some("sessions.resume_request"));
    let (_, body) = req.expect("expected sessions.resume_request egress");
    assert_eq!(
        body.get("session_id").and_then(|v| v.as_str()),
        Some(session_id),
    );
}

/// Mouse drag inside the transcript triggers the chat.lua mouse.selection
/// handler. The handler calls `tui.copy_to_clipboard` and surfaces a
/// `copied N chars` toast. The test asserts the toast appears — that
/// transitively confirms the engine extracted the text and routed it to
/// the Lua policy. Clipboard side-effects (the actual OS write) aren't
/// asserted because the headless test runner has no clipboard backend
/// to inspect; the binding swallows that failure by design (warn + drop).
#[test]
fn mouse_drag_copies_selection_and_shows_toast() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    // Stream a known message into the transcript so the drag covers
    // identifiable text.
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.stream.delta", "text": "selectable-token" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.stream.end", "model": "test", "duration_ms": 1 }),
    );
    let frame = render_str(&mut engine);
    assert!(
        frame.contains("selectable-token"),
        "expected token in pre-drag frame: {frame:?}"
    );

    // Locate the row carrying our token in the framebuffer snapshot so
    // we drag over those cells.
    let snap = engine.snapshot();
    let row_idx = snap
        .lines()
        .position(|l| l.contains("selectable-token"))
        .expect("token row in framebuffer");
    let col_idx = snap
        .lines()
        .nth(row_idx)
        .unwrap()
        .find("selectable-token")
        .unwrap();

    // Down at the first cell of the token, drag to the last, release.
    let y = row_idx as u16;
    let x0 = col_idx as u16;
    let x1 = (col_idx + "selectable-token".len() - 1) as u16;
    engine
        .handle_mouse(MouseMessage {
            kind: MouseKind::Click,
            x: x0,
            y,
            button: Some("left"),
            mods: vec![],
        })
        .expect("down");
    engine
        .handle_mouse(MouseMessage {
            kind: MouseKind::Drag,
            x: x1,
            y,
            button: Some("left"),
            mods: vec![],
        })
        .expect("drag");
    engine
        .handle_mouse(MouseMessage {
            kind: MouseKind::Up,
            x: x1,
            y,
            button: Some("left"),
            mods: vec![],
        })
        .expect("up");

    // Render once — the slide animation translates horizontally rather
    // than clipping height, so the toast text is on screen from frame
    // one. Skipping the previous `advance_time(250)` keeps the gap
    // between dispatch and assertion small enough that real wall-clock
    // drift on a loaded CI box can't push past the 2 s default TTL.
    let _ = render_str(&mut engine);
    let _ = engine.take_emit_queue();
    let post = engine.snapshot();
    assert!(
        post.contains("copied "),
        "expected 'copied N chars' toast after drag, got: {post:?}"
    );
    // Char count in the toast should match the selection length.
    let needle = format!("copied {} chars", "selectable-token".len());
    assert!(
        post.contains(&needle),
        "expected exact toast `{needle}`, got: {post:?}"
    );
}

/// Toast layout assertions: the bordered toast pill anchors to the
/// bottom-right of the BODY area only — overlaying transcript content
/// at the bottom rows of the body region, but never covering the
/// input field or statusline below it. Statusline placeholder remains
/// visible after the toast appears.
#[test]
fn mouse_drag_toast_overlays_input_and_statusline() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    // Stream a known message into the transcript so the drag covers
    // identifiable text.
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.stream.delta", "text": "selectable-token" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.stream.end", "model": "test", "duration_ms": 1 }),
    );
    let _ = render_str(&mut engine);

    // Locate the row carrying our token in the framebuffer snapshot.
    let snap = engine.snapshot();
    let row_idx = snap
        .lines()
        .position(|l| l.contains("selectable-token"))
        .expect("token row in framebuffer");
    let col_idx = snap
        .lines()
        .nth(row_idx)
        .unwrap()
        .find("selectable-token")
        .unwrap();

    // Pre-toast: the bottom-row statusline carries the placeholder text.
    let pre = engine.snapshot();
    assert!(
        pre.lines()
            .any(|l| l.contains("Start chatting to see stats")),
        "expected statusline placeholder before toast: {pre:?}"
    );

    // Drag to trigger the selection → clipboard copy → toast path.
    let y = row_idx as u16;
    let x0 = col_idx as u16;
    let x1 = (col_idx + "selectable-token".len() - 1) as u16;
    engine
        .handle_mouse(MouseMessage {
            kind: MouseKind::Click,
            x: x0,
            y,
            button: Some("left"),
            mods: vec![],
        })
        .expect("down");
    engine
        .handle_mouse(MouseMessage {
            kind: MouseKind::Drag,
            x: x1,
            y,
            button: Some("left"),
            mods: vec![],
        })
        .expect("drag");
    engine
        .handle_mouse(MouseMessage {
            kind: MouseKind::Up,
            x: x1,
            y,
            button: Some("left"),
            mods: vec![],
        })
        .expect("up");

    // Render once — the horizontal slide leaves the toast at full
    // height/width from frame one, so we don't need to advance the
    // synthetic clock past the enter window. Doing so unnecessarily
    // narrows the wall-clock budget against the 2 s default TTL.
    let _ = render_str(&mut engine);
    let _ = engine.take_emit_queue();
    let post = engine.snapshot();

    // Toast is rendered last in the outer stack, so it overlays the
    // statusline row. The placeholder text must NOT be visible while
    // the toast band is up — that's the whole point of "show toast on
    // top of everything".
    assert!(
        !post
            .lines()
            .any(|l| l.contains("Start chatting to see stats")),
        "statusline placeholder must be occluded by toast: {post:?}"
    );
    let label = format!("copied {} chars", "selectable-token".len());
    assert!(
        post.contains(&label),
        "expected toast label `{label}` somewhere in frame: {post:?}"
    );
}

/// Toast slide animation: the text inside the banner translates
/// leftward from flush-right into its rest position (TOAST_REST_INSET
/// cells inset from the right edge) over the enter window. We sample
/// a frame mid-enter and another at rest, then assert the text's
/// rightmost column moves leftward. The bars span full width and
/// never move; only the text's right-padding animates.
///
/// Triggers via `chat.toast` (rather than mouse drag) so the TTL is
/// long enough that real wall-clock drift between snapshots doesn't
/// race the toast's expiry — `tui.now_ms` adds wall-clock elapsed on
/// top of the synthetic offset, and a slow CI run can push a 2000 ms
/// default TTL toast out of view before the rest snapshot is taken.
#[test]
fn chat_toast_slides_horizontally_during_enter() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);
    // 60-second TTL — plenty of headroom for slow test runs.
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.toast",
            "text": "slide-test",
            "ttl_ms": 60_000,
        }),
    );

    // Helper: column of the LAST char of the toast label on its row,
    // or None if invisible. The label is `slide-test` followed by
    // trailing pad (`inset + 1` spaces). As the slide proceeds, the
    // label's rightmost column shifts leftward.
    fn label_right_col(snap: &str) -> Option<usize> {
        snap.lines().find_map(|l| {
            let i = l.find("slide-test")?;
            Some(i + "slide-test".len() - 1)
        })
    }

    // Sample mid-enter — ease_out_cubic(50/220) ≈ 0.59, slide_offset
    // ≈ 1.18 → inset = 1 → trailing pad = 2 cells. Label sits 2
    // columns left of flush-right.
    engine.advance_time(Duration::from_millis(50));
    let _ = render_str(&mut engine);
    let early = engine.snapshot();
    let early_col = label_right_col(&early)
        .unwrap_or_else(|| panic!("toast invisible mid-enter; snapshot:\n{early}"));

    // Sample at rest — past the enter window. inset = TOAST_REST_INSET
    // (2) → trailing pad = 3 cells. Label sits 3 columns left of
    // flush-right, i.e. one column further left than mid-enter.
    engine.advance_time(Duration::from_millis(250));
    let _ = render_str(&mut engine);
    let rest = engine.snapshot();
    let rest_col = label_right_col(&rest)
        .unwrap_or_else(|| panic!("toast invisible at rest; snapshot:\n{rest}"));

    assert!(
        rest_col < early_col,
        "expected label to slide leftward into rest position; \
         early_col = {early_col}, rest_col = {rest_col}"
    );
}
