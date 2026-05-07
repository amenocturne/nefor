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
fn slash_new_clears_transcript_and_mints_new_session() {
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
    // `/new` must cancel any in-flight work AND mint a brand-new
    // on-disk session — without the latter, every submit kept landing
    // in the same jsonl no matter how many times the user typed `/new`,
    // so the picker only ever showed one growing entry. The egress is
    // chat.interrupt_all (kills graphs/pending tool calls) +
    // sessions.new_request (the starter's sessions module mints a
    // fresh id and runs end → swap → start in-process).
    assert_eq!(
        emits.len(),
        2,
        "expected interrupt_all + sessions.new_request egress, got {emits:?}"
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
        kinds.contains(&"sessions.new_request"),
        "missing sessions.new_request in {kinds:?}"
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
// lifecycle events on the canonical tool contract:
//   * `graph.run_started { run_id, total_nodes }`
//   * `graph.node.fired   { run_id, node_id, firing_id, reasoner }`
//   * `tool.result        { id, result | error }`
//     — id == firing_id closes one node; id == run_id closes the run.
// The panel is visible by default; Ctrl+B toggles it off. Linger
// handling is pure-update, so a completed run drops on the next event
// after `DAG_LINGER_MS` of engine time has passed — `Engine::advance_time`
// plus a synthetic event drives the prune deterministically without
// sleeping.

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
            "kind": "graph.node.fired",
            "run_id": "run-bbbbbbbb",
            "node_id": "summarise",
            "firing_id": "f-summarise-1",
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

    // Now flip the node to `done` via a tool.result keyed on firing_id.
    // chat.lua's firing→node map (populated by graph.node.fired)
    // resolves the id back to (run_id, node_id).
    dispatch_event(
        &mut engine,
        json!({
            "kind": "tool.result",
            "id": "f-summarise-1",
            "result": { "text": "summary text" },
        }),
    );
    let out = render_str(&mut engine);
    assert!(
        out.contains('✓'),
        "done glyph (✓) missing after tool.result: {out:?}"
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
            "kind": "graph.node.fired",
            "run_id": "run-cccccccc",
            "node_id": "n1",
            "firing_id": "f-n1-1",
            "reasoner": "ollama",
        }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "tool.result",
            "id": "f-n1-1",
            "result": { "text": "ok" },
        }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "tool.result",
            "id": "run-cccccccc",
            "result": { "status": "success", "results": { "n1": { "output": "ok" } } },
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

/// Bug A7 regression: a replayed `chat.model.set_ack` (the original
/// session's provider hello → set_ack, persisted in the jsonl) must
/// NOT clobber the live `state.model` the user set via /model after
/// /new + before /resume. The agentic-loop's live config is the
/// source of truth for which provider serves the next turn; chat.lua
/// mirrors that posture by ignoring set_ack envelopes that arrive
/// inside the replay window. Visible bug: pick mock → /new → /model
/// qwen → /resume an old mock chat → status bar reverts to
/// mock-model even though the next reply still routes through qwen.
#[test]
fn replayed_chat_model_set_ack_does_not_clobber_live_model() {
    let mut engine = Engine::new(120, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Live: user is on `qwen-test`.
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.model.set_ack",
            "provider": "qwen",
            "model": "qwen-test",
        }),
    );
    let out = render_snapshot(&mut engine);
    assert!(out.contains("qwen-test"), "live model missing pre-replay: {out:?}");

    // /resume picker fires: replay window opens, replayed envelopes
    // include the OLD session's mock-provider set_ack.
    dispatch_event(&mut engine, json!({ "kind": "sessions.replay.start" }));
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.model.set_ack",
            "provider": "mock",
            "model": "mock-model",
        }),
    );
    dispatch_event(&mut engine, json!({ "kind": "sessions.replay.end" }));

    let out = render_snapshot(&mut engine);
    assert!(
        out.contains("qwen-test"),
        "live model must survive replayed set_ack (Bug A7): {out:?}"
    );
    assert!(
        !out.contains("mock-model"),
        "replayed set_ack must not clobber live model: {out:?}"
    );
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

