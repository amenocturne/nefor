//! nefor-tui — terminal frontend plugin for nefor.
//!
//! Speaks [NCP v0.1](../../../protocol/v0.1/spec.md) over stdio. Consumes
//! grid events (see `README.md`), renders them with ratatui, and forwards
//! key / paste / mouse / resize input back to the bus.
//!
//! This binary contains no chat logic; see `plugins/nefor-chat` (separate
//! crate) for the message-and-tool renderer built on top.

mod errors;
mod grid;
mod input;
mod render;
mod transport;

use std::io::stdout;

use anyhow::Context as _;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use nefor_protocol::{Body, Envelope, PluginOutgoing, SystemBody};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use serde_json::{Map, Value};
use tokio::sync::mpsc;

use crate::errors::TuiError;
use crate::grid::{DefaultColors, Grid, HlAttr, HlTable, LineCell};

const PLUGIN_NAME: &str = "nefor-tui";
const PLUGIN_VERSION: &str = "0.1.0";
const PROTOCOL_VERSION: &str = "0.1";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Logs go to stderr so they don't pollute the NCP stream on stdout.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    if let Err(e) = run().await {
        // TerminalGuard already ran by the time we get here (it's inside
        // `run`). Print once to stderr so the engine-spawned subprocess
        // surface shows the fault.
        tracing::error!(error = %e, "nefor-tui exited with error");
        eprintln!("nefor-tui: {e}");
        std::process::exit(1);
    }
    Ok(())
}

async fn run() -> anyhow::Result<()> {
    // Stdout writer — single owner, so the initial attach and every later
    // event share one lane and never interleave mid-line.
    let (out_tx, _writer_handle) = transport::spawn_stdout_writer(128);

    // Stdin reader. Sender is owned by the spawned task; we consume the
    // receiver in the main loop and in await_attach_ok.
    let (in_tx, mut in_rx) = mpsc::channel::<Result<Envelope, TuiError>>(128);
    let _reader_handle = transport::spawn_stdin_reader(in_tx);

    // 1) Send our attach before touching the terminal. If the engine
    //    rejects the attach we want to exit cleanly *without* clobbering
    //    the user's TTY.
    send_attach(&out_tx).await?;

    let engine_version = transport::await_attach_ok(&mut in_rx)
        .await
        .context("waiting for attach_ok")?;
    tracing::info!(engine_version = %engine_version, "attached");

    // 2) Enter raw mode + alt screen + mouse + bracketed paste. Install
    //    TerminalGuard before any possible panic path.
    enable_raw_mode().context("enable_raw_mode")?;
    execute!(
        stdout(),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )
    .context("enter alt screen / mouse / paste")?;
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend).context("build ratatui terminal")?;

    // 3) Measure the terminal and emit ready + initial resize so the
    //    engine (and downstream plugins) know our dimensions up front.
    let size = terminal.size().context("terminal size")?;
    let (cols, rows) = (size.width, size.height);
    let mut state = State {
        grid: Grid::new(cols, rows),
        hl: HlTable::new(),
        defaults: DefaultColors::default(),
    };
    send_event(&out_tx, input::resize_body(cols, rows)).await?;
    send_event(&out_tx, input::ready_body(cols, rows)).await?;

    // Draw an empty initial frame so the user sees a cleared alt screen.
    terminal
        .draw(|frame| render::draw(frame, &state.grid, &state.hl, &state.defaults))
        .context("initial draw")?;

    // 4) Main loop: multiplex stdin NCP messages and crossterm events.
    let mut term_events = EventStream::new();

    loop {
        tokio::select! {
            maybe_env = in_rx.recv() => {
                match maybe_env {
                    Some(Ok(env)) => {
                        let action = handle_envelope(env, &mut state);
                        match action {
                            LoopAction::Continue => {}
                            LoopAction::Flush => {
                                terminal
                                    .draw(|frame| render::draw(frame, &state.grid, &state.hl, &state.defaults))
                                    .context("frame draw")?;
                            }
                            LoopAction::Shutdown => break,
                        }
                    }
                    Some(Err(e)) => {
                        tracing::warn!(error = %e, "stdin parse error");
                    }
                    None => break, // engine closed stdio
                }
            }
            maybe_event = term_events.next() => {
                match maybe_event {
                    Some(Ok(event)) => {
                        if let Some(body) = translate_terminal_event(&event, &mut state) {
                            send_event(&out_tx, body).await?;
                        }
                    }
                    Some(Err(e)) => {
                        tracing::warn!(error = %e, "crossterm event error");
                    }
                    None => break,
                }
            }
        }
    }

    // Best-effort graceful detach before TerminalGuard tears down the
    // alt screen.
    let _ = out_tx
        .send(PluginOutgoing::system(SystemBody::Detach {
            reason: Some("stream closed".into()),
        }))
        .await;

    Ok(())
}

