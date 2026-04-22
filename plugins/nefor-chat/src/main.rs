//! nefor-chat — chat UI plugin for nefor.
//!
//! Bridges [mock-plugin](../mock-plugin/) (Claude Code wrapper) and
//! [nefor-tui](../nefor-tui/) (cell-grid renderer) over NCP v0.1.
//! Owns chat-layer state — transcript, input buffer, scroll offset — and
//! translates both sides into grid mutations published on the bus.
//!
//! The plugin never touches the terminal directly; every rendering concern
//! is expressed as a `nefor-tui.*` event.

mod error;
mod ncp;
mod render;
mod state;
mod wrap;

use nefor_protocol::{Body, Envelope, PluginOutgoing, SystemBody};
use serde_json::{Map, Value};
use tokio::sync::mpsc;

use crate::error::ChatError;
use crate::state::{ChatState, Role};

/// Plugin version for the `nefor-chat.hello` self-description event.
pub const PLUGIN_VERSION: &str = "0.1.0";
/// NCP version this plugin speaks.
pub const PROTOCOL_VERSION: &str = "0.1";

#[tokio::main]
async fn main() {
    init_tracing();
    if let Err(e) = run().await {
        tracing::error!(error = %e, "nefor-chat exited with error");
        eprintln!("nefor-chat: {e}");
        std::process::exit(1);
    }
    // Force exit: `tokio::io::stdin()` parks a non-cancellable blocking
    // reader thread; letting the runtime drop naturally would hang the
    // process after `run()` returns, keeping the engine's `child.wait()`
    // pending. Same fix as nefor-tui.
    std::process::exit(0);
}

fn init_tracing() {
    // Logs go to stderr so they never pollute the NCP stream on stdout.
    // `RUST_LOG=info` is the default; users can raise to `debug` for
    // deep inspection.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
}

async fn run() -> Result<(), ChatError> {
    let (out_tx, _writer) = ncp::spawn_stdout_writer();
    let (in_tx, mut in_rx) = mpsc::channel::<Result<Envelope, ChatError>>(ncp::CHANNEL_CAP);
    let _reader = ncp::spawn_stdin_reader(in_tx);

    // 1. Handshake.
    send_ready(&out_tx).await?;
    let engine_version = ncp::await_ready_ok(&mut in_rx).await?;
    tracing::info!(engine_version = %engine_version, "ready");

    // 2. Self-advertise. Purely informational per docs/plugin-authoring.md.
    send_event(&out_tx, hello_body()).await?;

    // 3. Main loop. We do *not* emit palette or render until we've seen
    //    `nefor-tui.ready` — nefor-tui won't have set up its grid yet
    //    otherwise. The first tui-ready triggers palette emit + initial
    //    render; every subsequent state change triggers a re-render
    //    (full-redraw strategy).
    let mut state = ChatState::new();
    let mut palette_emitted = false;

    loop {
        let maybe = in_rx.recv().await;
        let env = match maybe {
            Some(Ok(env)) => env,
            Some(Err(e)) => {
                tracing::warn!(error = %e, "stdin parse error; dropping line");
                continue;
            }
            None => {
                tracing::info!("stdin closed; exiting");
                break;
            }
        };

        match handle_envelope(env, &mut state) {
            Action::Shutdown => break,
            Action::Continue => {}
            Action::Render => {
                if state.tui_ready {
                    if !palette_emitted {
                        emit_palette(&out_tx).await?;
                        palette_emitted = true;
                    }
                    emit_render(&out_tx, &state).await?;
                }
            }
            Action::SubmitPrompt(text) => {
                // Slash-commands are handled locally instead of being sent
                // as a prompt. For `/resume` we don't push a confirmation
                // entry here — mock-plugin responds with `cc.history.replay`
                // which clears the transcript and populates it with the
                // stored conversation. Regular text follows the normal
                // cc.prompt path.
                match parse_command(&text) {
                    Some(Command::ResumeRecent) => {
                        send_event(&out_tx, cc_resume_body(None)).await?;
                    }
                    Some(Command::ResumeSpecific(id)) => {
                        send_event(&out_tx, cc_resume_body(Some(&id))).await?;
                    }
                    None => {
                        // Register the user turn locally before shipping the
                        // `cc.prompt` — keeps the transcript and the outgoing
                        // event in the same logical beat.
                        state.push_entry(Role::User, text.clone());
                        state.begin_turn();
                        send_event(&out_tx, cc_prompt_body(&text)).await?;
                    }
                }
                if state.tui_ready {
                    if !palette_emitted {
                        emit_palette(&out_tx).await?;
                        palette_emitted = true;
                    }
                    emit_render(&out_tx, &state).await?;
                }
            }
        }
    }

    // Best-effort farewell. Failure here doesn't matter — the engine
    // already considers stdout-close the liveness signal.
    let _ = send_event(&out_tx, goodbye_body()).await;
    Ok(())
}