/// Bug-B regression: a denied tool call (`chat.tool.end` with
/// `error = true`) flips the expanded tool block to a clearly denied
/// state — `error:` label in red, then the error message — instead
/// of leaving an empty `output:` line that reads as "running...
/// finished but produced nothing". The error message comes through in
/// the `output` field on the chat-side wire (mirroring openai-
/// provider's `chat_tool_end_body` contract); the tool-gate Lua
/// wrapper now preserves it instead of zeroing it on the way through.
#[test]
fn denied_tool_call_renders_error_state_not_empty_output() {
    let mut engine = Engine::new(120, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.tool.start",
            "id": "call_mock_ls",
            "name": "bash",
            "input": { "command": "ls -la" },
        }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.tool.end",
            "id": "call_mock_ls",
            "output": "tool `bash` denied by user",
            "error": true,
        }),
    );

    // Expanded view: clearly labelled as `error:` with the message.
    engine.handle_key(key("ctrl_o")).expect("ctrl_o");
    let _ = render_str(&mut engine);
    let out = engine.snapshot();
    assert!(
        !out.contains("running..."),
        "denied tool block should not show 'running...' (Bug B regression): {out:?}"
    );
    assert!(
        out.contains("error:"),
        "expanded view should label trailing block as `error:` on deny: {out:?}"
    );
    assert!(
        out.contains("denied by user"),
        "expanded view should surface the error message: {out:?}"
    );
    assert!(
        !out.contains("output:"),
        "expanded view should NOT show `output:` label when error is set: {out:?}"
    );
}
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
fn left_column_lifts_input_and_statusline_off_terminal_edges() {
    // No outer padding any more — the sidebar's vertical separator
    // runs full window height edge-to-edge. Per-element spacing now
    // lives inside `left_column`: a 1-row blank above the transcript
    // and a 1-row blank below the statusline so the input + status
    // sit one line off the top and bottom of the chat area without
    // forcing a uniform gutter on the sidebar side too.
    //
    // We assert the chat-side columns (left of the sidebar separator)
    // are blank on the very first and last rows; we do NOT assert the
    // whole row is blank — the sidebar runs flush with the terminal
    // top and bottom, which is the visual the user wants.
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);
    let snap = engine.snapshot();
    let rows: Vec<&str> = snap.lines().collect();

    // Find the sidebar separator column on a mid-screen row to bound
    // the chat area on the left.
    let sample_row = rows.get(rows.len() / 2).expect("mid row");
    let sep_col = sample_row
        .chars()
        .position(|c| c == '│')
        .expect("sidebar separator should be present in the default layout");

    let top = rows.first().expect("top row");
    let bot = rows.last().expect("bottom row");
    let chat_top: String = top.chars().take(sep_col).collect();
    let chat_bot: String = bot.chars().take(sep_col).collect();
    assert!(
        chat_top.chars().all(|c| c == ' '),
        "top row chat-side must be blank: {chat_top:?}"
    );
    assert!(
        chat_bot.chars().all(|c| c == ' '),
        "bottom row chat-side must be blank: {chat_bot:?}"
    );
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
#[ignore = "needs GUI clipboard; arboard suppresses toast on headless CI"]
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
#[ignore = "needs GUI clipboard; arboard suppresses toast on headless CI"]
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

    // Toast is a small pill anchored bottom-right. It overlays the
    // input + statusline area on the right side; the left side of
    // the statusline (where the placeholder text lives) is undisturbed.
    // What matters is that the toast LABEL renders into the bottom
    // few rows — proving it's painted above the input/statusline in
    // z-order, not that it occludes the entire row.
    let label = format!("copied {} chars", "selectable-token".len());
    let bottom_rows: String = post.lines().rev().take(5).collect::<Vec<_>>().join("\n");
    assert!(
        bottom_rows.contains(&label),
        "expected toast label `{label}` in the bottom rows: {bottom_rows:?}"
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

    // Helper: longest leading prefix of "slide-test" found in `snap`.
    // Mid-enter only the first few chars are rendered (the leading
    // characters peek through at the right edge); at rest the full
    // word is visible. So the prefix length grows monotonically as
    // the slide progresses — that's what we assert.
    fn longest_visible_prefix(snap: &str) -> usize {
        let candidate = "slide-test";
        let mut best = 0;
        for prefix_len in 1..=candidate.len() {
            if snap.contains(&candidate[..prefix_len]) {
                best = prefix_len;
            } else {
                break;
            }
        }
        best
    }

    // Sample mid-enter — ease_out_cubic(50/220) ≈ 0.59, total_slide
    // = 12 (10 chars + TOAST_REST_INSET=2), distance_slid ≈ 7. So
    // the first 7 chars of "slide-test" are rendered: "slide-t".
    engine.advance_time(Duration::from_millis(50));
    let _ = render_str(&mut engine);
    let early = engine.snapshot();
    let early_prefix = longest_visible_prefix(&early);
    assert!(
        early_prefix > 0 && early_prefix < "slide-test".len(),
        "expected partial label mid-enter (got prefix len {early_prefix}); snapshot:\n{early}"
    );

    // Sample at rest — past the enter window. distance_slid =
    // total_slide → full label visible.
    engine.advance_time(Duration::from_millis(250));
    let _ = render_str(&mut engine);
    let rest = engine.snapshot();
    let rest_prefix = longest_visible_prefix(&rest);
    assert_eq!(
        rest_prefix,
        "slide-test".len(),
        "expected full label visible at rest; snapshot:\n{rest}"
    );
}