/// Render-relevant state that the engine controls.
struct State {
    grid: Grid,
    hl: HlTable,
    defaults: DefaultColors,
}

enum LoopAction {
    Continue,
    Flush,
    Shutdown,
}

/// Apply an incoming envelope to the state. Returns whether the caller
/// should redraw now (`Flush`) or keep buffering (`Continue`), or whether
/// the engine signalled shutdown.
fn handle_envelope(env: Envelope, state: &mut State) -> LoopAction {
    match env.body {
        Body::System(SystemBody::Shutdown { .. }) => LoopAction::Shutdown,
        Body::System(_) => LoopAction::Continue,
        Body::Event(map) => match map.get("kind").and_then(Value::as_str) {
            Some("nefor-tui.grid.resize") => {
                if let (Some(w), Some(h)) = (as_u32(&map, "width"), as_u32(&map, "height")) {
                    if grid_of(&map) == 1 {
                        state.grid.apply_resize(w, h);
                    }
                }
                LoopAction::Continue
            }
            Some("nefor-tui.grid.clear") => {
                if grid_of(&map) == 1 {
                    state.grid.apply_clear();
                }
                LoopAction::Continue
            }
            Some("nefor-tui.grid.cursor_goto") => {
                if grid_of(&map) == 1 {
                    if let (Some(r), Some(c)) = (as_u32(&map, "row"), as_u32(&map, "col")) {
                        state.grid.apply_cursor_goto(r, c);
                    }
                }
                LoopAction::Continue
            }
            Some("nefor-tui.grid.line") => {
                if grid_of(&map) == 1 {
                    if let (Some(r), Some(cs), Some(cells)) = (
                        as_u32(&map, "row"),
                        as_u32(&map, "col_start"),
                        map.get("cells").and_then(Value::as_array),
                    ) {
                        let line_cells = parse_line_cells(cells);
                        state.grid.apply_line(r, cs, &line_cells);
                    }
                }
                LoopAction::Continue
            }
            Some("nefor-tui.grid.scroll") => {
                if grid_of(&map) == 1 {
                    if let (Some(top), Some(bot), Some(rows)) = (
                        as_u32(&map, "top"),
                        as_u32(&map, "bot"),
                        map.get("rows").and_then(Value::as_i64),
                    ) {
                        state.grid.apply_scroll(
                            top,
                            bot,
                            rows.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
                        );
                    }
                }
                LoopAction::Continue
            }
            Some("nefor-tui.grid.flush") => LoopAction::Flush,
            Some("nefor-tui.hl_attr_define") => {
                if let Some(id) = as_u32(&map, "id") {
                    let attr = parse_hl_attr(map.get("rgb"));
                    state.hl.define(id, attr);
                }
                LoopAction::Continue
            }
            Some("nefor-tui.default_colors") => {
                if let (Some(fg), Some(bg), Some(sp)) =
                    (as_u32(&map, "fg"), as_u32(&map, "bg"), as_u32(&map, "sp"))
                {
                    state.defaults = DefaultColors { fg, bg, sp };
                }
                LoopAction::Continue
            }
            _ => LoopAction::Continue,
        },
    }
}

fn grid_of(map: &Map<String, Value>) -> u32 {
    as_u32(map, "grid").unwrap_or(1)
}

fn as_u32(map: &Map<String, Value>, key: &str) -> Option<u32> {
    map.get(key)
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
}

