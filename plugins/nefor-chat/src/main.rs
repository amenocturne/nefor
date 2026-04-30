//! nefor-chat — chat UI plugin for nefor.
//!
//! Bridges any [`chat-contract v0.1`] producer (e.g. mock-plugin) and
//! [nefor-tui](../nefor-tui/) (cell-grid renderer) over NCP v0.1.
//! Owns chat-layer state — transcript, input buffer, scroll offset, session
//! metadata — and translates both sides into grid mutations published on
//! the bus.
//!
//! The plugin never touches the terminal directly; every rendering concern
//! is expressed as a `nefor-tui.*` event. Inputs are consumed exclusively as
//! `chat.*` events; the starter Lua config is responsible for renaming any
//! producer-specific event names (e.g. `cc.*`) into the chat contract before
//! they hit this plugin.
//!
//! [`chat-contract v0.1`]: required `chat.message.append`, `chat.stream.delta`,
//! `chat.stream.end`; optional `chat.session.stats`, `chat.tool.start`,
//! `chat.tool.end`, `chat.history.replay`. User → harness:
//! `chat.input.submit`.

mod error;
mod ncp;
mod render;
mod sidebar;
mod state;
mod wrap;

use std::time::{Duration, Instant};

use nefor_protocol::{Body, Envelope, PluginOutgoing, SystemBody};
use serde_json::{Map, Value};
use tokio::sync::mpsc;

use crate::error::ChatError;
use crate::state::{
    slash_command_matches, AuthStatus, ChatState, DagNodeState, DagNodeStatus, DagRunUiState,
    Popup, Role, SessionMetadata, SlashCommand,
};
use std::collections::BTreeMap;
use std::collections::HashSet;

/// Cadence for the pending-counter tick. One second is the placeholder's
/// resolution ("[thinking… Ns]"); a shorter interval would re-render
/// without the visible Ns changing.
const PENDING_TICK: Duration = Duration::from_secs(1);

/// Two ESCs within this window escalate to `Action::InterruptAll`. 600ms
/// is comfortable for a deliberate double-tap and short enough that a
/// stray ESC followed by an unrelated one half a second later won't
/// nuke the user's runs by accident.
const DOUBLE_ESC_WINDOW: Duration = Duration::from_millis(600);

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
    let mut pending_tick = tokio::time::interval(PENDING_TICK);
    // The first tick fires immediately; skip it so the counter starts at
    // 0s on submit rather than off by one.
    pending_tick.tick().await;

    loop {
        let env = tokio::select! {
            biased;
            maybe = in_rx.recv() => match maybe {
                Some(Ok(env)) => env,
                Some(Err(e)) => {
                    tracing::warn!(error = %e, "stdin parse error; dropping line");
                    continue;
                }
                None => {
                    tracing::info!("stdin closed; exiting");
                    break;
                }
            },
            _ = pending_tick.tick() => {
                // Re-render only while pending and unacknowledged — that's
                // when the "[thinking… Ns]" counter advances. Also re-render
                // when an open toast popup has expired and needs to clear,
                // or while DAG runs are active (the per-node elapsed counter
                // advances and finished-run linger windows need pruning).
                // Idle ticks (neither condition met) do nothing.
                let now = Instant::now();
                let counter_active = state.pending_seconds_at(now).is_some();
                let toast_expired = state.toast_expired_at(now);
                if toast_expired {
                    state.close_popup();
                }
                let dag_active = state.has_dag_activity();
                if dag_active {
                    let now_ms = state.now_ms();
                    if state.prune_finished_dag_runs(now_ms) {
                        state.invalidate_row_cache();
                    }
                }
                if counter_active || toast_expired || dag_active {
                    state.bump_transcript_version();
                    if state.tui_ready {
                        if !palette_emitted {
                            emit_palette(&out_tx).await?;
                            palette_emitted = true;
                        }
                        emit_render(&out_tx, &mut state).await?;
                    }
                }
                continue;
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
                    emit_render(&out_tx, &mut state).await?;
                }
            }
            Action::Interrupt => {
                // Acknowledge the watchdog so it doesn't fire after the abort,
                // but keep `pending`/`awaiting_response_since` armed — the
                // harness's synthetic chat.stream.end (followed by the
                // [interrupted] system message) is what completes the turn.
                state.acknowledge_response();
                send_event(&out_tx, interrupt_body()).await?;
                if state.tui_ready {
                    if !palette_emitted {
                        emit_palette(&out_tx).await?;
                        palette_emitted = true;
                    }
                    emit_render(&out_tx, &mut state).await?;
                }
            }
            Action::InterruptAll => {
                state.acknowledge_response();
                send_event(&out_tx, interrupt_all_body()).await?;
                if state.tui_ready {
                    if !palette_emitted {
                        emit_palette(&out_tx).await?;
                        palette_emitted = true;
                    }
                    emit_render(&out_tx, &mut state).await?;
                }
            }
            Action::SelectModel(sel) => {
                send_event(
                    &out_tx,
                    model_set_body(Some(&sel.provider), &sel.model),
                )
                .await?;
                if state.tui_ready {
                    if !palette_emitted {
                        emit_palette(&out_tx).await?;
                        palette_emitted = true;
                    }
                    emit_render(&out_tx, &mut state).await?;
                }
            }
            Action::RespondToolPermission { id, decision } => {
                send_event(&out_tx, tool_permission_response_body(&id, &decision)).await?;
                if state.tui_ready {
                    if !palette_emitted {
                        emit_palette(&out_tx).await?;
                        palette_emitted = true;
                    }
                    emit_render(&out_tx, &mut state).await?;
                }
            }
            Action::SubmitPrompt(text) => {
                // Slash-commands are intercepted here instead of being shipped
                // as raw prompts. For `/resume` we don't push a confirmation
                // entry — the harness responds with `chat.history.replay`
                // which clears and repopulates the transcript. Other slash
                // commands map to specialised events the user's Lua config
                // handles (login, logout, model selection); unknown commands
                // ship as a generic `chat.command` for user-defined handlers.
                // Regular text follows the normal `chat.input.submit` path.
                if let Some(cmd) = parse_command(&text) {
                    handle_command(cmd, &mut state, &out_tx).await?;
                } else {
                    // Register the user turn locally before shipping the
                    // submit event — keeps the transcript and the outgoing
                    // event in the same logical beat.
                    state.push_entry(Role::User, text.clone());
                    state.push_history(text.clone());
                    state.begin_turn();
                    state.arm_watchdog();
                    send_event(&out_tx, input_submit_body(&text)).await?;
                }
                if state.tui_ready {
                    if !palette_emitted {
                        emit_palette(&out_tx).await?;
                        palette_emitted = true;
                    }
                    emit_render(&out_tx, &mut state).await?;
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
    /// The user hit Enter — flush the buffer as a `chat.input.submit` and render.
    SubmitPrompt(String),
    /// The user hit ESC during a live turn — emit `chat.interrupt` and render.
    Interrupt,
    /// The user hit ESC twice within DOUBLE_ESC_WINDOW — escalate: cancel
    /// the in-flight chat run AND every sub-graph run AND drop any queued
    /// deferred results. Emits `chat.interrupt_all`; the orchestrator
    /// drives the actual cancellation fan-out.
    InterruptAll,
    /// Model picker confirmed a row — emit `chat.model.set` and render.
    SelectModel(ModelSelection),
    /// User responded to a tool-permission popup — emit
    /// `tool.permission_response { id, decision }` and render.
    /// `decision` is `"approve"` or `"deny"`.
    RespondToolPermission { id: String, decision: String },
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
            // The TUI's grid is freshly initialised — every cell on its
            // side is blank, so anything we previously thought we had
            // emitted is no longer reflected on screen.
            state.invalidate_row_cache();
            Action::Render
        }
        "nefor-tui.input.resize" => {
            if let (Some(c), Some(r)) = (as_u32(map, "cols"), as_u32(map, "rows")) {
                state.dims.cols = c;
                state.dims.rows = r;
            }
            // `apply_resize` on the TUI side reallocates and blanks every
            // cell, so the next frame must re-emit every row regardless of
            // whether its content changed.
            state.invalidate_row_cache();
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
        // ---- chat-contract v0.1 input path --------------------------
        "chat.message.append" => {
            let Some(role_str) = map.get("role").and_then(Value::as_str) else {
                return Action::Continue;
            };
            let Some(text) = map.get("text").and_then(Value::as_str) else {
                return Action::Continue;
            };
            let role = match role_str {
                "user" => Role::User,
                "assistant" => Role::Assistant,
                "system" => Role::System,
                _ => return Action::Continue,
            };
            // Same defense as `finalize_assistant`'s empty-text guard: an
            // empty append silently pushes a blank row that has no body to
            // render and breaks the per-role cadence (e.g. an empty assistant
            // entry still gets the model+duration footer stamp). Drop it.
            if text.is_empty() {
                return Action::Continue;
            }
            if role == Role::Assistant {
                state.acknowledge_response();
            }
            // Route system error-shaped messages to the error popup instead
            // of the transcript so the user can't miss them buried mid-scroll.
            if role == Role::System {
                let stripped = text
                    .strip_prefix("Error: ")
                    .or_else(|| text.strip_prefix("[error] "))
                    .or_else(|| text.strip_prefix("[error]"));
                if let Some(body) = stripped {
                    let body = body.trim();
                    let msg = if body.is_empty() { text } else { body };
                    state.open_popup_error("error", msg.to_owned(), None);
                    return Action::Render;
                }
            }
            state.push_entry(role, text.to_owned());
            Action::Render
        }
        "chat.stream.delta" => {
            if let Some(t) = map.get("text").and_then(Value::as_str) {
                state.acknowledge_response();
                state.append_assistant_delta(t);
                Action::Render
            } else {
                Action::Continue
            }
        }
        "chat.stream.reasoning_delta" => {
            // Live-stream the model's thinking trace into the in-flight
            // assistant entry. The renderer shows it as a dim preview
            // while content is empty; once content arrives the trace
            // collapses to a one-row marker (see `chat.stream.reasoning_end`).
            if let Some(t) = map.get("text").and_then(Value::as_str) {
                if !t.is_empty() {
                    state.acknowledge_response();
                    state.append_assistant_reasoning_delta(t);
                    return Action::Render;
                }
            }
            Action::Continue
        }
        "chat.stream.reasoning_end" => {
            // Reasoning channel closed — either content has started or
            // the turn ended reasoning-only. Flip the in-flight entry's
            // reasoning row from live-preview to collapsed and stamp
            // the duration. The full trace is preserved on the entry
            // for the Ctrl+O expanded view.
            let final_text = map
                .get("text")
                .and_then(Value::as_str)
                .map(|s| s.to_owned());
            let duration_ms = map.get("duration_ms").and_then(Value::as_u64);
            state.acknowledge_response();
            state.finalize_assistant_reasoning(final_text, duration_ms);
            Action::Render
        }
        "chat.stream.end" => {
            let authoritative = map
                .get("text")
                .and_then(Value::as_str)
                .map(|s| s.to_owned());
            let model = map
                .get("model")
                .and_then(Value::as_str)
                .map(|s| s.to_owned());
            let duration_ms = map.get("duration_ms").and_then(Value::as_u64);
            state.acknowledge_response();
            state.finalize_assistant(authoritative);
            state.stamp_last_assistant(model, duration_ms);
            state.end_turn();
            Action::Render
        }
        "chat.session.stats" => {
            state.metadata.update_from(map);
            Action::Render
        }
        "chat.tool.start" => {
            let Some(name) = map.get("name").and_then(Value::as_str) else {
                return Action::Continue;
            };
            let id = map
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_default();
            let input_json = map
                .get("input")
                .map(|v| v.to_string())
                .unwrap_or_default();
            state.acknowledge_response();
            state.push_tool_start(id, name.to_owned(), input_json);
            Action::Render
        }
        "chat.tool.end" => {
            let id = map
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            // `output` may arrive as a string or as a structured value; we
            // stringify for display either way. Pretty-print objects and
            // arrays so the expanded view stays readable.
            let output = match map.get("output") {
                Some(Value::String(s)) => s.clone(),
                Some(v) => serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string()),
                None => String::new(),
            };
            let error = map
                .get("error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            state.acknowledge_response();
            if state.attach_tool_end(&id, output, error) {
                Action::Render
            } else {
                tracing::debug!(?id, "chat.tool.end with no matching tool entry");
                Action::Continue
            }
        }
        "chat.auth.status" => handle_auth_status(map, state),
        "chat.models.listed" => handle_models_listed(map, state),
        "chat.model.set_ack" => handle_model_set_ack(map, state),
        "chat.popup" => handle_popup_event(map, state),
        "chat.tool.permission_request" => handle_tool_permission_request(map, state),
        "tool-gate.mode_changed" => handle_gate_mode_changed(map, state),
        "graph.run_started" => handle_dag_run_started(map, state),
        "graph.node_dispatched" => handle_dag_node_dispatched(map, state),
        "graph.node_result" => handle_dag_node_result(map, state),
        "graph.run_complete" => handle_dag_run_complete(map, state),
        "chat.history.replay" => {
            // Replace the transcript with stored-on-disk history from a
            // previous session. The producer guarantees `entries` is
            // already in chronological order.
            state.acknowledge_response();
            state.transcript.clear();
            state.bump_transcript_version();
            let mut count = 0usize;
            if let Some(arr) = map.get("entries").and_then(Value::as_array) {
                for e in arr {
                    let Some(role_str) = e.get("role").and_then(Value::as_str) else {
                        continue;
                    };
                    match role_str {
                        "user" | "assistant" => {
                            let role = if role_str == "user" {
                                Role::User
                            } else {
                                Role::Assistant
                            };
                            let text = e
                                .get("text")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_owned();
                            state.push_entry(role, text);
                            // Stamp model on assistant entries so the per-turn
                            // footer renders after replay. Live entries get
                            // stamped via `chat.stream.end`; this is the
                            // replay equivalent. `duration_ms` isn't recorded
                            // in claude's session log, so it stays None and
                            // the footer renders model-only.
                            if role == Role::Assistant {
                                let model = e
                                    .get("model")
                                    .and_then(Value::as_str)
                                    .map(str::to_owned);
                                if model.is_some() {
                                    state.stamp_last_assistant(model, None);
                                }
                            }
                            count += 1;
                        }
                        "tool" => {
                            let id = e
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_owned();
                            let name = e
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_owned();
                            let input_json = match e.get("input") {
                                Some(v) => serde_json::to_string(v).unwrap_or_default(),
                                None => String::new(),
                            };
                            state.push_tool_start(id.clone(), name, input_json);
                            // `output` is null when the source session was
                            // truncated mid-turn — render the tool collapsed
                            // with no result.
                            if let Some(out_val) = e.get("output") {
                                if !out_val.is_null() {
                                    let output = match out_val {
                                        Value::String(s) => s.clone(),
                                        v => serde_json::to_string_pretty(v)
                                            .unwrap_or_else(|_| v.to_string()),
                                    };
                                    let error = e
                                        .get("error")
                                        .and_then(Value::as_bool)
                                        .unwrap_or(false);
                                    state.attach_tool_end(&id, output, error);
                                }
                            }
                            count += 1;
                        }
                        _ => continue,
                    }
                }
            }
            let session = map.get("session_id").and_then(Value::as_str).unwrap_or("?");
            state.push_entry(
                Role::System,
                format!("resumed · {count} messages · session {session}"),
            );
            Action::Render
        }
        _ => Action::Continue,
    }
}

/// Handle a `chat.popup { level, title, message, source?, ttl_ms? }` event
/// — the public popup contract. Any plugin on the bus can publish
/// info / warning / error popups by emitting this kind. Internal
/// nefor-chat paths (login/logout validation, auth.status="error",
/// `Error:`-prefixed system messages) call the helpers directly and don't
/// go through this handler.
///
/// Validation:
///   * `level` must be `"info"`, `"warning"`, or `"error"`. Unknown values
///     drop the event with a `tracing::warn!`.
///   * Both `title` and `message` missing or empty drops the event as
///     malformed. One missing is fine — the absent field falls back to a
///     placeholder so the popup still opens with usable copy.
///
/// Optional `ttl_ms` upgrades the popup to a self-dismissing
/// `Popup::Toast` (no modal/full body, just a transient line near the input
/// bar). Used by nefor-tui to surface "Copied N chars" after a successful
/// mouse-selection copy. The toast variant ignores `level`, `title`, and
/// `source` — only `message` is shown.
fn handle_popup_event(map: &Map<String, Value>, state: &mut ChatState) -> Action {
    let level = match map.get("level").and_then(Value::as_str) {
        Some(s) => s,
        None => {
            tracing::warn!("chat.popup dropped: missing 'level'");
            return Action::Continue;
        }
    };
    let title = map.get("title").and_then(Value::as_str).unwrap_or("");
    let message = map.get("message").and_then(Value::as_str).unwrap_or("");
    if title.is_empty() && message.is_empty() {
        tracing::warn!(level, "chat.popup dropped: both 'title' and 'message' are missing/empty");
        return Action::Continue;
    }

    // ttl_ms turns the popup into a Toast regardless of level. The level
    // still has to validate (so `chat.popup` keeps a single set of accepted
    // values) but the rendered surface is the transient toast row.
    if let Some(ttl_ms) = map.get("ttl_ms").and_then(Value::as_u64) {
        if !matches!(level, "info" | "warning" | "error") {
            tracing::warn!(level, "chat.popup dropped: unknown level");
            return Action::Continue;
        }
        let body = if message.is_empty() { title } else { message };
        state.open_popup_toast(body.to_owned(), Duration::from_millis(ttl_ms));
        return Action::Render;
    }

    let title_owned = if title.is_empty() {
        "(no title)".to_owned()
    } else {
        title.to_owned()
    };
    let message_owned = if message.is_empty() {
        "(no message)".to_owned()
    } else {
        message.to_owned()
    };
    let source = map
        .get("source")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    match level {
        "info" => state.open_popup_info(title_owned, message_owned, source),
        "warning" => state.open_popup_warning(title_owned, message_owned, source),
        "error" => state.open_popup_error(title_owned, message_owned, source),
        other => {
            tracing::warn!(level = other, "chat.popup dropped: unknown level");
            return Action::Continue;
        }
    }
    Action::Render
}

/// Handle a `chat.tool.permission_request { id, tool, args }` event —
/// emitted by tool-gate when a tool call hits a `prompt`-policy decision.
/// Opens a `Popup::ToolPermission` with a pretty-printed args preview.
/// Missing `id` or `tool` drops the event (no caller to address). The
/// `args` field is optional — absent or invalid JSON renders as
/// `(no args)`.
fn handle_tool_permission_request(map: &Map<String, Value>, state: &mut ChatState) -> Action {
    let Some(id) = map.get("id").and_then(Value::as_str) else {
        tracing::warn!("chat.tool.permission_request dropped: missing 'id'");
        return Action::Continue;
    };
    let Some(tool) = map.get("tool").and_then(Value::as_str) else {
        tracing::warn!("chat.tool.permission_request dropped: missing 'tool'");
        return Action::Continue;
    };
    let args_preview = match map.get("args") {
        Some(v) if !v.is_null() => {
            // Pretty-print so multiline arg blobs read cleanly. The body
            // is rendered with wrap inside the popup so we don't have to
            // truncate here.
            serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
        }
        _ => "(no args)".to_owned(),
    };
    let source = map
        .get("source")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    state.open_popup_tool_permission(id, tool, args_preview, source);
    Action::Render
}

/// Handle a `tool-gate.mode_changed { mode }` broadcast — emitted by
/// tool-gate on every transition (and once at startup). Mirrors the gate's
/// runtime mode into chat state so the statusline indicator matches.
fn handle_gate_mode_changed(map: &Map<String, Value>, state: &mut ChatState) -> Action {
    let mode = match map.get("mode").and_then(Value::as_str) {
        Some(s) => s,
        None => {
            tracing::warn!("tool-gate.mode_changed missing string field 'mode'");
            return Action::Continue;
        }
    };
    let yolo = match mode {
        "yolo" => true,
        "normal" => false,
        other => {
            tracing::warn!(mode = %other, "tool-gate.mode_changed: unknown mode");
            return Action::Continue;
        }
    };
    if state.gate_yolo == yolo {
        return Action::Continue;
    }
    state.gate_yolo = yolo;
    // Statusline reads `gate_yolo` directly; bump version so the renderer
    // re-emits the affected row.
    state.bump_transcript_version();
    Action::Render
}

/// Handle a `chat.auth.status { provider, state, message? }` event from a
/// provider adapter. Updates the per-provider auth map, registers the
/// provider, and promotes the first connected provider to active. State
/// changes drive the statusline indicator only; "error" surfaces an error
/// popup (interrupting); "connected", "login_required", "disconnected" stay
/// passive (no popup, no transcript noise).
fn handle_auth_status(map: &Map<String, Value>, state: &mut ChatState) -> Action {
    let Some(provider) = map.get("provider").and_then(Value::as_str) else {
        return Action::Continue;
    };
    let Some(auth_state) = map.get("state").and_then(Value::as_str) else {
        return Action::Continue;
    };
    let message = map
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_owned);

    state.register_provider(provider);
    state.auth_status.insert(
        provider.to_owned(),
        AuthStatus {
            state: auth_state.to_owned(),
            message: message.clone(),
        },
    );
    if auth_state == "connected" && state.active_provider.is_none() {
        state.active_provider = Some(provider.to_owned());
    }

    if auth_state == "error" {
        let body = message.unwrap_or_else(|| "auth error".to_owned());
        state.open_popup_error(provider, body, None);
    } else {
        state.invalidate_row_cache();
    }
    Action::Render
}