// ──────────────────────────────────────────────────────────────────────────
// Resume / session lifecycle from the TUI's perspective
// ──────────────────────────────────────────────────────────────────────────
//
// These tests pin the chat-side handling of the four control envelopes the
// starter `sessions` module emits — `sessions.session_end`,
// `sessions.session_start`, `sessions.resume_done` (broadcast events) — and
// the orchestrator's `chat.message.append` round-trip echo. The earlier
// tests in this file cover the egress side (`/resume <id>` → emits
// `sessions.resume_request`); these cover the ingress side (the bus
// envelopes flow back into chat.lua).
//
// Why the dedicated section: the resume path has had subtle bugs (transcript
// stayed empty after pick, replayed deltas re-streamed in real time, first
// post-`/new` submit invisible) that only surface when the lifecycle events
// interleave with live keypresses. The chat surface has no Rust-side state
// observable from the test other than (a) what it renders and (b) what it
// emits — the assertions reflect that.

/// `/new` immediately followed by a submit must show the user's text. The
/// orchestrator echoes the submitted text back as `chat.message.append
/// role=user` so it persists + replays; the chat side has a `pending_user_echo`
/// dedup marker so the echo doesn't double-render the live message. The
/// regression: the lifecycle events from `/new` (chat.reset, session_end,
/// session_start, resume_done) used to land BEFORE the echo, and
/// session_end's `entries = {}` clear wiped the locally-pushed user message.
/// Then when the echo arrived, `pending_user_echo` was nil → `push_entry`
/// fires → the message appears. So the order is what matters: this test
/// drives the pessimistic order (lifecycle events arrive AFTER the local
/// submit, then the echo arrives) and asserts the message is visible.
#[test]
fn slash_new_then_submit_shows_user_message_after_lifecycle_round_trip() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // `/new` + Enter — locally clears state, emits chat.interrupt_all +
    // sessions.new_request.
    for ch in "/new".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    let _ = engine.take_emit_queue();
    let _ = render_str(&mut engine);

    // "hello" + Enter immediately — local push of user message,
    // pending_user_echo set to "hello", emits chat.input.submit.
    for ch in "hello".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    let _ = engine.take_emit_queue();
    let _ = render_str(&mut engine);

    // Now the engine catches up: agentic_workflow's session_end teardown
    // broadcasts chat.reset, sessions.lua emits the three lifecycle
    // envelopes, and the chat.input.submit handler emits the echo.
    dispatch_event(&mut engine, json!({ "kind": "chat.reset" }));
    dispatch_event(
        &mut engine,
        json!({ "kind": "sessions.session_end", "session_id": "old-id" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "sessions.session_start", "session_id": "new-id" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "sessions.resume_done", "session_id": "new-id", "replayed": 0 }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.message.append", "role": "user", "text": "hello" }),
    );

    let _ = render_str(&mut engine);
    let out = engine.snapshot();
    assert!(
        out.contains("hello"),
        "user's first post-/new message must remain visible after the \
         lifecycle round-trip; transcript was:\n{out}",
    );
}

/// Live submit (no `/new` preceding) must dedup the echo. Local push +
/// echo round-trip must produce ONE rendered user line, not two. The
/// `pending_user_echo` marker is what enforces this.
#[test]
fn live_submit_dedups_orchestrator_echo() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    for ch in "abc".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    let _ = engine.take_emit_queue();
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.message.append", "role": "user", "text": "abc" }),
    );
    let _ = render_str(&mut engine);
    let out = engine.snapshot();

    // "abc" must appear exactly once. Count the prefix occurrences with
    // some forgiveness for the timestamp / icon column.
    let occurrences = out.matches("abc").count();
    assert_eq!(
        occurrences, 1,
        "expected exactly one rendered user line for 'abc' (dedup against \
         the orchestrator's echo); got {occurrences} in: {out:?}",
    );
}

/// Replay path: between session_start and resume_done, `chat.message.append`
/// envelopes must paint the transcript. This is what makes a `/resume` show
/// the prior conversation. The dedup marker is irrelevant on replay (the
/// chat surface didn't emit anything live), so push_entry fires for every
/// replayed envelope.
#[test]
fn replay_paints_transcript_between_session_start_and_resume_done() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Open the resume cycle.
    dispatch_event(
        &mut engine,
        json!({ "kind": "sessions.session_end", "session_id": "old" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "sessions.session_start", "session_id": "new" }),
    );

    // Replay envelopes — what the engine's replay loop sends to nefor-tui.
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.message.append", "role": "user", "text": "first prompt" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.message.append", "role": "assistant", "text": "first reply" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.message.append", "role": "user", "text": "second prompt" }),
    );

    // Close the cycle.
    dispatch_event(
        &mut engine,
        json!({ "kind": "sessions.resume_done", "session_id": "new", "replayed": 3 }),
    );

    let _ = render_str(&mut engine);
    let out = engine.snapshot();
    for needle in ["first prompt", "first reply", "second prompt"] {
        assert!(
            out.contains(needle),
            "replayed entry {needle:?} missing from transcript:\n{out}",
        );
    }
}

