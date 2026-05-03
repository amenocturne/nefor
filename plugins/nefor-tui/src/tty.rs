//! /dev/tty open + raw-mode guard.
//!
//! Lifted from the legacy `nefor-tui` plugin (commit reference left out
//! of the comment because the legacy plugin will be deleted at phase 6).
//! NCP plugins talk JSONL on stdout/stdin; the terminal escape codes
//! must therefore land on a separate `/dev/tty` handle.

use std::fs::{File, OpenOptions};
use std::io::Write;

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};

/// Open the controlling terminal for read+write. Each caller gets its
/// own file descriptor.
pub fn open_tty() -> std::io::Result<File> {
    OpenOptions::new().read(true).write(true).open("/dev/tty")
}

/// RAII guard that turns raw mode on at construction and off on drop.
/// Holds its own `/dev/tty` write handle so any teardown escapes land on
/// the TTY rather than corrupting the JSONL stream on stdout.
///
/// In addition to raw mode, the guard enables crossterm mouse capture so
/// wheel + click events route to the app instead of the terminal
/// emulator's own scrollback. Without capture, scroll wheel events
/// scroll the terminal's history buffer and never reach our event loop;
/// the user's reported "wheel scrolls terminal, not the chat" symptom is
/// exactly that. Capture is released on drop so the user's prompt
/// inherits a clean terminal.
pub struct RawModeGuard {
    writer: File,
}

impl RawModeGuard {
    /// Enable raw mode + alt-screen + mouse capture and emit any setup
    /// escapes through the supplied writer. The writer is held until drop
    /// so teardown escapes can use the same file descriptor.
    ///
    /// Order on entry: enable raw mode → enter alternate screen → enable
    /// mouse capture. The alt-screen swap saves the user's existing
    /// terminal contents and gives the TUI a clean canvas; on exit we
    /// restore the original buffer so there are no leftover frame
    /// fragments under the shell prompt.
    pub fn enter(mut writer: File) -> std::io::Result<Self> {
        enable_raw_mode()?;
        if let Err(e) = execute!(&mut writer, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(e);
        }
        if let Err(e) = execute!(&mut writer, EnableMouseCapture) {
            let _ = execute!(&mut writer, LeaveAlternateScreen);
            let _ = disable_raw_mode();
            return Err(e);
        }
        Ok(RawModeGuard { writer })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        // Teardown order is the inverse of setup: disable mouse capture
        // first, leave the alternate screen (restoring the user's
        // original terminal contents), show the cursor, flush, then drop
        // raw mode. Errors here are best-effort: by the time the guard
        // runs there's nothing the caller can do.
        if let Err(e) = execute!(&mut self.writer, DisableMouseCapture) {
            tracing::error!(error = %e, "failed to disable mouse capture on tui exit");
        }
        if let Err(e) = execute!(&mut self.writer, LeaveAlternateScreen) {
            tracing::error!(error = %e, "failed to leave alternate screen on tui exit");
        }
        if let Err(e) = execute!(&mut self.writer, crossterm::cursor::Show) {
            tracing::error!(error = %e, "failed to show cursor on tui exit");
        }
        if let Err(e) = self.writer.flush() {
            tracing::error!(error = %e, "failed to flush tty on tui exit");
        }
        if let Err(e) = disable_raw_mode() {
            tracing::error!(error = %e, "failed to disable raw mode on tui exit");
        }
    }
}
