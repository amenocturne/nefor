//! Tracing setup.
//!
//! Writes to `<config_dir>/nefor.log` by default so log output doesn't paint
//! over a plugin that may have taken over the terminal (alternate-screen
//! buffer, raw mode, etc.). When `NEFOR_LOG_STDERR` is set (any non-empty
//! value), logs go to stderr instead — useful for headless runs
//! (`cargo test`, `--help` inspections, debugging with the terminal visible).
//!
//! Filter comes from `RUST_LOG` via `EnvFilter`, defaulting to `info`.
//!
//! In file mode, ERROR-level events are *also* mirrored to stderr — silent
//! failures (plugin spawn errors, init.lua exec errors) would otherwise be
//! invisible to a user who doesn't know to tail the log file.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::{SubscriberInitExt, TryInitError};
use tracing_subscriber::{fmt, EnvFilter, Layer};

#[derive(Debug, thiserror::Error)]
pub enum LogInitError {
    #[error("failed to open log file {path:?}: {source}")]
    OpenFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Init(#[from] TryInitError),
}

/// Initialize the global tracing subscriber writing to `log_path`.
///
/// Creates parent directories if needed; appends if the file already exists.
/// ANSI color codes are suppressed for file output (terminals don't interpret
/// them mid-file and plain text is friendlier to `cat` / `less`).
pub fn init(log_path: &Path) -> Result<(), LogInitError> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let use_stderr = std::env::var_os("NEFOR_LOG_STDERR").is_some_and(|v| !v.is_empty());

    if use_stderr {
        fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .finish()
            .try_init()?;
        return Ok(());
    }

    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| LogInitError::OpenFile {
            path: log_path.to_path_buf(),
            source,
        })?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .map_err(|source| LogInitError::OpenFile {
            path: log_path.to_path_buf(),
            source,
        })?;

    let file_layer = fmt::layer()
        .with_writer(std::sync::Mutex::new(file))
        .with_ansi(false)
        .with_filter(filter);

    // Surface ERROR-level events on stderr too. The user's terminal is the
    // primary feedback channel; silent failures (plugin spawn errors, bad
    // init.lua, etc.) waste a lot of debugging time. Terminal-takeover
    // plugins claim /dev/tty, not stderr, so these one-line errors print
    // before any alternate screen is entered and remain visible after exit,
    // never overwriting live frames.
    let stderr_errors = fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_ansi(false)
        .with_filter(LevelFilter::ERROR);

    tracing_subscriber::registry()
        .with(file_layer)
        .with(stderr_errors)
        .try_init()?;
    Ok(())
}
