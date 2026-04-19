//! TUI event loop.
//!
//! Enters raw mode + the alternate screen, drives a crossterm event stream
//! and a tick timer inside a `tokio::select!`, and draws the registered
//! widgets each tick. The loop is deliberately minimal: only `q` / Ctrl-C
//! are handled. Arbitrary key dispatch, `subscribe_key`, `subscribe_resize`,
//! and the Lua-visible event bus arrive in subsequent commits.
//!
//! Panic safety is handled by a `Drop`-based [`TerminalGuard`]: the guard is
//! constructed *immediately* after entering raw mode / alt-screen, so any
//! panic inside the loop still restores the terminal as the guard unwinds.

use std::io::stdout;
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::ui::error::UiError;
use crate::ui::widget::WidgetRegistry;

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
/// Returns once the loop exits cleanly. Errors during terminal setup or
/// event I/O bubble up as [`UiError`]; the [`TerminalGuard`] ensures the
/// terminal is restored on both normal exit and panic.
pub async fn run(registry: WidgetRegistry) -> Result<(), UiError> {
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen)?;
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(TICK_INTERVAL);
    // The first tick fires immediately (interval's default); that gives us a
    // frame on the screen without waiting 100 ms for input.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                terminal.draw(|frame| registry.render_all(frame))?;
            }
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(event)) => {
                        if should_quit(&event) {
                            break;
                        }
                        // Resize events are picked up automatically on the
                        // next `terminal.draw`; we just need to redraw.
                        terminal.draw(|frame| registry.render_all(frame))?;
                    }
                    Some(Err(e)) => return Err(UiError::Terminal(e)),
                    None => break, // stream closed
                }
            }
        }
    }

    Ok(())
}

/// `true` if `event` is `q` or Ctrl-C — the only exit keys MVP understands.
fn should_quit(event: &Event) -> bool {
    match event {
        Event::Key(KeyEvent {
            code: KeyCode::Char('q'),
            ..
        }) => true,
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

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        })
    }

    #[test]
    fn q_quits() {
        assert!(should_quit(&key(KeyCode::Char('q'), KeyModifiers::NONE)));
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
}