/// Cap on how many model IDs we render inline before showing a
/// "...and N more" footer. Keeps `/model` from overflowing the transcript
/// when a hosted catalog has hundreds of entries.
const MODEL_LIST_CAP: usize = 30;

/// Handle a `chat.models.listed { provider, models }` event. When a model
/// picker popup is open, append the provider's models into it (and clear the
/// awaiting-flag for that provider). Otherwise fall back to the legacy
/// transcript-system-message path so producers that emit `chat.models.listed`
/// outside of a `/model` flow still surface visibly.
fn handle_models_listed(map: &Map<String, Value>, state: &mut ChatState) -> Action {
    let Some(provider) = map.get("provider").and_then(Value::as_str) else {
        return Action::Continue;
    };
    let models: Vec<String> = map
        .get("models")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    if matches!(state.popup, Some(Popup::ModelPicker { .. })) {
        state.popup_models_listed(provider, &models);
        return Action::Render;
    }
    let line = format_models_listed_line(provider, &models);
    state.push_entry(Role::System, line);
    state.invalidate_row_cache();
    Action::Render
}

/// Render the system-message body for `chat.models.listed`. Empty list →
/// "(no models)"; small list → comma-separated; larger list → newline +
/// indented entries; truncates past [`MODEL_LIST_CAP`] with a footer.
fn format_models_listed_line(provider: &str, models: &[String]) -> String {
    if models.is_empty() {
        return format!("[{provider}] models: (none reported)");
    }
    let total = models.len();
    let shown = total.min(MODEL_LIST_CAP);
    let mut out = format!("[{provider}] models:\n");
    for m in &models[..shown] {
        out.push_str("  ");
        out.push_str(m);
        out.push('\n');
    }
    if total > shown {
        out.push_str(&format!("  ...and {} more", total - shown));
    } else {
        // Trim the trailing newline so the entry doesn't render a blank row.
        out.truncate(out.trim_end().len());
    }
    out
}

/// Handle a `chat.model.set_ack { provider, model }` event. Updates both
/// the per-provider active-model map (drives `/model` defaults) and the
/// statusline's headline `model` field, so the new selection is visible
/// immediately rather than only after the next turn's `chat.session.stats`
/// catches up. No transcript line — the statusline is enough; doubling it
/// up just adds noise.
fn handle_model_set_ack(map: &Map<String, Value>, state: &mut ChatState) -> Action {
    let Some(provider) = map.get("provider").and_then(Value::as_str) else {
        return Action::Continue;
    };
    let Some(model) = map.get("model").and_then(Value::as_str) else {
        return Action::Continue;
    };
    state
        .active_model_per_provider
        .insert(provider.to_owned(), model.to_owned());
    state.metadata.model = Some(model.to_owned());
    Action::Render
}

fn handle_key(map: &Map<String, Value>, state: &mut ChatState) -> Action {
    let Some(key) = map.get("key").and_then(Value::as_str) else {
        return Action::Continue;
    };
    let mods = map.get("modifiers").and_then(Value::as_array);
    let has_ctrl = mods
        .map(|arr| arr.iter().any(|v| v.as_str() == Some("ctrl")))
        .unwrap_or(false);
    let has_alt = mods
        .map(|arr| arr.iter().any(|v| v.as_str() == Some("alt")))
        .unwrap_or(false);
    let has_shift = mods
        .map(|arr| arr.iter().any(|v| v.as_str() == Some("shift")))
        .unwrap_or(false);

    // Popup overlay owns input while open. ESC always closes; per-popup
    // routing handles the rest. Other key handlers below (chat shortcuts,
    // history nav, typing) are gated behind this early return.
    //
    // Exception: SlashAutocomplete is a *sticky overlay* — it shadows the
    // input buffer but the popup handler forwards typed characters back into
    // the buffer (via `refresh_slash_autocomplete`). The popup handler also
    // owns the navigation keys, so this branch still routes everything to it.
    if state.popup.is_some() {
        return handle_popup_key(key, has_ctrl, has_alt, state);
    }

    // Word-level (Alt) — handle before plain key dispatch so Alt+Backspace
    // doesn't fall through to plain Backspace.
    if has_alt {
        match key {
            "backspace" => {
                state.input.delete_word_back();
                return Action::Render;
            }
            "delete" => {
                state.input.delete_word_forward();
                return Action::Render;
            }
            "left" => {
                state.input.move_word_back();
                return Action::Render;
            }
            "right" => {
                state.input.move_word_forward();
                return Action::Render;
            }
            _ => {}
        }
    }

    // Line-level (Ctrl) — readline-style bindings. macOS Cmd doesn't reach
    // TUIs, so Ctrl is the closest reachable modifier.
    if has_ctrl {
        match key {
            "a" => {
                state.input.cursor_home();
                return Action::Render;
            }
            "e" => {
                state.input.cursor_end();
                return Action::Render;
            }
            "u" => {
                state.input.delete_to_start();
                return Action::Render;
            }
            "k" => {
                state.input.delete_to_end();
                return Action::Render;
            }
            "w" => {
                state.input.delete_word_back();
                return Action::Render;
            }
            "o" => {
                // Toggle global tool-call expansion (Claude Code-style).
                state.toggle_tools_expanded();
                state.invalidate_row_cache();
                return Action::Render;
            }
            "b" => {
                // Toggle the right sidebar pane (DAG widget for v1, more
                // widgets later). Mirrors the Ctrl+O pattern: mutate state
                // directly, invalidate the row cache so the resized chat
                // pane re-emits, render. Auto-hide rules in
                // `state.sidebar_width()` decide whether the toggle takes
                // visible effect right now.
                state.toggle_sidebar();
                state.invalidate_row_cache();
                return Action::Render;
            }
            _ => {}
        }
    }

    match key {
        "enter" => {
            // Shift+Enter inserts a literal newline so the user can compose
            // multi-line prompts; plain Enter submits.
            if has_shift {
                state.input.insert_char('\n');
                return Action::Render;
            }
            if state.input.len() == 0 {
                return Action::Continue;
            }
            let text = state.input.as_string();
            state.input.clear();
            Action::SubmitPrompt(text)
        }
        "backspace" => {
            state.input.backspace();
            // Backspace in the chat buffer is the only way (without a popup
            // open) to remove the leading `/` and close the autocomplete —
            // but with no popup open here it's a no-op. The buffer-then-`/`
            // case is handled inside the popup itself.
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
            let page = page_size(state);
            state.scroll_up(page);
            Action::Render
        }
        "pagedown" => {
            let page = page_size(state);
            state.scroll_down(page);
            Action::Render
        }
        "up" => {
            // Three regimes:
            //   - empty input  → history recall (oldest entries first walk)
            //   - single-line  → no-op (don't escape into history once typing)
            //   - multi-line   → in-buffer cursor moves up; at top, no-op
            //                    (don't bleed into history mid-edit)
            if state.input.is_multiline() {
                state.input.move_cursor_up_in_buffer();
                return Action::Render;
            }
            if state.input.len() != 0 && state.history_cursor.is_none() {
                return Action::Continue;
            }
            if let Some(text) = state.history_up() {
                state.input.clear();
                state.input.insert_str(&text);
                Action::Render
            } else {
                Action::Continue
            }
        }
        "down" => {
            if state.input.is_multiline() {
                state.input.move_cursor_down_in_buffer();
                return Action::Render;
            }
            if state.input.len() != 0 && state.history_cursor.is_none() {
                return Action::Continue;
            }
            if let Some(text) = state.history_down() {
                state.input.clear();
                state.input.insert_str(&text);
                Action::Render
            } else {
                Action::Continue
            }
        }
        "escape" => {
            // Two ESCs inside DOUBLE_ESC_WINDOW escalate to "kill
            // everything" — chat run + every spawn_graph sub-graph + any
            // queued deferred results. Useful when a model wandered into
            // a thinking loop and a single chat.interrupt isn't enough
            // (sub-graphs keep churning).
            let now = std::time::Instant::now();
            let escalate = state
                .last_escape_at
                .map(|prev| now.duration_since(prev) <= DOUBLE_ESC_WINDOW)
                .unwrap_or(false);
            state.last_escape_at = Some(now);
            if escalate {
                state.last_escape_at = None;
                Action::InterruptAll
            } else if state.awaiting_response_since.is_some() {
                Action::Interrupt
            } else {
                Action::Continue
            }
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
                        // If this keystroke turned the buffer into a
                        // slash-prefixed input (typically the very first
                        // `/`), open the slash-autocomplete popup. Same
                        // gate as the popup handler — only opens when no
                        // other popup is in the way.
                        refresh_slash_autocomplete(state);
                        return Action::Render;
                    }
                }
            }
            Action::Continue
        }
    }
}

/// Outcome from a popup key handler. Most key presses just mutate state and
/// re-render; Enter on the model picker also wants to ship a `chat.model.set`
/// event, which the main loop handles via `Action::SelectModel`.
#[derive(Debug, Clone)]
struct ModelSelection {
    provider: String,
    model: String,
}

/// Per-popup key dispatch. Treats the popup as the focused widget — every
/// key either operates on the popup (cursor, query) or closes it. ESC always
/// closes regardless of popup variant; Q closes Help/Warning/Error (the
/// ModelPicker takes Q as a filter character so its only close key is ESC).
/// Returns `Action::Render` for any state mutation; `Action::Continue` for
/// keys we ignore. Selecting a model row emits a `chat.model.set` event via
/// the deferred `pending_model_select` channel — the simpler path is to
/// handle the side effect inline in the caller, but `handle_key` is sync.
/// We carry the selection back through a thread-local and let the main loop
/// drain it before the render pass.
fn handle_popup_key(
    key: &str,
    _has_ctrl: bool,
    _has_alt: bool,
    state: &mut ChatState,
) -> Action {
    if key == "escape" {
        // ToolPermission needs to TELL the gate it was denied, not just
        // close the popup — otherwise the gate hangs on `awaiting_approval`
        // and the provider's invocation never resolves. Capture the id
        // before we close.
        if let Some(Popup::ToolPermission { id, .. }) = &state.popup {
            let id = id.clone();
            state.close_popup();
            return Action::RespondToolPermission {
                id,
                decision: "deny".into(),
            };
        }
        // SlashAutocomplete: ESC closes the popup but leaves the input alone
        // so the user can keep typing or backspace deliberately.
        state.close_popup();
        return Action::Render;
    }
    // Q closes informational popups (Help, Info, Warning, Error). The
    // ModelPicker and SlashAutocomplete are excluded — Q can be a valid
    // filter / command character in their search-driven flows.
    if (key == "q" || key == "Q")
        && matches!(
            state.popup,
            Some(Popup::Help { .. })
                | Some(Popup::Info { .. })
                | Some(Popup::Warning { .. })
                | Some(Popup::Error { .. })
        )
    {
        state.close_popup();
        return Action::Render;
    }

    // Body-scroll keys (Up/Down/PageUp/PageDown/Home/End) for the read-only
    // popups. The ModelPicker has its own cursor-driven scroll, so we exclude
    // it from this branch.
    if matches!(
        state.popup,
        Some(Popup::Help { .. })
            | Some(Popup::Info { .. })
            | Some(Popup::Warning { .. })
            | Some(Popup::Error { .. })
    ) {
        if let Some(action) = handle_scrollable_popup_key(key, state) {
            return action;
        }
    }

    let popup = state.popup.as_mut().expect("popup is some (checked above)");
    match popup {
        Popup::Help { .. } => match key {
            "enter" => {
                state.close_popup();
                Action::Render
            }
            _ => Action::Continue,
        },
        Popup::Info { .. } | Popup::Warning { .. } | Popup::Error { .. } => match key {
            "enter" => {
                state.close_popup();
                Action::Render
            }
            _ => Action::Continue,
        },
        Popup::ModelPicker {
            all_models,
            query,
            cursor,
            scroll,
            awaiting: _,
        } => {
            let visible: Vec<(String, String)> = filter_models(all_models, query);
            match key {
                "up" => {
                    if !visible.is_empty() && *cursor > 0 {
                        *cursor -= 1;
                    }
                    keep_cursor_in_view(state);
                    Action::Render
                }
                "down" => {
                    if !visible.is_empty() && *cursor + 1 < visible.len() {
                        *cursor += 1;
                    }
                    keep_cursor_in_view(state);
                    Action::Render
                }
                "pageup" => {
                    let step = popup_page_step(state).max(1);
                    let popup = state
                        .popup
                        .as_mut()
                        .expect("popup is some (checked above)");
                    if let Popup::ModelPicker { cursor, .. } = popup {
                        *cursor = cursor.saturating_sub(step);
                    }
                    keep_cursor_in_view(state);
                    Action::Render
                }
                "pagedown" => {
                    let step = popup_page_step(state).max(1);
                    let popup = state
                        .popup
                        .as_mut()
                        .expect("popup is some (checked above)");
                    if let Popup::ModelPicker {
                        all_models,
                        query,
                        cursor,
                        ..
                    } = popup
                    {
                        let visible_len = filter_models(all_models, query).len();
                        if visible_len > 0 {
                            let max = visible_len - 1;
                            *cursor = (*cursor + step).min(max);
                        }
                    }
                    keep_cursor_in_view(state);
                    Action::Render
                }
                "home" => {
                    *cursor = 0;
                    *scroll = 0;
                    Action::Render
                }
                "end" => {
                    if !visible.is_empty() {
                        *cursor = visible.len() - 1;
                    }
                    keep_cursor_in_view(state);
                    Action::Render
                }
                "enter" => {
                    if let Some((provider, model)) = visible.get(*cursor).cloned() {
                        let sel = ModelSelection {
                            provider: provider.clone(),
                            model,
                        };
                        state.active_provider = Some(provider);
                        state.close_popup();
                        Action::SelectModel(sel)
                    } else {
                        Action::Continue
                    }
                }
                "backspace" => {
                    if !query.is_empty() {
                        query.pop();
                        let visible_len = filter_models(all_models, query).len();
                        if visible_len == 0 {
                            *cursor = 0;
                            *scroll = 0;
                        } else if *cursor >= visible_len {
                            *cursor = visible_len - 1;
                        }
                        keep_cursor_in_view(state);
                        Action::Render
                    } else {
                        Action::Continue
                    }
                }
                "tab" => Action::Continue,
                _ => {
                    if key.chars().count() == 1 {
                        if let Some(c) = key.chars().next() {
                            if !c.is_control() {
                                query.push(c);
                                let visible_len = filter_models(all_models, query).len();
                                if visible_len == 0 {
                                    *cursor = 0;
                                    *scroll = 0;
                                } else if *cursor >= visible_len {
                                    *cursor = visible_len - 1;
                                }
                                keep_cursor_in_view(state);
                                return Action::Render;
                            }
                        }
                    }
                    Action::Continue
                }
            }
        }
        Popup::SlashAutocomplete { .. } => handle_slash_autocomplete_key(key, state),
        // Toasts are non-interactive — they auto-dismiss on the tick.
        // Any key event lands in the input buffer below; for safety here
        // we just continue.
        Popup::Toast { .. } => Action::Continue,
        Popup::ToolPermission { id, .. } => {
            // A or Enter approves; D denies. Both close the popup and
            // ship the matching response so the gate can proceed.
            // Q is intentionally NOT a close-key here (it doesn't carry
            // a decision); the user must pick approve or deny, or use ESC
            // (handled above as deny).
            let id = id.clone();
            match key {
                "a" | "A" | "enter" => {
                    state.close_popup();
                    Action::RespondToolPermission {
                        id,
                        decision: "approve".into(),
                    }
                }
                "d" | "D" => {
                    state.close_popup();
                    Action::RespondToolPermission {
                        id,
                        decision: "deny".into(),
                    }
                }
                _ => Action::Continue,
            }
        }
    }
}

/// Body-scroll dispatcher for the read-only popups (Help / Warning / Error).
/// Returns `Some(action)` when the key was handled (cursor adjusted, popup
/// closed by Enter, etc.); returns `None` for keys that should fall through
/// to the per-variant `match` below.
fn handle_scrollable_popup_key(key: &str, state: &mut ChatState) -> Option<Action> {
    let visible_rows = popup_body_visible_rows(state);
    let body_len = popup_body_total_lines(state);
    let max_scroll = body_len.saturating_sub(visible_rows) as u16;

    let popup = state.popup.as_mut()?;
    let scroll = match popup {
        Popup::Help { scroll } => scroll,
        Popup::Info { scroll, .. } => scroll,
        Popup::Warning { scroll, .. } => scroll,
        Popup::Error { scroll, .. } => scroll,
        _ => return None,
    };
    let step = visible_rows.max(1) as u16;
    let action = match key {
        "up" => {
            *scroll = scroll.saturating_sub(1);
            Action::Render
        }
        "down" => {
            *scroll = (*scroll + 1).min(max_scroll);
            Action::Render
        }
        "pageup" => {
            *scroll = scroll.saturating_sub(step);
            Action::Render
        }
        "pagedown" => {
            *scroll = (*scroll + step).min(max_scroll);
            Action::Render
        }
        "home" => {
            *scroll = 0;
            Action::Render
        }
        "end" => {
            *scroll = max_scroll;
            Action::Render
        }
        _ => return None,
    };
    Some(action)
}

