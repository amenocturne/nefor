//! Phase-6 integration test for the chat surface as a Lua composition.
//!
//! Loads `examples/nefor-agent/chat.lua` into the in-process engine and verifies the
//! must-have wire path: a conversation-manager projection from a peer lands in the
//! transcript, an `input.submit` produces a `chat.input.submit` egress
//! envelope, and `/quit` exits.
//!
//! In-process per the same pattern as `engine_test.rs` — no spawned
//! subprocess, no /dev/tty — so the test stays fast and CI-portable.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use nefor_tui::engine::Engine;
use nefor_tui::input::KeyMessage;
use nefor_tui::mouse::{MouseKind, MouseMessage};
use serde_json::{json, Map as JsonMap, Value as JsonValue};

/// Per-process tempdir kept alive for the lifetime of `cargo test` and
/// pointed at by `NEFOR_DATA_DIR` on first access. Ensures chat.lua's
/// `load_input_history` (issue #39) reads from / writes to a clean
/// throwaway path instead of the developer's `$HOME/.local/share/
/// nefor/input-history` — without this, parallel test runs would
/// pollute (and read pre-existing entries from) the user's real
/// shell-style history file.
///
/// **Shared-state caveat**: this dir is shared across all parallel
/// tests. Tests that submit text write to the input-history file here,
/// so any test that depends on empty prompt_history (e.g. arrow-up
/// routing to scroll instead of history-recall) MUST use `ResumeEnv`
/// to get its own isolated tempdir.
///
/// Per-test ResumeEnv overrides this default for tests that need a
/// per-test isolated data dir (e.g. session-picker, input-history
/// regression tests, scroll tests that press arrow-up); they restore
/// back to whatever was set before (which is this process-wide tempdir)
/// on Drop.
static TEST_DATA_HOME: OnceLock<tempfile::TempDir> = OnceLock::new();

fn ensure_test_data_home() {
    let dir = TEST_DATA_HOME
        .get_or_init(|| tempfile::tempdir().expect("create per-process test data home"));
    // set_var is process-global; OnceLock guarantees the assignment
    // runs only once. Subsequent ResumeEnv-style overrides save +
    // restore around their scope, so this default is what they read
    // at construction time and what they restore on Drop.
    if std::env::var_os("NEFOR_DATA_DIR").is_none() {
        std::env::set_var("NEFOR_DATA_DIR", dir.path());
    }
}

fn chat_lua_source() -> String {
    // Side-effect on first call: install a per-process data home so
    // chat.lua's input-history loader doesn't reach into the user's
    // real `$HOME/.local/share/nefor`. Centralised here because every
    // chat-surface test reads this function — no per-test wiring.
    ensure_test_data_home();
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("repo root")
        .to_path_buf();
    // Pin the config the chat surface resolves so the suite is
    // hermetic: NEFOR_STARTER_CONFIG_DIR beats a NEFOR_CONFIG_DIR /
    // NEFOR_DEV_DIR exported in the developer's shell (which would
    // leak the real installed config in), and NEFOR_DEFAULT_* beat
    // examples/nefor-agent/config's developer-facing chatgpt/gpt-5.5 defaults.
    // Every initial-statusline assertion keys on these values.
    // Unconditional set_var (unlike the only-if-unset pins below):
    // an inherited value IS the leak being defended against. Once so
    // parallel test threads don't race the process-global env.
    static PIN_CONFIG_ENV: OnceLock<()> = OnceLock::new();
    PIN_CONFIG_ENV.get_or_init(|| {
        std::env::set_var(
            "NEFOR_STARTER_CONFIG_DIR",
            repo_root.join("examples/nefor-agent"),
        );
        std::env::set_var("NEFOR_DEFAULT_PROVIDER", "mock-plugin");
        std::env::set_var("NEFOR_DEFAULT_MODEL", "mock-model");
    });
    // Tell chat.lua's package.path bootstrap where the nefor-tui plugin
    // lib lives. In a normal `nefor` run, the engine sets NEFOR_CONFIG_DIR
    // and chat.lua derives the plugin-lib dir relative to that; tests
    // load chat.lua directly into the engine's Lua VM (no env from the
    // engine entry point) so we set the explicit override here.
    let plugin_lua = repo_root.join("plugins").join("nefor-tui").join("lua");
    if std::env::var_os("NEFOR_TUI_LUA_DIR").is_none() {
        std::env::set_var("NEFOR_TUI_LUA_DIR", &plugin_lua);
    }
    // Tell chat.lua's package.path bootstrap where the chat/ submodule
    // dir lives. Same rationale as NEFOR_TUI_LUA_DIR above — tests
    // load chat.lua directly into the engine VM with no NEFOR_CONFIG_DIR
    // (which is how the binary normally seeds this path).
    let chat_subdir = repo_root.join("examples/nefor-agent").join("chat");
    if std::env::var_os("NEFOR_STARTER_CHAT_DIR").is_none() {
        std::env::set_var("NEFOR_STARTER_CHAT_DIR", &chat_subdir);
    }
    let chat_path = repo_root
        .join("examples/nefor-agent")
        .join("chat")
        .join("init.lua");
    std::fs::read_to_string(&chat_path).unwrap_or_else(|e| panic!("read {:?}: {e}", chat_path))
}

fn canonical_chat_lua_source_for_config(config_dir: &std::path::Path) -> String {
    ensure_test_data_home();
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("repo root")
        .to_path_buf();
    let source = std::fs::read_to_string(repo_root.join("examples/nefor-agent/chat/init.lua"))
        .expect("read canonical chat entry");
    format!(
        r#"
        local real_getenv = os.getenv
        local overrides = {{
          NEFOR_CONFIG_DIR = {config:?},
          NEFOR_STARTER_CONFIG_DIR = {config:?},
          NEFOR_STARTER_CHAT_DIR = {chat:?},
          NEFOR_LOCAL_DIR = {repo:?},
          NEFOR_TUI_LUA_DIR = {tui:?},
          NEFOR_LUA_DIR = {lua:?},
          NEFOR_DEFAULT_PROVIDER = "mock-plugin",
          NEFOR_DEFAULT_MODEL = "mock-model",
        }}
        os.getenv = function(name)
          if overrides[name] ~= nil then return overrides[name] end
          return real_getenv(name)
        end
        os.execute = function() return true end
        {source}
        local config_ok, config_error = pcall(function() return require("config").active end)
        assert(config_ok, tostring(config_error))
        "#,
        config = config_dir.display().to_string(),
        chat = repo_root
            .join("examples/nefor-agent/chat")
            .display()
            .to_string(),
        repo = repo_root.display().to_string(),
        tui = repo_root
            .join("plugins/nefor-tui/lua")
            .display()
            .to_string(),
        lua = repo_root.join("lua").display().to_string(),
    )
}

fn render_str(engine: &mut Engine) -> String {
    match engine.render_if_dirty().expect("render") {
        Some(bytes) => String::from_utf8(bytes).expect("ansi is utf-8"),
        // Render-was-clean is fine for assertions that only care about
        // egress / state shape; the prior frame is on the wire already.
        None => String::new(),
    }
}

fn dispatch_event_from(engine: &mut Engine, source: &str, body: JsonValue) {
    let map: JsonMap<String, JsonValue> = body.as_object().expect("event body").clone();
    engine
        .dispatch_envelope_from(&map, source)
        .expect("dispatch event");
}

fn dispatch_event(engine: &mut Engine, body: JsonValue) {
    let source = match body.get("kind").and_then(JsonValue::as_str) {
        Some("mag.run_started") => "mag",
        Some("chat.instruction.notice") => "engine",
        _ => "test",
    };
    dispatch_event_from(engine, source, body);
}

fn activate_conversation(engine: &mut Engine, conversation_id: &str) {
    dispatch_event(
        engine,
        json!({ "kind": "conversation.active.changed", "conversation_id": conversation_id }),
    );
}

#[derive(Clone)]
struct FixtureTurn {
    conversation_id: String,
    turn_id: String,
    message_id: String,
}

#[derive(Default)]
struct FixtureConversation {
    sequence: u64,
    current: Option<FixtureTurn>,
}

thread_local! {
    static FIXTURE_CONVERSATIONS: RefCell<HashMap<usize, FixtureConversation>> =
        RefCell::new(HashMap::new());
}

fn fixture_key(engine: &mut Engine) -> usize {
    engine as *mut Engine as usize
}

fn fixture_turn(engine: &mut Engine) -> FixtureTurn {
    let key = fixture_key(engine);
    let turn = FIXTURE_CONVERSATIONS.with(|fixtures| {
        let mut fixtures = fixtures.borrow_mut();
        let fixture = fixtures.entry(key).or_default();
        if fixture.current.is_none() {
            fixture.sequence += 1;
            let suffix = fixture.sequence;
            fixture.current = Some(FixtureTurn {
                conversation_id: format!("fixture-conversation-{key}"),
                turn_id: format!("fixture-turn-{key}-{suffix}"),
                message_id: format!("fixture-assistant-{key}-{suffix}"),
            });
        }
        fixture.current.clone().expect("fixture turn")
    });
    activate_conversation(engine, &turn.conversation_id);
    dispatch_event(
        engine,
        json!({
            "kind": "conversation.projection.delta", "conversation_id": turn.conversation_id,
            "change": { "kind": "turn_started", "turn_id": turn.turn_id, "run_id": turn.turn_id }
        }),
    );
    dispatch_event(
        engine,
        json!({
            "kind": "conversation.projection.delta", "conversation_id": turn.conversation_id,
            "change": { "kind": "message_started", "turn_id": turn.turn_id,
                "message": { "id": turn.message_id, "turn_id": turn.turn_id, "role": "assistant" } }
        }),
    );
    turn
}

fn fixture_assistant_delta(engine: &mut Engine, text: impl Into<String>) {
    let text = text.into();
    let turn = fixture_turn(engine);
    dispatch_event(
        engine,
        json!({
            "kind": "conversation.projection.delta", "conversation_id": turn.conversation_id,
            "change": { "kind": "content_chunk_appended", "turn_id": turn.turn_id,
                "message_id": turn.message_id, "chunk": { "kind": "text", "data": text } }
        }),
    );
}

fn fixture_reasoning_delta(engine: &mut Engine, text: impl Into<String>) {
    let text = text.into();
    let turn = fixture_turn(engine);
    dispatch_event(
        engine,
        json!({
            "kind": "conversation.projection.delta", "conversation_id": turn.conversation_id,
            "change": { "kind": "content_chunk_appended", "turn_id": turn.turn_id,
                "message_id": turn.message_id, "chunk": { "kind": "reasoning", "data": text } }
        }),
    );
}

fn fixture_assistant_completed(engine: &mut Engine, text: Option<String>, terminal: JsonValue) {
    let turn = fixture_turn(engine);
    let text = text.unwrap_or_default();
    dispatch_event(
        engine,
        json!({
            "kind": "conversation.projection.delta", "conversation_id": turn.conversation_id,
            "change": { "kind": "message_completed", "turn_id": turn.turn_id,
                "message": { "id": turn.message_id, "turn_id": turn.turn_id, "role": "assistant",
                    "text": text, "terminal": terminal.clone() } }
        }),
    );
    dispatch_event(
        engine,
        json!({
            "kind": "conversation.projection.delta", "conversation_id": turn.conversation_id,
            "change": { "kind": "turn_completed", "turn_id": turn.turn_id,
                "run_id": turn.turn_id, "terminal": terminal }
        }),
    );
    let key = fixture_key(engine);
    FIXTURE_CONVERSATIONS.with(|fixtures| {
        let mut fixtures = fixtures.borrow_mut();
        let fixture = fixtures.entry(key).or_default();
        fixture.current = None;
    });
}

fn fixture_message(engine: &mut Engine, role: &str, text: impl Into<String>) {
    let text = text.into();
    let key = fixture_key(engine);
    let (conversation_id, message_id) = FIXTURE_CONVERSATIONS.with(|fixtures| {
        let mut fixtures = fixtures.borrow_mut();
        let fixture = fixtures.entry(key).or_default();
        fixture.sequence += 1;
        (
            format!("fixture-conversation-{key}"),
            format!("fixture-message-{key}-{}", fixture.sequence),
        )
    });
    activate_conversation(engine, &conversation_id);
    dispatch_event(
        engine,
        json!({
            "kind": "conversation.projection.delta", "conversation_id": conversation_id,
            "change": { "kind": "message_started",
                "message": { "id": message_id, "role": role } }
        }),
    );
    dispatch_event(
        engine,
        json!({
            "kind": "conversation.projection.delta", "conversation_id": conversation_id,
            "change": { "kind": "content_chunk_appended", "message_id": message_id,
                "chunk": { "kind": "text", "data": text } }
        }),
    );
    dispatch_event(
        engine,
        json!({
            "kind": "conversation.projection.delta", "conversation_id": conversation_id,
            "change": { "kind": "message_completed",
                "message": { "id": message_id, "role": role, "text": text } }
        }),
    );
}

fn fixture_tool_started(engine: &mut Engine, id: &str, name: &str, input: JsonValue) {
    let turn = fixture_turn(engine);
    dispatch_event(
        engine,
        json!({
            "kind": "conversation.projection.delta", "conversation_id": turn.conversation_id,
            "change": { "kind": "tool_call_completed", "turn_id": turn.turn_id,
                "exchange": { "id": id, "name": name, "status": "call_completed",
                    "arguments": input } }
        }),
    );
}

fn fixture_tool_completed(engine: &mut Engine, id: &str, output: JsonValue, error: bool) {
    let turn = fixture_turn(engine);
    let kind = if error {
        "tool_error_recorded"
    } else {
        "tool_result_recorded"
    };
    dispatch_event(
        engine,
        json!({
            "kind": "conversation.projection.delta", "conversation_id": turn.conversation_id,
            "change": { "kind": kind, "turn_id": turn.turn_id,
                "exchange": { "id": id, "status": if error { "error" } else { "result" },
                    "result": output, "error": if error { output } else { JsonValue::Null } } }
        }),
    );
}

fn fixture_compaction(engine: &mut Engine, status: &str, fields: JsonValue) {
    let key = fixture_key(engine);
    let conversation_id = format!("fixture-conversation-{key}");
    activate_conversation(engine, &conversation_id);
    dispatch_event(
        engine,
        json!({
            "kind": "conversation.projection.delta", "conversation_id": conversation_id,
            "change": { "kind": format!("context_compaction_{status}"), "compaction": fields }
        }),
    );
}

fn append_canonical_message(
    engine: &mut Engine,
    conversation_id: &str,
    message_id: &str,
    role: &str,
    text: &str,
) {
    dispatch_event(
        engine,
        json!({
            "kind": "conversation.projection.delta", "conversation_id": conversation_id,
            "change": { "kind": "message_started",
                "message": { "id": message_id, "role": role } }
        }),
    );
    dispatch_event(
        engine,
        json!({
            "kind": "conversation.projection.delta", "conversation_id": conversation_id,
            "change": { "kind": "content_chunk_appended", "message_id": message_id,
                "chunk": { "kind": "text", "data": text } }
        }),
    );
    dispatch_event(
        engine,
        json!({
            "kind": "conversation.projection.delta", "conversation_id": conversation_id,
            "change": { "kind": "message_completed",
                "message": { "id": message_id, "role": role, "text": text } }
        }),
    );
}

fn append_canonical_assistant_terminal(
    engine: &mut Engine,
    conversation_id: &str,
    turn_id: &str,
    text: &str,
    terminal_kind: &str,
    terminal: JsonValue,
) {
    let message_id = format!("{turn_id}-assistant");
    dispatch_event(
        engine,
        json!({
            "kind": "conversation.projection.delta", "conversation_id": conversation_id,
            "change": { "kind": "turn_started", "turn_id": turn_id, "run_id": turn_id }
        }),
    );
    dispatch_event(
        engine,
        json!({
            "kind": "conversation.projection.delta", "conversation_id": conversation_id,
            "change": { "kind": "message_started", "turn_id": turn_id,
                "message": { "id": message_id, "turn_id": turn_id, "role": "assistant" } }
        }),
    );
    if !text.is_empty() {
        dispatch_event(
            engine,
            json!({
                "kind": "conversation.projection.delta", "conversation_id": conversation_id,
                "change": { "kind": "content_chunk_appended", "message_id": message_id,
                    "chunk": { "kind": "text", "data": text } }
            }),
        );
    }
    dispatch_event(
        engine,
        json!({
            "kind": "conversation.projection.delta", "conversation_id": conversation_id,
            "change": { "kind": "message_completed", "turn_id": turn_id,
                "message": { "id": message_id, "turn_id": turn_id, "role": "assistant",
                    "text": text, "terminal": terminal.clone() } }
        }),
    );
    dispatch_event(
        engine,
        json!({
            "kind": "conversation.projection.delta", "conversation_id": conversation_id,
            "change": { "kind": terminal_kind, "turn_id": turn_id,
                "run_id": turn_id, "terminal": terminal }
        }),
    );
}

fn append_canonical_assistant_turn(
    engine: &mut Engine,
    conversation_id: &str,
    turn_id: &str,
    text: &str,
    terminal: JsonValue,
) {
    append_canonical_assistant_terminal(
        engine,
        conversation_id,
        turn_id,
        text,
        "turn_completed",
        terminal,
    );
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
    assert!(
        out.contains("mock-model"),
        "initial statusline should show configured default model: {out:?}"
    );
    assert!(
        !out.contains("Start chatting to see stats"),
        "configured defaults should replace pre-chat placeholder: {out:?}"
    );
    // The input field should NOT carry a default hint — the bordered
    // box below the transcript is self-explanatory. Substrings from the
    // removed hint must be absent.
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
fn model_list_keeps_context_widget_hidden_until_usage_is_known() {
    let mut engine = Engine::new(120, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");

    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.models.listed",
            "provider": "mock-plugin",
            "models": ["mock-model"],
            "context_windows": { "mock-model": 128000 },
        }),
    );

    let out = render_str(&mut engine);
    assert!(
        out.contains("mock-model"),
        "initial statusline should keep configured model: {out:?}"
    );
    assert!(
        !out.contains("ctx "),
        "context capacity alone must not masquerade as context used: {out:?}"
    );
}

#[test]
fn completed_turn_restores_context_used_widget() {
    let mut engine = Engine::new(120, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.models.listed",
            "provider": "mock-plugin",
            "models": ["mock-model"],
            "context_windows": { "mock-model": 128000 },
        }),
    );

    fixture_assistant_completed(
        &mut engine,
        Some("answer".into()),
        json!({
            "model": "mock-model",
            "usage": { "input_tokens": 80_000, "output_tokens": 7 }
        }),
    );

    let out = render_str(&mut engine);
    assert!(
        out.contains("ctx 80k/128k") && out.contains("63%"),
        "completed-turn input usage should render the context-used widget: {out:?}"
    );
}

#[test]
fn slash_new_clears_session_owned_context_usage() {
    let mut engine = Engine::new(120, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.models.listed",
            "provider": "mock-plugin",
            "models": ["mock-model"],
            "context_windows": { "mock-model": 128000 },
        }),
    );
    fixture_assistant_completed(
        &mut engine,
        Some("answer".into()),
        json!({
            "model": "mock-model",
            "usage": { "input_tokens": 80_000, "output_tokens": 7 }
        }),
    );
    assert!(render_str(&mut engine).contains("ctx 80k/128k"));

    dispatch_event(
        &mut engine,
        json!({ "kind": "input.submit", "value": "/new" }),
    );
    let out = render_str(&mut engine);
    assert!(
        !out.contains("ctx "),
        "a fresh session must not display the previous session's context usage: {out:?}"
    );
}

#[test]
fn slash_new_during_cooperative_resume_clears_loading_state() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    dispatch_event(
        &mut engine,
        json!({ "kind": "sessions.resume_loading", "session_id": "resume-1" }),
    );
    assert!(render_str(&mut engine).contains("loading session"));

    dispatch_event(
        &mut engine,
        json!({ "kind": "input.submit", "value": "/new" }),
    );
    let out = render_str(&mut engine);
    assert!(
        !out.contains("loading session"),
        "/new must cancel the local loading presentation without waiting for resume_done: {out:?}"
    );

    assert!(
        out.contains('╭'),
        "the normal prompt must return immediately after /new: {out:?}"
    );
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
        "examples/nefor-agent/chat.lua should not set a placeholder on the input"
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

// Batch-protocol Phase B regression — N replayed envelopes coalesce
// into ONE render pass. Before batching, every projected content chunk
// rode the dispatch loop independently and triggered its own render
// (visible re-streaming on /resume of a long chat). Post batch-protocol
// refactor + main.rs drain-before-paint: a burst of envelopes lands as
// dispatch_envelope_body calls back-to-back without an intervening
// render, then a single render_if_dirty paints the full transcript.
//
// The test asserts the engine-level invariant the main.rs drain
// depends on: state mutations from N envelopes accumulate into the
// reconciler tree, and a single render captures all of them. Without
// this guarantee, the main.rs optimization would silently swallow
// deltas (or render incorrect intermediate state).
#[test]
fn batched_stream_deltas_render_in_a_single_pass() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Push 200 deltas back-to-back without rendering between them.
    let n: usize = 200;
    for i in 0..n {
        fixture_assistant_delta(&mut engine, format!("d{i:03} "));
    }
    fixture_assistant_completed(
        &mut engine,
        None,
        json!({ "model": "qwen-test", "duration_ms": 42 }),
    );

    // Single render — the final transcript must carry the latest
    // deltas from the batch (the transcript scrolls so earlier deltas
    // are off-screen, but the LAST deltas at the bottom are the
    // visible signal that the batch was accumulated end-to-end). This
    // pins the invariant that powers main.rs's drain-before-paint
    // optimization on /resume.
    let out = render_str(&mut engine);
    for needle in ["d199", "d198", "d197"] {
        assert!(
            out.contains(needle),
            "expected coalesced render to carry {needle:?}; got: {out:?}"
        );
    }

    // A second render against the same state must be clean — no
    // remaining dirty flag, no extra paint pass for the same content.
    // This is the "render once, not N times" half of the invariant.
    let none = engine
        .render_if_dirty()
        .expect("second render call must succeed");
    assert!(
        none.is_none(),
        "render_if_dirty must return None after the batch was painted"
    );
}

#[test]
fn resume_loading_is_immediate_monotonic_and_clears_only_when_done() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({ "kind": "sessions.resume_loading", "session_id": "resume-1" }),
    );
    let initial = render_str(&mut engine);
    assert!(
        initial.contains("loading session"),
        "loading state must paint immediately: {initial:?}"
    );
    assert!(
        initial.contains("rebuilding session"),
        "input must read as unavailable while rebuilding: {initial:?}"
    );

    dispatch_event(
        &mut engine,
        json!({ "kind": "sessions.replay.start", "session_id": "resume-1", "count": 200 }),
    );
    fixture_message(&mut engine, "user", "concealed replay fixture");
    dispatch_event(
        &mut engine,
        json!({ "kind": "sessions.replay.progress", "session_id": "resume-1", "replayed": 64, "total": 200 }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "sessions.replay.progress", "session_id": "resume-1", "replayed": 32, "total": 200 }),
    );
    let progress = render_str(&mut engine);
    assert!(
        progress.contains("bytes 64 B / 200 B") && progress.contains("32.0"),
        "byte progress must be labeled, human-readable, and monotonic: {progress:?}"
    );
    assert!(
        !progress.contains("concealed replay fixture"),
        "replayed history must remain concealed while progress paints: {progress:?}"
    );

    dispatch_event(
        &mut engine,
        json!({ "kind": "sessions.replay.end", "session_id": "resume-1" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "sessions.replay.progress", "session_id": "resume-1", "replayed": 96, "total": 200 }),
    );
    let draining = render_str(&mut engine);
    assert!(
        draining.contains("bytes 96 B / 200 B") && draining.contains("48.0"),
        "loading must survive replay end while the TUI drain completes: {draining:?}"
    );

    dispatch_event(
        &mut engine,
        json!({ "kind": "sessions.resume_done", "session_id": "resume-1", "replayed": 200 }),
    );
    let done = render_str(&mut engine);
    assert!(
        !done.contains("loading session"),
        "resume_done must clear loading: {done:?}"
    );
    assert!(
        !done.contains("rebuilding session"),
        "resume_done must restore normal input: {done:?}"
    );
    assert!(
        done.contains("concealed replay fixture"),
        "resume_done must reveal the transcript reconstructed during loading: {done:?}"
    );
}

#[test]
fn resume_loading_formats_byte_boundaries_and_clamps_progress() {
    let cases = [
        (0_i64, 0_i64, "bytes 0 B / 0 B · 0.0%"),
        (50, -10, "bytes 0 B / 0 B · 0.0%"),
        (1023, 1023, "bytes 1023 B / 1023 B · 100.0%"),
        (1024, 2048, "bytes 1.0 KiB / 2.0 KiB · 50.0%"),
        (
            1_048_575,
            1_048_575,
            "bytes 1024.0 KiB / 1024.0 KiB · 100.0%",
        ),
        (3000, 2048, "bytes 2.0 KiB / 2.0 KiB · 100.0%"),
        (1_048_576, 2_097_152, "bytes 1.0 MiB / 2.0 MiB · 50.0%"),
        (
            1_073_741_823,
            1_073_741_823,
            "bytes 1024.0 MiB / 1024.0 MiB · 100.0%",
        ),
        (
            1_073_741_824,
            2_147_483_648,
            "bytes 1.0 GiB / 2.0 GiB · 50.0%",
        ),
    ];

    for (replayed, total, expected) in cases {
        let mut engine = Engine::new(100, 24).expect("engine");
        engine.load_scenario(&chat_lua_source()).expect("load");
        let _ = render_str(&mut engine);
        dispatch_event(
            &mut engine,
            json!({ "kind": "sessions.resume_loading", "session_id": "resume-1" }),
        );
        dispatch_event(
            &mut engine,
            json!({
                "kind": "sessions.replay.progress",
                "session_id": "resume-1",
                "replayed": replayed,
                "total": total,
            }),
        );
        let rendered = render_str(&mut engine);
        assert!(
            rendered.contains(expected.trim_end_matches('%')),
            "expected {expected:?} for {replayed}/{total}: {rendered:?}"
        );
    }
}

#[test]
fn conversation_projection_appends_to_transcript_with_terminal_metadata() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({ "kind": "conversation.active.changed", "conversation_id": "root" }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "conversation.projection.delta",
            "conversation_id": "root",
            "change": { "kind": "turn_started", "turn_id": "turn-1", "run_id": "run-1" },
        }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "conversation.projection.delta",
            "conversation_id": "root",
            "change": {
                "kind": "message_started", "turn_id": "turn-1",
                "message": { "id": "assistant", "turn_id": "turn-1", "role": "assistant" },
            },
        }),
    );
    for text in ["hello ", "world"] {
        dispatch_event(
            &mut engine,
            json!({
                "kind": "conversation.projection.delta",
                "conversation_id": "root",
                "change": {
                    "kind": "content_chunk_appended", "message_id": "assistant",
                    "chunk": { "kind": "text", "data": text },
                },
            }),
        );
    }
    dispatch_event(
        &mut engine,
        json!({
            "kind": "conversation.projection.delta",
            "conversation_id": "root",
            "change": {
                "kind": "message_completed", "turn_id": "turn-1",
                "message": {
                    "id": "assistant", "turn_id": "turn-1", "role": "assistant",
                    "text": "hello world", "terminal": {},
                },
            },
        }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "conversation.projection.delta",
            "conversation_id": "root",
            "change": {
                "kind": "turn_completed", "turn_id": "turn-1", "run_id": "run-1",
                "terminal": {
                    "model": "qwen-test", "duration_ms": 42,
                    "usage": { "input_tokens": 5, "output_tokens": 2 },
                },
            },
        }),
    );

    let out = render_str(&mut engine);
    assert!(
        out.contains("hello world"),
        "concatenated deltas missing from transcript: {out:?}"
    );
    assert_eq!(
        out.matches("hello world").count(),
        1,
        "message and turn completion must not append the answer twice: {out:?}"
    );
    // Assistant entries have NO role label — the visual
    // cue is the absence of the user block's left bar. The per-turn
    // footer marker `▣` + model name is the assistant signature.
    assert!(
        out.contains("▣ qwen-test · 42ms · 48 tok/s"),
        "manager terminal metadata should reach one canonical turn footer: {out:?}"
    );
}

#[test]
fn canonical_system_messages_remain_context_only_in_chat_projection() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({ "kind": "conversation.active.changed", "conversation_id": "root" }),
    );
    for (id, role, text) in [
        ("system", "system", "SYSTEM_CONTEXT_MUST_STAY_HIDDEN"),
        ("user", "user", "visible user message"),
    ] {
        dispatch_event(
            &mut engine,
            json!({
                "kind": "conversation.projection.delta",
                "conversation_id": "root",
                "change": {
                    "kind": "message_completed",
                    "message": { "id": id, "role": role, "text": text },
                },
            }),
        );
    }

    let out = render_str(&mut engine);
    assert!(
        out.contains("visible user message"),
        "user projection missing: {out:?}"
    );
    assert!(
        !out.contains("SYSTEM_CONTEXT_MUST_STAY_HIDDEN"),
        "system context leaked into the visible transcript: {out:?}"
    );
}

#[test]
fn structured_answers_keep_provider_order_and_footer_across_graph_status() {
    let mut engine = Engine::new(120, 40).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    for (answer, duration_ms, output_tokens) in [
        ("answer before status", 2_000, 40),
        ("answer after status", 3_000, 60),
    ] {
        fixture_reasoning_delta(&mut engine, "provider reasoning");
        fixture_assistant_completed(
            &mut engine,
            Some(answer.into()),
            json!({
                "model": "gpt-test", "duration_ms": duration_ms,
                "usage": { "output_tokens": output_tokens }
            }),
        );
        if answer == "answer before status" {
            dispatch_event(
                &mut engine,
                json!({
                    "kind": "chat.graph_result.append",
                    "run_id": "mag-run-1",
                    "run_name": "eval-1",
                    "status": "failed",
                    "duration_ms": 500,
                    "error": "killed",
                }),
            );
        }
    }

    let out = render_str(&mut engine);
    let before = out.find("answer before status").expect("first answer");
    let status = out.find("mag workflow").expect("graph status");
    let after = out.find("answer after status").expect("second answer");
    assert!(
        before < status && status < after,
        "provider chronology changed:\n{out}"
    );
    assert_eq!(
        out.matches("▣ gpt-test").count(),
        2,
        "each structured answer must keep its model footer:\n{out}"
    );
    assert_eq!(
        out.matches("20 tok/s").count(),
        2,
        "each structured answer must keep its duration/token footer:\n{out}"
    );
    let tail = &out[after..];
    assert!(
        tail.contains("gpt-test") && tail.contains("3s") && tail.contains("20 tok/s"),
        "the post-status answer must retain stats below its content:\n{out}"
    );
}