/// The action the main loop takes for each incoming envelope.
#[derive(Debug)]
enum Action {
    /// Nothing to do — keep reading.
    Continue,
    /// State changed in a way that demands a redraw.
    Render,
    /// The user hit Enter — flush the buffer as a `cc.prompt` and render.
    SubmitPrompt(String),
    /// Engine signalled shutdown — exit cleanly.
    Shutdown,
}

/// Mutate `state` for the given envelope and classify the follow-up action.
fn handle_envelope(env: Envelope, state: &mut ChatState) -> Action {
    match env.body {
        Body::System(SystemBody::Shutdown { .. }) => Action::Shutdown,
        Body::System(_) => Action::Continue,
        Body::Event(map) => handle_event(&map, state),
    }
}

fn handle_event(map: &Map<String, Value>, state: &mut ChatState) -> Action {
    let Some(kind) = map.get("kind").and_then(Value::as_str) else {
        return Action::Continue;
    };
    match kind {
        // ---- nefor-tui input path -----------------------------------
        "nefor-tui.ready" => {
            state.tui_ready = true;
            if let (Some(c), Some(r)) = (as_u32(map, "cols"), as_u32(map, "rows")) {
                state.dims.cols = c;
                state.dims.rows = r;
            }
            Action::Render
        }
        "nefor-tui.input.resize" => {
            if let (Some(c), Some(r)) = (as_u32(map, "cols"), as_u32(map, "rows")) {
                state.dims.cols = c;
                state.dims.rows = r;
            }
            Action::Render
        }
        "nefor-tui.input.key" => handle_key(map, state),
        "nefor-tui.input.paste" => {
            if let Some(text) = map.get("text").and_then(Value::as_str) {
                state.input.insert_str(text);
                Action::Render
            } else {
                Action::Continue
            }
        }
        "nefor-tui.input.mouse" => handle_mouse(map, state),
        // ---- mock-plugin output path ---------------------------------
        "cc.stream.delta" => {
            if let Some(t) = map.get("text").and_then(Value::as_str) {
                state.append_assistant_delta(t);
                Action::Render
            } else {
                Action::Continue
            }
        }
        "cc.stream.end" => {
            let authoritative = map
                .get("text")
                .and_then(Value::as_str)
                .map(|s| s.to_owned());
            state.finalize_assistant(authoritative);
            state.end_turn();
            // Persistent "done" indicator in the transcript. Prefer the
            // cost/timing info when mock-plugin supplies it; fall back to a
            // bare marker otherwise.
            let marker = done_marker(map);
            state.push_entry(Role::System, marker);
            Action::Render
        }
        "cc.tool.start" => {
            if let Some(name) = map.get("name").and_then(Value::as_str) {
                state.push_entry(Role::System, format!("tool: {name}"));
                Action::Render
            } else {
                Action::Continue
            }
        }
        "cc.turn.error" => {
            state.end_turn();
            if let Some(msg) = map.get("message").and_then(Value::as_str) {
                state.push_entry(Role::System, format!("error: {msg}"));
                Action::Render
            } else {
                Action::Continue
            }
        }
        "cc.history.replay" => {
            // Replace the transcript with stored-on-disk history from a
            // previous session. mock-plugin guarantees `entries` is already
            // in chronological order.
            state.transcript.clear();
            let mut count = 0usize;
            if let Some(arr) = map.get("entries").and_then(Value::as_array) {
                for e in arr {
                    let Some(role_str) = e.get("role").and_then(Value::as_str) else {
                        continue;
                    };
                    let role = match role_str {
                        "user" => Role::User,
                        "assistant" => Role::Assistant,
                        _ => continue,
                    };
                    let text = e
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned();
                    state.push_entry(role, text);
                    count += 1;
                }
            }
            let session = map.get("session_id").and_then(Value::as_str).unwrap_or("?");
            state.push_entry(
                Role::System,
                format!("resumed · {count} messages · session {session}"),
            );
            Action::Render
        }
        "cc.hello" | "cc.ready" | "cc.goodbye" => {
            tracing::debug!(kind, "mock-plugin lifecycle event (no-op on UI)");
            Action::Continue
        }
        _ => Action::Continue,
    }
}