/// Number of rows the body of the *currently open* popup can show. Mirrors
/// the layout reservations in `render.rs`: 2 borders + (per variant) extra
/// rows for separator/footer.
fn popup_body_visible_rows(state: &ChatState) -> usize {
    // Slash autocomplete is special — it renders inline above the input
    // bar (not as a centered overlay), so its visible-rows count is the
    // inline cap rather than a function of `popup_rect`. Returning the
    // inline height keeps the PageUp/PageDown stepping in
    // `handle_slash_autocomplete_key` aligned with what the user sees.
    if let Some(Popup::SlashAutocomplete { matches, .. }) = &state.popup {
        let cap = render::MAX_INLINE_AUTOCOMPLETE_ROWS as usize;
        return cap.min(matches.len()).max(1);
    }

    let cols = state.dims.cols.max(1);
    let rows = state.dims.rows.max(2);
    let Some((_, popup_h, _, _)) = render::popup_rect(cols, rows) else {
        return 0;
    };
    match &state.popup {
        Some(Popup::Help { .. }) => popup_h.saturating_sub(2),
        // Top border + separator + close-hint + bottom border = 4. When
        // `source` is set the renderer adds a `from: <s>` row above the
        // close-hint, taking one more row off the body.
        Some(Popup::Info { source, .. }) => {
            popup_h.saturating_sub(if source.is_some() { 5 } else { 4 })
        }
        Some(Popup::Warning { source, .. }) => {
            popup_h.saturating_sub(if source.is_some() { 5 } else { 4 })
        }
        Some(Popup::Error { source, .. }) => {
            popup_h.saturating_sub(if source.is_some() { 5 } else { 4 })
        }
        Some(Popup::ModelPicker { .. }) => popup_h.saturating_sub(5),
        // Slash autocomplete handled above (inline render).
        Some(Popup::SlashAutocomplete { .. }) => 0,
        // Toast is bottom-anchored single-line; no scrollable body.
        Some(Popup::Toast { .. }) => 0,
        // Tool permission popup: top border + title + separator + footer
        // (a/d hint) + bottom border ≈ 5 rows of chrome. Body is the args
        // preview, no scroll today (popup_body_total_lines returns 0
        // for it, so scroll never advances).
        Some(Popup::ToolPermission { .. }) => popup_h.saturating_sub(5),
        None => 0,
    }
}

/// Total number of body lines the *currently open* popup will produce. Drives
/// the scroll-clamp math; matches the body that `render.rs` will emit.
fn popup_body_total_lines(state: &ChatState) -> usize {
    let cols = state.dims.cols.max(1);
    let rows = state.dims.rows.max(2);
    let Some((popup_w, _, _, _)) = render::popup_rect(cols, rows) else {
        return 0;
    };
    let inner_w = popup_w.saturating_sub(2);
    let body_w = inner_w.saturating_sub(2);
    match &state.popup {
        Some(Popup::Help { .. }) => {
            let label_w = 14usize.min(inner_w.saturating_sub(4));
            render::help_body_lines(label_w).len()
        }
        Some(Popup::Info { message, .. })
        | Some(Popup::Warning { message, .. })
        | Some(Popup::Error { message, .. }) => {
            if body_w == 0 {
                0
            } else {
                wrap::wrap_to_width(message, body_w).len()
            }
        }
        _ => 0,
    }
}

/// After a ModelPicker cursor move, slide `scroll` so the cursor sits inside
/// the visible window. No-op when there's no model picker open.
fn keep_cursor_in_view(state: &mut ChatState) {
    let visible_rows = popup_body_visible_rows(state);
    let Some(Popup::ModelPicker {
        all_models,
        query,
        cursor,
        scroll,
        ..
    }) = state.popup.as_mut()
    else {
        return;
    };
    let visible_len = filter_models(all_models, query).len();
    if visible_rows == 0 || visible_len == 0 {
        *scroll = 0;
        return;
    }
    let mut start = (*scroll as usize).min(visible_len.saturating_sub(visible_rows));
    if *cursor < start {
        start = *cursor;
    } else if *cursor >= start + visible_rows {
        start = *cursor + 1 - visible_rows;
    }
    *scroll = start as u16;
}

/// Slash-autocomplete key handling. Up/Down moves cursor, PageUp/PageDown by
/// visible rows, Tab completes (without submitting), Enter completes AND
/// submits, Backspace removes the last char (and may close the popup if the
/// `/` is gone). Any other character forwards to the input buffer (via the
/// caller) — this fn only handles popup-internal navigation.
fn handle_slash_autocomplete_key(key: &str, state: &mut ChatState) -> Action {
    let visible_rows = popup_body_visible_rows(state);
    match key {
        "up" => {
            if let Some(Popup::SlashAutocomplete {
                matches,
                cursor,
                scroll,
            }) = state.popup.as_mut()
            {
                if !matches.is_empty() {
                    *cursor = if *cursor == 0 {
                        matches.len() - 1
                    } else {
                        *cursor - 1
                    };
                }
                clamp_slash_scroll(matches.len(), *cursor, scroll, visible_rows);
            }
            Action::Render
        }
        "down" => {
            if let Some(Popup::SlashAutocomplete {
                matches,
                cursor,
                scroll,
            }) = state.popup.as_mut()
            {
                if !matches.is_empty() {
                    *cursor = if *cursor + 1 >= matches.len() {
                        0
                    } else {
                        *cursor + 1
                    };
                }
                clamp_slash_scroll(matches.len(), *cursor, scroll, visible_rows);
            }
            Action::Render
        }
        "pageup" => {
            if let Some(Popup::SlashAutocomplete { cursor, scroll, matches }) = state.popup.as_mut() {
                let step = visible_rows.max(1);
                *cursor = cursor.saturating_sub(step);
                clamp_slash_scroll(matches.len(), *cursor, scroll, visible_rows);
            }
            Action::Render
        }
        "pagedown" => {
            if let Some(Popup::SlashAutocomplete {
                matches,
                cursor,
                scroll,
            }) = state.popup.as_mut()
            {
                let step = visible_rows.max(1);
                if !matches.is_empty() {
                    *cursor = (*cursor + step).min(matches.len() - 1);
                }
                clamp_slash_scroll(matches.len(), *cursor, scroll, visible_rows);
            }
            Action::Render
        }
        "tab" => {
            // Tab: replace input with `/<name>` (+ trailing space if the
            // command takes args), but DO NOT submit. Keeps the popup open
            // while the registry is re-filtered against the new prefix.
            if let Some(Popup::SlashAutocomplete { matches, cursor, .. }) = state.popup.as_ref() {
                if let Some(cmd) = matches.get(*cursor).cloned() {
                    apply_slash_completion(state, &cmd, false);
                    return Action::Render;
                }
            }
            Action::Continue
        }
        "enter" => {
            // Enter: replace input with the cursor's match and SUBMIT.
            if let Some(Popup::SlashAutocomplete { matches, cursor, .. }) = state.popup.as_ref() {
                if let Some(cmd) = matches.get(*cursor).cloned() {
                    apply_slash_completion(state, &cmd, true);
                    let text = state.input.as_string();
                    state.input.clear();
                    state.close_popup_slash_autocomplete();
                    return Action::SubmitPrompt(text);
                }
            }
            Action::Continue
        }
        "backspace" => {
            state.input.backspace();
            refresh_slash_autocomplete(state);
            Action::Render
        }
        // Forward any other printable character into the input buffer, then
        // re-filter the autocomplete list. Cursor movement keys inside the
        // input (left/right/home/end) are intentionally ignored here — the
        // user typically types the command name to drill in.
        _ => {
            if key.chars().count() == 1 {
                if let Some(c) = key.chars().next() {
                    if !c.is_control() {
                        state.input.insert_char(c);
                        refresh_slash_autocomplete(state);
                        return Action::Render;
                    }
                }
            }
            Action::Continue
        }
    }
}

/// Replace the input buffer with `/<name>` (+ optional trailing space when
/// `submit` is `false` and the command takes args). When `submit` is `true`
/// we omit the trailing space so the parser receives a clean command name.
fn apply_slash_completion(state: &mut ChatState, cmd: &SlashCommand, submit: bool) {
    state.input.clear();
    state.input.insert_char('/');
    state.input.insert_str(&cmd.name);
    if !submit && cmd.takes_args {
        state.input.insert_char(' ');
    }
    if !submit {
        // Re-filter against the new buffer so the popup updates.
        refresh_slash_autocomplete(state);
    }
}

/// Slide `scroll` so the cursor sits inside `[scroll, scroll + visible_rows)`.
fn clamp_slash_scroll(len: usize, cursor: usize, scroll: &mut u16, visible_rows: usize) {
    if visible_rows == 0 || len == 0 {
        *scroll = 0;
        return;
    }
    let max_start = len.saturating_sub(visible_rows);
    let mut start = (*scroll as usize).min(max_start);
    if cursor < start {
        start = cursor;
    } else if cursor >= start + visible_rows {
        start = cursor + 1 - visible_rows;
    }
    *scroll = start as u16;
}

/// Case-insensitive substring filter against `"<provider> <model>"`.
fn filter_models(all: &[(String, String)], query: &str) -> Vec<(String, String)> {
    if query.is_empty() {
        return all.to_vec();
    }
    let q = query.to_lowercase();
    all.iter()
        .filter(|(p, m)| format!("{p} {m}").to_lowercase().contains(&q))
        .cloned()
        .collect()
}

/// Half the popup body height for PageUp / PageDown jumps. Mirrors the popup
/// sizing in `render.rs` (60% of terminal height, minus 2 borders + footer).
fn popup_page_step(state: &ChatState) -> usize {
    let popup_h = (state.dims.rows as usize * 6 / 10).max(8);
    (popup_h.saturating_sub(4) / 2).max(1)
}

fn page_size(state: &ChatState) -> u32 {
    // One viewport minus one row of overlap, browser-style — keeps a row of
    // context across the boundary so the user doesn't lose their place.
    state.transcript_rows().saturating_sub(1).max(1)
}

/// Rows the mouse wheel moves per scroll notch. Three is the usual
/// terminal-convention tick — one wheel click feels like "a bit" but
/// doesn't blow past the screen.
const WHEEL_ROWS_PER_NOTCH: u32 = 3;

/// Handle a `nefor-tui.input.mouse` envelope. Wheel scroll updates the
/// transcript scroll offset; cell-selection (left-button down/drag/up) is
/// handled inside nefor-tui itself — chat just ignores those events here.
fn handle_mouse(map: &Map<String, Value>, state: &mut ChatState) -> Action {
    let action = map.get("action").and_then(Value::as_str);
    match action {
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
    state: &mut ChatState,
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

fn input_submit_body(text: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("chat.input.submit".into()));
    m.insert("text".into(), Value::String(text.to_owned()));
    m
}

fn interrupt_body() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("chat.interrupt".into()));
    m
}

fn interrupt_all_body() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("chat.interrupt_all".into()));
    m
}

fn resume_body(session_id: Option<&str>) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("chat.resume".into()));
    if let Some(id) = session_id {
        m.insert("session_id".into(), Value::String(id.to_owned()));
    }
    m
}

fn login_requested_body(provider: Option<&str>) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("chat.login_requested".into()));
    if let Some(p) = provider {
        m.insert("provider".into(), Value::String(p.to_owned()));
    }
    m
}

fn logout_requested_body(provider: Option<&str>) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String("chat.logout_requested".into()),
    );
    if let Some(p) = provider {
        m.insert("provider".into(), Value::String(p.to_owned()));
    }
    m
}

fn model_list_body(provider: Option<&str>) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String("chat.model.list_requested".into()),
    );
    if let Some(p) = provider {
        m.insert("provider".into(), Value::String(p.to_owned()));
    }
    m
}

fn model_set_body(provider: Option<&str>, model: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("chat.model.set".into()));
    if let Some(p) = provider {
        m.insert("provider".into(), Value::String(p.to_owned()));
    }
    m.insert("model".into(), Value::String(model.to_owned()));
    m
}

fn command_body(name: &str, args: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("chat.command".into()));
    m.insert("name".into(), Value::String(name.to_owned()));
    m.insert("args".into(), Value::String(args.to_owned()));
    m
}

fn chat_reset_body() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("chat.reset".into()));
    m
}

/// Broadcast the user's tool-permission decision back to whichever gate
/// (or other listener) is awaiting. Mirrors the `tool.result` broadcast
/// pattern: the kind is namespace-free so multiple listeners can correlate
/// by `id`. `decision` is `"approve"` or `"deny"` per the gate's contract.
fn tool_permission_response_body(id: &str, decision: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String("tool.permission_response".into()),
    );
    m.insert("id".into(), Value::String(id.to_owned()));
    m.insert("decision".into(), Value::String(decision.to_owned()));
    m
}

/// Address tool-gate to flip its runtime mode. Prefix `tool-gate.*` makes
/// the engine route this only to the gate. `mode` is `"yolo"` or `"normal"`.
fn gate_set_mode_body(mode: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), Value::String("tool-gate.set_mode".into()));
    m.insert("mode".into(), Value::String(mode.to_owned()));
    m
}

/// Build the `dag-scheduler.dag.run` body for the `/dag-test` smoke command.
/// Hardcoded 2-node fan-out: two independent ollama prompts, no edges. The
/// merge node is omitted because `starter/dag_adapter.lua` doesn't splice
/// upstream `inputs` into the prompt — n3 would just see the merge prompt
/// without the actual facts. See the implementation note in the task.
fn dag_test_run_body(run_id: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(
        "kind".into(),
        Value::String("dag-scheduler.dag.run".into()),
    );
    m.insert("run_id".into(), Value::String(run_id.to_owned()));
    let nodes = serde_json::json!([
        {
            "id": "n1",
            "reasoner": "ollama",
            "args": {
                "prompt": "Tell me one fun fact about octopuses. One sentence.",
            },
        },
        {
            "id": "n2",
            "reasoner": "ollama",
            "args": {
                "prompt": "Tell me one fun fact about lighthouses. One sentence.",
            },
        },
    ]);
    let graph = serde_json::json!({
        "nodes": nodes,
        "edges": [],
    });
    m.insert("graph".into(), graph);
    m
}

/// Truncate `s` to at most `max_chars` *characters* (not bytes), appending an
/// ellipsis when truncation actually happened. Used to keep `/dag-test` result
/// rendering compact when a chatty model returns a wall of text.
fn truncate_with_ellipsis(s: &str, max_chars: usize) -> String {
    let mut iter = s.chars();
    let head: String = iter.by_ref().take(max_chars).collect();
    if iter.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// Cap per-node output at this many characters when formatting the
/// `/dag-test` results block. Long enough to read a sentence; short enough
/// that a 2-node block fits comfortably in the transcript.
const DAG_TEST_OUTPUT_CAP: usize = 200;

/// Handle a `dag.run_complete { run_id, status, results }` broadcast — the
/// reply to a `/dag-test`-issued `dag-scheduler.dag.run`. Always clears the
/// matching live-panel entry from `dag_runs` (regardless of whether the run
/// is one of ours). For `/dag-test`-owned runs it additionally formats the
/// results into a system message; for unrelated runs it silently drops.
fn handle_dag_run_complete(map: &Map<String, Value>, state: &mut ChatState) -> Action {
    let Some(run_id) = map.get("run_id").and_then(Value::as_str) else {
        return Action::Continue;
    };

    // Live-panel cleanup happens for every observed run, not just our own
    // `/dag-test` runs. We don't remove the run immediately — instead we
    // stamp `completed_at_ms` so the panel keeps showing the final
    // green/red marker for [`DAG_RUN_LINGER_MS`] (visual feedback that the
    // run finished), then the per-second tick prunes it.
    let now_ms = state.now_ms();
    let panel_changed = if let Some(run) = state.dag_runs.get_mut(run_id) {
        run.completed_at_ms = Some(now_ms);
        true
    } else {
        false
    };

    if !state.pending_dag_runs.contains(run_id) {
        // Not one of our `/dag-test` runs — render only if the panel
        // actually shrank (avoid spurious diffs on unrelated traffic).
        if panel_changed {
            state.bump_transcript_version();
            state.invalidate_row_cache();
            return Action::Render;
        }
        return Action::Continue;
    }
    let status = map
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let mut body = format!("[dag-test] complete (status: {status})");

    if let Some(results) = map.get("results").and_then(Value::as_object) {
        // Stable sorted order so the output is deterministic across hash
        // iteration and easy on the eyes (n1, n2, …).
        let mut keys: Vec<&String> = results.keys().collect();
        keys.sort();
        for key in keys {
            let entry = &results[key];
            let line = format_dag_result_line(key, entry);
            body.push('\n');
            body.push_str("  ");
            body.push_str(&line);
        }
    }

    state.pending_dag_runs.remove(run_id);
    state.push_entry(Role::System, body);
    state.bump_transcript_version();
    if panel_changed {
        state.invalidate_row_cache();
    }
    Action::Render
}

/// Handle `dag.run_started { run_id, total_nodes }` — record an empty live
/// panel entry so observers see "a run with N nodes started" before any
/// dispatches arrive. Idempotent: a duplicate `run_started` for an already-
/// tracked run leaves the existing per-node state in place.
fn handle_dag_run_started(map: &Map<String, Value>, state: &mut ChatState) -> Action {
    let Some(run_id) = map.get("run_id").and_then(Value::as_str) else {
        return Action::Continue;
    };
    let total_nodes = map
        .get("total_nodes")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    if state.dag_runs.contains_key(run_id) {
        return Action::Continue;
    }
    let now_ms = state.now_ms();
    state.dag_runs.insert(
        run_id.to_owned(),
        DagRunUiState {
            run_id: run_id.to_owned(),
            started_at_ms: now_ms,
            total_nodes,
            nodes: BTreeMap::new(),
            completed_at_ms: None,
        },
    );
    state.bump_transcript_version();
    state.invalidate_row_cache();
    Action::Render
}

/// Handle `dag.node_dispatched { run_id, node_id, reasoner }` — mark the
/// node `Running` and stamp its `started_at_ms`. If we missed `run_started`
/// somehow (out-of-order delivery, restart mid-run), create a synthetic run
/// entry so the panel still shows in-flight nodes.
fn handle_dag_node_dispatched(map: &Map<String, Value>, state: &mut ChatState) -> Action {
    let Some(run_id) = map.get("run_id").and_then(Value::as_str) else {
        return Action::Continue;
    };
    let Some(node_id) = map.get("node_id").and_then(Value::as_str) else {
        return Action::Continue;
    };
    let reasoner = map
        .get("reasoner")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();

    let now_ms = state.now_ms();
    let run = state
        .dag_runs
        .entry(run_id.to_owned())
        .or_insert_with(|| DagRunUiState {
            run_id: run_id.to_owned(),
            started_at_ms: now_ms,
            total_nodes: 0,
            nodes: BTreeMap::new(),
            completed_at_ms: None,
        });
    run.nodes.insert(
        node_id.to_owned(),
        DagNodeState {
            reasoner,
            status: DagNodeStatus::Running,
            started_at_ms: now_ms,
            finished_at_ms: None,
        },
    );
    state.bump_transcript_version();
    state.invalidate_row_cache();
    Action::Render
}

/// Handle `dag.node_result { run_id, node_id, output | error }` — flip the
/// node to `Done` or `Error` and stamp `finished_at_ms`. Drops silently when
/// the run isn't tracked (e.g. some other plugin's traffic) or when the
/// node hasn't been dispatched yet (a result arriving before its dispatch
/// would be a scheduler bug, but we log + drop rather than crash).
fn handle_dag_node_result(map: &Map<String, Value>, state: &mut ChatState) -> Action {
    let Some(run_id) = map.get("run_id").and_then(Value::as_str) else {
        return Action::Continue;
    };
    let Some(node_id) = map.get("node_id").and_then(Value::as_str) else {
        return Action::Continue;
    };
    let now_ms = state.now_ms();
    let Some(run) = state.dag_runs.get_mut(run_id) else {
        return Action::Continue;
    };
    let Some(node) = run.nodes.get_mut(node_id) else {
        return Action::Continue;
    };
    node.status = if map.contains_key("output") {
        DagNodeStatus::Done
    } else if map.contains_key("error") {
        DagNodeStatus::Error
    } else {
        // Malformed but harmless — treat as Error so the panel doesn't lie
        // about the node still running.
        DagNodeStatus::Error
    };
    node.finished_at_ms = Some(now_ms);
    state.bump_transcript_version();
    state.invalidate_row_cache();
    Action::Render
}

/// Format one `(node_id, result)` pair for the `/dag-test` results block.
/// Mirrors the dag-scheduler spec's tri-state result shape: `{ output: ... }`,
/// `{ error: ... }`, or `{ skipped: true }`.
fn format_dag_result_line(node_id: &str, entry: &Value) -> String {
    let obj = match entry.as_object() {
        Some(o) => o,
        None => return format!("{node_id}: (malformed result)"),
    };
    if let Some(out) = obj.get("output").and_then(Value::as_str) {
        return format!(
            "{node_id}: {}",
            truncate_with_ellipsis(out, DAG_TEST_OUTPUT_CAP)
        );
    }
    if let Some(err) = obj.get("error").and_then(Value::as_str) {
        return format!(
            "{node_id}: ERROR: {}",
            truncate_with_ellipsis(err, DAG_TEST_OUTPUT_CAP)
        );
    }
    if obj
        .get("skipped")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return format!("{node_id}: [skipped]");
    }
    format!("{node_id}: (unknown result shape)")
}