#[test]
fn graph_results_wait_for_stream_and_open_tool_then_flush_fifo() {
    let mut engine = Engine::new(120, 80).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    fixture_assistant_delta(&mut engine, "lead answer");
    for (run_id, run_name) in [("run-first", "first"), ("run-second", "second")] {
        dispatch_event(
            &mut engine,
            json!({
                "kind": "chat.graph_result.append",
                "run_id": run_id,
                "run_name": run_name,
                "status": "success",
            }),
        );
    }
    let open = render_str(&mut engine);
    assert!(
        open.contains("lead answer") && !open.contains("mag workflow"),
        "results must remain hidden while the lead stream is open:\n{open}"
    );

    fixture_assistant_completed(
        &mut engine,
        None,
        json!({ "model": "gpt-test", "duration_ms": 10 }),
    );
    let streamed = render_str(&mut engine);
    let first = streamed.find("first").expect("first result");
    let second = streamed.find("second").expect("second result");
    assert!(
        first < second,
        "stream completion must flush graph results in FIFO order:\n{streamed}"
    );

    fixture_tool_started(&mut engine, "tool-1", "mag", json!({}));
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.graph_result.append",
            "run_id": "run-tool",
            "run_name": "tool-result",
            "status": "success",
        }),
    );
    let tool_open = render_str(&mut engine);
    assert!(
        !tool_open.contains("tool-result"),
        "result must remain hidden while the lead tool is open:\n{tool_open}"
    );

    fixture_tool_completed(&mut engine, "tool-1", json!("done"), false);
    fixture_assistant_completed(&mut engine, None, json!({}));
    let out = render_str(&mut engine);
    assert!(
        out.contains("tool-result"),
        "tool completion must flush its buffered graph result:\n{out}"
    );
}

#[test]
fn fast_graph_completion_waits_for_the_open_conversation_turn() {
    let mut engine = Engine::new(120, 80).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    fixture_tool_started(&mut engine, "fast-mag", "mag", json!({}));
    fixture_tool_completed(&mut engine, "fast-mag", json!("executing"), false);
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.graph_result.append",
            "run_id": "fast-run",
            "run_name": "fast-finished",
            "status": "success",
        }),
    );

    let raced = render_str(&mut engine);
    assert!(
        !raced.contains("fast-finished"),
        "a result cannot overtake provider output while its conversation turn is open:\n{raced}"
    );

    fixture_assistant_delta(&mut engine, "STARTED_FAST_WORKFLOW");
    fixture_assistant_completed(
        &mut engine,
        None,
        json!({ "model": "test", "duration_ms": 1 }),
    );
    let out = render_str(&mut engine);
    let comment = out.find("STARTED_FAST_WORKFLOW").expect("comment");
    let result = out.find("fast-finished").expect("graph result");
    assert!(
        comment < result,
        "provider output must retain its causal position before the fast result:\n{out}"
    );
}

#[test]
fn chat_reset_closes_the_lead_unit_and_preserves_buffered_graph_results() {
    let mut engine = Engine::new(100, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    fixture_assistant_delta(&mut engine, "unfinished");
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.graph_result.append",
            "run_id": "preserved",
            "run_name": "preserved-result",
            "status": "failed",
        }),
    );
    dispatch_event(&mut engine, json!({ "kind": "chat.reset" }));

    let out = render_str(&mut engine);
    assert!(
        out.contains("preserved-result"),
        "reset closes the interrupted lead unit without losing its graph result:\n{out}"
    );
}

#[test]
fn replayed_reset_preserves_results_but_new_session_selects_a_fresh_conversation() {
    let mut engine = Engine::new(100, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(&mut engine, json!({ "kind": "sessions.replay.start" }));
    fixture_assistant_completed(
        &mut engine,
        None,
        json!({ "model": "test", "duration_ms": 1 }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.graph_result.append",
            "run_id": "replayed",
            "run_name": "replayed-result",
            "status": "cancelled",
        }),
    );
    dispatch_event(&mut engine, json!({ "kind": "chat.reset" }));
    dispatch_event(&mut engine, json!({ "kind": "sessions.replay.end" }));

    let replayed = render_str(&mut engine);
    assert!(
        replayed.contains("replayed-result"),
        "replaying a reset must deterministically retain the preceding result:\n{replayed}"
    );

    fixture_assistant_delta(&mut engine, "old session lead");
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.graph_result.append",
            "run_id": "old-session",
            "run_name": "must-not-leak",
            "status": "success",
        }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "sessions.session_end", "session_id": "old" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "sessions.session_start", "session_id": "new" }),
    );
    fixture_assistant_completed(
        &mut engine,
        None,
        json!({ "model": "test", "duration_ms": 1 }),
    );

    let new_session = render_str(&mut engine);
    assert!(
        !new_session.contains("must-not-leak"),
        "selecting the new session's conversation must hide the old projection:\n{new_session}"
    );
}

#[test]
fn structured_answer_keeps_footer_across_interleaved_steered_user_append() {
    let mut engine = Engine::new(120, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    activate_conversation(&mut engine, "root");
    append_canonical_assistant_turn(
        &mut engine,
        "root",
        "turn-1",
        "structured answer",
        json!({ "model": "gpt-test", "duration_ms": 2_000,
            "usage": { "output_tokens": 40 } }),
    );
    append_canonical_message(&mut engine, "root", "steered", "user", "steered follow-up");

    let out = render_str(&mut engine);
    let answer = out.find("structured answer").expect("structured answer");
    let steering = out.find("steered follow-up").expect("steered user append");
    assert!(
        answer < steering,
        "the answer must retain its provider-round position before steering:\n{out}"
    );
    assert!(
        out[answer..steering].contains("▣ gpt-test · 2s · 20 tok/s"),
        "the projected answer must retain its provider footer:\n{out}"
    );
}

#[test]
fn structured_answer_keeps_footer_across_graceful_interrupt_notice() {
    let mut engine = Engine::new(120, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    activate_conversation(&mut engine, "root");
    append_canonical_assistant_turn(
        &mut engine,
        "root",
        "turn-1",
        "structured answer",
        json!({ "model": "gpt-test", "duration_ms": 2_000,
            "usage": { "output_tokens": 40 } }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "conversation.projection.delta", "conversation_id": "root",
            "change": { "kind": "turn_interrupted", "turn_id": "interrupt",
                "run_id": "interrupt",
                "terminal": { "reason": "[interrupted by user — cancelling in-flight work]" } }
        }),
    );

    let out = render_str(&mut engine);
    let answer = out.find("structured answer").expect("structured answer");
    let notice = out
        .find("[interrupted by user — cancelling in-flight work]")
        .expect("interrupt notice");
    assert!(
        answer < notice,
        "the answer must retain its provider-round position before the notice:\n{out}"
    );
    assert!(
        out[answer..notice].contains("▣ gpt-test · 2s · 20 tok/s"),
        "the projected answer must retain its provider footer:\n{out}"
    );
}

#[test]
fn lead_failure_closes_empty_provider_round_before_later_assistant_projection() {
    let mut engine = Engine::new(120, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    activate_conversation(&mut engine, "root");
    append_canonical_assistant_terminal(
        &mut engine,
        "root",
        "failed-turn",
        "",
        "turn_failed",
        json!({ "model": "gpt-test", "duration_ms": 2_000,
            "error": "[lead turn failed] terminal MAG failure",
            "usage": { "output_tokens": 40 } }),
    );
    append_canonical_message(&mut engine, "root", "next-user", "user", "next turn");
    append_canonical_assistant_turn(&mut engine, "root", "next-turn", "later answer", json!({}));

    let out = render_str(&mut engine);
    let failure = out.find("[lead turn failed]").expect("lead failure notice");
    let user = out.find("next turn").expect("next user message");
    let answer = out.find("later answer").expect("later assistant message");
    assert!(
        failure < user && user < answer,
        "failed-turn chronology changed:\n{out}"
    );
    assert!(
        out[..failure].contains("▣ gpt-test · 2s · 20 tok/s"),
        "failed provider round must retain its own footer before the notice:\n{out}"
    );
    assert!(
        !out[answer..].contains("▣ gpt-test") && !out[answer..].contains("tok/s"),
        "later assistant inherited stale failed-turn metadata:\n{out}"
    );
}

#[test]
fn structured_chat_error_is_readable_and_closes_the_provider_round() {
    let mut engine = Engine::new(120, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    activate_conversation(&mut engine, "root");
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.error.append",
            "title": "Provider temporarily unavailable",
            "message": "The model provider is overloaded right now. Please try again.",
            "retryable": true,
        }),
    );
    append_canonical_assistant_turn(&mut engine, "root", "next-turn", "later answer", json!({}));

    let out = render_str(&mut engine);
    assert!(
        out.contains("Provider temporarily unavailable")
            && out.contains("The model provider is overloaded right now.")
            && out.contains("You can retry the request."),
        "structured error should render as a concise actionable block:\n{out}"
    );
    assert!(
        !out.contains("semantic_type_id") && !out.contains("nefor.contracts.AgentError"),
        "runtime contract details leaked into the rendered chat:\n{out}"
    );
    let later = out.find("later answer").expect("later assistant answer");
    assert!(
        !out[later..].contains("gpt-test"),
        "later assistant inherited stale failed-turn metadata:\n{out}"
    );
}

#[test]
fn tool_start_closes_empty_provider_round_before_final_answer_projection() {
    let mut engine = Engine::new(100, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    fixture_assistant_completed(
        &mut engine,
        None,
        json!({ "model": "gpt-test", "duration_ms": 1 }),
    );
    fixture_tool_started(&mut engine, "t1", "mag-eval", json!({}));
    fixture_assistant_completed(
        &mut engine,
        None,
        json!({ "model": "gpt-test", "duration_ms": 2 }),
    );
    fixture_message(&mut engine, "assistant", "final answer");

    let out = render_str(&mut engine);
    let tool = out.find("mag-eval").expect("tool entry");
    let answer = out.find("final answer").expect("final answer");
    assert!(
        tool < answer,
        "final answer attached to the tool-call round:\n{out}"
    );
}

#[test]
fn foreign_chat_stream_delta_stays_out_of_transcript() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    activate_conversation(&mut engine, "root");
    dispatch_event(
        &mut engine,
        json!({
            "kind": "conversation.projection.delta", "conversation_id": "worker",
            "change": { "kind": "content_chunk_appended", "message_id": "worker-answer",
                "chunk": { "kind": "text", "data": "kernel-internal-bytes" } }
        }),
    );
    append_canonical_assistant_turn(
        &mut engine,
        "root",
        "root-turn",
        "lead-visible-reply",
        json!({ "model": "qwen-test", "duration_ms": 42 }),
    );

    let out = render_str(&mut engine);
    assert!(
        !out.contains("kernel-internal"),
        "a non-active conversation must not render in the transcript: {out:?}"
    );
    assert!(
        out.contains("lead-visible-reply"),
        "lead chat's delta must still render: {out:?}"
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
fn busy_submits_emit_immediately_and_coalesce_in_transcript() {
    // Lead-turn flip: the kernel owns turn queueing, so the chat
    // surface no longer buffers busy follow-ups locally. Each submit
    // while a turn is in flight (a) emits chat.input.submit right away
    // and (b) renders immediately, coalesced into ONE user entry
    // rather than one bubble per message. (This replaces the old
    // "queued follow-up widget" contract, whose text stayed out of the
    // transcript until stream end promoted it.)
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    submit_text(&mut engine, "first");
    let _ = engine.take_emit_queue();
    let _ = render_str(&mut engine);

    submit_text(&mut engine, "second");
    let _ = render_str(&mut engine);
    submit_text(&mut engine, "third");
    let emits = engine.take_emit_queue();
    assert_eq!(
        emits.len(),
        2,
        "each busy follow-up must emit its own submit immediately: {emits:?}"
    );
    for (text, (target_hint, body)) in ["second", "third"].iter().zip(&emits) {
        assert_eq!(target_hint.as_deref(), Some("engine"));
        assert_eq!(
            body.get("kind").and_then(|v| v.as_str()),
            Some("chat.input.submit")
        );
        assert_eq!(body.get("text").and_then(|v| v.as_str()), Some(*text));
    }

    // Full-frame snapshot: "second" already rendered on the previous
    // frame, so the line-diff from render_str would only carry "third".
    let out = render_snapshot(&mut engine);
    assert!(
        out.contains("second") && out.contains("third"),
        "busy follow-ups must render in the transcript immediately: {out:?}"
    );
    assert!(
        !out.contains("queued follow-up"),
        "the old pending-widget UX must not come back: {out:?}"
    );

    // No local buffer means stream end has nothing left to promote.
    fixture_assistant_completed(
        &mut engine,
        None,
        json!({ "model": "test", "duration_ms": 1 }),
    );
    let emits = engine.take_emit_queue();
    assert!(
        emits.is_empty(),
        "stream end must not re-emit follow-ups — they already went out: {emits:?}"
    );
}

#[test]
fn double_escape_stops_lead_and_moves_queue_before_existing_prompt_text() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    submit_text(&mut engine, "first");
    let _ = engine.take_emit_queue();
    let _ = render_str(&mut engine);

    submit_text(&mut engine, "queued");
    let _ = render_str(&mut engine);
    type_text(&mut engine, "draft");
    let _ = engine.take_emit_queue();
    engine.handle_key(key("escape")).expect("escape");
    assert!(
        engine.take_emit_queue().is_empty(),
        "first escape must wait for the double-Esc window"
    );
    engine.handle_key(key("escape")).expect("second escape");
    let emits = engine.take_emit_queue();
    assert_eq!(emits.len(), 1, "interrupt emit");
    assert_eq!(
        emits[0].1.get("kind").and_then(|v| v.as_str()),
        Some("chat.interrupt")
    );
    assert_eq!(
        emits[0].1.get("drop_queued").and_then(|v| v.as_bool()),
        Some(true)
    );

    let out = render_snapshot(&mut engine);
    assert!(
        out.contains("queued draft"),
        "double Esc should restore queued text before the existing draft with one space: {out:?}"
    );
    let queued_occurrences = out.matches("queued").count();
    assert_eq!(
        queued_occurrences, 1,
        "restored text must exist only in the prompt, not the transcript: {out:?}"
    );
}

#[test]
fn single_escape_waits_then_steers_only_when_a_message_is_queued() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    submit_text(&mut engine, "first");
    let _ = engine.take_emit_queue();
    let _ = render_str(&mut engine);
    submit_text(&mut engine, "queued");
    let _ = engine.take_emit_queue();

    engine.handle_key(key("escape")).expect("escape");
    assert!(
        engine.take_emit_queue().is_empty(),
        "single Esc acts only after timeout"
    );
    engine.advance_time(Duration::from_millis(601));
    engine.drive_scheduled_dispatches().expect("drive timer");
    let emits = engine.take_emit_queue();
    assert_eq!(emits.len(), 1);
    assert_eq!(
        emits[0].1.get("kind").and_then(|v| v.as_str()),
        Some("chat.steer")
    );

    let _ = render_str(&mut engine);
    dispatch_event(&mut engine, json!({ "kind": "chat.queue.steered" }));
    fixture_message(&mut engine, "user", "queued");
    let out = render_snapshot(&mut engine);
    assert_eq!(
        out.matches("queued").count(),
        1,
        "accepted steering keeps exactly one visible user occurrence: {out:?}"
    );
}

#[test]
fn accepted_steering_reconciles_indexed_entry_after_non_tail_interleaving() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    submit_text(&mut engine, "first");
    let _ = engine.take_emit_queue();
    let _ = render_str(&mut engine);
    submit_text(&mut engine, "queued-owned");
    let _ = engine.take_emit_queue();

    fixture_message(&mut engine, "assistant", "interleaved-result");
    dispatch_event(&mut engine, json!({ "kind": "chat.queue.steered" }));
    fixture_message(&mut engine, "user", "queued-owned");

    let out = render_snapshot(&mut engine);
    assert!(
        out.contains("interleaved-result"),
        "interleaved result missing: {out:?}"
    );
    assert_eq!(
        out.matches("queued-owned").count(),
        1,
        "accepted steering must reconcile its indexed optimistic entry even when it is not tail: {out:?}"
    );
}

#[test]
fn accepted_steering_preserves_repeated_equal_user_text() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    submit_text(&mut engine, "first");
    let _ = engine.take_emit_queue();
    let _ = render_str(&mut engine);
    submit_text(&mut engine, "same-text");
    let _ = engine.take_emit_queue();

    dispatch_event(&mut engine, json!({ "kind": "chat.queue.steered" }));
    fixture_message(&mut engine, "user", "same-text");
    fixture_message(&mut engine, "user", "same-text");

    let out = render_snapshot(&mut engine);
    assert_eq!(
        out.matches("same-text").count(),
        2,
        "accepted steering must not globally dedup repeated equal user text: {out:?}"
    );
}

#[test]
fn single_escape_without_queue_expires_as_noop() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);
    submit_text(&mut engine, "first");
    let _ = engine.take_emit_queue();

    engine.handle_key(key("escape")).expect("escape");
    engine.advance_time(Duration::from_millis(601));
    engine.drive_scheduled_dispatches().expect("drive timer");
    assert!(engine.take_emit_queue().is_empty());
}

#[test]
fn absolute_path_submit_is_plain_chat_not_slash_command() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    let path = "/home/example/.local/share/nefor/clipboard-images/paste.png";
    submit_text(&mut engine, path);

    let emits = engine.take_emit_queue();
    assert_eq!(emits.len(), 1, "submit should produce exactly one emit");
    let (target_hint, body) = &emits[0];
    assert_eq!(target_hint.as_deref(), Some("engine"));
    assert_eq!(
        body.get("kind").and_then(|v| v.as_str()),
        Some("chat.input.submit")
    );
    assert_eq!(body.get("text").and_then(|v| v.as_str()), Some(path));

    let out = render_str(&mut engine);
    assert!(
        out.contains("/home/example/.local/share/nefor/clipboa")
            && out.contains("rd-images/paste.png"),
        "absolute-path user echo missing: {out:?}"
    );
}

#[test]
fn input_field_renders_full_width_rounded_border() {
    // The input box has `╭─╮ │ ╰─╯` chrome in
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
    fixture_message(&mut engine, "user", "hello");
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

fn press_ctrl_c(engine: &mut Engine) {
    engine
        .handle_key(KeyMessage {
            name: "c".into(),
            mods: vec!["ctrl"],
        })
        .expect("ctrl+c");
}

fn assert_first_ctrl_c_warns(engine: &mut Engine) {
    press_ctrl_c(engine);
    assert!(!engine.exit_requested(), "first Ctrl+C must not exit");
    assert!(
        engine.take_emit_queue().is_empty(),
        "first Ctrl+C must not interrupt work",
    );
    engine.advance_time(Duration::from_millis(250));
    let out = render_snapshot(engine);
    assert!(
        out.contains("Press Ctrl+C again to exit"),
        "first Ctrl+C warning missing: {out:?}",
    );
}

#[test]
fn double_ctrl_c_exits_consistently_across_chat_contexts() {
    for setup in ["idle", "active", "focused-input", "popup"] {
        let mut engine = Engine::new(80, 24).expect("engine");
        engine.load_scenario(&chat_lua_source()).expect("load");
        let _ = render_str(&mut engine);

        match setup {
            "active" => {
                submit_text(&mut engine, "working");
                let _ = engine.take_emit_queue();
            }
            "focused-input" => type_text(&mut engine, "draft"),
            "popup" => {
                submit_text(&mut engine, "/help");
                let out = render_snapshot(&mut engine);
                assert!(out.contains("── help ──"), "help popup missing: {out:?}");
            }
            "idle" => {}
            _ => unreachable!(),
        }

        assert_first_ctrl_c_warns(&mut engine);
        press_ctrl_c(&mut engine);
        assert!(
            engine.exit_requested(),
            "second consecutive Ctrl+C must exit in {setup} context",
        );
        let emits = engine.take_emit_queue();
        assert!(
            emits.iter().any(|(_, b)| {
                b.get("kind").and_then(|v| v.as_str()) == Some("chat.interrupt_all")
            }),
            "second Ctrl+C must interrupt work before exit in {setup} context: {emits:?}",
        );
    }
}

#[test]
fn ctrl_c_latch_resets_after_timeout_and_intervening_action() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    assert_first_ctrl_c_warns(&mut engine);
    engine.advance_time(Duration::from_millis(601));
    engine.drive_scheduled_dispatches().expect("drive timer");
    let out = render_snapshot(&mut engine);
    assert!(
        out.contains("Press Ctrl+C again to exit"),
        "warning must remain visible after the double-press latch expires: {out:?}",
    );
    engine.advance_time(Duration::from_millis(2000));
    let out = render_snapshot(&mut engine);
    assert!(
        out.contains("Press Ctrl+C again to exit"),
        "warning must remain visible for about three seconds: {out:?}",
    );
    engine.advance_time(Duration::from_millis(500));
    let out = render_snapshot(&mut engine);
    assert!(
        !out.contains("Press Ctrl+C again to exit"),
        "warning must disappear after its three-second TTL: {out:?}",
    );
    press_ctrl_c(&mut engine);
    assert!(
        !engine.exit_requested(),
        "Ctrl+C after timeout must warn again"
    );

    engine.handle_key(key("x")).expect("intervening input");
    press_ctrl_c(&mut engine);
    assert!(
        !engine.exit_requested(),
        "Ctrl+C after an intervening action must warn again",
    );
    press_ctrl_c(&mut engine);
    assert!(
        engine.exit_requested(),
        "fresh consecutive second press must exit"
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
    let emits = engine.take_emit_queue();
    assert!(
        emits
            .iter()
            .any(|(_, b)| b.get("kind").and_then(|v| v.as_str()) == Some("chat.interrupt_all")),
        "Ctrl+D must interrupt in-flight work before exiting; got {emits:?}",
    );
}

#[test]
fn slash_new_clears_transcript_and_mints_new_session() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Seed a couple of entries first.
    fixture_message(&mut engine, "user", "previous");
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
fn failed_new_retains_transcript_and_ignores_late_acknowledgements() {
    let mut engine = Engine::new(100, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);
    fixture_message(&mut engine, "user", "authoritative old transcript");

    dispatch_event(
        &mut engine,
        json!({ "kind": "input.submit", "value": "/new" }),
    );
    let _ = render_str(&mut engine);
    assert!(engine.snapshot().contains("authoritative old transcript"));

    dispatch_event(
        &mut engine,
        json!({ "kind": "sessions.transition_failed", "operation": "new",
            "request_id": "stale-request", "message": "stale failure" }),
    );
    let _ = render_str(&mut engine);
    assert!(engine.snapshot().contains("authoritative old transcript"));

    dispatch_event(
        &mut engine,
        json!({ "kind": "sessions.transition_failed", "operation": "new",
            "request_id": "chat-transition-1", "message": "disk unavailable" }),
    );
    let _ = render_str(&mut engine);
    let failed = engine.snapshot();
    assert!(failed.contains("authoritative old transcript"), "{failed}");
    assert!(
        failed.contains("Session switch failed") && failed.contains("disk unavailable"),
        "{failed}"
    );

    dispatch_event(
        &mut engine,
        json!({ "kind": "sessions.session_start", "session_id": "late-session",
            "request_id": "chat-transition-1" }),
    );
    let _ = render_str(&mut engine);
    let late = engine.snapshot();
    assert!(late.contains("authoritative old transcript"), "{late}");
    let state = engine.state_table().expect("state");
    assert_ne!(
        state
            .get::<Option<String>>("session_id")
            .unwrap()
            .as_deref(),
        Some("late-session")
    );
}

#[test]
fn slash_new_clears_panel_runs() {
    let mut engine = Engine::new(120, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Seed an active kernel run.
    dispatch_event(
        &mut engine,
        json!({
            "kind": "mag.run_started",
            "run_id": "run-aaaaaaaa",
        }),
    );
    let out = render_str(&mut engine);
    assert!(
        out.contains("MAG run-aaaa"),
        "run-panel header should appear pre-/new: {out:?}"
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
        !out.contains("MAG run-aaaa"),
        "run panel should be empty after /new: {out:?}"
    );
}

// ── Run panel ─────────────────────────────────────────────────────────
//
// The kernel's `mag.*` lifecycle stream drives the sidebar run panel.
// Kernel runs are concurrent and every event carries its run_id; the
// surface keys panel state straight off it. The panel groups actors by
// top-level namespace segment — an agent's whole subtree collapses to a
// single group row. The panel is visible by default; Ctrl+B toggles it
// off. Linger handling is pure-update plus a view-side filter, so a
// completed run drops after `LINGER_MS` of engine time.
#[test]
fn mag_run_lifecycle_renders_in_run_panel() {
    let mut engine = Engine::new(120, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({
            "kind": "mag.run_started",
            "run_id": "mag-demo-1",
            "run_name": "demo",
        }),
    );
    // Actor events carry their run_id — the surface keys them into that
    // run's panel entry. Two actors in the same namespace collapse into
    // one `explorer` group row.
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_spawned", "run_id": "mag-demo-1", "id": "explorer.entry", "factory": "llm" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_spawned", "run_id": "mag-demo-1", "id": "explorer.loop-counter", "factory": "llm" }),
    );
    let out = render_str(&mut engine);
    assert!(
        out.contains("MAG demo"),
        "mag run header should show the run name: {out:?}"
    );
    assert!(
        out.contains("explorer"),
        "explorer group row missing from panel: {out:?}"
    );
    assert!(
        !out.contains("explorer.entry"),
        "group row must collapse the subtree, not expose member ids: {out:?}"
    );
    assert!(
        out.contains('○'),
        "pending glyph (○) missing for a pending group: {out:?}"
    );
    assert!(
        out.contains("(0/1)"),
        "header should count one pending group: {out:?}"
    );

    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_ready", "run_id": "mag-demo-1", "id": "explorer.entry" }),
    );
    let out = render_str(&mut engine);
    assert!(
        !out.contains('●'),
        "a constructed but unfired member must not start yellow time: {out:?}"
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_busy", "run_id": "mag-demo-1", "id": "explorer.entry" }),
    );
    let out = render_str(&mut engine);
    assert!(
        out.contains('●'),
        "running glyph (●) missing once a group member is busy: {out:?}"
    );

    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_complete", "run_id": "mag-demo-1", "from": "sink" }),
    );
    let out = render_str(&mut engine);
    assert!(
        out.contains('✓'),
        "done glyph (✓) missing after run_complete: {out:?}"
    );
    assert!(
        out.contains("(1/1)"),
        "counter should read 1/1 (groups) after run completes: {out:?}"
    );
}

// A killed actor renders with its own glyph, distinct from a
// failed/errored node. Under grouping, killing the group's members marks
// the group row ⊗.
#[test]
fn mag_killed_actor_renders_distinct_glyph() {
    let mut engine = Engine::new(120, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_started", "run_id": "mag-kill-1", "run_name": "killer" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_spawned", "run_id": "mag-kill-1", "id": "writer.draft", "factory": "llm" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_ready", "run_id": "mag-kill-1", "id": "writer.draft" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_killed", "run_id": "mag-kill-1", "id": "writer.draft" }),
    );
    let out = render_str(&mut engine);
    assert!(
        out.contains('⊗'),
        "killed glyph (⊗) missing for a killed group: {out:?}"
    );
    assert!(
        out.contains("writer"),
        "killed group row missing from panel: {out:?}"
    );
    assert!(
        !out.contains("writer.draft"),
        "group row must not expose member ids: {out:?}"
    );
}

// A completed run's teardown sweep is bookkeeping, not death: after
// `mag.run_complete` the kernel reaps the run's still-live actors and stamps
// those `mag.actor_killed` events with reason "run_complete"
// (plugins/mag/docs/actor-model.md, Kill reasons). The panel keeps the ✓ the
// completion painted — a successful run must never repaint ⊗ — while a kill
// with any other reason still renders ⊗.
#[test]
fn mag_post_complete_teardown_keeps_done_glyphs() {
    let mut engine = Engine::new(120, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_started", "run_id": "mag-done-1", "run_name": "finisher" }),
    );
    for id in ["writer.draft", "sink"] {
        dispatch_event(
            &mut engine,
            json!({ "kind": "mag.actor_spawned", "run_id": "mag-done-1", "id": id, "factory": "llm" }),
        );
        dispatch_event(
            &mut engine,
            json!({ "kind": "mag.actor_ready", "run_id": "mag-done-1", "id": id }),
        );
    }
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_complete", "run_id": "mag-done-1", "from": "sink" }),
    );
    // The plugin ends the run context after the terminal reply; the sweep's
    // killed events arrive on the wire stamped run_complete.
    for id in ["writer.draft", "sink"] {
        dispatch_event(
            &mut engine,
            json!({ "kind": "mag.actor_killed", "run_id": "mag-done-1", "id": id, "reason": "run_complete" }),
        );
    }
    let _ = render_str(&mut engine);
    let snap = engine.snapshot();
    assert!(
        !snap.contains('\u{2297}'),
        "post-complete teardown kills must not repaint groups killed:\n{snap}"
    );
    assert!(
        snap.contains('\u{2713}'),
        "completed groups keep the done glyph through the teardown sweep:\n{snap}"
    );

    // Contrast: a kill with a non-completion reason still renders ⊗.
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_started", "run_id": "mag-done-2", "run_name": "victim" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_spawned", "run_id": "mag-done-2", "id": "worker.llm", "factory": "llm" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_ready", "run_id": "mag-done-2", "id": "worker.llm" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_killed", "run_id": "mag-done-2", "id": "worker.llm", "reason": "killed" }),
    );
    let _ = render_str(&mut engine);
    let snap = engine.snapshot();
    assert_eq!(
        snap.matches('\u{2297}').count(),
        1,
        "a real kill still renders the killed glyph:\n{snap}"
    );
}

// A FAILED run must enter the same linger→prune cycle a completed run does:
// `mag.run_failed` stamps the run terminal so it fades out. Regression for the
// 0.4.0 acceptance defect where a failed eval run (`bash-1 killed`, `sink`)
// lingered in the sidebar forever because only `mag.run_complete` stamped
// `completed_at_ms`.
#[test]
fn mag_run_failed_prunes_after_linger() {
    let mut engine = Engine::new(120, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_started", "run_id": "run-failaaa", "run_name": "eval-5" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_spawned", "run_id": "run-failaaa", "id": "bash-1", "factory": "shell.script" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_ready", "run_id": "run-failaaa", "id": "bash-1" }),
    );
    // The run fails, then its still-live actors are reaped with reason
    // "run_failed" (the teardown sweep) — the wire order the kernel produces.
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_failed", "run_id": "run-failaaa", "from": "bash-1" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_killed", "run_id": "run-failaaa", "id": "bash-1", "reason": "run_failed" }),
    );

    let out = render_str(&mut engine);
    assert!(
        out.contains("MAG eval-5"),
        "failed run should linger initially: {out:?}"
    );

    // Past the linger window the failed run must disappear, exactly like a
    // completed one — no dispatch, view-side filter alone.
    engine.advance_time(Duration::from_millis(3000));
    let out = render_str(&mut engine);
    assert!(
        !out.contains("MAG eval-5"),
        "failed run should be pruned past the linger window: {out:?}"
    );
    assert!(
        out.contains("Space: inspect last completed run"),
        "completed-run inspection hint missing after a failed run prunes: {out:?}"
    );
}