fn handle_key(map: &Map<String, Value>, state: &mut ChatState) -> Action {
    let Some(key) = map.get("key").and_then(Value::as_str) else {
        return Action::Continue;
    };
    // Modifiers are currently only used to suppress input when the user
    // is holding Ctrl (so Ctrl+C / Ctrl+D don't leak into the buffer).
    let has_ctrl = map
        .get("modifiers")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().any(|v| v.as_str() == Some("ctrl")))
        .unwrap_or(false);

    match key {
        "enter" => {
            if state.input.len() == 0 {
                return Action::Continue;
            }
            let text = state.input.as_string();
            state.input.clear();
            Action::SubmitPrompt(text)
        }
        "backspace" => {
            state.input.backspace();
            Action::Render
        }
        "left" => {
            state.input.cursor_left();
            Action::Render
        }
        "right" => {
            state.input.cursor_right();
            Action::Render
        }
        "home" => {
            state.input.cursor_home();
            Action::Render
        }
        "end" => {
            state.input.cursor_end();
            Action::Render
        }
        "pageup" => {
            // Page-size: half the transcript area, minimum 1.
            let page = page_size(state);
            state.scroll_up(page);
            Action::Render
        }
        "pagedown" => {
            let page = page_size(state);
            state.scroll_down(page);
            Action::Render
        }
        "escape" => {
            // v1: no-op on UI. Future: emit cc.interrupt.
            Action::Continue
        }
        // Printable keys. nefor-tui forwards single-char key strings for
        // regular typing ("a", "A", "!", " ", "漢"). We accept anything
        // whose grapheme count is exactly one and that isn't a named
        // multi-char key above.
        _ => {
            if has_ctrl {
                return Action::Continue;
            }
            if key.chars().count() == 1 {
                if let Some(c) = key.chars().next() {
                    // Skip control chars.
                    if !c.is_control() {
                        state.input.insert_char(c);
                        return Action::Render;
                    }
                }
            }
            Action::Continue
        }
    }
}

fn page_size(state: &ChatState) -> u32 {
    // Transcript area height is rows-1 (last row is the input).
    let transcript = state.dims.rows.saturating_sub(1);
    (transcript / 2).max(1)
}

/// Rows the mouse wheel moves per scroll notch. Three is the usual
/// terminal-convention tick — one wheel click feels like "a bit" but
/// doesn't blow past the screen.
const WHEEL_ROWS_PER_NOTCH: u32 = 3;

/// Handle a `nefor-tui.input.mouse` envelope. For v1 we only react to
/// wheel-scroll actions (up/down); clicks and drags are ignored.
fn handle_mouse(map: &Map<String, Value>, state: &mut ChatState) -> Action {
    match map.get("action").and_then(Value::as_str) {
        Some("scroll_up") => {
            state.scroll_up(WHEEL_ROWS_PER_NOTCH);
            Action::Render
        }
        Some("scroll_down") => {
            state.scroll_down(WHEEL_ROWS_PER_NOTCH);
            Action::Render
        }
        _ => Action::Continue,
    }
}

/// Build the persistent "done" line appended after a turn finishes. Picks
/// the most informative subset from the `cc.stream.end` payload that is
/// present — `duration_ms` is strongly preferred since it answers the
/// "was that fast or slow?" question directly.
fn done_marker(map: &Map<String, Value>) -> String {
    let duration = map.get("duration_ms").and_then(Value::as_u64);
    let num_turns = map.get("num_turns").and_then(Value::as_u64);
    match (duration, num_turns) {
        (Some(ms), Some(n)) if n > 1 => format!("done in {ms}ms / {n} turns"),
        (Some(ms), _) => format!("done in {ms}ms"),
        (None, _) => "done".into(),
    }
}