// `slash_command_registry()` and `slash_command_matches()` live in
// `state.rs` so `render.rs` can derive the help body from the same source.
// Re-imported through the local `state::` use at the top of this file.

/// Sync the slash-autocomplete popup against `state.input`. Opens or updates
/// the popup if `input` starts with `/` and contains no newline AND the user
/// hasn't typed any whitespace after the command-name token; closes the
/// existing slash-autocomplete popup otherwise. Other popup variants are left
/// alone so an open Help / ModelPicker isn't accidentally stomped by typing.
///
/// Whitespace-after-name closes the popup so Enter falls through to the
/// normal Submit path. Without this, Tab on `/login` (which appends a trailing
/// space) leaves the popup open with an empty `matches` list, and Enter does
/// nothing — the popup's enter-handler returns `Continue` for empty matches.
fn refresh_slash_autocomplete(state: &mut ChatState) {
    let input = state.input.as_string();
    let should_show = input.starts_with('/') && !input.contains('\n');
    if !should_show {
        state.close_popup_slash_autocomplete();
        return;
    }
    let after_slash = &input[1..];
    // Any whitespace after the slash means the user has moved past selecting
    // a command and into typing args / committing; let Enter submit through
    // the normal path instead of trapping it in the popup.
    if after_slash.chars().any(char::is_whitespace) {
        state.close_popup_slash_autocomplete();
        return;
    }
    // Only open if no other popup is in the way.
    match state.popup {
        Some(Popup::SlashAutocomplete { .. }) | None => {}
        _ => return,
    }
    let matches = slash_command_matches(after_slash);
    state.open_or_update_popup_slash_autocomplete(matches);
}