/// `sessions.session_end` deliberately does NOT touch `entries` —
/// the trigger paths (`/new`, `/resume`) own the local transcript
/// clear. Earlier the handler wiped entries here, but that was a
/// race: when the user typed their first prompt in the new session
/// before the bus envelope arrived, the wipe destroyed the
/// locally-pushed message. This test pins the new contract.
#[test]
fn session_end_does_not_wipe_entries() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.message.append", "role": "user", "text": "user-typed-quickly" }),
    );
    let _ = render_str(&mut engine);
    dispatch_event(
        &mut engine,
        json!({ "kind": "sessions.session_end", "session_id": "old" }),
    );
    let _ = render_str(&mut engine);
    let out = engine.snapshot();
    assert!(
        out.contains("user-typed-quickly"),
        "session_end must NOT wipe entries — that was the production \
         race that destroyed the user's first prompt after /new. \
         Transcript:\n{out}",
    );
}

/// Local entry-clear is owned by the trigger paths — `/new`, `/resume`,
/// picker selection. The lifecycle envelopes are NOT responsible for
/// wiping entries (see `session_end_does_not_wipe_entries`). This test
/// pins the picker-selection clear: hitting Enter on a session row
/// emits `sessions.resume_request` AND locally empties `entries` so
/// the imminent replay paints onto a clean slate.
#[test]
fn picker_enter_locally_clears_transcript_before_resume() {
    let env = ResumeEnv::new();
    let target = "deadbeef-aaaa-4bbb-8ccc-000000000001";
    env.write_session(target, "2026-05-04T12:00:00.000Z", Some("seed"));

    let mut engine = Engine::new(120, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Old content from the current session.
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.message.append", "role": "user", "text": "old-content" }),
    );
    let _ = render_str(&mut engine);

    // Open the picker and press Enter on the (only) row.
    for ch in "/resume".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter open");
    let _ = render_str(&mut engine);
    let _ = engine.take_emit_queue();
    engine.handle_key(key("enter")).expect("enter pick");
    let _ = render_str(&mut engine);

    let out = engine.snapshot();
    assert!(
        !out.contains("old-content"),
        "picker Enter must locally clear the transcript so replay \
         paints fresh:\n{out}",
    );
}

/// `/new` must not strand a `pending_user_echo` from the prior turn. If
/// the user submits "abc", presses `/new` before the echo arrives, then
/// types "abc" again as their first post-`/new` submit, the second "abc"
/// must NOT be deduped against the stranded marker — that would silently
/// drop the user's first message in the new session.
#[test]
fn slash_new_clears_pending_user_echo_so_repeated_text_is_not_swallowed() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // First submit — sets pending_user_echo to "abc".
    for ch in "abc".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    let _ = engine.take_emit_queue();
    let _ = render_str(&mut engine);

    // `/new` BEFORE the orchestrator's echo arrives, so the marker is
    // stranded. Then immediately submit the same text again.
    for ch in "/new".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    let _ = engine.take_emit_queue();
    let _ = render_str(&mut engine);

    // Second submit, identical text — different new session, NOT a
    // duplicate. (No echo for the first "abc" was ever delivered.)
    for ch in "abc".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    let _ = engine.take_emit_queue();
    let _ = render_str(&mut engine);

    // Lifecycle catches up + echo arrives for the post-/new submit.
    dispatch_event(&mut engine, json!({ "kind": "chat.reset" }));
    dispatch_event(
        &mut engine,
        json!({ "kind": "sessions.session_end", "session_id": "old" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "sessions.session_start", "session_id": "new" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "sessions.resume_done", "session_id": "new", "replayed": 0 }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.message.append", "role": "user", "text": "abc" }),
    );

    let _ = render_str(&mut engine);
    let out = engine.snapshot();
    assert!(
        out.contains("abc"),
        "the post-/new 'abc' must remain visible — a stranded \
         pending_user_echo from the pre-/new submit must not eat it. \
         Transcript:\n{out}",
    );
}

/// `/new` egress contract: cancels in-flight work AND mints a new on-disk
/// session. Already covered by `slash_new_clears_transcript_and_mints_new_session`
/// at the top of this file; this companion test pins the absence of stale
/// emits — `/new` must NOT emit `sessions.resume_request` (that's the
/// /resume path).
#[test]
fn slash_new_does_not_emit_resume_request() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    for ch in "/new".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    let emits = engine.take_emit_queue();

    let kinds: Vec<_> = emits
        .iter()
        .map(|(_, b)| b.get("kind").and_then(|v| v.as_str()).unwrap_or(""))
        .collect();
    assert!(
        !kinds.contains(&"sessions.resume_request"),
        "/new must not emit sessions.resume_request; got {kinds:?}",
    );
    assert!(
        kinds.contains(&"sessions.new_request"),
        "/new must emit sessions.new_request; got {kinds:?}",
    );
}

