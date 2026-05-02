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
    // Statusline placeholder when stats haven't arrived yet.
    assert!(
        out.contains("model:"),
        "statusline missing 'model:': {out:?}"
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
    assert!(
        out.contains("assistant"),
        "assistant role label missing: {out:?}"
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
    assert!(
        out.contains("qwen-test"),
        "statusline missing model: {out:?}"
    );
    assert!(out.contains("in: 11"), "in tokens missing: {out:?}");
    assert!(out.contains("out: 7"), "out tokens missing: {out:?}");
    assert!(out.contains("turns: 1"), "turns missing: {out:?}");
}