fn as_u32(map: &Map<String, Value>, key: &str) -> Option<u32> {
    map.get(key)
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
}

async fn emit_palette(out_tx: &mpsc::Sender<PluginOutgoing>) -> Result<(), ChatError> {
    for def in render::palette_defines() {
        send_event(out_tx, def).await?;
    }
    Ok(())
}

async fn emit_render(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    state: &ChatState,
) -> Result<(), ChatError> {
    for body in render::render_frame(state) {
        send_event(out_tx, body).await?;
    }
    Ok(())
}

async fn send_ready(out_tx: &mpsc::Sender<PluginOutgoing>) -> Result<(), ChatError> {
    out_tx
        .send(PluginOutgoing::system(SystemBody::Ready {
            protocol_version: PROTOCOL_VERSION.into(),
        }))
        .await
        .map_err(|_| ChatError::WriterClosed)
}

async fn send_event(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: Map<String, Value>,
) -> Result<(), ChatError> {
    out_tx
        .send(PluginOutgoing::event(body))
        .await
        .map_err(|_| ChatError::WriterClosed)
}

fn hello_body() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("nefor-chat.hello".into()));
    m.insert("version".into(), Value::String(PLUGIN_VERSION.into()));
    m
}

fn goodbye_body() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("nefor-chat.goodbye".into()));
    m.insert("reason".into(), Value::String("stream closed".into()));
    m
}

fn cc_prompt_body(text: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("cc.prompt".into()));
    m.insert("text".into(), Value::String(text.to_owned()));
    m
}

fn cc_resume_body(session_id: Option<&str>) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("cc.resume".into()));
    if let Some(id) = session_id {
        m.insert("session_id".into(), Value::String(id.to_owned()));
    }
    m
}