/// User flow with prior content, then `/new`, then immediate submit.
/// Mimics the production scenario the user reported: had one session,
/// switched to a new one, typed a prompt, first message didn't display.
/// This drives the optimistic order (session lifecycle ARRIVES BEFORE
/// the user's submit) — the realistic order under interactive typing.
#[test]
fn realistic_new_flow_with_prior_content_displays_first_message() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Prior session content.
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.message.append", "role": "user", "text": "old-prompt" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.message.append", "role": "assistant", "text": "old-reply" }),
    );
    let _ = render_str(&mut engine);

    // `/new` → emits chat.interrupt_all + sessions.new_request.
    for ch in "/new".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    let _ = engine.take_emit_queue();
    let _ = render_str(&mut engine);

    // Lifecycle catches up FIRST (engine is fast → events arrive before
    // the user finishes typing the next prompt).
    dispatch_event(&mut engine, json!({ "kind": "chat.reset" }));
    dispatch_event(
        &mut engine,
        json!({ "kind": "sessions.session_end", "session_id": "old" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "sessions.session_start", "session_id": "new" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "sessions.resume_done", "session_id": "new", "replayed": 0 }),
    );
    let _ = render_str(&mut engine);

    // Old content is gone.
    let mid = engine.snapshot();
    assert!(
        !mid.contains("old-prompt"),
        "old content must be cleared by lifecycle: {mid}"
    );

    // User types first message in fresh session.
    for ch in "fresh-prompt".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    let _ = engine.take_emit_queue();
    let _ = render_str(&mut engine);

    // Orchestrator's echo for the fresh submit.
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.message.append", "role": "user", "text": "fresh-prompt" }),
    );
    let _ = render_str(&mut engine);
    let out = engine.snapshot();
    assert!(
        out.contains("fresh-prompt"),
        "first post-/new submit must render — production bug repro. \
         Transcript:\n{out}",
    );
}

/// Boot-time race: ncp.lua's replay-on-attach delivers `sessions.session_start`
/// (emitted during `sessions.init()`) AFTER nefor-tui finished its handshake.
/// If the user types their first prompt before that envelope lands, the
/// local push is in `entries`. The session_start handler used to wipe
/// `entries = {}` "for cleanliness" — but at boot the transcript is
/// already empty, so the clear only ever destroyed the user's locally-
/// pushed message. The user then saw only the assistant's reply because
/// the orchestrator's chat.message.append echo got deduped against
/// pending_user_echo, so nothing repaints the user line.
#[test]
fn boot_session_start_after_local_submit_keeps_user_message_visible() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // User types FIRST, before the boot session_start arrives.
    for ch in "first-prompt".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    let _ = engine.take_emit_queue();
    let _ = render_str(&mut engine);

    // Now the boot session_start arrives (replay-on-attach).
    dispatch_event(
        &mut engine,
        json!({ "kind": "sessions.session_start", "session_id": "boot" }),
    );

    // Then the orchestrator's echo arrives — it's deduped against the
    // pending_user_echo marker the local submit set.
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.message.append", "role": "user", "text": "first-prompt" }),
    );

    // Assistant streams a reply.
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.stream.delta", "text": "response-token" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.stream.end", "model": "test", "duration_ms": 1 }),
    );

    let _ = render_str(&mut engine);
    let out = engine.snapshot();
    assert!(
        out.contains("first-prompt"),
        "user's first message must remain visible after boot session_start \
         lands; this is the production regression — only the assistant's \
         reply was visible, never the user prompt. Transcript:\n{out}",
    );
    assert!(
        out.contains("response-token"),
        "assistant reply must also be visible:\n{out}"
    );
}

/// Production bug: user submits, orchestrator emits a tool_call right away
/// (no preceding text), the user sees the tool call but NOT their own
/// prompt. Reproduces by: do the local submit (push_entry + set marker),
/// then have a session-lifecycle event wipe `entries` (this is what
/// `sessions.session_end` does — broadcast by `teardown_for_session_end`
/// at the start of `/new` or `/resume`, but also reachable via other
/// races). When the orchestrator's echo arrives, the dedup matches the
/// stranded marker and silently swallows it. Then `chat.tool.start`
/// pushes the tool block. The transcript ends up showing only the tool
/// call.
///
/// Fix: dedup must verify the local push actually landed in entries
/// (tail is a user-role entry with matching text) before suppressing
/// the echo. Otherwise it lets the echo through so the user line is at
/// least visible via the round-trip.
#[test]
fn echo_repaints_user_message_when_local_push_was_wiped_before_echo() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // User submits — local push goes into entries, pending_user_echo set.
    for ch in "summarize-thing".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    let _ = engine.take_emit_queue();
    let _ = render_str(&mut engine);

    // Some lifecycle event wipes entries (simulating a stranded clear —
    // this could be session_end fired late, or any future code path
    // that clears entries while the marker is still set).
    dispatch_event(
        &mut engine,
        json!({ "kind": "sessions.session_end", "session_id": "old" }),
    );
    let _ = render_str(&mut engine);

    // Orchestrator's echo arrives with the SAME text the marker
    // tracks — naive dedup would swallow it.
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.message.append", "role": "user", "text": "summarize-thing" }),
    );
    // Then the tool call paints (the visible artefact of the bug).
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.tool.start",
            "id": "t1",
            "name": "spawn_graph",
            "input": "{}",
        }),
    );
    let _ = render_str(&mut engine);

    let out = engine.snapshot();
    assert!(
        out.contains("summarize-thing"),
        "user prompt must remain visible even when entries was wiped \
         between local push and echo (production bug repro). \
         Transcript:\n{out}",
    );
    assert!(
        out.contains("spawn_graph"),
        "tool call must still render:\n{out}"
    );
}