// A run torn down by an outright `mag.kill_run` emits no run-level terminal
// event — only `mag.actor_killed` with reason "killed". That teardown reason
// must still stamp the run terminal so it prunes; otherwise a killed run
// lingers forever the same way a failed one used to.
#[test]
fn mag_killed_run_prunes_after_linger() {
    let mut engine = Engine::new(120, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_started", "run_id": "run-killbbb", "run_name": "victim" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_spawned", "run_id": "run-killbbb", "id": "worker.llm", "factory": "llm" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_ready", "run_id": "run-killbbb", "id": "worker.llm" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_killed", "run_id": "run-killbbb", "id": "worker.llm", "reason": "killed" }),
    );

    let out = render_str(&mut engine);
    assert!(
        out.contains("MAG victim"),
        "killed run should linger initially: {out:?}"
    );

    engine.advance_time(Duration::from_millis(3000));
    dispatch_event(&mut engine, json!({ "kind": "input.changed", "value": "" }));
    let out = render_str(&mut engine);
    assert!(
        !out.contains("MAG victim"),
        "killed run should be pruned past the linger window: {out:?}"
    );
}

// A mid-run control-plane kill (`reason: "modification"`) removes one actor
// but the RUN stays live — it must NOT be stamped terminal, so it keeps
// rendering and never prunes while its siblings run. Guards the terminal
// stamping from over-reaching past genuine teardown reasons.
#[test]
fn mag_modification_kill_keeps_run_live() {
    let mut engine = Engine::new(120, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_started", "run_id": "run-modccc", "run_name": "living" }),
    );
    for id in ["writer.draft", "explorer.entry"] {
        dispatch_event(
            &mut engine,
            json!({ "kind": "mag.actor_spawned", "run_id": "run-modccc", "id": id, "factory": "llm" }),
        );
        dispatch_event(
            &mut engine,
            json!({ "kind": "mag.actor_ready", "run_id": "run-modccc", "id": id }),
        );
    }
    // One actor is killed by an applied modification — the run continues.
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_killed", "run_id": "run-modccc", "id": "writer.draft", "reason": "modification" }),
    );

    // Well past any linger window, with a dispatch to run the prune: the run
    // is still live (never stamped terminal), so it must stay on screen.
    engine.advance_time(Duration::from_millis(5000));
    dispatch_event(&mut engine, json!({ "kind": "input.changed", "value": "" }));
    let out = render_str(&mut engine);
    assert!(
        out.contains("MAG living"),
        "a modification kill must not prune the still-live run: {out:?}"
    );
}

// Reason-accurate member labels: a run torn down as "run_failed" reads its
// member rows as `failed`, not the literal word `killed`. Only kill_run /
// modification reasons read "killed" (covered above). Unfolds the group so the
// member leaf row — where the status word is painted — renders.
#[test]
fn mag_failed_run_member_row_reads_failed() {
    let mut engine = Engine::new(120, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_started", "run_id": "run-fmlabel", "run_name": "eval-9" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_spawned", "run_id": "run-fmlabel", "id": "writer.draft", "factory": "llm" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_ready", "run_id": "run-fmlabel", "id": "writer.draft" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_failed", "run_id": "run-fmlabel", "from": "writer.draft" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_killed", "run_id": "run-fmlabel", "id": "writer.draft", "reason": "run_failed" }),
    );

    // Focus the sidebar and unfold the (default-collapsed) `writer` group so
    // its member leaf row paints its status word.
    engine.handle_key(key("tab")).expect("tab");
    let _ = render_str(&mut engine);
    engine.handle_key(key("down")).expect("down"); // → writer group row
    engine.handle_key(key("enter")).expect("enter"); // unfold
    let out = render_str(&mut engine);

    assert!(
        out.contains("writer.draft"),
        "unfolded member row should expose the failed actor id: {out:?}"
    );
    assert!(
        out.contains("failed"),
        "a run_failed member must read `failed`: {out:?}"
    );
    assert!(
        !out.contains("killed"),
        "a run_failed member must NOT read the literal word `killed`: {out:?}"
    );
}

// Grouping across multiple agents: a two-agent run (explorer.* + writer.*)
// plus a standalone `sink` actor collapses to exactly three group rows.
// Killing the whole writer subtree marks only the writer group ⊗; the
// explorer group keeps running.
#[test]
fn mag_two_agent_run_groups_by_namespace() {
    let mut engine = Engine::new(120, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Sum of the four group-status glyphs == one per rendered group row.
    let glyphs = |s: &str| {
        s.matches('○').count()
            + s.matches('●').count()
            + s.matches('✓').count()
            + s.matches('⊗').count()
    };

    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_started", "run_id": "mag-multi-1", "run_name": "multi" }),
    );
    for id in [
        "explorer.entry",
        "explorer.exhaust",
        "writer.plan",
        "writer.draft",
        "sink",
    ] {
        dispatch_event(
            &mut engine,
            json!({ "kind": "mag.actor_spawned", "run_id": "mag-multi-1", "id": id, "factory": "llm" }),
        );
    }
    let out = render_str(&mut engine);
    assert!(
        out.contains("explorer") && out.contains("writer") && out.contains("sink"),
        "all three group rows should be present: {out:?}"
    );
    assert!(
        !out.contains("explorer.entry") && !out.contains("writer.plan"),
        "group rows must collapse subtrees, not expose member ids: {out:?}"
    );
    assert_eq!(
        glyphs(&out),
        3,
        "five actors across three namespaces should render exactly 3 group rows: {out:?}"
    );
    assert!(
        out.contains("(0/3)"),
        "header should count three groups: {out:?}"
    );

    // Bring one member of each agent live.
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_ready", "run_id": "mag-multi-1", "id": "explorer.entry" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_busy", "run_id": "mag-multi-1", "id": "explorer.entry" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_ready", "run_id": "mag-multi-1", "id": "writer.plan" }),
    );
    // Kill the entire writer subtree.
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_killed", "run_id": "mag-multi-1", "id": "writer.plan" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_killed", "run_id": "mag-multi-1", "id": "writer.draft" }),
    );
    let out = render_str(&mut engine);
    assert_eq!(
        out.matches('⊗').count(),
        1,
        "only the writer group should be marked killed: {out:?}"
    );
    // Header still counts three groups (writer now terminal). The frame is
    // an incremental line-diff, so sink's unchanged pending row isn't
    // re-emitted here — the header total, always repainted, is the reliable
    // group-count signal.
    assert!(
        out.contains("(1/3)"),
        "header should still count three groups with writer completed: {out:?}"
    );
    assert!(
        out.contains('●'),
        "explorer group should still be running: {out:?}"
    );
}

// Concurrent kernel runs render independently, keyed by run_id: the
// lead's own turn-program overlaps its dispatched sub-runs, and the two
// author overlapping actor ids (fresh run contexts reuse names freely).
// Events must land in exactly the run their run_id names.
#[test]
fn mag_concurrent_runs_key_panel_state_by_run_id() {
    let mut engine = Engine::new(120, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_started", "run_id": "lead-run-1", "run_name": "lead", "scope": "r1" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_started", "run_id": "sub-run-1", "run_name": "auth-fix", "scope": "r2" }),
    );
    // Same actor id in both runs — panel state must not cross.
    for run_id in ["lead-run-1", "sub-run-1"] {
        dispatch_event(
            &mut engine,
            json!({ "kind": "mag.actor_spawned", "run_id": run_id, "id": "worker.llm", "factory": "llm" }),
        );
        dispatch_event(
            &mut engine,
            json!({ "kind": "mag.actor_ready", "run_id": run_id, "id": "worker.llm" }),
        );
        dispatch_event(
            &mut engine,
            json!({ "kind": "mag.actor_busy", "run_id": run_id, "id": "worker.llm" }),
        );
    }
    let _ = render_str(&mut engine);
    let snap = engine.snapshot();
    assert!(
        snap.contains("MAG lead") && snap.contains("MAG auth-fix"),
        "both concurrent runs render, distinguishable by run_name:\n{snap}"
    );

    // Kill the sub-run's actor: only ITS group flips ⊗; the lead's
    // same-named group keeps running.
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_killed", "run_id": "sub-run-1", "id": "worker.llm" }),
    );
    let _ = render_str(&mut engine);
    let snap = engine.snapshot();
    assert_eq!(
        snap.matches('\u{2297}').count(),
        1,
        "the kill lands in exactly the run its run_id names:\n{snap}"
    );
    assert!(
        snap.contains('\u{25cf}'),
        "the lead run's same-named actor keeps running:\n{snap}"
    );

    // Completing the sub-run finalises only that run's panel entry.
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_complete", "run_id": "sub-run-1", "from": "sink" }),
    );
    let _ = render_str(&mut engine);
    let snap = engine.snapshot();
    assert!(
        snap.contains("MAG lead") && snap.contains("MAG auth-fix"),
        "the completed sub-run lingers alongside the live lead run:\n{snap}"
    );
    assert!(
        snap.contains('\u{25cf}'),
        "run_complete for the sub-run must not finalise the lead run:\n{snap}"
    );
}

#[test]
fn active_conversation_switch_filters_old_and_foreign_projections() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    activate_conversation(&mut engine, "root");
    append_canonical_assistant_turn(
        &mut engine,
        "root",
        "root-turn",
        "root-visible",
        json!({ "model": "qwen-test", "duration_ms": 7 }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "conversation.projection.delta", "conversation_id": "worker",
            "change": { "kind": "message_completed",
                "message": { "id": "worker-message", "role": "assistant",
                    "text": "worker-internal" } }
        }),
    );

    let out = render_str(&mut engine);
    assert!(
        out.contains("root-visible"),
        "active projection missing: {out:?}"
    );
    assert!(
        !out.contains("worker-internal"),
        "foreign projection leaked: {out:?}"
    );

    activate_conversation(&mut engine, "next");
    dispatch_event(
        &mut engine,
        json!({
            "kind": "conversation.projection.delta", "conversation_id": "root",
            "change": { "kind": "message_completed",
                "message": { "id": "stale", "role": "assistant", "text": "stale-root" } }
        }),
    );
    append_canonical_assistant_turn(
        &mut engine,
        "next",
        "next-turn",
        "next-visible",
        json!({ "model": "m", "duration_ms": 1 }),
    );
    let out = render_str(&mut engine);
    assert!(
        !out.contains("stale-root"),
        "old active projection leaked: {out:?}"
    );
    assert!(
        out.contains("next-visible"),
        "new active projection missing: {out:?}"
    );
}

#[test]
fn first_canonical_activation_preserves_optimistic_user_message() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    for ch in "visible prompt".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    let _ = engine.take_emit_queue();
    let _ = render_str(&mut engine);

    activate_conversation(&mut engine, "root");
    dispatch_event(
        &mut engine,
        json!({
            "kind": "conversation.projection.delta", "conversation_id": "root",
            "change": { "kind": "turn_started", "turn_id": "turn", "run_id": "run" }
        }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "conversation.projection.delta", "conversation_id": "root",
            "change": { "kind": "message_started", "turn_id": "turn",
                "message": { "id": "user", "turn_id": "turn", "role": "user" } }
        }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "conversation.projection.delta", "conversation_id": "root",
            "change": { "kind": "content_chunk_appended", "turn_id": "turn",
                "message_id": "user",
                "chunk": { "kind": "structured", "data": {
                    "value": { "prompt": "visible prompt" },
                    "mag_type": { "version": 1 }
                } } }
        }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "conversation.projection.delta", "conversation_id": "root",
            "change": { "kind": "message_completed", "turn_id": "turn",
                "message": { "id": "user", "turn_id": "turn", "role": "user",
                    "text": "", "content": {
                        "value": { "prompt": "visible prompt" },
                        "mag_type": { "version": 1 }
                    } } }
        }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "conversation.projection.delta", "conversation_id": "root",
            "change": { "kind": "tool_call_completed", "turn_id": "turn",
                "exchange": { "id": "tool", "name": "mag", "status": "call_completed",
                    "arguments": { "id": "provider-call", "arguments": {} } } }
        }),
    );

    let _ = render_str(&mut engine);
    let out = engine.snapshot();
    assert!(
        out.contains("visible prompt"),
        "first activation must not erase the locally-owned user row when the canonical input is structured:\n{out}"
    );
    assert!(
        out.contains("mag"),
        "agent activity must remain visible:\n{out}"
    );
}

#[test]
fn graph_run_complete_hides_run_after_linger_without_dispatch() {
    // Regression for the "fully green sidebar until I interact" bug:
    // the wallclock_tick in plugins/nefor-tui/src/main.rs paints
    // every second so live elapsed labels advance, but the reducer
    // only runs on dispatched messages — so the prune in `update`
    // never ran between user keystrokes and the completed run lingered
    // on screen as a fully-done run. The view-side `is_expired`
    // filter in `run_panel.panel_children` drops the run at paint time so
    // wallclock_tick re-renders surface the empty panel without
    // needing a synthetic event.
    let mut engine = Engine::new(120, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_started", "run_id": "run-dddddddd" }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "mag.actor_spawned",
            "run_id": "run-dddddddd",
            "id": "n1",
            "factory": "llm",
        }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_ready", "run_id": "run-dddddddd", "id": "n1" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_complete", "run_id": "run-dddddddd" }),
    );

    let out = render_str(&mut engine);
    assert!(
        out.contains("MAG run-dddd"),
        "completed run should linger initially: {out:?}"
    );

    // Advance past the 2s linger and render again — `advance_time`
    // marks dirty (the same effect wallclock_tick has in production)
    // but does NOT dispatch any message. The view-side filter must
    // hide the run on this paint alone.
    engine.advance_time(Duration::from_millis(3000));
    let out = render_str(&mut engine);
    assert!(
        !out.contains("MAG run-dddd"),
        "completed run should be hidden past linger window without a dispatch: {out:?}"
    );
    assert!(
        out.contains("Space: inspect last completed run"),
        "completed-run inspection hint missing after linger window: {out:?}"
    );
}

#[test]
fn graph_run_complete_removes_run_after_linger_window() {
    let mut engine = Engine::new(120, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Stand up a completed run: started, spawned, ready, complete.
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_started", "run_id": "run-cccccccc" }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "mag.actor_spawned",
            "run_id": "run-cccccccc",
            "id": "n1",
            "factory": "llm",
        }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_ready", "run_id": "run-cccccccc", "id": "n1" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_complete", "run_id": "run-cccccccc" }),
    );

    // The run is still within its linger window — header is visible.
    let out = render_str(&mut engine);
    assert!(
        out.contains("MAG run-cccc"),
        "completed run should linger initially: {out:?}"
    );

    // Advance past the 2s linger and dispatch a no-op event so update
    // runs and prunes the stale entry. (The pure-update prune fires on
    // every dispatch — use a harmless input refresh that does not touch runs.)
    engine.advance_time(Duration::from_millis(3000));
    dispatch_event(&mut engine, json!({ "kind": "input.changed", "value": "" }));
    let out = render_str(&mut engine);
    assert!(
        !out.contains("MAG run-cccc"),
        "completed run should be pruned past linger window: {out:?}"
    );
    // The empty-state hint should now show in the sidebar.
    assert!(
        out.contains("Space: inspect last completed run"),
        "completed-run inspection hint missing after prune: {out:?}"
    );
}

#[test]
fn canonical_turn_stats_update_statusline_and_turn_footer() {
    let mut engine = Engine::new(120, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    fixture_assistant_completed(
        &mut engine,
        Some("answer".into()),
        json!({
            "model": "qwen-test", "duration_ms": 1500,
            "usage": { "input_tokens": 11, "output_tokens": 7 }
        }),
    );

    let out = render_str(&mut engine);
    // Persistent status carries current configuration/resources only;
    // completed-turn duration and speed live on the turn itself.
    assert!(
        out.contains("qwen-test"),
        "statusline missing model: {out:?}"
    );
    assert!(
        !out.contains("1 turns"),
        "turn count should stay out of the footer: {out:?}"
    );
    assert!(
        out.contains("▣ qwen-test · 1s · 5 tok/s"),
        "turn metadata should stay on the completed turn footer: {out:?}"
    );
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

    // Live: user is on `qwen-test`. The ack rides the active provider
    // (harness-pinned "mock-plugin") — acks from non-active providers
    // are dropped by the stale-ack guard, which is a separate contract.
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.model.set_ack",
            "provider": "mock-plugin",
            "model": "qwen-test",
        }),
    );
    let out = render_snapshot(&mut engine);
    assert!(
        out.contains("qwen-test"),
        "live model missing pre-replay: {out:?}"
    );

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
fn replayed_notifications_do_not_surface_as_popups_or_toasts() {
    let mut engine = Engine::new(100, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(&mut engine, json!({ "kind": "sessions.replay.start" }));
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.popup",
            "level": "warning",
            "title": "replay-popup",
            "message": "should stay silent",
        }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.toast",
            "text": "resume-toast",
            "ttl_ms": 60_000,
        }),
    );
    dispatch_event(&mut engine, json!({ "kind": "sessions.replay.end" }));

    let out = render_snapshot(&mut engine);
    assert!(
        !out.contains("should stay silent"),
        "replayed chat.popup must not open a notification popup: {out:?}"
    );
    assert!(
        !out.contains("resume-toast"),
        "replayed chat.toast must not display a toast notification: {out:?}"
    );
}

#[test]
fn statusline_shows_model_with_reasoning_effort() {
    let mut engine = Engine::new(120, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Provider matches the harness-pinned active provider: a
    // chat.model.set_ack from a NON-active provider is deliberately
    // ignored (stale-ack guard in handle_model_set_ack).
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.model.set_ack",
            "provider": "mock-plugin",
            "model": "qwen3:32b",
        }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.reasoning.set_ack",
            "provider": "mock-plugin",
            "effort": "high",
        }),
    );

    let out = render_snapshot(&mut engine);
    assert!(
        out.contains("qwen3:32b · high"),
        "statusline should render model and effort compactly: {out:?}"
    );
}

#[test]
fn ctrl_o_toggles_expanded_details() {
    let mut engine = Engine::new(120, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({ "kind": "tool.register", "tools": [{
            "name": "shell.script",
            "display": {
                "label": "Run command",
                "primary": { "arg": "command" },
                "result": { "kind": "content" }
            }
        }] }),
    );
    fixture_tool_started(
        &mut engine,
        "t1",
        "shell.script",
        json!({ "command": "ls -la /tmp" }),
    );
    fixture_tool_completed(&mut engine, "t1", json!("SECRET RAW OUTPUT"), false);

    let out = render_str(&mut engine);
    assert!(
        out.contains('▸') && out.contains("bash · ls -la /tmp"),
        "collapsed tool header missing: {out:?}"
    );
    assert!(!out.contains("SECRET RAW OUTPUT"), "{out:?}");

    engine.handle_key(key("ctrl_o")).expect("ctrl_o");
    let semantic = render_str(&mut engine);
    assert!(semantic.contains("▼ bash · ls -la /tmp"), "{semantic:?}");
    assert!(
        semantic.contains("completed") && semantic.contains("hidden"),
        "{semantic:?}"
    );
    assert!(semantic.contains("/raw t1 to reveal"), "{semantic:?}");
    assert!(!semantic.contains("SECRET RAW OUTPUT"), "{semantic:?}");

    engine.handle_key(key("ctrl_r")).expect("ctrl_r");
    let raw = render_str(&mut engine);
    assert!(raw.contains("SECRET RAW OUTPUT"), "{raw:?}");
    assert!(raw.contains("raw: visible"), "{raw:?}");

    engine.handle_key(key("ctrl_r")).expect("ctrl_r again");
    let hidden = render_str(&mut engine);
    assert!(!hidden.contains("SECRET RAW OUTPUT"), "{hidden:?}");

    dispatch_event(
        &mut engine,
        json!({ "kind": "input.submit", "value": "/raw t1" }),
    );
    let selected_raw = render_str(&mut engine);
    assert!(
        selected_raw.contains("SECRET RAW OUTPUT"),
        "{selected_raw:?}"
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "input.submit", "value": "/raw t1" }),
    );
    let selected_hidden = render_str(&mut engine);
    assert!(
        !selected_hidden.contains("SECRET RAW OUTPUT"),
        "{selected_hidden:?}"
    );

    engine.handle_key(key("ctrl_o")).expect("ctrl_o again");
    let collapsed = render_str(&mut engine);
    assert!(
        collapsed.contains('▸') && !collapsed.contains("raw:"),
        "{collapsed:?}"
    );
}

fn collapsed_read_file_snapshot(width: u16, running: bool) -> String {
    let mut engine = Engine::new(width, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);
    engine.handle_key(key("ctrl_b")).expect("hide sidebar");
    let _ = render_str(&mut engine);
    dispatch_event(
        &mut engine,
        json!({ "kind": "tool.register", "tools": [{
            "name": "read_file",
            "display": {
                "label": "Read file",
                "primary": { "arg": "path" },
                "result": { "kind": "content" }
            }
        }] }),
    );
    fixture_tool_started(
        &mut engine,
        "read-path",
        "read_file",
        json!({ "path": "/workspace/very/long/component/src/important.lua" }),
    );
    if !running {
        fixture_tool_completed(&mut engine, "read-path", json!("contents"), false);
    }
    render_snapshot(&mut engine)
}

fn collapsed_read_file_header(width: u16, running: bool) -> String {
    let snapshot = collapsed_read_file_snapshot(width, running);
    snapshot
        .lines()
        .find(|line| line.contains("▸ read_file · "))
        .unwrap_or_else(|| panic!("tool prefix must survive at width {width}: {snapshot:?}"))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn collapsed_completed_path_marks_only_actual_clipping() {
    let full = collapsed_read_file_header(80, false);
    assert_eq!(
        full,
        "▸ read_file · /workspace/very/long/component/src/important.lua"
    );

    let clipped = collapsed_read_file_header(49, false);
    assert_eq!(clipped, "▸ read_file · …/long/component/src/important.lua");
}

#[test]
fn collapsed_running_path_marks_clipping_before_reserved_marker() {
    let full = collapsed_read_file_header(80, true);
    assert_eq!(
        full,
        "▸ read_file · /workspace/very/long/component/src/important.lua …"
    );

    let clipped = collapsed_read_file_header(51, true);
    assert_eq!(
        clipped,
        "▸ read_file · …/long/component/src/important.lua …"
    );
}

#[test]
fn collapsed_path_with_one_display_column_shows_only_clipping_indicator() {
    let header = collapsed_read_file_header(16, false);
    assert_eq!(header, "▸ read_file · …");
}

#[test]
fn collapsed_mag_headers_show_action_and_filename_without_changing_expanded_header() {
    let mut engine = Engine::new(120, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({ "kind": "tool.register", "tools": [{
            "name": "mag",
            "display": {
                "label": "MAG",
                "primary": { "arg": "file" },
                "arguments": [{ "label": "action", "arg": "action" }],
                "result": { "kind": "content" }
            }
        }] }),
    );
    for (id, input) in [
        (
            "mag-write",
            json!({ "action": "write", "file": "draft.mag", "content": "graph" }),
        ),
        ("mag-compile", json!({ "file": "check.mag" })),
        (
            "mag-execute",
            json!({ "action": "execute", "file": "ship.mag" }),
        ),
    ] {
        fixture_tool_started(&mut engine, id, "mag", json!(input));
        fixture_tool_completed(&mut engine, id, json!("done"), false);
    }

    let collapsed = render_str(&mut engine);
    for expected in [
        "▸ mag write · draft.mag",
        "▸ mag compile · check.mag",
        "▸ mag execute · ship.mag",
    ] {
        assert!(
            collapsed.contains(expected),
            "missing {expected:?}: {collapsed:?}"
        );
    }

    engine.handle_key(key("ctrl_o")).expect("ctrl_o");
    let expanded = render_str(&mut engine);
    assert!(expanded.contains("▼ mag · draft.mag"), "{expanded:?}");
    assert!(expanded.contains("  action: write"), "{expanded:?}");
    assert!(
        !expanded.contains("▼ mag write · draft.mag"),
        "expanded MAG header must keep the shared tool-header shape: {expanded:?}"
    );
}

/// A denied canonical tool result flips the expanded block to a clearly denied
/// state instead of leaving an empty output line that reads as successful.
#[test]
fn denied_tool_call_renders_error_state_not_empty_output() {
    let mut engine = Engine::new(120, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    fixture_tool_started(
        &mut engine,
        "call_mock_ls",
        "shell.script",
        json!({ "command": "ls -la" }),
    );
    fixture_tool_completed(
        &mut engine,
        "call_mock_ls",
        json!("tool `bash` denied by user"),
        true,
    );

    // Failure diagnostics are visible even while the entry is collapsed.
    let collapsed = render_str(&mut engine);
    assert!(collapsed.contains("denied by user"), "{collapsed:?}");

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
    // The chat surface boots with `show_sidebar = true` (default:
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
    fixture_tool_started(
        &mut engine,
        "t1",
        "Read",
        json!({ "file_path": "/tmp/example.txt" }),
    );
    engine.handle_key(key("ctrl_o")).expect("ctrl_o expand");
    engine.handle_key(key("ctrl_r")).expect("ctrl_r reveal");
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
    fixture_assistant_delta(&mut engine, "hello");
    fixture_assistant_completed(
        &mut engine,
        None,
        json!({ "model": "test", "duration_ms": 100 }),
    );
    let out = render_str(&mut engine);
    assert!(
        !out.contains("[thinking"),
        "thinking placeholder should clear after stream end: {out:?}"
    );
    // Legacy spec section 4 shows the turn duration as a bare segment
    // (`100ms`, `2s`, etc.) — no `[done in ...]` brackets. The previous
    // behavior added an extra status_ok segment that duplicated the duration.
    assert!(
        out.contains("100ms"),
        "turn duration missing on statusline: {out:?}"
    );
    assert!(
        !out.contains("[done in"),
        "no [done in ...] segment, just bare duration: {out:?}"
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
            "braille spinner glyph '{braille}' present (no spinner): {out:?}"
        );
    }
}

#[test]
fn double_escape_stops_only_the_lead() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_started", "run_id": "mag-run-1" }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "mag.actor_spawned",
            "run_id": "mag-run-1",
            "id": "n1",
            "factory": "llm",
        }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_busy", "run_id": "mag-run-1", "id": "n1" }),
    );
    let _ = engine.take_emit_queue();

    engine.handle_key(key("escape")).expect("first esc");
    assert!(
        engine.take_emit_queue().is_empty(),
        "first ESC while lead is idle only arms the run-cancel rung"
    );

    engine.handle_key(key("escape")).expect("second esc");
    let second = engine.take_emit_queue();
    assert_eq!(
        second[0].1.get("kind").and_then(|v| v.as_str()),
        Some("chat.interrupt"),
        "second ESC should hard-stop the lead"
    );
    assert_eq!(
        second[0].1.get("drop_queued").and_then(|v| v.as_bool()),
        Some(true)
    );
}

#[test]
fn triple_escape_immediately_kills_every_workflow_including_lead() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    submit_text(&mut engine, "first");
    let _ = engine.take_emit_queue();
    let _ = render_str(&mut engine);
    submit_text(&mut engine, "queued");
    let _ = engine.take_emit_queue();

    engine.handle_key(key("escape")).expect("first esc");
    assert!(engine.take_emit_queue().is_empty());

    engine.handle_key(key("escape")).expect("second esc");
    let second = engine.take_emit_queue();
    assert_eq!(
        second[0].1.get("kind").and_then(|v| v.as_str()),
        Some("chat.interrupt"),
        "second ESC still stops lead immediately"
    );
    let _ = render_str(&mut engine);

    engine.handle_key(key("escape")).expect("third esc");
    let third = engine.take_emit_queue();
    let kinds: Vec<_> = third
        .iter()
        .filter_map(|(_, body)| body.get("kind").and_then(|v| v.as_str()))
        .collect();
    assert_eq!(
        kinds,
        vec![
            "chat.interrupt",
            "chat.workflows.terminate_requested",
            "mag.kill_all_runs"
        ],
        "third ESC must take the unconditional global kill path"
    );
    assert_eq!(third[1].0.as_deref(), Some("engine"));
    assert_eq!(
        third[1].1.get("scope").and_then(|v| v.as_str()),
        Some("all")
    );
    assert_eq!(third[2].0.as_deref(), Some("mag"));

    type_text(&mut engine, "tail");
    let out = render_snapshot(&mut engine);
    assert!(
        out.contains("queued tail"),
        "triple ESC must retain double-ESC queue restoration with a trailing space: {out:?}"
    );
}

#[test]
fn selected_workflow_termination_emits_classification_before_kill() {
    let mut engine = Engine::new(100, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_started", "run_id": "run-one", "run_name": "one", "principal": "subagent" }),
    );
    let _ = render_str(&mut engine);
    engine.handle_key(key("tab")).expect("focus sidebar");
    let _ = render_str(&mut engine);
    engine.handle_key(key("x")).expect("open terminate popup");
    assert!(render_snapshot(&mut engine).contains("Terminate one?"));

    engine.handle_key(key("enter")).expect("confirm popup");
    let emits = engine.take_emit_queue();
    let kinds: Vec<_> = emits
        .iter()
        .filter_map(|(_, body)| body.get("kind").and_then(|v| v.as_str()))
        .collect();
    assert_eq!(
        kinds,
        vec!["chat.workflows.terminate_requested", "mag.kill_run"]
    );
    assert_eq!(emits[0].0.as_deref(), Some("engine"));
    assert_eq!(
        emits[0].1.get("scope").and_then(|v| v.as_str()),
        Some("one")
    );
    assert_eq!(
        emits[0].1.get("run_id").and_then(|v| v.as_str()),
        Some("run-one")
    );
    assert_eq!(emits[1].0.as_deref(), Some("mag"));
    assert_eq!(
        emits[1].1.get("run_id").and_then(|v| v.as_str()),
        Some("run-one")
    );
}

#[test]
fn x_on_selected_lead_restores_queue_and_hard_stops_it() {
    let mut engine = Engine::new(100, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);
    submit_text(&mut engine, "first");
    let _ = engine.take_emit_queue();
    submit_text(&mut engine, "queued");
    let _ = render_str(&mut engine);
    type_text(&mut engine, "message");
    let _ = engine.take_emit_queue();
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_started", "run_id": "lead-run", "run_name": "lead", "principal": "lead" }),
    );
    let _ = render_str(&mut engine);
    engine.handle_key(key("tab")).expect("focus sidebar");
    let _ = render_str(&mut engine);
    engine.handle_key(key("x")).expect("open terminate popup");
    engine.handle_key(key("enter")).expect("confirm");

    let emits = engine.take_emit_queue();
    assert_eq!(emits.len(), 1);
    assert_eq!(
        emits[0].1.get("kind").and_then(|v| v.as_str()),
        Some("chat.interrupt")
    );
    assert_eq!(
        emits[0].1.get("drop_queued").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert!(render_snapshot(&mut engine).contains("queued message"));
}