/// Parse a slash-command submitted via the input line. Returns `None` if
/// the text isn't a recognised command, so the caller falls through to the
/// usual `cc.prompt` path.
///
/// Grammar is deliberately minimal: `/resume` and `/resume <session-id>`.
/// Everything else is treated as a regular prompt — we don't surface an
/// error on unknown slash commands because Claude prompts often start with
/// a slash for role instruction (e.g. "/think step by step").
fn parse_command(text: &str) -> Option<Command> {
    let trimmed = text.trim();
    let rest = trimmed.strip_prefix("/resume")?;
    let rest = rest.trim();
    if rest.is_empty() {
        Some(Command::ResumeRecent)
    } else {
        // Accept everything after /resume as the session-id; mock-plugin
        // will validate and surface a turn_error if it's not a UUID.
        Some(Command::ResumeSpecific(rest.to_owned()))
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    ResumeRecent,
    ResumeSpecific(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use nefor_protocol::{PluginName, Timestamp};
    use serde_json::json;

    fn ts() -> Timestamp {
        Timestamp::parse("2026-04-21T00:00:00.000Z").expect("valid")
    }

    fn event_env(body: Value) -> Envelope {
        let Value::Object(map) = body else {
            panic!("body must be an object");
        };
        Envelope::event(PluginName::new("engine-mock").expect("valid"), ts(), map)
    }

    #[test]
    fn tui_ready_sets_dims_and_flags() {
        let mut s = ChatState::new();
        let env = event_env(json!({
            "kind": "nefor-tui.ready",
            "cols": 100,
            "rows": 30,
        }));
        let a = handle_envelope(env, &mut s);
        assert!(matches!(a, Action::Render));
        assert!(s.tui_ready);
        assert_eq!(s.dims.cols, 100);
        assert_eq!(s.dims.rows, 30);
    }

    #[test]
    fn resize_updates_dims() {
        let mut s = ChatState::new();
        let env = event_env(json!({
            "kind": "nefor-tui.input.resize",
            "cols": 55,
            "rows": 12,
        }));
        handle_envelope(env, &mut s);
        assert_eq!(s.dims.cols, 55);
        assert_eq!(s.dims.rows, 12);
    }

    #[test]
    fn printable_key_appends_to_buffer() {
        let mut s = ChatState::new();
        let env = event_env(json!({
            "kind": "nefor-tui.input.key",
            "key": "a",
            "modifiers": [],
        }));
        let a = handle_envelope(env, &mut s);
        assert!(matches!(a, Action::Render));
        assert_eq!(s.input.as_string(), "a");
    }

    #[test]
    fn ctrl_modifier_blocks_printable_key() {
        let mut s = ChatState::new();
        let env = event_env(json!({
            "kind": "nefor-tui.input.key",
            "key": "c",
            "modifiers": ["ctrl"],
        }));
        let a = handle_envelope(env, &mut s);
        assert!(matches!(a, Action::Continue));
        assert_eq!(s.input.as_string(), "");
    }

    #[test]
    fn enter_submits_prompt_and_clears_buffer() {
        let mut s = ChatState::new();
        s.input.insert_str("hi claude");
        let env = event_env(json!({
            "kind": "nefor-tui.input.key",
            "key": "enter",
            "modifiers": [],
        }));
        let a = handle_envelope(env, &mut s);
        match a {
            Action::SubmitPrompt(t) => assert_eq!(t, "hi claude"),
            other => panic!("expected SubmitPrompt, got {other:?}"),
        }
        assert_eq!(s.input.as_string(), "");
    }

    #[test]
    fn enter_on_empty_buffer_is_continue() {
        let mut s = ChatState::new();
        let env = event_env(json!({
            "kind": "nefor-tui.input.key",
            "key": "enter",
            "modifiers": [],
        }));
        let a = handle_envelope(env, &mut s);
        assert!(matches!(a, Action::Continue));
    }

    #[test]
    fn backspace_removes_last_char() {
        let mut s = ChatState::new();
        s.input.insert_str("abc");
        let env = event_env(json!({
            "kind": "nefor-tui.input.key",
            "key": "backspace",
            "modifiers": [],
        }));
        handle_envelope(env, &mut s);
        assert_eq!(s.input.as_string(), "ab");
    }

    #[test]
    fn paste_event_inserts_text() {
        let mut s = ChatState::new();
        let env = event_env(json!({
            "kind": "nefor-tui.input.paste",
            "text": "pasted",
        }));
        handle_envelope(env, &mut s);
        assert_eq!(s.input.as_string(), "pasted");
    }

    #[test]
    fn cc_stream_delta_appends_assistant() {
        let mut s = ChatState::new();
        let env = event_env(json!({
            "kind": "cc.stream.delta",
            "text": "hello",
        }));
        handle_envelope(env, &mut s);
        assert_eq!(s.transcript.len(), 1);
        assert_eq!(s.transcript[0].role, Role::Assistant);
        assert_eq!(s.transcript[0].text, "hello");
        assert!(s.transcript[0].streaming);
    }

    #[test]
    fn cc_stream_end_finalizes_assistant() {
        let mut s = ChatState::new();
        handle_envelope(
            event_env(json!({"kind":"cc.stream.delta","text":"partial"})),
            &mut s,
        );
        handle_envelope(
            event_env(json!({"kind":"cc.stream.end","text":"FINAL"})),
            &mut s,
        );
        assert_eq!(s.transcript[0].text, "FINAL");
        assert!(!s.transcript[0].streaming);
    }

    #[test]
    fn cc_stream_end_without_text_keeps_deltas() {
        let mut s = ChatState::new();
        handle_envelope(
            event_env(json!({"kind":"cc.stream.delta","text":"keep"})),
            &mut s,
        );
        handle_envelope(event_env(json!({"kind":"cc.stream.end"})), &mut s);
        assert_eq!(s.transcript[0].text, "keep");
        assert!(!s.transcript[0].streaming);
    }

    #[test]
    fn cc_tool_start_appends_system_entry() {
        let mut s = ChatState::new();
        handle_envelope(
            event_env(json!({"kind":"cc.tool.start","name":"read"})),
            &mut s,
        );
        assert_eq!(s.transcript[0].role, Role::System);
        assert_eq!(s.transcript[0].text, "tool: read");
    }

    #[test]
    fn cc_turn_error_appends_system_entry() {
        let mut s = ChatState::new();
        handle_envelope(
            event_env(json!({"kind":"cc.turn.error","message":"boom"})),
            &mut s,
        );
        assert_eq!(s.transcript[0].role, Role::System);
        assert_eq!(s.transcript[0].text, "error: boom");
    }

    #[test]
    fn cc_hello_and_ready_are_noop_on_ui() {
        let mut s = ChatState::new();
        let a = handle_envelope(event_env(json!({"kind":"cc.hello"})), &mut s);
        assert!(matches!(a, Action::Continue));
        let a = handle_envelope(event_env(json!({"kind":"cc.ready"})), &mut s);
        assert!(matches!(a, Action::Continue));
        assert!(s.transcript.is_empty());
    }

    #[test]
    fn pageup_scrolls_up_by_half_transcript() {
        let mut s = ChatState::new();
        s.dims.rows = 11; // transcript area = 10 → half-page = 5
        for i in 0..20 {
            s.push_entry(Role::User, format!("{i}"));
        }
        let env = event_env(json!({
            "kind":"nefor-tui.input.key",
            "key":"pageup",
            "modifiers":[],
        }));
        handle_envelope(env, &mut s);
        assert_eq!(s.scroll_offset, 5);
    }

    #[test]
    fn mouse_scroll_up_moves_viewport_up() {
        let mut s = ChatState::new();
        for i in 0..20 {
            s.push_entry(Role::User, format!("{i}"));
        }
        let env = event_env(json!({
            "kind":"nefor-tui.input.mouse",
            "action":"scroll_up",
            "row": 0,
            "col": 0,
            "modifiers": [],
        }));
        handle_envelope(env, &mut s);
        assert_eq!(s.scroll_offset, WHEEL_ROWS_PER_NOTCH);
    }

    #[test]
    fn mouse_scroll_down_reduces_offset() {
        let mut s = ChatState::new();
        for i in 0..20 {
            s.push_entry(Role::User, format!("{i}"));
        }
        s.scroll_up(10);
        let env = event_env(json!({
            "kind":"nefor-tui.input.mouse",
            "action":"scroll_down",
            "row": 0,
            "col": 0,
            "modifiers": [],
        }));
        handle_envelope(env, &mut s);
        assert_eq!(s.scroll_offset, 10 - WHEEL_ROWS_PER_NOTCH);
    }

    #[test]
    fn shutdown_system_message_yields_shutdown_action() {
        let mut s = ChatState::new();
        let env = Envelope::system(
            PluginName::engine(),
            ts(),
            SystemBody::Shutdown {
                reason: None,
                grace_ms: None,
            },
        );
        let a = handle_envelope(env, &mut s);
        assert!(matches!(a, Action::Shutdown));
    }

    #[test]
    fn unknown_event_is_ignored() {
        let mut s = ChatState::new();
        let a = handle_envelope(event_env(json!({"kind":"some.unknown"})), &mut s);
        assert!(matches!(a, Action::Continue));
    }

    #[test]
    fn hello_body_shape() {
        let b = hello_body();
        assert_eq!(b["kind"], Value::String("nefor-chat.hello".into()));
        assert_eq!(b["version"], Value::String(PLUGIN_VERSION.into()));
    }

    #[test]
    fn cc_prompt_body_carries_text() {
        let b = cc_prompt_body("go");
        assert_eq!(b["kind"], Value::String("cc.prompt".into()));
        assert_eq!(b["text"], Value::String("go".into()));
    }

    #[test]
    fn cc_resume_body_without_id_omits_field() {
        let b = cc_resume_body(None);
        assert_eq!(b["kind"], Value::String("cc.resume".into()));
        assert!(b.get("session_id").is_none());
    }

    #[test]
    fn cc_resume_body_with_id_includes_field() {
        let b = cc_resume_body(Some("abc"));
        assert_eq!(b["session_id"], Value::String("abc".into()));
    }

    #[test]
    fn parse_command_resume_alone() {
        assert_eq!(parse_command("/resume"), Some(Command::ResumeRecent));
        assert_eq!(parse_command("  /resume  "), Some(Command::ResumeRecent));
    }

    #[test]
    fn parse_command_resume_with_uuid() {
        let id = "550e8400-e29b-41d4-a716-446655440000";
        assert_eq!(
            parse_command(&format!("/resume {id}")),
            Some(Command::ResumeSpecific(id.to_owned()))
        );
    }

    #[test]
    fn parse_command_unknown_slash_stays_prompt() {
        // "/think step by step" is a legitimate prompt — don't intercept it.
        assert_eq!(parse_command("/think step by step"), None);
        assert_eq!(parse_command("hello"), None);
        assert_eq!(parse_command(""), None);
    }
}