/// Direct production repro: at boot the first message renders fine, but
/// after `/new` the very first submit's user message disappears while
/// subsequent submits show. This drives the exact sequence the user sees:
/// 1. Boot session, submit message #1, echo deduped, both visible.
/// 2. `/new` → lifecycle cycle.
/// 3. Submit message #2 in the new session.
/// 4. Tool call arrives (no preceding text) — the orchestrator decided
///    to spawn_graph immediately.
/// 5. Assistant streams a final answer.
///
/// At step 5, the user must see message #2 above the tool call, not just
/// the tool call. This pins it.
#[test]
fn first_submit_after_slash_new_renders_user_message_when_tool_call_follows() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Step 1: first session, first submit.
    for ch in "old-prompt".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    let _ = engine.take_emit_queue();
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.message.append", "role": "user", "text": "old-prompt" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.stream.delta", "text": "old-reply" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.stream.end", "model": "test", "duration_ms": 1 }),
    );
    let _ = render_str(&mut engine);
    let first = engine.snapshot();
    assert!(
        first.contains("old-prompt"),
        "boot session must show user message:\n{first}"
    );

    // Step 2: /new fires the lifecycle. Engine broadcasts chat.reset +
    // session_end + session_start + resume_done back.
    for ch in "/new".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    let _ = engine.take_emit_queue();
    dispatch_event(&mut engine, json!({ "kind": "chat.reset" }));
    dispatch_event(
        &mut engine,
        json!({ "kind": "sessions.session_end", "session_id": "boot" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "sessions.session_start", "session_id": "new" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "sessions.resume_done", "session_id": "new", "replayed": 0 }),
    );
    let _ = render_str(&mut engine);
    let mid = engine.snapshot();
    assert!(
        !mid.contains("old-prompt"),
        "old session content must be cleared after /new:\n{mid}"
    );

    // Step 3: submit a tool-call-triggering prompt in the new session.
    for ch in "summarize-things".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    let _ = engine.take_emit_queue();

    // Step 4: orchestrator's echo + immediate tool_call (no preceding
    // text/reasoning — the orchestrator went straight to spawn_graph).
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.message.append", "role": "user", "text": "summarize-things" }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.tool.start",
            "id": "t1",
            "name": "spawn_graph",
            "input": "{}",
        }),
    );

    // Step 5: graph events + final answer.
    dispatch_event(
        &mut engine,
        json!({
            "kind": "graph.run_started",
            "run_id": "r1",
            "total_nodes": 3,
        }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "tool.result",
            "id": "r1",
            "result": { "status": "ok", "results": {} },
        }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.tool.end",
            "id": "t1",
            "output": "ok",
            "error": false,
        }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.stream.delta", "text": "final-answer" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.stream.end", "model": "test", "duration_ms": 1 }),
    );
    let _ = render_str(&mut engine);

    let out = engine.snapshot();
    assert!(
        out.contains("summarize-things"),
        "user's first prompt in the post-/new session must be visible \
         above the tool call. Production bug repro. Transcript:\n{out}",
    );
    assert!(
        out.contains("spawn_graph") || out.contains("final-answer"),
        "tool call or final answer must also be visible:\n{out}"
    );
}

