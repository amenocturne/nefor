//! /dev/tty open + raw-mode guard.
//!
//! Lifted from the legacy `nefor-tui` plugin (commit reference left out
//! of the comment because the legacy plugin will be deleted at phase 6).
//! NCP plugins talk JSONL on stdout/stdin; the terminal escape codes
//! must therefore land on a separate `/dev/tty` handle.

use std::fs::{File, OpenOptions};
use std::io::Write;

use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

/// Open the controlling terminal for read+write. Each caller gets its
/// own file descriptor.
pub fn open_tty() -> std::io::Result<File> {
    OpenOptions::new().read(true).write(true).open("/dev/tty")
}

/// RAII guard that turns raw mode on at construction and off on drop.
/// Holds its own `/dev/tty` write handle so any teardown escapes land on
/// the TTY rather than corrupting the JSONL stream on stdout.
pub struct RawModeGuard {
    writer: File,
}

impl RawModeGuard {
    /// Enable raw mode and emit any setup escapes through the supplied
    /// writer. The writer is held until drop so teardown escapes can use
    /// the same file descriptor.
    pub fn enter(writer: File) -> std::io::Result<Self> {
        enable_raw_mode()?;
        Ok(RawModeGuard { writer })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        // Show the cursor again on the way out (we hide it during
        // rendering). Then disable raw mode. Errors here are best-effort:
        // by the time the guard runs there's nothing the caller can do.
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