#[test]
fn uppercase_x_terminates_all_runs_including_lead() {
    let mut engine = Engine::new(100, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);
    for (run_id, principal) in [("lead-run", "lead"), ("sub-run", "subagent")] {
        dispatch_event(
            &mut engine,
            json!({ "kind": "mag.run_started", "run_id": run_id, "run_name": run_id, "principal": principal }),
        );
    }
    let _ = render_str(&mut engine);
    engine.handle_key(key("tab")).expect("focus sidebar");
    let _ = render_str(&mut engine);
    engine
        .handle_key(key("X"))
        .expect("open terminate-all popup");
    engine.handle_key(key("enter")).expect("confirm");

    let emits = engine.take_emit_queue();
    let kinds: Vec<_> = emits
        .iter()
        .filter_map(|(_, body)| body.get("kind").and_then(|v| v.as_str()))
        .collect();
    assert_eq!(
        kinds,
        vec![
            "chat.interrupt",
            "chat.workflows.terminate_requested",
            "mag.kill_all_runs"
        ]
    );
    assert_eq!(emits[1].0.as_deref(), Some("engine"));
    assert_eq!(
        emits[1].1.get("scope").and_then(|v| v.as_str()),
        Some("all")
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
    let popup_body_idx = rows
        .iter()
        .position(|r| r.contains("│ Keys:"))
        .expect("first bordered popup body row");
    let popup_body_chars: Vec<char> = rows[popup_body_idx].chars().collect();
    let popup_left_col = popup_body_chars
        .iter()
        .position(|&c| c == '│')
        .expect("popup left border column");
    let popup_right_col = popup_body_chars
        .iter()
        .rposition(|&c| c == '│')
        .expect("popup right border column");

    // Every body row of the popup must carry side bars at the popup's
    // left + right edges. Iterate until we hit the popup's bottom rule
    // (`╰────╯`); every row in between must have `│` at both edges, and
    // the bottom rule itself must be present (full enclosure — the
    // overflow regression that motivated the scrollable wrap).
    let mut body_rows_seen = 0;
    let mut popup_bottom_idx: Option<usize> = None;
    for (i, row) in rows.iter().enumerate().skip(popup_body_idx) {
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
        "popup bottom rule `╰────╯` not found below first body row {popup_body_idx} — \
         the help popup must be fully enclosed (top + bottom rules):\n{snap}"
    );
    assert!(
        body_rows_seen >= 5,
        "expected ≥ 5 popup body rows with side bars (saw {body_rows_seen}); \
         the help popup is multi-line by construction:\n{snap}"
    );
}

#[test]
fn slash_permission_modes_emit_tool_gate_set_mode() {
    for mode in ["safe", "auto", "yolo"] {
        let mut engine = Engine::new(80, 24).expect("engine");
        engine.load_scenario(&chat_lua_source()).expect("load");
        let _ = render_str(&mut engine);

        for ch in format!("/{mode}").chars() {
            engine.handle_key(key(&ch.to_string())).expect("type");
        }
        engine.handle_key(key("enter")).expect("enter");
        let emits = engine.take_emit_queue();
        let emit = emits
            .iter()
            .find(|(_, body)| {
                body.get("kind").and_then(|v| v.as_str()) == Some("tool-gate.set_mode")
            })
            .unwrap_or_else(|| panic!("expected tool-gate.set_mode for /{mode}; got {emits:?}"));
        assert_eq!(emit.1.get("mode").and_then(|v| v.as_str()), Some(mode));
    }
}

#[test]
fn statusline_shows_current_permission_mode() {
    for (mode, label) in [("safe", "SAFE"), ("auto", "AUTO"), ("yolo", "YOLO")] {
        let mut engine = Engine::new(80, 24).expect("engine");
        engine.load_scenario(&chat_lua_source()).expect("load");
        let _ = render_str(&mut engine);

        dispatch_event(
            &mut engine,
            json!({
                "kind": "tool-gate.mode_changed",
                "mode": mode,
            }),
        );
        let _ = render_str(&mut engine);
        let out = engine.snapshot();
        assert!(
            out.contains(label),
            "statusline should show current permission mode {mode:?}: {out:?}"
        );
    }
}

#[test]
fn mag_human_approval_is_run_addressed_and_cancel_safe() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({
            "kind": "mag.approval_request", "run_id": "run-dead", "from": "gate-a",
            "correlation": "approval-a", "prompt": "Ship it?", "subject": { "plan": "A" }
        }),
    );
    assert!(render_str(&mut engine).contains("Ship it?"));

    // The kernel emits this for hard kill, terminate, and run teardown. Once
    // retracted, a key intended for the stale popup cannot reach the dead run.
    dispatch_event(
        &mut engine,
        json!({
            "kind": "mag.approval_cancel", "run_id": "run-dead", "from": "gate-a",
            "correlation": "approval-a"
        }),
    );
    let _ = engine.take_emit_queue();
    engine.handle_key(key("a")).expect("late approval key");
    assert!(
        engine.take_emit_queue().is_empty(),
        "late approval must emit no mag.apply"
    );

    // A future request still works and carries both run and gate addressing.
    dispatch_event(
        &mut engine,
        json!({
            "kind": "mag.approval_request", "run_id": "run-live", "from": "gate-b",
            "correlation": "approval-b", "prompt": "Continue?", "subject": { "plan": "B" }
        }),
    );
    engine.handle_key(key("a")).expect("approve");
    let emits = engine.take_emit_queue();
    assert_eq!(emits.len(), 1);
    let body = serde_json::Value::Object(emits[0].1.clone());
    assert_eq!(body.get("kind").and_then(|v| v.as_str()), Some("mag.apply"));
    assert_eq!(
        body.get("run_id").and_then(|v| v.as_str()),
        Some("run-live")
    );
    assert_eq!(
        body.pointer("/modification/messages/0/to")
            .and_then(|v| v.as_str()),
        Some("gate-b")
    );
    assert_eq!(
        body.pointer("/modification/messages/0/content/kind")
            .and_then(|v| v.as_str()),
        Some("mag.ApprovalReply")
    );
    assert_eq!(
        body.pointer("/modification/messages/0/content/approved")
            .and_then(|v| v.as_bool()),
        Some(true)
    );
}

#[test]
fn mag_approval_cancel_only_retracts_its_correlated_popup() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    for (run, gate, correlation, prompt) in [
        ("run-a", "gate-a", "approval-a", "First approval"),
        ("run-b", "gate-b", "approval-b", "Unrelated approval"),
    ] {
        dispatch_event(
            &mut engine,
            json!({
                "kind": "mag.approval_request", "run_id": run, "from": gate,
                "correlation": correlation, "prompt": prompt, "subject": {}
            }),
        );
    }
    dispatch_event(
        &mut engine,
        json!({
            "kind": "mag.approval_cancel", "run_id": "run-a", "from": "gate-a",
            "correlation": "approval-a"
        }),
    );
    let out = render_str(&mut engine);
    assert!(
        out.contains("Unrelated approval"),
        "queued unrelated approval must advance: {out:?}"
    );
    engine.handle_key(key("d")).expect("deny unrelated");
    let emits = engine.take_emit_queue();
    assert_eq!(emits.len(), 1);
    assert_eq!(
        emits[0].1.get("run_id").and_then(|v| v.as_str()),
        Some("run-b")
    );
    let body = serde_json::Value::Object(emits[0].1.clone());
    assert_eq!(
        body.pointer("/modification/messages/0/content/approved")
            .and_then(|v| v.as_bool()),
        Some(false)
    );
}

#[test]
fn canonical_turn_completion_reconciles_pending_before_mag_result() {
    let mut engine = Engine::new(100, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    submit_text(&mut engine, "first turn");
    let _ = engine.take_emit_queue();
    dispatch_event(
        &mut engine,
        json!({
            "kind": "mag.run_started", "run_id": "lead-run",
            "run_name": "lead", "principal": "lead"
        }),
    );
    fixture_assistant_completed(
        &mut engine,
        Some("provider answer".into()),
        json!({ "model": "mock-model", "duration_ms": 1 }),
    );
    let streamed = render_str(&mut engine);
    assert!(
        streamed.contains("provider answer"),
        "answer missing: {streamed}"
    );
    assert!(
        !streamed.contains("[thinking"),
        "canonical turn completion must settle transcript pending state: {streamed}"
    );

    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_complete", "run_id": "lead-run" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_result", "run_id": "lead-run", "status": "completed" }),
    );
    let settled = render_str(&mut engine);
    assert!(
        !settled.contains("[thinking"),
        "MAG lifecycle must not disturb the canonical terminal: {settled}"
    );

    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_result", "run_id": "lead-run", "status": "completed" }),
    );
    let duplicate = render_str(&mut engine);
    assert!(!duplicate.contains("[thinking"));
}

#[test]
fn mag_result_before_canonical_completion_does_not_duplicate_assistant() {
    let mut engine = Engine::new(100, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    submit_text(&mut engine, "first turn");
    let _ = engine.take_emit_queue();
    dispatch_event(
        &mut engine,
        json!({
            "kind": "mag.run_started", "run_id": "lead-run",
            "run_name": "lead", "principal": "lead"
        }),
    );
    fixture_assistant_delta(&mut engine, "provider answer");
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_complete", "run_id": "lead-run" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_result", "run_id": "lead-run", "status": "completed" }),
    );

    fixture_assistant_completed(
        &mut engine,
        Some("provider answer".into()),
        json!({ "model": "mock-model", "duration_ms": 1_000 }),
    );
    let settled = render_snapshot(&mut engine);
    assert_eq!(
        settled.matches("provider answer").count(),
        1,
        "canonical completion must finalize the streamed entry in place:\n{settled}"
    );
    assert_eq!(settled.matches("▣ mock-model · 1s").count(), 1, "{settled}");
}

#[test]
fn abnormal_lead_close_settles_partial_reasoning_and_text() {
    let mut engine = Engine::new(100, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    submit_text(&mut engine, "reasoning turn");
    let _ = engine.take_emit_queue();
    dispatch_event(
        &mut engine,
        json!({
            "kind": "mag.run_started", "run_id": "lead-run",
            "run_name": "lead", "principal": "lead"
        }),
    );
    fixture_reasoning_delta(&mut engine, "partial reasoning");
    fixture_assistant_delta(&mut engine, "partial answer");
    fixture_assistant_completed(
        &mut engine,
        None,
        json!({
            "model": "review-model", "duration_ms": 12,
            "usage": { "output_tokens": 3 }
        }),
    );
    assert!(render_snapshot(&mut engine).contains("partial answer"));

    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_result", "run_id": "lead-run", "status": "failed" }),
    );
    let settled = render_snapshot(&mut engine);
    assert!(
        settled.contains("partial answer"),
        "partial text is preserved: {settled}"
    );
    assert!(
        settled.contains("reasoning"),
        "reasoning remains available: {settled}"
    );
    assert!(
        settled.contains("review-model"),
        "request-owned model survives closure: {settled}"
    );
    assert!(
        settled.contains("250 tok/s"),
        "projected turn stats survive closure: {settled}"
    );
    assert!(
        !settled.contains("thinking"),
        "reasoning is no longer active: {settled}"
    );
    assert!(
        !settled.contains("[thinking"),
        "placeholder is cleared: {settled}"
    );

    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_result", "run_id": "lead-run", "status": "failed" }),
    );
    let duplicate = render_snapshot(&mut engine);
    assert!(duplicate.contains("partial answer"));
    assert!(!duplicate.contains("thinking"));
}

#[test]
fn mag_terminal_results_do_not_close_the_lead_placeholder() {
    let mut engine = Engine::new(100, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    submit_text(&mut engine, "lead turn");
    let _ = engine.take_emit_queue();
    for (run_id, principal) in [("lead-run", "lead"), ("sub-run", "subagent")] {
        dispatch_event(
            &mut engine,
            json!({
                "kind": "mag.run_started", "run_id": run_id,
                "run_name": run_id, "principal": principal
            }),
        );
    }
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_result", "run_id": "sub-run", "status": "completed" }),
    );
    assert!(render_snapshot(&mut engine).contains("[thinking"));

    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_result", "run_id": "lead-run", "status": "killed" }),
    );
    assert!(render_snapshot(&mut engine).contains("[thinking"));
}

#[test]
fn terminal_run_and_session_cleanup_retract_mag_approvals() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);
    dispatch_event(
        &mut engine,
        json!({
            "kind": "mag.approval_request", "run_id": "run-terminal", "from": "gate",
            "correlation": "approval", "prompt": "Stale", "subject": {}
        }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "mag.run_result", "run_id": "run-terminal", "status": "killed"
        }),
    );
    assert!(!render_str(&mut engine).contains("Stale"));

    dispatch_event(
        &mut engine,
        json!({
            "kind": "mag.approval_request", "run_id": "run-session", "from": "gate",
            "correlation": "approval-session", "prompt": "Session stale", "subject": {}
        }),
    );
    dispatch_event(&mut engine, json!({ "kind": "sessions.session_end" }));
    assert!(!render_str(&mut engine).contains("Session stale"));
    engine.handle_key(key("a")).expect("late session approval");
    assert!(engine.take_emit_queue().is_empty());
}

#[test]
fn tool_permission_request_opens_popup_with_approve_deny() {
    // Wire-shape contract: the event the popup listens for is
    // `chat.tool.popup_request` — emitted by examples/nefor-agent/tool-validator
    // after it has chosen NOT to auto-approve or auto-deny. tool-gate's
    // `chat.tool.permission_request` goes to the validator; only the
    // validator's popup_request reaches the chat surface.
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.tool.popup_request",
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
            "kind": "chat.tool.popup_request",
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
            "kind": "chat.tool.popup_request",
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
            "kind": "chat.tool.popup_request",
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
    // `chat.model.list_requested` per connected provider (
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

    // `/model` is the sole canonical match. Enter runs the highlighted entry,
    // not the typed fragment `mo`.
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
            "fan-out must include `provider` field per provider contract: {:?}",
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
    // `/model` is the sole canonical match for this prefix.
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
fn slash_think_emits_reasoning_effort_set() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    type_text(&mut engine, "/think high");
    engine.handle_key(key("enter")).expect("enter");

    let emits = engine.take_emit_queue();
    assert_eq!(emits.len(), 1, "expected one egress");
    assert_eq!(
        emits[0].1.get("kind").and_then(|v| v.as_str()),
        Some("chat.reasoning.set")
    );
    assert_eq!(
        emits[0].1.get("effort").and_then(|v| v.as_str()),
        Some("high")
    );
}

#[test]
fn slash_compact_renders_pending_entry_until_commit() {
    let mut engine = Engine::new(100, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.models.listed", "provider": "mock-plugin",
            "models": ["mock-model"], "context_windows": { "mock-model": 128_000 }
        }),
    );
    fixture_assistant_completed(
        &mut engine,
        Some("prior answer".into()),
        json!({
            "model": "mock-model", "usage": { "input_tokens": 80_000 }
        }),
    );
    let before = render_str(&mut engine);
    assert!(
        before.contains("ctx 80k/128k"),
        "precondition: statusline should show the completed turn's context: {before:?}"
    );

    submit_text(&mut engine, "/compact");

    let out = render_str(&mut engine);
    assert!(
        out.contains("context compacting..."),
        "/compact should show immediate pending feedback: {out:?}"
    );

    let emits = engine.take_emit_queue();
    assert_eq!(emits.len(), 1, "expected one compaction request");
    assert_eq!(
        emits[0].1.get("kind").and_then(|v| v.as_str()),
        Some("chat.compaction.request")
    );

    fixture_compaction(
        &mut engine,
        "completed",
        json!({
                "request_id": "fixture-compaction",
                "status": "completed",
                "provider": "mock-plugin",
                "model": "mock-model",
                "trigger": "manual",
                "display_summary": "Kept the important bits.",
                "metadata": { "before_items": 7, "after_items": 2 },
        }),
    );

    let out = render_str(&mut engine);
    assert!(
        out.contains("context compacted"),
        "commit should replace the pending label: {out:?}"
    );
    let snapshot = engine.snapshot();
    let separator_row = snapshot
        .lines()
        .find(|line| line.contains("context compacted · manual · mock-plugin/mock-model"))
        .expect("completed compaction separator row");
    assert!(
        separator_row.matches('─').count() >= 8,
        "completed compaction label should share a full-width separator row: {separator_row:?}"
    );
    assert!(
        out.contains("ctx 128k") && !out.contains("ctx 80k/128k"),
        "commit should clear the stale current-context count immediately: {out:?}"
    );
    assert!(
        out.contains("Kept the important bits."),
        "commit summary should be rendered: {out:?}"
    );
    assert!(
        !out.contains("context compacting..."),
        "pending compaction entry should be replaced, not duplicated: {out:?}"
    );
}

#[test]
fn slash_compact_replaces_pending_entry_with_failure() {
    let mut engine = Engine::new(100, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    submit_text(&mut engine, "/compact");
    fixture_compaction(
        &mut engine,
        "failed",
        json!({
                "request_id": "fixture-compaction",
                "status": "failed",
                "provider": "chatgpt",
                "model": "gpt-5.6-sol",
                "trigger": "manual",
                "error": "nothing to compact",
        }),
    );

    let out = render_str(&mut engine);
    assert!(
        out.contains("context compaction failed"),
        "failed compaction should replace the pending label: {out:?}"
    );
    assert!(
        out.contains("nothing to compact"),
        "failed compaction should show the provider/orchestrator error: {out:?}"
    );
    assert!(
        !out.contains("context compacting..."),
        "failed compaction must not leave a permanent pending entry: {out:?}"
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

// ──────────────────────────────────────────────────────────────────────
// @-path autocomplete — mirrors the slash autocomplete machinery but
// the completion source is the filesystem under CWD. Trigger fires on
// the trailing `@<token>` (last whitespace-separated word starts
// with `@`); selection inserts the path back into the input.
// ──────────────────────────────────────────────────────────────────────

/// CWD is process-global, so tests that mutate it must serialise.
/// Each `CwdSwitch` instance changes CWD to the provided path on
/// construction and restores it (to a stable repo path, not whatever
/// `current_dir` returned, since concurrent tests may have left CWD
/// pointing at a since-deleted tempdir) on Drop. The Drop also
/// releases the inner Mutex guard, freeing the next test.
static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct CwdSwitch {
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl CwdSwitch {
    fn to(path: &std::path::Path) -> Self {
        let guard = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_current_dir(path).expect("chdir into fixture");
        Self { _guard: guard }
    }
}

impl Drop for CwdSwitch {
    fn drop(&mut self) {
        // CARGO_MANIFEST_DIR is stable for the test binary's lifetime
        // so it's the safe restore target — even if the original cwd
        // was deleted by a sibling test, the next test starts from a
        // real directory.
        let safe = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let _ = std::env::set_current_dir(&safe);
    }
}

/// Set the chat-test process CWD to a tempdir populated with a known
/// fixture tree, returning the dir handle (callers must keep it alive
/// for the test's duration so the directory survives until ls runs).
fn at_complete_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("src")).expect("mkdir src");
    std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").expect("write main.rs");
    std::fs::write(dir.path().join("src/macro.rs"), "// mac").expect("write macro.rs");
    std::fs::write(dir.path().join("src/lib.rs"), "// lib").expect("write lib.rs");
    std::fs::create_dir_all(dir.path().join("docs")).expect("mkdir docs");
    std::fs::write(dir.path().join("docs/spec.md"), "# spec").expect("write spec.md");
    std::fs::write(dir.path().join("README.md"), "# readme").expect("write readme");
    std::fs::create_dir_all(dir.path().join(".git")).expect("mkdir .git");
    std::fs::write(dir.path().join(".git/HEAD"), "ref:").expect("write head");
    dir
}

#[test]
fn at_path_autocomplete_opens_when_typing_at_sign() {
    let fixture = at_complete_fixture();
    let _cwd = CwdSwitch::to(fixture.path());

    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    engine.handle_key(key("@")).expect("@");
    let out = render_str(&mut engine);

    // CWD-level entries should appear: README.md and src/ at minimum.
    // .git is on the ignore allowlist; it must not surface.
    assert!(
        out.contains("README.md"),
        "@ autocomplete should list README.md at CWD: {out:?}"
    );
    assert!(
        out.contains("src/"),
        "@ autocomplete should list src/ as a directory: {out:?}"
    );
    assert!(
        !out.contains(".git"),
        "@ autocomplete must not list .git directory: {out:?}"
    );
}

#[test]
fn at_path_autocomplete_does_not_break_slash_popup() {
    // Belt-and-braces: typing `/` from empty input still opens the
    // slash popup; the @-popup wiring must not interfere.
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    engine.handle_key(key("/")).expect("/");
    let out = render_str(&mut engine);
    assert!(
        out.contains("/new") || out.contains("/help"),
        "slash autocomplete should still open after @-completion wiring: {out:?}"
    );
    // No `@`-popup artefacts.
    assert!(
        !out.contains("no matching paths"),
        "slash popup must not double-render with @-popup empty state: {out:?}"
    );
}

#[test]
fn at_path_autocomplete_filters_by_leaf_prefix_in_subdir() {
    // Typing `@src/m` should list only `src/` entries whose name
    // starts with `m` (case-insensitive). The fixture has
    // `src/main.rs`, `src/macro.rs`, `src/lib.rs` — `m` matches the
    // first two, NOT lib.rs.
    let fixture = at_complete_fixture();
    let _cwd = CwdSwitch::to(fixture.path());

    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    for ch in "@src/m".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    let out = render_str(&mut engine);

    assert!(
        out.contains("main.rs"),
        "@src/m should list main.rs: {out:?}"
    );
    assert!(
        out.contains("macro.rs"),
        "@src/m should list macro.rs: {out:?}"
    );
    assert!(
        !out.contains("lib.rs"),
        "@src/m should NOT list lib.rs (no `m` prefix): {out:?}"
    );
}

#[test]
fn at_path_autocomplete_navigation_into_subdir_shows_subdir_contents() {
    // `@src/` (trailing slash, no leaf) should show the contents of
    // src/, not CWD. Mirrors bash tab-completion intuition.
    let fixture = at_complete_fixture();
    let _cwd = CwdSwitch::to(fixture.path());

    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    for ch in "@src/".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    let out = render_str(&mut engine);

    assert!(
        out.contains("main.rs"),
        "@src/ should show src contents (main.rs missing): {out:?}"
    );
    assert!(
        out.contains("lib.rs"),
        "@src/ should show src contents (lib.rs missing): {out:?}"
    );
    // README.md is at CWD, not under src/, so it MUST NOT appear in
    // the subdir listing.
    assert!(
        !out.contains("README.md"),
        "@src/ should NOT show CWD entries (README.md leaked): {out:?}"
    );
}

#[test]
fn at_path_autocomplete_tab_inserts_selected_match_into_input() {
    // Tab on the @-popup replaces the trailing @<token> with the
    // resolved path. We verify by submitting after Tab and inspecting
    // the wire envelope's text — the @-path preprocessor inlines the
    // file contents, so post-Tab + Enter must produce a wire payload
    // carrying the resolved file's body.
    //
    // ResumeEnv isolates the on-disk input-history so this test's
    // submit doesn't pollute siblings (the failing-prereq surface in
    // serial test runs).
    let _env = ResumeEnv::new();
    let fixture = at_complete_fixture();
    let _cwd = CwdSwitch::to(fixture.path());

    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Type `@READ`, popup highlights README.md (the only match).
    for ch in "@READ".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    let _ = render_str(&mut engine);

    // Tab inserts the path → input value becomes `@README.md`.
    engine.handle_key(key("tab")).expect("tab");
    // Tab on its own does NOT submit.
    let emits_after_tab = engine.take_emit_queue();
    assert!(
        emits_after_tab.is_empty(),
        "Tab on @-popup must not submit: {emits_after_tab:?}"
    );

    // Render to give the text_input widget a chance to sync from
    // the Lua-side input_value the Tab handler wrote — handle_key
    // doesn't reconcile, so the widget's internal value is one
    // sync-cycle behind without an interleaving render.
    let _ = render_str(&mut engine);

    // Enter now submits with the @-token expanded by the existing
    // @path preprocessor → wire text contains the file contents.
    engine.handle_key(key("enter")).expect("enter");
    let emits = engine.take_emit_queue();
    assert_eq!(
        emits.len(),
        1,
        "Tab+Enter should produce exactly one chat.input.submit: {emits:?}"
    );
    let body = &emits[0].1;
    let wire = body
        .get("text")
        .and_then(|v| v.as_str())
        .expect("text on envelope");
    assert!(
        wire.contains("# readme"),
        "wire text should contain README.md contents after Tab inserted the path: {wire:?}"
    );
}

#[test]
fn at_path_autocomplete_escape_closes_popup() {
    let fixture = at_complete_fixture();
    let _cwd = CwdSwitch::to(fixture.path());

    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    engine.handle_key(key("@")).expect("@");
    let out = render_str(&mut engine);
    assert!(out.contains("README.md"), "@ should open popup: {out:?}");

    engine.handle_key(key("escape")).expect("esc");
    let out2 = render_str(&mut engine);
    assert!(
        !out2.contains("README.md") || !out2.contains("src/"),
        "Escape should close @-popup: {out2:?}"
    );
}

#[test]
fn at_path_autocomplete_triggers_mid_message_not_only_at_start() {
    // Per spec: trigger fires on the *trailing* `@<token>`, not just at
    // column 0. Type a sentence, then `@` — the popup must open.
    let fixture = at_complete_fixture();
    let _cwd = CwdSwitch::to(fixture.path());

    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    for ch in "summarize @".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    let out = render_str(&mut engine);
    assert!(
        out.contains("README.md"),
        "@-popup should open when @ appears mid-message: {out:?}"
    );
}

#[test]
fn at_path_autocomplete_arrow_keys_move_cursor() {
    // Down then Tab inserts the second match, not the first.
    let _env = ResumeEnv::new();
    let fixture = at_complete_fixture();
    let _cwd = CwdSwitch::to(fixture.path());

    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // `@src/` lists src/ contents alphabetically with dirs-first.
    // Fixture: lib.rs, macro.rs, main.rs (no subdirs under src) →
    // cursor at lib.rs. Down → macro.rs.
    for ch in "@src/".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    let _ = render_str(&mut engine);
    engine.handle_key(key("down")).expect("down");
    let _ = render_str(&mut engine);
    engine.handle_key(key("tab")).expect("tab");
    let _ = render_str(&mut engine);
    engine.handle_key(key("enter")).expect("enter");

    let emits = engine.take_emit_queue();
    assert_eq!(emits.len(), 1, "submit emit");
    let wire = emits[0]
        .1
        .get("text")
        .and_then(|v| v.as_str())
        .expect("text");
    assert!(
        wire.contains("// mac"),
        "Down+Tab on @src/ should select macro.rs (2nd alphabetical), \
         wire contents should be `// mac`: {wire:?}"
    );
}

/// Lua-level smoke that `nefor.fs.list_dir` is wired into the same VM
/// that hosts chat.lua. Targets the chat.lua-facing contract: a string
/// path in, a `{ { name, is_dir }, ... }` table out (or `(nil, err)`
/// on failure). Guards against a future reinstall regression where
/// `install_fs` gets dropped from `LuaHost::new` and chat.lua's
/// `ls_entries` silently falls back to empty-with-no-error.
#[test]
fn nefor_fs_list_dir_binding_is_available_in_chat_lua_vm() {
    let fixture = at_complete_fixture();
    let _cwd = CwdSwitch::to(fixture.path());

    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");

    let lua = engine.lua();
    let (has_readme, has_src_dir, missing_path_is_nil): (bool, bool, bool) = lua
        .load(
            r#"
            assert(nefor and type(nefor) == "table", "nefor global missing")
            assert(nefor.fs and type(nefor.fs) == "table", "nefor.fs missing")
            assert(type(nefor.fs.list_dir) == "function", "nefor.fs.list_dir missing")
            local entries = nefor.fs.list_dir(".")
            local has_readme, has_src_dir = false, false
            for _, e in ipairs(entries) do
              if e.name == "README.md" and e.is_dir == false then has_readme = true end
              if e.name == "src" and e.is_dir == true then has_src_dir = true end
            end
            local missing, err = nefor.fs.list_dir("/this/path/does/not/exist")
            return has_readme, has_src_dir, missing == nil and type(err) == "string"
            "#,
        )
        .eval()
        .expect("eval nefor.fs.list_dir checks");

    assert!(
        has_readme,
        "fixture's README.md should appear as is_dir=false"
    );
    assert!(has_src_dir, "fixture's src/ should appear as is_dir=true");
    assert!(
        missing_path_is_nil,
        "missing path should yield (nil, err_string)"
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
        fixture_message(&mut engine, "user", "x");
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
    //
    // Hold a `ResumeEnv` so chat.lua's `load_input_history` reads from
    // a clean tempdir — without it, the developer's $HOME/.local/share/
    // nefor/input-history could prefill `prompt_history`, and the
    // arrow-up handler would route to history-recall instead of
    // scroll. Issue #39 added the disk-load on init.
    let _env = ResumeEnv::new();
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Pump enough transcript content that there's something to scroll up
    // through. Auto-scroll keeps the transcript pinned to the bottom, so
    // the offset is positive after the deltas land.
    for _ in 0..40 {
        fixture_message(&mut engine, "user", "x");
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
    //
    // ResumeEnv pins an empty input-history (issue #39 hydrates
    // chat.lua's prompt_history from disk on init); without it, a
    // populated $HOME history would route arrow-up to recall instead
    // of scroll.
    let _env = ResumeEnv::new();
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    for _ in 0..40 {
        fixture_message(&mut engine, "user", "x");
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
        fixture_message(&mut engine, "user", "x");
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
    // The statusline sits BELOW the input box. Verify
    // by rendering and walking rows: the input box's bottom-right
    // corner `╯` lies above the statusline, not below it.
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    fixture_assistant_completed(
        &mut engine,
        Some("answer".into()),
        json!({ "model": "claude-test" }),
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
    // hidden entirely ( section 4: "Only rendered when total
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
fn statusline_omits_scroll_marker_when_transcript_overflows() {
    // The scrollbar is the sole scroll-position indicator; duplicating
    // it as footer text adds noise even when the transcript overflows.
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);
    for i in 0..30 {
        fixture_message(&mut engine, "user", format!("line-{i}"));
    }
    let _ = render_snapshot(&mut engine);
    let snap = render_snapshot(&mut engine);
    assert!(
        !snap.contains("100% ↓ bottom") && !snap.contains("0% ↑"),
        "scroll position should not be duplicated in the footer:\n{snap}"
    );
}

#[test]
fn usage_snapshot_updates_footer_and_usage_command_opens_details() {
    let mut engine = Engine::new(120, 28).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.auth.status",
            "provider": "mock-plugin",
            "state": "connected",
            "supports_usage": true,
        }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.usage.updated",
            "provider": "mock-plugin",
            "plan_type": "pro",
            "rate_limit": {
                "primary_window": {
                    "used_percent": 66,
                    "limit_window_seconds": 18000,
                    "reset_at": 1770000000
                },
                "secondary_window": {
                    "used_percent": 20,
                    "limit_window_seconds": 604800,
                    "reset_at": 1770500000
                }
            }
        }),
    );
    let snapshot = render_snapshot(&mut engine);
    assert!(
        snapshot.contains("◔ 34% until"),
        "compact available-quota widget missing: {snapshot}"
    );

    // Per-turn response headers only carry the primary percentage.
    // Merging that sparse event must retain reset time and weekly data
    // from the last full endpoint response.
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.usage.updated",
            "provider": "mock-plugin",
            "rate_limit": { "primary_window": { "used_percent": 70 } }
        }),
    );
    let sparse_snapshot = render_snapshot(&mut engine);
    assert!(
        sparse_snapshot.contains("◔ 30% until"),
        "sparse usage refresh should retain the reset time: {sparse_snapshot}"
    );

    for ch in "/usage".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    let emits = engine.take_emit_queue();
    assert!(emits.iter().any(|(_, body)| {
        body.get("kind").and_then(|v| v.as_str()) == Some("chat.usage.requested")
            && body.get("provider").and_then(|v| v.as_str()) == Some("mock-plugin")
    }));
    let popup = render_snapshot(&mut engine);
    assert!(
        popup.contains("5-hour window"),
        "primary usage details missing: {popup}"
    );
    assert!(
        popup.contains("Weekly window"),
        "weekly usage details missing: {popup}"
    );
}