/// Popups must paint an opaque background — transcript text behind the
/// popup box must NOT bleed through the empty rows inside the box. The
/// permission popup is the worst offender because its natural content
/// is short relative to the 50%-height shell, leaving lots of empty
/// dead-space cells that used to render whatever was on the layer
/// below (the transcript). The fix puts a `tui.fill { char = " " }`
/// stack-layer behind the content so every cell inside the box is
/// painted.
#[test]
fn popup_paints_opaque_background_over_transcript() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Seed transcript with a known marker that sits in the area the
    // popup will eventually overlay (centred, 60% × 50%).
    for i in 0..20 {
        dispatch_event(
            &mut engine,
            json!({
                "kind": "chat.message.append",
                "role": "user",
                "text": format!("MARKER-LEAK-LINE-{i}"),
            }),
        );
    }
    let _ = render_str(&mut engine);

    // Open a tool-permission popup (short content; lots of dead space
    // inside the box).
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.tool.permission_request",
            "id": "t1",
            "tool": "spawn_graph",
            "args": {
                "nodes": [
                    {"id": "a", "reasoner": "responder"},
                    {"id": "b", "reasoner": "responder"},
                ],
            },
        }),
    );
    let _ = render_str(&mut engine);

    // Locate the popup's row range. Title row contains
    // "permission requested". Popup top border is one row above
    // (a `╭───...───╮` row). Popup bottom border is the next
    // `╰───...───╯` row after the title.
    let snap = engine.snapshot();
    let lines: Vec<&str> = snap.lines().collect();
    let title_row = lines
        .iter()
        .position(|l| l.contains("permission requested"))
        .expect("popup title row missing — popup didn't render");
    let popup_top = lines[..title_row]
        .iter()
        .rposition(|l| l.contains('╭'))
        .expect("popup top border row missing");
    let popup_bottom = lines[title_row..]
        .iter()
        .position(|l| l.contains('╰'))
        .map(|i| title_row + i)
        .expect("popup bottom border row missing");

    // Identify popup column range from the title row. The popup's
    // outer borders are the LAST `│` to the left of the title text
    // and the FIRST `│` to the right.
    let title_line = lines[title_row];
    let title_byte = title_line
        .find("permission requested")
        .expect("title text in row");
    let left_border = title_line[..title_byte]
        .rfind('│')
        .expect("popup left border on title row");
    let right_border = title_line[title_byte..]
        .find('│')
        .map(|i| title_byte + i)
        .expect("popup right border on title row");

    // Walk every popup INTERIOR row and slice out only the popup's
    // columns. Anything OUTSIDE that slice (transcript bubbles to the
    // left, sidebar to the right) is not a leak — it's other UI.
    // Inside the slice, ANY transcript marker means the popup failed
    // to paint an opaque background.
    for (idx, row) in lines
        .iter()
        .enumerate()
        .take(popup_bottom)
        .skip(popup_top + 1)
    {
        if right_border > left_border + '│'.len_utf8() && row.len() > right_border {
            let interior = &row[left_border + '│'.len_utf8()..right_border];
            assert!(
                !interior.contains("MARKER-LEAK-LINE"),
                "transcript text leaked into popup interior at row {idx}: \
                 {interior:?}\nfull snapshot:\n{snap}",
            );
        }
    }
}

