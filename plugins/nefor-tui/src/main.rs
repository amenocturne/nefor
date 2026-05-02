//! nefor-tui binary entrypoint.
//!
//! Phase 6 wiring: NCP handshake on stdin/stdout, raw-mode + /dev/tty for
//! terminal output, crossterm event stream into the engine. Receives
//! `event`-shaped envelopes from peers (e.g. `chat.stream.delta` from a
//! provider via `agentic_workflow.for_provider`'s outer adapter) and
//! routes them into the user-authored Lua composition via
//! `Engine::dispatch_envelope_body`. Egress (`tui.emit` / `tui.send_to`
//! from Lua) lands on stdout as `PluginOutgoing::event(body)`.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use crossterm::event::{Event, EventStream};
use crossterm::terminal::size as term_size;
use futures::StreamExt;
use nefor_protocol::{Body, Envelope, PluginOutgoing, SystemBody};
use serde_json::{Map as JsonMap, Value as JsonValue};
use tokio::sync::mpsc;
use tokio::time::interval;

use nefor_tui::engine::Engine;
use nefor_tui::error::TuiError;
use nefor_tui::input::from_key_event;
use nefor_tui::mouse::from_crossterm as from_mouse_event;
use nefor_tui::ncp::{await_ready_ok, spawn_stdin_reader, spawn_stdout_writer, CHANNEL_CAP};
use nefor_tui::tty::{open_tty, RawModeGuard};

const PROTOCOL_VERSION: &str = "0.1";

/// Default scenario when no `--script` flag is supplied. Useful for
/// `cargo run -p nefor-tui` smoke runs and `cargo install` users who
/// haven't picked a chat composition yet. The real chat surface lives
/// at `starter/chat.lua` and gets loaded via `--script <path>`.
const PLACEHOLDER_SCENARIO: &str = r#"
    tui.start {
      initial_state = { count = 0 },
      view = function(s)
        return tui.column { gap = 0, children = {
          tui.padding { value = 1, child = tui.text { content = "count: " .. tostring(s.count) } },
          tui.text { content = "press space; q to quit; pass --script <path> to load a real composition" },
        }}
      end,
      update = function(msg, s)
        if msg.kind == "key.space" then return { count = s.count + 1 }, {} end
        if msg.kind == "key.q" then return s, { { kind = "exit" } } end
        return s, {}
      end,
    }
"#;

/// Parse `--script <path>` (or `-s <path>`) out of `std::env::args`.
/// Hand-rolled, no external dep — clap would just slow build times for
/// what is structurally a single-flag CLI. Unrecognised flags abort the
/// run with a usage hint so a typo doesn't silently load the placeholder.
fn parse_script_flag() -> Result<Option<PathBuf>, String> {
    let mut iter = std::env::args().skip(1);
    let mut script: Option<PathBuf> = None;
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-s" | "--script" => {
                let path = iter
                    .next()
                    .ok_or_else(|| "nefor-tui: --script requires a path argument".to_string())?;
                script = Some(PathBuf::from(path));
            }
            "-h" | "--help" => {
                return Err("Usage: nefor-tui [--script <path>]\n\n\
                     --script <path>   Load a Lua composition that calls tui.start { ... }.\n\
                     --help            Show this message.\n"
                    .to_string());
            }
            other => {
                return Err(format!(
                    "nefor-tui: unknown argument `{other}` (use --help for usage)"
                ));
            }
        }
    }
    Ok(script)
}

/// Read the `--script` file (UTF-8, no encoding sniffing) and feed it to
/// the engine. Errors carry the path so a missing/wrong file is obvious.
fn load_script_or_placeholder(
    engine: &mut Engine,
    script: Option<&PathBuf>,
) -> Result<(), TuiError> {
    match script {
        Some(path) => {
            let src = std::fs::read_to_string(path).map_err(|e| {
                TuiError::Io(std::io::Error::other(format!(
                    "nefor-tui: failed to read --script {}: {e}",
                    path.display()
                )))
            })?;
            engine.load_scenario(&src)
        }
        None => engine.load_scenario(PLACEHOLDER_SCENARIO),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let script = match parse_script_flag() {
        Ok(s) => s,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };

    let result = run(script.as_ref()).await;

    // `tokio::io::stdin()` parks a blocking reader thread the runtime
    // cannot cancel; letting `tokio::main` drop the runtime would hang
    // forever waiting for that thread to finish a `next_line().await`
    // that never returns. Bypass the runtime drop with `process::exit`
    // — by this point the terminal has already been restored via
    // `RawModeGuard`'s Drop in `run`, so this is a clean exit.
    match result {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            tracing::error!(error = %e, "nefor-tui exited with error");
            eprintln!("nefor-tui: {e}");
            std::process::exit(1)
        }
    }
}