#[test]
fn completed_turn_footer_absorbs_canonical_token_speed() {
    let mut engine = Engine::new(100, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);
    fixture_assistant_delta(&mut engine, "done");
    fixture_assistant_completed(
        &mut engine,
        None,
        json!({
            "model": "gpt-test", "duration_ms": 1000,
            "usage": { "output_tokens": 35 }
        }),
    );
    let out = render_snapshot(&mut engine);
    assert!(
        out.contains("▣ gpt-test · 1s · 35 tok/s"),
        "turn-local speed missing: {out}"
    );
}

#[test]
fn statusline_omits_loaded_providers_list() {
    // Multiple providers' auth statuses must NOT clutter the statusline
    // — the model picker (and `/model` slash command) shows providers
    // alongside their models, so duplicating that information beside
    // the active model just adds visual noise. The active model itself
    // from the canonical turn terminal stays visible.
    let mut engine = Engine::new(120, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    // Seed three auth statuses so the redundant `auth_segment` would have
    // emitted `ollama:✓ anthropic:? openai:!` into the statusline.
    for (provider, status) in [
        ("ollama", "connected"),
        ("anthropic", "login_required"),
        ("openai", "error"),
    ] {
        dispatch_event(
            &mut engine,
            json!({ "kind": "chat.auth.status", "provider": provider, "status": status }),
        );
    }
    // Canonical completion so the statusline carries the active model name —
    // anchors the assertion to a frame where the statusline is fully
    // populated rather than the pre-stats placeholder branch.
    fixture_assistant_completed(
        &mut engine,
        Some("answer".into()),
        json!({ "model": "claude-test" }),
    );
    let _ = render_snapshot(&mut engine);
    let snap = render_snapshot(&mut engine);

    // Statusline lives on the row(s) BELOW the input box. Take the rows
    // after the last `╯` so we can assert against the statusline without
    // false matches from popups or transcript content above.
    let rows: Vec<&str> = snap.lines().collect();
    let last_corner = rows
        .iter()
        .rposition(|r| r.contains('╯'))
        .expect("input bottom-right corner");
    let tail: String = rows[last_corner + 1..].join("\n");

    // Active model still rendered (chat.lua strips the `claude-` prefix
    // for display, so the surviving substring is `test`).
    assert!(
        tail.contains("test"),
        "active model missing from statusline tail: {tail:?}"
    );
    // Provider names from the loaded-providers list must NOT appear.
    for needle in ["ollama", "anthropic", "openai"] {
        assert!(
            !tail.contains(needle),
            "loaded-provider name {needle:?} should not appear in statusline tail: {tail:?}"
        );
    }
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
fn slash_model_no_args_fans_out_per_known_provider_and_opens_popup() {
    // `/model` with no args fans out one `chat.model.list_requested`
    // per *known* provider (connected or not). Disconnected providers
    // respond with an empty list but their section still renders so the
    // user sees what's available behind a login.
    let mut engine = Engine::new(120, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Two connected providers + one login_required.
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
        3,
        "should emit one list_requested per known provider (including login_required): {emits:?}"
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
    assert_eq!(providers, vec!["anthropic", "ollama", "openai"]);

    // Popup is now visible with per-provider sections.
    let out = render_str(&mut engine);
    assert!(
        out.contains("pick a model"),
        "ModelPicker popup title not visible: {out:?}"
    );
    assert!(
        out.contains("anthropic") && out.contains("ollama") && out.contains("openai"),
        "ModelPicker should show all known providers: {out:?}"
    );
}

#[test]
fn model_picker_shows_pending_hint_while_login_is_in_progress() {
    let mut engine = Engine::new(120, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.auth.status",
            "provider": "chatgpt",
            "status": "login_in_progress",
            "supports_login": true,
        }),
    );
    for ch in "/model".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");

    let out = render_str(&mut engine);
    assert!(
        out.contains("[logging in]") && out.contains("(completing login…)"),
        "pending login should have an intentional non-action hint: {out:?}"
    );
    assert!(
        !out.contains("(log in to load models)"),
        "pending login must not invite a duplicate login: {out:?}"
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
// The picker reads from `$NEFOR_DATA_DIR/sessions/` (overridable via
// env var, set per-test for isolation). Selecting a row emits a
// `sessions.resume_request { session_id }` envelope onto the NCP bus —
// no process exit, no sidechannel file. The starter's `sessions` Lua
// module subscribes to that kind and runs the in-process swap.
//
// Test isolation: each test creates a tempdir, sets NEFOR_DATA_DIR to
// it, and tears it down on completion. Env var manipulation is
// process-global so we serialize via a mutex.

use std::io::Write;
use std::sync::Mutex;

// Process-global lock — env var mutation is unsafe across threads.
// `cargo test` runs unit tests in parallel by default; this serializes
// only the tests that touch NEFOR_DATA_DIR.
static RESUME_ENV_LOCK: Mutex<()> = Mutex::new(());

struct ResumeEnv {
    _guard: std::sync::MutexGuard<'static, ()>,
    _tempdir: tempfile::TempDir,
    data_home: PathBuf,
    prev_data: Option<String>,
    prev_sessions: Option<String>,
}

impl ResumeEnv {
    fn new() -> Self {
        let guard = RESUME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tempdir = tempfile::tempdir().expect("tempdir");
        let data_home = tempdir.path().to_path_buf();
        std::fs::create_dir_all(data_home.join("sessions")).expect("mkdir sessions");
        let prev_data = std::env::var("NEFOR_DATA_DIR").ok();
        let prev_sessions = std::env::var("NEFOR_SESSIONS_DIR").ok();
        // Tests serialize via RESUME_ENV_LOCK so concurrent reads/writes
        // don't race. set_var is safe under edition 2021.
        std::env::set_var("NEFOR_DATA_DIR", &data_home);
        std::env::set_var("NEFOR_SESSIONS_DIR", data_home.join("sessions"));
        ResumeEnv {
            _guard: guard,
            _tempdir: tempdir,
            data_home,
            prev_data,
            prev_sessions,
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
        match self.prev_data.as_deref() {
            Some(v) => std::env::set_var("NEFOR_DATA_DIR", v),
            None => std::env::remove_var("NEFOR_DATA_DIR"),
        }
        match self.prev_sessions.as_deref() {
            Some(v) => std::env::set_var("NEFOR_SESSIONS_DIR", v),
            None => std::env::remove_var("NEFOR_SESSIONS_DIR"),
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
fn session_picker_skips_empty_recent_sessions_until_it_finds_resumable_ones() {
    let env = ResumeEnv::new();
    env.write_session(
        "11111111-1111-1111-1111-111111111111",
        "2026-05-01T10:00:00.000Z",
        Some("older real prompt"),
    );
    std::thread::sleep(std::time::Duration::from_millis(2));
    for i in 0..10 {
        env.write_session(
            &format!("empty0000-0000-0000-0000-00000000000{i}"),
            "2026-05-02T10:00:00.000Z",
            None,
        );
    }

    let mut engine = Engine::new(120, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    for ch in "/resume".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    let out = render_str(&mut engine);

    assert!(
        out.contains("older real prompt"),
        "picker should scan past empty newest sessions: {out:?}"
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
    fixture_assistant_delta(&mut engine, "selectable-token");
    fixture_assistant_completed(
        &mut engine,
        None,
        json!({ "model": "test", "duration_ms": 1 }),
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
    fixture_assistant_delta(&mut engine, "selectable-token");
    fixture_assistant_completed(
        &mut engine,
        None,
        json!({ "model": "test", "duration_ms": 1 }),
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
// the canonical user-message round trip. The earlier
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
/// conversation manager projects the submitted text back as a canonical message
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
    fixture_message(&mut engine, "user", "hello");

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

    fixture_message(&mut engine, "user", "abc");
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

/// Replay path: between session_start and resume_done, canonical message deltas
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
    fixture_message(&mut engine, "user", "first prompt");
    fixture_message(&mut engine, "assistant", "first reply");
    fixture_message(&mut engine, "user", "second prompt");

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

#[test]
fn height_cache_tolerates_unversioned_replay_entries() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");

    let measured: i64 = engine
        .lua()
        .load(
            r#"
            local height_cache = require("libs.chat.height_cache")
            height_cache.set_width(40)
            return height_cache.get({
              role = "assistant",
              kind = "text",
              text = "replay entry without cache version",
            }, function(e)
              return tui.text { content = e.text, wrap = "word" }
            end)
            "#,
        )
        .eval()
        .expect("unversioned replay entry must render without cache crash");

    assert!(measured > 0, "expected measured height, got {measured}");
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

    fixture_message(&mut engine, "user", "user-typed-quickly");
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
    fixture_message(&mut engine, "user", "old-content");
    let _ = render_str(&mut engine);

    // Open the picker and press Enter on the (only) row.
    for ch in "/resume".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter open");
    let _ = render_str(&mut engine);
    let _ = engine.take_emit_queue();
    engine.handle_key(key("enter")).expect("enter pick");
    dispatch_event(
        &mut engine,
        json!({ "kind": "sessions.session_start", "session_id": target,
            "from_resume": true, "request_id": "chat-transition-1" }),
    );
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
    fixture_message(&mut engine, "user", "abc");

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
    fixture_message(&mut engine, "user", "old-prompt");
    fixture_message(&mut engine, "assistant", "old-reply");
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
        json!({ "kind": "sessions.session_start", "session_id": "new",
            "request_id": "chat-transition-1" }),
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
    fixture_message(&mut engine, "user", "fresh-prompt");
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
/// the canonical user-message projection got deduped against
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
    fixture_message(&mut engine, "user", "first-prompt");

    // Assistant streams a reply.
    fixture_assistant_delta(&mut engine, "response-token");
    fixture_assistant_completed(
        &mut engine,
        None,
        json!({ "model": "test", "duration_ms": 1 }),
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
/// stranded marker and silently swallows it. Then a canonical tool projection
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
    fixture_message(&mut engine, "user", "summarize-thing");
    // Then the tool call paints (the visible artefact of the bug).
    fixture_tool_started(&mut engine, "t1", "mag", json!({}));
    let _ = render_str(&mut engine);

    let out = engine.snapshot();
    assert!(
        out.contains("summarize-thing"),
        "user prompt must remain visible even when entries was wiped \
         between local push and echo (production bug repro). \
         Transcript:\n{out}",
    );
    assert!(out.contains("mag"), "tool call must still render:\n{out}");
}

/// Direct production repro: at boot the first message renders fine, but
/// after `/new` the very first submit's user message disappears while
/// subsequent submits show. This drives the exact sequence the user sees:
/// 1. Boot session, submit message #1, echo deduped, both visible.
/// 2. `/new` → lifecycle cycle.
/// 3. Submit message #2 in the new session.
/// 4. Tool call arrives (no preceding text) — the orchestrator decided
///    to dispatch a kernel run immediately.
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
    fixture_message(&mut engine, "user", "old-prompt");
    fixture_assistant_delta(&mut engine, "old-reply");
    fixture_assistant_completed(
        &mut engine,
        None,
        json!({ "model": "test", "duration_ms": 1 }),
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
        json!({ "kind": "sessions.session_start", "session_id": "new",
            "request_id": "chat-transition-1" }),
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
    // text/reasoning — the orchestrator went straight to the kernel
    // dispatch).
    fixture_message(&mut engine, "user", "summarize-things");
    fixture_tool_started(&mut engine, "t1", "mag", json!({}));

    // Step 5: kernel-run events + final answer.
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_started", "run_id": "r1" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_complete", "run_id": "r1" }),
    );
    fixture_tool_completed(&mut engine, "t1", json!("ok"), false);
    fixture_assistant_delta(&mut engine, "final-answer");
    fixture_assistant_completed(
        &mut engine,
        None,
        json!({ "model": "test", "duration_ms": 1 }),
    );
    let _ = render_str(&mut engine);

    let out = engine.snapshot();
    assert!(
        out.contains("summarize-things"),
        "user's first prompt in the post-/new session must be visible \
         above the tool call. Production bug repro. Transcript:\n{out}",
    );
    assert!(
        out.contains("mag") || out.contains("final-answer"),
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
        fixture_message(&mut engine, "user", format!("MARKER-LEAK-LINE-{i}"));
    }
    let _ = render_str(&mut engine);

    // Open a tool-permission popup (short content; lots of dead space
    // inside the box).
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.tool.popup_request",
            "id": "t1",
            "tool": "shell.script",
            "args": { "command": "ls -la" },
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

    // Resolve popup columns from its top border; the title deliberately
    // overlays the first interior row and therefore has no side bars.
    let top_chars: Vec<char> = lines[popup_top].chars().collect();
    let left_border = top_chars
        .iter()
        .position(|&c| c == '╭')
        .expect("popup left border on top row");
    let right_border = top_chars
        .iter()
        .rposition(|&c| c == '╮')
        .expect("popup right border on top row");

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
        let chars: Vec<char> = row.chars().collect();
        if right_border > left_border + 1 && chars.len() > right_border {
            let interior: String = chars[left_border + 1..right_border].iter().collect();
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
    // ResumeEnv isolates input-history so parallel tests that submit
    // text don't populate prompt_history and route arrow-up to
    // history-recall instead of scroll.
    let _env = ResumeEnv::new();
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Pump enough content that the transcript has somewhere to scroll
    // away from. Auto-scroll keeps it pinned to the bottom while
    // entries arrive.
    for _ in 0..40 {
        fixture_message(&mut engine, "user", "x");
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
    // ResumeEnv isolates input-history so parallel tests that submit
    // text don't populate prompt_history and route arrow-up to
    // history-recall instead of scroll.
    let _env = ResumeEnv::new();
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Pre-fill enough content that there's somewhere to scroll up.
    for _ in 0..40 {
        fixture_message(&mut engine, "user", "x");
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
        fixture_assistant_delta(&mut engine, "lorem ipsum dolor sit amet ");
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

// ── persistent input history (issue #39) ────────────────────────────
//
// Like shell history: a submit on session A writes the prompt to
// `<NEFOR_DATA_DIR>/input-history`; a fresh nefor process (session B)
// reads it back at init so arrow-up recalls it. Cap is INPUT_HISTORY_MAX
// (50) — pushing the 51st entry rolls the oldest off the disk file
// the next time we trim.
//
// Reuses the `ResumeEnv` harness above for tempdir + NEFOR_DATA_DIR
// isolation. Each test scopes its own env so the file lives in a
// per-test tmp dir and tests don't race over the shared XDG path.

fn read_history_file(path: &std::path::Path) -> Vec<String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    raw.lines().map(|l| l.to_string()).collect()
}

#[test]
fn submit_persists_input_history_to_disk_for_next_session() {
    let env = ResumeEnv::new();

    // Session A: submit two prompts. Each Enter must mirror the text
    // to `<data_home>/input-history`, newest at line 1.
    {
        let mut engine = Engine::new(80, 24).expect("engine A");
        engine.load_scenario(&chat_lua_source()).expect("load");
        let _ = render_str(&mut engine);

        for ch in "hello".chars() {
            engine.handle_key(key(&ch.to_string())).expect("type");
        }
        engine.handle_key(key("enter")).expect("submit hello");
        let _ = render_str(&mut engine);

        for ch in "world".chars() {
            engine.handle_key(key(&ch.to_string())).expect("type");
        }
        engine.handle_key(key("enter")).expect("submit world");
        let _ = render_str(&mut engine);
    }

    let path = env.data_home.join("input-history");
    let lines = read_history_file(&path);
    assert_eq!(
        lines,
        vec!["world".to_string(), "hello".to_string()],
        "input-history must hold both submits, newest first"
    );

    // Session B: a fresh engine on the same NEFOR_DATA_DIR hydrates
    // its `prompt_history` from disk. Arrow-up on the empty input
    // recalls the most-recent ("world") on the first press.
    let mut engine_b = Engine::new(80, 24).expect("engine B");
    engine_b.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine_b);

    engine_b.handle_key(key("up")).expect("arrow up");
    let _ = render_str(&mut engine_b);

    // Probe state.input_value via Lua. The state table is held inside
    // the engine's frame closure; expose it indirectly by triggering
    // a render and inspecting the rendered input field. But since the
    // text_input desc carries the value verbatim, easier: pull it via
    // the engine's snapshot — the input row contains the recalled
    // text.
    let snap = engine_b.snapshot();
    assert!(
        snap.contains("world"),
        "expected last-session's prompt 'world' to recall via arrow-up, got snapshot:\n{snap}"
    );

    // Second arrow-up walks to the older entry.
    engine_b.handle_key(key("up")).expect("arrow up 2");
    let _ = render_str(&mut engine_b);
    let snap = engine_b.snapshot();
    assert!(
        snap.contains("hello"),
        "expected older prompt 'hello' on second arrow-up, got snapshot:\n{snap}"
    );
}

#[test]
fn submit_caps_input_history_at_max_and_rolls_oldest() {
    // Pump 51 distinct prompts. The disk file must hold exactly 50
    // entries (INPUT_HISTORY_MAX), with the OLDEST submission rolled
    // off. Newest-first ordering means line 1 is the 51st prompt
    // ("p51") and line 50 is the second-oldest ("p2"); "p1" is gone.
    let env = ResumeEnv::new();

    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    for i in 1..=51 {
        let prompt = format!("p{i}");
        for ch in prompt.chars() {
            engine.handle_key(key(&ch.to_string())).expect("type");
        }
        engine.handle_key(key("enter")).expect("submit");
        let _ = render_str(&mut engine);
    }

    let path = env.data_home.join("input-history");
    let lines = read_history_file(&path);
    assert_eq!(
        lines.len(),
        50,
        "input-history must trim to INPUT_HISTORY_MAX (50); got {} lines",
        lines.len()
    );
    assert_eq!(
        lines[0], "p51",
        "newest submit must be at the top of the file"
    );
    assert_eq!(
        lines[49], "p2",
        "the 50th line must be the second-oldest — p1 rolled off"
    );
    assert!(
        !lines.iter().any(|l| l == "p1"),
        "oldest submit (p1) must have rolled off past the cap"
    );
}

#[test]
fn input_history_round_trips_multiline_payload_through_disk() {
    // A multi-line paste landed via Shift+Enter / bracketed-paste +
    // Enter must round-trip through the on-disk file: the file format
    // escapes \n / \r so each entry stays on a single physical line,
    // and the loader decodes back to the original verbatim. Without
    // that escaping the second line of a multi-line paste would be
    // read back as a separate history entry.
    let env = ResumeEnv::new();

    // Session A: paste a 3-line block, submit it. Use the engine's
    // bracketed-paste path so the test exercises the same code path
    // a real paste would.
    {
        let mut engine = Engine::new(80, 24).expect("engine A");
        engine.load_scenario(&chat_lua_source()).expect("load");
        let _ = render_str(&mut engine);

        let payload = "line1\nline2\nline3";
        engine.handle_paste(payload).expect("paste");
        engine.handle_key(key("enter")).expect("submit");
        let _ = render_str(&mut engine);
    }

    let path = env.data_home.join("input-history");
    let lines = read_history_file(&path);
    assert_eq!(
        lines.len(),
        1,
        "multi-line submit must occupy exactly one physical line in the file (got {lines:?})"
    );
    assert!(
        lines[0].contains(r"\n"),
        "multi-line submit must have its real \\n escaped to the literal two-char `\\n` sequence \
         on disk so the per-line frame survives — got: {:?}",
        lines[0]
    );

    // Session B reloads. The first arrow-up recall puts the FULL
    // multi-line text into the input — verify by a substring of each
    // line in the snapshot.
    let mut engine_b = Engine::new(80, 24).expect("engine B");
    engine_b.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine_b);
    engine_b.handle_key(key("up")).expect("arrow up");
    let _ = render_str(&mut engine_b);
    let snap = engine_b.snapshot();
    assert!(
        snap.contains("line1") && snap.contains("line2") && snap.contains("line3"),
        "all three lines of the multi-line history entry must be visible \
         in the recalled input — got snapshot:\n{snap}"
    );
}

// ──────────────────────────────────────────────────────────────────────
// @path preprocessor (#47)
// ──────────────────────────────────────────────────────────────────────
// chat.lua's submit reducer scans plain-text submits for `@<path>`
// tokens and inlines the file contents as a `<file path="…">` block
// before emitting `chat.input.submit`. The lead workflow spec
// (lead-workflow-spec §1, §6, §8) treats this as a starter-config
// prerequisite: the orchestrator's first turn sees `@`-files already
// resolved, with the existing `read_file` tool as the fallback for
// truncated or larger files.

fn type_text(engine: &mut Engine, s: &str) {
    for ch in s.chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
}

fn submit_text(engine: &mut Engine, s: &str) {
    type_text(engine, s);
    engine.handle_key(key("enter")).expect("enter");
}

#[test]
fn at_path_inlines_existing_file_into_wire_envelope() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("hello.lua");
    std::fs::write(&path, "print('hi from fixture')\n").expect("write fixture");

    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Absolute paths sidestep CWD assumptions — the io.open fallback
    // for absolute tokens is the codepath this test pins.
    let prompt = format!("summarize @{}", path.display());
    submit_text(&mut engine, &prompt);

    let emits = engine.take_emit_queue();
    assert_eq!(emits.len(), 1, "submit should produce exactly one emit");
    let (target_hint, body) = &emits[0];
    assert_eq!(target_hint.as_deref(), Some("engine"));
    assert_eq!(
        body.get("kind").and_then(|v| v.as_str()),
        Some("chat.input.submit")
    );
    let wire = body
        .get("text")
        .and_then(|v| v.as_str())
        .expect("text on the envelope");

    assert!(
        wire.contains("print('hi from fixture')"),
        "file contents missing from wire text: {wire:?}"
    );
    assert!(
        wire.contains(&format!("<file path=\"{}\">", path.display())),
        "expected `<file path=\"…\">` wrapper in wire text: {wire:?}"
    );
    assert!(
        wire.contains("```lua"),
        "expected lua fence inferred from .lua extension: {wire:?}"
    );
    assert!(
        wire.contains("</file>"),
        "expected closing `</file>` in wire text: {wire:?}"
    );
    // The raw `@<path>` token must NOT survive into the wire envelope —
    // the inlined block replaced it. (`@` may still appear inside the
    // file path attribute, but the standalone `@<path>` token gone.)
    let needle = format!("@{}", path.display());
    assert!(
        !wire.contains(&needle),
        "@-token should be replaced, not present verbatim: {wire:?}"
    );
}

#[test]
fn at_path_missing_file_leaves_token_untouched_no_error() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Absolute path that does not exist — chat.lua MUST leave the
    // token verbatim and emit a normal `chat.input.submit` envelope.
    // The orchestrator + model can ask the user about it; chat.lua's
    // job is just to not error.
    let prompt = "look at @/this/does/not/exist/anywhere.lua please";
    submit_text(&mut engine, prompt);

    let emits = engine.take_emit_queue();
    assert_eq!(emits.len(), 1, "submit should produce exactly one emit");
    let body = &emits[0].1;
    assert_eq!(
        body.get("kind").and_then(|v| v.as_str()),
        Some("chat.input.submit")
    );
    let wire = body
        .get("text")
        .and_then(|v| v.as_str())
        .expect("text on the envelope");
    assert!(
        wire.contains("@/this/does/not/exist/anywhere.lua"),
        "missing-file token should pass through unchanged: {wire:?}"
    );
    assert!(
        !wire.contains("<file path"),
        "no <file> wrapper should appear when the file is missing: {wire:?}"
    );
}

#[test]
fn at_path_truncates_files_over_inline_budget() {
    // Budget in chat.lua is 16 KiB — write 32 KiB so the truncation
    // marker fires regardless of any +1 boundary off-by-one.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("big.txt");
    let payload = "x".repeat(32 * 1024);
    std::fs::write(&path, &payload).expect("write big fixture");

    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    let prompt = format!("summarize @{}", path.display());
    submit_text(&mut engine, &prompt);

    let emits = engine.take_emit_queue();
    assert_eq!(emits.len(), 1, "submit should produce exactly one emit");
    let body = &emits[0].1;
    let wire = body
        .get("text")
        .and_then(|v| v.as_str())
        .expect("text on the envelope");
    assert!(
        wire.contains("[truncated; use read_file tool for full contents]"),
        "expected truncation marker in wire text: {wire:?}"
    );
    // Total size of the inlined content + wrapper should be far below
    // the original 32 KiB — pin a soft upper bound so a regression
    // that emits the full file slips the assertion. 24 KiB is well
    // above the 16 KiB budget + wrapper text but well below 32 KiB.
    assert!(
        wire.len() < 24 * 1024,
        "wire text should be truncated near 16 KiB budget; got {} bytes",
        wire.len()
    );
}

#[test]
fn at_path_expands_multiple_references_in_one_message() {
    let dir = tempfile::tempdir().expect("tempdir");
    let a = dir.path().join("alpha.lua");
    let b = dir.path().join("beta.md");
    std::fs::write(&a, "ALPHA_CONTENTS\n").expect("write alpha");
    std::fs::write(&b, "BETA_CONTENTS\n").expect("write beta");

    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    let prompt = format!("compare @{} and @{}", a.display(), b.display());
    submit_text(&mut engine, &prompt);

    let emits = engine.take_emit_queue();
    assert_eq!(emits.len(), 1, "submit should produce exactly one emit");
    let body = &emits[0].1;
    let wire = body
        .get("text")
        .and_then(|v| v.as_str())
        .expect("text on the envelope");
    assert!(
        wire.contains("ALPHA_CONTENTS"),
        "alpha file contents missing: {wire:?}"
    );
    assert!(
        wire.contains("BETA_CONTENTS"),
        "beta file contents missing: {wire:?}"
    );
    // Two `<file path="…">` wrappers — one per resolved reference.
    let opens = wire.matches("<file path=\"").count();
    assert_eq!(
        opens, 2,
        "expected exactly two <file> blocks for two refs: {wire:?}"
    );
}

#[test]
fn at_path_strips_trailing_punctuation_when_resolving() {
    // User types `@<path>.` at end of sentence — the trailing period
    // is prompt punctuation, not part of the filename. chat.lua peels
    // common trailing punctuation off the captured token before
    // resolution and re-attaches it verbatim after the inlined block.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("ref.md");
    std::fs::write(&path, "PUNCT_FIXTURE\n").expect("write ref");

    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Period after the path; the model still gets the file inlined.
    let prompt = format!("see @{}.", path.display());
    submit_text(&mut engine, &prompt);

    let emits = engine.take_emit_queue();
    assert_eq!(emits.len(), 1);
    let wire = emits[0]
        .1
        .get("text")
        .and_then(|v| v.as_str())
        .expect("text");
    assert!(
        wire.contains("PUNCT_FIXTURE"),
        "expected file contents inlined despite trailing period: {wire:?}"
    );
    assert!(
        wire.contains(&format!("<file path=\"{}\">", path.display())),
        "wrapper should carry the trimmed (no-trailing-period) path: {wire:?}"
    );
}

// ───────────────────────────────────────────────────────────────────
// plan-message contract — yellow-bordered render-only entry kind
// emitted by the lead-workflow actor's `write-review` tool. The plan
// body is shown to the user and reviewed via /approve | /reject; it
// does NOT enter model context (the model already saw it via the
// tool call's args).
// ───────────────────────────────────────────────────────────────────

/// SGR fragment for the plan border colour `#FFD75F`. The 24-bit
/// colour gets written as `38;2;255;215;95` per ansi.rs's `write_fg`.
/// Asserting the literal SGR substring is the cheapest way to confirm
/// the renderer painted the yellow chrome — the plan border is the
/// only thing in chat.lua that uses this RGB triple, so a hit on this
/// substring uniquely identifies the plan box.
const PLAN_YELLOW_SGR: &str = "38;2;255;215;95";

#[test]
fn chat_plan_append_renders_yellow_bordered_plan_entry() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.plan.append",
            "plan_id": "p1",
            "text": "Step one\nStep two",
            "submitted_at": "2026-05-08T14:30:00Z",
        }),
    );

    let out = render_str(&mut engine);

    // Body lines from the markdown plan land in the transcript.
    assert!(
        out.contains("Step one"),
        "plan body line 1 missing: {out:?}"
    );
    assert!(
        out.contains("Step two"),
        "plan body line 2 missing: {out:?}"
    );

    // The bordered_box helper paints all four rounded corners, same as
    // the user block — confirms render_plan_entry routed through
    // bordered_box rather than dropping to the plain-text fallback.
    for corner in ['╭', '╮', '╰', '╯'] {
        assert!(
            out.contains(corner),
            "plan block missing corner {corner:?}: {out:?}"
        );
    }

    // Yellow border colour (#FFD75F → SGR 38;2;255;215;95). This is the
    // load-bearing assertion: a non-yellow border (e.g. blue user_chrome)
    // would emit `38;2;127;180;255` instead, and this substring would
    // NOT appear.
    assert!(
        out.contains(PLAN_YELLOW_SGR),
        "yellow plan border SGR `{PLAN_YELLOW_SGR}` missing in: {out:?}",
    );

    // Subtitle carries the timestamp the actor stamped.
    assert!(
        out.contains("submitted at 14:30"),
        "plan subtitle missing timestamp: {out:?}"
    );

    // Pending plans show the action hint so the user knows the
    // /approve | /reject convention without leaving the chat surface.
    assert!(
        out.contains("/approve"),
        "pending plan should show /approve hint: {out:?}"
    );
    assert!(
        out.contains("/reject"),
        "pending plan should show /reject hint: {out:?}"
    );
}

#[test]
fn lead_workflow_plan_approved_updates_status() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.plan.append",
            "plan_id": "abc-123",
            "text": "Refactor the bus reducer",
            "submitted_at": "2026-05-08T09:15:00Z",
        }),
    );
    let pre = render_str(&mut engine);
    assert!(
        pre.contains("/approve"),
        "pre-approval should show pending hint: {pre:?}"
    );

    dispatch_event(
        &mut engine,
        json!({
            "kind": "lead-workflow.plan.approved",
            "plan_id": "abc-123",
            "approved": true,
        }),
    );
    let out = render_str(&mut engine);

    // After approval the hint disappears and a check-mark status row
    // takes its place.
    assert!(
        !out.contains("/approve"),
        "approved plan should NOT carry the pending hint: {out:?}"
    );
    assert!(
        out.contains("✓ approved"),
        "approved plan should show the check-mark status: {out:?}"
    );
    // Plan body still in the transcript — approval doesn't hide it.
    assert!(
        out.contains("Refactor the bus reducer"),
        "approved plan body missing: {out:?}"
    );
}