/// `/clear` is an alias for `/new`. Same egress, same lifecycle expectations.
/// Submitting a chat message must re-pin the transcript to the bottom
/// even when the user had scrolled up to read older context. Without
/// this, `stick_to = "end"` only auto-follows new content while
/// `was_at_end` is still true; once the user wheels up, the flag
/// clears and a subsequent submit (Enter) leaves the viewport parked
/// where it was — the user's fresh message + the streaming response
/// render below the visible area until the user scrolls down manually.
/// The submit reducer fires `tui.scroll_into_view("transcript")` so
/// the next paint snaps to the new bottom and re-engages auto-follow
/// for the streaming response that lands after.
#[test]
fn submit_re_pins_transcript_to_bottom_after_user_scrolled_up() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Pump enough content that the transcript has somewhere to scroll
    // away from. Auto-scroll keeps it pinned to the bottom while
    // entries arrive.
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
    fn read_max(engine: &mut Engine, key: &str) -> u16 {
        let lua = engine.lua();
        let chunk = format!(
            r#"
            local p = tui.scroll_position("{key}")
            return p and p.max or -1
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

    let pinned = read_offset(&mut engine, "transcript");
    let max_before = read_max(&mut engine, "transcript");
    assert_eq!(
        pinned, max_before,
        "auto-scroll prereq: transcript should be at bottom after 40 entries"
    );

    // User scrolls up (Up arrow with focused single-line input bubbles
    // to scroll_by("transcript", -1)).
    for _ in 0..5 {
        engine.handle_key(key("up")).expect("up");
    }
    let _ = render_str(&mut engine);
    let after_scroll_up = read_offset(&mut engine, "transcript");
    assert!(
        after_scroll_up < pinned,
        "test prereq: arrow-up should move the transcript away from the bottom"
    );

    // Type + submit. The stick_to = end auto-follow is dormant now
    // because was_at_end is false; the submit reducer must explicitly
    // re-pin via scroll_into_view.
    for ch in "hi".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    let _ = render_str(&mut engine);

    let after_submit = read_offset(&mut engine, "transcript");
    let max_after = read_max(&mut engine, "transcript");
    assert_eq!(
        after_submit, max_after,
        "submit must re-pin transcript to the bottom (offset={after_submit}, max={max_after})"
    );
    // And the new bottom must be past the prior bottom (the user's
    // message added a new row), so we're not just lucking into the
    // pre-submit offset.
    assert!(
        max_after > max_before,
        "user message should have grown content height past max_before={max_before}, got max_after={max_after}"
    );
}

/// Streaming output must NOT yank the user back to the bottom when
/// they've manually scrolled up to read older context (issue #37).
/// `stick_to = "end"` only auto-follows new content while
/// `was_at_end == true`; once the user wheels up the flag clears, and
/// the streaming-delta append path must respect it — content keeps
/// growing in the model, but the viewport stays parked at the user's
/// chosen offset until they explicitly press End / Ctrl+End to re-pin.
#[test]
fn streaming_deltas_do_not_yank_user_back_to_bottom_when_scrolled_up() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Pre-fill enough content that there's somewhere to scroll up.
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
    fn read_max(engine: &mut Engine, key: &str) -> u16 {
        let lua = engine.lua();
        let chunk = format!(
            r#"
            local p = tui.scroll_position("{key}")
            return p and p.max or -1
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

    // Prereq: auto-scroll has us pinned to the bottom.
    let pinned = read_offset(&mut engine, "transcript");
    let max_before_scroll = read_max(&mut engine, "transcript");
    assert_eq!(
        pinned, max_before_scroll,
        "auto-scroll prereq: transcript should be pinned to bottom"
    );

    // User scrolls up off the bottom via arrow-up. The chat input is
    // empty at this point, so chat.lua's key.up handler fires
    // `tui.scroll_by("transcript", -1)` per its arrow-on-empty branch
    // (the engine-level wheel path is exercised separately in
    // `mouse_wheel_up_scrolls_transcript`). Walk a few rows so we have
    // measurable headroom against the streaming content's growth.
    for _ in 0..6 {
        engine.handle_key(key("up")).expect("arrow up");
    }
    let _ = render_str(&mut engine);
    let after_scroll = read_offset(&mut engine, "transcript");
    assert!(
        after_scroll < pinned,
        "test prereq: arrow-up must move the viewport off the bottom \
         (was {pinned}, now {after_scroll})"
    );

    // Now pump 20 streaming deltas — the LLM's response is arriving
    // while the user is scrolled up. The deltas grow content_height
    // (so scroll_y_max grows), but scroll_y must stay parked at
    // after_scroll because was_at_end is false. Without that
    // invariant, `stick_to = "end"`'s auto-follow would yank the
    // user back to the bottom on every delta and they'd never get to
    // read the older context they scrolled up to see (issue #37).
    for _ in 0..20 {
        dispatch_event(
            &mut engine,
            json!({ "kind": "chat.stream.delta", "text": "lorem ipsum dolor sit amet " }),
        );
    }
    let _ = render_str(&mut engine);

    let mid_stream = read_offset(&mut engine, "transcript");
    let max_mid = read_max(&mut engine, "transcript");
    assert!(
        max_mid > max_before_scroll,
        "streaming deltas must grow content_height past the pre-scroll max \
         (was {max_before_scroll}, now {max_mid})"
    );
    assert_eq!(
        mid_stream, after_scroll,
        "streaming deltas must NOT yank the viewport back to the bottom — \
         scroll_y was {after_scroll} when user scrolled up, expected to stay \
         there but is now {mid_stream} (max grew to {max_mid})"
    );

    // Scroll back to bottom via the explicit programmatic path the
    // chat-side `/end` slash command + key.end (when keyboard isn't
    // captured by the input) both use. After this, was_at_end flips
    // back to true and a subsequent delta would auto-follow as before.
    engine
        .lua()
        .load(r#"tui.scroll_into_view("transcript")"#)
        .exec()
        .expect("scroll_into_view");
    // The Lua call only QUEUES the scroll command on the host's
    // pending list — it doesn't dispatch through the engine. Drive a
    // dispatch_msg with a no-op kind so engine.dispatch_msg drains the
    // queue via take_scroll_commands, the same way a real Lua-side
    // tui.scroll_into_view() inside an `update` reducer would.
    let drain = engine.lua().create_table().expect("table");
    drain.set("kind", "noop").expect("kind");
    engine.dispatch_msg(drain).expect("drain");
    let _ = render_str(&mut engine);
    let after_repin = read_offset(&mut engine, "transcript");
    let max_after_repin = read_max(&mut engine, "transcript");
    assert_eq!(
        after_repin, max_after_repin,
        "tui.scroll_into_view must snap viewport back to bottom \
         (offset={after_repin}, max={max_after_repin})"
    );
}

#[test]
fn slash_clear_is_alias_for_slash_new() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    for ch in "/clear".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    let emits = engine.take_emit_queue();

    let kinds: Vec<_> = emits
        .iter()
        .map(|(_, b)| b.get("kind").and_then(|v| v.as_str()).unwrap_or(""))
        .collect();
    assert!(
        kinds.contains(&"chat.interrupt_all") && kinds.contains(&"sessions.new_request"),
        "/clear must emit interrupt_all + new_request like /new; got {kinds:?}",
    );
}
