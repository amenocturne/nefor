//! nefor-tui binary entrypoint.
//!
//! Phase 1 wiring: NCP handshake on stdin/stdout, raw-mode + /dev/tty for
//! terminal output, crossterm event stream into the engine. Receives no
//! NCP events of its own use yet (chat lives on the legacy plugin until
//! phase 6); the binary is here so the plugin can be smoke-tested end to
//! end with a hand-supplied scenario before the migration ramps up.

use std::io::Write as _;
use std::process::ExitCode;
use std::time::Duration;

use crossterm::event::{Event, EventStream};
use crossterm::terminal::size as term_size;
use futures::StreamExt;
use nefor_protocol::{Body, Envelope, PluginOutgoing, SystemBody};
use tokio::sync::mpsc;
use tokio::time::interval;

use nefor_tui::engine::Engine;
use nefor_tui::error::TuiError;
use nefor_tui::input::from_key_event;
use nefor_tui::mouse::from_crossterm as from_mouse_event;
use nefor_tui::ncp::{await_ready_ok, spawn_stdin_reader, spawn_stdout_writer, CHANNEL_CAP};
use nefor_tui::tty::{open_tty, RawModeGuard};

const PROTOCOL_VERSION: &str = "0.1";

/// Hard-coded scenario for phase-1 smoke runs. The real plugin entry
/// point will load Lua from a CLI flag once the surface stabilises;
/// hard-coding keeps the binary buildable and exercisable today.
const PLACEHOLDER_SCENARIO: &str = r#"
    tui.start {
      initial_state = { count = 0 },
      view = function(s)
        return tui.column { gap = 0, children = {
          tui.padding { value = 1, child = tui.text { content = "count: " .. tostring(s.count) } },
          tui.text { content = "press space; q to quit" },
        }}
      end,
      update = function(msg, s)
        if msg.kind == "key.space" then return { count = s.count + 1 }, {} end
        if msg.kind == "key.q" then return s, { { kind = "exit" } } end
        return s, {}
      end,
    }
"#;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "nefor-tui exited with error");
            eprintln!("nefor-tui: {e}");
            ExitCode::from(1)
        }
    }
}

async fn run() -> Result<(), TuiError> {
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
    engine.load_scenario(PLACEHOLDER_SCENARIO)?;

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
                    if let Body::System(SystemBody::Shutdown { .. }) = env.body {
                        break;
                    }
                    // Phase 1 ignores all post-handshake NCP events; the
                    // legacy plugin still owns chat until phase 6.
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