fn as_bool(map: &Map<String, Value>, key: &str) -> bool {
    map.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn parse_line_cells(cells: &[Value]) -> Vec<LineCell> {
    cells
        .iter()
        .filter_map(|v| {
            let arr = v.as_array()?;
            let text = arr.first().and_then(Value::as_str)?.to_owned();
            let hl_id = arr
                .get(1)
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok());
            let repeat = arr
                .get(2)
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok());
            Some(LineCell {
                text,
                hl_id,
                repeat,
            })
        })
        .collect()
}

fn parse_hl_attr(rgb: Option<&Value>) -> HlAttr {
    let Some(Value::Object(obj)) = rgb else {
        return HlAttr::default();
    };
    HlAttr {
        fg: as_u32(obj, "fg"),
        bg: as_u32(obj, "bg"),
        sp: as_u32(obj, "sp"),
        bold: as_bool(obj, "bold"),
        italic: as_bool(obj, "italic"),
        underline: as_bool(obj, "underline"),
        reverse: as_bool(obj, "reverse"),
    }
}

/// Translate a crossterm event into an NCP event body. Returns `None` for
/// events we don't forward (focus, etc.). Side effect: updates `state.grid`
/// on resize so the internal buffer matches the terminal.
fn translate_terminal_event(evt: &Event, state: &mut State) -> Option<Map<String, Value>> {
    match evt {
        Event::Key(k) => input::key_body(k),
        Event::Paste(text) => Some(input::paste_body(text)),
        Event::Mouse(m) => input::mouse_body(m),
        Event::Resize(cols, rows) => {
            state.grid.apply_resize(u32::from(*cols), u32::from(*rows));
            Some(input::resize_body(*cols, *rows))
        }
        Event::FocusGained | Event::FocusLost => None,
    }
}

async fn send_attach(out_tx: &mpsc::Sender<PluginOutgoing>) -> anyhow::Result<()> {
    out_tx
        .send(PluginOutgoing::system(SystemBody::Attach {
            name: PLUGIN_NAME.into(),
            version: PLUGIN_VERSION.into(),
            protocol_version: PROTOCOL_VERSION.into(),
        }))
        .await
        .map_err(|_| anyhow::anyhow!("stdout writer closed before attach was sent"))
}

async fn send_event(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: Map<String, Value>,
) -> anyhow::Result<()> {
    out_tx
        .send(PluginOutgoing::event(body))
        .await
        .map_err(|_| anyhow::anyhow!("stdout writer closed"))
}