/// Parse a slash-command submitted via the input line. Returns `None` only
/// when the text doesn't start with `/` — every `/`-prefixed input becomes
/// some `Command` so the caller intercepts it instead of shipping as a
/// prompt. Unknown commands map to [`Command::Generic`] which the user's
/// Lua config can route however it likes.
fn parse_command(text: &str) -> Option<Command> {
    let trimmed = text.trim();
    let rest = trimmed.strip_prefix('/')?;
    // Bare `/` with nothing after: ignore (treated as no-op via Generic
    // with empty name). The harness shouldn't see that either; let it ship
    // as a generic command and the Lua side can decide.
    let mut parts = rest.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or("").to_owned();
    let args = parts.next().unwrap_or("").trim().to_owned();

    match name.as_str() {
        "resume" => {
            if args.is_empty() {
                Some(Command::ResumeRecent)
            } else {
                Some(Command::ResumeSpecific(args))
            }
        }
        "login" => Some(Command::Login {
            provider: if args.is_empty() { None } else { Some(args) },
        }),
        "logout" => Some(Command::Logout {
            provider: if args.is_empty() { None } else { Some(args) },
        }),
        "model" => {
            if args.is_empty() {
                Some(Command::ModelList)
            } else {
                Some(Command::ModelSet(args))
            }
        }
        "help" => Some(Command::Help),
        "new" | "clear" => Some(Command::New),
        "yolo" => Some(Command::SetGateMode("yolo")),
        "safe" => Some(Command::SetGateMode("normal")),
        "dag-test" => Some(Command::DagTest),
        _ => Some(Command::Generic { name, args }),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    ResumeRecent,
    ResumeSpecific(String),
    Login { provider: Option<String> },
    Logout { provider: Option<String> },
    ModelList,
    ModelSet(String),
    Help,
    /// Start a fresh chat — clear the local transcript and emit `chat.reset`
    /// so the harness/providers drop their conversation history. Silent: no
    /// popup, no system entry.
    New,
    /// Flip the tool-gate runtime mode. `"yolo"` skips every permission
    /// prompt; `"normal"` restores per-tool policy.
    SetGateMode(&'static str),
    /// Submit a hardcoded 2-node parallel DAG to dag-scheduler as a smoke
    /// test. Generates a fresh `run_id`, tracks it in
    /// [`ChatState::pending_dag_runs`], and renders results when the matching
    /// `dag.run_complete` arrives.
    DagTest,
    Generic { name: String, args: String },
}

/// Dispatch a parsed slash-command. Side effects:
///   - emit zero or more outgoing events (login_requested, logout_requested,
///     model.list_requested, model.set, command, resume)
///   - mutate state for purely-local commands (`/help` pushes a system
///     message into the transcript)
async fn handle_command(
    cmd: Command,
    state: &mut ChatState,
    out_tx: &mpsc::Sender<PluginOutgoing>,
) -> Result<(), ChatError> {
    match cmd {
        Command::ResumeRecent => {
            send_event(out_tx, resume_body(None)).await?;
        }
        Command::ResumeSpecific(id) => {
            send_event(out_tx, resume_body(Some(&id))).await?;
        }
        Command::Login { provider } => {
            if state.providers.is_empty() {
                state.open_popup_warning(
                    "login",
                    "No providers connected. Wire one up in starter/init.lua \
                     (see docs/provider-plugins.md).",
                    None,
                );
                return Ok(());
            }
            if let Some(name) = provider.as_deref() {
                if !state.providers.iter().any(|p| p == name) {
                    let connected = state.providers.join(", ");
                    state.open_popup_warning(
                        "login",
                        format!("Unknown provider '{name}'. Connected: {connected}."),
                        None,
                    );
                    return Ok(());
                }
            }
            let target = provider.or_else(|| pick_default_provider(state));
            send_event(out_tx, login_requested_body(target.as_deref())).await?;
        }
        Command::Logout { provider } => {
            if state.providers.is_empty() {
                state.open_popup_warning(
                    "logout",
                    "No providers connected. Wire one up in starter/init.lua \
                     (see docs/provider-plugins.md).",
                    None,
                );
                return Ok(());
            }
            if let Some(name) = provider.as_deref() {
                if !state.providers.iter().any(|p| p == name) {
                    let connected = state.providers.join(", ");
                    state.open_popup_warning(
                        "logout",
                        format!("Unknown provider '{name}'. Connected: {connected}."),
                        None,
                    );
                    return Ok(());
                }
            }
            let target = provider.or_else(|| pick_default_provider(state));
            send_event(out_tx, logout_requested_body(target.as_deref())).await?;
        }
        Command::ModelList => {
            // Aggregate from every connected provider — `awaiting` tracks
            // pending responses so the popup can render a "loading…" footer.
            let connected: Vec<String> = state
                .providers
                .iter()
                .filter(|p| {
                    state
                        .auth_status
                        .get(*p)
                        .is_some_and(|s| s.state == "connected")
                })
                .cloned()
                .collect();
            let awaiting: HashSet<String> = connected.iter().cloned().collect();
            state.open_popup_model_picker(awaiting);
            for p in &connected {
                send_event(out_tx, model_list_body(Some(p))).await?;
            }
        }
        Command::ModelSet(model) => {
            let target = state.active_provider.clone();
            send_event(out_tx, model_set_body(target.as_deref(), &model)).await?;
        }
        Command::Help => {
            state.open_popup_help();
        }
        Command::New => {
            // Local reset: drop the transcript, clear in-flight turn flags so
            // the pending state doesn't survive the reset, and bump the
            // transcript version so the renderer redraws. Silent — no
            // popup, no system entry.
            state.transcript.clear();
            state.pending = false;
            state.awaiting_response_since = None;
            state.awaiting_response_acknowledged = false;
            // Drop in-flight DAG bookkeeping. The user has mentally moved on,
            // and any leftover panel rows or pending-run ids from before the
            // reset would correlate against a transcript that no longer
            // exists. Live `dag.run_complete` events for those orphaned runs
            // still arrive — they get silent-dropped (no matching pending or
            // panel entry) under the same correlate-by-id rule.
            state.dag_runs.clear();
            state.pending_dag_runs.clear();
            // Reset the per-turn telemetry so the statusline doesn't keep
            // showing the old run's ctx/cost/turns/tok-s after the user has
            // mentally moved on. Preserve `model` — that's a property of the
            // active provider, not the session, and gets re-set on the next
            // `chat.model.set_ack` regardless. `stats_seen` flips back to
            // false so the renderer's pre-first-turn branch takes over until
            // the next `chat.session.stats` arrives.
            state.metadata = SessionMetadata {
                model: state.metadata.model.clone(),
                ..SessionMetadata::default()
            };
            state.bump_transcript_version();
            state.invalidate_row_cache();
            // Notify the harness/providers so they drop their conversation
            // history and don't keep streaming into a transcript that no
            // longer exists.
            send_event(out_tx, chat_reset_body()).await?;
        }
        Command::SetGateMode(mode) => {
            send_event(out_tx, gate_set_mode_body(mode)).await?;
        }
        Command::DagTest => {
            // Fresh uuid per invocation so back-to-back `/dag-test`s don't
            // collide on `pending_dag_runs` keys.
            let run_id = uuid::Uuid::new_v4().to_string();
            state.pending_dag_runs.insert(run_id.clone());
            send_event(out_tx, dag_test_run_body(&run_id)).await?;
            // Show 8-char prefix so the system line stays compact while the
            // user can still grep logs by it.
            let short = run_id.get(..8).unwrap_or(run_id.as_str());
            state.push_entry(
                Role::System,
                format!("[dag-test] running 2-node parallel graph (run_id: {short})…"),
            );
            state.bump_transcript_version();
        }
        Command::Generic { name, args } => {
            send_event(out_tx, command_body(&name, &args)).await?;
        }
    }
    Ok(())
}

/// When `/login` / `/logout` is invoked without an explicit provider arg,
/// fall back to the lone connected provider. Returns `None` when there
/// are zero or multiple providers — the receiver decides how to disambiguate.
fn pick_default_provider(state: &ChatState) -> Option<String> {
    if state.providers.len() == 1 {
        state.providers.first().cloned()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::DAG_RUN_LINGER_MS;
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
    fn alt_backspace_deletes_word_back() {
        let mut s = ChatState::new();
        s.input.insert_str("hello world");
        let env = event_env(json!({
            "kind": "nefor-tui.input.key",
            "key": "backspace",
            "modifiers": ["alt"],
        }));
        handle_envelope(env, &mut s);
        assert_eq!(s.input.as_string(), "hello ");
    }

    #[test]
    fn alt_delete_deletes_word_forward() {
        let mut s = ChatState::new();
        s.input.insert_str("hello world");
        s.input.cursor_home();
        let env = event_env(json!({
            "kind": "nefor-tui.input.key",
            "key": "delete",
            "modifiers": ["alt"],
        }));
        handle_envelope(env, &mut s);
        assert_eq!(s.input.as_string(), " world");
    }

    #[test]
    fn alt_left_jumps_word_back() {
        let mut s = ChatState::new();
        s.input.insert_str("hello world");
        let env = event_env(json!({
            "kind": "nefor-tui.input.key",
            "key": "left",
            "modifiers": ["alt"],
        }));
        handle_envelope(env, &mut s);
        assert_eq!(s.input.cursor(), 6);
    }

    #[test]
    fn alt_right_jumps_word_forward() {
        let mut s = ChatState::new();
        s.input.insert_str("hello world");
        s.input.cursor_home();
        let env = event_env(json!({
            "kind": "nefor-tui.input.key",
            "key": "right",
            "modifiers": ["alt"],
        }));
        handle_envelope(env, &mut s);
        assert_eq!(s.input.cursor(), 5);
    }

    #[test]
    fn ctrl_a_jumps_to_start() {
        let mut s = ChatState::new();
        s.input.insert_str("hello");
        let env = event_env(json!({
            "kind": "nefor-tui.input.key",
            "key": "a",
            "modifiers": ["ctrl"],
        }));
        handle_envelope(env, &mut s);
        assert_eq!(s.input.cursor(), 0);
        assert_eq!(s.input.as_string(), "hello");
    }

    #[test]
    fn ctrl_e_jumps_to_end() {
        let mut s = ChatState::new();
        s.input.insert_str("hello");
        s.input.cursor_home();
        let env = event_env(json!({
            "kind": "nefor-tui.input.key",
            "key": "e",
            "modifiers": ["ctrl"],
        }));
        handle_envelope(env, &mut s);
        assert_eq!(s.input.cursor(), 5);
    }

    #[test]
    fn ctrl_u_deletes_to_start() {
        let mut s = ChatState::new();
        s.input.insert_str("hello world");
        let env = event_env(json!({
            "kind": "nefor-tui.input.key",
            "key": "u",
            "modifiers": ["ctrl"],
        }));
        handle_envelope(env, &mut s);
        assert_eq!(s.input.as_string(), "");
    }

    #[test]
    fn ctrl_k_deletes_to_end() {
        let mut s = ChatState::new();
        s.input.insert_str("hello world");
        s.input.cursor_home();
        let env = event_env(json!({
            "kind": "nefor-tui.input.key",
            "key": "k",
            "modifiers": ["ctrl"],
        }));
        handle_envelope(env, &mut s);
        assert_eq!(s.input.as_string(), "");
    }

    #[test]
    fn ctrl_w_deletes_word_back() {
        let mut s = ChatState::new();
        s.input.insert_str("hello world");
        let env = event_env(json!({
            "kind": "nefor-tui.input.key",
            "key": "w",
            "modifiers": ["ctrl"],
        }));
        handle_envelope(env, &mut s);
        assert_eq!(s.input.as_string(), "hello ");
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
    fn chat_stream_delta_appends_assistant() {
        let mut s = ChatState::new();
        let env = event_env(json!({
            "kind": "chat.stream.delta",
            "text": "hello",
        }));
        handle_envelope(env, &mut s);
        assert_eq!(s.transcript.len(), 1);
        assert_eq!(s.transcript[0].role, Role::Assistant);
        assert_eq!(s.transcript[0].text, "hello");
        assert!(s.transcript[0].streaming);
    }

    #[test]
    fn chat_stream_end_finalizes_assistant() {
        let mut s = ChatState::new();
        handle_envelope(
            event_env(json!({"kind":"chat.stream.delta","text":"partial"})),
            &mut s,
        );
        handle_envelope(
            event_env(json!({"kind":"chat.stream.end","text":"FINAL"})),
            &mut s,
        );
        assert_eq!(s.transcript[0].text, "FINAL");
        assert!(!s.transcript[0].streaming);
    }

    #[test]
    fn chat_stream_end_stamps_model_and_duration() {
        let mut s = ChatState::new();
        handle_envelope(
            event_env(json!({"kind":"chat.stream.delta","text":"reply"})),
            &mut s,
        );
        handle_envelope(
            event_env(json!({
                "kind":"chat.stream.end",
                "text":"reply",
                "model":"claude-sonnet-4-6",
                "duration_ms": 12_000_u64,
            })),
            &mut s,
        );
        let last = s.transcript.last().expect("entry");
        assert_eq!(last.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(last.duration_ms, Some(12_000));
    }

    #[test]
    fn chat_stream_end_without_text_keeps_deltas() {
        let mut s = ChatState::new();
        handle_envelope(
            event_env(json!({"kind":"chat.stream.delta","text":"keep"})),
            &mut s,
        );
        handle_envelope(event_env(json!({"kind":"chat.stream.end"})), &mut s);
        assert_eq!(s.transcript[0].text, "keep");
        assert!(!s.transcript[0].streaming);
    }

    #[test]
    fn chat_stream_end_empty_text_keeps_deltas_and_stamps_footer() {
        // Disappearing-body bug repro: mock-plugin's `result.result` is
        // sometimes `""` (e.g. for table-shaped responses where claude's
        // terminal `result` envelope omits the visible reply). The adapter
        // ships that through verbatim, so `chat.stream.end.text=""` lands
        // here. Without guarding against the empty authoritative override,
        // the accumulated deltas were wiped — leaving the footer (model +
        // duration) stamped on an empty assistant body.
        let mut s = ChatState::new();
        handle_envelope(
            event_env(json!({"kind":"chat.stream.delta","text":"| col |\n"})),
            &mut s,
        );
        handle_envelope(
            event_env(json!({"kind":"chat.stream.delta","text":"| --- |\n"})),
            &mut s,
        );
        handle_envelope(
            event_env(json!({"kind":"chat.stream.delta","text":"| val |\n"})),
            &mut s,
        );
        handle_envelope(
            event_env(json!({
                "kind":"chat.stream.end",
                "text":"",
                "model":"claude-opus-4-7",
                "duration_ms": 4_000_u64,
            })),
            &mut s,
        );
        let last = s.transcript.last().expect("entry");
        assert_eq!(
            last.text, "| col |\n| --- |\n| val |\n",
            "deltas must survive an empty stream.end.text"
        );
        assert!(!last.streaming);
        assert_eq!(last.model.as_deref(), Some("claude-opus-4-7"));
        assert_eq!(last.duration_ms, Some(4_000));
    }

    #[test]
    fn gate_mode_changed_to_yolo_sets_gate_yolo() {
        let mut s = ChatState::new();
        assert!(!s.gate_yolo);
        handle_envelope(
            event_env(json!({"kind":"tool-gate.mode_changed","mode":"yolo"})),
            &mut s,
        );
        assert!(s.gate_yolo);
    }

    #[test]
    fn gate_mode_changed_to_normal_clears_gate_yolo() {
        let mut s = ChatState::new();
        s.gate_yolo = true;
        handle_envelope(
            event_env(json!({"kind":"tool-gate.mode_changed","mode":"normal"})),
            &mut s,
        );
        assert!(!s.gate_yolo);
    }

    #[test]
    fn gate_mode_changed_unknown_value_leaves_state_alone() {
        let mut s = ChatState::new();
        s.gate_yolo = true;
        handle_envelope(
            event_env(json!({"kind":"tool-gate.mode_changed","mode":"wat"})),
            &mut s,
        );
        assert!(s.gate_yolo);
    }

    #[test]
    fn chat_tool_start_pushes_tool_entry() {
        let mut s = ChatState::new();
        handle_envelope(
            event_env(json!({
                "kind":"chat.tool.start",
                "id":"toolu_1",
                "name":"Read",
                "input":{"file_path":"/a"}
            })),
            &mut s,
        );
        assert_eq!(s.transcript[0].role, Role::Tool);
        let payload = s.transcript[0]
            .tool
            .as_ref()
            .expect("tool payload present");
        assert_eq!(payload.id, "toolu_1");
        assert_eq!(payload.name, "Read");
        assert!(payload.input_json.contains("/a"));
        assert!(payload.output.is_none());
    }

    #[test]
    fn chat_tool_end_attaches_output_to_matching_id() {
        let mut s = ChatState::new();
        handle_envelope(
            event_env(json!({
                "kind":"chat.tool.start",
                "id":"toolu_1",
                "name":"Bash",
                "input":{"command":"ls"}
            })),
            &mut s,
        );
        handle_envelope(
            event_env(json!({
                "kind":"chat.tool.end",
                "id":"toolu_1",
                "output":"file1\nfile2\n"
            })),
            &mut s,
        );
        let payload = s.transcript[0]
            .tool
            .as_ref()
            .expect("tool payload present");
        assert_eq!(payload.output.as_deref(), Some("file1\nfile2\n"));
        assert!(!payload.error);
    }

    #[test]
    fn chat_history_replay_with_tool_entries_pushes_tool_payloads() {
        let mut s = ChatState::new();
        // Pre-existing transcript content must be cleared on replay.
        s.push_entry(Role::User, "stale".into());
        let env = event_env(json!({
            "kind": "chat.history.replay",
            "session_id": "sess-1",
            "entries": [
                {"role": "user", "text": "do thing"},
                {
                    "role": "tool",
                    "id": "toolu_1",
                    "name": "Bash",
                    "input": {"command": "ls"},
                    "output": "file1\nfile2",
                    "error": false,
                },
                {"role": "assistant", "text": "done"},
            ],
        }));
        handle_envelope(env, &mut s);
        // 3 replayed entries + system "resumed" footer.
        assert_eq!(s.transcript.len(), 4);
        assert_eq!(s.transcript[0].role, Role::User);
        assert_eq!(s.transcript[0].text, "do thing");
        assert_eq!(s.transcript[1].role, Role::Tool);
        let payload = s.transcript[1]
            .tool
            .as_ref()
            .expect("tool payload present");
        assert_eq!(payload.id, "toolu_1");
        assert_eq!(payload.name, "Bash");
        assert!(payload.input_json.contains("ls"));
        assert_eq!(payload.output.as_deref(), Some("file1\nfile2"));
        assert!(!payload.error);
        assert_eq!(s.transcript[2].role, Role::Assistant);
        assert_eq!(s.transcript[2].text, "done");
        assert_eq!(s.transcript[3].role, Role::System);
        assert!(s.transcript[3].text.contains("3 messages"));
    }

    #[test]
    fn chat_history_replay_tool_with_output_renders_collapsed_with_command() {
        // The replay handler should produce a Tool entry whose payload
        // matches what live `chat.tool.start` + `chat.tool.end` would yield —
        // same id/name/input/output/error fields, ready for the collapsed-
        // tool renderer.
        let mut s = ChatState::new();
        let env = event_env(json!({
            "kind": "chat.history.replay",
            "session_id": "sess-x",
            "entries": [
                {
                    "role": "tool",
                    "id": "toolu_X",
                    "name": "Read",
                    "input": {"file_path": "/etc/hosts"},
                    "output": "127.0.0.1 localhost\n",
                    "error": false,
                }
            ],
        }));
        handle_envelope(env, &mut s);
        // Tool entry + system footer.
        assert_eq!(s.transcript.len(), 2);
        assert_eq!(s.transcript[0].role, Role::Tool);
        let payload = s.transcript[0]
            .tool
            .as_ref()
            .expect("tool payload present");
        assert_eq!(payload.id, "toolu_X");
        assert_eq!(payload.name, "Read");
        assert!(payload.input_json.contains("/etc/hosts"));
        assert_eq!(payload.output.as_deref(), Some("127.0.0.1 localhost\n"));
    }

    #[test]
    fn chat_history_replay_tool_without_output_keeps_pending() {
        // Truncated session: tool_use without a paired tool_result.
        let mut s = ChatState::new();
        let env = event_env(json!({
            "kind": "chat.history.replay",
            "session_id": "sess-x",
            "entries": [
                {
                    "role": "tool",
                    "id": "toolu_unmatched",
                    "name": "Bash",
                    "input": {"command": "sleep 100"},
                    "output": null,
                    "error": false,
                }
            ],
        }));
        handle_envelope(env, &mut s);
        let payload = s.transcript[0]
            .tool
            .as_ref()
            .expect("tool payload present");
        assert!(payload.output.is_none());
    }

    #[test]
    fn ctrl_o_toggles_global_expansion_and_bumps_version() {
        let mut s = ChatState::new();
        let v0 = s.transcript_version;
        assert!(!s.tools_expanded_global);
        let env = event_env(json!({
            "kind":"nefor-tui.input.key",
            "key":"o",
            "modifiers":["ctrl"],
        }));
        let a = handle_envelope(env, &mut s);
        assert!(matches!(a, Action::Render));
        assert!(s.tools_expanded_global);
        assert_ne!(s.transcript_version, v0);
        let v1 = s.transcript_version;
        let env = event_env(json!({
            "kind":"nefor-tui.input.key",
            "key":"o",
            "modifiers":["ctrl"],
        }));
        handle_envelope(env, &mut s);
        assert!(!s.tools_expanded_global);
        assert_ne!(s.transcript_version, v1);
    }

    #[test]
    fn ctrl_b_toggles_sidebar_visibility_and_renders() {
        // Mirrors the Ctrl+O test: dispatch a `ctrl+b` key event, expect
        // an `Action::Render` and a flipped `sidebar_visible` flag. Two
        // presses should round-trip back to the original state.
        let mut s = ChatState::new();
        assert!(s.sidebar_visible, "sidebar starts visible by default");
        let env = event_env(json!({
            "kind":"nefor-tui.input.key",
            "key":"b",
            "modifiers":["ctrl"],
        }));
        let a = handle_envelope(env, &mut s);
        assert!(matches!(a, Action::Render));
        assert!(!s.sidebar_visible, "Ctrl-B must toggle sidebar off");

        let env = event_env(json!({
            "kind":"nefor-tui.input.key",
            "key":"b",
            "modifiers":["ctrl"],
        }));
        let a = handle_envelope(env, &mut s);
        assert!(matches!(a, Action::Render));
        assert!(s.sidebar_visible, "Ctrl-B must toggle sidebar back on");
    }

    #[test]
    fn plain_b_typed_into_buffer_does_not_toggle_sidebar() {
        // Bare `b` (no ctrl modifier) is regular typing — it goes into the
        // input buffer and the sidebar flag must not change.
        let mut s = ChatState::new();
        assert!(s.sidebar_visible);
        let env = event_env(json!({
            "kind":"nefor-tui.input.key",
            "key":"b",
            "modifiers":[],
        }));
        let a = handle_envelope(env, &mut s);
        assert!(matches!(a, Action::Render));
        assert!(s.sidebar_visible, "plain `b` must not toggle sidebar");
        assert_eq!(s.input.as_string(), "b");
    }

    #[test]
    fn chat_message_append_pushes_entry() {
        let mut s = ChatState::new();
        handle_envelope(
            event_env(json!({"kind":"chat.message.append","role":"user","text":"hi"})),
            &mut s,
        );
        assert_eq!(s.transcript[0].role, Role::User);
        assert_eq!(s.transcript[0].text, "hi");
    }

    #[test]
    fn chat_session_stats_updates_metadata() {
        let mut s = ChatState::new();
        handle_envelope(
            event_env(json!({
                "kind":"chat.session.stats",
                "model":"claude-opus-4-7",
                "turns": 3,
                "cumulative_cost_usd": 0.42,
            })),
            &mut s,
        );
        assert!(s.metadata.stats_seen);
        assert_eq!(s.metadata.model.as_deref(), Some("claude-opus-4-7"));
        assert_eq!(s.metadata.turns, Some(3));
        assert_eq!(s.metadata.cumulative_cost_usd, Some(0.42));
    }

    #[test]
    fn pageup_advances_by_transcript_rows_minus_one() {
        let mut s = ChatState::new();
        s.dims = state::Dims { cols: 80, rows: 24 };
        for i in 0..50 {
            s.push_entry(Role::User, format!("{i}"));
        }
        let expected = s.transcript_rows().saturating_sub(1).max(1);
        let env = event_env(json!({
            "kind":"nefor-tui.input.key",
            "key":"pageup",
            "modifiers":[],
        }));
        handle_envelope(env, &mut s);
        assert_eq!(s.scroll_offset, expected);
    }

    #[test]
    fn pagedown_with_small_transcript_does_not_overshoot() {
        // Tiny terminal where transcript_rows is small; PageDown without a
        // prior PageUp clamps at 0 (saturating_sub) and doesn't underflow.
        let mut s = ChatState::new();
        s.dims = state::Dims { cols: 40, rows: 8 };
        for i in 0..3 {
            s.push_entry(Role::User, format!("{i}"));
        }
        let env = event_env(json!({
            "kind":"nefor-tui.input.key",
            "key":"pagedown",
            "modifiers":[],
        }));
        handle_envelope(env, &mut s);
        assert_eq!(s.scroll_offset, 0);
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
    fn toast_expires_after_duration() {
        // Open a toast with a duration in the past; toast_expired_at returns
        // true so the main loop's tick handler clears it on the next pass.
        let mut s = ChatState::new();
        s.open_popup_toast("Copied 5 chars", Duration::from_millis(0));
        assert!(matches!(s.popup, Some(Popup::Toast { .. })));
        // Sleep for one millisecond to make sure expires_at <= now.
        std::thread::sleep(Duration::from_millis(2));
        assert!(s.toast_expired_at(Instant::now()));
        s.close_popup();
        assert!(s.popup.is_none());
    }

    #[test]
    fn toast_does_not_expire_before_duration() {
        let mut s = ChatState::new();
        s.open_popup_toast("Copied 5 chars", Duration::from_secs(60));
        assert!(matches!(s.popup, Some(Popup::Toast { .. })));
        assert!(!s.toast_expired_at(Instant::now()));
    }

    #[test]
    fn pageup_disables_auto_follow_and_scrolls_up() {
        // PageUp moves scroll_offset > 0; subsequent stream deltas must NOT
        // pull the user back to the bottom (state-level: offset stays put;
        // render-level compensation is covered separately).
        let mut s = ChatState::new();
        s.dims = state::Dims { cols: 80, rows: 24 };
        for i in 0..50 {
            s.push_entry(Role::User, format!("{i}"));
        }
        let expected = s.transcript_rows().saturating_sub(1).max(1);
        let env = event_env(json!({
            "kind":"nefor-tui.input.key",
            "key":"pageup",
            "modifiers":[],
        }));
        handle_envelope(env, &mut s);
        assert_eq!(s.scroll_offset, expected);

        s.append_assistant_delta("streaming response");
        assert_eq!(s.scroll_offset, expected, "delta must not snap back to bottom");
    }

    #[test]
    fn pagedown_to_bottom_re_enables_auto_follow() {
        // After scrolling up, repeated PageDowns saturate at 0; once at 0
        // a new entry keeps the user pinned to the bottom (offset stays 0).
        let mut s = ChatState::new();
        s.dims = state::Dims { cols: 80, rows: 24 };
        for i in 0..50 {
            s.push_entry(Role::User, format!("{i}"));
        }
        s.scroll_up(5);
        assert_eq!(s.scroll_offset, 5);

        let env = event_env(json!({
            "kind":"nefor-tui.input.key",
            "key":"pagedown",
            "modifiers":[],
        }));
        handle_envelope(env, &mut s);
        assert_eq!(s.scroll_offset, 0, "PageDown returns to bottom");

        s.append_assistant_delta("new content");
        assert_eq!(s.scroll_offset, 0, "auto-follow re-engaged");
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
    fn input_submit_body_carries_text() {
        let b = input_submit_body("go");
        assert_eq!(b["kind"], Value::String("chat.input.submit".into()));
        assert_eq!(b["text"], Value::String("go".into()));
    }

    #[test]
    fn resume_body_without_id_omits_field() {
        let b = resume_body(None);
        assert_eq!(b["kind"], Value::String("chat.resume".into()));
        assert!(b.get("session_id").is_none());
    }

    #[test]
    fn resume_body_with_id_includes_field() {
        let b = resume_body(Some("abc"));
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
    fn parse_command_non_slash_returns_none() {
        // Non-slash inputs always fall through to the regular submit path.
        assert_eq!(parse_command("hello"), None);
        assert_eq!(parse_command(""), None);
        assert_eq!(parse_command("  "), None);
    }

    #[test]
    fn parse_command_unknown_slash_emits_generic() {
        // Unknown commands still parse — the user's Lua config decides what
        // (if anything) to do with the resulting `chat.command` event.
        assert_eq!(
            parse_command("/think step by step"),
            Some(Command::Generic {
                name: "think".into(),
                args: "step by step".into(),
            })
        );
    }

    #[test]
    fn esc_during_live_turn_yields_interrupt() {
        let mut s = ChatState::new();
        s.arm_watchdog();
        let env = event_env(json!({
            "kind": "nefor-tui.input.key",
            "key": "escape",
            "modifiers": [],
        }));
        let a = handle_envelope(env, &mut s);
        assert!(matches!(a, Action::Interrupt));
    }

    #[test]
    fn esc_with_no_live_turn_is_noop() {
        let mut s = ChatState::new();
        let env = event_env(json!({
            "kind": "nefor-tui.input.key",
            "key": "escape",
            "modifiers": [],
        }));
        let a = handle_envelope(env, &mut s);
        assert!(matches!(a, Action::Continue));
    }

    #[test]
    fn interrupt_body_kind_only() {
        let b = interrupt_body();
        assert_eq!(b["kind"], Value::String("chat.interrupt".into()));
        assert_eq!(b.len(), 1);
    }

    #[test]
    fn empty_assistant_message_append_is_dropped() {
        let mut s = ChatState::new();
        let env = event_env(json!({
            "kind": "chat.message.append",
            "role": "assistant",
            "text": "",
        }));
        let a = handle_envelope(env, &mut s);
        assert!(matches!(a, Action::Continue));
        assert!(s.transcript.is_empty());
    }

    #[test]
    fn empty_user_message_append_is_dropped() {
        let mut s = ChatState::new();
        let env = event_env(json!({
            "kind": "chat.message.append",
            "role": "user",
            "text": "",
        }));
        let a = handle_envelope(env, &mut s);
        assert!(matches!(a, Action::Continue));
        assert!(s.transcript.is_empty());
    }

    #[test]
    fn empty_system_message_append_is_dropped() {
        let mut s = ChatState::new();
        let env = event_env(json!({
            "kind": "chat.message.append",
            "role": "system",
            "text": "",
        }));
        let a = handle_envelope(env, &mut s);
        assert!(matches!(a, Action::Continue));
        assert!(s.transcript.is_empty());
    }

    #[test]
    fn nonempty_assistant_message_append_pushes_entry() {
        let mut s = ChatState::new();
        let env = event_env(json!({
            "kind": "chat.message.append",
            "role": "assistant",
            "text": "hello",
        }));
        let a = handle_envelope(env, &mut s);
        assert!(matches!(a, Action::Render));
        assert_eq!(s.transcript.len(), 1);
        assert_eq!(s.transcript[0].role, Role::Assistant);
        assert_eq!(s.transcript[0].text, "hello");
    }

    #[test]
    fn up_arrow_with_empty_input_recalls_latest_prompt() {
        let mut s = ChatState::new();
        s.push_history("first".into());
        s.push_history("second".into());
        let env = event_env(json!({
            "kind": "nefor-tui.input.key",
            "key": "up",
            "modifiers": [],
        }));
        let a = handle_envelope(env, &mut s);
        assert!(matches!(a, Action::Render));
        assert_eq!(s.input.as_string(), "second");
    }

    #[test]
    fn up_arrow_with_nonempty_input_is_noop() {
        let mut s = ChatState::new();
        s.push_history("recorded".into());
        s.input.insert_str("typing");
        let env = event_env(json!({
            "kind": "nefor-tui.input.key",
            "key": "up",
            "modifiers": [],
        }));
        let a = handle_envelope(env, &mut s);
        assert!(matches!(a, Action::Continue));
        assert_eq!(s.input.as_string(), "typing");
    }

    #[test]
    fn up_then_up_walks_older_in_main() {
        let mut s = ChatState::new();
        s.push_history("a".into());
        s.push_history("b".into());
        for _ in 0..2 {
            let env = event_env(json!({
                "kind": "nefor-tui.input.key",
                "key": "up",
                "modifiers": [],
            }));
            handle_envelope(env, &mut s);
        }
        assert_eq!(s.input.as_string(), "a");
    }

    #[test]
    fn down_after_up_returns_to_empty_input() {
        let mut s = ChatState::new();
        s.push_history("only".into());
        // Up to recall, then Down to return to empty.
        let up = event_env(json!({
            "kind": "nefor-tui.input.key",
            "key": "up",
            "modifiers": [],
        }));
        handle_envelope(up, &mut s);
        assert_eq!(s.input.as_string(), "only");
        let down = event_env(json!({
            "kind": "nefor-tui.input.key",
            "key": "down",
            "modifiers": [],
        }));
        let a = handle_envelope(down, &mut s);
        assert!(matches!(a, Action::Render));
        assert_eq!(s.input.as_string(), "");
        assert!(s.history_cursor.is_none());
    }

    #[test]
    fn synthetic_stream_end_after_interrupt_clears_awaiting_state() {
        // ESC during a live turn yields Action::Interrupt; the main loop
        // then acks the watchdog and ships chat.interrupt. The harness
        // confirms the abort with a synthetic chat.stream.end (text=""),
        // which must drive the local state back to fully idle.
        let mut s = ChatState::new();
        s.arm_watchdog();
        s.append_assistant_delta("partial...");
        // Simulate the Action::Interrupt acknowledgement step.
        s.acknowledge_response();
        assert!(s.awaiting_response_acknowledged);

        // Synthetic stream.end (mock-plugin's interrupt path).
        let env = event_env(json!({"kind": "chat.stream.end", "text": ""}));
        handle_envelope(env, &mut s);
        assert!(s.awaiting_response_since.is_none());
        assert!(!s.pending);
        // Partial deltas survived the empty authoritative override.
        assert_eq!(s.transcript[0].text, "partial...");
        assert!(!s.transcript[0].streaming);
    }

    #[test]
    fn shift_enter_inserts_newline() {
        let mut s = ChatState::new();
        s.input.insert_str("hi");
        let env = event_env(json!({
            "kind": "nefor-tui.input.key",
            "key": "enter",
            "modifiers": ["shift"],
        }));
        let a = handle_envelope(env, &mut s);
        assert!(matches!(a, Action::Render));
        assert_eq!(s.input.as_string(), "hi\n");
    }

    #[test]
    fn enter_alone_submits() {
        let mut s = ChatState::new();
        s.input.insert_str("hello");
        let env = event_env(json!({
            "kind": "nefor-tui.input.key",
            "key": "enter",
            "modifiers": [],
        }));
        let a = handle_envelope(env, &mut s);
        assert!(matches!(a, Action::SubmitPrompt(t) if t == "hello"));
    }

    #[test]
    fn paste_with_newlines_preserves_them() {
        let mut s = ChatState::new();
        let env = event_env(json!({
            "kind": "nefor-tui.input.paste",
            "text": "line1\nline2\nline3",
        }));
        handle_envelope(env, &mut s);
        assert_eq!(s.input.as_string(), "line1\nline2\nline3");
    }

    #[test]
    fn up_with_multiline_moves_cursor_within_buffer() {
        let mut s = ChatState::new();
        s.input.insert_str("hello\nworld");
        // cursor at end of "world" (row 1, col 5)
        assert_eq!(s.input.cursor_row(), 1);
        let env = event_env(json!({
            "kind": "nefor-tui.input.key",
            "key": "up",
            "modifiers": [],
        }));
        let a = handle_envelope(env, &mut s);
        assert!(matches!(a, Action::Render));
        assert_eq!(s.input.cursor_row(), 0);
    }

    #[test]
    fn up_at_top_of_multiline_is_noop_not_history() {
        let mut s = ChatState::new();
        s.push_history("recorded".into());
        s.input.insert_str("a\nb");
        s.input.cursor_home(); // cursor at row 0, col 0
        let env = event_env(json!({
            "kind": "nefor-tui.input.key",
            "key": "up",
            "modifiers": [],
        }));
        let a = handle_envelope(env, &mut s);
        // Render (cursor nav attempted, no-op succeeds silently),
        // and crucially the buffer is NOT replaced by history.
        assert!(matches!(a, Action::Render));
        assert_eq!(s.input.as_string(), "a\nb");
    }

    #[test]
    fn chat_history_replay_stamps_model_on_assistant_entries() {
        let mut s = ChatState::new();
        let env = event_env(json!({
            "kind": "chat.history.replay",
            "session_id": "sess-m",
            "entries": [
                {"role": "user", "text": "hi"},
                {"role": "assistant", "text": "hello", "model": "claude-opus-4-7"},
            ],
        }));
        handle_envelope(env, &mut s);
        // 2 replayed + system footer = 3.
        assert_eq!(s.transcript.len(), 3);
        assert_eq!(s.transcript[0].role, Role::User);
        assert!(s.transcript[0].model.is_none(), "user has no model");
        assert_eq!(s.transcript[1].role, Role::Assistant);
        assert_eq!(
            s.transcript[1].model.as_deref(),
            Some("claude-opus-4-7"),
            "assistant must carry model from replay"
        );
        // Duration not reported in replay → footer renders model-only.
        assert!(s.transcript[1].duration_ms.is_none());
    }

    #[test]
    fn up_with_empty_buffer_recalls_history_regression() {
        let mut s = ChatState::new();
        s.push_history("recorded".into());
        let env = event_env(json!({
            "kind": "nefor-tui.input.key",
            "key": "up",
            "modifiers": [],
        }));
        let a = handle_envelope(env, &mut s);
        assert!(matches!(a, Action::Render));
        assert_eq!(s.input.as_string(), "recorded");
    }

    // ---- slash-command parsing & dispatch -----------------------------------

    /// Drive a parsed slash-command through `handle_command` and collect every
    /// outgoing event body it emits. Uses a tiny tokio runtime so the rest of
    /// the test stays synchronous.
    fn drive_command(cmd: Command, state: &mut ChatState) -> Vec<Map<String, Value>> {
        let (tx, mut rx) = mpsc::channel::<PluginOutgoing>(8);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            handle_command(cmd, state, &tx).await.expect("handle_command");
        });
        drop(tx);
        let mut out = Vec::new();
        while let Ok(env) = rx.try_recv() {
            if let Body::Event(map) = env.body {
                out.push(map);
            }
        }
        out
    }

    #[test]
    fn parse_command_login_with_no_arg() {
        assert_eq!(
            parse_command("/login"),
            Some(Command::Login { provider: None })
        );
    }

    #[test]
    fn parse_command_login_with_arg() {
        assert_eq!(
            parse_command("/login anthropic"),
            Some(Command::Login {
                provider: Some("anthropic".into())
            })
        );
    }

    #[test]
    fn parse_command_logout_with_arg() {
        assert_eq!(
            parse_command("/logout ollama"),
            Some(Command::Logout {
                provider: Some("ollama".into())
            })
        );
    }

    #[test]
    fn parse_command_model_with_no_arg() {
        assert_eq!(parse_command("/model"), Some(Command::ModelList));
    }

    #[test]
    fn parse_command_model_with_arg() {
        assert_eq!(
            parse_command("/model claude-opus-4-7"),
            Some(Command::ModelSet("claude-opus-4-7".into()))
        );
    }

    #[test]
    fn parse_command_help() {
        assert_eq!(parse_command("/help"), Some(Command::Help));
    }

    #[test]
    fn parse_command_yolo_and_safe() {
        assert_eq!(parse_command("/yolo"), Some(Command::SetGateMode("yolo")));
        assert_eq!(parse_command("/safe"), Some(Command::SetGateMode("normal")));
        assert_eq!(parse_command("  /yolo  "), Some(Command::SetGateMode("yolo")));
    }

    #[test]
    fn slash_yolo_emits_gate_set_mode_yolo() {
        let mut s = ChatState::new();
        let bodies = drive_command(Command::SetGateMode("yolo"), &mut s);
        assert_eq!(bodies.len(), 1);
        assert_eq!(bodies[0]["kind"], "tool-gate.set_mode");
        assert_eq!(bodies[0]["mode"], "yolo");
    }

    #[test]
    fn slash_safe_emits_gate_set_mode_normal() {
        let mut s = ChatState::new();
        let bodies = drive_command(Command::SetGateMode("normal"), &mut s);
        assert_eq!(bodies.len(), 1);
        assert_eq!(bodies[0]["kind"], "tool-gate.set_mode");
        assert_eq!(bodies[0]["mode"], "normal");
    }

    // ---- /dag-test smoke command -------------------------------------------

    #[test]
    fn parse_command_dag_test() {
        assert_eq!(parse_command("/dag-test"), Some(Command::DagTest));
        assert_eq!(parse_command("  /dag-test  "), Some(Command::DagTest));
    }

    #[test]
    fn slash_dag_test_emits_dag_run_with_two_node_graph() {
        let mut s = ChatState::new();
        let bodies = drive_command(Command::DagTest, &mut s);
        assert_eq!(bodies.len(), 1, "exactly one outgoing event");
        let body = &bodies[0];
        assert_eq!(body["kind"], "dag-scheduler.dag.run");
        let run_id = body["run_id"].as_str().expect("run_id is a string");
        // uuid v4 hyphenated form is exactly 36 chars (8-4-4-4-12 + 4 hyphens).
        assert_eq!(run_id.len(), 36, "run_id must be a uuid-shaped string");
        let graph = body["graph"].as_object().expect("graph object");
        let nodes = graph["nodes"].as_array().expect("nodes array");
        assert_eq!(nodes.len(), 2, "v1 ships only the parallel pair (no fan-in)");
        for n in nodes {
            assert_eq!(
                n["reasoner"], "ollama",
                "both nodes must address the ollama provider (matches starter/init.lua)"
            );
        }
        let edges = graph["edges"].as_array().expect("edges array");
        assert!(edges.is_empty(), "no edges in the parallel-only v1 graph");
    }

    #[test]
    fn slash_dag_test_inserts_run_id_into_pending() {
        let mut s = ChatState::new();
        let bodies = drive_command(Command::DagTest, &mut s);
        let run_id = bodies[0]["run_id"]
            .as_str()
            .expect("run_id is a string")
            .to_owned();
        assert_eq!(s.pending_dag_runs.len(), 1);
        assert!(
            s.pending_dag_runs.contains(&run_id),
            "the dispatched run_id must be tracked"
        );
    }

    #[test]
    fn slash_dag_test_appends_running_system_message() {
        // The user should see immediate feedback that the run is in flight,
        // not a silent dispatch.
        let mut s = ChatState::new();
        let _ = drive_command(Command::DagTest, &mut s);
        let last = s.transcript.last().expect("transcript has an entry");
        assert_eq!(last.role, Role::System);
        assert!(
            last.text.starts_with("[dag-test] running"),
            "expected running banner, got: {}",
            last.text
        );
    }

    #[test]
    fn dag_run_complete_for_pending_run_renders_results() {
        let mut s = ChatState::new();
        let run_id = "11111111-2222-4333-8444-555555555555".to_owned();
        s.pending_dag_runs.insert(run_id.clone());
        let env = event_env(json!({
            "kind": "graph.run_complete",
            "run_id": run_id,
            "status": "success",
            "results": {
                "n1": { "output": "Octopuses have three hearts." },
                "n2": { "output": "Lighthouses warn ships at night." },
            },
        }));
        let action = handle_envelope(env, &mut s);
        assert!(matches!(action, Action::Render));
        assert!(
            !s.pending_dag_runs.contains(&run_id),
            "completed run must be removed from pending set"
        );
        let last = s.transcript.last().expect("transcript has an entry");
        assert_eq!(last.role, Role::System);
        assert!(
            last.text.contains("status: success"),
            "expected status line, got: {}",
            last.text
        );
        assert!(last.text.contains("Octopuses have three hearts."));
        assert!(last.text.contains("Lighthouses warn ships at night."));
        assert!(last.text.contains("n1:"));
        assert!(last.text.contains("n2:"));
    }

    #[test]
    fn dag_run_complete_for_unknown_run_id_is_dropped() {
        let mut s = ChatState::new();
        s.pending_dag_runs
            .insert("aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".to_owned());
        let v0 = s.transcript_version;
        let len0 = s.transcript.len();
        let env = event_env(json!({
            "kind": "graph.run_complete",
            "run_id": "ffffffff-0000-4000-8000-000000000000",
            "status": "success",
            "results": { "n1": { "output": "x" } },
        }));
        let action = handle_envelope(env, &mut s);
        assert!(matches!(action, Action::Continue));
        assert_eq!(
            s.transcript.len(),
            len0,
            "transcript must not grow on a non-matching run_id"
        );
        assert_eq!(
            s.transcript_version, v0,
            "version must not bump on a non-matching run_id"
        );
        // The unrelated pending run must still be tracked.
        assert_eq!(s.pending_dag_runs.len(), 1);
    }

    #[test]
    fn dag_run_complete_renders_errors_and_skipped() {
        let mut s = ChatState::new();
        let run_id = "abcdef01-2345-4678-89ab-cdef01234567".to_owned();
        s.pending_dag_runs.insert(run_id.clone());
        let env = event_env(json!({
            "kind": "graph.run_complete",
            "run_id": run_id,
            "status": "failure",
            "results": {
                "n1": { "error": "ollama: connection refused" },
                "n2": { "skipped": true },
            },
        }));
        handle_envelope(env, &mut s);
        let last = s.transcript.last().expect("transcript has an entry");
        assert!(
            last.text.contains("n1: ERROR: ollama: connection refused"),
            "errored node must render with ERROR: prefix, got: {}",
            last.text
        );
        assert!(
            last.text.contains("n2: [skipped]"),
            "skipped node must render with [skipped], got: {}",
            last.text
        );
        assert!(last.text.contains("status: failure"));
    }

    #[test]
    fn dag_run_complete_truncates_long_outputs() {
        let mut s = ChatState::new();
        let run_id = "ddddddee-eeee-4eee-8eee-eeeeeeeeeeee".to_owned();
        s.pending_dag_runs.insert(run_id.clone());
        let big = "x".repeat(1000);
        let env = event_env(json!({
            "kind": "graph.run_complete",
            "run_id": run_id,
            "status": "success",
            "results": { "n1": { "output": big.clone() } },
        }));
        handle_envelope(env, &mut s);
        let last = s.transcript.last().expect("transcript has an entry");
        assert!(
            last.text.chars().any(|c| c == '…'),
            "long outputs must be truncated with an ellipsis, got: {}",
            last.text
        );
        assert!(
            last.text.len() < big.len(),
            "rendered system message must be shorter than the raw 1000-char output"
        );
    }

    #[test]
    fn dag_run_started_inserts_run_into_dag_runs() {
        let mut s = ChatState::new();
        let env = event_env(json!({
            "kind": "graph.run_started",
            "run_id": "run-aaa",
            "total_nodes": 3,
        }));
        let action = handle_envelope(env, &mut s);
        assert!(matches!(action, Action::Render));
        let entry = s.dag_runs.get("run-aaa").expect("run inserted");
        assert_eq!(entry.run_id, "run-aaa");
        assert_eq!(entry.total_nodes, 3);
        assert!(entry.nodes.is_empty());
    }

    #[test]
    fn dag_node_dispatched_marks_node_running() {
        let mut s = ChatState::new();
        let _ = handle_envelope(
            event_env(json!({
                "kind": "graph.run_started",
                "run_id": "run-bbb",
                "total_nodes": 2,
            })),
            &mut s,
        );
        let env = event_env(json!({
            "kind": "graph.node_dispatched",
            "run_id": "run-bbb",
            "node_id": "n1",
            "reasoner": "ollama",
        }));
        let action = handle_envelope(env, &mut s);
        assert!(matches!(action, Action::Render));
        let entry = s.dag_runs.get("run-bbb").expect("run tracked");
        let node = entry.nodes.get("n1").expect("n1 recorded");
        assert_eq!(node.reasoner, "ollama");
        assert_eq!(node.status, DagNodeStatus::Running);
        assert!(node.finished_at_ms.is_none());
    }

    #[test]
    fn dag_node_dispatched_without_run_started_synthesises_run() {
        // Out-of-order delivery: dispatched arrives before run_started.
        // The handler should still record the node so the panel shows
        // something, with total_nodes=0 until run_started fills it in.
        let mut s = ChatState::new();
        let env = event_env(json!({
            "kind": "graph.node_dispatched",
            "run_id": "run-orph",
            "node_id": "n7",
            "reasoner": "ollama",
        }));
        let _ = handle_envelope(env, &mut s);
        let entry = s.dag_runs.get("run-orph").expect("synthetic run created");
        assert_eq!(entry.total_nodes, 0);
        assert!(entry.nodes.contains_key("n7"));
    }

    #[test]
    fn dag_node_result_with_output_marks_done() {
        let mut s = ChatState::new();
        let _ = handle_envelope(
            event_env(json!({
                "kind": "graph.run_started",
                "run_id": "run-ccc",
                "total_nodes": 1,
            })),
            &mut s,
        );
        let _ = handle_envelope(
            event_env(json!({
                "kind": "graph.node_dispatched",
                "run_id": "run-ccc",
                "node_id": "n1",
                "reasoner": "ollama",
            })),
            &mut s,
        );
        let env = event_env(json!({
            "kind": "graph.node_result",
            "run_id": "run-ccc",
            "node_id": "n1",
            "output": "ok",
        }));
        let action = handle_envelope(env, &mut s);
        assert!(matches!(action, Action::Render));
        let node = s
            .dag_runs
            .get("run-ccc")
            .and_then(|r| r.nodes.get("n1"))
            .expect("n1 in panel");
        assert_eq!(node.status, DagNodeStatus::Done);
        assert!(node.finished_at_ms.is_some());
    }

    #[test]
    fn dag_node_result_with_error_marks_error() {
        let mut s = ChatState::new();
        let _ = handle_envelope(
            event_env(json!({
                "kind": "graph.run_started",
                "run_id": "run-ddd",
                "total_nodes": 1,
            })),
            &mut s,
        );
        let _ = handle_envelope(
            event_env(json!({
                "kind": "graph.node_dispatched",
                "run_id": "run-ddd",
                "node_id": "n1",
                "reasoner": "ollama",
            })),
            &mut s,
        );
        let env = event_env(json!({
            "kind": "graph.node_result",
            "run_id": "run-ddd",
            "node_id": "n1",
            "error": "timeout",
        }));
        let _ = handle_envelope(env, &mut s);
        let node = s
            .dag_runs
            .get("run-ddd")
            .and_then(|r| r.nodes.get("n1"))
            .expect("n1 in panel");
        assert_eq!(node.status, DagNodeStatus::Error);
        assert!(node.finished_at_ms.is_some());
    }

    #[test]
    fn dag_run_complete_clears_dag_runs_entry() {
        let mut s = ChatState::new();
        // Use a run id that's tracked via the panel but is also one of our
        // pending /dag-test runs so the existing complete-handler still fires.
        let run_id = "11111111-2222-4333-8444-555555555555".to_owned();
        s.pending_dag_runs.insert(run_id.clone());
        let _ = handle_envelope(
            event_env(json!({
                "kind": "graph.run_started",
                "run_id": run_id.clone(),
                "total_nodes": 1,
            })),
            &mut s,
        );
        assert!(s.dag_runs.contains_key(&run_id));
        let env = event_env(json!({
            "kind": "graph.run_complete",
            "run_id": run_id,
            "status": "success",
            "results": { "n1": { "output": "ok" } },
        }));
        let _ = handle_envelope(env, &mut s);
        // Run lingers in the panel for `DAG_RUN_LINGER_MS` so the user gets
        // visual confirmation; `completed_at_ms` is stamped now and the
        // tick-driven prune drops it later.
        let run = s
            .dag_runs
            .get(&run_id)
            .expect("run must linger in panel after run_complete");
        assert!(
            run.completed_at_ms.is_some(),
            "completed_at_ms must be stamped for prune to find it"
        );
        // After the linger window, prune empties the panel.
        let now_after_linger = run.completed_at_ms.unwrap() + DAG_RUN_LINGER_MS + 1;
        assert!(s.prune_finished_dag_runs(now_after_linger));
        assert!(s.dag_runs.is_empty(), "panel must clear after linger");
    }

    #[test]
    fn dag_run_complete_for_panel_only_run_clears_and_renders() {
        // A run we observed via the panel but didn't start ourselves still
        // needs to disappear when it completes.
        let mut s = ChatState::new();
        let run_id = "panel-only".to_owned();
        let _ = handle_envelope(
            event_env(json!({
                "kind": "graph.run_started",
                "run_id": run_id.clone(),
                "total_nodes": 1,
            })),
            &mut s,
        );
        let env = event_env(json!({
            "kind": "graph.run_complete",
            "run_id": run_id,
            "status": "success",
            "results": {},
        }));
        let action = handle_envelope(env, &mut s);
        assert!(matches!(action, Action::Render));
        // Lingers for visual feedback (same as our own runs); prune empties
        // it after `DAG_RUN_LINGER_MS`.
        let run = s.dag_runs.get(&run_id).expect("panel-only run lingers");
        let stamp = run.completed_at_ms.expect("completed_at_ms set");
        assert!(s.prune_finished_dag_runs(stamp + DAG_RUN_LINGER_MS + 1));
        assert!(s.dag_runs.is_empty());
    }

    #[test]
    fn dag_panel_height_zero_when_no_runs() {
        let s = ChatState::new();
        assert_eq!(s.dag_panel_rows(), 0);
    }

    #[test]
    fn dag_panel_height_matches_run_and_node_count() {
        let mut s = ChatState::new();
        let _ = handle_envelope(
            event_env(json!({
                "kind": "graph.run_started",
                "run_id": "run-h",
                "total_nodes": 2,
            })),
            &mut s,
        );
        // 1 header, 0 nodes.
        assert_eq!(s.dag_panel_rows(), 1);
        let _ = handle_envelope(
            event_env(json!({
                "kind": "graph.node_dispatched",
                "run_id": "run-h",
                "node_id": "n1",
                "reasoner": "ollama",
            })),
            &mut s,
        );
        let _ = handle_envelope(
            event_env(json!({
                "kind": "graph.node_dispatched",
                "run_id": "run-h",
                "node_id": "n2",
                "reasoner": "ollama",
            })),
            &mut s,
        );
        // 1 header + 2 node rows.
        assert_eq!(s.dag_panel_rows(), 3);
    }

    #[test]
    fn slash_new_clears_dag_runs_and_pending_dag_runs() {
        let mut s = ChatState::new();
        // Simulate a /dag-test in flight + matching panel entry.
        s.pending_dag_runs.insert("run-pre".into());
        let _ = handle_envelope(
            event_env(json!({
                "kind": "graph.run_started",
                "run_id": "run-pre",
                "total_nodes": 1,
            })),
            &mut s,
        );
        assert!(!s.dag_runs.is_empty());
        assert!(!s.pending_dag_runs.is_empty());
        let _ = drive_command(Command::New, &mut s);
        assert!(s.dag_runs.is_empty(), "/new must clear dag_runs");
        assert!(
            s.pending_dag_runs.is_empty(),
            "/new must clear pending_dag_runs"
        );
    }

    #[test]
    fn parse_command_new_and_clear_alias() {
        // Both the canonical `/new` and the `/clear` alias resolve to the
        // same `Command::New` variant.
        assert_eq!(parse_command("/new"), Some(Command::New));
        assert_eq!(parse_command("/clear"), Some(Command::New));
        // Trim is already handled by `parse_command`.
        assert_eq!(parse_command("  /new  "), Some(Command::New));
        assert_eq!(parse_command("  /clear  "), Some(Command::New));
    }

    #[test]
    fn slash_command_matches_aliases() {
        // `/cl` should match `/new` because `clear` is its alias — the
        // returned entry carries the canonical `name`, not the alias.
        let names: Vec<String> = slash_command_matches("cl")
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, vec!["new".to_string()]);
        // The same query should not produce duplicate entries even though
        // the alias and an unrelated name might both match.
        let news: Vec<String> = slash_command_matches("new")
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(news, vec!["new".to_string()]);
    }

    #[test]
    fn slash_new_resets_per_turn_telemetry_but_keeps_model() {
        let mut s = ChatState::new();
        s.metadata = SessionMetadata {
            stats_seen: true,
            model: Some("gemma4:latest".into()),
            turns: Some(3),
            cumulative_cost_usd: Some(0.42),
            last_turn_context_tokens: Some(47_000),
            last_turn_duration_ms: Some(10_000),
            last_turn_output_tokens: Some(450),
            ..Default::default()
        };
        let _ = drive_command(Command::New, &mut s);
        // Per-turn fields wiped — the next render hits the pre-first-turn
        // path, not "stats_seen with stale 12s · 0.42$ · 3 turns".
        assert!(!s.metadata.stats_seen);
        assert!(s.metadata.turns.is_none());
        assert!(s.metadata.cumulative_cost_usd.is_none());
        assert!(s.metadata.last_turn_duration_ms.is_none());
        assert!(s.metadata.last_turn_output_tokens.is_none());
        assert!(s.metadata.last_turn_context_tokens.is_none());
        // Model name preserved — it's a property of the active provider,
        // not the session.
        assert_eq!(s.metadata.model.as_deref(), Some("gemma4:latest"));
    }

    #[test]
    fn slash_new_clears_transcript_and_emits_chat_reset() {
        let mut s = ChatState::new();
        s.push_entry(Role::User, "first".into());
        s.push_entry(Role::System, "second".into());
        s.pending = true;
        s.arm_watchdog();
        let before_version = s.transcript_version;
        let bodies = drive_command(Command::New, &mut s);
        // Local state cleared.
        assert!(s.transcript.is_empty(), "transcript must be empty after /new");
        assert!(!s.pending, "pending flag must be cleared");
        assert!(s.awaiting_response_since.is_none(), "watchdog must be disarmed");
        assert!(
            s.transcript_version > before_version,
            "transcript_version must be bumped so the renderer redraws"
        );
        // Exactly one outgoing event: chat.reset (no popup, no transcript
        // entry).
        assert_eq!(bodies.len(), 1, "expected exactly one outgoing event");
        assert_eq!(bodies[0]["kind"], Value::String("chat.reset".into()));
        // No popup surfaced — reset is silent.
        assert!(s.popup.is_none(), "/new must not open any popup");
    }

    #[test]
    fn slash_login_emits_login_requested() {
        // Single connected provider → it becomes the implicit target.
        let mut s = ChatState::new();
        s.register_provider("ollama");
        let bodies = drive_command(Command::Login { provider: None }, &mut s);
        assert_eq!(bodies.len(), 1);
        assert_eq!(bodies[0]["kind"], "chat.login_requested");
        assert_eq!(bodies[0]["provider"], "ollama");
    }

    #[test]
    fn slash_login_with_arg_targets_provider() {
        let mut s = ChatState::new();
        // Multiple providers; explicit arg wins.
        s.register_provider("ollama");
        s.register_provider("anthropic");
        let bodies = drive_command(
            Command::Login {
                provider: Some("anthropic".into()),
            },
            &mut s,
        );
        assert_eq!(bodies.len(), 1);
        assert_eq!(bodies[0]["kind"], "chat.login_requested");
        assert_eq!(bodies[0]["provider"], "anthropic");
    }

    #[test]
    fn slash_login_no_arg_with_multiple_providers_omits_provider() {
        let mut s = ChatState::new();
        s.register_provider("ollama");
        s.register_provider("anthropic");
        let bodies = drive_command(Command::Login { provider: None }, &mut s);
        assert_eq!(bodies.len(), 1);
        assert_eq!(bodies[0]["kind"], "chat.login_requested");
        assert!(
            bodies[0].get("provider").is_none(),
            "ambiguous /login must not pin a provider: {:?}",
            bodies[0]
        );
    }

    #[test]
    fn slash_logout_emits_logout_requested() {
        let mut s = ChatState::new();
        s.register_provider("ollama");
        let bodies = drive_command(Command::Logout { provider: None }, &mut s);
        assert_eq!(bodies.len(), 1);
        assert_eq!(bodies[0]["kind"], "chat.logout_requested");
        assert_eq!(bodies[0]["provider"], "ollama");
    }

    #[test]
    fn slash_model_no_arg_emits_list_requested() {
        let mut s = ChatState::new();
        s.set_active_provider("ollama");
        s.auth_status.insert(
            "ollama".into(),
            AuthStatus {
                state: "connected".into(),
                message: None,
            },
        );
        let bodies = drive_command(Command::ModelList, &mut s);
        assert_eq!(bodies.len(), 1);
        assert_eq!(bodies[0]["kind"], "chat.model.list_requested");
        assert_eq!(bodies[0]["provider"], "ollama");
    }

    #[test]
    fn slash_model_with_arg_emits_set() {
        let mut s = ChatState::new();
        s.set_active_provider("anthropic");
        let bodies = drive_command(Command::ModelSet("claude-opus-4-7".into()), &mut s);
        assert_eq!(bodies.len(), 1);
        assert_eq!(bodies[0]["kind"], "chat.model.set");
        assert_eq!(bodies[0]["provider"], "anthropic");
        assert_eq!(bodies[0]["model"], "claude-opus-4-7");
    }

    #[test]
    fn slash_help_opens_help_popup_no_transcript_message() {
        let mut s = ChatState::new();
        let bodies = drive_command(Command::Help, &mut s);
        assert!(bodies.is_empty(), "help must not emit network events");
        assert!(
            s.transcript.is_empty(),
            "help popup must not push a transcript entry"
        );
        assert!(matches!(s.popup, Some(Popup::Help { .. })));
    }

    #[test]
    fn popup_help_esc_closes() {
        let mut s = ChatState::new();
        s.open_popup_help();
        assert!(s.popup.is_some());
        let env = event_env(json!({
            "kind": "nefor-tui.input.key",
            "key": "escape",
            "modifiers": [],
        }));
        let a = handle_envelope(env, &mut s);
        assert!(matches!(a, Action::Render));
        assert!(s.popup.is_none());
    }

    #[test]
    fn popup_help_enter_closes() {
        let mut s = ChatState::new();
        s.open_popup_help();
        let env = event_env(json!({
            "kind": "nefor-tui.input.key",
            "key": "enter",
            "modifiers": [],
        }));
        let a = handle_envelope(env, &mut s);
        assert!(matches!(a, Action::Render));
        assert!(s.popup.is_none());
    }

    #[test]
    fn slash_model_opens_picker_popup_with_loading_state() {
        let mut s = ChatState::new();
        s.register_provider("ollama");
        s.register_provider("groq");
        s.auth_status.insert(
            "ollama".into(),
            AuthStatus {
                state: "connected".into(),
                message: None,
            },
        );
        s.auth_status.insert(
            "groq".into(),
            AuthStatus {
                state: "connected".into(),
                message: None,
            },
        );
        let bodies = drive_command(Command::ModelList, &mut s);
        assert_eq!(bodies.len(), 2, "one list request per connected provider");
        assert!(bodies.iter().all(|b| b["kind"] == "chat.model.list_requested"));
        match &s.popup {
            Some(Popup::ModelPicker {
                all_models,
                awaiting,
                cursor,
                query,
                ..
            }) => {
                assert!(all_models.is_empty());
                assert!(query.is_empty());
                assert_eq!(*cursor, 0);
                assert!(awaiting.contains("ollama"));
                assert!(awaiting.contains("groq"));
                assert_eq!(awaiting.len(), 2);
            }
            other => panic!("expected ModelPicker, got {other:?}"),
        }
    }

    #[test]
    fn popup_model_picker_appends_listed_results() {
        let mut s = ChatState::new();
        s.register_provider("ollama");
        s.auth_status.insert(
            "ollama".into(),
            AuthStatus {
                state: "connected".into(),
                message: None,
            },
        );
        let _ = drive_command(Command::ModelList, &mut s);
        handle_envelope(
            event_env(json!({
                "kind": "chat.models.listed",
                "provider": "ollama",
                "models": ["llama3:8b", "qwen2.5-coder:7b"],
            })),
            &mut s,
        );
        match &s.popup {
            Some(Popup::ModelPicker {
                all_models,
                awaiting,
                ..
            }) => {
                assert_eq!(all_models.len(), 2);
                assert!(awaiting.is_empty(), "ollama should be removed from awaiting");
            }
            other => panic!("expected ModelPicker, got {other:?}"),
        }
        // No transcript pollution.
        assert!(s.transcript.is_empty());
    }

    #[test]
    fn popup_model_picker_filter_narrows_list() {
        let mut s = ChatState::new();
        s.register_provider("ollama");
        s.auth_status.insert(
            "ollama".into(),
            AuthStatus {
                state: "connected".into(),
                message: None,
            },
        );
        let _ = drive_command(Command::ModelList, &mut s);
        handle_envelope(
            event_env(json!({
                "kind": "chat.models.listed",
                "provider": "ollama",
                "models": ["llama3:8b", "qwen2.5-coder:7b", "mistral:7b"],
            })),
            &mut s,
        );
        for c in "qwen".chars() {
            handle_envelope(
                event_env(json!({
                    "kind": "nefor-tui.input.key",
                    "key": c.to_string(),
                    "modifiers": [],
                })),
                &mut s,
            );
        }
        match &s.popup {
            Some(Popup::ModelPicker { query, all_models, .. }) => {
                assert_eq!(query, "qwen");
                assert_eq!(all_models.len(), 3, "underlying list unchanged");
                let visible = filter_models(all_models, query);
                assert_eq!(visible.len(), 1);
                assert_eq!(visible[0].1, "qwen2.5-coder:7b");
            }
            other => panic!("expected ModelPicker, got {other:?}"),
        }
    }

    #[test]
    fn popup_model_picker_filter_reset_after_typing() {
        // Backspace shrinks the query; an empty query restores the full list.
        let mut s = ChatState::new();
        s.register_provider("ollama");
        s.auth_status.insert(
            "ollama".into(),
            AuthStatus {
                state: "connected".into(),
                message: None,
            },
        );
        let _ = drive_command(Command::ModelList, &mut s);
        handle_envelope(
            event_env(json!({
                "kind": "chat.models.listed",
                "provider": "ollama",
                "models": ["a", "b"],
            })),
            &mut s,
        );
        // Type "x" to filter (matches nothing), then backspace to clear.
        handle_envelope(
            event_env(json!({
                "kind": "nefor-tui.input.key",
                "key": "x",
                "modifiers": [],
            })),
            &mut s,
        );
        match &s.popup {
            Some(Popup::ModelPicker { query, all_models, .. }) => {
                assert_eq!(query, "x");
                assert!(filter_models(all_models, query).is_empty());
            }
            _ => panic!(),
        }
        handle_envelope(
            event_env(json!({
                "kind": "nefor-tui.input.key",
                "key": "backspace",
                "modifiers": [],
            })),
            &mut s,
        );
        match &s.popup {
            Some(Popup::ModelPicker { query, all_models, .. }) => {
                assert!(query.is_empty());
                assert_eq!(filter_models(all_models, query).len(), 2);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn popup_model_picker_enter_emits_set_event_and_closes() {
        let mut s = ChatState::new();
        s.register_provider("ollama");
        s.auth_status.insert(
            "ollama".into(),
            AuthStatus {
                state: "connected".into(),
                message: None,
            },
        );
        let _ = drive_command(Command::ModelList, &mut s);
        handle_envelope(
            event_env(json!({
                "kind": "chat.models.listed",
                "provider": "ollama",
                "models": ["llama3:8b"],
            })),
            &mut s,
        );
        let env = event_env(json!({
            "kind": "nefor-tui.input.key",
            "key": "enter",
            "modifiers": [],
        }));
        let a = handle_envelope(env, &mut s);
        match a {
            Action::SelectModel(sel) => {
                assert_eq!(sel.provider, "ollama");
                assert_eq!(sel.model, "llama3:8b");
            }
            other => panic!("expected SelectModel, got {other:?}"),
        }
        assert!(s.popup.is_none());
        assert_eq!(s.active_provider.as_deref(), Some("ollama"));
    }

    #[test]
    fn popup_model_picker_empty_state_when_no_providers() {
        let mut s = ChatState::new();
        // No providers, no auth status. /model still opens the picker; user
        // sees the empty-state hint instead of nothing happening.
        let bodies = drive_command(Command::ModelList, &mut s);
        assert!(bodies.is_empty(), "no providers → no list_requested events");
        match &s.popup {
            Some(Popup::ModelPicker {
                all_models,
                awaiting,
                ..
            }) => {
                assert!(all_models.is_empty());
                assert!(awaiting.is_empty());
            }
            other => panic!("expected ModelPicker, got {other:?}"),
        }
    }

    #[test]
    fn popup_blocks_normal_chat_input() {
        // Typing while the help popup is open must NOT append to the chat
        // input buffer (Help has no filter, so chars are dropped). For the
        // model picker it goes into the search query, also not the chat input.
        let mut s = ChatState::new();
        s.open_popup_help();
        for c in "abc".chars() {
            handle_envelope(
                event_env(json!({
                    "kind": "nefor-tui.input.key",
                    "key": c.to_string(),
                    "modifiers": [],
                })),
                &mut s,
            );
        }
        assert_eq!(s.input.as_string(), "");
    }

    #[test]
    fn esc_closes_popup_then_subsequent_esc_interrupts_turn() {
        let mut s = ChatState::new();
        s.arm_watchdog();
        s.open_popup_help();
        let esc1 = event_env(json!({
            "kind": "nefor-tui.input.key",
            "key": "escape",
            "modifiers": [],
        }));
        let a1 = handle_envelope(esc1, &mut s);
        assert!(matches!(a1, Action::Render));
        assert!(s.popup.is_none());
        let esc2 = event_env(json!({
            "kind": "nefor-tui.input.key",
            "key": "escape",
            "modifiers": [],
        }));
        let a2 = handle_envelope(esc2, &mut s);
        assert!(matches!(a2, Action::Interrupt));
    }

    #[test]
    fn popup_open_disables_prompt_history_navigation() {
        let mut s = ChatState::new();
        s.push_history("recorded".into());
        s.open_popup_help();
        let env = event_env(json!({
            "kind": "nefor-tui.input.key",
            "key": "up",
            "modifiers": [],
        }));
        handle_envelope(env, &mut s);
        // Help popup ignores Up; chat buffer must stay empty.
        assert_eq!(s.input.as_string(), "");
    }

    #[test]
    fn slash_login_with_no_providers_opens_warning_popup() {
        let mut s = ChatState::new();
        let bodies = drive_command(Command::Login { provider: None }, &mut s);
        assert!(
            bodies.is_empty(),
            "no providers → no chat.login_requested event"
        );
        match &s.popup {
            Some(Popup::Warning { title, message, .. }) => {
                assert_eq!(title, "login");
                assert!(
                    message.contains("No providers connected"),
                    "message={message}"
                );
            }
            other => panic!("expected Popup::Warning, got {other:?}"),
        }
        assert!(s.transcript.is_empty());
    }

    #[test]
    fn slash_login_with_unknown_provider_opens_warning_popup() {
        let mut s = ChatState::new();
        s.register_provider("ollama");
        let bodies = drive_command(
            Command::Login {
                provider: Some("anthropic".into()),
            },
            &mut s,
        );
        assert!(bodies.is_empty(), "unknown provider → no event");
        match &s.popup {
            Some(Popup::Warning { title, message, .. }) => {
                assert_eq!(title, "login");
                assert!(message.contains("anthropic"), "message={message}");
                assert!(message.contains("ollama"), "message={message}");
            }
            other => panic!("expected Popup::Warning, got {other:?}"),
        }
    }

    #[test]
    fn slash_logout_with_no_providers_opens_warning_popup() {
        let mut s = ChatState::new();
        let bodies = drive_command(Command::Logout { provider: None }, &mut s);
        assert!(bodies.is_empty());
        assert!(matches!(s.popup, Some(Popup::Warning { .. })));
    }

    #[test]
    fn slash_logout_with_unknown_provider_opens_warning_popup() {
        let mut s = ChatState::new();
        s.register_provider("ollama");
        let bodies = drive_command(
            Command::Logout {
                provider: Some("groq".into()),
            },
            &mut s,
        );
        assert!(bodies.is_empty());
        match &s.popup {
            Some(Popup::Warning { message, .. }) => {
                assert!(message.contains("groq"), "message={message}");
            }
            other => panic!("expected Popup::Warning, got {other:?}"),
        }
    }

    #[test]
    fn system_message_starting_with_error_routes_to_popup_not_transcript() {
        let mut s = ChatState::new();
        let env = event_env(json!({
            "kind": "chat.message.append",
            "role": "system",
            "text": "Error: HTTP 500: upstream borked",
        }));
        let a = handle_envelope(env, &mut s);
        assert!(matches!(a, Action::Render));
        assert!(s.transcript.is_empty(), "no transcript pollution");
        match &s.popup {
            Some(Popup::Error { title, message, .. }) => {
                assert_eq!(title, "error");
                assert!(message.contains("HTTP 500"), "message={message}");
            }
            other => panic!("expected Popup::Error, got {other:?}"),
        }
    }

    #[test]
    fn system_message_without_error_prefix_stays_in_transcript() {
        let mut s = ChatState::new();
        let env = event_env(json!({
            "kind": "chat.message.append",
            "role": "system",
            "text": "[interrupted]",
        }));
        handle_envelope(env, &mut s);
        assert_eq!(s.transcript.len(), 1);
        assert!(s.popup.is_none());
    }

    #[test]
    fn q_key_closes_help_popup() {
        let mut s = ChatState::new();
        s.open_popup_help();
        let env = event_env(json!({
            "kind": "nefor-tui.input.key",
            "key": "q",
            "modifiers": [],
        }));
        let a = handle_envelope(env, &mut s);
        assert!(matches!(a, Action::Render));
        assert!(s.popup.is_none());
    }

    #[test]
    fn q_key_closes_warning_popup() {
        let mut s = ChatState::new();
        s.open_popup_warning("test", "body", None);
        let env = event_env(json!({
            "kind": "nefor-tui.input.key",
            "key": "q",
            "modifiers": [],
        }));
        let a = handle_envelope(env, &mut s);
        assert!(matches!(a, Action::Render));
        assert!(s.popup.is_none());
    }

    #[test]
    fn q_key_closes_error_popup() {
        let mut s = ChatState::new();
        s.open_popup_error("test", "body", None);
        let env = event_env(json!({
            "kind": "nefor-tui.input.key",
            "key": "q",
            "modifiers": [],
        }));
        let a = handle_envelope(env, &mut s);
        assert!(matches!(a, Action::Render));
        assert!(s.popup.is_none());
    }

    #[test]
    fn q_key_in_model_picker_appends_to_query() {
        // Q must NOT close the model picker — it's a valid filter character
        // for searching model names. Only ESC closes the picker.
        let mut s = ChatState::new();
        s.register_provider("ollama");
        s.auth_status.insert(
            "ollama".into(),
            AuthStatus {
                state: "connected".into(),
                message: None,
            },
        );
        let _ = drive_command(Command::ModelList, &mut s);
        handle_envelope(
            event_env(json!({
                "kind": "chat.models.listed",
                "provider": "ollama",
                "models": ["qwen2.5-coder:7b"],
            })),
            &mut s,
        );
        let env = event_env(json!({
            "kind": "nefor-tui.input.key",
            "key": "q",
            "modifiers": [],
        }));
        handle_envelope(env, &mut s);
        match &s.popup {
            Some(Popup::ModelPicker { query, .. }) => {
                assert_eq!(query, "q");
            }
            other => panic!("model picker must remain open, got {other:?}"),
        }
    }

    #[test]
    fn slash_unknown_command_emits_generic_chat_command() {
        let mut s = ChatState::new();
        let bodies = drive_command(
            Command::Generic {
                name: "think".into(),
                args: "step by step".into(),
            },
            &mut s,
        );
        assert_eq!(bodies.len(), 1);
        assert_eq!(bodies[0]["kind"], "chat.command");
        assert_eq!(bodies[0]["name"], "think");
        assert_eq!(bodies[0]["args"], "step by step");
    }

    #[test]
    fn bare_prompt_still_emits_input_submit() {
        // Sanity: a non-slash prompt taken through the Action::SubmitPrompt
        // path produces a `chat.input.submit` body shape (parse_command
        // returns None so the caller hits the default branch).
        assert_eq!(parse_command("just a prompt"), None);
        let body = input_submit_body("just a prompt");
        assert_eq!(body["kind"], "chat.input.submit");
        assert_eq!(body["text"], "just a prompt");
    }

    // ---- chat.auth.status ---------------------------------------------------

    #[test]
    fn chat_auth_status_connected_no_popup_just_statusline() {
        // "connected" is a passive transition: drives the statusline indicator
        // and the active-provider promotion, but doesn't push a transcript
        // line or pop a popup (would interrupt the user for no reason).
        let mut s = ChatState::new();
        let env = event_env(json!({
            "kind": "chat.auth.status",
            "provider": "ollama",
            "state": "connected",
        }));
        let a = handle_envelope(env, &mut s);
        assert!(matches!(a, Action::Render));
        assert_eq!(s.providers, vec!["ollama".to_owned()]);
        assert_eq!(
            s.auth_status.get("ollama").map(|s| s.state.as_str()),
            Some("connected")
        );
        assert!(s.transcript.is_empty(), "no transcript pollution");
        assert!(s.popup.is_none(), "no popup interruption");
    }

    #[test]
    fn chat_auth_status_login_required_no_popup_just_statusline() {
        // "login_required" is also passive — the statusline auth indicator
        // shows the warn marker; the user sees what they need without a popup.
        let mut s = ChatState::new();
        let env = event_env(json!({
            "kind": "chat.auth.status",
            "provider": "anthropic",
            "state": "login_required",
        }));
        handle_envelope(env, &mut s);
        assert!(s.transcript.is_empty(), "no transcript pollution");
        assert!(s.popup.is_none(), "no popup interruption");
        assert_eq!(
            s.auth_status.get("anthropic").map(|s| s.state.as_str()),
            Some("login_required")
        );
    }

    #[test]
    fn chat_auth_status_error_opens_error_popup_no_transcript_message() {
        let mut s = ChatState::new();
        let env = event_env(json!({
            "kind": "chat.auth.status",
            "provider": "anthropic",
            "state": "error",
            "message": "HTTP 401: bad token",
        }));
        let a = handle_envelope(env, &mut s);
        assert!(matches!(a, Action::Render));
        // Transcript stays clean.
        assert!(s.transcript.is_empty());
        // Statusline backing data is updated.
        assert_eq!(
            s.auth_status.get("anthropic").map(|s| s.state.as_str()),
            Some("error")
        );
        // The popup carries the diagnostic.
        match &s.popup {
            Some(Popup::Error { title, message, .. }) => {
                assert!(title.contains("anthropic"), "title={title}");
                assert!(message.contains("HTTP 401"), "message={message}");
            }
            other => panic!("expected Popup::Error, got {other:?}"),
        }
    }

    #[test]
    fn auth_status_first_connected_provider_becomes_active() {
        let mut s = ChatState::new();
        // login_required first — must NOT promote to active.
        handle_envelope(
            event_env(json!({
                "kind": "chat.auth.status",
                "provider": "anthropic",
                "state": "login_required",
            })),
            &mut s,
        );
        assert!(s.active_provider.is_none());
        // Then ollama connects — becomes active.
        handle_envelope(
            event_env(json!({
                "kind": "chat.auth.status",
                "provider": "ollama",
                "state": "connected",
            })),
            &mut s,
        );
        assert_eq!(s.active_provider.as_deref(), Some("ollama"));
        // Subsequent connects don't override the first one.
        handle_envelope(
            event_env(json!({
                "kind": "chat.auth.status",
                "provider": "anthropic",
                "state": "connected",
            })),
            &mut s,
        );
        assert_eq!(s.active_provider.as_deref(), Some("ollama"));
    }

    // ---- chat.models.listed / chat.model.set_ack ---------------------------

    #[test]
    fn chat_models_listed_pushes_system_message() {
        let mut s = ChatState::new();
        let env = event_env(json!({
            "kind": "chat.models.listed",
            "provider": "ollama",
            "models": ["llama3:8b", "qwen2.5-coder:7b"],
        }));
        let a = handle_envelope(env, &mut s);
        assert!(matches!(a, Action::Render));
        assert_eq!(s.transcript.len(), 1);
        assert_eq!(s.transcript[0].role, Role::System);
        let text = &s.transcript[0].text;
        assert!(text.contains("[ollama]"), "got: {text}");
        assert!(text.contains("llama3:8b"), "got: {text}");
        assert!(text.contains("qwen2.5-coder:7b"), "got: {text}");
    }

    #[test]
    fn chat_models_listed_empty_list_pushes_none_marker() {
        let mut s = ChatState::new();
        let env = event_env(json!({
            "kind": "chat.models.listed",
            "provider": "ollama",
            "models": [],
        }));
        handle_envelope(env, &mut s);
        let text = &s.transcript[0].text;
        assert!(text.contains("none"), "got: {text}");
    }

    #[test]
    fn chat_models_listed_truncates_long_list() {
        let mut s = ChatState::new();
        let big: Vec<String> = (0..MODEL_LIST_CAP + 5)
            .map(|i| format!("model-{i:02}"))
            .collect();
        let env = event_env(json!({
            "kind": "chat.models.listed",
            "provider": "groq",
            "models": big,
        }));
        handle_envelope(env, &mut s);
        let text = &s.transcript[0].text;
        assert!(
            text.contains("...and 5 more"),
            "expected overflow footer, got: {text}"
        );
        // The model at index MODEL_LIST_CAP+1 must NOT appear inline.
        let hidden = format!("model-{:02}", MODEL_LIST_CAP + 1);
        assert!(
            !text.contains(&hidden),
            "expected {hidden} to be truncated, got: {text}"
        );
    }

    #[test]
    fn chat_models_listed_bumps_transcript_version() {
        let mut s = ChatState::new();
        let v0 = s.transcript_version;
        let env = event_env(json!({
            "kind": "chat.models.listed",
            "provider": "ollama",
            "models": ["a"],
        }));
        handle_envelope(env, &mut s);
        assert_ne!(s.transcript_version, v0);
    }

    #[test]
    fn chat_model_set_ack_updates_metadata_model_for_statusline() {
        let mut s = ChatState::new();
        let env = event_env(json!({
            "kind": "chat.model.set_ack",
            "provider": "ollama",
            "model": "qwen2.5-coder:7b",
        }));
        let a = handle_envelope(env, &mut s);
        assert!(matches!(a, Action::Render));
        // No transcript noise — statusline carries the model now.
        assert!(s.transcript.is_empty(), "no system line on model change");
        assert_eq!(
            s.metadata.model.as_deref(),
            Some("qwen2.5-coder:7b"),
            "statusline reads md.model; must update immediately on set_ack"
        );
    }

    #[test]
    fn chat_model_set_ack_updates_active_model_map() {
        let mut s = ChatState::new();
        handle_envelope(
            event_env(json!({
                "kind": "chat.model.set_ack",
                "provider": "ollama",
                "model": "first",
            })),
            &mut s,
        );
        assert_eq!(
            s.active_model_per_provider.get("ollama").map(String::as_str),
            Some("first")
        );
        // A second ack on the same provider replaces the prior value.
        handle_envelope(
            event_env(json!({
                "kind": "chat.model.set_ack",
                "provider": "ollama",
                "model": "second",
            })),
            &mut s,
        );
        assert_eq!(
            s.active_model_per_provider.get("ollama").map(String::as_str),
            Some("second")
        );
        // Independent provider keeps its own slot.
        handle_envelope(
            event_env(json!({
                "kind": "chat.model.set_ack",
                "provider": "groq",
                "model": "groq-model",
            })),
            &mut s,
        );
        assert_eq!(
            s.active_model_per_provider.get("groq").map(String::as_str),
            Some("groq-model")
        );
        assert_eq!(
            s.active_model_per_provider.get("ollama").map(String::as_str),
            Some("second")
        );
    }

    // ---- popup scroll & slash autocomplete --------------------------------

    fn key_env(key: &str) -> Envelope {
        event_env(json!({
            "kind": "nefor-tui.input.key",
            "key": key,
            "modifiers": [],
        }))
    }

    fn type_text(state: &mut ChatState, text: &str) {
        for c in text.chars() {
            handle_envelope(key_env(&c.to_string()), state);
        }
    }

    #[test]
    fn help_popup_scroll_keys_adjust_offset() {
        let mut s = ChatState::new();
        s.dims = state::Dims { cols: 80, rows: 12 };
        s.tui_ready = true;
        s.open_popup_help();
        // Down increments scroll by 1.
        handle_envelope(key_env("down"), &mut s);
        match &s.popup {
            Some(Popup::Help { scroll }) => assert_eq!(*scroll, 1),
            other => panic!("expected Popup::Help, got {other:?}"),
        }
        // End jumps to the maximum scroll.
        handle_envelope(key_env("end"), &mut s);
        match &s.popup {
            Some(Popup::Help { scroll }) => assert!(*scroll > 1),
            other => panic!("expected Popup::Help, got {other:?}"),
        }
        // Home returns to 0.
        handle_envelope(key_env("home"), &mut s);
        match &s.popup {
            Some(Popup::Help { scroll }) => assert_eq!(*scroll, 0),
            other => panic!("expected Popup::Help, got {other:?}"),
        }
    }

    #[test]
    fn slash_typed_opens_autocomplete_with_full_registry() {
        let mut s = ChatState::new();
        type_text(&mut s, "/");
        match &s.popup {
            Some(Popup::SlashAutocomplete { matches, .. }) => {
                let names: Vec<&str> = matches.iter().map(|c| c.name.as_str()).collect();
                assert!(names.contains(&"help"));
                assert!(names.contains(&"login"));
                assert!(names.contains(&"logout"));
                assert!(names.contains(&"model"));
                assert!(names.contains(&"resume"));
            }
            other => panic!("expected SlashAutocomplete, got {other:?}"),
        }
        assert_eq!(s.input.as_string(), "/");
    }

    #[test]
    fn slash_lo_filters_to_login_and_logout() {
        let mut s = ChatState::new();
        type_text(&mut s, "/lo");
        match &s.popup {
            Some(Popup::SlashAutocomplete { matches, .. }) => {
                let names: Vec<&str> = matches.iter().map(|c| c.name.as_str()).collect();
                assert_eq!(names, vec!["login", "logout"]);
            }
            other => panic!("expected SlashAutocomplete, got {other:?}"),
        }
    }

    #[test]
    fn slash_autocomplete_tab_completes_to_full_command() {
        let mut s = ChatState::new();
        type_text(&mut s, "/lo");
        // Tab on cursor row 0 (login): replaces input with `/login ` (with
        // trailing space because login takes args). The trailing whitespace
        // means the user has moved past command selection — the popup closes
        // so Enter falls through to the normal Submit path.
        handle_envelope(key_env("tab"), &mut s);
        assert_eq!(s.input.as_string(), "/login ");
        assert!(
            s.popup.is_none(),
            "trailing-space input must close the slash popup so Enter submits cleanly"
        );
    }

    #[test]
    fn slash_autocomplete_enter_completes_and_dispatches() {
        // Enter completes to the cursor's match AND submits as a SubmitPrompt
        // action. The submit text starts with `/` so the main loop will route
        // through `parse_command` → `handle_command`.
        let mut s = ChatState::new();
        type_text(&mut s, "/he");
        // Top match is "help" by registry order.
        let action = handle_envelope(key_env("enter"), &mut s);
        match action {
            Action::SubmitPrompt(text) => {
                assert_eq!(text, "/help");
                assert_eq!(parse_command(&text), Some(Command::Help));
            }
            other => panic!("expected SubmitPrompt, got {other:?}"),
        }
        // Popup is closed and input cleared by the submit path.
        assert!(s.popup.is_none());
        assert_eq!(s.input.as_string(), "");
    }

    #[test]
    fn slash_autocomplete_backspace_to_empty_closes_popup() {
        let mut s = ChatState::new();
        type_text(&mut s, "/");
        assert!(matches!(s.popup, Some(Popup::SlashAutocomplete { .. })));
        // Backspace removes the `/` → no longer slash-prefixed → popup closes.
        handle_envelope(key_env("backspace"), &mut s);
        assert!(s.popup.is_none());
        assert_eq!(s.input.as_string(), "");
    }

    #[test]
    fn non_slash_first_char_does_not_open_autocomplete() {
        let mut s = ChatState::new();
        type_text(&mut s, "hello");
        assert!(s.popup.is_none(), "non-slash input must not open the popup");
        assert_eq!(s.input.as_string(), "hello");
    }

    #[test]
    fn slash_autocomplete_arrow_keys_move_cursor() {
        let mut s = ChatState::new();
        type_text(&mut s, "/");
        // Down moves cursor.
        handle_envelope(key_env("down"), &mut s);
        match &s.popup {
            Some(Popup::SlashAutocomplete { cursor, .. }) => assert_eq!(*cursor, 1),
            other => panic!("expected SlashAutocomplete, got {other:?}"),
        }
        // Up moves it back.
        handle_envelope(key_env("up"), &mut s);
        match &s.popup {
            Some(Popup::SlashAutocomplete { cursor, .. }) => assert_eq!(*cursor, 0),
            other => panic!("expected SlashAutocomplete, got {other:?}"),
        }
    }

    #[test]
    fn slash_autocomplete_escape_closes_without_modifying_input() {
        let mut s = ChatState::new();
        type_text(&mut s, "/lo");
        handle_envelope(key_env("escape"), &mut s);
        assert!(s.popup.is_none());
        assert_eq!(s.input.as_string(), "/lo", "input must be untouched");
    }

    #[test]
    fn slash_autocomplete_closes_when_whitespace_after_command_name() {
        // After Tab on `/login`, the buffer becomes `/login ` (trailing space
        // because `/login` takes args). The post-Tab refresh must close the
        // popup so Enter falls through to the normal Submit path — without
        // this, `slash_command_matches("login ")` returns no entries and
        // Enter is silently dropped by the popup handler.
        let mut s = ChatState::new();
        type_text(&mut s, "/login");
        assert!(matches!(s.popup, Some(Popup::SlashAutocomplete { .. })));
        handle_envelope(key_env("tab"), &mut s);
        assert_eq!(s.input.as_string(), "/login ");
        assert!(s.popup.is_none(), "trailing space must close the popup");
    }

    #[test]
    fn enter_on_login_with_trailing_space_submits_through_normal_path() {
        // Reproduces the bug fix end-to-end: Tab leaves `/login `, Enter must
        // submit (action SubmitPrompt) so the main loop's `parse_command`
        // path dispatches `Command::Login { provider: None }`.
        let mut s = ChatState::new();
        type_text(&mut s, "/login");
        handle_envelope(key_env("tab"), &mut s);
        assert_eq!(s.input.as_string(), "/login ");
        assert!(s.popup.is_none());
        let action = handle_envelope(key_env("enter"), &mut s);
        match action {
            Action::SubmitPrompt(text) => {
                assert_eq!(text, "/login ");
                assert_eq!(
                    parse_command(&text),
                    Some(Command::Login { provider: None })
                );
            }
            other => panic!("expected SubmitPrompt, got {other:?}"),
        }
    }

    #[test]
    fn enter_on_unknown_slash_with_trailing_space_submits_as_generic() {
        // Same bug-fix rail but for an unknown command: `/foo  ` (with
        // trailing whitespace) must close the popup and Enter must submit
        // the buffer as a `Command::Generic` after `parse_command` trims it.
        let mut s = ChatState::new();
        type_text(&mut s, "/foo  ");
        assert!(s.popup.is_none(), "whitespace after name must close popup");
        let action = handle_envelope(key_env("enter"), &mut s);
        match action {
            Action::SubmitPrompt(text) => {
                assert_eq!(text, "/foo  ");
                assert_eq!(
                    parse_command(&text),
                    Some(Command::Generic {
                        name: "foo".into(),
                        args: String::new(),
                    })
                );
            }
            other => panic!("expected SubmitPrompt, got {other:?}"),
        }
    }

    #[test]
    fn chat_popup_info_event_opens_info_popup() {
        let mut s = ChatState::new();
        let env = event_env(json!({
            "kind": "chat.popup",
            "level": "info",
            "title": "models updated",
            "message": "phi4-mini is now the active model.",
            "source": "ollama",
        }));
        let a = handle_envelope(env, &mut s);
        assert!(matches!(a, Action::Render));
        match &s.popup {
            Some(Popup::Info { title, message, source, .. }) => {
                assert_eq!(title, "models updated");
                assert_eq!(message, "phi4-mini is now the active model.");
                assert_eq!(source.as_deref(), Some("ollama"));
            }
            other => panic!("expected Popup::Info, got {other:?}"),
        }
    }

    #[test]
    fn chat_popup_warning_event_opens_warning_popup() {
        let mut s = ChatState::new();
        let env = event_env(json!({
            "kind": "chat.popup",
            "level": "warning",
            "title": "rate limit",
            "message": "slow down",
            "source": "anthropic",
        }));
        let a = handle_envelope(env, &mut s);
        assert!(matches!(a, Action::Render));
        match &s.popup {
            Some(Popup::Warning { title, message, source, .. }) => {
                assert_eq!(title, "rate limit");
                assert_eq!(message, "slow down");
                assert_eq!(
                    source.as_deref(),
                    Some("anthropic"),
                    "publisher source must be captured"
                );
            }
            other => panic!("expected Popup::Warning, got {other:?}"),
        }
    }

    #[test]
    fn chat_popup_error_event_opens_error_popup() {
        let mut s = ChatState::new();
        let env = event_env(json!({
            "kind": "chat.popup",
            "level": "error",
            "title": "spawn failed",
            "message": "binary not found",
            "source": "nefor-combinators",
        }));
        let a = handle_envelope(env, &mut s);
        assert!(matches!(a, Action::Render));
        match &s.popup {
            Some(Popup::Error { title, message, source, .. }) => {
                assert_eq!(title, "spawn failed");
                assert_eq!(message, "binary not found");
                assert_eq!(source.as_deref(), Some("nefor-combinators"));
            }
            other => panic!("expected Popup::Error, got {other:?}"),
        }
    }

    #[test]
    fn chat_popup_unknown_level_dropped() {
        let mut s = ChatState::new();
        let env = event_env(json!({
            "kind": "chat.popup",
            "level": "fatal",
            "title": "boom",
            "message": "ignored",
        }));
        let a = handle_envelope(env, &mut s);
        assert!(
            matches!(a, Action::Continue),
            "unknown level must drop the event"
        );
        assert!(s.popup.is_none(), "popup must not open on unknown level");
    }

    #[test]
    fn chat_popup_missing_title_and_message_dropped() {
        let mut s = ChatState::new();
        let env = event_env(json!({
            "kind": "chat.popup",
            "level": "info",
            // both title and message absent
        }));
        let a = handle_envelope(env, &mut s);
        assert!(matches!(a, Action::Continue));
        assert!(s.popup.is_none());

        // Empty strings count as missing too — same drop policy.
        let env2 = event_env(json!({
            "kind": "chat.popup",
            "level": "warning",
            "title": "",
            "message": "",
        }));
        let a2 = handle_envelope(env2, &mut s);
        assert!(matches!(a2, Action::Continue));
        assert!(s.popup.is_none());

        // Only-title or only-message is fine; the other side falls back.
        let env3 = event_env(json!({
            "kind": "chat.popup",
            "level": "info",
            "title": "heads up",
            // message missing
        }));
        let a3 = handle_envelope(env3, &mut s);
        assert!(matches!(a3, Action::Render));
        match &s.popup {
            Some(Popup::Info { title, message, .. }) => {
                assert_eq!(title, "heads up");
                assert!(message.contains("no message"));
            }
            other => panic!("expected Popup::Info, got {other:?}"),
        }
    }

    #[test]
    fn typing_args_after_command_name_keeps_popup_closed() {
        // `/login anthropic` — popup must stay closed throughout the typing
        // of args, and Enter submits with `provider = Some("anthropic")`.
        let mut s = ChatState::new();
        type_text(&mut s, "/login anthropic");
        assert!(
            s.popup.is_none(),
            "popup must stay closed while typing args after a command name"
        );
        let action = handle_envelope(key_env("enter"), &mut s);
        match action {
            Action::SubmitPrompt(text) => {
                assert_eq!(
                    parse_command(&text),
                    Some(Command::Login {
                        provider: Some("anthropic".into()),
                    })
                );
            }
            other => panic!("expected SubmitPrompt, got {other:?}"),
        }
    }
}