#[test]
fn plan_approval_before_append_marks_matching_plan() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({
            "kind": "lead-workflow.plan.approved",
            "plan_id": "early-approval",
            "approved": true,
        }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.plan.append",
            "plan_id": "early-approval",
            "text": "Plan appended after approval",
            "submitted_at": "2026-05-08T09:16:00Z",
        }),
    );
    let out = render_str(&mut engine);

    assert!(
        out.contains("✓ approved"),
        "plan appended after approval should render as approved: {out:?}"
    );
    assert!(
        !out.contains("/approve"),
        "approved plan must not trap later input in review mode: {out:?}"
    );

    let _ = engine.take_emit_queue();
    submit_text(&mut engine, "so what's the status?");
    let emits = engine.take_emit_queue();
    assert_eq!(emits.len(), 1, "status prompt should emit one envelope");
    let (target, body) = &emits[0];
    assert_eq!(target.as_deref(), Some("engine"));
    assert_eq!(
        body.get("kind").and_then(|v| v.as_str()),
        Some("chat.input.submit")
    );
    assert_eq!(
        body.get("text").and_then(|v| v.as_str()),
        Some("so what's the status?")
    );
}

#[test]
fn lead_workflow_plan_rejected_marks_entry() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.plan.append",
            "plan_id": "rej-1",
            "text": "Drop the index",
            "submitted_at": "2026-05-08T10:00:00Z",
        }),
    );
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({
            "kind": "lead-workflow.plan.approved",
            "plan_id": "rej-1",
            "approved": false,
        }),
    );
    let out = render_str(&mut engine);

    assert!(
        out.contains("✗ rejected"),
        "rejected plan should show the cross status: {out:?}"
    );
    assert!(
        !out.contains("/approve"),
        "rejected plan should NOT carry the pending hint: {out:?}"
    );
}

#[test]
fn approval_targets_the_latest_pending_plan() {
    // New model: only ONE plan is in flight at a time, and there is no
    // plan_id on the wire. The chat surface tracks plan entries in the
    // transcript and applies an approval envelope to the most recent
    // pending entry. Older decided entries keep their status.
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.plan.append",
            "text": "FIRST_PLAN_BODY",
            "submitted_at": "2026-05-08T08:00:00Z",
        }),
    );
    // First plan decided before the second appears (realistic flow:
    // actor only allows one in-flight at a time).
    dispatch_event(
        &mut engine,
        json!({
            "kind": "lead-workflow.plan.approved",
            "approved": true,
        }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.plan.append",
            "text": "SECOND_PLAN_BODY",
            "submitted_at": "2026-05-08T08:05:00Z",
        }),
    );
    let _ = render_str(&mut engine);

    // Now approve the second.
    dispatch_event(
        &mut engine,
        json!({
            "kind": "lead-workflow.plan.approved",
            "approved": true,
        }),
    );
    let out = render_str(&mut engine);

    // Both plans should carry approved status.
    let approved_count = out.matches("✓ approved").count();
    assert!(
        approved_count >= 1,
        "expected at least one approved row after the second approval; out: {out:?}"
    );
    // No pending hints should remain — every plan has been decided.
    assert!(
        !out.contains("/approve"),
        "no pending hint should remain after both plans approved: {out:?}"
    );
}

#[test]
fn approval_with_no_pending_plan_is_dropped() {
    // Defence: an approval envelope with no pending plan entry in the
    // transcript is a no-op — chat.lua's reducer leaves state
    // untouched rather than creating a synthetic entry. Pre-fix a
    // sloppy reducer that pushed an entry on approval would surface a
    // phantom row here.
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({
            "kind": "lead-workflow.plan.approved",
            "approved": true,
        }),
    );
    let out = render_str(&mut engine);

    // No plan body → no approval check should appear.
    assert!(
        !out.contains("✓ approved"),
        "approval with no pending plan should NOT paint a status row: {out:?}"
    );
}

#[test]
fn chat_plan_append_does_not_emit_chat_message_append_or_input_submit() {
    // Render-only contract: the plan envelope drops into the chat
    // surface as a yellow-bordered entry but MUST NOT cause chat.lua
    // to re-emit a `chat.message.append` (which would feed the
    // assistant/orchestrator history) or a `chat.input.submit` (which
    // would re-trigger model inference). The model already saw the
    // plan via the write-review tool's args; chat.lua's job is purely
    // visual review-and-approve.
    //
    // This is the "plan content is NOT included in model context"
    // assertion: model context is built from `chat.message.append`
    // envelopes the orchestrator/agentic-loop replays into providers.
    // If chat.lua doesn't echo the plan back as a message, the
    // provider's history never sees it — preventing the duplication
    // the user flagged on the spec.
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);
    let _ = engine.take_emit_queue();

    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.plan.append",
            "plan_id": "p1",
            "text": "PLAN_TEXT_THAT_MUST_NOT_LEAK",
            "submitted_at": "2026-05-08T12:00:00Z",
        }),
    );

    let emits = engine.take_emit_queue();
    for (_target, body) in &emits {
        let kind = body
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        assert_ne!(
            kind, "chat.message.append",
            "chat.plan.append must NOT echo as chat.message.append (would land in model context): {body:?}"
        );
        assert_ne!(
            kind, "chat.input.submit",
            "chat.plan.append must NOT trigger a chat.input.submit: {body:?}"
        );
        // The plan body is the load-bearing thing to NOT leak. Even if
        // some other envelope did get emitted, the plan text itself
        // must not be a payload field of it.
        let serialized = serde_json::to_string(body).unwrap_or_default();
        assert!(
            !serialized.contains("PLAN_TEXT_THAT_MUST_NOT_LEAK"),
            "plan body leaked into emitted envelope {kind:?}: {body:?}"
        );
    }

    // And the plan body still rendered visibly — proving the entry
    // landed locally without going through any wire echo.
    let out = render_str(&mut engine);
    assert!(
        out.contains("PLAN_TEXT_THAT_MUST_NOT_LEAK"),
        "plan body missing from local render: {out:?}"
    );
}

#[test]
fn user_submit_after_plan_does_not_carry_plan_body_in_wire_text() {
    // Tighter version of the previous test: even after the plan lands
    // and the user types their next prompt, the `chat.input.submit`
    // wire envelope must carry ONLY the user's typed text — the plan
    // body must never be appended (e.g. as context glue) into that
    // text field. Pre-fix a naive reducer that built `text` by
    // concatenating recent entries would leak the plan here.
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.plan.append",
            "plan_id": "p1",
            "text": "PLAN_BODY_PRIVATE_TO_RENDER",
            "submitted_at": "2026-05-08T12:00:00Z",
        }),
    );
    let _ = render_str(&mut engine);
    let _ = engine.take_emit_queue();

    submit_text(&mut engine, "/approve");

    let emits = engine.take_emit_queue();
    for (_target, body) in &emits {
        let serialized = serde_json::to_string(body).unwrap_or_default();
        assert!(
            !serialized.contains("PLAN_BODY_PRIVATE_TO_RENDER"),
            "plan body leaked into post-plan emit {body:?}"
        );
    }
}

#[test]
fn plan_review_reply_emits_review_response_not_chat_input_submit() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.plan.append",
            "plan_id": "p1",
            "text": "Plan awaiting review",
            "submitted_at": "2026-05-08T12:00:00Z",
        }),
    );
    let _ = render_str(&mut engine);
    let _ = engine.take_emit_queue();

    let reply = "please delete the disposable audio stems";
    submit_text(&mut engine, reply);

    let emits = engine.take_emit_queue();
    assert_eq!(emits.len(), 1, "review reply should emit one envelope");
    let (target, body) = &emits[0];
    assert_eq!(target.as_deref(), Some("engine"));
    assert_eq!(
        body.get("kind").and_then(|v| v.as_str()),
        Some("chat.review.respond")
    );
    assert_eq!(body.get("text").and_then(|v| v.as_str()), Some(reply));
    assert_ne!(
        body.get("kind").and_then(|v| v.as_str()),
        Some("chat.input.submit"),
        "review replies must not enter the normal prompt queue"
    );
}

#[test]
fn chat_plan_append_is_idempotent_on_submitted_at() {
    // Plan identity is the submission timestamp. Even if a duplicate
    // chat.plan.append arrives, the chat surface must render one block.
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    let envelope = json!({
        "kind": "chat.plan.append",
        "text": "DUP_PLAN_BODY",
        "submitted_at": "2026-05-08T12:00:00Z",
    });
    dispatch_event(&mut engine, envelope.clone());
    dispatch_event(&mut engine, envelope);
    let out = render_str(&mut engine);

    let body_count = out.matches("DUP_PLAN_BODY").count();
    assert_eq!(
        body_count, 1,
        "duplicate chat.plan.append for same submitted_at must render only one yellow box; out: {out:?}"
    );
}

#[test]
fn chat_plan_append_dedup_preserves_approved_status() {
    // After a plan is approved, a duplicate chat.plan.append with the
    // same submitted_at must NOT regress the entry's status back to
    // "pending". The dedup branch returns the existing entry untouched.
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.plan.append",
            "text": "PLAN_TO_APPROVE",
            "submitted_at": "2026-05-08T12:00:00Z",
        }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "lead-workflow.plan.approved",
            "approved": true,
        }),
    );
    // Duplicate append for the already-approved plan (same timestamp).
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.plan.append",
            "text": "PLAN_TO_APPROVE",
            "submitted_at": "2026-05-08T12:00:00Z",
        }),
    );

    let out = render_str(&mut engine);
    assert!(
        out.contains("✓ approved"),
        "approved status must survive a duplicate chat.plan.append: {out:?}"
    );
    let body_count = out.matches("PLAN_TO_APPROVE").count();
    assert_eq!(
        body_count, 1,
        "duplicate append must not paint a second entry: {out:?}"
    );
}

// ── Agent view: sidebar focus, fold toggle, stream capture, popup ─────
//
// Agent-view observability: Tab/Shift-Tab cycles key focus between the
// prompt and the sidebar; MAG groups carry per-group stored fold state,
// DEFAULT COLLAPSED — Enter on a group row toggles it (fold state
// survives re-renders and focus changes, resets with run prune / new
// session); Enter on an actor leaf opens a read-only popup fed by
// per-actor capture buffers that consume scoped provider observations
// broadcasts BEFORE the transcript's foreign-chat guard. Member rows are
// activity-honest (kernel busy/idle events): working members tick their
// current activation, idle members render quietly, and only a
// busy-and-silent member grows the ⚠ stale alarm.

/// Concatenated text of every screen segment painted with the cursor
/// row style (fg #000000 on bg #7FB4FF). Lets tests assert WHICH
/// sidebar row carries the cursor highlight.
fn cursor_styled_text(out: &str) -> String {
    let mut acc = String::new();
    for chunk in out.split('\u{1b}') {
        // Each chunk is "[<sgr>m<text>" after the split; keep text from
        // chunks whose SGR paints the cursor-row background.
        if let Some(m_idx) = chunk.find('m') {
            let (sgr, text) = chunk.split_at(m_idx);
            if sgr.contains("48;2;127;180;255") {
                acc.push_str(&text[1..]);
            }
        }
    }
    acc
}

/// Boot a chat surface with one scoped MAG run (`scope: r2`) carrying a
/// single WORKING actor `worker.llm` (spawned → ready → busy, the kernel's
/// activation-delivery order). Shared setup for the capture/view tests.
fn engine_with_scoped_worker() -> Engine {
    let mut engine = Engine::new(120, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_started", "run_id": "sub-1", "run_name": "auth-fix", "scope": "r2" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_spawned", "run_id": "sub-1", "id": "worker.llm", "factory": "llm" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_ready", "run_id": "sub-1", "id": "worker.llm" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_busy", "run_id": "sub-1", "id": "worker.llm" }),
    );
    engine
}

/// Focused-sidebar navigation to the `worker.llm` leaf in
/// `engine_with_scoped_worker`: Tab, unfold the (default-collapsed)
/// `worker` group, land the cursor on the member row. Returns the last
/// render so callers can assert on the freshly-painted leaf.
fn focus_worker_leaf(engine: &mut Engine) -> String {
    engine.handle_key(key("tab")).expect("tab");
    let _ = render_str(engine); // refresh the tree so keys route on focus
    engine.handle_key(key("down")).expect("down"); // → worker group row
    engine.handle_key(key("enter")).expect("enter"); // unfold
    engine.handle_key(key("down")).expect("down"); // → worker.llm leaf
    render_str(engine)
}

#[test]
fn groups_default_collapsed_and_enter_toggles_fold() {
    let mut engine = Engine::new(120, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_started", "run_id": "mag-focus-1", "run_name": "demo" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_spawned", "run_id": "mag-focus-1", "id": "explorer.entry", "factory": "llm" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_spawned", "run_id": "mag-focus-1", "id": "explorer.loop-counter", "factory": "llm" }),
    );

    // Unfocused sidebar: grouped + collapsed.
    let out = render_str(&mut engine);
    assert!(
        !out.contains("explorer.entry"),
        "members must render collapsed by default: {out:?}"
    );

    // Tab → sidebar focused. DEFAULT COLLAPSED: focus alone reveals
    // nothing; the cursor highlight starts on the run header row.
    engine.handle_key(key("tab")).expect("tab");
    let out = render_str(&mut engine);
    assert!(
        !out.contains("explorer.entry"),
        "focus must not imply expansion — groups stay folded: {out:?}"
    );
    let cursor = cursor_styled_text(&out);
    assert!(
        cursor.contains("MAG demo"),
        "cursor highlight should start on the run header row: {cursor:?}"
    );

    // Down → the group row; Enter unfolds it: member rows appear.
    engine.handle_key(key("down")).expect("down");
    engine.handle_key(key("enter")).expect("enter");
    let out = render_str(&mut engine);
    assert!(
        out.contains("explorer.entry") && out.contains("explorer.loop-counter"),
        "Enter on the group row must unfold its member rows: {out:?}"
    );

    // Fold state survives losing focus: Esc back to the prompt keeps the
    // unfolded members on screen (stored state, not focus-implied). The
    // full-frame snapshot sees unchanged rows the incremental diff skips.
    engine.handle_key(key("escape")).expect("escape");
    let _ = render_str(&mut engine);
    let snap = engine.snapshot();
    assert!(
        snap.contains("explorer.entry"),
        "fold state must survive focus changes:\n{snap}"
    );

    // Re-focus; Enter on the group row again folds it back.
    engine.handle_key(key("tab")).expect("tab");
    let _ = render_str(&mut engine);
    engine.handle_key(key("enter")).expect("enter");
    let _ = render_str(&mut engine);
    let snap = engine.snapshot();
    assert!(
        !snap.contains("explorer.entry"),
        "Enter on the unfolded group row must fold it again:\n{snap}"
    );
}

#[test]
fn shift_tab_also_cycles_focus() {
    let mut engine = Engine::new(120, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_started", "run_id": "mag-focus-2", "run_name": "demo" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_spawned", "run_id": "mag-focus-2", "id": "explorer.entry", "factory": "llm" }),
    );

    engine.handle_key(key("shift_tab")).expect("shift_tab");
    let out = render_str(&mut engine);
    let cursor = cursor_styled_text(&out);
    assert!(
        cursor.contains("MAG demo"),
        "shift_tab must focus the sidebar too (2 panes ⇒ toggle): {cursor:?}"
    );
    engine.handle_key(key("shift_tab")).expect("shift_tab");
    let out = render_str(&mut engine);
    let cursor = cursor_styled_text(&out);
    assert!(
        !cursor.contains("MAG demo"),
        "second shift_tab must hand focus back to the prompt: {cursor:?}"
    );
}

#[test]
fn tab_is_noop_while_a_popup_owns_the_keyboard() {
    let mut engine = Engine::new(120, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_started", "run_id": "mag-popup-1", "run_name": "demo" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_spawned", "run_id": "mag-popup-1", "id": "explorer.entry", "factory": "llm" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.popup", "level": "info", "title": "hi", "message": "body" }),
    );

    engine.handle_key(key("tab")).expect("tab");
    let out = render_str(&mut engine);
    let cursor = cursor_styled_text(&out);
    assert!(
        !cursor.contains("MAG demo"),
        "tab must not switch pane focus while a popup owns keys: {cursor:?}"
    );

    // Dismiss the popup; tab now works normally.
    engine.handle_key(key("escape")).expect("escape");
    engine.handle_key(key("tab")).expect("tab");
    let out = render_str(&mut engine);
    let cursor = cursor_styled_text(&out);
    assert!(
        cursor.contains("MAG demo"),
        "after popup dismissal tab must focus the sidebar: {cursor:?}"
    );
}

#[test]
fn tab_with_completion_open_completes_instead_of_switching_focus() {
    let mut engine = Engine::new(120, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_started", "run_id": "mag-comp-1", "run_name": "demo" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_spawned", "run_id": "mag-comp-1", "id": "explorer.entry", "factory": "llm" }),
    );

    for ch in ["/", "h", "e"] {
        engine.handle_key(key(ch)).expect("type");
    }
    engine.handle_key(key("tab")).expect("tab");
    let out = render_str(&mut engine);
    let cursor = cursor_styled_text(&out);
    assert!(
        !cursor.contains("MAG demo"),
        "tab with the completion popup open must not move pane focus: {cursor:?}"
    );
    assert!(
        out.contains("/help"),
        "tab should have applied the slash completion: {out:?}"
    );
}

#[test]
fn node_inspector_navigation_is_read_only_and_closes_back_to_sidebar_then_prompt() {
    let mut engine = engine_with_scoped_worker();

    // Tab, unfold the worker group, land on the leaf (the helper renders
    // between the focus flip and the keys: the Rust router reads the
    // prompt's `focused` flag off the last reconciled tree, and the real
    // event loop repaints after every event), then open the view.
    focus_worker_leaf(&mut engine);
    engine.handle_key(key("space")).expect("space");
    let out = render_str(&mut engine);
    assert!(out.contains("[read-only]"), "view should be open: {out:?}");
    engine.take_emit_queue();

    // Keys that would type into the prompt (or approve a tool) do
    // nothing: no envelope, popup stays, prompt receives no text.
    for ch in ["z", "a", "d", "enter"] {
        engine.handle_key(key(ch)).expect("keypress");
    }
    assert!(
        engine.take_emit_queue().is_empty(),
        "keystrokes inside the read-only view must not emit envelopes"
    );
    // Read-only keypresses can still coincide with an elapsed-time repaint of
    // the workflow underlay. The empty emit queue above proves they had no
    // input effect; Esc below proves the inspector remains on top.
    let _ = render_str(&mut engine);

    // Esc closes back to the sidebar with the cursor preserved on the
    // same leaf; Space re-opens the same actor's view.
    engine.handle_key(key("escape")).expect("escape");
    let out = render_str(&mut engine);
    assert!(
        !out.contains("[read-only]") && out.contains("worker.llm"),
        "Esc must close the view and land back on the focused sidebar: {out:?}"
    );
    let cursor = cursor_styled_text(&out);
    assert!(
        cursor.contains("worker.llm"),
        "cursor must be preserved on the actor row: {cursor:?}"
    );
    engine.handle_key(key("space")).expect("space");
    let out = render_str(&mut engine);
    assert!(
        out.contains("[read-only]"),
        "Space must re-open the view for the preserved cursor row: {out:?}"
    );

    // q also closes; a second Esc hands focus back to the prompt. The
    // unfolded members STAY on screen — fold state is stored, not
    // focus-implied — and typing lands in the input again.
    engine.handle_key(key("q")).expect("q");
    engine.handle_key(key("escape")).expect("escape");
    let _ = render_str(&mut engine);
    let snap = engine.snapshot();
    assert!(
        snap.contains("worker.llm"),
        "fold state must survive handing focus back to the prompt:\n{snap}"
    );
    let cursor = cursor_styled_text(&engine.snapshot_ansi());
    assert!(
        !cursor.contains("worker.llm"),
        "the cursor highlight must leave with sidebar focus: {cursor:?}"
    );
    for ch in ["z", "q", "z"] {
        engine.handle_key(key(ch)).expect("type");
    }
    let out = render_str(&mut engine);
    assert!(
        out.contains("zqz"),
        "prompt must receive keys again after leaving the sidebar: {out:?}"
    );
}

/// Boot a chat surface with one MAG run (`loop`) carrying a two-member
/// `lead` group cycling like an agent loop: `lead.llm` working,
/// `lead.run-tool` spawned-but-pending. Shared setup for the
/// activity-ticking tests.
fn engine_with_cycling_group() -> Engine {
    let mut engine = Engine::new(120, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_started", "run_id": "loop-1", "run_name": "loop", "scope": "r4" }),
    );
    for id in ["lead.llm", "lead.run-tool"] {
        dispatch_event(
            &mut engine,
            json!({ "kind": "mag.actor_spawned", "run_id": "loop-1", "id": id, "factory": "stub" }),
        );
    }
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_ready", "run_id": "loop-1", "id": "lead.llm" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_busy", "run_id": "loop-1", "id": "lead.llm" }),
    );
    engine
}

#[test]
fn settled_firing_completes_its_workflow_node_without_killing_its_actor() {
    let mut engine = Engine::new(120, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({
            "kind": "mag.run_started",
            "run_id": "investigate-1",
            "run_name": "investigate",
            "scope": "r5"
        }),
    );
    for (id, factory) in [
        ("task", "nefor.factory.source"),
        ("investigator.llm", "nefor.factory.llm"),
        ("result", "nefor.factory.output"),
    ] {
        dispatch_event(
            &mut engine,
            json!({
                "kind": "mag.actor_spawned",
                "run_id": "investigate-1",
                "id": id,
                "factory": factory
            }),
        );
    }

    for kind in ["mag.actor_ready", "mag.actor_busy", "mag.actor_idle"] {
        dispatch_event(
            &mut engine,
            json!({ "kind": kind, "run_id": "investigate-1", "id": "task" }),
        );
    }
    for kind in ["mag.actor_ready", "mag.actor_busy"] {
        dispatch_event(
            &mut engine,
            json!({
                "kind": kind,
                "run_id": "investigate-1",
                "id": "investigator.llm"
            }),
        );
    }

    let _ = render_str(&mut engine);
    let snap = engine.snapshot();
    assert!(
        snap.contains("MAG investigate (1/3)"),
        "the settled task firing must count as completed while the run remains active:\n{snap}"
    );
    assert!(
        snap.lines().any(|line| {
            line.split('│').any(|segment| {
                let node = segment.trim();
                node.contains("✓ task ")
                    && ["ms", "s", "m", "h"]
                        .iter()
                        .any(|unit| node.ends_with(unit))
            })
        }),
        "task must show its actual firing duration rather than the run lifetime:\n{snap}"
    );
    assert!(
        snap.contains("● investigator") && snap.contains("○ result"),
        "other nodes retain their independent running and pending states:\n{snap}"
    );

    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_busy", "run_id": "investigate-1", "id": "task" }),
    );
    let _ = render_str(&mut engine);
    let snap = engine.snapshot();
    assert!(
        snap.contains("MAG investigate (0/3)") && snap.contains("● task"),
        "a later firing moves the same resident actor back to running:\n{snap}"
    );
}

#[test]
fn member_rows_tick_per_activation_and_idle_rows_do_not() {
    let mut engine = engine_with_cycling_group();

    // Unfold the lead group.
    engine.handle_key(key("tab")).expect("tab");
    let _ = render_str(&mut engine);
    engine.handle_key(key("down")).expect("down");
    engine.handle_key(key("enter")).expect("enter");
    let _ = render_str(&mut engine);

    // 5s in: the working member ticks, the pending one carries no timer.
    // Full-frame snapshots throughout: the incremental diff repaints only
    // changed rows, and "does not tick" IS an unchanged row.
    engine.advance_time(Duration::from_millis(5_000));
    engine.handle_key(key("down")).expect("down");
    let _ = render_str(&mut engine);
    let snap = engine.snapshot();
    assert!(
        snap.contains("lead.llm  working 5s"),
        "the working member must tick its activation elapsed:\n{snap}"
    );
    assert!(
        snap.contains("lead.run-tool  pending") && !snap.contains("pending 5"),
        "a pending member must not tick:\n{snap}"
    );

    // The loop hands over: llm settles idle, run-tool goes busy. Only the
    // working member ticks; the idle one renders quietly, timer-less.
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_idle", "run_id": "loop-1", "id": "lead.llm", "busy_ms": 5000 }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_ready", "run_id": "loop-1", "id": "lead.run-tool" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_busy", "run_id": "loop-1", "id": "lead.run-tool" }),
    );
    engine.advance_time(Duration::from_millis(3_000));
    engine.handle_key(key("down")).expect("down");
    let _ = render_str(&mut engine);
    let snap = engine.snapshot();
    assert!(
        snap.contains("lead.run-tool  working 3s"),
        "the newly-busy member ticks its OWN activation window:\n{snap}"
    );
    assert!(
        snap.contains("lead.llm  idle")
            && !snap.contains("idle 3s")
            && !snap.contains("idle 5s")
            && !snap.contains("idle 8s"),
        "an idle member renders without a timer:\n{snap}"
    );
    assert!(
        !snap.contains("working 8s"),
        "no member may tick the run's wall clock:\n{snap}"
    );

    // The loop cycles back: a fresh llm activation RESETS its timer —
    // per-activation elapsed, not accumulated life.
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_idle", "run_id": "loop-1", "id": "lead.run-tool", "busy_ms": 3000 }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_busy", "run_id": "loop-1", "id": "lead.llm" }),
    );
    engine.advance_time(Duration::from_millis(2_000));
    engine.handle_key(key("up")).expect("up");
    let _ = render_str(&mut engine);
    let snap = engine.snapshot();
    assert!(
        snap.contains("lead.llm  working 2s"),
        "a fresh activation must reset the member timer:\n{snap}"
    );

    // Once every member's latest firing settles, the workflow node is done
    // even though its actors remain resident. Its elapsed time freezes at the
    // final settle rather than continuing to mirror the run wall clock.
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_idle", "run_id": "loop-1", "id": "lead.llm", "busy_ms": 2000 }),
    );
    engine.advance_time(Duration::from_millis(4_000));
    engine.handle_key(key("up")).expect("up");
    let _ = render_str(&mut engine);
    let snap = engine.snapshot();
    assert!(
        snap.contains("MAG loop (1/1)") && snap.contains("✓ lead (2) 10s"),
        "the settled group must be done with a frozen firing window:\n{snap}"
    );
    assert!(
        !snap.contains("working"),
        "no member is working between rounds:\n{snap}"
    );
}

/// Space on a group row opens the COMPOSITE view: the merged member
/// timeline, an llm message and a tool invoke+result attributed to the
/// distinct members that produced them, in chronological (capture) order.
/// Also exercises tool-event capture from `tool-gate.tool.invoke`'s `from`
/// field and the correlated `tool.result`.
/// Tool events attribute to the EMITTING actor named by `from`, not to a
/// sibling: opening the run-tool leaf shows the tool call; the llm leaf,
/// whose own stream carried no tool, does not.
/// Space on a run-header row observes the WHOLE run merged (documented
/// run-header decision): every actor under the run, one timeline.
/// Empty sidebar refuses focus, loudly: Tab with no rows keeps prompt
/// focus AND raises a warning toast that explains the refusal.
#[test]
fn empty_sidebar_tab_warns_and_keeps_prompt_focus() {
    let mut engine = Engine::new(120, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // No runs → the sidebar has no navigable rows. The toast slides in
    // from width 0, so advance past the enter animation before reading
    // the full frame.
    engine.handle_key(key("tab")).expect("tab");
    engine.advance_time(Duration::from_millis(300));
    let _ = render_str(&mut engine);
    let snap = engine.snapshot();
    assert!(
        snap.contains("can't focus an empty sidebar"),
        "Tab on an empty sidebar must raise the warning toast:\n{snap}"
    );
    assert!(
        !snap.contains("· focused"),
        "focus must not move to an empty sidebar (title stays unfocused):\n{snap}"
    );

    // Prompt still owns keys: typing lands in the input.
    for ch in ["h", "i"] {
        engine.handle_key(key(ch)).expect("type");
    }
    let _ = render_str(&mut engine);
    let snap = engine.snapshot();
    assert!(
        snap.contains("hi"),
        "the prompt must keep focus after the refused Tab:\n{snap}"
    );
}

/// A focused sidebar is unmistakable: the title bar carries the focused
/// treatment and the dimmed prompt states the way back in its
/// placeholder.
#[test]
fn focused_sidebar_highlights_title_and_prompt_states_the_way_back() {
    let mut engine = Engine::new(120, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_started", "run_id": "sub-1", "run_name": "demo", "scope": "r2" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_spawned", "run_id": "sub-1", "id": "worker.llm", "factory": "llm" }),
    );

    // Unfocused: plain title, no placeholder hint.
    let out = render_str(&mut engine);
    assert!(
        !out.contains("· focused") && !out.contains("Tab to return"),
        "unfocused surface carries no focus treatment: {out:?}"
    );

    engine.handle_key(key("tab")).expect("tab");
    let out = render_str(&mut engine);
    assert!(
        out.contains("Workflows · focused"),
        "a focused sidebar must carry the highlighted title treatment: {out:?}"
    );
    assert!(
        out.contains("Tab to return"),
        "the dimmed prompt must state the way back in its placeholder: {out:?}"
    );
    // The cursor row stays the in-pane focus indicator.
    let cursor = cursor_styled_text(&out);
    assert!(
        cursor.contains("MAG demo"),
        "the cursor row remains the in-pane focus indicator: {cursor:?}"
    );
}