/// Restore cooked mode, main screen, mouse capture off, bracketed paste
/// off. Runs on drop so panics unwind through a clean terminal.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if let Err(e) = execute!(
            stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        ) {
            tracing::error!(error = %e, "failed to leave alt screen on TUI exit");
        }
        if let Err(e) = disable_raw_mode() {
            tracing::error!(error = %e, "failed to disable raw mode on TUI exit");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn state() -> State {
        State {
            grid: Grid::new(10, 4),
            hl: HlTable::new(),
            defaults: DefaultColors::default(),
        }
    }

    fn event_env(body: Value) -> Envelope {
        use nefor_protocol::{PluginName, Timestamp};
        let Value::Object(map) = body else {
            panic!("body must be a JSON object");
        };
        Envelope::event(
            PluginName::new("engine-mock").expect("valid"),
            Timestamp::parse("2026-04-21T00:00:00.000Z").expect("valid"),
            map,
        )
    }

    #[test]
    fn resize_event_resizes_grid() {
        let mut s = state();
        let env = event_env(json!({
            "kind": "nefor-tui.grid.resize",
            "grid": 1,
            "width": 20,
            "height": 5
        }));
        let action = handle_envelope(env, &mut s);
        assert!(matches!(action, LoopAction::Continue));
        assert_eq!(s.grid.width(), 20);
        assert_eq!(s.grid.height(), 5);
    }

    #[test]
    fn resize_ignores_non_grid_1() {
        let mut s = state();
        let env = event_env(json!({
            "kind": "nefor-tui.grid.resize",
            "grid": 2,
            "width": 999,
            "height": 999
        }));
        handle_envelope(env, &mut s);
        assert_eq!(s.grid.width(), 10);
        assert_eq!(s.grid.height(), 4);
    }

    #[test]
    fn flush_signals_redraw() {
        let mut s = state();
        let env = event_env(json!({ "kind": "nefor-tui.grid.flush" }));
        let action = handle_envelope(env, &mut s);
        assert!(matches!(action, LoopAction::Flush));
    }

    #[test]
    fn shutdown_system_message_signals_shutdown() {
        use nefor_protocol::{PluginName, Timestamp};
        let mut s = state();
        let env = Envelope::system(
            PluginName::engine(),
            Timestamp::parse("2026-04-21T00:00:00.000Z").expect("valid"),
            SystemBody::Shutdown {
                reason: None,
                grace_ms: None,
            },
        );
        assert!(matches!(handle_envelope(env, &mut s), LoopAction::Shutdown));
    }

    #[test]
    fn line_event_writes_cells() {
        let mut s = state();
        let env = event_env(json!({
            "kind": "nefor-tui.grid.line",
            "grid": 1,
            "row": 0,
            "col_start": 0,
            "cells": [["h", 1, 1], ["i", null, 1]]
        }));
        handle_envelope(env, &mut s);
        assert_eq!(s.grid.row(0)[0].text, "h");
        assert_eq!(s.grid.row(0)[0].hl_id, 1);
        assert_eq!(s.grid.row(0)[1].text, "i");
        // hl_id inherits from previous when null/missing.
        assert_eq!(s.grid.row(0)[1].hl_id, 1);
    }

    #[test]
    fn scroll_event_accepts_negative_rows() {
        let mut s = state();
        // Prime row 0
        let line = event_env(json!({
            "kind": "nefor-tui.grid.line",
            "grid": 1,
            "row": 0,
            "col_start": 0,
            "cells": [["a", 1, 10]]
        }));
        handle_envelope(line, &mut s);
        let scroll = event_env(json!({
            "kind": "nefor-tui.grid.scroll",
            "grid": 1,
            "top": 0,
            "bot": 4,
            "rows": -1
        }));
        handle_envelope(scroll, &mut s);
        assert_eq!(s.grid.row(0)[0].text, " ");
        assert_eq!(s.grid.row(1)[0].text, "a");
    }

    #[test]
    fn hl_attr_define_event_stores_attributes() {
        let mut s = state();
        let env = event_env(json!({
            "kind": "nefor-tui.hl_attr_define",
            "id": 7,
            "rgb": { "fg": 0xFF8800u32, "bold": true, "italic": true }
        }));
        handle_envelope(env, &mut s);
        let a = s.hl.get(7);
        assert_eq!(a.fg, Some(0xFF8800));
        assert!(a.bold);
        assert!(a.italic);
        assert!(!a.underline);
    }

    #[test]
    fn default_colors_event_replaces_defaults() {
        let mut s = state();
        let env = event_env(json!({
            "kind": "nefor-tui.default_colors",
            "fg": 0xAABBCCu32,
            "bg": 0x112233u32,
            "sp": 0x445566u32
        }));
        handle_envelope(env, &mut s);
        assert_eq!(s.defaults.fg, 0xAABBCC);
        assert_eq!(s.defaults.bg, 0x112233);
        assert_eq!(s.defaults.sp, 0x445566);
    }

    #[test]
    fn unknown_event_kind_is_ignored() {
        let mut s = state();
        let env = event_env(json!({ "kind": "something-else.foo" }));
        assert!(matches!(handle_envelope(env, &mut s), LoopAction::Continue));
    }

    #[test]
    fn malformed_line_event_is_ignored() {
        let mut s = state();
        let env = event_env(json!({
            "kind": "nefor-tui.grid.line",
            "grid": 1,
            "row": 0,
            // missing col_start and cells — handler returns Continue and
            // leaves grid untouched.
        }));
        handle_envelope(env, &mut s);
        assert_eq!(s.grid.row(0)[0].text, " ");
    }

    #[test]
    fn parse_line_cells_handles_missing_optionals() {
        let cells = vec![json!(["a", 1, 3]), json!(["b"]), json!(["c", 2])];
        let out = parse_line_cells(&cells);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].text, "a");
        assert_eq!(out[0].hl_id, Some(1));
        assert_eq!(out[0].repeat, Some(3));
        assert_eq!(out[1].hl_id, None);
        assert_eq!(out[1].repeat, None);
        assert_eq!(out[2].hl_id, Some(2));
        assert_eq!(out[2].repeat, None);
    }
}
