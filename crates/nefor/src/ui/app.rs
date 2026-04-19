//! TUI event loop.
//!
//! Enters raw mode + the alternate screen, drives a crossterm event stream
//! and a tick timer inside a `tokio::select!`, and draws the registered
//! widgets each tick. Every input event is also emitted onto the shared
//! [`EventBus`] so other subscribers (logging probes, plugins once Lua is
//! wired) can react — the renderer itself still owns the terminal and the
//! crossterm event source.
//!
//! Only `q` / Ctrl-C are handled in-loop as quit keys. Arbitrary key dispatch
//! at the Lua boundary lands with the Lua bindings.
//!
//! Panic safety is handled by a `Drop`-based [`TerminalGuard`]: the guard is
//! constructed *immediately* after entering raw mode / alt-screen, so any
//! panic inside the loop still restores the terminal as the guard unwinds.

use std::io::stdout;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::events::{EventBus, EventName, EventPayload, KEY, RESIZE, SHUTDOWN, STARTUP, TICK};
use crate::ui::error::UiError;
use crate::ui::widget::WidgetRegistry;

/// Shared handle on the widget registry.
///
/// Wrapped in an `Arc<Mutex<_>>` so Lua bindings (next commit) can register
/// widgets *after* `run` has taken ownership of the render loop. Critical
/// sections are tiny — push on register, iterate+render on draw — and
/// Lua-side registration happens on whichever thread emitted the call, so a
/// `std::sync::Mutex` suffices.
pub type SharedRegistry = Arc<Mutex<WidgetRegistry>>;

/// How often to redraw when no input event arrives. 100 ms is the usual TUI
/// baseline — fast enough that future time-based widgets (clocks, spinners)
/// feel live, slow enough that the idle CPU cost is negligible.
const TICK_INTERVAL: Duration = Duration::from_millis(100);

/// Restores the terminal to cooked mode + main screen on drop.
///
/// Constructed after `enable_raw_mode` + `EnterAlternateScreen` so that
/// unwinding through a panic still puts the user's terminal back.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if let Err(e) = disable_raw_mode() {
            tracing::error!(error = %e, "failed to disable raw mode on TUI exit");
        }
        if let Err(e) = execute!(stdout(), LeaveAlternateScreen) {
            tracing::error!(error = %e, "failed to leave alternate screen on TUI exit");
        }
    }
}

/// Run the TUI event loop until the user presses `q` or Ctrl-C.
///
/// On entry the loop emits [`STARTUP`] on `bus`; on clean exit it emits
/// [`SHUTDOWN`]. Each crossterm event is forwarded as [`KEY`] or [`RESIZE`];
/// each tick fires [`TICK`]. The renderer keeps ownership of the crossterm
/// stream — the bus is broadcast-to-other-subscribers, not a reader.
///
/// Errors during terminal setup or event I/O bubble up as [`UiError`]; the
/// [`TerminalGuard`] ensures the terminal is restored on both normal exit and
/// panic.
pub async fn run(bus: Arc<EventBus>, registry: SharedRegistry) -> Result<(), UiError> {
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen)?;
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;

    bus.emit(&EventName::from(STARTUP), EventPayload::None);

    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(TICK_INTERVAL);
    // The first tick fires immediately (interval's default); that gives us a
    // frame on the screen without waiting 100 ms for input.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                bus.emit(&EventName::from(TICK), EventPayload::Tick);
                draw_locked(&mut terminal, &registry)?;
            }
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(event)) => {
                        // Fan out to bus subscribers first, then handle quit
                        // and redraw locally. Quit still exits even if a
                        // subscriber panics — we rely on the bus being
                        // reentrancy-safe (see events::bus doc).
                        emit_crossterm(&bus, &event);
                        if should_quit(&event) {
                            break;
                        }
                        // Resize events are picked up automatically on the
                        // next `terminal.draw`; we just need to redraw.
                        draw_locked(&mut terminal, &registry)?;
                    }
                    Some(Err(e)) => {
                        bus.emit(&EventName::from(SHUTDOWN), EventPayload::None);
                        return Err(UiError::Terminal(e));
                    }
                    None => break, // stream closed
                }
            }
        }
    }

    bus.emit(&EventName::from(SHUTDOWN), EventPayload::None);
    Ok(())
}