#[test]
fn overflowing_sidebar_navigation_reveals_top_middle_and_bottom() {
    let mut engine = Engine::new(120, 14).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_started", "run_id": "many-1", "run_name": "many" }),
    );
    for i in 1..=24 {
        dispatch_event(
            &mut engine,
            json!({
                "kind": "mag.actor_spawned", "run_id": "many-1",
                "id": format!("group{i:02}.llm"), "factory": "llm"
            }),
        );
    }
    let _ = render_str(&mut engine);
    engine.handle_key(key("tab")).expect("focus sidebar");
    let _ = render_str(&mut engine);

    let pos = |engine: &Engine| -> (i64, i64) {
        engine
            .lua()
            .load(r#"local p=tui.scroll_position("sidebar"); return p.offset,p.max"#)
            .eval()
            .expect("sidebar position")
    };
    let (top, max) = pos(&engine);
    assert_eq!(top, 0);
    assert!(max > 0, "long sidebar must overflow");
    let top_frame = engine.snapshot();
    let sidebar_bar = |frame: &str| -> Vec<usize> {
        frame
            .lines()
            .enumerate()
            .filter_map(|(row, line)| line.chars().skip(84).any(|cell| cell == '█').then_some(row))
            .collect()
    };
    let top_thumb = sidebar_bar(&top_frame);
    assert!(
        !top_thumb.is_empty() && top_thumb[0] <= 1,
        "overflowing sidebar must paint its thumb at the top:\n{top_frame}"
    );

    engine.handle_key(key("pagedown")).expect("first page down");
    let _ = render_str(&mut engine);
    engine
        .handle_key(key("pagedown"))
        .expect("second page down");
    let _ = render_str(&mut engine);
    let (middle, _) = pos(&engine);
    assert!(
        middle > 0 && middle < max,
        "page navigation should reveal a middle row"
    );

    engine.handle_key(key("end")).expect("end");
    let _ = render_str(&mut engine);
    let (bottom, max) = pos(&engine);
    assert_eq!(bottom, max, "End must reveal the final sidebar row");
    let bottom_frame = engine.snapshot();
    assert!(bottom_frame.contains("group24"));
    let bottom_thumb = sidebar_bar(&bottom_frame);
    assert!(
        !bottom_thumb.is_empty() && bottom_thumb.last().copied() == Some(13),
        "scrollbar thumb must land at the sidebar viewport bottom:\n{bottom_frame}"
    );

    engine.handle_key(key("down")).expect("bounded down");
    let _ = render_str(&mut engine);
    assert_eq!(pos(&engine).0, max, "Down at the final row stays bounded");

    engine
        .handle_mouse(MouseMessage {
            kind: MouseKind::Wheel,
            x: 100,
            y: 5,
            button: Some("up"),
            mods: vec![],
        })
        .expect("wheel sidebar up");
    let _ = render_str(&mut engine);
    let wheeled = pos(&engine).0;
    assert_eq!(wheeled, max - 3, "wheel owns the viewport passively");
    engine.handle_key(key("up")).expect("navigate after wheel");
    let _ = render_str(&mut engine);
    let after_key = pos(&engine).0;
    assert!(
        after_key <= wheeled,
        "navigation resumes from the nearest visible row instead of snapping back down"
    );

    engine.handle_key(key("home")).expect("home");
    let _ = render_str(&mut engine);
    assert_eq!(pos(&engine).0, 0, "Home must reveal the first sidebar row");

    engine.handle_resize(120, 40).expect("grow terminal");
    let _ = render_str(&mut engine);
    assert_eq!(
        pos(&engine),
        (0, 0),
        "growing until content fits must clamp the offset and remove overflow"
    );
    let fitted = engine.snapshot();
    assert!(
        sidebar_bar(&fitted).is_empty(),
        "auto scrollbar must disappear when resized content fits:\n{fitted}"
    );

    engine.handle_resize(120, 14).expect("shrink terminal");
    let _ = render_str(&mut engine);
    let (resized_offset, resized_max) = pos(&engine);
    assert_eq!(
        resized_offset, 0,
        "top selection must remain visible after shrink"
    );
    assert!(resized_max > 0, "shrinking must restore sidebar overflow");
    assert!(
        !sidebar_bar(&engine.snapshot()).is_empty(),
        "shrinking must restore the visible scrollbar"
    );
}

#[test]
fn short_sidebar_has_no_scroll_extent_and_keeps_content_width() {
    let mut engine = Engine::new(120, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_started", "run_id": "short-1", "run_name": "short" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_spawned", "run_id": "short-1", "id": "worker.llm", "factory": "llm" }),
    );
    let _ = render_str(&mut engine);
    let (offset, max, inner_width): (i64, i64, i64) = engine
        .lua()
        .load(r#"local p=tui.scroll_position("sidebar"); return p.offset,p.max,p.inner_width"#)
        .eval()
        .expect("sidebar position");
    assert_eq!((offset, max), (0, 0));
    assert_eq!(
        inner_width, 36,
        "auto scrollbar must not reserve a gutter or narrow the restored sidebar width"
    );
}

// ── Run-result (mag workflow) rendering ──────────────────────────────────

/// The collapsed run-result entry reads as a `mag workflow` line carrying
/// the human-readable run name and wall-clock duration — nothing else.
/// The machine detail (exact run_id, node count) only appears once the
/// entry is unfolded (Ctrl+O), matching the reasoning/tool fold vocabulary.
#[test]
fn graph_result_collapsed_shows_workflow_name_and_duration_only() {
    let mut engine = Engine::new(120, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.graph_result.append",
            "run_id": "mag-nefor_bughunt-2026-07-06T14:14:16.709Z",
            "run_name": "nefor_bughunt",
            "status": "success",
            "nodes": [
                { "id": "worker", "role": "llm" },
                { "id": "out", "role": "sink" }
            ],
            "duration_ms": 34000,
            "output": "output_path: /tmp/out.md",
        }),
    );

    let _ = render_str(&mut engine);
    let out = engine.snapshot();
    assert!(
        out.contains("mag workflow") && out.contains("nefor_bughunt") && out.contains("34s"),
        "collapsed run-result must show `mag workflow · <name> · <duration>`: {out:?}"
    );
    assert!(
        !out.contains("mag-nefor_bughunt-2026") && !out.contains("2 nodes"),
        "collapsed run-result must NOT show the raw run_id or node count: {out:?}"
    );

    // Unfold: run_id, node count, and the output path all surface.
    engine.handle_key(key("ctrl_o")).expect("ctrl_o");
    let _ = render_str(&mut engine);
    let out = engine.snapshot();
    assert!(
        out.contains("run_id:") && out.contains("mag-nefor_bughunt"),
        "unfolded run-result must show the exact run_id: {out:?}"
    );
    assert!(
        out.contains("2 nodes"),
        "unfolded run-result must show the node count: {out:?}"
    );
    assert!(
        out.contains("output_path:"),
        "unfolded run-result must show the output path: {out:?}"
    );
}

/// A failed run keeps the `mag workflow` framing but appends a FAILED tail
/// so a failure is unmistakable in the collapsed view.
#[test]
fn graph_result_failed_collapsed_reads_failed() {
    let mut engine = Engine::new(120, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.graph_result.append",
            "run_id": "mag-broken-123",
            "run_name": "broken_run",
            "status": "failed",
            "nodes": [ { "id": "worker", "role": "llm" } ],
            "duration_ms": 5000,
            "error": "node worker crashed",
        }),
    );

    let _ = render_str(&mut engine);
    let out = engine.snapshot();
    assert!(
        out.contains("mag workflow") && out.contains("broken_run") && out.contains("FAILED"),
        "failed run-result must read `mag workflow · <name> · … · FAILED`: {out:?}"
    );
}

/// A long output path is fully visible — wrapped across transcript rows —
/// in the unfolded run-result, never single-line-clipped.
#[test]
fn graph_result_long_output_path_wraps_fully() {
    let mut engine = Engine::new(80, 40).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    let path = "/Users/skril/Vault/Projects/personal/nefor/very/deeply/nested/directory/structure/that/keeps/going/output-artifact-final-result.md";
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.graph_result.append",
            "run_id": "mag-longpath-1",
            "run_name": "longpath",
            "status": "success",
            "nodes": [ { "id": "out", "role": "sink" } ],
            "duration_ms": 1000,
            "output": format!("output_path: {path}"),
        }),
    );
    engine.handle_key(key("ctrl_o")).expect("ctrl_o");
    let _ = render_str(&mut engine);

    // Strip whitespace so a path wrapped across rows re-joins contiguously.
    // Reconstruct just the transcript (left) column — the sidebar lives to
    // the right of a `│` separator on every row, so stripping whitespace
    // across the full frame would interleave sidebar text between the
    // wrapped content fragments. Cut each row at the sidebar boundary, then
    // strip whitespace so content wrapped across rows re-joins contiguously.
    let joined: String = engine
        .snapshot()
        .lines()
        .map(|l| l.split('│').next().unwrap_or(""))
        .collect::<String>()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    assert!(
        joined.contains(path),
        "the full output path must be visible (wrapped) in the unfolded run-result"
    );
}

/// A long single-line tool RESULT (path/URL/one-line JSON) wraps to the
/// transcript width in the unfolded tool entry — no truncation.
#[test]
fn tool_result_long_single_line_wraps_fully() {
    let mut engine = Engine::new(80, 40).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    let long = format!("https://example.com/api/v1/resource/{}", "x".repeat(160));
    fixture_tool_started(&mut engine, "t1", "Bash", json!("curl url"));
    fixture_tool_completed(&mut engine, "t1", json!(long.clone()), false);
    engine.handle_key(key("ctrl_o")).expect("ctrl_o");
    engine.handle_key(key("ctrl_r")).expect("ctrl_r");
    let _ = render_str(&mut engine);

    // Reconstruct just the transcript (left) column — the sidebar lives to
    // the right of a `│` separator on every row, so stripping whitespace
    // across the full frame would interleave sidebar text between the
    // wrapped content fragments. Cut each row at the sidebar boundary, then
    // strip whitespace so content wrapped across rows re-joins contiguously.
    let joined: String = engine
        .snapshot()
        .lines()
        .map(|l| l.split('│').next().unwrap_or(""))
        .collect::<String>()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    assert!(
        joined.contains(&long),
        "the full tool result must be visible (wrapped) in the unfolded tool entry"
    );
}

/// A long tool INPUT (a mag-eval expression, long path, write payload) is
/// fully visible — wrapped — in the unfolded tool entry. The collapsed row
/// keeps its truncated one-line summary; the requirement is that the full
/// input is visible somewhere in the unfolded entry.
#[test]
fn tool_input_long_single_line_wraps_fully() {
    let mut engine = Engine::new(80, 40).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    let expr = format!("(pipeline{})", "-step".repeat(40));
    fixture_tool_started(
        &mut engine,
        "t1",
        "mag-eval",
        json!({ "expression": expr.clone() }),
    );
    engine.handle_key(key("ctrl_o")).expect("ctrl_o");
    engine.handle_key(key("ctrl_r")).expect("ctrl_r");
    let _ = render_str(&mut engine);

    // Reconstruct just the transcript (left) column — the sidebar lives to
    // the right of a `│` separator on every row, so stripping whitespace
    // across the full frame would interleave sidebar text between the
    // wrapped content fragments. Cut each row at the sidebar boundary, then
    // strip whitespace so content wrapped across rows re-joins contiguously.
    let joined: String = engine
        .snapshot()
        .lines()
        .map(|l| l.split('│').next().unwrap_or(""))
        .collect::<String>()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    assert!(
        joined.contains(&expr),
        "the full tool input must be visible (wrapped) in the unfolded tool entry"
    );
}

/// Regression: `/new` (and the other reset-shaped paths) must collapse the
/// transcript's scroll geometry immediately. Clearing `state.entries`
/// without invalidating the virtual scroll cache left an empty viewport
/// pinned to the previous session's extent.
#[test]
fn slash_new_collapses_stale_scroll_region() {
    let mut engine = Engine::new(60, 12).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    // Overflow the viewport so the virtual-scroll cache has real extent.
    for i in 0..25 {
        fixture_message(
            &mut engine,
            "user",
            format!("message number {i} filling the transcript"),
        );
    }
    let _ = render_str(&mut engine);
    fixture_message(&mut engine, "user", "one more line");
    let _ = render_str(&mut engine);
    let before = engine.snapshot();
    assert!(
        before.contains("one more line"),
        "precondition: overflowing transcript did not render its tail: {before:?}"
    );

    // Reset via /new.
    for ch in "/new".chars() {
        engine.handle_key(key(&ch.to_string())).expect("type");
    }
    engine.handle_key(key("enter")).expect("enter");
    dispatch_event(
        &mut engine,
        json!({ "kind": "sessions.session_start", "session_id": "new",
            "request_id": "chat-transition-1" }),
    );
    let _ = render_str(&mut engine);
    let after = engine.snapshot();

    assert!(
        !after.contains("one more line"),
        "reset should remove the previous transcript: {after:?}"
    );
    let max: i64 = engine
        .lua()
        .load(
            r#"
            local p = tui.scroll_position("transcript")
            return p and p.max or 0
            "#,
        )
        .eval()
        .expect("scroll max after /new");
    assert_eq!(max, 0, "reset should collapse stale scroll extent");
}

#[test]
fn malformed_and_replayed_instruction_notices_never_enter_main_transcript() {
    let mut engine = Engine::new(120, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);
    dispatch_event(
        &mut engine,
        json!({ "kind": "sessions.session_start", "session_id": "s" }),
    );

    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.instruction.notice", "notice_id": "bad", "path": "/private",
            "text": "Local instruction files available for /private",
            "invocation": { "principal": "lead" }
        }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "sessions.replay.start", "session_id": "s" }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.instruction.notice", "notice_id": "replayed", "path": "/historical",
            "text": "Local instruction files available for /historical",
            "invocation": {
                "session_id": "s", "run_id": "old-run", "run_scope": "r9",
                "actor_id": "old.run-tool", "capability_id": "r9/cap-1", "principal": "subagent"
            }
        }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "sessions.replay.end", "session_id": "s" }),
    );
    let state = engine.state_table().expect("state");
    let entries: mlua::Table = state.get("entries").expect("entries");
    assert_eq!(entries.raw_len(), 0);
    let snapshot = render_str(&mut engine);
    assert!(!snapshot.contains("/private"), "{snapshot}");
    assert!(!snapshot.contains("/historical"), "{snapshot}");
}

#[test]
fn tool_path_summary_keeps_tail_collapsed_and_wraps_full_path_expanded() {
    for (name, arg, label) in [("read_file", "path", "Read file"), ("mag", "file", "MAG")] {
        let mut engine = Engine::new(80, 30).expect("engine");
        engine.load_scenario(&chat_lua_source()).expect("load");
        let _ = render_str(&mut engine);
        dispatch_event(
            &mut engine,
            json!({ "kind": "tool.register", "tools": [{
                "name": name,
                "display": {
                    "label": label,
                    "primary": { "arg": arg },
                    "result": { "kind": "receipt", "text": "content loaded" }
                }
            }] }),
        );
        let path = "/home/user/projects/nefor/src/rendering/deep/components/useful_file.rs";
        fixture_tool_started(
            &mut engine,
            &format!("{name}-path-display"),
            name,
            json!({ arg: path }),
        );

        let collapsed = render_snapshot(&mut engine);
        assert!(collapsed.contains("useful_file.rs"), "{name}: {collapsed}");
        assert!(
            !collapsed.contains("/home/user/projects"),
            "{name}: {collapsed}"
        );

        engine.handle_key(key("ctrl_o")).expect("expand");
        let expanded = render_snapshot(&mut engine);
        let compact: String = expanded
            .lines()
            .map(|line| line.split('│').next().unwrap_or(line).trim())
            .collect();
        assert!(
            compact.contains(path),
            "{name}: expanded path was not preserved across wrapped lines:\n{expanded}"
        );
    }
}

#[test]
fn tool_display_projection_preserves_entry_payload_identity() {
    let mut engine = Engine::new(120, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);
    dispatch_event(
        &mut engine,
        json!({ "kind": "tool.register", "tools": [{
            "name": "loader",
            "display": {
                "label": "Load",
                "primary": { "arg": "path", "cwd_arg": "cwd" },
                "result": { "kind": "receipt", "text": "loaded" }
            }
        }] }),
    );
    fixture_tool_started(
        &mut engine,
        "identity-id",
        "loader",
        json!({
            "path": "secret.txt", "cwd": "/exact/root", "token": "EXACT INPUT TOKEN"
        }),
    );
    fixture_tool_completed(
        &mut engine,
        "identity-id",
        json!({
            "body": "EXACT MODEL RESULT", "count": 2
        }),
        false,
    );

    engine.handle_key(key("ctrl_o")).expect("expand");
    let semantic = render_snapshot(&mut engine);
    assert!(
        semantic.contains("loader · secret.txt (cwd: /exact/root)"),
        "{semantic}"
    );
    assert!(!semantic.contains("EXACT INPUT TOKEN"), "{semantic}");
    assert!(!semantic.contains("EXACT MODEL RESULT"), "{semantic}");

    let state = engine.state_table().expect("state");
    let entries: mlua::Table = state.get("entries").expect("entries");
    let entry: mlua::Table = entries.get(entries.raw_len()).expect("tool entry");
    let raw_input: mlua::Table = entry.get("raw_input").expect("raw input");
    let input_table: mlua::Table = entry.get("input_table").expect("input table");
    let output: mlua::Table = entry.get("output").expect("output");
    assert_eq!(
        raw_input.to_pointer(),
        input_table.to_pointer(),
        "transcript must retain the exact invocation table"
    );
    assert_eq!(raw_input.get::<String>("path").unwrap(), "secret.txt");
    assert_eq!(raw_input.get::<String>("cwd").unwrap(), "/exact/root");
    assert_eq!(
        raw_input.get::<String>("token").unwrap(),
        "EXACT INPUT TOKEN"
    );
    assert_eq!(output.get::<String>("body").unwrap(), "EXACT MODEL RESULT");
    assert_eq!(output.get::<i64>("count").unwrap(), 2);
    drop(state);

    dispatch_event(
        &mut engine,
        json!({ "kind": "input.submit", "value": "/raw identity-id" }),
    );
    let raw = render_snapshot(&mut engine);
    assert!(raw.contains("EXACT INPUT TOKEN"), "{raw}");
    assert!(raw.contains("EXACT MODEL RESULT"), "{raw}");
}

#[test]
fn personal_extension_behavior_uses_canonical_tui() {
    let Some(personal_config) = std::env::var_os("NEFOR_PERSONAL_CONFIG_DIR") else {
        return;
    };
    let personal_config = PathBuf::from(personal_config);
    let temp = tempfile::tempdir().expect("personal chat tempdir");
    let config = temp.path().join("config");
    std::fs::create_dir_all(&config).expect("config dir");
    std::fs::create_dir_all(config.join("mag/lib")).expect("catalog dir");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(personal_config.join("config"), config.join("config"))
            .expect("link personal runtime config");
        std::os::unix::fs::symlink(
            personal_config.join("chat-extension.lua"),
            config.join("chat-extension.lua"),
        )
        .expect("link personal chat extension");
        std::os::unix::fs::symlink(
            personal_config.join("chat-save.lua"),
            config.join("chat-save.lua"),
        )
        .expect("link personal chat save helper");
        std::os::unix::fs::symlink(
            personal_config.join("tool-catalog.lua"),
            config.join("tool-catalog.lua"),
        )
        .expect("link personal tool catalog loader");
        std::os::unix::fs::symlink(
            personal_config.join("mag/lib/tool-catalog.json"),
            config.join("mag/lib/tool-catalog.json"),
        )
        .expect("link personal tool catalog");
        std::os::unix::fs::symlink(
            personal_config.join("gemma-audio-core.lua"),
            config.join("gemma-audio-core.lua"),
        )
        .expect("link audio config");
    }
    let kb = temp.path().join("kb");
    std::fs::create_dir_all(kb.join("inbox/audio-discussions")).expect("save dir");
    std::fs::write(
        config.join("agentic-kit.json"),
        serde_json::to_vec(&json!({
            "knowledge_base": kb,
            "nefor_repo": PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .parent().and_then(|p| p.parent()).unwrap(),
        }))
        .unwrap(),
    )
    .expect("metadata");

    let mut engine = Engine::new(120, 40).expect("engine");
    engine
        .load_scenario(&canonical_chat_lua_source_for_config(&config))
        .expect("load personal chat");
    let _ = render_str(&mut engine);

    let raw_tool_id = |engine: &Engine| -> Option<String> {
        engine.state_table().unwrap().get("raw_tool_id").unwrap()
    };
    let popup_body = |engine: &Engine| -> Option<String> {
        let state = engine.state_table().unwrap();
        let popup: Option<mlua::Table> = state.get("popup").unwrap();
        popup.and_then(|p| p.get("body").ok())
    };

    // Personal config must preserve the shared per-entry raw receipt path.
    // The command is exercised against a real entry so both state and body
    // visibility are meaningful when this optional integration is enabled.
    fixture_tool_started(
        &mut engine,
        "personal-raw-id",
        "personal_unknown",
        json!({ "token": "PERSONAL SECRET INPUT" }),
    );
    fixture_tool_completed(
        &mut engine,
        "personal-raw-id",
        json!("PERSONAL SECRET OUTPUT"),
        false,
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "input.submit", "value": "/raw personal-raw-id" }),
    );
    assert_eq!(raw_tool_id(&engine).as_deref(), Some("personal-raw-id"));
    let revealed = render_snapshot(&mut engine);
    assert!(revealed.contains("PERSONAL SECRET INPUT"), "{revealed}");
    assert!(revealed.contains("PERSONAL SECRET OUTPUT"), "{revealed}");
    dispatch_event(
        &mut engine,
        json!({ "kind": "input.submit", "value": "/raw personal-raw-id" }),
    );
    assert_eq!(raw_tool_id(&engine), None);
    let hidden = render_snapshot(&mut engine);
    assert!(!hidden.contains("PERSONAL SECRET INPUT"), "{hidden}");
    assert!(!hidden.contains("PERSONAL SECRET OUTPUT"), "{hidden}");

    // /chatlog has independent on/off/toggle behavior and invalid args do
    // not accidentally toggle the module-local logger.
    dispatch_event(
        &mut engine,
        json!({ "kind": "input.submit", "value": "/chatlog off" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "input.submit", "value": "/chatlog maybe" }),
    );
    assert_eq!(
        popup_body(&engine).as_deref(),
        Some("Usage: /chatlog [on|off]")
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "input.submit", "value": "/chatlog" }),
    );
    let state = engine.state_table().unwrap();
    let toasts: mlua::Table = state.get("toasts").unwrap();
    let last: mlua::Table = toasts.get(toasts.raw_len()).unwrap();
    assert_eq!(
        last.get::<String>("text").unwrap(),
        "chat diagnostic logging ON"
    );
    drop(state);
    dispatch_event(
        &mut engine,
        json!({ "kind": "input.submit", "value": "/chatlog on" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "input.submit", "value": "/chatlog off" }),
    );
    let state = engine.state_table().unwrap();
    let toasts: mlua::Table = state.get("toasts").unwrap();
    let last: mlua::Table = toasts.get(toasts.raw_len()).unwrap();
    assert_eq!(
        last.get::<String>("text").unwrap(),
        "chat diagnostic logging OFF"
    );
    drop(state);

    // Public prompt behavior exposes on/off candidates for chat logging.
    let command = "/chatlog ";
    dispatch_event(
        &mut engine,
        json!({ "kind": "input.changed", "value": command }),
    );
    let state = engine.state_table().unwrap();
    let completion: mlua::Table = state.get("completion").unwrap();
    let matches: mlua::Table = completion.get("matches").unwrap();
    let names = matches
        .sequence_values::<mlua::Table>()
        .map(|entry| entry.unwrap().get::<String>("name").unwrap())
        .collect::<Vec<_>>();
    assert!(
        names.iter().any(|name| name.ends_with(" on")),
        "{command}: {names:?}"
    );
    assert!(
        names.iter().any(|name| name.ends_with(" off")),
        "{command}: {names:?}"
    );

    // Personal-only commands still execute through the real reducer.
    // Clear the completion popup left by the candidate assertions first;
    // Enter with an open combobox intentionally selects its highlighted row.
    dispatch_event(&mut engine, json!({ "kind": "input.changed", "value": "" }));
    dispatch_event(
        &mut engine,
        json!({ "kind": "input.submit", "value": "/save" }),
    );
    let state = engine.state_table().unwrap();
    let toasts: mlua::Table = state.get("toasts").unwrap();
    let last: mlua::Table = toasts.get(toasts.raw_len()).unwrap();
    assert!(last.get::<String>("text").unwrap().starts_with("saved "));
    drop(state);
    dispatch_event(
        &mut engine,
        json!({ "kind": "input.submit", "value": "/mode audio" }),
    );
    let state = engine.state_table().unwrap();
    assert_eq!(state.get::<String>("mode").unwrap(), "audio");
    assert_eq!(state.get::<String>("provider").unwrap(), "gemma-audio");
    assert_eq!(
        state.get::<String>("model").unwrap(),
        "gemma-4-12b-audio-q4"
    );
    drop(state);
    let effects = engine.take_emit_queue();
    assert!(effects
        .iter()
        .any(|(_, body)| body.get("kind") == Some(&json!("chat.model.set"))));

    let current = json!({
        "kind": "tool.register",
        "tools": [
            { "name": "loader", "display": { "label": "Load current", "primary": { "arg": "path" }, "result": { "kind": "receipt", "text": "content loaded" } } },
            { "name": "content", "display": { "label": "Show content", "result": { "kind": "content" } } },
            { "name": "removed", "display": { "label": "Old removed label", "result": { "kind": "content" } } }
        ]
    });
    dispatch_event(&mut engine, current);
    dispatch_event(&mut engine, json!({ "kind": "sessions.replay.start" }));
    dispatch_event(
        &mut engine,
        json!({ "kind": "tool.register", "tools": [{ "name": "loader", "display": { "label": "STALE LABEL", "result": { "kind": "content" } } }] }),
    );
    dispatch_event(&mut engine, json!({ "kind": "sessions.replay.end" }));

    // A malformed live aggregate fails atomically and leaves the current
    // catalog intact.
    let malformed = json!({ "kind": "tool.register", "tools": [{ "name": "loader", "display": { "label": "Broken", "arguments": {}, "result": { "kind": "content" } } }] });
    let malformed_map = malformed.as_object().unwrap().clone();
    assert!(engine.dispatch_envelope_body(&malformed_map).is_err());

    // Full live replacement removes omitted tools.
    dispatch_event(
        &mut engine,
        json!({ "kind": "tool.register", "tools": [
        { "name": "loader", "display": { "label": "Load current", "primary": { "arg": "path" }, "result": { "kind": "receipt", "text": "content loaded" } } },
        { "name": "content", "display": { "label": "Show content", "result": { "kind": "content" } } }
    ] }),
    );
    fixture_tool_started(&mut engine, "removed-id", "removed", json!({}));
    fixture_tool_completed(&mut engine, "removed-id", json!("removed-output"), false);
    fixture_tool_started(
        &mut engine,
        "load-id",
        "loader",
        json!({ "path": "secret.txt" }),
    );
    fixture_tool_completed(
        &mut engine,
        "load-id",
        json!("SECRET SUCCESS PAYLOAD"),
        false,
    );
    fixture_tool_started(&mut engine, "content-id", "content", json!({}));
    fixture_tool_completed(
        &mut engine,
        "content-id",
        json!("VISIBLE CONTENT RESULT"),
        false,
    );
    fixture_tool_started(
        &mut engine,
        "error-id",
        "loader",
        json!({ "path": "bad.txt" }),
    );
    fixture_tool_completed(&mut engine, "error-id", json!("VISIBLE ERROR RESULT"), true);
    let expanded_details = engine
        .state_table()
        .expect("state")
        .get::<bool>("expanded_details")
        .expect("expanded_details");
    if !expanded_details {
        engine
            .handle_key(key("ctrl_o"))
            .expect("expand semantic tools");
    }
    let semantic = render_snapshot(&mut engine);
    assert!(semantic.contains("loader · secret.txt"), "{semantic}");
    assert!(semantic.contains("content loaded"), "{semantic}");
    assert!(!semantic.contains("SECRET SUCCESS PAYLOAD"), "{semantic}");
    assert!(!semantic.contains("VISIBLE CONTENT RESULT"), "{semantic}");
    assert!(semantic.contains("VISIBLE ERROR RESULT"), "{semantic}");
    assert!(
        semantic.contains("removed") && !semantic.contains("Old removed label"),
        "{semantic}"
    );
    assert!(!semantic.contains("STALE LABEL"), "{semantic}");

    dispatch_event(
        &mut engine,
        json!({ "kind": "input.submit", "value": "/raw load-id" }),
    );
    assert_eq!(raw_tool_id(&engine).as_deref(), Some("load-id"));
    let raw = render_snapshot(&mut engine);
    assert!(raw.contains("SECRET SUCCESS PAYLOAD"), "{raw}");
    assert!(raw.contains("secret.txt"), "{raw}");
}

#[test]
#[cfg(unix)]
fn config_chat_extension_coexists_with_canonical_conversation_projection() {
    let temp = tempfile::tempdir().expect("chat extension tempdir");
    let config = temp.path().join("config");
    std::fs::create_dir_all(config.join("config")).expect("config module dir");
    std::fs::create_dir_all(config.join("chat")).expect("poison chat dir");
    std::fs::write(
        config.join("chat/update.lua"),
        "error('foreign config shadowed canonical chat.update')",
    )
    .expect("write poison reducer");
    std::fs::write(
        config.join("chat/statusline.lua"),
        "error('foreign config shadowed canonical chat.statusline')",
    )
    .expect("write poison statusline");
    std::fs::write(
        config.join("chat/slash.lua"),
        "error('foreign config shadowed canonical chat.slash')",
    )
    .expect("write poison slash registry");
    std::fs::write(
        config.join("config/init.lua"),
        r#"
        return { active = {
          default_provider = "mock-plugin",
          default_model = "mock-model",
          default_reasoning_effort = "medium",
          chat_extension = "test-chat-extension",
        } }
        "#,
    )
    .expect("write config");
    std::fs::write(
        config.join("test-chat-extension.lua"),
        r#"
        local function effect(kind, value)
          return { kind = "send_to", target = "engine",
            body = { kind = kind, value = value } }
        end
        return {
          initial_state = function(_state) return { extension_value = "ready" } end,
          commands = {
            {
              name = "echo", hint = "set extension state", takes_args = true,
              arg_completions = { { name = "hello", hint = "test value" } },
              run = function(args, state, api)
                assert(state.extension_value ~= nil)
                return api.finish({ extension_value = args or "" }),
                  { effect("test.extension.echo", args) }
              end,
            },
            {
              name = "mode", hint = "test mode", takes_args = true,
              arg_completions = { { name = "audio", hint = "test mode" } },
              run = function(args, _state, api)
                if args ~= "audio" then return nil end
                return api.new_session({
                  mode = "audio", provider = "audio-provider", model = "audio-model",
                }), {
                  effect("sessions.new_request"),
                  { kind = "send_to", target = "engine", body = {
                    kind = "chat.model.set", provider = "audio-provider", model = "audio-model",
                  } },
                }
              end,
            },
            {
              name = "mutate", hint = "prove state is immutable", takes_args = false,
              run = function(_args, state, _api)
                state.mode = "corrupted"
              end,
            },
          },
          status_segments = function(state)
            return { { spans = { { text = "EXT:" .. tostring(state.extension_value) } } } }
          end,
        }
        "#,
    )
    .expect("write extension");

    let mut engine = Engine::new(120, 40).expect("engine");
    engine
        .load_scenario(&canonical_chat_lua_source_for_config(&config))
        .expect("load canonical chat with extension");
    let initial = render_snapshot(&mut engine);
    assert!(initial.contains("EXT:ready"), "{initial}");

    dispatch_event(
        &mut engine,
        json!({ "kind": "input.changed", "value": "/echo " }),
    );
    let state = engine.state_table().expect("state");
    let completion: mlua::Table = state.get("completion").expect("completion");
    let matches: mlua::Table = completion.get("matches").expect("matches");
    let names = matches
        .sequence_values::<mlua::Table>()
        .map(|entry| entry.unwrap().get::<String>("name").unwrap())
        .collect::<Vec<_>>();
    assert!(names.iter().any(|name| name == "echo hello"), "{names:?}");
    drop(state);

    dispatch_event(&mut engine, json!({ "kind": "input.changed", "value": "" }));
    dispatch_event(
        &mut engine,
        json!({ "kind": "input.submit", "value": "/echo configured" }),
    );
    let state = engine.state_table().expect("state");
    assert_eq!(
        state.get::<String>("extension_value").unwrap(),
        "configured"
    );
    drop(state);
    assert!(engine
        .take_emit_queue()
        .iter()
        .any(|(_, body)| body.get("kind") == Some(&json!("test.extension.echo"))));

    dispatch_event(
        &mut engine,
        json!({ "kind": "input.submit", "value": "/mode audio" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "sessions.session_start", "session_id": "audio-session",
            "request_id": "chat-transition-1" }),
    );
    let state = engine.state_table().expect("state");
    assert_eq!(state.get::<String>("mode").unwrap(), "audio");
    assert_eq!(state.get::<String>("provider").unwrap(), "audio-provider");
    assert_eq!(state.get::<String>("model").unwrap(), "audio-model");
    drop(state);

    fixture_assistant_delta(&mut engine, "CANONICAL ANSWER");
    fixture_assistant_completed(
        &mut engine,
        Some("CANONICAL ANSWER".into()),
        json!({
            "model": "ext-model", "duration_ms": 2_000,
            "usage": { "output_tokens": 40 }
        }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.stream.delta", "text": "LEGACY DUPLICATE" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.stream.end", "text": "LEGACY DUPLICATE",
            "model": "legacy-model", "duration_ms": 99_000 }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "chat.session.stats", "model": "legacy-model",
            "duration_ms": 99_000, "output_tokens": 1 }),
    );
    let snapshot = render_snapshot(&mut engine);
    assert!(snapshot.contains("CANONICAL ANSWER"), "{snapshot}");
    assert!(!snapshot.contains("LEGACY DUPLICATE"), "{snapshot}");
    assert!(!snapshot.contains("legacy-model"), "{snapshot}");
    assert_eq!(
        snapshot.matches("▣ ext-model · 2s · 20 tok/s").count(),
        1,
        "extension must not duplicate the canonical terminal footer:\n{snapshot}"
    );

    let mutation = json!({ "kind": "input.submit", "value": "/mutate" });
    let mutation_body = mutation.as_object().expect("mutation body").clone();
    let error = engine
        .dispatch_envelope_body(&mutation_body)
        .expect_err("extension must not mutate canonical state");
    assert!(error.to_string().contains("read-only"), "{error}");
    let state = engine.state_table().expect("state after rejected mutation");
    assert_eq!(state.get::<String>("mode").unwrap(), "audio");
}