async fn run(script: Option<&PathBuf>) -> Result<(), TuiError> {
    // Stdout writer first so the handshake can land cleanly.
    let (out_tx, _writer_handle) = spawn_stdout_writer();
    let (in_tx, mut in_rx) = mpsc::channel::<Result<Envelope, TuiError>>(CHANNEL_CAP);
    let _reader_handle = spawn_stdin_reader(in_tx);

    out_tx
        .send(PluginOutgoing::system(SystemBody::Ready {
            protocol_version: PROTOCOL_VERSION.into(),
        }))
        .await
        .map_err(|_| TuiError::WriterClosed)?;

    let engine_version = await_ready_ok(&mut in_rx).await?;
    tracing::info!(engine_version = %engine_version, "ready");

    // Bring up the terminal *after* the handshake so an early reject
    // doesn't clobber the user's cooked-mode tty.
    let tty_writer = open_tty()?;
    let mut tty_main = open_tty()?;
    let _guard = RawModeGuard::enter(tty_writer)?;

    let (cols, rows) = term_size().unwrap_or((80, 24));
    let mut engine = Engine::new(cols, rows)?;
    load_script_or_placeholder(&mut engine, script)?;

    // Flush any emit-queue entries the script produced at load time
    // (e.g. an initial `tui.emit { kind = "nefor-tui.hello" }` style
    // self-advertisement). The placeholder produces nothing here; chat
    // compositions may.
    drain_emits_to_writer(&mut engine, &out_tx).await?;

    let mut term_events = EventStream::new();
    // ~60Hz tick for animation primitives. The arm is always armed —
    // when no animation is active, `mark_animation_tick` is skipped and
    // `render_if_dirty` returns `None`, so the loop just goes back to
    // sleep.
    let mut anim_tick = interval(Duration::from_millis(16));
    loop {
        tokio::select! {
            maybe_env = in_rx.recv() => match maybe_env {
                Some(Ok(env)) => {
                    match env.body {
                        Body::System(SystemBody::Shutdown { .. }) => break,
                        Body::System(_) => {
                            // Other system messages (ready_ok again, errors)
                            // are not actionable post-handshake; log + skip.
                            tracing::debug!("post-handshake system message ignored");
                        }
                        Body::Event(map) => {
                            if let Err(e) = engine.dispatch_envelope_body(&map) {
                                tracing::warn!(error = %e, "engine.dispatch_envelope_body");
                            }
                        }
                    }
                }
                Some(Err(e)) => tracing::warn!(error = %e, "stdin parse error"),
                None => break,
            },
            maybe_evt = term_events.next() => match maybe_evt {
                Some(Ok(Event::Key(k))) => {
                    if let Some(km) = from_key_event(&k) {
                        engine.handle_key(km)?;
                    }
                }
                Some(Ok(Event::Resize(w, h))) => engine.handle_resize(w, h)?,
                Some(Ok(Event::Mouse(m))) => {
                    if let Some(mm) = from_mouse_event(&m) {
                        engine.handle_mouse(mm)?;
                    }
                }
                Some(Ok(_)) => {} // paste / focus — phase 4 doesn't surface these
                Some(Err(e)) => tracing::warn!(error = %e, "crossterm event error"),
                None => break,
            },
            _ = anim_tick.tick() => {
                if engine.has_active_animations() {
                    engine.mark_animation_tick();
                }
            }
        }

        // Drain Lua egress before painting — the user expects a single
        // pass: handle event → emit messages → repaint reflecting the
        // new state.
        drain_emits_to_writer(&mut engine, &out_tx).await?;

        if let Some(bytes) = engine.render_if_dirty()? {
            tty_main.write_all(&bytes)?;
            tty_main.flush()?;
        }
        if engine.exit_requested() {
            break;
        }
    }

    // Best-effort: clear the alt-screen-style state by writing a final
    // SGR reset so the user's prompt doesn't inherit colors.
    let _ = tty_main.write_all(b"\x1b[0m");
    let _ = tty_main.flush();
    drop(tty_main);
    let _ = out_tx; // keep writer alive until end of run
    Ok(())
}

/// Drain accumulated Lua egress and forward each entry as a
/// `PluginOutgoing::event(body)` line. `target_hint` is logged but not
/// used for routing — the engine broadcasts; per-peer delivery happens
/// via the bus (prefix-targeting in `starter/ncp.lua`).
async fn drain_emits_to_writer(
    engine: &mut Engine,
    out_tx: &mpsc::Sender<PluginOutgoing>,
) -> Result<(), TuiError> {
    let pending = engine.take_emit_queue();
    for (target_hint, body) in pending {
        if let Some(t) = &target_hint {
            tracing::trace!(target = %t, kind = ?body.get("kind"), "emit (hint)");
        }
        let outgoing = PluginOutgoing::event(canonical_body(body));
        out_tx
            .send(outgoing)
            .await
            .map_err(|_| TuiError::WriterClosed)?;
    }
    Ok(())
}

/// `serde_json::Map` already has insertion-order semantics with the
/// `preserve_order` feature on, so this is identity. Wrapped as a
/// helper anyway so a future canonicalization pass (e.g. moving `kind`
/// to the front) has one place to live.
fn canonical_body(map: JsonMap<String, JsonValue>) -> JsonMap<String, JsonValue> {
    map
}