/// Lock the shared registry briefly and render one frame.
///
/// The lock is held only for the duration of `terminal.draw`, which is
/// itself bounded by how long the registered widgets take to produce their
/// lines. Widgets that want to do heavy work should push the computation out
/// of their renderer and read from a cached value — the usual ratatui
/// discipline.
fn draw_locked(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    registry: &SharedRegistry,
) -> Result<(), UiError> {
    let guard = match registry.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    terminal.draw(|frame| guard.render_all(frame))?;
    Ok(())
}

/// Translate a crossterm event into a bus emit. Events we don't yet have a
/// typed payload for (focus, mouse, paste) are ignored at this layer — they
/// land when a subscriber actually asks for them.
fn emit_crossterm(bus: &EventBus, event: &Event) {
    match event {
        Event::Key(k) => bus.emit(&EventName::from(KEY), EventPayload::Key(*k)),
        Event::Resize(cols, rows) => bus.emit(
            &EventName::from(RESIZE),
            EventPayload::Resize {
                cols: *cols,
                rows: *rows,
            },
        ),
        _ => {}
    }
}

/// `true` if `event` is Ctrl-C — the only exit key core handles. Plugins can
/// emit additional quit events on the bus in the future.
fn should_quit(event: &Event) -> bool {
    match event {
        Event::Key(KeyEvent {
            code: KeyCode::Char('c'),
            modifiers,
            ..
        }) => modifiers.contains(KeyModifiers::CONTROL),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventKind;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        })
    }

    #[test]
    fn q_does_not_quit() {
        assert!(!should_quit(&key(KeyCode::Char('q'), KeyModifiers::NONE)));
    }

    #[test]
    fn ctrl_c_quits() {
        assert!(should_quit(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)));
    }

    #[test]
    fn plain_c_does_not_quit() {
        assert!(!should_quit(&key(KeyCode::Char('c'), KeyModifiers::NONE)));
    }

    #[test]
    fn other_key_does_not_quit() {
        assert!(!should_quit(&key(KeyCode::Enter, KeyModifiers::NONE)));
    }

    #[test]
    fn resize_does_not_quit() {
        assert!(!should_quit(&Event::Resize(80, 24)));
    }

    #[test]
    fn emit_crossterm_forwards_key_events() {
        let bus = EventBus::new();
        let count = Arc::new(AtomicU64::new(0));
        let c = Arc::clone(&count);
        bus.on(
            EventName::from(KEY),
            Box::new(move |payload| {
                if matches!(payload, EventPayload::Key(_)) {
                    c.fetch_add(1, Ordering::Relaxed);
                }
            }),
        );

        emit_crossterm(&bus, &key(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn emit_crossterm_forwards_resize() {
        let bus = EventBus::new();
        let seen = Arc::new(std::sync::Mutex::new(None::<(u16, u16)>));
        let s = Arc::clone(&seen);
        bus.on(
            EventName::from(RESIZE),
            Box::new(move |payload| {
                if let EventPayload::Resize { cols, rows } = payload {
                    *s.lock().unwrap() = Some((*cols, *rows));
                }
            }),
        );

        emit_crossterm(&bus, &Event::Resize(120, 40));
        assert_eq!(*seen.lock().unwrap(), Some((120, 40)));
    }

    #[test]
    fn emit_crossterm_ignores_other_events() {
        let bus = EventBus::new();
        let any = Arc::new(AtomicU64::new(0));
        for name in [KEY, RESIZE, TICK, STARTUP, SHUTDOWN] {
            let a = Arc::clone(&any);
            bus.on(
                EventName::from(name),
                Box::new(move |_| {
                    a.fetch_add(1, Ordering::Relaxed);
                }),
            );
        }
        // FocusGained is silently dropped — no emit, no panic.
        emit_crossterm(&bus, &Event::FocusGained);
        assert_eq!(any.load(Ordering::Relaxed), 0);
    }
}