#[test]
fn starter_tool_catalog_replay_freshness_and_atomic_replacement() {
    let mut engine = Engine::new(100, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);

    let label = |engine: &Engine, name: &str| -> Option<String> {
        let state = engine.state_table().unwrap();
        let displays: mlua::Table = state.get("tool_displays").unwrap();
        let display: Option<mlua::Table> = displays.get(name).unwrap();
        display.and_then(|d| d.get("label").ok())
    };

    dispatch_event(
        &mut engine,
        json!({ "kind": "tool.register", "tools": [
        { "name": "current", "display": { "label": "Current catalog", "arguments": [], "result": { "kind": "content" } } },
        { "name": "removed", "display": { "label": "Will be removed", "result": { "kind": "content" } } }
    ] }),
    );
    assert_eq!(
        label(&engine, "current").as_deref(),
        Some("Current catalog")
    );

    dispatch_event(&mut engine, json!({ "kind": "sessions.replay.start" }));
    dispatch_event(
        &mut engine,
        json!({ "kind": "tool.register", "tools": [
        { "name": "current", "display": { "label": "Stale catalog", "result": { "kind": "content" } } }
    ] }),
    );
    dispatch_event(&mut engine, json!({ "kind": "sessions.replay.end" }));
    assert_eq!(
        label(&engine, "current").as_deref(),
        Some("Current catalog")
    );
    assert_eq!(
        label(&engine, "removed").as_deref(),
        Some("Will be removed")
    );

    // Validation builds a fresh local catalog and only swaps state after the
    // whole aggregate succeeds.
    let malformed = json!({ "kind": "tool.register", "tools": [
        { "name": "current", "display": { "label": "Broken", "arguments": {}, "result": { "kind": "content" } } }
    ] });
    assert!(engine
        .dispatch_envelope_body(&malformed.as_object().unwrap().clone())
        .is_err());
    assert_eq!(
        label(&engine, "current").as_deref(),
        Some("Current catalog")
    );

    dispatch_event(
        &mut engine,
        json!({ "kind": "tool.register", "tools": [
        { "name": "current", "display": { "label": "Replacement catalog", "result": { "kind": "content" } } }
    ] }),
    );
    assert_eq!(
        label(&engine, "current").as_deref(),
        Some("Replacement catalog")
    );
    assert_eq!(label(&engine, "removed"), None);

    // A cold TUI has no live authority yet, so replay can hydrate the catalog.
    let mut cold = Engine::new(100, 24).expect("cold engine");
    cold.load_scenario(&chat_lua_source()).expect("cold load");
    dispatch_event(&mut cold, json!({ "kind": "sessions.replay.start" }));
    dispatch_event(
        &mut cold,
        json!({ "kind": "tool.register", "tools": [
        { "name": "late", "display": { "label": "Late attach catalog", "result": { "kind": "content" } } }
    ] }),
    );
    dispatch_event(&mut cold, json!({ "kind": "sessions.replay.end" }));
    assert_eq!(label(&cold, "late").as_deref(), Some("Late attach catalog"));
}

// Consumer-local node projection regressions.
fn open_single_node(engine: &mut Engine, factory: &str, actor: &str) {
    let _ = render_str(engine);
    dispatch_event(
        engine,
        json!({
            "kind": "mag.run_started", "run_id": "preview-run", "run_name": "preview-demo", "scope": "rp"
        }),
    );
    dispatch_event(
        engine,
        json!({
            "kind": "mag.actor_spawned", "run_id": "preview-run", "id": actor, "factory": factory
        }),
    );
    engine.handle_key(key("tab")).expect("focus sidebar");
    let _ = render_str(engine);
    engine.handle_key(key("down")).expect("select group");
    engine.handle_key(key("enter")).expect("unfold group");
    let _ = render_str(engine);
    engine.handle_key(key("down")).expect("select actor");
    engine.handle_key(key("space")).expect("open inspector");
}

#[test]
fn tui_projects_generic_diagnostics_without_factory_specific_renderer() {
    let mut engine = Engine::new(120, 32).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    open_single_node(&mut engine, "third-party", "custom.node");
    let initial = render_snapshot(&mut engine);
    assert!(
        initial.contains("third-party · pending"),
        "local projection must render actor lifecycle: {initial}"
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "mag.diagnostic", "run_id": "preview-run", "from": "custom.node",
            "diagnostic": { "kind": "line", "text": "LIVE THIRD PARTY CONTENT" }
        }),
    );
    let frame = render_snapshot(&mut engine);
    assert!(
        frame.contains("LIVE THIRD PARTY CONTENT"),
        "open inspector must repaint live:\n{frame}"
    );
    let header_row = frame
        .lines()
        .position(|line| line.contains("node · custom.node"));
    let body_row = frame
        .lines()
        .position(|line| line.contains("LIVE THIRD PARTY CONTENT"));
    let footer_row = frame
        .lines()
        .position(|line| line.contains("Up/Down PgUp/PgDn"));
    assert!(
        matches!((header_row, body_row, footer_row), (Some(h), Some(b), Some(f)) if h < b && b < f),
        "inspector must keep header above its flex body and pin the footer below it:\n{frame}"
    );
    assert!(
        frame.contains("node · custom.node [read-only]"),
        "shared shell identity missing:\n{frame}"
    );
    assert!(
        engine.take_emit_queue().is_empty(),
        "inspector observation must not emit"
    );
}

#[test]
fn generic_arrivals_and_firings_materialize_tui_inputs_and_outputs() {
    let mut engine = Engine::new(100, 26).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    open_single_node(&mut engine, "endpoints", "endpoint.node");
    dispatch_event(
        &mut engine,
        json!({
            "kind": "mag.arrival", "run_id": "preview-run", "arrival_id": "arrival:input",
            "from": "source.node", "wire": "request", "semantic_type_id": "request",
            "value": "GENERIC INPUT OBSERVED"
        }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "mag.firing", "run_id": "preview-run", "id": "endpoint.node",
            "port": "input", "shape": "single",
            "arrivals": [{ "arrival_id": "arrival:input", "wire": "request" }]
        }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "mag.arrival", "run_id": "preview-run", "arrival_id": "arrival:output",
            "from": "endpoint.node", "wire": "response", "semantic_type_id": "response",
            "value": "GENERIC OUTPUT OBSERVED"
        }),
    );
    engine
        .handle_key(key("ctrl_o"))
        .expect("show projected endpoint details");
    let frame = render_snapshot(&mut engine);
    assert!(frame.contains("GENERIC INPUT OBSERVED"), "{frame}");
    assert!(frame.contains("GENERIC OUTPUT OBSERVED"), "{frame}");
}

#[test]
fn completed_local_projection_remains_inspectable_and_bounded_to_latest_run() {
    let mut engine = Engine::new(110, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    open_single_node(&mut engine, "retained", "retained.node");
    dispatch_event(
        &mut engine,
        json!({
            "kind": "mag.arrival", "run_id": "preview-run", "arrival_id": "arrival:result",
            "from": "retained.node", "wire": "result", "semantic_type_id": "result",
            "value": "RETAINED COMPLETED OUTPUT"
        }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "mag.run_complete", "run_id": "preview-run", "status": "success"
        }),
    );
    engine
        .handle_key(key("escape"))
        .expect("close live inspector");
    engine.advance_time(Duration::from_millis(2_001));
    let quiet = render_snapshot(&mut engine);
    assert!(
        !quiet.contains("MAG preview-demo"),
        "completed run must leave the active list:\n{quiet}"
    );
    assert!(
        quiet.contains("Space: inspect last completed run"),
        "bounded archive route missing:\n{quiet}"
    );
    engine.handle_key(key("tab")).expect("return to prompt");
    engine
        .handle_key(key("tab"))
        .expect("focus completed-run archive target");
    let focused = render_snapshot(&mut engine);
    assert!(
        !focused.contains("can't focus an empty sidebar"),
        "discoverable archive target must remain focusable:\n{focused}"
    );

    engine
        .handle_key(key("space"))
        .expect("open retained completed run");
    let concise = render_snapshot(&mut engine);
    assert!(concise.contains("run · preview-demo"), "{concise}");
    assert!(
        !concise.contains("RETAINED COMPLETED OUTPUT"),
        "details must remain progressive:\n{concise}"
    );
    engine
        .handle_key(key("ctrl_o"))
        .expect("reveal completed node details");
    let detailed = render_snapshot(&mut engine);
    assert!(detailed.contains("RETAINED COMPLETED OUTPUT"), "{detailed}");

    engine.handle_key(key("escape")).expect("close archive");
    dispatch_event(
        &mut engine,
        json!({
            "kind": "mag.run_started", "run_id": "newer-run", "run_name": "newer", "scope": "newer"
        }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "mag.run_complete", "run_id": "newer-run", "status": "success"
        }),
    );
    engine.advance_time(Duration::from_millis(2_001));
    engine.handle_key(key("down")).expect("dispatch prune");
    let state = engine.state_table().expect("state");
    let runs: mlua::Table = state.get("runs").expect("runs");
    assert!(runs
        .get::<Option<mlua::Table>>("preview-run")
        .unwrap()
        .is_none());
    assert!(runs
        .get::<Option<mlua::Table>>("newer-run")
        .unwrap()
        .is_some());
}

#[test]
fn consumer_projection_keeps_raw_tool_payloads_behind_details() {
    let mut engine = Engine::new(110, 72).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    dispatch_event(
        &mut engine,
        json!({ "kind": "tool.register", "tools": [
            { "name": "skill", "display": { "label": "Load skill", "primary": { "arg": "name" }, "result": { "kind": "receipt", "text": "skill loaded" } } },
            { "name": "list_dir", "display": { "label": "List directory", "primary": { "arg": "path" }, "result": { "kind": "content" } } },
            { "name": "run_tool", "display": { "label": "Run tool", "primary": { "arg": "name" }, "result": { "kind": "content" } } }
        ] }),
    );
    open_single_node(&mut engine, "transcriptish", "agent.node");
    dispatch_event(
        &mut engine,
        json!({
            "kind": "mock.completion.request", "request_id": "provider-1",
            "invocation": { "run_id": "preview-run", "actor_id": "agent.node" }
        }),
    );
    for (event, text) in [
        ("reasoning_delta", "reason "),
        ("reasoning_delta", "continued"),
        ("text_delta", "## Answer\ncomplete prose"),
    ] {
        dispatch_event(
            &mut engine,
            json!({
                "kind": "mock.completion.event", "request_id": "provider-1",
                "event": event, "text": text
            }),
        );
    }
    for (id, name, args, output, error) in [
        ("call-1", "skill", json!({ "name": ["dev"], "debug_wrapper": "CALL-WRAPPER-MUST-STAY-HIDDEN" }),
          Some("# Dev skill\nLARGE-SKILL-DOCUMENT-MUST-STAY-HIDDEN\nfull workflow body"), None),
        ("list-raw-id", "list_dir", json!({ "path": "/project/src", "debug_wrapper": "LIST-WRAPPER-MUST-STAY-HIDDEN" }),
          Some("(f) lib.rs\nLARGE-LIST-RESULT-MUST-STAY-HIDDEN\n"), None),
        ("error-raw-id", "run_tool", json!({ "name": "deploy", "secret": "ERROR-CALL-MUST-STAY-HIDDEN" }),
          None, Some("permission denied: concise cause. XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX-HUGE-ERROR-DETAIL-MUST-STAY-HIDDEN")),
    ] {
        dispatch_event(&mut engine, json!({
            "kind": "tool-gate.tool.invoke", "id": id, "name": name, "args": args,
            "invocation": { "run_id": "preview-run", "actor_id": "agent.node" }
        }));
        dispatch_event(&mut engine, json!({
            "kind": "tool.result", "id": id, "output": output, "error": error
        }));
    }
    let frame = render_snapshot(&mut engine);
    for expected in [
        "▸ reasoning",
        "Answer",
        "complete prose",
        "▸ Load skill · dev",
        "✓ skill · completed",
        "▸ List directory · /project/src",
        "✓ list_dir · completed",
        "▸ Run tool · deploy",
        "✗ run_tool · failed · permission denied: concise cause.",
        "hidden",
    ] {
        assert!(
            frame.contains(expected),
            "concise transcript lost {expected:?}:\n{frame}"
        );
    }
    for hidden in [
        "reason continued",
        "call-1",
        "list-raw-id",
        "error-raw-id",
        "CALL-WRAPPER-MUST-STAY-HIDDEN",
        "LIST-WRAPPER-MUST-STAY-HIDDEN",
        "ERROR-CALL-MUST-STAY-HIDDEN",
        "LARGE-SKILL-DOCUMENT-MUST-STAY-HIDDEN",
        "LARGE-LIST-RESULT-MUST-STAY-HIDDEN",
        "HUGE-ERROR-DETAIL-MUST-STAY-HIDDEN",
        "full workflow body",
    ] {
        assert!(
            !frame.contains(hidden),
            "default transcript leaked verbose detail {hidden:?}:\n{frame}"
        );
    }

    engine
        .handle_key(key("ctrl_o"))
        .expect("reveal preview details");
    let expanded = render_snapshot(&mut engine);
    for expected in [
        "reason continued",
        "call-1",
        "list-raw-id",
        "error-raw-id",
        "CALL-WRAPPER-MUST-STAY-HIDDEN",
        "LIST-WRAPPER-MUST-STAY-HIDDEN",
        "ERROR-CALL-MUST-STAY-HIDDEN",
        "LARGE-SKILL-DOCUMENT-MUST-STAY-HIDDEN",
        "LARGE-LIST-RESULT-MUST-STAY-HIDDEN",
        "HUGE-ERROR-DETAIL-MUST-STAY-HIDDEN",
        "full workflow body",
    ] {
        assert!(
            expanded.contains(expected),
            "expanded transcript lost {expected:?}:\n{expanded}"
        );
    }

    engine
        .handle_key(key("ctrl_o"))
        .expect("collapse node details");
    engine
        .handle_key(key("escape"))
        .expect("close node inspector");
    engine.handle_key(key("up")).expect("select group");
    engine
        .handle_key(key("space"))
        .expect("open group inspector");
    let group = render_snapshot(&mut engine);
    for expected in [
        "chronological activity",
        "▸ Load skill · dev",
        "✓ skill · completed",
        "▸ List directory · /project/src",
        "✓ list_dir · completed",
        "▸ Run tool · deploy",
        "✗ run_tool · failed · permission denied: concise cause.",
    ] {
        assert!(
            group.contains(expected),
            "group preview lost {expected:?}:\n{group}"
        );
    }
    for hidden in [
        "call-1",
        "list-raw-id",
        "error-raw-id",
        "LARGE-SKILL-DOCUMENT-MUST-STAY-HIDDEN",
        "LARGE-LIST-RESULT-MUST-STAY-HIDDEN",
        "HUGE-ERROR-DETAIL-MUST-STAY-HIDDEN",
    ] {
        assert!(
            !group.contains(hidden),
            "default group preview leaked {hidden:?}:\n{group}"
        );
    }
    engine
        .handle_key(key("ctrl_o"))
        .expect("expand group details");
    engine.handle_key(key("end")).expect("scroll group details");
    let detailed_group = render_snapshot(&mut engine);
    for expected in [
        "error-raw-id",
        "ERROR-CALL-MUST-STAY-HIDDEN",
        "HUGE-ERROR-DETAIL-MUST-STAY-HIDDEN",
    ] {
        assert!(
            detailed_group.contains(expected),
            "detailed group preview lost {expected:?}:\n{detailed_group}"
        );
    }
}

#[test]
fn generic_validation_diagnostic_never_renders_rejected_candidate_json() {
    let mut engine = Engine::new(110, 32).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    open_single_node(&mut engine, "structured-output", "typed.node");
    dispatch_event(
        &mut engine,
        json!({
            "kind": "mag.diagnostic", "run_id": "preview-run", "from": "typed.node",
            "diagnostic": {
                "attempt": 1,
                "kind": "validation",
                "violations": [{ "path": "$.content", "message": "required" }]
            }
        }),
    );

    let concise = render_snapshot(&mut engine);
    assert!(
        concise.contains("validation attempt 1 · details hidden"),
        "{concise}"
    );
    assert!(
        !concise.contains("REJECTED-CANDIDATE-MUST-NEVER-RENDER"),
        "normal preview leaked rejected JSON:\n{concise}"
    );

    engine
        .handle_key(key("ctrl_o"))
        .expect("expand validation details");
    let expanded = render_snapshot(&mut engine);
    assert!(expanded.contains("violations:"), "{expanded}");
    assert!(expanded.contains("required"), "{expanded}");
    assert!(
        !expanded.contains("REJECTED-CANDIDATE-MUST-NEVER-RENDER"),
        "expanded preview leaked rejected JSON:\n{expanded}"
    );
}

#[test]
fn tui_terminal_projection_preserves_exact_large_multiline_tool_streams() {
    for width in [58, 150] {
        let mut engine = Engine::new(width, 40).expect("engine");
        engine.load_scenario(&chat_lua_source()).expect("load");
        dispatch_event(
            &mut engine,
            json!({
                "kind": "mag.run_started", "run_id": "preview-run", "run_name": "preview-demo", "scope": "rp"
            }),
        );
        dispatch_event(
            &mut engine,
            json!({
                "kind": "mag.actor_spawned", "run_id": "preview-run", "id": "shell.node", "factory": "terminalish",
                "spec": { "params": { "command": "printf 'EXACT COMMAND WITH SPACES'" } }
            }),
        );
        dispatch_event(
            &mut engine,
            json!({
                "kind": "mock.completion.request", "request_id": "terminal-cap",
                "invocation": { "run_id": "preview-run", "actor_id": "shell.node" }
            }),
        );
        for (stream, text) in [
            ("stdout", "ALPHA-LINE\n"),
            ("stderr", "ERROR-LINE\n"),
            (
                "stdout",
                "OMEGA-CONTENT-THAT-MUST-NOT-BE-CLIPPED-0123456789",
            ),
        ] {
            dispatch_event(
                &mut engine,
                json!({
                    "kind": "tool.stream", "id": "terminal-cap", "stream": stream, "text": text
                }),
            );
        }
        engine.handle_key(key("tab")).unwrap();
        let _ = render_str(&mut engine);
        engine.handle_key(key("down")).unwrap();
        engine.handle_key(key("enter")).unwrap();
        let _ = render_str(&mut engine);
        engine.handle_key(key("down")).unwrap();
        engine.handle_key(key("space")).unwrap();
        engine.handle_key(key("ctrl_o")).unwrap();
        let _ = render_str(&mut engine);
        let frame = engine
            .snapshot()
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '│')
            .collect::<String>();
        for expected in [
            "printf'EXACTCOMMANDWITHSPACES'",
            "ALPHA-LINE",
            "ERROR-LINE",
            "OMEGA-CONTENT-THAT-MUST-NOT-BE-CLIPPED-0123456789",
        ] {
            assert!(
                frame.contains(expected),
                "width {width} lost {expected}:\n{frame}"
            );
        }
    }
}

#[test]
fn appended_consumer_projection_does_not_steal_manual_scroll_position() {
    let mut engine = Engine::new(100, 26).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    open_single_node(&mut engine, "scrolling", "scroll.node");
    dispatch_event(
        &mut engine,
        json!({
            "kind": "tool-gate.tool.invoke", "id": "scroll-cap", "name": "stream",
            "args": {}, "invocation": { "run_id": "preview-run", "actor_id": "scroll.node" }
        }),
    );
    for seq in 1..=50 {
        dispatch_event(
            &mut engine,
            json!({
                "kind": "tool.stream", "id": "scroll-cap", "stream": "stdout",
                "text": format!("line {seq:02}")
            }),
        );
    }
    let _ = render_str(&mut engine);
    engine.handle_key(key("home")).expect("scroll to top");
    let _ = render_str(&mut engine);
    let before: i64 = engine
        .lua()
        .load(r#"return tui.scroll_position("popup_node_inspector").offset"#)
        .eval()
        .expect("offset before append");

    dispatch_event(
        &mut engine,
        json!({
            "kind": "tool.stream", "id": "scroll-cap", "stream": "stdout",
            "text": "new tail output"
        }),
    );
    let _ = render_str(&mut engine);
    let after: i64 = engine
        .lua()
        .load(r#"return tui.scroll_position("popup_node_inspector").offset"#)
        .eval()
        .expect("offset after append");
    assert_eq!(
        after, before,
        "appended output must preserve ordinary reading position"
    );
    let state = engine.state_table().expect("state");
    let popup: mlua::Table = state.get("popup").expect("popup");
    assert!(
        popup.get::<bool>("unseen").unwrap_or(false),
        "append away from the tail should mark unseen output"
    );
}

#[test]
fn node_inspector_is_read_only_and_escape_restores_sidebar_focus() {
    let mut engine = Engine::new(100, 26).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    open_single_node(&mut engine, "readonly", "readonly.node");
    let _ = render_str(&mut engine);
    let before = engine.take_emit_queue();
    engine.handle_key(key("x")).expect("ordinary key inert");
    engine.handle_key(key("enter")).expect("enter inert");
    engine.handle_key(key("pageup")).expect("scroll");
    engine.handle_key(key("pagedown")).expect("scroll");
    engine.handle_key(key("home")).expect("scroll top");
    engine.handle_key(key("end")).expect("follow tail");
    assert_eq!(
        engine.take_emit_queue(),
        before,
        "inspector keys must not reach graph or prompt"
    );
    engine.handle_key(key("escape")).expect("close");
    let frame = render_snapshot(&mut engine);
    assert!(
        !frame.contains("node · readonly.node"),
        "escape must close inspector"
    );
    assert!(
        frame.contains("Workflows · focused"),
        "sidebar focus must be restored:\n{frame}"
    );
}

#[test]
fn agent_group_inspector_shows_its_initial_assignment() {
    let mut engine = Engine::new(120, 32).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_started", "run_id": "assignment-run", "run_name": "delegated" }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "mag.actor_spawned", "run_id": "assignment-run",
            "id": "worker.entry", "factory": "entry", "spec": {}
        }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "mag.actor_spawned", "run_id": "assignment-run",
            "id": "worker.llm", "factory": "llm",
            "spec": { "params": { "system": "base instructions\n\n---\n\nImplement the bounded fix." } }
        }),
    );

    engine.handle_key(key("tab")).expect("focus sidebar");
    let _ = render_str(&mut engine);
    engine.handle_key(key("down")).expect("select agent group");
    engine
        .handle_key(key("space"))
        .expect("open group inspector");
    let snapshot = render_snapshot(&mut engine);
    assert!(
        snapshot.contains("Initial assignment") && snapshot.contains("Implement the bounded fix."),
        "agent inspector must expose the task delegated by its lead:\n{snapshot}"
    );
}

#[test]
fn agent_previews_hide_tool_streams_but_keep_conversation_activity() {
    let mut engine = Engine::new(120, 36).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_started", "run_id": "agent-run", "run_name": "delegated" }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "mag.actor_spawned", "run_id": "agent-run",
            "id": "worker.llm", "factory": "llm",
            "spec": { "params": { "system": "base instructions\n\n---\n\nInspect the repository." } }
        }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "tool-gate.tool.invoke", "id": "shell-call", "name": "shell.script", "args": {},
            "invocation": { "run_id": "agent-run", "actor_id": "worker.llm" }
        }),
    );
    for (stream, text) in [
        (
            "stdout",
            "/project/src/main.rs\nrefs/heads/worktree-popup-stdout\n",
        ),
        ("stderr", "agent-preview-stderr-must-stay-hidden"),
    ] {
        dispatch_event(
            &mut engine,
            json!({ "kind": "tool.stream", "id": "shell-call", "stream": stream, "text": text }),
        );
    }
    dispatch_event(
        &mut engine,
        json!({
            "kind": "mock.completion.request", "request_id": "provider-call",
            "invocation": { "run_id": "agent-run", "actor_id": "worker.llm" }
        }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "mock.completion.event", "request_id": "provider-call",
            "event": "text_delta", "text": "Useful agent summary"
        }),
    );

    engine.handle_key(key("tab")).expect("focus sidebar");
    let _ = render_str(&mut engine);
    engine.handle_key(key("down")).expect("select agent group");
    engine
        .handle_key(key("space"))
        .expect("open agent inspector");
    let snapshot = render_snapshot(&mut engine);
    for expected in [
        "Initial assignment",
        "Inspect the repository.",
        "Useful agent summary",
    ] {
        assert!(
            snapshot.contains(expected),
            "agent group preview lost {expected:?}:\n{snapshot}"
        );
    }
    for hidden in [
        "stdout",
        "/project/src/main.rs",
        "refs/heads/worktree-popup-stdout",
        "stderr",
        "agent-preview-stderr-must-stay-hidden",
    ] {
        assert!(
            !snapshot.contains(hidden),
            "agent group preview leaked tool stream content {hidden:?}:\n{snapshot}"
        );
    }

    engine
        .handle_key(key("escape"))
        .expect("close group inspector");
    engine.handle_key(key("enter")).expect("unfold agent group");
    let _ = render_str(&mut engine);
    engine.handle_key(key("down")).expect("select agent node");
    engine
        .handle_key(key("space"))
        .expect("open agent node inspector");
    let node_snapshot = render_snapshot(&mut engine);
    assert!(
        node_snapshot.contains("Useful agent summary"),
        "agent node preview lost conversation activity:\n{node_snapshot}"
    );
    for hidden in [
        "/project/src/main.rs",
        "refs/heads/worktree-popup-stdout",
        "agent-preview-stderr-must-stay-hidden",
    ] {
        assert!(
            !node_snapshot.contains(hidden),
            "agent node preview leaked tool stream content {hidden:?}:\n{node_snapshot}"
        );
    }
}

#[test]
fn compact_duration_formatter_covers_unit_boundaries() {
    let mut engine = Engine::new(80, 24).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");

    for (milliseconds, expected) in [
        (0_u64, "0ms"),
        (999, "999ms"),
        (1_000, "1s"),
        (59_999, "59s"),
        (60_000, "1m"),
        (61_000, "1m1s"),
        (3_600_000, "1h"),
        (18_615_000, "5h10m15s"),
        (86_400_000, "1d"),
        (176_461_000, "2d1h1m1s"),
    ] {
        let actual: String = engine
            .lua()
            .load(format!(
                "return require('libs.chat.common').humanize_duration_ms({milliseconds})"
            ))
            .eval()
            .expect("format duration");
        assert_eq!(actual, expected, "{milliseconds}ms");
    }
}

#[test]
fn running_and_completed_workflow_rows_use_compact_durations() {
    let mut engine = Engine::new(120, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.run_started", "run_id": "long-1", "run_name": "long-work" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_spawned", "run_id": "long-1", "id": "worker.llm", "factory": "llm" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_ready", "run_id": "long-1", "id": "worker.llm" }),
    );
    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_busy", "run_id": "long-1", "id": "worker.llm" }),
    );

    engine.advance_time(Duration::from_millis(310_000));
    let _ = render_str(&mut engine);
    let running = engine.snapshot();
    assert!(
        running.contains("● worker 5m10s"),
        "running workflow row must stay compact:\n{running}"
    );

    dispatch_event(
        &mut engine,
        json!({ "kind": "mag.actor_idle", "run_id": "long-1", "id": "worker.llm" }),
    );
    let _ = render_str(&mut engine);
    let completed = engine.snapshot();
    assert!(
        completed.contains("✓ worker 5m10s"),
        "completed workflow row must freeze the same compact duration:\n{completed}"
    );
}

#[test]
fn assistant_and_workflow_result_footers_share_compact_durations() {
    let mut engine = Engine::new(120, 30).expect("engine");
    engine.load_scenario(&chat_lua_source()).expect("load");
    let _ = render_str(&mut engine);
    fixture_assistant_completed(
        &mut engine,
        Some(("long answer").into()),
        json!({ "model": "test", "duration_ms": 18_615_000 }),
    );
    dispatch_event(
        &mut engine,
        json!({
            "kind": "chat.graph_result.append", "run_id": "long-result",
            "run_name": "long-result", "status": "success", "nodes": [],
            "duration_ms": 18_615_000, "output": "done"
        }),
    );

    let _ = render_str(&mut engine);
    let snapshot = engine.snapshot();
    assert_eq!(
        snapshot.matches("5h10m15s").count(),
        2,
        "assistant and workflow result footers must share the formatter:\n{snapshot}"
    );
}
